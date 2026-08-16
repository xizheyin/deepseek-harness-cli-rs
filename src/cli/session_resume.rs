//! CLI ownership for prepare -> warn -> commit -> active Session resume.

use std::{path::PathBuf, time::Duration};

use crate::session::{Session, SessionId, SessionStore, StoreError, SystemClock};

use super::{
    assembly::AssemblySession,
    recovery_warning,
    script_io::{OwnedOutputEvent, OwnedScriptOutput},
    shutdown,
    signal::{DriverMode, SignalLatch, SignalStreams, UiSignal, self_suspend},
    terminal::{AsyncTerminal, TerminalError},
};

const WARNING_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) enum WarningTarget<'a> {
    Script,
    Interactive(&'a AsyncTerminal),
    #[cfg(test)]
    TestDeadline {
        cleanup_signal: Option<UiSignal>,
    },
}

impl WarningTarget<'_> {
    const fn mode(&self) -> DriverMode {
        match self {
            Self::Script => DriverMode::Script,
            Self::Interactive(_) => DriverMode::Interactive,
            #[cfg(test)]
            Self::TestDeadline { .. } => DriverMode::Script,
        }
    }

    const fn deadline_cleanup_signal(self) -> Option<UiSignal> {
        match self {
            #[cfg(test)]
            Self::TestDeadline { cleanup_signal } => cleanup_signal,
            _ => None,
        }
    }
}

pub(super) struct ResumeReady {
    pub(super) assembly: AssemblySession,
}

pub(super) enum ResumeError {
    Storage(StoreError),
    Terminal(TerminalError),
    Output,
    Signal(UiSignal),
}

pub(super) async fn resume(
    store: &SessionStore,
    id: SessionId,
    asserted_workspace: Option<PathBuf>,
    target: WarningTarget<'_>,
    signals: &mut SignalStreams,
) -> Result<ResumeReady, ResumeError> {
    let mode = target.mode();
    let mut preparing = store
        .begin_resume(id, asserted_workspace, SystemClock)
        .map_err(ResumeError::Storage)?;

    enum ReadyWait {
        Ready(Result<(), StoreError>),
        Signal(UiSignal),
    }
    let ready = {
        let wait = preparing.wait_ready();
        tokio::pin!(wait);
        tokio::select! {
            biased;
            signal = signals.next() => ReadyWait::Signal(signal),
            result = &mut wait => ReadyWait::Ready(result),
        }
    };
    let ready = match ready {
        ReadyWait::Signal(signal) => {
            let (cleanup, observed) = shutdown::future_with_signal_streams(
                preparing.cancel_and_shutdown(),
                mode,
                signals,
                Some(signal),
            )
            .await;
            drop(preparing);
            return Err(match observed {
                Some(signal) => ResumeError::Signal(signal),
                None => cleanup
                    .err()
                    .map_or(ResumeError::Signal(signal), ResumeError::Storage),
            });
        }
        ReadyWait::Ready(result) => result,
    };
    if let Err(error) = ready {
        let (cleanup, observed) = shutdown::future_with_signal_streams(
            preparing.cancel_and_shutdown(),
            mode,
            signals,
            None,
        )
        .await;
        drop(preparing);
        if let Some(signal) = observed {
            return Err(ResumeError::Signal(signal));
        }
        let _ = cleanup;
        return Err(ResumeError::Storage(error));
    }
    if let Some(signal) = drain_signals(mode, signals).await {
        let (_cleanup, observed) = shutdown::future_with_signal_streams(
            preparing.cancel_and_shutdown(),
            mode,
            signals,
            Some(signal),
        )
        .await;
        drop(preparing);
        return Err(ResumeError::Signal(observed.unwrap_or(signal)));
    }

    // `wait_ready` acknowledged success, so this synchronous extraction has
    // no remaining fallible work in the worker. Keep it adjacent to the wait.
    let mut prepared = preparing.finish().map_err(ResumeError::Storage)?;
    let warning = match recovery_warning::render(prepared.recovery_report()) {
        Ok(warning) => warning,
        Err(_) => {
            let (cleanup, observed) = shutdown::future_with_signal_streams(
                prepared.cancel_and_shutdown(),
                mode,
                signals,
                None,
            )
            .await;
            drop(prepared);
            return Err(warning_failure_after_cleanup(cleanup, observed));
        }
    };

    if let Some(bytes) = warning {
        let event = match target {
            WarningTarget::Script => match OwnedScriptOutput::start_stderr(bytes) {
                Ok(mut output) => match output.wait_event(mode, signals).await {
                    OwnedOutputEvent::Complete(result) => WarningEvent::Complete(result),
                    OwnedOutputEvent::Signal(signal) => WarningEvent::Signal(signal, Some(output)),
                    OwnedOutputEvent::Deadline => WarningEvent::Deadline(Some(output)),
                },
                Err(error) => WarningEvent::Complete(Err(error)),
            },
            WarningTarget::Interactive(terminal) => {
                write_interactive_warning(terminal, &bytes, signals).await
            }
            #[cfg(test)]
            WarningTarget::TestDeadline { .. } => WarningEvent::Deadline(None),
        };
        match event {
            WarningEvent::Complete(Ok(())) => {}
            WarningEvent::Complete(Err(_)) => {
                let (cleanup, observed) = shutdown::future_with_signal_streams(
                    prepared.cancel_and_shutdown(),
                    mode,
                    signals,
                    None,
                )
                .await;
                drop(prepared);
                return Err(warning_failure_after_cleanup(cleanup, observed));
            }
            WarningEvent::Signal(signal, output) => {
                let (_cleanup, observed) = shutdown::future_with_signal_streams(
                    prepared.cancel_and_shutdown(),
                    mode,
                    signals,
                    Some(signal),
                )
                .await;
                drop(prepared);
                let signal = observed.unwrap_or(signal);
                if let Some(output) = output {
                    exit_script_warning_after_cleanup(output, signal, signals);
                }
                return Err(ResumeError::Signal(signal));
            }
            WarningEvent::Deadline(output) => {
                let (cleanup, observed) = shutdown::future_with_signal_streams(
                    prepared.cancel_and_shutdown(),
                    mode,
                    signals,
                    target.deadline_cleanup_signal(),
                )
                .await;
                drop(prepared);
                if let Some(signal) = observed {
                    if let Some(output) = output {
                        exit_script_warning_after_cleanup(output, signal, signals);
                    }
                    return Err(ResumeError::Signal(signal));
                }
                if let Some(output) = output {
                    output.exit_after_cleanup(1);
                }
                return Err(warning_failure_after_cleanup(cleanup, None));
            }
        }
    }

    if let Some(signal) = drain_signals(mode, signals).await {
        let (_cleanup, observed) = shutdown::future_with_signal_streams(
            prepared.cancel_and_shutdown(),
            mode,
            signals,
            Some(signal),
        )
        .await;
        drop(prepared);
        return Err(ResumeError::Signal(observed.unwrap_or(signal)));
    }
    if let Err(error) = prepared.begin_commit() {
        let (cleanup, observed) = shutdown::future_with_signal_streams(
            prepared.cancel_and_shutdown(),
            mode,
            signals,
            None,
        )
        .await;
        drop(prepared);
        if let Some(signal) = observed {
            return Err(ResumeError::Signal(signal));
        }
        let _ = cleanup;
        return Err(ResumeError::Storage(error));
    }

    // A commit command is deliberately non-cancelable. Signals are only
    // latched until the same flight reaches a durable success or failure.
    let (commit, observed) =
        shutdown::future_with_signal_streams(prepared.wait_commit(), mode, signals, None).await;
    if let Err(error) = commit {
        let (cleanup, later) = shutdown::future_with_signal_streams(
            prepared.cancel_and_shutdown(),
            mode,
            signals,
            observed,
        )
        .await;
        drop(prepared);
        if let Some(signal) = later.or(observed) {
            return Err(ResumeError::Signal(signal));
        }
        let _ = cleanup;
        return Err(ResumeError::Storage(error));
    }
    let recovered = prepared.finish_commit().map_err(ResumeError::Storage)?;
    finish_committed_resume(recovered, target, mode, signals, observed).await
}

async fn finish_committed_resume(
    recovered: crate::session::RecoveredSession,
    target: WarningTarget<'_>,
    mode: DriverMode,
    signals: &mut SignalStreams,
    observed: Option<UiSignal>,
) -> Result<ResumeReady, ResumeError> {
    let (mut session, authority) = recovered.into_parts();
    let observed = if observed == Some(UiSignal::Suspend) {
        match target {
            WarningTarget::Script => observed,
            WarningTarget::Interactive(terminal) => {
                let suspend = super::interactive::suspend_and_resume(terminal, signals);
                match hold_session_during_suspend(&mut session, suspend).await {
                    Ok(signal) => signal,
                    Err(error) => {
                        let (cleanup, signal) =
                            shutdown::session_with_signals(&mut session, mode, signals, None).await;
                        drop(authority);
                        drop(session);
                        if let Some(signal) = signal {
                            return Err(ResumeError::Signal(signal));
                        }
                        if let Err(error) = cleanup {
                            return Err(ResumeError::Storage(
                                super::storage_failure::from_shutdown(&error),
                            ));
                        }
                        return Err(ResumeError::Terminal(error));
                    }
                }
            }
            #[cfg(test)]
            WarningTarget::TestDeadline { .. } => observed,
        }
    } else {
        observed
    };
    if let Some(signal) = observed {
        let error = close_recovered_after_signal(&mut session, mode, signals, signal).await;
        drop(authority);
        drop(session);
        return Err(error);
    }
    Ok(ResumeReady {
        assembly: AssemblySession::resumed(session, authority),
    })
}

async fn hold_session_during_suspend<F, T>(session: &mut Session, suspend: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let result = suspend.await;
    // The post-await read deliberately keeps the Active Session borrowed for
    // the whole stop/continue window, so its writer and flock cannot be
    // released while the process is suspended.
    let _ = session.next_seq();
    result
}

fn warning_failure_after_cleanup(
    cleanup: Result<(), StoreError>,
    observed: Option<UiSignal>,
) -> ResumeError {
    if let Some(signal) = observed {
        ResumeError::Signal(signal)
    } else if let Err(error) = cleanup {
        ResumeError::Storage(error)
    } else {
        ResumeError::Output
    }
}

async fn close_recovered_after_signal(
    session: &mut Session,
    mode: DriverMode,
    signals: &mut SignalStreams,
    signal: UiSignal,
) -> ResumeError {
    let (_cleanup, later) =
        shutdown::session_with_signals(session, mode, signals, Some(signal)).await;
    ResumeError::Signal(later.unwrap_or(signal))
}

enum WarningEvent {
    Complete(Result<(), super::script_io::ScriptOutputError>),
    Signal(UiSignal, Option<OwnedScriptOutput>),
    Deadline(Option<OwnedScriptOutput>),
}

async fn write_interactive_warning(
    terminal: &AsyncTerminal,
    bytes: &[u8],
    signals: &mut SignalStreams,
) -> WarningEvent {
    if terminal.revalidate().is_err() {
        return WarningEvent::Complete(Err(super::script_io::ScriptOutputError));
    }
    let deadline = tokio::time::Instant::now() + WARNING_DEADLINE;
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let event = tokio::select! {
            biased;
            signal = signals.next() => return WarningEvent::Signal(signal, None),
            () = tokio::time::sleep_until(deadline) => return WarningEvent::Deadline(None),
            result = terminal.write_once(&bytes[offset..]) => result,
        };
        match event {
            Ok(count) if count != 0 && count <= bytes.len() - offset => offset += count,
            Ok(_) => {
                return WarningEvent::Complete(Err(super::script_io::ScriptOutputError));
            }
            Err(_) => {
                return WarningEvent::Complete(Err(super::script_io::ScriptOutputError));
            }
        }
    }
    if let Some(signal) = drain_signals(DriverMode::Interactive, signals).await {
        return WarningEvent::Signal(signal, None);
    }
    WarningEvent::Complete(Ok(()))
}

async fn drain_signals(mode: DriverMode, signals: &mut SignalStreams) -> Option<UiSignal> {
    tokio::task::yield_now().await;
    let mut latch = SignalLatch::default();
    signals.drain_ready(mode, &mut latch);
    latch.observed()
}

fn exit_script_warning_after_cleanup(
    output: OwnedScriptOutput,
    signal: UiSignal,
    signals: &mut SignalStreams,
) -> ! {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            output.exit_after_cleanup(1);
        }
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Script, signal);
        signals.drain_ready(DriverMode::Script, &mut latch);
        output.exit_after_cleanup(
            latch
                .observed()
                .and_then(UiSignal::exit_code)
                .unwrap_or(148),
        );
    }
    output.exit_after_cleanup(signal.exit_code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::PathBuf};
    use std::{
        future::Future as _,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::Poll,
    };

    use futures_util::{StreamExt as _, stream, task::noop_waker_ref};
    use tokio::sync::oneshot;

    use crate::{
        session::{
            Clock, ClockError, EventKind, NewEvent, SessionId, SessionStore, StoreError, TurnId,
            UnixMillis,
        },
        workspace_authority::WorkspaceAuthority,
    };

    use super::{
        DriverMode, ResumeError, SignalStreams, UiSignal, WarningTarget, finish_committed_resume,
        hold_session_during_suspend, resume, warning_failure_after_cleanup,
    };

    #[derive(Clone, Copy)]
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            UnixMillis::new(self.0).map_err(|error| ClockError::new(error.to_string()))
        }
    }

    #[test]
    fn terminating_signal_and_cleanup_failure_both_outrank_warning_output_failure() {
        assert!(matches!(
            warning_failure_after_cleanup(
                Err(StoreError::WriterStopped),
                Some(UiSignal::Terminate)
            ),
            ResumeError::Signal(UiSignal::Terminate)
        ));
        assert!(matches!(
            warning_failure_after_cleanup(Err(StoreError::WriterStopped), None),
            ResumeError::Storage(StoreError::WriterStopped)
        ));
        assert!(matches!(
            warning_failure_after_cleanup(Ok(()), None),
            ResumeError::Output
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn warning_deadline_cleanup_is_not_cancelled_when_a_signal_arrives() {
        let released = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let release_on_signal = Arc::clone(&released);
        let signal_stream = stream::once(async move {
            release_on_signal.store(true, Ordering::SeqCst);
            UiSignal::Terminate
        })
        .chain(stream::pending());
        let cleanup = GatedCleanup {
            released,
            completed: Arc::clone(&completed),
        };

        let (cleanup, observed) = crate::cli::shutdown::future_with_signals(
            cleanup,
            DriverMode::Script,
            None,
            signal_stream,
        )
        .await;

        assert!(completed.load(Ordering::SeqCst));
        assert!(matches!(
            warning_failure_after_cleanup(cleanup, observed),
            ResumeError::Signal(UiSignal::Terminate)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_warning_deadline_aborts_without_mutating_and_releases_the_lock() {
        let fixture = ResumeFixture::new("resume-warning-deadline").await;
        let mut signals = SignalStreams::install().unwrap();
        let result = resume(
            &fixture.store,
            fixture.id.clone(),
            Some(fixture.workspace.clone()),
            WarningTarget::TestDeadline {
                cleanup_signal: None,
            },
            &mut signals,
        )
        .await;
        assert!(matches!(result, Err(ResumeError::Output)));
        assert_eq!(fs::read(&fixture.path).unwrap(), fixture.original);
        fixture.assert_reopenable(2_100).await;
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_warning_deadline_preserves_a_signal_observed_during_real_owner_cleanup() {
        let fixture = ResumeFixture::new("resume-warning-deadline-signal").await;
        let mut signals = SignalStreams::install().unwrap();
        let result = resume(
            &fixture.store,
            fixture.id.clone(),
            Some(fixture.workspace.clone()),
            WarningTarget::TestDeadline {
                cleanup_signal: Some(UiSignal::Terminate),
            },
            &mut signals,
        )
        .await;
        assert!(matches!(
            result,
            Err(ResumeError::Signal(UiSignal::Terminate))
        ));
        assert_eq!(fs::read(&fixture.path).unwrap(), fixture.original);
        fixture.assert_reopenable(2_100).await;
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_commit_time_signal_closes_the_handed_off_session_before_assembly() {
        let fixture = ResumeFixture::new("resume-commit-signal").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();
        prepared.begin_commit().unwrap();
        prepared.wait_commit().await.unwrap();
        let recovered = prepared.finish_commit().unwrap();

        let mut signals = SignalStreams::install().unwrap();
        let result = finish_committed_resume(
            recovered,
            WarningTarget::Script,
            DriverMode::Script,
            &mut signals,
            Some(UiSignal::Terminate),
        )
        .await;
        assert!(matches!(
            result,
            Err(ResumeError::Signal(UiSignal::Terminate))
        ));
        fixture.assert_reopenable(2_100).await;
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interactive_suspend_keeps_the_recovered_session_lock_owned() {
        let fixture = ResumeFixture::new("resume-suspend-owner").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();
        prepared.begin_commit().unwrap();
        prepared.wait_commit().await.unwrap();
        let recovered = prepared.finish_commit().unwrap();
        let (mut session, authority) = recovered.into_parts();
        let (release, wait) = oneshot::channel::<()>();
        let mut suspended = Box::pin(hold_session_during_suspend(&mut session, async move {
            let _ = wait.await;
        }));

        let mut context = std::task::Context::from_waker(noop_waker_ref());
        assert!(matches!(
            Pin::as_mut(&mut suspended).poll(&mut context),
            Poll::Pending
        ));
        let mut competing = fixture.begin(2_100);
        assert_eq!(competing.wait_ready().await.unwrap_err(), StoreError::Busy);
        competing.cancel_and_shutdown().await.unwrap();

        release.send(()).unwrap();
        suspended.await;
        session.shutdown().await.unwrap();
        drop(authority);
        fixture.assert_reopenable(2_200).await;
        fixture.cleanup();
    }

    struct ResumeFixture {
        root: PathBuf,
        workspace: PathBuf,
        store: SessionStore,
        id: SessionId,
        path: PathBuf,
        original: Vec<u8>,
    }

    struct GatedCleanup {
        released: Arc<AtomicBool>,
        completed: Arc<AtomicBool>,
    }

    impl std::future::Future for GatedCleanup {
        type Output = Result<(), StoreError>;

        fn poll(self: Pin<&mut Self>, _context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            if self.released.load(Ordering::SeqCst) {
                self.completed.store(true, Ordering::SeqCst);
                Poll::Ready(Err(StoreError::WriterStopped))
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for GatedCleanup {
        fn drop(&mut self) {
            assert!(
                self.completed.load(Ordering::SeqCst),
                "warning cleanup must finish before its owner is dropped"
            );
        }
    }

    impl ResumeFixture {
        async fn new(label: &str) -> Self {
            let root = private_dir(&format!("{label}-root"));
            let workspace = private_dir(&format!("{label}-workspace"));
            let store = SessionStore::open_existing(&root).unwrap();
            let authority = WorkspaceAuthority::open(&workspace).unwrap();
            let id = SessionId::new("session-b50e8400-e29b-41d4-a716-446655440000");
            let mut session = store
                .prepare_new(id.clone(), &authority, FixedClock(1_000))
                .unwrap();
            session.materialize_if_needed().await.unwrap();
            session
                .append_settled(NewEvent::log(EventKind::turn_start(
                    TurnId::new(1).unwrap(),
                )))
                .await
                .unwrap();
            session.flush_barrier().await.unwrap();
            session.shutdown().await.unwrap();
            let path = root.join(format!("{id}.jsonl"));
            let original = fs::read(&path).unwrap();
            Self {
                root,
                workspace,
                store,
                id,
                path,
                original,
            }
        }

        fn begin(&self, time: i64) -> crate::session::PreparingResume {
            self.store
                .begin_resume(
                    self.id.clone(),
                    Some(self.workspace.clone()),
                    FixedClock(time),
                )
                .unwrap()
        }

        async fn assert_reopenable(&self, time: i64) {
            let mut reopened = self.begin(time);
            reopened.wait_ready().await.unwrap();
            reopened.cancel_and_shutdown().await.unwrap();
        }

        fn cleanup(self) {
            drop(self.store);
            fs::remove_file(self.path).unwrap();
            fs::remove_dir(self.root).unwrap();
            fs::remove_dir(self.workspace).unwrap();
        }
    }

    fn private_dir(label: &str) -> PathBuf {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = parent.join(format!("dsh-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
