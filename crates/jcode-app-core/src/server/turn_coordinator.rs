use crate::tool::{TurnCapability, TurnExecutionContext, TurnOrigin};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

#[cfg(test)]
static ACTIVE_SERVER_CAPTURE_WATCHERS: std::sync::LazyLock<
    Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
struct ActiveServerCaptureWatcher {
    session_id: String,
}

#[cfg(test)]
impl ActiveServerCaptureWatcher {
    fn new(session_id: String) -> Self {
        ACTIVE_SERVER_CAPTURE_WATCHERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.clone());
        Self { session_id }
    }
}

#[cfg(test)]
impl Drop for ActiveServerCaptureWatcher {
    fn drop(&mut self) {
        ACTIVE_SERVER_CAPTURE_WATCHERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.session_id);
    }
}

#[cfg(test)]
pub(super) fn server_capture_watcher_active(session_id: &str) -> bool {
    ACTIVE_SERVER_CAPTURE_WATCHERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(session_id)
}

struct AbortTaskOnDrop<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> AbortTaskOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn abort_and_join(&mut self) -> Result<T, tokio::task::JoinError> {
        let handle = self
            .handle
            .take()
            .expect("abort-on-drop task must retain its handle until joined");
        handle.abort();
        handle.await
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct TurnCoordinator {
    inner: Arc<Mutex<CoordinatorState>>,
}

#[derive(Default)]
struct CoordinatorState {
    sessions: HashMap<String, SessionState>,
    reservations: HashMap<String, String>,
}

#[derive(Default)]
struct SessionState {
    last_generation: u64,
    active: Option<ActiveTurn>,
}

struct ActiveTurn {
    generation: u64,
    capability: TurnCapability,
    origin: TurnOrigin,
    can_send: bool,
    cancellation: InterruptSignal,
    launch_state: Arc<AtomicU8>,
}

const TURN_LAUNCH_PENDING: u8 = 0;
const TURN_LAUNCH_STARTED: u8 = 1;
const TURN_LAUNCH_CANCELLED: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BeginTurnError {
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BeginPeerError {
    SameSession,
    Busy,
    PeerExchangeInProgress,
    InvalidSender(TurnValidationError),
}

impl fmt::Display for BeginPeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SameSession => {
                formatter.write_str("a session cannot send a peer message to itself")
            }
            Self::Busy => formatter.write_str("a participating session is busy"),
            Self::PeerExchangeInProgress => {
                formatter.write_str("the recipient already has an active peer exchange")
            }
            Self::InvalidSender(error) => write!(formatter, "invalid sender turn: {error}"),
        }
    }
}

impl std::error::Error for BeginPeerError {}

impl fmt::Display for BeginTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("session already has an active turn"),
        }
    }
}

impl std::error::Error for BeginTurnError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnValidationError {
    MissingServerIdentity,
    NoActiveLease,
    GenerationMismatch,
    CapabilityMismatch,
    OriginMismatch,
    OriginNotAllowed,
    SendAlreadyConsumed,
}

impl fmt::Display for TurnValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingServerIdentity => "turn is missing server-minted identity",
            Self::NoActiveLease => "session has no active turn lease",
            Self::GenerationMismatch => "turn generation is stale",
            Self::CapabilityMismatch => "turn capability is invalid",
            Self::OriginMismatch => "turn origin does not match its active lease",
            Self::OriginNotAllowed => "turn origin cannot perform this operation",
            Self::SendAlreadyConsumed => "this turn already used its peer send permit",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for TurnValidationError {}

pub(super) struct ServerTurnLease {
    coordinator: TurnCoordinator,
    session_id: String,
    generation: u64,
    context: TurnExecutionContext,
    cancellation: InterruptSignal,
    launch_state: Arc<AtomicU8>,
}

pub(super) struct PendingPeerStart {
    coordinator: TurnCoordinator,
    exchange_id: String,
    sender_session_id: String,
    sender_generation: u64,
    sender_cancellation: InterruptSignal,
    recipient_lease: Option<ServerTurnLease>,
    committed: bool,
}

pub(super) struct ActivePeerReservation {
    coordinator: TurnCoordinator,
    exchange_id: String,
    sender_session_id: String,
    sender_generation: u64,
    sender_cancellation: InterruptSignal,
    recipient_lease: Option<ServerTurnLease>,
    delivery_started: bool,
}

impl PendingPeerStart {
    pub(super) fn exchange_id(&self) -> &str {
        &self.exchange_id
    }

    pub(super) fn sender_session_id(&self) -> &str {
        &self.sender_session_id
    }

    pub(super) fn sender_generation(&self) -> u64 {
        self.sender_generation
    }

    pub(super) fn recipient_context(&self) -> &TurnExecutionContext {
        self.recipient_lease
            .as_ref()
            .expect("pending peer start must retain its recipient lease")
            .context()
    }

    pub(super) fn commit(mut self) -> ActivePeerReservation {
        self.committed = true;
        ActivePeerReservation {
            coordinator: self.coordinator.clone(),
            exchange_id: self.exchange_id.clone(),
            sender_session_id: self.sender_session_id.clone(),
            sender_generation: self.sender_generation,
            sender_cancellation: self.sender_cancellation.clone(),
            recipient_lease: self.recipient_lease.take(),
            delivery_started: false,
        }
    }
}

impl Drop for PendingPeerStart {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.recipient_lease.take();
        self.coordinator.rollback_peer_start(
            &self.exchange_id,
            &self.sender_session_id,
            self.sender_generation,
        );
    }
}

impl ActivePeerReservation {
    #[cfg(test)]
    pub(super) fn exchange_id(&self) -> &str {
        &self.exchange_id
    }

    pub(super) fn sender_cancellation(&self) -> InterruptSignal {
        self.sender_cancellation.clone()
    }

    pub(super) fn recipient_lease(&self) -> &ServerTurnLease {
        self.recipient_lease
            .as_ref()
            .expect("active peer reservation must retain its recipient lease")
    }

    pub(super) fn recipient_cancellation(&self) -> InterruptSignal {
        self.recipient_lease().cancellation()
    }

    pub(super) fn take_recipient_lease(&mut self) -> ServerTurnLease {
        self.recipient_lease
            .take()
            .expect("active peer reservation must retain its recipient lease until delivery starts")
    }

    pub(super) fn mark_delivery_started(&mut self) {
        self.delivery_started = true;
    }
}

impl Drop for ActivePeerReservation {
    fn drop(&mut self) {
        self.recipient_lease.take();
        self.coordinator.release_peer_reservations(
            &self.exchange_id,
            &self.sender_session_id,
            self.sender_generation,
            !self.delivery_started,
        );
    }
}

impl ServerTurnLease {
    pub(super) fn context(&self) -> &TurnExecutionContext {
        &self.context
    }

    #[cfg(test)]
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn cancellation(&self) -> InterruptSignal {
        self.cancellation.clone()
    }

    pub(super) fn commit_launch(&self) -> bool {
        self.launch_state
            .compare_exchange(
                TURN_LAUNCH_PENDING,
                TURN_LAUNCH_STARTED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }
}

impl Drop for ServerTurnLease {
    fn drop(&mut self) {
        self.coordinator
            .clear_generation(&self.session_id, self.generation);
    }
}

impl TurnCoordinator {
    pub(super) async fn run_server_capture(
        &self,
        session_id: &str,
        agent: Arc<tokio::sync::Mutex<crate::agent::Agent>>,
        message: &str,
        turn_kind: &'static str,
    ) -> anyhow::Result<String> {
        let lease = self
            .begin_server_turn(
                session_id,
                TurnOrigin::ServerInitiated {
                    kind: turn_kind.to_string(),
                },
            )
            .map_err(|error| anyhow::anyhow!(error))?;
        let turn_execution = lease.context().clone();
        let lease_cancellation = lease.cancellation();
        if !lease.commit_launch() {
            return Err(anyhow::anyhow!(
                "The turn was cancelled before message delivery."
            ));
        }
        let _lease = lease;
        let mut agent = agent.lock().await;
        let agent_shutdown = agent.graceful_shutdown_signal();
        let watcher_shutdown = agent_shutdown.clone();
        #[cfg(test)]
        let watcher_session_id = session_id.to_string();
        let mut cancellation_watcher = AbortTaskOnDrop::new(tokio::spawn(async move {
            #[cfg(test)]
            let _active_watcher = ActiveServerCaptureWatcher::new(watcher_session_id);
            lease_cancellation.notified().await;
            watcher_shutdown.fire();
            watcher_shutdown.epoch()
        }));
        let result = agent.run_once_capture(message, turn_execution).await;
        if let Ok(cancel_epoch) = cancellation_watcher.abort_and_join().await {
            agent_shutdown.reset_if_epoch(cancel_epoch);
        }
        result
    }

    pub(super) fn validated_context_from_hidden_identity(
        &self,
        session_id: &str,
        generation: u64,
        capability: &str,
    ) -> Result<TurnExecutionContext, TurnValidationError> {
        let state = self.lock_state();
        let active = state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
            .ok_or(TurnValidationError::NoActiveLease)?;
        if active.generation != generation {
            return Err(TurnValidationError::GenerationMismatch);
        }
        if active.capability.expose_secret() != capability {
            return Err(TurnValidationError::CapabilityMismatch);
        }
        Ok(TurnExecutionContext {
            origin: active.origin.clone(),
            server_session_id: Some(session_id.to_string()),
            turn_generation: Some(generation),
            turn_capability: Some(active.capability.clone()),
        })
    }

    pub(super) fn session_is_busy(&self, session_id: &str) -> bool {
        let state = self.lock_state();
        state.reservations.contains_key(session_id)
            || state
                .sessions
                .get(session_id)
                .is_some_and(|session| session.active.is_some())
    }

    pub(super) fn begin_server_turn(
        &self,
        session_id: &str,
        origin: TurnOrigin,
    ) -> Result<ServerTurnLease, BeginTurnError> {
        let mut state = self.lock_state();
        if state.reservations.contains_key(session_id) {
            return Err(BeginTurnError::Busy);
        }

        let session = state.sessions.entry(session_id.to_string()).or_default();
        if session.active.is_some() {
            return Err(BeginTurnError::Busy);
        }

        session.last_generation = session.last_generation.saturating_add(1);
        let generation = session.last_generation;
        let capability = TurnCapability::new(format!("turn_{}", Uuid::new_v4().simple()));
        let cancellation = InterruptSignal::new();
        let launch_state = Arc::new(AtomicU8::new(TURN_LAUNCH_PENDING));
        let context = TurnExecutionContext {
            origin: origin.clone(),
            server_session_id: Some(session_id.to_string()),
            turn_generation: Some(generation),
            turn_capability: Some(capability.clone()),
        };
        session.active = Some(ActiveTurn {
            generation,
            capability,
            can_send: matches!(origin, TurnOrigin::NormalUser),
            origin,
            cancellation: cancellation.clone(),
            launch_state: Arc::clone(&launch_state),
        });

        Ok(ServerTurnLease {
            coordinator: self.clone(),
            session_id: session_id.to_string(),
            generation,
            context,
            cancellation,
            launch_state,
        })
    }

    pub(super) fn validate_context(
        &self,
        context: &TurnExecutionContext,
    ) -> Result<(), TurnValidationError> {
        let state = self.lock_state();
        Self::validated_active(&state, context).map(|_| ())
    }

    pub(super) fn begin_peer_turn(
        &self,
        sender_context: &TurnExecutionContext,
        recipient_session_id: &str,
        exchange_id: String,
    ) -> Result<PendingPeerStart, BeginPeerError> {
        let mut state = self.lock_state();
        let (sender_session_id, sender_generation, _) =
            Self::hidden_identity(sender_context).map_err(BeginPeerError::InvalidSender)?;
        if sender_session_id == recipient_session_id {
            return Err(BeginPeerError::SameSession);
        }

        let sender = Self::validated_active(&state, sender_context)
            .map_err(BeginPeerError::InvalidSender)?;
        if !matches!(sender.origin, TurnOrigin::NormalUser) {
            return Err(BeginPeerError::InvalidSender(
                TurnValidationError::OriginNotAllowed,
            ));
        }
        if !sender.can_send {
            return Err(BeginPeerError::InvalidSender(
                TurnValidationError::SendAlreadyConsumed,
            ));
        }
        if state.reservations.contains_key(sender_session_id) {
            return Err(BeginPeerError::InvalidSender(
                TurnValidationError::SendAlreadyConsumed,
            ));
        }
        if state.reservations.contains_key(recipient_session_id) {
            return Err(BeginPeerError::PeerExchangeInProgress);
        }
        if state
            .sessions
            .get(recipient_session_id)
            .is_some_and(|session| session.active.is_some())
        {
            return Err(BeginPeerError::Busy);
        }

        let sender_cancellation = state
            .sessions
            .get_mut(sender_session_id)
            .and_then(|session| session.active.as_mut())
            .expect("validated sender must remain active while coordinator lock is held")
            .cancellation
            .clone();
        state
            .sessions
            .get_mut(sender_session_id)
            .and_then(|session| session.active.as_mut())
            .expect("validated sender must remain active while coordinator lock is held")
            .can_send = false;

        state
            .reservations
            .insert(sender_session_id.to_string(), exchange_id.clone());
        state
            .reservations
            .insert(recipient_session_id.to_string(), exchange_id.clone());

        let recipient = state
            .sessions
            .entry(recipient_session_id.to_string())
            .or_default();
        recipient.last_generation = recipient.last_generation.saturating_add(1);
        let recipient_generation = recipient.last_generation;
        let recipient_capability = TurnCapability::new(format!("turn_{}", Uuid::new_v4().simple()));
        let recipient_cancellation = InterruptSignal::new();
        let recipient_launch_state = Arc::new(AtomicU8::new(TURN_LAUNCH_PENDING));
        let recipient_origin = TurnOrigin::PeerInbound {
            exchange_id: exchange_id.clone(),
        };
        let recipient_context = TurnExecutionContext {
            origin: recipient_origin.clone(),
            server_session_id: Some(recipient_session_id.to_string()),
            turn_generation: Some(recipient_generation),
            turn_capability: Some(recipient_capability.clone()),
        };
        recipient.active = Some(ActiveTurn {
            generation: recipient_generation,
            capability: recipient_capability,
            origin: recipient_origin,
            can_send: false,
            cancellation: recipient_cancellation.clone(),
            launch_state: Arc::clone(&recipient_launch_state),
        });

        Ok(PendingPeerStart {
            coordinator: self.clone(),
            exchange_id,
            sender_session_id: sender_session_id.to_string(),
            sender_generation,
            sender_cancellation,
            recipient_lease: Some(ServerTurnLease {
                coordinator: self.clone(),
                session_id: recipient_session_id.to_string(),
                generation: recipient_generation,
                context: recipient_context,
                cancellation: recipient_cancellation,
                launch_state: recipient_launch_state,
            }),
            committed: false,
        })
    }

    #[cfg(test)]
    pub(super) fn consume_send_permit(
        &self,
        context: &TurnExecutionContext,
    ) -> Result<(), TurnValidationError> {
        let mut state = self.lock_state();
        let active = Self::validated_active_mut(&mut state, context)?;
        if !matches!(active.origin, TurnOrigin::NormalUser) {
            return Err(TurnValidationError::OriginNotAllowed);
        }
        if !active.can_send {
            return Err(TurnValidationError::SendAlreadyConsumed);
        }
        active.can_send = false;
        Ok(())
    }

    pub(super) fn cancel_session(&self, session_id: &str) -> bool {
        let state = self.lock_state();
        let Some(active) = state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
        else {
            return false;
        };
        Self::cancel_active(active);
        true
    }

    pub(super) fn cancel_generation(&self, session_id: &str, generation: u64) -> bool {
        let state = self.lock_state();
        let Some(active) = state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
            .filter(|active| active.generation == generation)
        else {
            return false;
        };
        Self::cancel_active(active);
        true
    }

    pub(super) fn remove_session(&self, session_id: &str) {
        let mut state = self.lock_state();
        if let Some(active) = state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
        {
            Self::cancel_active(active);
        }
        state.reservations.remove(session_id);
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.active = None;
        }
    }

    fn validated_active<'a>(
        state: &'a CoordinatorState,
        context: &TurnExecutionContext,
    ) -> Result<&'a ActiveTurn, TurnValidationError> {
        let (session_id, generation, capability) = Self::hidden_identity(context)?;
        let active = state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
            .ok_or(TurnValidationError::NoActiveLease)?;
        Self::validate_active(active, context, generation, capability)?;
        Ok(active)
    }

    #[cfg(test)]
    fn validated_active_mut<'a>(
        state: &'a mut CoordinatorState,
        context: &TurnExecutionContext,
    ) -> Result<&'a mut ActiveTurn, TurnValidationError> {
        let (session_id, generation, capability) = Self::hidden_identity(context)?;
        let active = state
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.active.as_mut())
            .ok_or(TurnValidationError::NoActiveLease)?;
        Self::validate_active(active, context, generation, capability)?;
        Ok(active)
    }

    fn hidden_identity(
        context: &TurnExecutionContext,
    ) -> Result<(&str, u64, &TurnCapability), TurnValidationError> {
        match (
            context.server_session_id.as_deref(),
            context.turn_generation,
            context.turn_capability.as_ref(),
        ) {
            (Some(session_id), Some(generation), Some(capability)) => {
                Ok((session_id, generation, capability))
            }
            _ => Err(TurnValidationError::MissingServerIdentity),
        }
    }

    fn validate_active(
        active: &ActiveTurn,
        context: &TurnExecutionContext,
        generation: u64,
        capability: &TurnCapability,
    ) -> Result<(), TurnValidationError> {
        if active.generation != generation {
            return Err(TurnValidationError::GenerationMismatch);
        }
        if &active.capability != capability {
            return Err(TurnValidationError::CapabilityMismatch);
        }
        if active.origin != context.origin {
            return Err(TurnValidationError::OriginMismatch);
        }
        Ok(())
    }

    fn cancel_active(active: &ActiveTurn) {
        let _ = active.launch_state.compare_exchange(
            TURN_LAUNCH_PENDING,
            TURN_LAUNCH_CANCELLED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        active.cancellation.fire();
    }

    fn clear_generation(&self, session_id: &str, generation: u64) -> bool {
        let mut state = self.lock_state();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return false;
        };
        if session
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            session.active = None;
            true
        } else {
            false
        }
    }

    fn rollback_peer_start(
        &self,
        exchange_id: &str,
        sender_session_id: &str,
        sender_generation: u64,
    ) {
        let mut state = self.lock_state();
        Self::remove_matching_reservations(&mut state, exchange_id);
        if let Some(active) = state
            .sessions
            .get_mut(sender_session_id)
            .and_then(|session| session.active.as_mut())
            && active.generation == sender_generation
            && matches!(active.origin, TurnOrigin::NormalUser)
        {
            active.can_send = true;
        }
    }

    fn release_peer_reservations(
        &self,
        exchange_id: &str,
        sender_session_id: &str,
        sender_generation: u64,
        restore_sender_permit: bool,
    ) {
        let mut state = self.lock_state();
        Self::remove_matching_reservations(&mut state, exchange_id);
        if restore_sender_permit
            && let Some(active) = state
                .sessions
                .get_mut(sender_session_id)
                .and_then(|session| session.active.as_mut())
            && active.generation == sender_generation
            && matches!(active.origin, TurnOrigin::NormalUser)
        {
            active.can_send = true;
        }
    }

    fn remove_matching_reservations(state: &mut CoordinatorState, exchange_id: &str) {
        state
            .reservations
            .retain(|_, reserved_exchange_id| reserved_exchange_id != exchange_id);
    }

    #[cfg(test)]
    fn clear_generation_for_test(&self, session_id: &str, generation: u64) -> bool {
        self.clear_generation(session_id, generation)
    }

    fn lock_state(&self) -> MutexGuard<'_, CoordinatorState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::{BeginPeerError, BeginTurnError, TurnCoordinator, TurnValidationError};
    use crate::tool::{TurnCapability, TurnExecutionContext, TurnOrigin};
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn normal_user_lease_has_hidden_server_identity_and_one_send() {
        let coordinator = TurnCoordinator::default();
        let lease = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("normal turn should acquire a lease");

        assert_eq!(lease.context().origin, TurnOrigin::NormalUser);
        assert_eq!(lease.context().server_session_id.as_deref(), Some("sender"));
        assert_eq!(lease.context().turn_generation, Some(1));
        assert!(lease.context().turn_capability.is_some());
        assert!(coordinator.consume_send_permit(lease.context()).is_ok());
        assert_eq!(
            coordinator.consume_send_permit(lease.context()),
            Err(TurnValidationError::SendAlreadyConsumed)
        );
    }

    #[test]
    fn stale_forged_and_cross_session_capabilities_are_rejected() {
        let coordinator = TurnCoordinator::default();
        let lease = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("normal turn should acquire a lease");

        let mut forged = lease.context().clone();
        forged.turn_capability = Some(TurnCapability::new("forged".to_string()));
        assert_eq!(
            coordinator.validate_context(&forged),
            Err(TurnValidationError::CapabilityMismatch)
        );

        let mut wrong_session = lease.context().clone();
        wrong_session.server_session_id = Some("other".to_string());
        assert_eq!(
            coordinator.validate_context(&wrong_session),
            Err(TurnValidationError::NoActiveLease)
        );

        let stale = lease.context().clone();
        drop(lease);
        let replacement = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("replacement turn should acquire a lease");
        assert_eq!(replacement.context().turn_generation, Some(2));
        assert_eq!(
            coordinator.validate_context(&stale),
            Err(TurnValidationError::GenerationMismatch)
        );
    }

    #[test]
    fn server_initiated_and_peer_inbound_leases_cannot_send() {
        let coordinator = TurnCoordinator::default();
        let server_lease = coordinator
            .begin_server_turn(
                "background",
                TurnOrigin::ServerInitiated {
                    kind: "background-completion".to_string(),
                },
            )
            .expect("server turn should acquire a lease");
        assert_eq!(
            coordinator.consume_send_permit(server_lease.context()),
            Err(TurnValidationError::OriginNotAllowed)
        );
        drop(server_lease);

        let peer_lease = coordinator
            .begin_server_turn(
                "recipient",
                TurnOrigin::PeerInbound {
                    exchange_id: "exchange".to_string(),
                },
            )
            .expect("peer turn should acquire a lease");
        assert_eq!(
            coordinator.consume_send_permit(peer_lease.context()),
            Err(TurnValidationError::OriginNotAllowed)
        );
    }

    #[test]
    fn generation_aware_cleanup_cannot_clear_a_newer_turn() {
        let coordinator = TurnCoordinator::default();
        let first = coordinator
            .begin_server_turn("session", TurnOrigin::NormalUser)
            .expect("first turn should acquire a lease");
        let first_generation = first.generation();
        assert!(coordinator.clear_generation_for_test("session", first_generation));

        let second = coordinator
            .begin_server_turn("session", TurnOrigin::NormalUser)
            .expect("second turn should acquire a lease");
        assert_eq!(second.generation(), first_generation + 1);
        drop(first);

        assert!(coordinator.validate_context(second.context()).is_ok());
    }

    #[test]
    fn concurrent_starts_have_exactly_one_winner() {
        let coordinator = TurnCoordinator::default();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let coordinator = coordinator.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                let result = coordinator.begin_server_turn("same", TurnOrigin::NormalUser);
                barrier.wait();
                result
            }));
        }

        barrier.wait();
        barrier.wait();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("thread should finish"))
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(BeginTurnError::Busy)))
                .count(),
            1
        );
    }

    #[test]
    fn missing_hidden_context_is_rejected() {
        let coordinator = TurnCoordinator::default();
        let context = TurnExecutionContext::server_initiated("standalone-server-origin");
        assert_eq!(
            coordinator.validate_context(&context),
            Err(TurnValidationError::MissingServerIdentity)
        );
    }

    #[test]
    fn peer_start_atomically_reserves_both_sessions_and_consumes_sender_send() {
        let coordinator = TurnCoordinator::default();
        let sender = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("sender turn should start");

        let pending = coordinator
            .begin_peer_turn(sender.context(), "recipient", "exchange-1".to_string())
            .expect("peer turn should reserve both sessions");
        assert_eq!(
            pending.recipient_context().origin,
            TurnOrigin::PeerInbound {
                exchange_id: "exchange-1".to_string()
            }
        );
        assert!(matches!(
            coordinator.begin_server_turn("recipient", TurnOrigin::NormalUser),
            Err(BeginTurnError::Busy)
        ));
        assert!(matches!(
            coordinator.begin_peer_turn(sender.context(), "other", "exchange-2".to_string()),
            Err(BeginPeerError::InvalidSender(
                TurnValidationError::SendAlreadyConsumed
            ))
        ));

        let active = pending.commit();
        assert_eq!(active.exchange_id(), "exchange-1");
        assert_eq!(
            coordinator.consume_send_permit(sender.context()),
            Err(TurnValidationError::SendAlreadyConsumed)
        );
        drop(active);
        assert!(
            coordinator
                .begin_server_turn("recipient", TurnOrigin::NormalUser)
                .is_ok(),
            "terminal exchange cleanup must release recipient state"
        );
    }

    #[test]
    fn uncommitted_peer_start_rolls_back_lease_reservations_and_send_permit() {
        let coordinator = TurnCoordinator::default();
        let sender = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("sender turn should start");
        let pending = coordinator
            .begin_peer_turn(sender.context(), "recipient", "exchange-1".to_string())
            .expect("peer start should install a provisional lease");

        drop(pending);

        assert!(coordinator.consume_send_permit(sender.context()).is_ok());
        assert!(
            coordinator
                .begin_server_turn("recipient", TurnOrigin::NormalUser)
                .is_ok()
        );
    }

    #[test]
    fn undelivered_reservation_does_not_restore_a_newer_sender_generation() {
        let coordinator = TurnCoordinator::default();
        let old_sender = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("old sender turn should start");
        let active = coordinator
            .begin_peer_turn(
                old_sender.context(),
                "recipient",
                "exchange-old".to_string(),
            )
            .expect("old peer exchange should reserve both sessions")
            .commit();

        coordinator.remove_session("sender");
        let new_sender = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("new sender generation should start");
        assert!(
            coordinator
                .consume_send_permit(new_sender.context())
                .is_ok()
        );

        drop(active);

        assert_eq!(
            coordinator.consume_send_permit(new_sender.context()),
            Err(TurnValidationError::SendAlreadyConsumed),
            "stale reservation cleanup must not grant a permit to a newer sender turn"
        );
    }

    #[test]
    fn delivered_reservation_keeps_the_sender_permit_consumed() {
        let coordinator = TurnCoordinator::default();
        let sender = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("sender turn should start");
        let mut active = coordinator
            .begin_peer_turn(sender.context(), "recipient", "exchange-1".to_string())
            .expect("peer exchange should reserve both sessions")
            .commit();
        active.mark_delivery_started();

        drop(active);

        assert!(matches!(
            coordinator.begin_peer_turn(sender.context(), "other", "exchange-2".to_string()),
            Err(BeginPeerError::InvalidSender(
                TurnValidationError::SendAlreadyConsumed
            ))
        ));
    }

    #[test]
    fn peer_start_rejects_busy_or_reserved_participants_without_partial_state() {
        let coordinator = TurnCoordinator::default();
        let sender = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("sender turn should start");
        let recipient = coordinator
            .begin_server_turn("recipient", TurnOrigin::NormalUser)
            .expect("recipient turn should start");

        assert!(matches!(
            coordinator.begin_peer_turn(sender.context(), "recipient", "exchange-busy".to_string()),
            Err(BeginPeerError::Busy)
        ));
        drop(recipient);
        assert!(
            coordinator
                .begin_peer_turn(sender.context(), "recipient", "exchange-ok".to_string())
                .is_ok(),
            "a rejected peer start must not consume the sender permit"
        );
    }

    #[test]
    fn peer_start_distinguishes_reserved_recipient_from_ordinary_busy() {
        let coordinator = TurnCoordinator::default();
        let first_sender = coordinator
            .begin_server_turn("first-sender", TurnOrigin::NormalUser)
            .expect("first sender turn should start");
        let second_sender = coordinator
            .begin_server_turn("second-sender", TurnOrigin::NormalUser)
            .expect("second sender turn should start");
        let first_exchange = coordinator
            .begin_peer_turn(
                first_sender.context(),
                "recipient",
                "exchange-1".to_string(),
            )
            .expect("first peer exchange should reserve the recipient");

        assert!(matches!(
            coordinator.begin_peer_turn(
                second_sender.context(),
                "recipient",
                "exchange-2".to_string(),
            ),
            Err(BeginPeerError::PeerExchangeInProgress)
        ));

        drop(first_exchange);
        assert!(
            coordinator
                .begin_peer_turn(
                    second_sender.context(),
                    "recipient",
                    "exchange-3".to_string(),
                )
                .is_ok(),
            "the rejected start must not consume the second sender permit"
        );
    }
}
