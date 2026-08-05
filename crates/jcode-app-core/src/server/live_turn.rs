//! Server-initiated ("wake") turns for live sessions.
//!
//! Several server paths start a full conversation turn in a session without
//! that session's client sending a message: swarm DM/broadcast wake delivery,
//! background-task completion wakes, scheduled-task delivery, and post-reload
//! resume. Those turns must keep the same bookkeeping as client-initiated
//! turns, otherwise the swarm member status stays "ready/idle" while the agent
//! is actually streaming and attached TUIs never learn the turn finished.
//!
//! This module is the single shared implementation: it marks the member
//! `running` while the turn streams, flips it back to `ready` (with a
//! completion report) or `failed` at the end, and fans out a terminal
//! `Done`/`Error` event (id 0) so attached clients can settle the externally
//! started turn in their UI.

use super::client_lifecycle::{
    process_message_streaming_mpsc, process_message_streaming_mpsc_with_guard,
};
use super::peer_exchange::PeerExchangeRegistry;
use super::turn_coordinator::{ServerTurnLease, TurnCoordinator};
use super::{
    SwarmEvent, SwarmMember, session_event_fanout_sender, truncate_detail, update_member_status,
    update_member_status_with_report,
};
use crate::agent::Agent;
use crate::protocol::ServerEvent;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast, oneshot};

type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

/// Swarm bookkeeping handles needed to keep member status accurate around a
/// server-initiated turn.
#[derive(Clone)]
pub(super) struct LiveTurnSwarmContext {
    pub members: Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub swarms_by_id: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub event_history: Arc<RwLock<VecDeque<SwarmEvent>>>,
    pub event_counter: Arc<AtomicU64>,
    pub event_tx: broadcast::Sender<SwarmEvent>,
}

pub(super) struct PeerLaunchFence {
    sessions: SessionAgents,
    exchanges: PeerExchangeRegistry,
    exchange_id: String,
    expected_agent: Arc<Mutex<Agent>>,
}

impl PeerLaunchFence {
    pub(super) fn new(
        sessions: &SessionAgents,
        exchanges: &PeerExchangeRegistry,
        exchange_id: String,
        expected_agent: Arc<Mutex<Agent>>,
    ) -> Self {
        Self {
            sessions: Arc::clone(sessions),
            exchanges: exchanges.clone(),
            exchange_id,
            expected_agent,
        }
    }

    async fn commit(self, recipient_context: &crate::tool::TurnExecutionContext) -> bool {
        let sessions = self.sessions.read().await;
        let same_live_agent = sessions
            .get(
                recipient_context
                    .server_session_id
                    .as_deref()
                    .unwrap_or_default(),
            )
            .is_some_and(|agent| Arc::ptr_eq(agent, &self.expected_agent));
        same_live_agent
            && self
                .exchanges
                .commit_recipient_launch(&self.exchange_id, recipient_context)
    }
}

impl LiveTurnSwarmContext {
    pub(super) fn new(
        members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
        swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
        event_history: &Arc<RwLock<VecDeque<SwarmEvent>>>,
        event_counter: &Arc<AtomicU64>,
        event_tx: &broadcast::Sender<SwarmEvent>,
    ) -> Self {
        Self {
            members: Arc::clone(members),
            swarms_by_id: Arc::clone(swarms_by_id),
            event_history: Arc::clone(event_history),
            event_counter: Arc::clone(event_counter),
            event_tx: event_tx.clone(),
        }
    }
}

/// Return the live agent for `session_id` when the session has at least one
/// live client attachment and its agent is currently idle (lock not held).
pub(super) async fn idle_live_agent(
    session_id: &str,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<Arc<Mutex<Agent>>> {
    let agent = {
        let guard = sessions.read().await;
        guard.get(session_id).cloned()
    }?;

    let has_live_attachments = {
        let members = swarm_members.read().await;
        members
            .get(session_id)
            .map(|member| !member.event_txs.is_empty() || !member.event_tx.is_closed())
            .unwrap_or(false)
    };
    if !has_live_attachments {
        return None;
    }

    let is_idle = agent.try_lock().is_ok();
    is_idle.then_some(agent)
}

/// Spawn `message` as a full tracked turn in a live session.
///
/// Mirrors the client-initiated turn lifecycle: the swarm member is marked
/// `running` before the turn starts and `ready` (with a completion report) or
/// `failed` when it finishes. A synthetic terminal `Done { id: 0 }` (or
/// `Error { id: 0, .. }`) is fanned out to attached clients so their UI can
/// finish rendering the externally started turn.
pub(super) async fn spawn_tracked_live_turn(
    session_id: &str,
    agent: Arc<Mutex<Agent>>,
    message: String,
    system_reminder: Option<String>,
    status_detail: Option<String>,
    swarm: LiveTurnSwarmContext,
    turn_lease: ServerTurnLease,
) {
    spawn_tracked_live_turn_with_completion(
        session_id,
        agent,
        None,
        message,
        system_reminder,
        status_detail,
        swarm,
        turn_lease,
        None,
        None,
    )
    .await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "tracked peer turns additionally report their terminal outcome to the exchange registry"
)]
pub(super) async fn spawn_tracked_live_turn_with_completion(
    session_id: &str,
    agent: Arc<Mutex<Agent>>,
    acquired_agent: Option<OwnedMutexGuard<Agent>>,
    message: String,
    system_reminder: Option<String>,
    status_detail: Option<String>,
    swarm: LiveTurnSwarmContext,
    turn_lease: ServerTurnLease,
    peer_launch_fence: Option<PeerLaunchFence>,
    completion_tx: Option<oneshot::Sender<Result<(), String>>>,
) {
    if let Some(peer_launch_fence) = peer_launch_fence
        && !peer_launch_fence.commit(turn_lease.context()).await
    {
        if let Some(completion_tx) = completion_tx {
            let _ = completion_tx.send(Err(
                "The peer recipient session or exchange ended before message delivery.".to_string(),
            ));
        }
        return;
    }

    // With `acquired_agent`, startup's real order here is recipient Agent ->
    // swarm_members(write), the reverse of admission's map-to-Agent ordering.
    // This cannot form a blocking cycle: admission only uses `try_lock_owned`
    // for its Agent edge and never waits for the Agent, while this status write
    // never locks or waits for an Agent. The Agent guard is released by the
    // spawned turn before either terminal swarm-members status write below.
    update_member_status(
        session_id,
        "running",
        status_detail,
        &swarm.members,
        &swarm.swarms_by_id,
        Some(&swarm.event_history),
        Some(&swarm.event_counter),
        Some(&swarm.event_tx),
    )
    .await;

    let event_tx = session_event_fanout_sender(session_id.to_string(), Arc::clone(&swarm.members));
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        let (result, completion_report) = match acquired_agent {
            Some(agent_guard) => {
                let start_message_index = agent_guard.message_count();
                let (result, agent_guard) = process_message_streaming_mpsc_with_guard(
                    agent_guard,
                    &message,
                    vec![],
                    system_reminder,
                    event_tx.clone(),
                    turn_lease,
                )
                .await;
                let completion_report = if result.is_ok() {
                    agent_guard.latest_assistant_text_after(start_message_index)
                } else {
                    None
                };
                (result, completion_report)
            }
            None => {
                let start_message_index = {
                    let agent_guard = agent.lock().await;
                    agent_guard.message_count()
                };
                let result = process_message_streaming_mpsc(
                    Arc::clone(&agent),
                    &message,
                    vec![],
                    system_reminder,
                    event_tx.clone(),
                    turn_lease,
                )
                .await;
                let completion_report = if result.is_ok() {
                    let agent_guard = agent.lock().await;
                    agent_guard.latest_assistant_text_after(start_message_index)
                } else {
                    None
                };
                (result, completion_report)
            }
        };
        let completion = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| error.to_string());
        match result {
            Ok(()) => {
                update_member_status_with_report(
                    &session_id,
                    "ready",
                    None,
                    completion_report,
                    &swarm.members,
                    &swarm.swarms_by_id,
                    Some(&swarm.event_history),
                    Some(&swarm.event_counter),
                    Some(&swarm.event_tx),
                )
                .await;
                let _ = event_tx.send(ServerEvent::Done { id: 0 });
            }
            Err(error) => {
                crate::logging::error(&format!(
                    "Server-initiated turn failed for live session {}: {}",
                    session_id, error
                ));
                update_member_status(
                    &session_id,
                    "failed",
                    Some(truncate_detail(&error.to_string(), 120)),
                    &swarm.members,
                    &swarm.swarms_by_id,
                    Some(&swarm.event_history),
                    Some(&swarm.event_counter),
                    Some(&swarm.event_tx),
                )
                .await;
                let _ = event_tx.send(ServerEvent::Error {
                    id: 0,
                    message: crate::util::format_error_chain(&error),
                    retry_after_secs: None,
                });
            }
        }
        if let Some(completion_tx) = completion_tx {
            let _ = completion_tx.send(completion);
        }
    });
}

/// Run `message` immediately as a tracked turn if the session is live and
/// idle. Returns `true` when the turn was started.
pub(super) async fn run_live_turn_if_idle(
    session_id: &str,
    message: &str,
    system_reminder: Option<String>,
    sessions: &SessionAgents,
    swarm: LiveTurnSwarmContext,
    turn_coordinator: &TurnCoordinator,
    turn_kind: &'static str,
) -> bool {
    let Some(agent) = idle_live_agent(session_id, sessions, &swarm.members).await else {
        return false;
    };
    let Ok(turn_lease) = turn_coordinator.begin_server_turn(
        session_id,
        crate::tool::TurnOrigin::ServerInitiated {
            kind: turn_kind.to_string(),
        },
    ) else {
        return false;
    };
    if agent.try_lock().is_err() {
        return false;
    }
    let detail = Some(truncate_detail(message, 120)).filter(|detail| !detail.is_empty());
    spawn_tracked_live_turn(
        session_id,
        agent,
        message.to_string(),
        system_reminder,
        detail,
        swarm,
        turn_lease,
    )
    .await;
    true
}
