use super::live_turn::{LiveTurnSwarmContext, spawn_tracked_live_turn_with_completion};
use super::peer_exchange::{
    PeerExchangeRegistry, PeerExchangeResult, PeerIdentity, PeerRecipientOutcome,
    PinnedPeerIdentity,
};
use super::turn_coordinator::{BeginPeerError, TurnCoordinator};
use super::{SessionAgents, SwarmEvent, SwarmMember, session_event_fanout_sender};
use crate::agent::Agent;
use crate::protocol::{
    NotificationType, PeerCaller, PeerInfo, PeerOutcome, PeerResult, PeerState, Request,
    ServerEvent,
};
use crate::tool::{TurnExecutionContext, TurnOrigin};
use jcode_base::peer_groups::{PeerGroup, PeerGroups, PeerMember};
use jcode_swarm_core::validate_swarm_tldr;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use uuid::Uuid;

const MAX_PEER_MESSAGE_CHARS: usize = 8_000;

pub(super) struct PeerServerContext<'a> {
    pub sessions: &'a SessionAgents,
    pub peer_groups: &'a PeerGroups,
    pub exchanges: &'a PeerExchangeRegistry,
    pub swarm_members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub event_history: &'a Arc<RwLock<VecDeque<SwarmEvent>>>,
    pub event_counter: &'a Arc<AtomicU64>,
    pub swarm_event_tx: &'a broadcast::Sender<SwarmEvent>,
    pub turn_coordinator: &'a TurnCoordinator,
}

#[derive(Clone)]
struct SessionPeerSnapshot {
    session_id: String,
    agent: Arc<Mutex<Agent>>,
    identity: Option<PinnedPeerIdentity>,
    live: bool,
}

struct CallerIdentity<'a> {
    context: TurnExecutionContext,
    group: &'a PeerGroup,
    member: &'a PeerMember,
}

fn send_error(id: u64, message: impl Into<String>, tx: &mpsc::UnboundedSender<ServerEvent>) {
    let _ = tx.send(ServerEvent::Error {
        id,
        message: message.into(),
        retry_after_secs: None,
    });
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .to_string()
}

fn peer_result(result: PeerExchangeResult) -> PeerResult {
    let status = match result.recipient_outcome {
        PeerRecipientOutcome::Replied => PeerOutcome::Replied,
        PeerRecipientOutcome::CompletedWithoutReply => PeerOutcome::CompletedWithoutReply,
        PeerRecipientOutcome::Failed | PeerRecipientOutcome::FailedAfterReply => {
            PeerOutcome::Failed
        }
        PeerRecipientOutcome::TimedOut => PeerOutcome::TimedOut,
        PeerRecipientOutcome::Cancelled => PeerOutcome::Cancelled,
    };
    PeerResult {
        status,
        message_id: result.exchange_id,
        from: result.recipient_alias,
        to: result.sender_alias,
        reply: result.reply,
        error: result.detail,
    }
}

async fn session_snapshots(context: &PeerServerContext<'_>) -> Vec<SessionPeerSnapshot> {
    let agents = context
        .sessions
        .read()
        .await
        .iter()
        .map(|(session_id, agent)| (session_id.clone(), Arc::clone(agent)))
        .collect::<Vec<_>>();
    let members = context.swarm_members.read().await;
    agents
        .into_iter()
        .map(|(session_id, agent)| {
            let member = members.get(&session_id);
            SessionPeerSnapshot {
                identity: context.exchanges.pinned_session_identity(&session_id),
                session_id,
                agent,
                live: member.is_some_and(|member| {
                    !member.event_txs.is_empty() || !member.event_tx.is_closed()
                }),
            }
        })
        .collect()
}

fn configured_identity<'a>(
    groups: &'a PeerGroups,
    identity: Option<&PinnedPeerIdentity>,
) -> Option<(&'a PeerGroup, &'a PeerMember)> {
    let identity = identity?;
    let group = groups
        .groups()
        .iter()
        .find(|group| group.name == identity.group_name)?;
    let member = group.members.iter().find(|member| {
        member.alias.eq_ignore_ascii_case(&identity.alias)
            && member.working_dir == identity.working_dir
    })?;
    Some((group, member))
}

fn find_snapshot<'a>(
    snapshots: &'a [SessionPeerSnapshot],
    groups: &PeerGroups,
    group_name: &str,
    alias: &str,
) -> Vec<&'a SessionPeerSnapshot> {
    snapshots
        .iter()
        .filter(|snapshot| {
            configured_identity(groups, snapshot.identity.as_ref()).is_some_and(
                |(group, member)| {
                    group.name == group_name && member.alias.eq_ignore_ascii_case(alias)
                },
            )
        })
        .collect()
}

fn visible_state(matching: &[&SessionPeerSnapshot], coordinator: &TurnCoordinator) -> PeerState {
    let live = matching
        .iter()
        .copied()
        .filter(|snapshot| snapshot.live)
        .collect::<Vec<_>>();
    match live.as_slice() {
        [] => PeerState::Offline,
        [_first, _second, ..] => PeerState::Ambiguous,
        [snapshot] => {
            if coordinator.session_is_busy(&snapshot.session_id)
                || snapshot.agent.try_lock().is_err()
            {
                PeerState::Busy
            } else {
                PeerState::Idle
            }
        }
    }
}

fn validated_caller<'a>(
    caller: &PeerCaller,
    snapshots: &[SessionPeerSnapshot],
    context: &'a PeerServerContext<'a>,
) -> Result<CallerIdentity<'a>, String> {
    let turn = context
        .turn_coordinator
        .validated_context_from_hidden_identity(
            &caller.session_id,
            caller.generation,
            &caller.capability,
        )
        .map_err(|error| format!("Peer caller validation failed: {error}."))?;
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.session_id == caller.session_id)
        .ok_or_else(|| "The peer caller session is no longer live on this server.".to_string())?;
    let (group, member) = configured_identity(context.peer_groups, snapshot.identity.as_ref())
        .ok_or_else(|| "This project is not configured as a peer.".to_string())?;
    Ok(CallerIdentity {
        context: turn,
        group,
        member,
    })
}

fn target_member<'a>(group: &'a PeerGroup, alias: &str) -> Option<&'a PeerMember> {
    group
        .members
        .iter()
        .find(|member| member.alias.eq_ignore_ascii_case(alias.trim()))
}

fn peer_prompt(
    sender: &PeerMember,
    recipient: &PeerMember,
    exchange_id: &str,
    message: &str,
) -> String {
    format!(
        "Verified peer message from {} (`{}`) to {} (`{}`).\nMessage ID: `{}`\n\n{}",
        sender.alias,
        project_name(&sender.working_dir),
        recipient.alias,
        project_name(&recipient.working_dir),
        exchange_id,
        message
    )
}

fn peer_system_reminder(sender: &PeerMember, exchange_id: &str) -> String {
    format!(
        "This is one bounded peer turn from verified peer {} for exchange `{}`. You may call `peer reply` once to return a result to the fixed sender. You cannot call `peer send`, redirect the reply, or create another peer thread.",
        sender.alias, exchange_id
    )
}

fn peer_notification_message(sender: &PeerMember, message: &str) -> String {
    format!(
        "Peer message from {} (`{}`)\n\n{}",
        sender.alias,
        project_name(&sender.working_dir),
        message.trim()
    )
}

pub(super) async fn handle_peer_request(
    request: Request,
    tx: &mpsc::UnboundedSender<ServerEvent>,
    context: PeerServerContext<'_>,
) {
    if !crate::config::config().features.peer_messaging {
        send_error(request.id(), "Peer messaging is disabled.", tx);
        return;
    }
    if let Some(error) = context.peer_groups.load_error() {
        send_error(request.id(), error, tx);
        return;
    }

    let snapshots = session_snapshots(&context).await;
    match request {
        Request::PeerList { id, caller } => {
            let caller = match validated_caller(&caller, &snapshots, &context) {
                Ok(caller) => caller,
                Err(error) => {
                    send_error(id, error, tx);
                    return;
                }
            };
            let peers = caller
                .group
                .members
                .iter()
                .filter(|member| !member.alias.eq_ignore_ascii_case(&caller.member.alias))
                .map(|member| {
                    let matching = find_snapshot(
                        &snapshots,
                        context.peer_groups,
                        &caller.group.name,
                        &member.alias,
                    );
                    PeerInfo {
                        alias: member.alias.clone(),
                        group: caller.group.name.clone(),
                        project: project_name(&member.working_dir),
                        state: visible_state(&matching, context.turn_coordinator),
                    }
                })
                .collect();
            let _ = tx.send(ServerEvent::PeerListResult { id, peers });
        }
        Request::PeerReply {
            id,
            caller,
            message,
        } => {
            let caller = match validated_caller(&caller, &snapshots, &context) {
                Ok(caller) => caller,
                Err(error) => {
                    send_error(id, error, tx);
                    return;
                }
            };
            let exchange_id = match &caller.context.origin {
                TurnOrigin::PeerInbound { exchange_id } => exchange_id.clone(),
                _ => {
                    send_error(id, "This turn cannot reply to peer messages.", tx);
                    return;
                }
            };
            match context.exchanges.record_reply(&caller.context, message) {
                Ok(()) => {
                    let _ = tx.send(ServerEvent::PeerReplyAccepted {
                        id,
                        message_id: exchange_id,
                    });
                }
                Err(error) => send_error(id, error.to_string(), tx),
            }
        }
        Request::PeerCancel { id, caller } => {
            let caller = match validated_caller(&caller, &snapshots, &context) {
                Ok(caller) => caller,
                Err(error) => {
                    send_error(id, error, tx);
                    return;
                }
            };
            match context.exchanges.cancel_from_sender(&caller.context) {
                Ok(result) => {
                    let _ = tx.send(ServerEvent::PeerCancelled {
                        id,
                        message_id: result.exchange_id,
                    });
                }
                Err(error) => send_error(id, error.to_string(), tx),
            }
        }
        Request::PeerSend {
            id,
            caller,
            to,
            message,
            tldr,
        } => {
            if message.trim().is_empty() {
                send_error(id, "Peer message must not be empty.", tx);
                return;
            }
            if message.chars().count() > MAX_PEER_MESSAGE_CHARS {
                send_error(id, "Peer message must be at most 8,000 characters.", tx);
                return;
            }
            if let Err(error) = validate_swarm_tldr(tldr.as_deref(), &message, "this peer message")
            {
                send_error(id, error, tx);
                return;
            }
            let caller = match validated_caller(&caller, &snapshots, &context) {
                Ok(caller) => caller,
                Err(error) => {
                    send_error(id, error, tx);
                    return;
                }
            };
            if !matches!(caller.context.origin, TurnOrigin::NormalUser) {
                send_error(
                    id,
                    "Peer messages can only be started during a normal user-directed turn.",
                    tx,
                );
                return;
            }
            let Some(recipient_member) = target_member(caller.group, &to) else {
                send_error(
                    id,
                    format!("{} is not a member of your peer group.", to.trim()),
                    tx,
                );
                return;
            };
            let matching = find_snapshot(
                &snapshots,
                context.peer_groups,
                &caller.group.name,
                &recipient_member.alias,
            );
            let live = matching
                .into_iter()
                .filter(|snapshot| snapshot.live)
                .collect::<Vec<_>>();
            let recipient = match live.as_slice() {
                [] => {
                    send_error(
                        id,
                        format!(
                            "{} is not currently available on this jcode server. No message was sent.",
                            recipient_member.alias
                        ),
                        tx,
                    );
                    return;
                }
                [_first, _second, ..] => {
                    send_error(
                        id,
                        format!(
                            "{} has more than one live session, so jcode cannot safely choose one.",
                            recipient_member.alias
                        ),
                        tx,
                    );
                    return;
                }
                [recipient] => *recipient,
            };
            let exchange_id = format!("peer_{}", Uuid::new_v4().simple());
            let pending = match context.turn_coordinator.begin_peer_turn(
                &caller.context,
                &recipient.session_id,
                exchange_id.clone(),
            ) {
                Ok(pending) => pending,
                Err(BeginPeerError::Busy) => {
                    send_error(
                        id,
                        format!("{} is busy. No message was sent.", recipient_member.alias),
                        tx,
                    );
                    return;
                }
                Err(error) => {
                    send_error(id, format!("Peer message was rejected: {error}."), tx);
                    return;
                }
            };
            if recipient.agent.try_lock().is_err() {
                drop(pending);
                send_error(
                    id,
                    format!("{} is busy. No message was sent.", recipient_member.alias),
                    tx,
                );
                return;
            }
            let sender_identity = PeerIdentity {
                session_id: caller.context.server_session_id.clone().unwrap_or_default(),
                alias: caller.member.alias.clone(),
                project_name: project_name(&caller.member.working_dir),
            };
            let recipient_identity = PeerIdentity {
                session_id: recipient.session_id.clone(),
                alias: recipient_member.alias.clone(),
                project_name: project_name(&recipient_member.working_dir),
            };
            let registered =
                match context
                    .exchanges
                    .register(pending, sender_identity, recipient_identity)
                {
                    Ok(registered) => registered,
                    Err(error) => {
                        send_error(id, format!("Peer message was rejected: {error}."), tx);
                        return;
                    }
                };
            let super::peer_exchange::RegisteredPeerExchange {
                recipient_lease,
                waiter,
                ..
            } = registered;
            let recipient_event_tx = session_event_fanout_sender(
                recipient.session_id.clone(),
                Arc::clone(context.swarm_members),
            );
            let _ = recipient_event_tx.send(ServerEvent::Notification {
                from_session: caller.context.server_session_id.clone().unwrap_or_default(),
                from_name: Some(caller.member.alias.clone()),
                notification_type: NotificationType::Message {
                    scope: Some("peer".to_string()),
                    channel: None,
                    tldr: tldr.clone(),
                },
                message: peer_notification_message(caller.member, &message),
            });
            let (completion_tx, completion_rx) = oneshot::channel();
            let swarm = LiveTurnSwarmContext::new(
                context.swarm_members,
                context.swarms_by_id,
                context.event_history,
                context.event_counter,
                context.swarm_event_tx,
            );
            spawn_tracked_live_turn_with_completion(
                &recipient.session_id,
                Arc::clone(&recipient.agent),
                peer_prompt(
                    caller.member,
                    recipient_member,
                    &exchange_id,
                    message.trim(),
                ),
                Some(peer_system_reminder(caller.member, &exchange_id)),
                Some(format!("Peer message from {}", caller.member.alias)),
                swarm,
                recipient_lease,
                Some(completion_tx),
            )
            .await;

            let exchanges = context.exchanges.clone();
            let completion_exchange_id = exchange_id.clone();
            tokio::spawn(async move {
                let completion = completion_rx.await.unwrap_or_else(|_| {
                    Err("The peer recipient turn ended without a completion signal.".to_string())
                });
                let _ = exchanges.finish_recipient(&completion_exchange_id, completion);
            });

            let result = waiter.wait().await;
            let _ = tx.send(ServerEvent::PeerSendResult {
                id,
                result: peer_result(result),
            });
        }
        _ => send_error(request.id(), "unsupported peer request", tx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn peer_notification_identifies_sender_project_and_body() {
        let sender = PeerMember {
            alias: "Eve".to_string(),
            working_dir: PathBuf::from("healthview-app"),
        };

        let message = peer_notification_message(&sender, "  Please review this.  ");

        assert_eq!(
            message,
            "Peer message from Eve (`healthview-app`)\n\nPlease review this."
        );
    }

    #[test]
    fn peer_result_preserves_reply_when_recipient_failed_after_reply() {
        let result = peer_result(PeerExchangeResult {
            exchange_id: "peer_1".to_string(),
            sender_alias: "Eve".to_string(),
            sender_project_name: "sender".to_string(),
            recipient_alias: "Atlas".to_string(),
            recipient_project_name: "recipient".to_string(),
            reply: Some("Reviewed.".to_string()),
            recipient_outcome: PeerRecipientOutcome::FailedAfterReply,
            detail: Some("recipient failed".to_string()),
        });
        assert_eq!(result.status, PeerOutcome::Failed);
        assert_eq!(result.reply.as_deref(), Some("Reviewed."));
        assert_eq!(result.error.as_deref(), Some("recipient failed"));
        assert_eq!(result.from, "Atlas");
        assert_eq!(result.to, "Eve");
    }
}
