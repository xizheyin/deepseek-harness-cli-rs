//! Keep signal ownership live while the durable Session finishes and joins.

use std::future::Future;

use futures_util::{Stream, StreamExt as _};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{AgentLoop, AgentShutdownError},
    session::{Session, SessionIoError},
};

use super::signal::{DriverMode, SignalLatch, SignalStreams, UiSignal};

/// Await the same owned shutdown operation to completion while remembering any
/// process signal that arrives. The shutdown branch is polled first when both
/// are ready, then one bounded drain captures signals made ready in that poll.
pub(super) async fn agent_with_signals(
    agent: &mut AgentLoop,
    mode: DriverMode,
    signals: &mut SignalStreams,
    initial_signal: Option<UiSignal>,
) -> (Result<(), AgentShutdownError>, Option<UiSignal>) {
    future_with_signal_streams(agent.shutdown(), mode, signals, initial_signal).await
}

/// The same cleanup discipline for an Active Session that has not yet been
/// consumed by Agent construction (for example, a resumed session whose tool
/// registry or Provider failed to build).
pub(super) async fn session_with_signals(
    session: &mut Session,
    mode: DriverMode,
    signals: &mut SignalStreams,
    initial_signal: Option<UiSignal>,
) -> (Result<(), SessionIoError>, Option<UiSignal>) {
    future_with_signal_streams(session.shutdown(), mode, signals, initial_signal).await
}

pub(super) async fn future_with_signal_streams<F>(
    future: F,
    mode: DriverMode,
    signals: &mut SignalStreams,
    initial_signal: Option<UiSignal>,
) -> (F::Output, Option<UiSignal>)
where
    F: Future,
{
    let signal_stream = futures_util::stream::poll_fn(|context| match signals.poll_next(context) {
        std::task::Poll::Ready(signal) => std::task::Poll::Ready(Some(signal)),
        std::task::Poll::Pending => std::task::Poll::Pending,
    });
    let (result, observed) = future_with_signals(future, mode, initial_signal, signal_stream).await;
    tokio::task::yield_now().await;
    let mut latch = SignalLatch::default();
    if let Some(signal) = observed {
        latch.observe(mode, signal);
    }
    signals.drain_ready(mode, &mut latch);
    (result, latch.observed())
}

/// Poll a startup future to completion while latching signals and asking the
/// startup owner to stop opening new resources. The future itself is never
/// dropped; it must finish joining anything it already started.
pub(super) async fn cancellable_future_with_signal_streams<F>(
    future: F,
    cancellation: &CancellationToken,
    mode: DriverMode,
    signals: &mut SignalStreams,
) -> (F::Output, Option<UiSignal>)
where
    F: Future,
{
    let mut latch = SignalLatch::default();
    tokio::pin!(future);
    let result = loop {
        tokio::select! {
            biased;
            result = &mut future => break result,
            signal = signals.next() => {
                latch.observe(mode, signal);
                cancellation.cancel();
            }
        }
    };
    tokio::task::yield_now().await;
    signals.drain_ready(mode, &mut latch);
    (result, latch.observed())
}

pub(super) async fn future_with_signals<F, S>(
    future: F,
    mode: DriverMode,
    initial_signal: Option<UiSignal>,
    signals: S,
) -> (F::Output, Option<UiSignal>)
where
    F: Future,
    S: Stream<Item = UiSignal>,
{
    let mut latch = SignalLatch::default();
    if let Some(signal) = initial_signal {
        latch.observe(mode, signal);
    }
    tokio::pin!(future);
    tokio::pin!(signals);
    let mut signals_open = true;
    let result = loop {
        tokio::select! {
            biased;
            result = &mut future => break result,
            signal = signals.next(), if signals_open => match signal {
                Some(signal) => latch.observe(mode, signal),
                None => signals_open = false,
            },
        }
    };
    (result, latch.observed())
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };

    use futures_util::{StreamExt as _, stream};

    use super::future_with_signals;
    use crate::cli::signal::{DriverMode, UiSignal};

    struct MustComplete {
        released: Arc<AtomicBool>,
        completed: Arc<AtomicBool>,
    }

    impl Future for MustComplete {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            if self.released.load(Ordering::SeqCst) {
                self.completed.store(true, Ordering::SeqCst);
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for MustComplete {
        fn drop(&mut self) {
            assert!(
                self.completed.load(Ordering::SeqCst),
                "the shutdown owner must not be dropped when a signal arrives"
            );
        }
    }

    #[tokio::test]
    async fn a_signal_is_latched_without_cancelling_the_owned_shutdown() {
        let released = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let release_on_signal = released.clone();
        let signals = stream::once(async move {
            release_on_signal.store(true, Ordering::SeqCst);
            UiSignal::Terminate
        })
        .chain(stream::pending());

        let ((), observed) = future_with_signals(
            MustComplete {
                released,
                completed: completed.clone(),
            },
            DriverMode::Interactive,
            None,
            signals,
        )
        .await;

        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(observed, Some(UiSignal::Terminate));
    }
}
