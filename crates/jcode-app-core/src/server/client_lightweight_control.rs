use super::client_comm::{
    handle_comm_channel_members, handle_comm_list, handle_comm_list_channels, handle_comm_message,
    handle_comm_read, handle_comm_share, handle_comm_subscribe_channel,
    handle_comm_unsubscribe_channel,
};
use super::client_peer::{PeerServerContext, handle_peer_request};
use super::client_writer::{log_event_write_failure, write_direct_event};
use super::comm_await::{CommAwaitMembersContext, handle_comm_await_members};
use super::comm_control::{
    handle_comm_assign_next, handle_comm_assign_role, handle_comm_assign_task,
    handle_comm_task_control,
};
use super::comm_plan::{
    handle_comm_approve_plan, handle_comm_propose_plan, handle_comm_reject_plan,
};
use super::comm_session::{handle_comm_list_models, handle_comm_spawn, handle_comm_stop};
use super::comm_sync::{
    CommResyncPlanContext, handle_comm_plan_status, handle_comm_read_context,
    handle_comm_resync_plan, handle_comm_status, handle_comm_summary,
};
use super::{
    AwaitMembersRuntime, ChannelSubscriptions, ClientConnectionInfo, FileTouchService,
    SessionAgents, SessionInterruptQueues, SharedContext, SwarmEvent, SwarmMember,
    SwarmMutationRuntime, VersionedPlan, format_structured_completion_report, truncate_detail,
    update_member_status_with_report_tldr,
};
use crate::config::SwarmSpawnMode;
use crate::protocol::{Request, ServerEvent};
use crate::provider::Provider;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

pub(super) fn parse_swarm_spawn_mode(
    id: u64,
    spawn_mode: Option<String>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> Option<Option<SwarmSpawnMode>> {
    match spawn_mode {
        Some(value) => match SwarmSpawnMode::parse(&value) {
            Some(mode) => Some(Some(mode)),
            None => {
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message: format!(
                        "Invalid spawn_mode '{value}'. Expected one of: visible, headless, inline, auto"
                    ),
                    retry_after_secs: None,
                });
                None
            }
        },
        None => Some(None),
    }
}

pub(super) struct LightweightControlContext<'a> {
    pub(super) sessions: &'a SessionAgents,
    pub(super) global_session_id: &'a Arc<RwLock<String>>,
    pub(super) provider_template: &'a Arc<dyn Provider>,
    pub(super) swarm_members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub(super) swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub(super) shared_context: &'a Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    pub(super) swarm_plans: &'a Arc<RwLock<HashMap<String, VersionedPlan>>>,
    pub(super) swarm_coordinators: &'a Arc<RwLock<HashMap<String, String>>>,
    pub(super) file_touch: &'a FileTouchService,
    pub(super) channel_subscriptions: &'a ChannelSubscriptions,
    pub(super) channel_subscriptions_by_session: &'a ChannelSubscriptions,
    pub(super) client_connections: &'a Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    pub(super) event_history: &'a Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub(super) event_counter: &'a Arc<std::sync::atomic::AtomicU64>,
    pub(super) swarm_event_tx: &'a broadcast::Sender<SwarmEvent>,
    pub(super) mcp_pool: &'a Arc<crate::mcp::SharedMcpPool>,
    pub(super) soft_interrupt_queues: &'a SessionInterruptQueues,
    pub(super) await_members_runtime: &'a AwaitMembersRuntime,
    pub(super) swarm_mutation_runtime: &'a SwarmMutationRuntime,
    pub(super) turn_coordinator: &'a super::turn_coordinator::TurnCoordinator,
    pub(super) peer_groups: &'a jcode_base::peer_groups::PeerGroups,
    pub(super) peer_exchanges: &'a super::peer_exchange::PeerExchangeRegistry,
}

pub(super) async fn handle_lightweight_control_request(
    request: Request,
    reader: &mut BufReader<crate::transport::ReadHalf>,
    writer: Arc<Mutex<crate::transport::WriteHalf>>,
    context: LightweightControlContext<'_>,
) -> Result<()> {
    let LightweightControlContext {
        sessions,
        global_session_id,
        provider_template,
        swarm_members,
        swarms_by_id,
        shared_context,
        swarm_plans,
        swarm_coordinators,
        file_touch,
        channel_subscriptions,
        channel_subscriptions_by_session,
        client_connections,
        event_history,
        event_counter,
        swarm_event_tx,
        mcp_pool,
        soft_interrupt_queues,
        await_members_runtime,
        swarm_mutation_runtime,
        turn_coordinator,
        peer_groups,
        peer_exchanges,
    } = context;
    if let Request::Ping { id } = request {
        write_direct_event(&writer, &ServerEvent::Pong { id }).await?;
        return Ok(());
    }

    write_direct_event(&writer, &ServerEvent::Ack { id: request.id() }).await?;

    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let writer_clone = Arc::clone(&writer);
    let event_handle = tokio::spawn(async move {
        while let Some(event) = client_event_rx.recv().await {
            if let Err(error) = write_direct_event(&writer_clone, &event).await {
                log_event_write_failure("lightweight_control", None, &event, &error);
                break;
            }
        }
    });

    match request {
        request @ Request::PeerOverview { .. } => {
            handle_peer_request(
                request,
                &client_event_tx,
                PeerServerContext {
                    sessions,
                    peer_groups,
                    exchanges: peer_exchanges,
                    swarm_members,
                    swarms_by_id,
                    event_history,
                    event_counter,
                    swarm_event_tx,
                    turn_coordinator,
                },
            )
            .await;
        }
        request @ (Request::PeerList { .. }
        | Request::PeerSend { .. }
        | Request::PeerReply { .. }
        | Request::PeerCancel { .. }) => {
            run_peer_request_until_disconnect(
                reader,
                handle_peer_request(
                    request,
                    &client_event_tx,
                    PeerServerContext {
                        sessions,
                        peer_groups,
                        exchanges: peer_exchanges,
                        swarm_members,
                        swarms_by_id,
                        event_history,
                        event_counter,
                        swarm_event_tx,
                        turn_coordinator,
                    },
                ),
            )
            .await;
        }
        Request::CommShare {
            id,
            session_id: req_session_id,
            key,
            value,
            append,
        } => {
            handle_comm_share(
                id,
                req_session_id,
                key,
                value,
                append,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommRead {
            id,
            session_id: req_session_id,
            key,
        } => {
            handle_comm_read(
                id,
                req_session_id,
                key,
                &client_event_tx,
                swarm_members,
                shared_context,
            )
            .await;
        }
        Request::CommMessage {
            id,
            from_session,
            message,
            to_session,
            channel,
            delivery,
            wake,
            tldr,
        } => {
            handle_comm_message(
                id,
                from_session,
                message,
                to_session,
                channel,
                delivery,
                wake,
                tldr,
                &client_event_tx,
                sessions,
                soft_interrupt_queues,
                swarm_members,
                swarms_by_id,
                channel_subscriptions,
                event_history,
                event_counter,
                swarm_event_tx,
                client_connections,
                turn_coordinator,
            )
            .await;
        }
        Request::CommList {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list(
                id,
                req_session_id,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                file_touch,
                sessions,
                client_connections,
            )
            .await;
        }
        Request::CommListChannels {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_channels(
                id,
                req_session_id,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
            )
            .await;
        }
        Request::CommChannelMembers {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_channel_members(
                id,
                req_session_id,
                channel,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
            )
            .await;
        }
        Request::CommProposePlan {
            id,
            session_id: req_session_id,
            items,
        } => {
            handle_comm_propose_plan(
                id,
                req_session_id,
                items,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                swarm_plans,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommApprovePlan {
            id,
            session_id: req_session_id,
            proposer_session,
        } => {
            handle_comm_approve_plan(
                id,
                req_session_id,
                proposer_session,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                swarm_plans,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommRejectPlan {
            id,
            session_id: req_session_id,
            proposer_session,
            reason,
        } => {
            handle_comm_reject_plan(
                id,
                req_session_id,
                proposer_session,
                reason,
                &client_event_tx,
                swarm_members,
                shared_context,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommSeedGraph {
            id,
            session_id: req_session_id,
            mode,
            nodes,
        } => {
            super::comm_graph::handle_comm_seed_graph(
                id,
                req_session_id,
                mode,
                nodes,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommExpandNode {
            id,
            session_id: req_session_id,
            node_id,
            children,
        } => {
            super::comm_graph::handle_comm_expand_node(
                id,
                req_session_id,
                node_id,
                children,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommCompleteNode {
            id,
            session_id: req_session_id,
            node_id,
            artifact_json,
        } => {
            super::comm_graph::handle_comm_complete_node(
                id,
                req_session_id,
                node_id,
                artifact_json,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommInjectGap {
            id,
            session_id: req_session_id,
            gate_id,
            nodes,
        } => {
            super::comm_graph::handle_comm_inject_gap(
                id,
                req_session_id,
                gate_id,
                nodes,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommSpawn {
            id,
            session_id: req_session_id,
            working_dir,
            initial_message,
            request_nonce,
            spawn_mode,
            model,
            effort,
            label,
        } => {
            let spawn_mode = match parse_swarm_spawn_mode(id, spawn_mode, &client_event_tx) {
                Some(spawn_mode) => spawn_mode,
                None => return Ok(()),
            };
            handle_comm_spawn(
                id,
                req_session_id,
                working_dir,
                initial_message,
                request_nonce,
                spawn_mode,
                model,
                effort,
                label,
                &client_event_tx,
                sessions,
                global_session_id,
                provider_template,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
                mcp_pool,
                soft_interrupt_queues,
                swarm_mutation_runtime,
                client_connections,
                turn_coordinator,
            )
            .await;
        }
        Request::CommListModels {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_models(id, &req_session_id, sessions, provider_template, |event| {
                let _ = client_event_tx.send(event);
            })
            .await;
        }
        Request::CommStop {
            id,
            session_id: req_session_id,
            target_session,
            force,
        } => {
            handle_comm_stop(
                id,
                req_session_id,
                target_session,
                force.unwrap_or(false),
                &client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
                soft_interrupt_queues,
                swarm_mutation_runtime,
                peer_exchanges,
                turn_coordinator,
            )
            .await;
        }
        Request::CommAssignRole {
            id,
            session_id: req_session_id,
            target_session,
            role,
        } => {
            handle_comm_assign_role(
                id,
                req_session_id,
                target_session,
                role,
                &client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommSummary {
            id,
            session_id: req_session_id,
            target_session,
            limit,
        } => {
            handle_comm_summary(
                id,
                req_session_id,
                target_session,
                limit,
                sessions,
                swarm_members,
                &client_event_tx,
            )
            .await;
        }
        Request::CommStatus {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_status(
                id,
                req_session_id,
                target_session,
                sessions,
                swarm_members,
                client_connections,
                file_touch,
                &client_event_tx,
            )
            .await;
        }
        Request::CommReport {
            id,
            session_id: req_session_id,
            status,
            message,
            validation,
            follow_up,
            tldr,
        } => {
            let status = status.unwrap_or_else(|| "ready".to_string());
            let report = format_structured_completion_report(
                &message,
                validation.as_deref(),
                follow_up.as_deref(),
            );
            let detail = Some(truncate_detail(&message, 160));
            update_member_status_with_report_tldr(
                &req_session_id,
                &status,
                detail,
                Some(report.clone()),
                tldr,
                swarm_members,
                swarms_by_id,
                Some(event_history),
                Some(event_counter),
                Some(swarm_event_tx),
            )
            .await;
            let _ = client_event_tx.send(ServerEvent::CommReportResponse {
                id,
                status,
                message: "Report recorded and delivered to the coordinator when applicable."
                    .to_string(),
            });
        }
        Request::CommPlanStatus {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_plan_status(
                id,
                req_session_id,
                swarm_members,
                swarm_plans,
                &client_event_tx,
            )
            .await;
        }
        Request::CommReadContext {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_read_context(
                id,
                req_session_id,
                target_session,
                sessions,
                swarm_members,
                &client_event_tx,
            )
            .await;
        }
        Request::CommResyncPlan {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_resync_plan(
                id,
                req_session_id,
                &CommResyncPlanContext {
                    client_event_tx: &client_event_tx,
                    swarm_members,
                    swarms_by_id,
                    swarm_plans,
                    swarm_coordinators,
                    event_history,
                    event_counter,
                    swarm_event_tx,
                },
            )
            .await;
        }
        Request::CommAssignTask {
            id,
            session_id: req_session_id,
            target_session,
            task_id,
            message,
        } => {
            handle_comm_assign_task(
                id,
                req_session_id,
                target_session,
                task_id,
                message,
                &client_event_tx,
                sessions,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
                turn_coordinator,
            )
            .await;
        }
        Request::CommAssignNext {
            id,
            session_id: req_session_id,
            target_session,
            working_dir,
            prefer_spawn,
            spawn_if_needed,
            message,
            model,
            effort,
        } => {
            handle_comm_assign_next(
                id,
                req_session_id,
                target_session,
                working_dir,
                prefer_spawn,
                spawn_if_needed,
                message,
                model,
                effort,
                &client_event_tx,
                sessions,
                global_session_id,
                provider_template,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                mcp_pool,
                swarm_mutation_runtime,
                turn_coordinator,
            )
            .await;
        }
        Request::CommTaskControl {
            id,
            session_id: req_session_id,
            action,
            task_id,
            target_session,
            message,
        } => {
            handle_comm_task_control(
                id,
                req_session_id,
                action,
                task_id,
                target_session,
                message,
                &client_event_tx,
                sessions,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
                turn_coordinator,
            )
            .await;
        }
        Request::CommSubscribeChannel {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_subscribe_channel(
                id,
                req_session_id,
                channel,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommUnsubscribeChannel {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_unsubscribe_channel(
                id,
                req_session_id,
                channel,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommAwaitMembers {
            id,
            session_id: req_session_id,
            target_status,
            session_ids: requested_ids,
            mode,
            timeout_secs,
            background,
            notify,
            wake,
        } => {
            handle_comm_await_members(
                id,
                req_session_id,
                target_status,
                requested_ids,
                mode,
                timeout_secs,
                background,
                notify,
                wake,
                CommAwaitMembersContext {
                    client_event_tx: &client_event_tx,
                    swarm_members,
                    swarms_by_id,
                    swarm_event_tx,
                    await_members_runtime,
                },
            )
            .await;
        }
        other => {
            let _ = client_event_tx.send(ServerEvent::Error {
                id: other.id(),
                message: "unsupported lightweight control request".to_string(),
                retry_after_secs: None,
            });
        }
    }

    drop(client_event_tx);
    let _ = event_handle.await;
    Ok(())
}

async fn run_peer_request_until_disconnect<F>(
    reader: &mut BufReader<crate::transport::ReadHalf>,
    request: F,
) where
    F: Future<Output = ()>,
{
    tokio::pin!(request);
    let mut line = String::new();
    loop {
        tokio::select! {
            biased;
            () = &mut request => return,
            read = reader.read_line(&mut line) => match read {
                Ok(0) | Err(_) => return,
                Ok(_) => line.clear(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::peer_exchange::{
        PeerExchangeRegistry, PeerIdentity, RegisteredPeerExchange,
    };
    use crate::server::turn_coordinator::TurnCoordinator;
    use crate::tool::TurnOrigin;
    use std::time::Duration;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn lightweight_peer_wait_is_cancelled_when_sender_socket_closes() {
        let coordinator = TurnCoordinator::default();
        let sender_lease = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("sender turn");
        let pending = coordinator
            .begin_peer_turn(
                sender_lease.context(),
                "recipient",
                "peer-socket-eof".to_string(),
            )
            .expect("peer reservation");
        let registry = PeerExchangeRegistry::new(coordinator, Duration::from_secs(600));
        let registered = registry
            .register(
                pending,
                PeerIdentity {
                    session_id: "sender".to_string(),
                    alias: "Eve".to_string(),
                    project_name: "sender-project".to_string(),
                },
                PeerIdentity {
                    session_id: "recipient".to_string(),
                    alias: "Atlas".to_string(),
                    project_name: "recipient-project".to_string(),
                },
            )
            .expect("registered peer exchange");
        let RegisteredPeerExchange {
            recipient_cancellation,
            recipient_lease,
            waiter,
            ..
        } = registered;

        let (server_stream, client_stream) = crate::transport::Stream::pair().expect("socket pair");
        let (server_reader, _server_writer) = server_stream.into_split();
        let mut server_reader = BufReader::new(server_reader);

        let wait = async move {
            let _recipient_lease = recipient_lease;
            let _ = waiter.wait().await;
        };
        drop(client_stream);
        tokio::time::timeout(
            Duration::from_secs(1),
            run_peer_request_until_disconnect(&mut server_reader, wait),
        )
        .await
        .expect("socket EOF should abandon peer wait promptly");

        assert!(
            recipient_cancellation.is_set(),
            "abandoning the sender socket must cancel the recipient peer turn"
        );
        drop(sender_lease);
    }
}
