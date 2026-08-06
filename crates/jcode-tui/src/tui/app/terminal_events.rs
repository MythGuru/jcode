use futures::{Stream, future::poll_fn};
use std::pin::Pin;
use std::task::Poll;

/// Extra terminal events to coalesce after the event that woke the run loop.
/// Keeping this bounded limits how long one wake is spent processing input
/// before the run loop gets another scheduling opportunity.
const MAX_DRAINED_EVENTS_PER_WAKE: usize = 32;

/// Collect the event that woke the run loop plus any events already ready on
/// the same async stream. Crossterm explicitly forbids combining `EventStream`
/// with its synchronous `poll`/`read` API, so batching must stay on the stream.
pub(super) async fn ready_event_batch<S>(first: S::Item, stream: &mut S) -> Vec<S::Item>
where
    S: Stream + Unpin,
{
    let mut events = Vec::with_capacity(MAX_DRAINED_EVENTS_PER_WAKE + 1);
    events.push(first);

    for _ in 0..MAX_DRAINED_EVENTS_PER_WAKE {
        // Poll once with the current Tokio task's real waker. `now_or_never`
        // uses a no-op waker, which is unsafe for Crossterm's EventStream: it
        // keeps only the first registered waker until its blocking reader fires.
        // Leaving a no-op registered would make the next key wait for an
        // unrelated timer tick before the run loop notices it.
        let next = poll_fn(|cx| match Pin::new(&mut *stream).poll_next(cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(event)),
            Poll::Ready(None) | Poll::Pending => Poll::Ready(None),
        })
        .await;
        let Some(event) = next else {
            break;
        };
        events.push(event);
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    #[derive(Default)]
    struct SingleRegistrationState {
        item: Option<usize>,
        registered_waker: Option<Waker>,
        wake_task_executed: bool,
    }

    struct SingleRegistrationStream {
        state: Arc<Mutex<SingleRegistrationState>>,
    }

    struct SingleRegistrationSender {
        state: Arc<Mutex<SingleRegistrationState>>,
    }

    fn single_registration_stream() -> (SingleRegistrationSender, SingleRegistrationStream) {
        let state = Arc::new(Mutex::new(SingleRegistrationState::default()));
        (
            SingleRegistrationSender {
                state: Arc::clone(&state),
            },
            SingleRegistrationStream { state },
        )
    }

    impl SingleRegistrationSender {
        fn send(&self, item: usize) {
            let waker = {
                let mut state = self
                    .state
                    .lock()
                    .expect("single-registration state poisoned");
                state.item = Some(item);
                state.wake_task_executed = false;
                state.registered_waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    impl Stream for SingleRegistrationStream {
        type Item = usize;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let mut state = self
                .state
                .lock()
                .expect("single-registration state poisoned");
            if let Some(item) = state.item.take() {
                return Poll::Ready(Some(item));
            }
            if !state.wake_task_executed {
                state.registered_waker = Some(cx.waker().clone());
                state.wake_task_executed = true;
            }
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn ready_event_batch_preserves_order_and_caps_the_drain() {
        let mut stream = futures::stream::iter(1usize..=40);

        let batch = ready_event_batch(0, &mut stream).await;

        assert_eq!(batch, (0usize..=32).collect::<Vec<_>>());
        assert_eq!(stream.next().await, Some(33));
    }

    #[tokio::test]
    async fn ready_event_batch_never_waits_for_an_unready_event() {
        let mut stream = futures::stream::pending::<usize>();

        assert_eq!(ready_event_batch(7, &mut stream).await, vec![7]);
    }

    #[tokio::test]
    async fn pending_batch_probe_keeps_a_live_waker_for_the_next_event() {
        let (sender, mut stream) = single_registration_stream();
        let task_waker = poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        assert_eq!(ready_event_batch(7, &mut stream).await, vec![7]);
        let registered_waker = stream
            .state
            .lock()
            .expect("single-registration state poisoned")
            .registered_waker
            .clone()
            .expect("pending poll must register a waker");
        assert!(
            registered_waker.will_wake(&task_waker),
            "pending batch probe registered a different or no-op waker"
        );

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            sender.send(8);
        });

        let next = tokio::time::timeout(Duration::from_millis(250), stream.next())
            .await
            .expect("the event should wake the waiting task without a timer tick");
        assert_eq!(next, Some(8));
    }
}
