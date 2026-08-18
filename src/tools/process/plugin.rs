use std::{
    ffi::OsString,
    io::{self, Read, Write as _},
    os::{fd::OwnedFd, unix::process::ExitStatusExt as _},
    path::Path,
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{
    AnchorState, GroupGuard, ProcessRunner, ProcessTermination, capture, host, observe_leader,
    spawn,
};

const CLEAN_EXIT_GRACE: Duration = Duration::from_millis(500);
const TERM_GRACE: Duration = Duration::from_secs(3);
const POST_KILL_DEADLINE: Duration = Duration::from_secs(1);
const OBSERVER_INTERVAL: Duration = Duration::from_millis(10);
const READ_CHUNK_BYTES: usize = 8 * 1024;
const MAX_STDOUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 256 * 1024;
const RETAINED_STDERR_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PluginProcessError {
    #[error("plugin process observer is unavailable")]
    ObserverUnavailable,
    #[error("plugin process startup was cancelled")]
    Cancelled,
    #[error("plugin process could not be started")]
    Spawn,
    #[error("plugin process pipes could not be configured")]
    Pipes,
    #[error("plugin process ownership could not be proven during startup cleanup")]
    OwnershipLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginLeaderState {
    Running,
    Exited(ProcessTermination),
    OwnershipLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginCleanup {
    Quiescent(ProcessTermination),
    OwnershipLost,
}

pub(crate) struct PluginCleanupReport {
    state: PluginCleanup,
    stdout_limit_exceeded: bool,
    stderr_limit_exceeded: bool,
    pipe_failed: bool,
    stderr_tail: Vec<u8>,
}

impl std::fmt::Debug for PluginCleanupReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginCleanupReport")
            .field("state", &self.state)
            .field("stdout_limit_exceeded", &self.stdout_limit_exceeded)
            .field("stderr_limit_exceeded", &self.stderr_limit_exceeded)
            .field("pipe_failed", &self.pipe_failed)
            .field("stderr_tail_bytes", &self.stderr_tail.len())
            .finish()
    }
}

impl PluginCleanupReport {
    pub(crate) fn state(&self) -> PluginCleanup {
        self.state
    }

    pub(crate) fn stdout_limit_exceeded(&self) -> bool {
        self.stdout_limit_exceeded
    }

    pub(crate) fn stderr_limit_exceeded(&self) -> bool {
        self.stderr_limit_exceeded
    }

    pub(crate) fn pipe_failed(&self) -> bool {
        self.pipe_failed
    }
}

#[derive(Debug)]
pub(crate) enum PluginIo {
    Bytes(usize),
    WouldBlock,
    Eof,
    LimitExceeded,
}

#[derive(Clone)]
pub(crate) struct PluginEmergencyHandle {
    leader: Pid,
    armed: Arc<AtomicBool>,
}

impl std::fmt::Debug for PluginEmergencyHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginEmergencyHandle")
            .field("armed", &self.armed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl PluginEmergencyHandle {
    pub(crate) fn kill_group(&self) {
        if self.armed.load(Ordering::Acquire)
            && emergency_signal_is_safe(observe_leader(self.leader))
        {
            let _ = rustix::process::kill_process_group(self.leader, Signal::KILL);
        }
    }
}

fn emergency_signal_is_safe(state: AnchorState) -> bool {
    matches!(state, AnchorState::Running | AnchorState::Exited(_))
}

pub(crate) struct PluginProcess {
    host: Arc<host::Host>,
    child: Option<Child>,
    leader: Pid,
    session: Pid,
    harness: Pid,
    armed: Arc<AtomicBool>,
    _guard: GroupGuard,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdout_budget: capture::ObservedBudget,
    stderr_budget: capture::ObservedBudget,
    stderr_tail: capture::TailCapture,
    stdout_limit_exceeded: bool,
    stderr_limit_exceeded: bool,
    pipe_failed: bool,
}

impl std::fmt::Debug for PluginProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginProcess")
            .field("leader", &self.leader)
            .field("owned", &self.armed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl PluginProcess {
    pub(crate) fn spawn(
        runner: &ProcessRunner,
        program: &Path,
        arguments: &[String],
        workdir: OwnedFd,
        environment: &[(OsString, OsString)],
        cancellation: &CancellationToken,
    ) -> Result<Self, PluginProcessError> {
        runner
            .host
            .recheck(cancellation)
            .map_err(|error| match error {
                host::HostError::Cancelled => PluginProcessError::Cancelled,
                host::HostError::Unsupported => PluginProcessError::ObserverUnavailable,
            })?;
        let armed = Arc::new(AtomicBool::new(false));
        let guard_armed = Arc::clone(&armed);
        let mut child = spawn::plugin(program, arguments, workdir, environment)
            .map_err(|_| PluginProcessError::Spawn)?;
        let leader = Pid::from_child(&child);
        armed.store(true, Ordering::Release);
        let guard = GroupGuard {
            leader,
            armed: guard_armed,
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pipes_ready = stdin
            .as_ref()
            .is_some_and(|pipe| set_nonblocking(pipe).is_ok())
            && stdout
                .as_ref()
                .is_some_and(|pipe| set_nonblocking(pipe).is_ok())
            && stderr
                .as_ref()
                .is_some_and(|pipe| set_nonblocking(pipe).is_ok());
        let mut process = Self {
            host: Arc::clone(&runner.host),
            child: Some(child),
            leader,
            session: leader,
            harness: rustix::process::getpid(),
            armed,
            _guard: guard,
            stdin,
            stdout,
            stderr,
            stdout_budget: capture::ObservedBudget::new(MAX_STDOUT_BYTES),
            stderr_budget: capture::ObservedBudget::new(MAX_STDERR_BYTES),
            stderr_tail: capture::TailCapture::new(RETAINED_STDERR_BYTES),
            stdout_limit_exceeded: false,
            stderr_limit_exceeded: false,
            pipe_failed: false,
        };
        if !pipes_ready {
            process.stdin = None;
            process.stdout = None;
            process.stderr = None;
            return match process.kill().state() {
                PluginCleanup::Quiescent(_) => Err(PluginProcessError::Pipes),
                PluginCleanup::OwnershipLost => Err(PluginProcessError::OwnershipLost),
            };
        }
        Ok(process)
    }

    pub(crate) fn leader_state(&self) -> PluginLeaderState {
        match observe_leader(self.leader) {
            AnchorState::Running => PluginLeaderState::Running,
            AnchorState::Exited(termination) => PluginLeaderState::Exited(termination),
            AnchorState::Unowned | AnchorState::Indeterminate => PluginLeaderState::OwnershipLost,
        }
    }

    pub(crate) fn emergency_handle(&self) -> PluginEmergencyHandle {
        PluginEmergencyHandle {
            leader: self.leader,
            armed: Arc::clone(&self.armed),
        }
    }

    pub(crate) fn try_write(&mut self, bytes: &[u8]) -> io::Result<PluginIo> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(PluginIo::Eof);
        };
        match stdin.write(bytes) {
            Ok(0) => Ok(PluginIo::Eof),
            Ok(count) => Ok(PluginIo::Bytes(count)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(PluginIo::WouldBlock),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn try_read_stdout(&mut self, buffer: &mut [u8]) -> io::Result<PluginIo> {
        let limit = self.stdout_budget.next_read_len(buffer.len());
        if limit == 0 {
            self.stdout_limit_exceeded = true;
            return Ok(PluginIo::LimitExceeded);
        }
        match read_pipe(&mut self.stdout, &mut buffer[..limit]) {
            Ok(PluginIo::Bytes(count)) => {
                if self.stdout_budget.record(count) {
                    self.stdout_limit_exceeded = true;
                    Ok(PluginIo::LimitExceeded)
                } else {
                    Ok(PluginIo::Bytes(count))
                }
            }
            Ok(other) => Ok(other),
            Err(error) => {
                self.pipe_failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn try_read_stderr(&mut self, buffer: &mut [u8]) -> io::Result<PluginIo> {
        let limit = self.stderr_budget.next_read_len(buffer.len());
        if limit == 0 {
            self.stderr_limit_exceeded = true;
            return Ok(PluginIo::LimitExceeded);
        }
        match read_pipe(&mut self.stderr, &mut buffer[..limit]) {
            Ok(PluginIo::Bytes(count)) => {
                self.stderr_tail.push(&buffer[..count]);
                if self.stderr_budget.record(count) {
                    self.stderr_limit_exceeded = true;
                    Ok(PluginIo::LimitExceeded)
                } else {
                    Ok(PluginIo::Bytes(count))
                }
            }
            Ok(other) => Ok(other),
            Err(error) => {
                self.pipe_failed = true;
                Err(error)
            }
        }
    }

    pub(crate) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub(crate) fn send_signal(&self, signal: Signal) -> io::Result<()> {
        if !emergency_signal_is_safe(observe_leader(self.leader)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "plugin process ownership is unavailable",
            ));
        }
        rustix::process::kill_process_group(self.leader, signal)
            .or_else(|error| {
                if error == rustix::io::Errno::SRCH {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(io::Error::from)
    }

    pub(crate) fn cleanup(mut self) -> PluginCleanupReport {
        self.close_stdin();
        let mut expected_exit = None;
        if let Some(done) = self.wait_until(CLEAN_EXIT_GRACE, &mut expected_exit) {
            return self.finish_cleanup(done);
        }
        let _ = self.send_signal(Signal::TERM);
        if let Some(done) = self.wait_until(TERM_GRACE, &mut expected_exit) {
            return self.finish_cleanup(done);
        }
        let _ = self.send_signal(Signal::KILL);
        let state = self
            .wait_until(POST_KILL_DEADLINE, &mut expected_exit)
            .unwrap_or(PluginCleanup::OwnershipLost);
        self.finish_cleanup(state)
    }

    pub(crate) fn terminate(mut self) -> PluginCleanupReport {
        self.close_stdin();
        let mut expected_exit = None;
        let _ = self.send_signal(Signal::TERM);
        if let Some(done) = self.wait_until(TERM_GRACE, &mut expected_exit) {
            return self.finish_cleanup(done);
        }
        let _ = self.send_signal(Signal::KILL);
        let state = self
            .wait_until(POST_KILL_DEADLINE, &mut expected_exit)
            .unwrap_or(PluginCleanup::OwnershipLost);
        self.finish_cleanup(state)
    }

    pub(crate) fn kill(mut self) -> PluginCleanupReport {
        self.close_stdin();
        let mut expected_exit = None;
        let _ = self.send_signal(Signal::KILL);
        let state = self
            .wait_until(POST_KILL_DEADLINE, &mut expected_exit)
            .unwrap_or(PluginCleanup::OwnershipLost);
        self.finish_cleanup(state)
    }

    fn wait_until(
        &mut self,
        duration: Duration,
        expected_exit: &mut Option<ProcessTermination>,
    ) -> Option<PluginCleanup> {
        let mut deadline = Instant::now() + duration;
        let mut complete_passes = 0_u8;
        let mut scratch = [0_u8; READ_CHUNK_BYTES];
        loop {
            let stdout = self.try_read_stdout(&mut scratch);
            let stderr = self.try_read_stderr(&mut scratch);
            if matches!(stdout, Ok(PluginIo::LimitExceeded))
                || matches!(stderr, Ok(PluginIo::LimitExceeded))
            {
                let _ = self.send_signal(Signal::KILL);
                deadline = deadline.min(Instant::now() + POST_KILL_DEADLINE);
            }
            match observe_leader(self.leader) {
                AnchorState::Running => {
                    complete_passes = 0;
                }
                AnchorState::Exited(termination) => {
                    if expected_exit.is_some_and(|expected| expected != termination) {
                        return Some(PluginCleanup::OwnershipLost);
                    }
                    *expected_exit = Some(termination);
                    match self
                        .host
                        .scan_group(self.leader, self.session, self.harness)
                    {
                        host::GroupScan::Complete => {
                            complete_passes = complete_passes.saturating_add(1);
                            if complete_passes >= 2 {
                                return Some(self.reap(termination));
                            }
                        }
                        host::GroupScan::Live | host::GroupScan::Unknown => {
                            complete_passes = 0;
                        }
                        #[cfg(target_os = "linux")]
                        host::GroupScan::Mutated => {
                            complete_passes = 0;
                        }
                        host::GroupScan::OwnershipLost => {
                            return Some(PluginCleanup::OwnershipLost);
                        }
                    }
                }
                AnchorState::Unowned | AnchorState::Indeterminate => {
                    return Some(PluginCleanup::OwnershipLost);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(OBSERVER_INTERVAL);
        }
    }

    fn reap(&mut self, expected: ProcessTermination) -> PluginCleanup {
        let Some(child) = self.child.as_mut() else {
            return PluginCleanup::OwnershipLost;
        };
        let actual = child.try_wait().ok().flatten().and_then(|status| {
            status
                .code()
                .filter(|code| *code >= 0)
                .map(ProcessTermination::ExitCode)
                .or_else(|| {
                    status
                        .signal()
                        .filter(|signal| *signal > 0)
                        .map(ProcessTermination::Signal)
                })
        });
        if actual == Some(expected) {
            self.armed.store(false, Ordering::Release);
            self.child = None;
            PluginCleanup::Quiescent(expected)
        } else {
            PluginCleanup::OwnershipLost
        }
    }

    fn finish_cleanup(self, state: PluginCleanup) -> PluginCleanupReport {
        let (stderr_tail, _) = self.stderr_tail.finish();
        PluginCleanupReport {
            state,
            stdout_limit_exceeded: self.stdout_limit_exceeded,
            stderr_limit_exceeded: self.stderr_limit_exceeded,
            pipe_failed: self.pipe_failed,
            stderr_tail,
        }
    }
}

fn set_nonblocking(file: &impl std::os::fd::AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(file).map_err(io::Error::from)?;
    rustix::fs::fcntl_setfl(file, flags | rustix::fs::OFlags::NONBLOCK).map_err(io::Error::from)
}

fn read_pipe<T: Read>(pipe: &mut Option<T>, buffer: &mut [u8]) -> io::Result<PluginIo> {
    let Some(inner) = pipe.as_mut() else {
        return Ok(PluginIo::Eof);
    };
    match inner.read(buffer) {
        Ok(0) => {
            *pipe = None;
            Ok(PluginIo::Eof)
        }
        Ok(count) => Ok(PluginIo::Bytes(count)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(PluginIo::WouldBlock),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::emergency_signal_is_safe;
    use crate::tools::process::{AnchorState, ProcessTermination};

    #[test]
    fn emergency_cleanup_signals_only_a_still_waitable_owned_leader() {
        assert!(emergency_signal_is_safe(AnchorState::Running));
        assert!(emergency_signal_is_safe(AnchorState::Exited(
            ProcessTermination::ExitCode(0)
        )));
        assert!(!emergency_signal_is_safe(AnchorState::Unowned));
        assert!(!emergency_signal_is_safe(AnchorState::Indeterminate));
    }
}
