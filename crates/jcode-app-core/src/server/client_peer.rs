use super::live_turn::{LiveTurnSwarmContext, spawn_tracked_live_turn_with_completion};
use super::peer_exchange::{
    PeerExchangeRegistry, PeerExchangeResult, PeerRecipientOutcome, PeerStartError,
    PinnedPeerIdentity, RegisteredPeerExchange,
};
use super::turn_coordinator::{BeginPeerError, TurnCoordinator, TurnValidationError};
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
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast, mpsc, oneshot};

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

struct PreparedPeerSend {
    sender: PeerMember,
    recipient: PeerMember,
    recipient_session_id: String,
    recipient_agent: Arc<Mutex<Agent>>,
    recipient_agent_guard: OwnedMutexGuard<Agent>,
    exchange_id: String,
    registered: RegisteredPeerExchange,
}

fn send_error(id: u64, message: impl Into<String>, tx: &mpsc::UnboundedSender<ServerEvent>) {
    let _ = tx.send(ServerEvent::Error {
        id,
        message: message.into(),
        retry_after_secs: None,
    });
}

fn peer_turn_validation_message(error: TurnValidationError) -> &'static str {
    match error {
        TurnValidationError::OriginNotAllowed => {
            "Peer messages can only be started during a normal user-directed turn."
        }
        TurnValidationError::SendAlreadyConsumed => {
            "This normal turn has already started a peer exchange."
        }
        TurnValidationError::MissingServerIdentity
        | TurnValidationError::NoActiveLease
        | TurnValidationError::GenerationMismatch
        | TurnValidationError::CapabilityMismatch
        | TurnValidationError::OriginMismatch => {
            "This tool call does not have a valid live server turn capability."
        }
    }
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
        from_project: result.recipient_project_name,
        to: result.sender_alias,
        to_project: result.sender_project_name,
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
        .map_err(|error| peer_turn_validation_message(error).to_string())?;
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

async fn begin_peer_send_transaction(
    sender_context: &TurnExecutionContext,
    target_alias: &str,
    context: &PeerServerContext<'_>,
) -> Result<PreparedPeerSend, PeerStartError> {
    // Admission lock order is sessions(read) -> swarm_members(read) -> peer
    // registry/coordinator -> recipient Agent(try_lock only). No awaited mutex is
    // acquired while these map guards are nested, and both map guards are
    // released before rollback or turn startup.
    let sessions = context.sessions.read().await;
    let members = context.swarm_members.read().await;
    let resolved = context.exchanges.resolve_and_register(
        sender_context,
        target_alias,
        context.peer_groups,
        sessions.keys().map(String::as_str),
        |session_id| {
            members
                .get(session_id)
                .is_some_and(|member| !member.event_txs.is_empty() || !member.event_tx.is_closed())
        },
    )?;
    let recipient_agent = Arc::clone(
        sessions
            .get(&resolved.recipient_session_id)
            .expect("atomically resolved peer session must retain its live agent"),
    );
    let recipient_agent_guard = Arc::clone(&recipient_agent).try_lock_owned();
    drop(members);
    drop(sessions);
    let recipient_agent_guard = match recipient_agent_guard {
        Ok(guard) => guard,
        Err(_) => {
            let alias = resolved.recipient.alias.clone();
            let exchange_id = resolved.exchange_id.clone();
            drop(resolved);
            let _ = context.exchanges.cancel_exchange(&exchange_id);
            return Err(PeerStartError::Coordinator {
                alias,
                error: BeginPeerError::Busy,
            });
        }
    };
    if !context
        .exchanges
        .mark_recipient_delivery_started(&resolved.exchange_id)
    {
        return Err(PeerStartError::TargetOffline(
            resolved.recipient.alias.clone(),
        ));
    }

    Ok(PreparedPeerSend {
        sender: resolved.sender,
        recipient: resolved.recipient,
        recipient_session_id: resolved.recipient_session_id,
        recipient_agent,
        recipient_agent_guard,
        exchange_id: resolved.exchange_id,
        registered: resolved.registered,
    })
}

fn peer_start_error_message(error: PeerStartError) -> String {
    match error {
        PeerStartError::InvalidSender(error) => peer_turn_validation_message(error).to_string(),
        PeerStartError::SenderSessionMissing => {
            "The peer caller session is no longer live on this server.".to_string()
        }
        PeerStartError::SenderNotConfigured => {
            "This project is not configured as a peer.".to_string()
        }
        PeerStartError::TargetNotInGroup(alias) => {
            format!("{alias} is not a member of your peer group.")
        }
        PeerStartError::TargetOffline(alias) => {
            format!("{alias} is not currently available on this jcode server. No message was sent.")
        }
        PeerStartError::TargetAmbiguous(alias) => {
            format!("{alias} has more than one live session, so jcode cannot safely choose one.")
        }
        PeerStartError::Coordinator {
            alias,
            error: BeginPeerError::Busy,
        } => format!("{alias} is busy. No message was sent."),
        PeerStartError::Coordinator {
            alias,
            error: BeginPeerError::PeerExchangeInProgress,
        }
        | PeerStartError::Registration {
            alias,
            error: super::peer_exchange::RegisterExchangeError::SessionAlreadyReserved,
        } => format!("{alias} is already handling another peer exchange."),
        PeerStartError::Coordinator {
            error: BeginPeerError::InvalidSender(error),
            ..
        } => peer_turn_validation_message(error).to_string(),
        PeerStartError::Coordinator { error, .. } => {
            format!("Peer message was rejected: {error}.")
        }
        PeerStartError::Registration { error, .. } => {
            format!("Peer message was rejected: {error}.")
        }
    }
}

fn send_peer_start_error(id: u64, error: PeerStartError, tx: &mpsc::UnboundedSender<ServerEvent>) {
    send_error(id, peer_start_error_message(error), tx);
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

    match request {
        Request::PeerList { id, caller } => {
            let snapshots = session_snapshots(&context).await;
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
            let snapshots = session_snapshots(&context).await;
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
                    send_error(id, "This turn cannot start or reply to peer messages.", tx);
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
            let snapshots = session_snapshots(&context).await;
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
            let sender_context = match context
                .turn_coordinator
                .validated_context_from_hidden_identity(
                    &caller.session_id,
                    caller.generation,
                    &caller.capability,
                ) {
                Ok(turn) => turn,
                Err(error) => {
                    send_error(id, peer_turn_validation_message(error), tx);
                    return;
                }
            };
            if !matches!(sender_context.origin, TurnOrigin::NormalUser) {
                send_error(
                    id,
                    "Peer messages can only be started during a normal user-directed turn.",
                    tx,
                );
                return;
            }
            let prepared = match begin_peer_send_transaction(&sender_context, &to, &context).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    send_peer_start_error(id, error, tx);
                    return;
                }
            };
            let PreparedPeerSend {
                sender,
                recipient,
                recipient_session_id,
                recipient_agent,
                recipient_agent_guard,
                exchange_id,
                registered,
            } = prepared;
            let super::peer_exchange::RegisteredPeerExchange {
                recipient_lease,
                waiter,
                ..
            } = registered;
            let recipient_event_tx = session_event_fanout_sender(
                recipient_session_id.clone(),
                Arc::clone(context.swarm_members),
            );
            let _ = recipient_event_tx.send(ServerEvent::Notification {
                from_session: sender_context.server_session_id.clone().unwrap_or_default(),
                from_name: Some(sender.alias.clone()),
                notification_type: NotificationType::Message {
                    scope: Some("peer".to_string()),
                    channel: None,
                    tldr: tldr.clone(),
                },
                message: peer_notification_message(&sender, &message),
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
                &recipient_session_id,
                Arc::clone(&recipient_agent),
                Some(recipient_agent_guard),
                peer_prompt(&sender, &recipient, &exchange_id, message.trim()),
                Some(peer_system_reminder(&sender, &exchange_id)),
                Some(format!("Peer message from {}", sender.alias)),
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
    use crate::message::{Message, ToolDefinition};
    use crate::provider::{EventStream, Provider};
    use crate::session::Session;
    use crate::tool::Registry;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    struct UnusedProvider;

    #[async_trait]
    impl Provider for UnusedProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            Err(anyhow::anyhow!(
                "atomic reservation test does not run the model"
            ))
        }

        fn name(&self) -> &str {
            "unused"
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(Self)
        }
    }

    async fn test_agent(session_id: &str) -> Arc<Mutex<Agent>> {
        let provider: Arc<dyn Provider> = Arc::new(UnusedProvider);
        let registry = Registry::new(provider.clone()).await;
        let session = Session::create_with_id(session_id.to_string(), None, None);
        Arc::new(Mutex::new(Agent::new_with_session(
            provider, registry, session, None,
        )))
    }

    fn live_member(
        session_id: &str,
        working_dir: PathBuf,
    ) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            SwarmMember {
                session_id: session_id.to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: Some(working_dir),
                swarm_id: None,
                swarm_enabled: false,
                status: "ready".to_string(),
                detail: None,
                task_label: None,
                friendly_name: None,
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            },
            event_rx,
        )
    }

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
        assert_eq!(result.from_project, "recipient");
        assert_eq!(result.to, "Eve");
        assert_eq!(result.to_project, "sender");
    }

    #[test]
    fn peer_start_errors_preserve_exact_specification_distinctions() {
        assert_eq!(
            peer_start_error_message(PeerStartError::Coordinator {
                alias: "Atlas".to_string(),
                error: BeginPeerError::InvalidSender(
                    super::super::turn_coordinator::TurnValidationError::SendAlreadyConsumed,
                ),
            }),
            "This normal turn has already started a peer exchange."
        );
        assert_eq!(
            peer_start_error_message(PeerStartError::Coordinator {
                alias: "Atlas".to_string(),
                error: BeginPeerError::PeerExchangeInProgress,
            }),
            "Atlas is already handling another peer exchange."
        );
        assert_eq!(
            peer_start_error_message(PeerStartError::Coordinator {
                alias: "Atlas".to_string(),
                error: BeginPeerError::Busy,
            }),
            "Atlas is busy. No message was sent."
        );
        assert_eq!(
            peer_start_error_message(PeerStartError::Registration {
                alias: "Atlas".to_string(),
                error: super::super::peer_exchange::RegisterExchangeError::SessionAlreadyReserved,
            }),
            "Atlas is already handling another peer exchange."
        );
    }

    #[test]
    fn invalid_live_turn_errors_use_the_exact_specification_text() {
        use super::super::turn_coordinator::TurnValidationError;

        assert_eq!(
            peer_turn_validation_message(TurnValidationError::CapabilityMismatch),
            "This tool call does not have a valid live server turn capability."
        );
        assert_eq!(
            peer_turn_validation_message(TurnValidationError::OriginNotAllowed),
            "Peer messages can only be started during a normal user-directed turn."
        );
        assert_eq!(
            peer_turn_validation_message(TurnValidationError::SendAlreadyConsumed),
            "This normal turn has already started a peer exchange."
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_resolution_and_reservation_block_competing_live_attachment() {
        let home = tempfile::TempDir::new().expect("peer config home");
        let eve_dir = tempfile::TempDir::new().expect("Eve project");
        let atlas_dir = tempfile::TempDir::new().expect("Atlas project");
        let config = serde_json::json!({
            "version": 1,
            "groups": [{
                "name": "reviewers",
                "members": [
                    { "alias": "Eve", "working_dir": eve_dir.path() },
                    { "alias": "Atlas", "working_dir": atlas_dir.path() }
                ]
            }]
        });
        std::fs::write(
            home.path().join("peer-groups.json"),
            serde_json::to_vec(&config).expect("serialize peer config"),
        )
        .expect("write peer config");
        let groups = PeerGroups::load_from_jcode_home(home.path()).expect("load peer groups");

        let coordinator = TurnCoordinator::default();
        let exchanges = PeerExchangeRegistry::new(coordinator.clone(), Duration::from_secs(60));
        let sender_id = "sender";
        let recipient_id = "recipient";
        let duplicate_id = "recipient-duplicate";
        exchanges.pin_or_invalidate_session(sender_id, eve_dir.path(), &groups);
        exchanges.pin_or_invalidate_session(recipient_id, atlas_dir.path(), &groups);
        exchanges.pin_or_invalidate_session(duplicate_id, atlas_dir.path(), &groups);

        let sender_agent = test_agent(sender_id).await;
        let recipient_agent = test_agent(recipient_id).await;
        let duplicate_agent = test_agent(duplicate_id).await;
        let sessions = Arc::new(RwLock::new(HashMap::from([
            (sender_id.to_string(), sender_agent),
            (recipient_id.to_string(), recipient_agent),
        ])));
        let (sender_member, _sender_events) = live_member(sender_id, eve_dir.path().to_path_buf());
        let (recipient_member, _recipient_events) =
            live_member(recipient_id, atlas_dir.path().to_path_buf());
        let (duplicate_member, _duplicate_events) =
            live_member(duplicate_id, atlas_dir.path().to_path_buf());
        let swarm_members = Arc::new(RwLock::new(HashMap::from([
            (sender_id.to_string(), sender_member),
            (recipient_id.to_string(), recipient_member),
        ])));
        let sender_lease = coordinator
            .begin_server_turn(sender_id, TurnOrigin::NormalUser)
            .expect("sender turn");
        let sender_context = sender_lease.context().clone();

        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (proceed_tx, proceed_rx) = std::sync::mpsc::sync_channel(1);
        exchanges.set_test_atomic_start_hook(reached_tx, proceed_rx);

        let start_task = tokio::spawn({
            let sessions = Arc::clone(&sessions);
            let swarm_members = Arc::clone(&swarm_members);
            let groups = groups.clone();
            let exchanges = exchanges.clone();
            let coordinator = coordinator.clone();
            async move {
                let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
                let event_history = Arc::new(RwLock::new(VecDeque::new()));
                let event_counter = Arc::new(AtomicU64::new(0));
                let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(4);
                let context = PeerServerContext {
                    sessions: &sessions,
                    peer_groups: &groups,
                    exchanges: &exchanges,
                    swarm_members: &swarm_members,
                    swarms_by_id: &swarms_by_id,
                    event_history: &event_history,
                    event_counter: &event_counter,
                    swarm_event_tx: &swarm_event_tx,
                    turn_coordinator: &coordinator,
                };
                begin_peer_send_transaction(&sender_context, "Atlas", &context).await
            }
        });

        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("peer start reached resolved target");
        })
        .await
        .expect("wait for atomic start hook");

        let mut competing_attach = tokio::spawn({
            let sessions = Arc::clone(&sessions);
            let swarm_members = Arc::clone(&swarm_members);
            async move {
                sessions
                    .write()
                    .await
                    .insert(duplicate_id.to_string(), duplicate_agent);
                swarm_members
                    .write()
                    .await
                    .insert(duplicate_id.to_string(), duplicate_member);
            }
        });

        let competing_attach_won =
            tokio::time::timeout(Duration::from_millis(100), &mut competing_attach)
                .await
                .is_ok();
        proceed_tx.send(()).expect("release peer start");
        if !competing_attach_won {
            competing_attach.await.expect("competing attachment");
        }
        let prepared = start_task
            .await
            .expect("peer start task")
            .expect("peer start succeeds");
        drop(prepared);
        drop(sender_lease);

        assert!(
            !competing_attach_won,
            "a second live attachment entered between target resolution and dual-session reservation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_send_rejects_locked_recipient_then_allows_same_turn_retry() {
        let home = tempfile::TempDir::new().expect("peer config home");
        let eve_dir = tempfile::TempDir::new().expect("Eve project");
        let atlas_dir = tempfile::TempDir::new().expect("Atlas project");
        let config = serde_json::json!({
            "version": 1,
            "groups": [{
                "name": "reviewers",
                "members": [
                    { "alias": "Eve", "working_dir": eve_dir.path() },
                    { "alias": "Atlas", "working_dir": atlas_dir.path() }
                ]
            }]
        });
        std::fs::write(
            home.path().join("peer-groups.json"),
            serde_json::to_vec(&config).expect("serialize peer config"),
        )
        .expect("write peer config");
        let groups = PeerGroups::load_from_jcode_home(home.path()).expect("load peer groups");

        let coordinator = TurnCoordinator::default();
        let exchanges = PeerExchangeRegistry::new(coordinator.clone(), Duration::from_millis(50));
        let sender_id = "sender-busy-agent";
        let recipient_id = "recipient-busy-agent";
        exchanges.pin_or_invalidate_session(sender_id, eve_dir.path(), &groups);
        exchanges.pin_or_invalidate_session(recipient_id, atlas_dir.path(), &groups);

        let sender_agent = test_agent(sender_id).await;
        let recipient_agent = test_agent(recipient_id).await;
        let sessions = Arc::new(RwLock::new(HashMap::from([
            (sender_id.to_string(), sender_agent),
            (recipient_id.to_string(), Arc::clone(&recipient_agent)),
        ])));
        let (sender_member, _sender_events) = live_member(sender_id, eve_dir.path().to_path_buf());
        let (recipient_member, _recipient_events) =
            live_member(recipient_id, atlas_dir.path().to_path_buf());
        let swarm_members = Arc::new(RwLock::new(HashMap::from([
            (sender_id.to_string(), sender_member),
            (recipient_id.to_string(), recipient_member),
        ])));
        let swarms_by_id = Arc::new(RwLock::new(HashMap::new()));
        let event_history = Arc::new(RwLock::new(VecDeque::new()));
        let event_counter = Arc::new(AtomicU64::new(0));
        let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(4);
        let sender_lease = coordinator
            .begin_server_turn(sender_id, TurnOrigin::NormalUser)
            .expect("sender turn");
        let sender_context = sender_lease.context().clone();
        let context = PeerServerContext {
            sessions: &sessions,
            peer_groups: &groups,
            exchanges: &exchanges,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            event_history: &event_history,
            event_counter: &event_counter,
            swarm_event_tx: &swarm_event_tx,
            turn_coordinator: &coordinator,
        };

        let busy_guard = recipient_agent.lock().await;
        let message_count_before = busy_guard.message_count();
        let result = begin_peer_send_transaction(&sender_context, "Atlas", &context).await;
        match result {
            Err(PeerStartError::Coordinator {
                alias,
                error: BeginPeerError::Busy,
            }) => assert_eq!(alias, "Atlas"),
            Err(other) => panic!("expected Atlas busy rejection, got {other:?}"),
            Ok(prepared) => {
                let exchange_id = prepared.exchange_id.clone();
                drop(prepared);
                let _ = exchanges.cancel_exchange(&exchange_id);
                panic!("locked recipient Agent was admitted to peer delivery");
            }
        }
        assert_eq!(
            exchanges.active_exchange_count(),
            0,
            "busy rejection must roll back the exchange registry"
        );
        assert!(
            !coordinator.session_is_busy(recipient_id),
            "busy rejection must roll back the recipient coordinator lease"
        );

        drop(busy_guard);
        let retry = begin_peer_send_transaction(&sender_context, "Atlas", &context).await;
        let prepared = retry.expect("busy rejection must preserve the same-turn send permit");
        drop(prepared);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            recipient_agent.lock().await.message_count(),
            message_count_before,
            "a rejected peer body must never appear later in the recipient transcript"
        );
        drop(sender_lease);
    }
}
