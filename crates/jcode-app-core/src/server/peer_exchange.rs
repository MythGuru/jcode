use super::turn_coordinator::{
    ActivePeerReservation, PendingPeerStart, ServerTurnLease, TurnCoordinator,
};
use crate::tool::{TurnExecutionContext, TurnOrigin};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

pub(super) const MAX_PEER_MESSAGE_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PeerIdentity {
    pub session_id: String,
    pub alias: String,
    pub project_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerExchangePhase {
    RecipientRunning,
    ReplyRecorded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerRecipientOutcome {
    Replied,
    CompletedWithoutReply,
    Failed,
    FailedAfterReply,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PeerExchangeResult {
    pub exchange_id: String,
    pub sender_alias: String,
    pub sender_project_name: String,
    pub recipient_alias: String,
    pub recipient_project_name: String,
    pub reply: Option<String>,
    pub recipient_outcome: PeerRecipientOutcome,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegisterExchangeError {
    IdentityMismatch,
    DuplicateExchange,
    SessionAlreadyReserved,
}

impl fmt::Display for RegisterExchangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::IdentityMismatch => "peer identity does not match the reserved turn",
            Self::DuplicateExchange => "peer exchange already exists",
            Self::SessionAlreadyReserved => "a participating session already has a peer exchange",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RegisterExchangeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerReplyError {
    UnknownExchange,
    InvalidTurn,
    WrongRecipient,
    AlreadyReplied,
    MessageTooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerCancelError {
    InvalidTurn,
    UnknownExchange,
    WrongSender,
}

impl fmt::Display for PeerCancelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidTurn => "this turn cannot cancel peer messages",
            Self::UnknownExchange => "the peer exchange is no longer active",
            Self::WrongSender => "this peer exchange is not owned by the caller",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PeerCancelError {}

impl fmt::Display for PeerReplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnknownExchange => "the peer exchange is no longer active",
            Self::InvalidTurn => "this turn cannot reply to peer messages",
            Self::WrongRecipient => "this peer reply is not from the fixed recipient",
            Self::AlreadyReplied => "this peer message has already been replied to",
            Self::MessageTooLong => "peer replies may not exceed 8,000 characters",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PeerReplyError {}

#[derive(Clone)]
pub(super) struct PeerExchangeRegistry {
    coordinator: TurnCoordinator,
    deadline: Duration,
    inner: Arc<Mutex<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    exchanges: HashMap<String, ActiveExchange>,
    session_index: HashMap<String, String>,
}

struct ActiveExchange {
    exchange_id: String,
    sender: PeerIdentity,
    recipient: PeerIdentity,
    created_at: Instant,
    deadline: Instant,
    phase: PeerExchangePhase,
    reply_token_available: bool,
    reply: Option<String>,
    sender_generation: u64,
    recipient_generation: u64,
    recipient_cancellation: InterruptSignal,
    _reservation: ActivePeerReservation,
    result_tx: Option<oneshot::Sender<PeerExchangeResult>>,
}

pub(super) struct RegisteredPeerExchange {
    #[cfg(test)]
    pub recipient_context: TurnExecutionContext,
    #[cfg(test)]
    pub recipient_cancellation: InterruptSignal,
    pub recipient_lease: ServerTurnLease,
    pub waiter: PeerExchangeWaiter,
}

pub(super) struct PeerExchangeWaiter {
    registry: PeerExchangeRegistry,
    exchange_id: String,
    deadline: Instant,
    sender_cancellation: InterruptSignal,
    result_rx: Option<oneshot::Receiver<PeerExchangeResult>>,
    armed: bool,
}

impl PeerExchangeRegistry {
    pub(super) fn new(coordinator: TurnCoordinator, deadline: Duration) -> Self {
        Self {
            coordinator,
            deadline,
            inner: Arc::new(Mutex::new(RegistryState::default())),
        }
    }

    pub(super) fn register(
        &self,
        pending: PendingPeerStart,
        sender: PeerIdentity,
        recipient: PeerIdentity,
    ) -> Result<RegisteredPeerExchange, RegisterExchangeError> {
        let exchange_id = pending.exchange_id().to_string();
        let recipient_context = pending.recipient_context().clone();
        if pending.sender_session_id() != sender.session_id
            || recipient_context.server_session_id.as_deref() != Some(recipient.session_id.as_str())
        {
            return Err(RegisterExchangeError::IdentityMismatch);
        }
        let recipient_generation = recipient_context
            .turn_generation
            .ok_or(RegisterExchangeError::IdentityMismatch)?;
        let sender_generation = pending.sender_generation();

        let mut state = self.lock_state();
        if state.exchanges.contains_key(&exchange_id) {
            return Err(RegisterExchangeError::DuplicateExchange);
        }
        if state.session_index.contains_key(&sender.session_id)
            || state.session_index.contains_key(&recipient.session_id)
        {
            return Err(RegisterExchangeError::SessionAlreadyReserved);
        }

        let mut reservation = pending.commit();
        let sender_cancellation = reservation.sender_cancellation();
        let recipient_cancellation = reservation.recipient_cancellation();
        let recipient_lease = reservation.take_recipient_lease();
        let created_at = Instant::now();
        let deadline = created_at + self.deadline;
        let (result_tx, result_rx) = oneshot::channel();
        let active = ActiveExchange {
            exchange_id: exchange_id.clone(),
            sender: sender.clone(),
            recipient: recipient.clone(),
            created_at,
            deadline,
            phase: PeerExchangePhase::RecipientRunning,
            reply_token_available: true,
            reply: None,
            sender_generation,
            recipient_generation,
            recipient_cancellation: recipient_cancellation.clone(),
            _reservation: reservation,
            result_tx: Some(result_tx),
        };
        state
            .session_index
            .insert(sender.session_id.clone(), exchange_id.clone());
        state
            .session_index
            .insert(recipient.session_id.clone(), exchange_id.clone());
        state.exchanges.insert(exchange_id.clone(), active);
        drop(state);

        Ok(RegisteredPeerExchange {
            #[cfg(test)]
            recipient_context,
            #[cfg(test)]
            recipient_cancellation,
            recipient_lease,
            waiter: PeerExchangeWaiter {
                registry: self.clone(),
                exchange_id,
                deadline,
                sender_cancellation,
                result_rx: Some(result_rx),
                armed: true,
            },
        })
    }

    pub(super) fn record_reply(
        &self,
        context: &TurnExecutionContext,
        reply: String,
    ) -> Result<(), PeerReplyError> {
        if reply.chars().count() > MAX_PEER_MESSAGE_CHARS {
            return Err(PeerReplyError::MessageTooLong);
        }
        self.coordinator
            .validate_context(context)
            .map_err(|_| PeerReplyError::InvalidTurn)?;
        let TurnOrigin::PeerInbound { exchange_id } = &context.origin else {
            return Err(PeerReplyError::InvalidTurn);
        };

        let mut state = self.lock_state();
        let exchange = state
            .exchanges
            .get_mut(exchange_id)
            .ok_or(PeerReplyError::UnknownExchange)?;
        if context.server_session_id.as_deref() != Some(exchange.recipient.session_id.as_str())
            || context.turn_generation != Some(exchange.recipient_generation)
        {
            return Err(PeerReplyError::WrongRecipient);
        }
        if !exchange.reply_token_available {
            return Err(PeerReplyError::AlreadyReplied);
        }
        exchange.reply_token_available = false;
        exchange.reply = Some(reply);
        exchange.phase = PeerExchangePhase::ReplyRecorded;
        Ok(())
    }

    pub(super) fn finish_recipient(
        &self,
        exchange_id: &str,
        result: Result<(), String>,
    ) -> Option<PeerExchangeResult> {
        self.finish(exchange_id, FinishReason::Recipient(result))
    }

    pub(super) fn cancel_exchange(&self, exchange_id: &str) -> Option<PeerExchangeResult> {
        self.finish(exchange_id, FinishReason::Cancelled)
    }

    pub(super) fn cancel_from_sender(
        &self,
        context: &TurnExecutionContext,
    ) -> Result<PeerExchangeResult, PeerCancelError> {
        self.coordinator
            .validate_context(context)
            .map_err(|_| PeerCancelError::InvalidTurn)?;
        if !matches!(context.origin, TurnOrigin::NormalUser) {
            return Err(PeerCancelError::InvalidTurn);
        }
        let session_id = context
            .server_session_id
            .as_deref()
            .ok_or(PeerCancelError::InvalidTurn)?;
        let exchange_id = {
            let state = self.lock_state();
            let exchange_id = state
                .session_index
                .get(session_id)
                .cloned()
                .ok_or(PeerCancelError::UnknownExchange)?;
            let exchange = state
                .exchanges
                .get(&exchange_id)
                .ok_or(PeerCancelError::UnknownExchange)?;
            if exchange.sender.session_id != session_id
                || context.turn_generation != Some(exchange.sender_generation)
            {
                return Err(PeerCancelError::WrongSender);
            }
            exchange_id
        };
        self.cancel_exchange(&exchange_id)
            .ok_or(PeerCancelError::UnknownExchange)
    }

    pub(super) fn remove_session(&self, session_id: &str) -> Option<PeerExchangeResult> {
        let exchange_id = {
            let state = self.lock_state();
            state.session_index.get(session_id).cloned()
        }?;
        let sender_removed = {
            let state = self.lock_state();
            state
                .exchanges
                .get(&exchange_id)
                .is_some_and(|exchange| exchange.sender.session_id == session_id)
        };
        if sender_removed {
            self.finish(&exchange_id, FinishReason::SenderRemoved)
        } else {
            self.finish(&exchange_id, FinishReason::RecipientRemoved)
        }
    }

    fn timeout_exchange(&self, exchange_id: &str) -> Option<PeerExchangeResult> {
        self.finish(exchange_id, FinishReason::TimedOut)
    }

    fn finish(&self, exchange_id: &str, reason: FinishReason) -> Option<PeerExchangeResult> {
        let mut state = self.lock_state();
        let mut exchange = state.exchanges.remove(exchange_id)?;
        state.session_index.remove(&exchange.sender.session_id);
        state.session_index.remove(&exchange.recipient.session_id);
        exchange.reply_token_available = false;

        if matches!(
            reason,
            FinishReason::TimedOut | FinishReason::Cancelled | FinishReason::SenderRemoved
        ) {
            exchange.recipient_cancellation.fire();
        }

        let (recipient_outcome, detail) = match reason {
            FinishReason::Recipient(Ok(())) if exchange.reply.is_some() => {
                (PeerRecipientOutcome::Replied, None)
            }
            FinishReason::Recipient(Ok(())) => (PeerRecipientOutcome::CompletedWithoutReply, None),
            FinishReason::Recipient(Err(error)) if exchange.reply.is_some() => {
                (PeerRecipientOutcome::FailedAfterReply, Some(error))
            }
            FinishReason::Recipient(Err(error)) => (PeerRecipientOutcome::Failed, Some(error)),
            FinishReason::TimedOut => (
                PeerRecipientOutcome::TimedOut,
                Some("The peer exchange timed out. The recipient turn was cancelled.".to_string()),
            ),
            FinishReason::Cancelled | FinishReason::SenderRemoved => (
                PeerRecipientOutcome::Cancelled,
                Some("The peer exchange was cancelled before a reply was delivered.".to_string()),
            ),
            FinishReason::RecipientRemoved => (
                PeerRecipientOutcome::Failed,
                Some(
                    "The recipient session was removed before the peer turn completed.".to_string(),
                ),
            ),
        };
        let result = PeerExchangeResult {
            exchange_id: exchange.exchange_id.clone(),
            sender_alias: exchange.sender.alias.clone(),
            sender_project_name: exchange.sender.project_name.clone(),
            recipient_alias: exchange.recipient.alias.clone(),
            recipient_project_name: exchange.recipient.project_name.clone(),
            reply: exchange.reply.take(),
            recipient_outcome,
            detail,
        };
        crate::logging::info(&format!(
            "PEER_EXCHANGE_TERMINAL exchange_id={} sender={} recipient={} outcome={:?} elapsed_ms={} deadline_reached={} sender_generation={} recipient_generation={}",
            exchange.exchange_id,
            exchange.sender.alias,
            exchange.recipient.alias,
            result.recipient_outcome,
            exchange.created_at.elapsed().as_millis(),
            Instant::now() >= exchange.deadline,
            exchange.sender_generation,
            exchange.recipient_generation,
        ));
        if let Some(result_tx) = exchange.result_tx.take() {
            let _ = result_tx.send(result.clone());
        }
        drop(exchange);
        Some(result)
    }

    #[cfg(test)]
    fn contains_session(&self, session_id: &str) -> bool {
        self.lock_state().session_index.contains_key(session_id)
    }

    #[cfg(test)]
    fn phase(&self, exchange_id: &str) -> Option<PeerExchangePhase> {
        self.lock_state()
            .exchanges
            .get(exchange_id)
            .map(|exchange| exchange.phase)
    }

    fn lock_state(&self) -> MutexGuard<'_, RegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug)]
enum FinishReason {
    Recipient(Result<(), String>),
    TimedOut,
    Cancelled,
    SenderRemoved,
    RecipientRemoved,
}

impl PeerExchangeWaiter {
    pub(super) async fn wait(mut self) -> PeerExchangeResult {
        let mut result_rx = self
            .result_rx
            .take()
            .expect("peer exchange waiter may only be awaited once");
        let deadline = tokio::time::Instant::from_std(self.deadline);
        let result = tokio::select! {
            biased;
            received = &mut result_rx => received.unwrap_or_else(|_| PeerExchangeResult {
                exchange_id: self.exchange_id.clone(),
                sender_alias: String::new(),
                sender_project_name: String::new(),
                recipient_alias: String::new(),
                recipient_project_name: String::new(),
                reply: None,
                recipient_outcome: PeerRecipientOutcome::Failed,
                detail: Some("The peer exchange ended without a terminal server result.".to_string()),
            }),
            _ = self.sender_cancellation.notified() => self
                .registry
                .cancel_exchange(&self.exchange_id)
                .unwrap_or_else(|| terminal_race_result(&self.exchange_id)),
            _ = tokio::time::sleep_until(deadline) => self
                .registry
                .timeout_exchange(&self.exchange_id)
                .unwrap_or_else(|| terminal_race_result(&self.exchange_id)),
        };
        self.armed = false;
        result
    }
}

impl Drop for PeerExchangeWaiter {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.registry.cancel_exchange(&self.exchange_id);
        }
    }
}

fn terminal_race_result(exchange_id: &str) -> PeerExchangeResult {
    PeerExchangeResult {
        exchange_id: exchange_id.to_string(),
        sender_alias: String::new(),
        sender_project_name: String::new(),
        recipient_alias: String::new(),
        recipient_project_name: String::new(),
        reply: None,
        recipient_outcome: PeerRecipientOutcome::Failed,
        detail: Some("The peer exchange terminal outcome raced with cleanup.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::TurnOrigin;

    fn identity(session_id: &str, alias: &str) -> PeerIdentity {
        PeerIdentity {
            session_id: session_id.to_string(),
            alias: alias.to_string(),
            project_name: format!("{alias}-project"),
        }
    }

    fn make_registered(
        deadline: Duration,
    ) -> (
        TurnCoordinator,
        super::super::turn_coordinator::ServerTurnLease,
        PeerExchangeRegistry,
        RegisteredPeerExchange,
    ) {
        let coordinator = TurnCoordinator::default();
        let sender_lease = coordinator
            .begin_server_turn("sender", TurnOrigin::NormalUser)
            .expect("sender lease should start");
        let pending = coordinator
            .begin_peer_turn(
                sender_lease.context(),
                "recipient",
                "exchange-1".to_string(),
            )
            .expect("peer lease should start");
        let registry = PeerExchangeRegistry::new(coordinator.clone(), deadline);
        let registered = registry
            .register(
                pending,
                identity("sender", "Eve"),
                identity("recipient", "Atlas"),
            )
            .expect("exchange should register");
        (coordinator, sender_lease, registry, registered)
    }

    #[tokio::test]
    async fn recipient_may_reply_once_and_result_returns_to_original_waiter() {
        let (_coordinator, _sender, registry, registered) =
            make_registered(Duration::from_secs(60));
        assert!(
            registry
                .record_reply(&registered.recipient_context, "Reviewed.".to_string())
                .is_ok()
        );
        assert_eq!(
            registry.phase("exchange-1"),
            Some(PeerExchangePhase::ReplyRecorded)
        );
        assert_eq!(
            registry.record_reply(&registered.recipient_context, "Again.".to_string()),
            Err(PeerReplyError::AlreadyReplied)
        );

        registry
            .finish_recipient("exchange-1", Ok(()))
            .expect("recipient finish should resolve exchange");
        let result = registered.waiter.wait().await;
        assert_eq!(result.reply.as_deref(), Some("Reviewed."));
        assert_eq!(result.recipient_outcome, PeerRecipientOutcome::Replied);
        assert_eq!(result.sender_alias, "Eve");
        assert_eq!(result.recipient_alias, "Atlas");
        assert!(!registry.contains_session("sender"));
        assert!(!registry.contains_session("recipient"));
    }

    #[tokio::test]
    async fn ready_terminal_result_wins_the_deadline_race() {
        let (_coordinator, _sender, registry, registered) = make_registered(Duration::ZERO);
        registry
            .record_reply(&registered.recipient_context, "Reviewed.".to_string())
            .expect("reply should be recorded");
        registry
            .finish_recipient("exchange-1", Ok(()))
            .expect("recipient finish should resolve exchange");

        let result = registered.waiter.wait().await;

        assert_eq!(result.reply.as_deref(), Some("Reviewed."));
        assert_eq!(result.recipient_outcome, PeerRecipientOutcome::Replied);
    }

    #[tokio::test]
    async fn completion_without_reply_and_failure_after_reply_are_explicit() {
        let (_coordinator, _sender, registry, registered) =
            make_registered(Duration::from_secs(60));
        registry.finish_recipient("exchange-1", Ok(()));
        let result = registered.waiter.wait().await;
        assert_eq!(
            result.recipient_outcome,
            PeerRecipientOutcome::CompletedWithoutReply
        );
        assert_eq!(result.reply, None);

        let (_coordinator, _sender, registry, registered) =
            make_registered(Duration::from_secs(60));
        registry
            .record_reply(&registered.recipient_context, "Partial answer".to_string())
            .expect("reply should record");
        registry.finish_recipient("exchange-1", Err("provider failed".to_string()));
        let result = registered.waiter.wait().await;
        assert_eq!(
            result.recipient_outcome,
            PeerRecipientOutcome::FailedAfterReply
        );
        assert_eq!(result.reply.as_deref(), Some("Partial answer"));
        assert_eq!(result.detail.as_deref(), Some("provider failed"));
    }

    #[tokio::test]
    async fn timeout_cancels_recipient_rejects_late_reply_and_releases_state() {
        let (coordinator, _sender, registry, registered) =
            make_registered(Duration::from_millis(10));
        let recipient_context = registered.recipient_context.clone();
        let recipient_cancellation = registered.recipient_cancellation.clone();
        let result = registered.waiter.wait().await;
        assert_eq!(result.recipient_outcome, PeerRecipientOutcome::TimedOut);
        assert!(recipient_cancellation.is_set());
        assert_eq!(
            registry.record_reply(&recipient_context, "late".to_string()),
            Err(PeerReplyError::UnknownExchange)
        );
        assert!(
            coordinator
                .begin_server_turn("recipient", TurnOrigin::NormalUser)
                .is_err(),
            "the cancelled recipient remains busy until its tracked turn unwinds"
        );
        drop(registered.recipient_lease);
        assert!(
            coordinator
                .begin_server_turn("recipient", TurnOrigin::NormalUser)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn sender_cancellation_and_session_removal_resolve_once() {
        let (coordinator, _sender, _registry, registered) =
            make_registered(Duration::from_secs(60));
        coordinator.cancel_session("sender");
        let result = registered.waiter.wait().await;
        assert_eq!(result.recipient_outcome, PeerRecipientOutcome::Cancelled);

        let (_coordinator, _sender, registry, registered) =
            make_registered(Duration::from_secs(60));
        let recipient_context = registered.recipient_context.clone();
        registry
            .remove_session("recipient")
            .expect("session removal should terminate exchange");
        let result = registered.waiter.wait().await;
        assert_eq!(result.recipient_outcome, PeerRecipientOutcome::Failed);
        assert_eq!(
            registry.record_reply(&recipient_context, "late".to_string()),
            Err(PeerReplyError::UnknownExchange)
        );
    }

    #[test]
    fn dropping_waiter_never_records_success_and_cancels_recipient() {
        let (_coordinator, _sender, registry, registered) =
            make_registered(Duration::from_secs(60));
        let cancellation = registered.recipient_cancellation.clone();
        drop(registered.waiter);
        assert!(cancellation.is_set());
        assert!(!registry.contains_session("sender"));
        assert!(registry.finish_recipient("exchange-1", Ok(())).is_none());
    }
}
