use crate::tool::{TurnCapability, TurnExecutionContext, TurnOrigin};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BeginTurnError {
    Busy,
}

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
}

impl ServerTurnLease {
    pub(super) fn context(&self) -> &TurnExecutionContext {
        &self.context
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn cancellation(&self) -> InterruptSignal {
        self.cancellation.clone()
    }
}

impl Drop for ServerTurnLease {
    fn drop(&mut self) {
        self.coordinator
            .clear_generation(&self.session_id, self.generation);
    }
}

impl TurnCoordinator {
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
        });

        Ok(ServerTurnLease {
            coordinator: self.clone(),
            session_id: session_id.to_string(),
            generation,
            context,
            cancellation,
        })
    }

    pub(super) fn validate_context(
        &self,
        context: &TurnExecutionContext,
    ) -> Result<(), TurnValidationError> {
        let state = self.lock_state();
        Self::validated_active(&state, context).map(|_| ())
    }

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
        active.cancellation.fire();
        true
    }

    pub(super) fn remove_session(&self, session_id: &str) {
        let mut state = self.lock_state();
        if let Some(active) = state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
        {
            active.cancellation.fire();
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
    use super::{BeginTurnError, TurnCoordinator, TurnValidationError};
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
}
