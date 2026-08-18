//! Owned foreground-process execution for the built-in shell tool.

use std::{
    ffi::OsString,
    fmt,
    future::pending,
    io::{self, Read},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::ExitStatusExt},
    },
    process::{Child, ChildStderr, ChildStdout, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rustix::{
    fs::OFlags,
    process::{Pid, Signal, WaitId, WaitIdOptions},
};
use tokio::{
    io::{Interest, unix::AsyncFd},
    task::{JoinError, JoinHandle},
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

mod capture;
mod host;
#[cfg(any(target_os = "linux", test))]
mod mountinfo;
mod plugin;
#[cfg(any(target_os = "linux", test))]
mod proc_stat;
mod spawn;

pub(crate) use plugin::{
    PluginCleanup, PluginCleanupReport, PluginEmergencyHandle, PluginIo, PluginLeaderState,
    PluginProcess, PluginProcessError,
};

const MAX_COMMAND_BYTES: usize = 32 * 1_024;
const MAX_ENVIRONMENT_BYTES: usize = 32 * 1_024;
const MAX_ENVIRONMENT_ENTRIES: usize = 24;
const MAX_COMMAND_TIMEOUT: Duration = Duration::from_millis(295_000);
const TERM_GRACE: Duration = Duration::from_secs(3);
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(1);
const OBSERVER_INTERVAL: Duration = Duration::from_millis(10);
const READ_CHUNK_BYTES: usize = 8 * 1_024;
const MAX_OBSERVED_BYTES: usize = 8 * 1_024 * 1_024;
const RETAINED_TAIL_BYTES: usize = 64_000;

#[derive(Clone)]
pub(crate) struct ProcessRunner {
    host: Arc<host::Host>,
}

impl fmt::Debug for ProcessRunner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessRunner")
            .field("observer_ready", &true)
            .finish()
    }
}

impl ProcessRunner {
    pub(crate) fn open() -> Result<Self, ProcessBuildError> {
        let host = host::Host::open().map_err(|_| ProcessBuildError)?;
        Ok(Self {
            host: Arc::new(host),
        })
    }

    /// Synchronous final host check. The sealed shell action owns the blocking
    /// job that calls this alongside its final work-directory identity check.
    pub(crate) fn pre_spawn_check(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<ProcessLaunchPermit, ProcessPrecheckError> {
        self.host
            .recheck(cancellation)
            .map_err(|error| match error {
                host::HostError::Cancelled => ProcessPrecheckError::Cancelled,
                host::HostError::Unsupported => ProcessPrecheckError::ObserverUnavailable,
            })?;
        Ok(ProcessLaunchPermit {
            owner: Arc::clone(&self.host),
        })
    }

    pub(crate) async fn run(
        &self,
        request: ProcessRequest,
        control: ProcessControl,
    ) -> ProcessOutcome {
        run_process(self, request, control).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessBuildError;

impl fmt::Display for ProcessBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the platform process observer is unavailable")
    }
}

impl std::error::Error for ProcessBuildError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessPrecheckError {
    Cancelled,
    ObserverUnavailable,
}

pub(crate) struct ProcessLaunchPermit {
    owner: Arc<host::Host>,
}

impl fmt::Debug for ProcessLaunchPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessLaunchPermit")
            .field("host_rechecked", &true)
            .finish()
    }
}

pub(crate) struct ProcessRequest {
    command: String,
    workdir: OwnedFd,
    environment: Arc<[(OsString, OsString)]>,
    timeout: Duration,
    permit: ProcessLaunchPermit,
    #[cfg(test)]
    test_hooks: ProcessTestHooks,
}

impl ProcessRequest {
    pub(crate) fn new(
        command: String,
        workdir: OwnedFd,
        environment: Arc<[(OsString, OsString)]>,
        timeout: Duration,
        permit: ProcessLaunchPermit,
    ) -> Result<Self, ProcessRequestError> {
        if command.is_empty()
            || command.len() > MAX_COMMAND_BYTES
            || command.as_bytes().contains(&0)
        {
            return Err(ProcessRequestError::Command);
        }
        if timeout.is_zero() || timeout > MAX_COMMAND_TIMEOUT {
            return Err(ProcessRequestError::Timeout);
        }
        if environment.len() > MAX_ENVIRONMENT_ENTRIES {
            return Err(ProcessRequestError::Environment);
        }
        let mut environment_bytes = 0_usize;
        for (name, value) in environment.iter() {
            if name.to_str().is_none()
                || value.to_str().is_none()
                || name.as_bytes().is_empty()
                || name.as_bytes().contains(&b'=')
                || name.as_bytes().contains(&0)
                || value.as_bytes().contains(&0)
            {
                return Err(ProcessRequestError::Environment);
            }
            environment_bytes = environment_bytes
                .checked_add(name.as_bytes().len())
                .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
                .ok_or(ProcessRequestError::Environment)?;
        }
        if environment_bytes > MAX_ENVIRONMENT_BYTES {
            return Err(ProcessRequestError::Environment);
        }
        Ok(Self {
            command,
            workdir,
            environment,
            timeout,
            permit,
            #[cfg(test)]
            test_hooks: ProcessTestHooks::default(),
        })
    }

    #[cfg(test)]
    fn with_test_hooks(mut self, hooks: ProcessTestHooks) -> Self {
        self.test_hooks = hooks;
        self
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct ProcessTestHooks {
    pipe_setup_failure: Option<StreamKind>,
    pipe_read_failure: Option<StreamKind>,
    injected_signal_error: Option<InjectedSignalError>,
    scan_results: Option<Arc<std::sync::Mutex<std::collections::VecDeque<host::GroupScan>>>>,
    scan_delay: Duration,
    scan_finished: Option<Arc<AtomicBool>>,
    scan_panics: bool,
    lose_anchor_while_scan_runs: bool,
    lose_ownership_while_running: bool,
    reap_delay: Duration,
    reap_finished: Option<Arc<AtomicBool>>,
    reap_panics: bool,
    reap_termination_override: Option<ProcessTermination>,
    lose_ownership_while_reaping: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InjectedSignalError {
    Search,
    Permission,
}

impl fmt::Debug for ProcessRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessRequest")
            .field("command_bytes", &self.command.len())
            .field("environment_entries", &self.environment.len())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessRequestError {
    Command,
    Timeout,
    Environment,
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessControl {
    cancellation: CancellationToken,
    turn_deadline: Instant,
    action_deadline: Instant,
}

impl ProcessControl {
    pub(crate) fn new(
        cancellation: CancellationToken,
        turn_deadline: Instant,
        action_deadline: Instant,
    ) -> Self {
        Self {
            cancellation,
            turn_deadline,
            action_deadline,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProcessOutcome {
    NotStarted {
        turn_stop: ProcessTurnStop,
        cause: ProcessStartFailure,
    },
    StartedAndQuiescent(ProcessReport),
    StartedOwnershipLost {
        turn_stop: ProcessTurnStop,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessTurnStop {
    None,
    CallerCancelled,
    TurnTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessStartFailure {
    CallerCancelled,
    TurnTimeout,
    ActionTimeout,
    ObserverUnavailable,
    AsyncRuntimeUnavailable,
    PipePreflightFailed,
    SpawnFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessPrimaryCause {
    Natural,
    CallerCancelled,
    TurnTimeout,
    ActionTimeout,
    CommandTimeout,
    PipeSetupFailed,
    PipeReadFailed,
    OutputLimit,
    PipeDrainTimeout,
    BackgroundNotSupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessTermination {
    ExitCode(i32),
    Signal(i32),
}

impl ProcessTermination {
    pub(crate) fn canonical_signal_name(self) -> Option<String> {
        let Self::Signal(signal) = self else {
            return None;
        };
        canonical_signal_name(signal)
    }
}

pub(crate) struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedStream {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }
}

impl fmt::Debug for CapturedStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedStream")
            .field("bytes", &self.bytes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessFlags {
    output_limit_exceeded: bool,
    pipe_setup_failed: bool,
    pipe_read_failed: bool,
    signal_delivery_failed: bool,
    pipe_drain_timed_out: bool,
}

impl ProcessFlags {
    pub(crate) fn output_limit_exceeded(self) -> bool {
        self.output_limit_exceeded
    }

    pub(crate) fn pipe_setup_failed(self) -> bool {
        self.pipe_setup_failed
    }

    pub(crate) fn pipe_read_failed(self) -> bool {
        self.pipe_read_failed
    }

    pub(crate) fn signal_delivery_failed(self) -> bool {
        self.signal_delivery_failed
    }

    pub(crate) fn pipe_drain_timed_out(self) -> bool {
        self.pipe_drain_timed_out
    }
}

pub(crate) struct ProcessReport {
    termination: ProcessTermination,
    primary: ProcessPrimaryCause,
    turn_stop: ProcessTurnStop,
    stdout: CapturedStream,
    stderr: CapturedStream,
    flags: ProcessFlags,
}

impl ProcessReport {
    pub(crate) fn termination(&self) -> ProcessTermination {
        self.termination
    }

    pub(crate) fn primary(&self) -> ProcessPrimaryCause {
        self.primary
    }

    pub(crate) fn turn_stop(&self) -> ProcessTurnStop {
        self.turn_stop
    }

    pub(crate) fn stdout(&self) -> &CapturedStream {
        &self.stdout
    }

    pub(crate) fn stderr(&self) -> &CapturedStream {
        &self.stderr
    }

    pub(crate) fn flags(&self) -> ProcessFlags {
        self.flags
    }
}

impl fmt::Debug for ProcessReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessReport")
            .field("termination", &self.termination)
            .field("primary", &self.primary)
            .field("turn_stop", &self.turn_stop)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("flags", &self.flags)
            .finish()
    }
}

fn canonical_signal_name(signal: i32) -> Option<String> {
    if signal <= 0 {
        return None;
    }
    let known = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        libc::SIGURG => "SIGURG",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGVTALRM => "SIGVTALRM",
        libc::SIGPROF => "SIGPROF",
        libc::SIGWINCH => "SIGWINCH",
        libc::SIGIO => "SIGIO",
        libc::SIGSYS => "SIGSYS",
        #[cfg(target_os = "macos")]
        libc::SIGEMT => "SIGEMT",
        #[cfg(target_os = "macos")]
        libc::SIGINFO => "SIGINFO",
        #[cfg(target_os = "linux")]
        libc::SIGSTKFLT => "SIGSTKFLT",
        #[cfg(target_os = "linux")]
        libc::SIGPWR => "SIGPWR",
        _ => return Some(format!("SIG{signal}")),
    };
    Some(known.to_owned())
}

async fn run_process(
    runner: &ProcessRunner,
    request: ProcessRequest,
    control: ProcessControl,
) -> ProcessOutcome {
    let ProcessRequest {
        command,
        workdir,
        environment,
        timeout,
        permit,
        #[cfg(test)]
        test_hooks,
    } = request;
    if !Arc::ptr_eq(&runner.host, &permit.owner) {
        return not_started(
            ProcessTurnStop::None,
            ProcessStartFailure::ObserverUnavailable,
        );
    }
    if let Some(stop) = sample_start_stop(&control) {
        return not_started(stop.turn_stop(), stop.start_failure());
    }

    let preflight = reactor_preflight();
    if let Some(stop) = sample_start_stop(&control) {
        return not_started(stop.turn_stop(), stop.start_failure());
    }
    if let Err(error) = preflight {
        return not_started(ProcessTurnStop::None, error);
    }

    // Both Arc allocations happen before spawn. Installing the guard after a
    // successful spawn is then only stack construction and atomic stores.
    let armed = Arc::new(AtomicBool::new(false));
    let guard_armed = Arc::clone(&armed);
    let spawn_result = spawn::shell(command, workdir, environment.as_ref());
    let boundary_stop = sample_start_stop(&control);
    let child = match spawn_result {
        Ok(child) => child,
        Err(_) => {
            return match boundary_stop {
                Some(stop) => not_started(stop.turn_stop(), stop.start_failure()),
                None => not_started(ProcessTurnStop::None, ProcessStartFailure::SpawnFailed),
            };
        }
    };

    let pid = Pid::from_child(&child);
    armed.store(true, Ordering::Release);
    let guard = GroupGuard {
        leader: pid,
        armed: guard_armed,
    };
    let command_deadline = Instant::now() + timeout;
    let mut state = RunningProcess::new(
        Arc::clone(&runner.host),
        child,
        pid,
        armed,
        guard,
        command_deadline,
        #[cfg(test)]
        test_hooks,
    );
    if let Some(stop) = boundary_stop {
        state.observe_stop(stop);
    }
    state.adapt_pipes(&control);
    state.ensure_cleanup_started();
    drive_process(state, control).await
}

fn not_started(turn_stop: ProcessTurnStop, cause: ProcessStartFailure) -> ProcessOutcome {
    ProcessOutcome::NotStarted { turn_stop, cause }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservedStop {
    Caller,
    Turn,
    Action,
    Command,
}

impl ObservedStop {
    fn turn_stop(self) -> ProcessTurnStop {
        match self {
            Self::Caller => ProcessTurnStop::CallerCancelled,
            Self::Turn => ProcessTurnStop::TurnTimeout,
            Self::Action | Self::Command => ProcessTurnStop::None,
        }
    }

    fn start_failure(self) -> ProcessStartFailure {
        match self {
            Self::Caller => ProcessStartFailure::CallerCancelled,
            Self::Turn => ProcessStartFailure::TurnTimeout,
            Self::Action | Self::Command => ProcessStartFailure::ActionTimeout,
        }
    }

    fn primary(self) -> ProcessPrimaryCause {
        match self {
            Self::Caller => ProcessPrimaryCause::CallerCancelled,
            Self::Turn => ProcessPrimaryCause::TurnTimeout,
            Self::Action => ProcessPrimaryCause::ActionTimeout,
            Self::Command => ProcessPrimaryCause::CommandTimeout,
        }
    }
}

fn sample_start_stop(control: &ProcessControl) -> Option<ObservedStop> {
    if control.cancellation.is_cancelled() {
        Some(ObservedStop::Caller)
    } else if Instant::now() >= control.turn_deadline {
        Some(ObservedStop::Turn)
    } else if Instant::now() >= control.action_deadline {
        Some(ObservedStop::Action)
    } else {
        None
    }
}

fn reactor_preflight() -> Result<(), ProcessStartFailure> {
    let (tested, peer) = std::os::unix::net::UnixStream::pair()
        .map_err(|_| ProcessStartFailure::PipePreflightFailed)?;
    tested
        .set_nonblocking(true)
        .map_err(|_| ProcessStartFailure::PipePreflightFailed)?;
    let tested_flags =
        rustix::io::fcntl_getfd(&tested).map_err(|_| ProcessStartFailure::PipePreflightFailed)?;
    let peer_flags =
        rustix::io::fcntl_getfd(&peer).map_err(|_| ProcessStartFailure::PipePreflightFailed)?;
    if !tested_flags.contains(rustix::io::FdFlags::CLOEXEC)
        || !peer_flags.contains(rustix::io::FdFlags::CLOEXEC)
    {
        return Err(ProcessStartFailure::PipePreflightFailed);
    }
    let registration =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| AsyncFd::new(tested)));
    drop(peer);
    match registration {
        Ok(Ok(registered)) => {
            drop(registered);
            Ok(())
        }
        Ok(Err(_)) | Err(_) => Err(ProcessStartFailure::AsyncRuntimeUnavailable),
    }
}

struct GroupGuard {
    leader: Pid,
    armed: Arc<AtomicBool>,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        if matches!(
            observe_leader(self.leader),
            AnchorState::Running | AnchorState::Exited(_)
        ) {
            let _ = rustix::process::kill_process_group(self.leader, Signal::KILL);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessPhase {
    Observing,
    Reaping,
    Draining,
}

struct RunningProcess {
    host: Arc<host::Host>,
    child: Option<Child>,
    leader: Pid,
    session: Pid,
    harness: Pid,
    armed: Arc<AtomicBool>,
    _guard: GroupGuard,
    stdout: Option<AsyncFd<ChildStdout>>,
    stderr: Option<AsyncFd<ChildStderr>>,
    stdout_tail: capture::TailCapture,
    stderr_tail: capture::TailCapture,
    observed: capture::ObservedBudget,
    primary: Option<ProcessPrimaryCause>,
    turn_stop: ProcessTurnStop,
    natural: Option<ProcessTermination>,
    final_termination: Option<ProcessTermination>,
    flags: ProcessFlags,
    command_deadline: Option<Instant>,
    grace_deadline: Option<Instant>,
    drain_deadline: Option<Instant>,
    term_sent: bool,
    kill_sent: bool,
    phase: ProcessPhase,
    complete_passes: u8,
    next_scan_at: Option<Instant>,
    scan: Option<JoinHandle<host::GroupScan>>,
    reap: Option<JoinHandle<io::Result<ExitStatus>>>,
    stdout_first: bool,
    pipes_precede_housekeeping: bool,
    #[cfg(test)]
    test_hooks: ProcessTestHooks,
    #[cfg(test)]
    pipe_read_failure_fired: bool,
}

impl RunningProcess {
    fn new(
        host: Arc<host::Host>,
        child: Child,
        leader: Pid,
        armed: Arc<AtomicBool>,
        guard: GroupGuard,
        command_deadline: Instant,
        #[cfg(test)] test_hooks: ProcessTestHooks,
    ) -> Self {
        Self {
            host,
            child: Some(child),
            leader,
            session: leader,
            harness: rustix::process::getpid(),
            armed,
            _guard: guard,
            stdout: None,
            stderr: None,
            stdout_tail: capture::TailCapture::new(RETAINED_TAIL_BYTES),
            stderr_tail: capture::TailCapture::new(RETAINED_TAIL_BYTES),
            observed: capture::ObservedBudget::new(MAX_OBSERVED_BYTES),
            primary: None,
            turn_stop: ProcessTurnStop::None,
            natural: None,
            final_termination: None,
            flags: ProcessFlags::default(),
            command_deadline: Some(command_deadline),
            grace_deadline: None,
            drain_deadline: None,
            term_sent: false,
            kill_sent: false,
            phase: ProcessPhase::Observing,
            complete_passes: 0,
            next_scan_at: None,
            scan: None,
            reap: None,
            stdout_first: true,
            pipes_precede_housekeeping: true,
            #[cfg(test)]
            test_hooks,
            #[cfg(test)]
            pipe_read_failure_fired: false,
        }
    }

    fn adapt_pipes(&mut self, control: &ProcessControl) {
        let (stdout, stderr) = match self.child.as_mut() {
            Some(child) => (child.stdout.take(), child.stderr.take()),
            None => (None, None),
        };
        self.stdout = self.adapt_one(stdout, control, StreamKind::Stdout);
        self.stderr = self.adapt_one(stderr, control, StreamKind::Stderr);
        if self.flags.pipe_setup_failed {
            self.stdout = None;
            self.stderr = None;
            self.stdout_tail.mark_truncated();
            self.stderr_tail.mark_truncated();
        }
    }

    fn adapt_one<T>(
        &mut self,
        pipe: Option<T>,
        control: &ProcessControl,
        stream: StreamKind,
    ) -> Option<AsyncFd<T>>
    where
        T: AsFd + AsRawFd,
    {
        #[cfg(test)]
        let inject_failure = self.test_hooks.pipe_setup_failure == Some(stream);
        #[cfg(not(test))]
        let _ = stream;
        #[cfg(test)]
        let result = if inject_failure {
            drop(pipe);
            Err(())
        } else {
            pipe.ok_or(()).and_then(adapt_pipe)
        };
        #[cfg(not(test))]
        let result = pipe.ok_or(()).and_then(adapt_pipe);
        if let Some(stop) = self.sample_stop(control) {
            self.observe_stop(stop);
        }
        match result {
            Ok(pipe) => Some(pipe),
            Err(()) => {
                self.flags.pipe_setup_failed = true;
                self.primary
                    .get_or_insert(ProcessPrimaryCause::PipeSetupFailed);
                None
            }
        }
    }

    fn sample_stop(&self, control: &ProcessControl) -> Option<ObservedStop> {
        if self.turn_stop == ProcessTurnStop::None && control.cancellation.is_cancelled() {
            return Some(ObservedStop::Caller);
        }
        if self.turn_stop == ProcessTurnStop::None && Instant::now() >= control.turn_deadline {
            return Some(ObservedStop::Turn);
        }
        if self.primary.is_none() && Instant::now() >= control.action_deadline {
            return Some(ObservedStop::Action);
        }
        if self.primary.is_none()
            && self.natural.is_none()
            && self
                .command_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Some(ObservedStop::Command);
        }
        None
    }

    #[cfg(test)]
    fn inject_pipe_read_failure(
        &mut self,
        stream: StreamKind,
        result: io::Result<PipeRead>,
    ) -> io::Result<PipeRead> {
        if !self.pipe_read_failure_fired && self.test_hooks.pipe_read_failure == Some(stream) {
            self.pipe_read_failure_fired = true;
            Err(io::Error::other("injected pipe read failure"))
        } else {
            result
        }
    }

    fn observe_stop(&mut self, stop: ObservedStop) {
        if self.turn_stop == ProcessTurnStop::None {
            self.turn_stop = stop.turn_stop();
        }
        self.primary.get_or_insert(stop.primary());
    }

    fn ensure_cleanup_started(&mut self) {
        let Some(primary) = self.primary else {
            return;
        };
        if !self.armed.load(Ordering::Acquire) {
            if matches!(
                primary,
                ProcessPrimaryCause::CallerCancelled
                    | ProcessPrimaryCause::TurnTimeout
                    | ProcessPrimaryCause::ActionTimeout
                    | ProcessPrimaryCause::CommandTimeout
                    | ProcessPrimaryCause::OutputLimit
            ) {
                self.close_pipes();
            }
            return;
        }
        if primary == ProcessPrimaryCause::OutputLimit {
            self.send_kill();
            self.close_pipes();
        } else if !self.term_sent && !self.kill_sent {
            self.term_sent = true;
            self.send_signal(Signal::TERM);
            self.grace_deadline = Some(Instant::now() + TERM_GRACE);
        }
    }

    fn send_signal(&mut self, signal: Signal) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let result = rustix::process::kill_process_group(self.leader, signal);
        #[cfg(test)]
        match self.test_hooks.injected_signal_error {
            Some(InjectedSignalError::Search) => return,
            Some(InjectedSignalError::Permission) => {
                self.flags.signal_delivery_failed = true;
                return;
            }
            None => {}
        }
        if let Err(error) = result {
            if error != rustix::io::Errno::SRCH {
                self.flags.signal_delivery_failed = true;
            }
        }
    }

    fn send_kill(&mut self) {
        if self.kill_sent {
            return;
        }
        self.kill_sent = true;
        self.grace_deadline = None;
        self.send_signal(Signal::KILL);
    }

    fn close_pipes(&mut self) {
        if self.stdout.take().is_some() {
            self.stdout_tail.mark_truncated();
        }
        if self.stderr.take().is_some() {
            self.stderr_tail.mark_truncated();
        }
    }

    fn disarm(&mut self) {
        self.armed.store(false, Ordering::Release);
        self.grace_deadline = None;
    }

    fn start_scan_if_due(&mut self) {
        if self.phase != ProcessPhase::Observing
            || self.natural.is_none()
            || self.scan.is_some()
            || self
                .next_scan_at
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }
        let host = Arc::clone(&self.host);
        let leader = self.leader;
        let session = self.session;
        let harness = self.harness;
        #[cfg(test)]
        let hooks = self.test_hooks.clone();
        self.next_scan_at = None;
        self.scan = Some(tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if !hooks.scan_delay.is_zero() {
                std::thread::sleep(hooks.scan_delay);
            }
            #[cfg(test)]
            let result = hooks
                .scan_results
                .as_ref()
                .and_then(|results| {
                    results
                        .lock()
                        .ok()
                        .and_then(|mut results| results.pop_front())
                })
                .unwrap_or_else(|| host.scan_group(leader, session, harness));
            #[cfg(not(test))]
            let result = host.scan_group(leader, session, harness);
            #[cfg(test)]
            if let Some(finished) = hooks.scan_finished.as_ref() {
                finished.store(true, Ordering::Release);
            }
            #[cfg(test)]
            assert!(!hooks.scan_panics, "injected process observer panic");
            result
        }));
    }

    fn begin_reap(&mut self) -> Result<(), ()> {
        self.disarm();
        let Some(mut child) = self.child.take() else {
            return Err(());
        };
        self.phase = ProcessPhase::Reaping;
        self.scan = None;
        #[cfg(test)]
        let hooks = self.test_hooks.clone();
        self.reap = Some(tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if !hooks.reap_delay.is_zero() {
                std::thread::sleep(hooks.reap_delay);
            }
            let result = child.wait();
            #[cfg(test)]
            if let Some(finished) = hooks.reap_finished.as_ref() {
                finished.store(true, Ordering::Release);
            }
            #[cfg(test)]
            assert!(!hooks.reap_panics, "injected direct-child reap panic");
            result
        }));
        Ok(())
    }

    fn finish_report(self) -> ProcessOutcome {
        let Some(termination) = self.final_termination else {
            return ProcessOutcome::StartedOwnershipLost {
                turn_stop: self.turn_stop,
            };
        };
        let (stdout, stdout_truncated) = self.stdout_tail.finish();
        let (stderr, stderr_truncated) = self.stderr_tail.finish();
        ProcessOutcome::StartedAndQuiescent(ProcessReport {
            termination,
            primary: self.primary.unwrap_or(ProcessPrimaryCause::Natural),
            turn_stop: self.turn_stop,
            stdout: CapturedStream {
                bytes: stdout,
                truncated: stdout_truncated,
            },
            stderr: CapturedStream {
                bytes: stderr,
                truncated: stderr_truncated,
            },
            flags: self.flags,
        })
    }
}

fn adapt_pipe<T>(pipe: T) -> Result<AsyncFd<T>, ()>
where
    T: AsFd + AsRawFd,
{
    let flags = rustix::fs::fcntl_getfl(&pipe).map_err(|_| ())?;
    rustix::fs::fcntl_setfl(&pipe, flags | OFlags::NONBLOCK).map_err(|_| ())?;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| AsyncFd::new(pipe))) {
        Ok(Ok(pipe)) => Ok(pipe),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}

#[derive(Debug)]
enum RunnerEvent {
    Caller,
    Turn,
    Action,
    Command,
    Grace,
    Drain,
    Tick,
    Stdout(io::Result<PipeRead>),
    Stderr(io::Result<PipeRead>),
    Scan(Result<host::GroupScan, JoinError>),
    Reap(Result<io::Result<ExitStatus>, JoinError>),
}

#[derive(Debug)]
enum PipeRead {
    Bytes(Vec<u8>),
    Eof,
}

async fn drive_process(mut state: RunningProcess, control: ProcessControl) -> ProcessOutcome {
    let mut ticker =
        tokio::time::interval_at(Instant::now() + OBSERVER_INTERVAL, OBSERVER_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if let Some(stop) = state.sample_stop(&control) {
            state.observe_stop(stop);
            state.ensure_cleanup_started();
        }
        state.start_scan_if_due();
        if state.phase == ProcessPhase::Draining && state.stdout.is_none() && state.stderr.is_none()
        {
            return state.finish_report();
        }

        let event = next_runner_event(&mut state, &control, &mut ticker).await;
        if matches!(event, RunnerEvent::Stdout(_) | RunnerEvent::Stderr(_)) {
            state.pipes_precede_housekeeping = false;
        } else if matches!(
            event,
            RunnerEvent::Scan(_) | RunnerEvent::Reap(_) | RunnerEvent::Tick
        ) {
            state.pipes_precede_housekeeping = true;
        }
        #[cfg(test)]
        if matches!(&event, RunnerEvent::Tick)
            && state.phase == ProcessPhase::Observing
            && state.natural.is_none()
            && state.test_hooks.lose_ownership_while_running
        {
            return finish_ownership_lost(state, AnchorState::Running, &control).await;
        }
        #[cfg(test)]
        if matches!(&event, RunnerEvent::Tick)
            && state.phase == ProcessPhase::Observing
            && state.scan.is_some()
            && state.test_hooks.lose_anchor_while_scan_runs
        {
            return finish_ownership_lost(state, AnchorState::Indeterminate, &control).await;
        }
        #[cfg(test)]
        if matches!(&event, RunnerEvent::Tick)
            && state.phase == ProcessPhase::Reaping
            && state.test_hooks.lose_ownership_while_reaping
        {
            return finish_ownership_lost(state, AnchorState::Indeterminate, &control).await;
        }

        match event {
            RunnerEvent::Caller => state.observe_stop(ObservedStop::Caller),
            RunnerEvent::Turn => state.observe_stop(ObservedStop::Turn),
            RunnerEvent::Action => state.observe_stop(ObservedStop::Action),
            RunnerEvent::Command => state.observe_stop(ObservedStop::Command),
            RunnerEvent::Grace => state.send_kill(),
            RunnerEvent::Drain => {
                state.flags.pipe_drain_timed_out = true;
                state
                    .primary
                    .get_or_insert(ProcessPrimaryCause::PipeDrainTimeout);
                state.close_pipes();
            }
            RunnerEvent::Tick if state.phase != ProcessPhase::Observing => {}
            RunnerEvent::Tick => {
                poll_ready_pipes_before_leader(&mut state);
                state.ensure_cleanup_started();
                match observe_leader(state.leader) {
                    AnchorState::Running => {}
                    AnchorState::Exited(termination) => {
                        if let Some(previous) = state.natural {
                            if previous != termination {
                                return finish_ownership_lost(
                                    state,
                                    AnchorState::Exited(previous),
                                    &control,
                                )
                                .await;
                            }
                        } else {
                            state.natural = Some(termination);
                            state.command_deadline = None;
                            state.next_scan_at = Some(Instant::now());
                        }
                        state.start_scan_if_due();
                    }
                    anchor @ (AnchorState::Unowned | AnchorState::Indeterminate) => {
                        return finish_ownership_lost(state, anchor, &control).await;
                    }
                }
            }
            RunnerEvent::Stdout(result) => {
                #[cfg(test)]
                let result = state.inject_pipe_read_failure(StreamKind::Stdout, result);
                handle_pipe_result(&mut state, StreamKind::Stdout, result);
                state.stdout_first = false;
            }
            RunnerEvent::Stderr(result) => {
                #[cfg(test)]
                let result = state.inject_pipe_read_failure(StreamKind::Stderr, result);
                handle_pipe_result(&mut state, StreamKind::Stderr, result);
                state.stdout_first = true;
            }
            RunnerEvent::Scan(result) => {
                state.scan = None;
                let retained = observe_leader(state.leader);
                if !matches!(
                    (state.natural, retained),
                    (Some(expected), AnchorState::Exited(actual)) if expected == actual
                ) {
                    return finish_ownership_lost(state, retained, &control).await;
                }
                match result {
                    Err(_) | Ok(host::GroupScan::OwnershipLost) => {
                        let anchor = state
                            .natural
                            .map_or_else(|| observe_leader(state.leader), AnchorState::Exited);
                        return finish_ownership_lost(state, anchor, &control).await;
                    }
                    Ok(host::GroupScan::Live) => {
                        state.complete_passes = 0;
                        if state.natural.is_some() && state.primary.is_none() {
                            state.primary = Some(ProcessPrimaryCause::BackgroundNotSupported);
                        }
                        state.next_scan_at = Some(Instant::now() + OBSERVER_INTERVAL);
                    }
                    Ok(host::GroupScan::Unknown) => {
                        state.complete_passes = 0;
                        state.next_scan_at = Some(Instant::now() + OBSERVER_INTERVAL);
                    }
                    #[cfg(target_os = "linux")]
                    Ok(host::GroupScan::Mutated) => {
                        state.complete_passes = 0;
                        state.next_scan_at = Some(Instant::now());
                    }
                    Ok(host::GroupScan::Complete) => {
                        state.complete_passes = state.complete_passes.saturating_add(1);
                        if state.complete_passes >= 2 {
                            if state.begin_reap().is_err() {
                                return finish_ownership_lost(
                                    state,
                                    AnchorState::Indeterminate,
                                    &control,
                                )
                                .await;
                            }
                        } else {
                            state.next_scan_at = Some(Instant::now() + OBSERVER_INTERVAL);
                        }
                    }
                }
            }
            RunnerEvent::Reap(result) => {
                state.reap = None;
                let final_status = match result {
                    Ok(Ok(status)) => termination_from_exit_status(status),
                    Ok(Err(_)) | Err(_) => None,
                };
                #[cfg(test)]
                let final_status = state.test_hooks.reap_termination_override.or(final_status);
                if final_status.is_none() || final_status != state.natural {
                    return finish_ownership_lost(state, AnchorState::Indeterminate, &control)
                        .await;
                }
                state.final_termination = final_status;
                state.phase = ProcessPhase::Draining;
                state.drain_deadline = Some(Instant::now() + PIPE_DRAIN_GRACE);
            }
        }
        state.ensure_cleanup_started();
    }
}

async fn next_runner_event(
    state: &mut RunningProcess,
    control: &ProcessControl,
    ticker: &mut tokio::time::Interval,
) -> RunnerEvent {
    let caller_enabled = state.turn_stop == ProcessTurnStop::None;
    let turn_deadline = caller_enabled.then_some(control.turn_deadline);
    let action_deadline = state.primary.is_none().then_some(control.action_deadline);
    let command_deadline = (state.primary.is_none() && state.natural.is_none())
        .then_some(state.command_deadline)
        .flatten();
    let grace_deadline = state.grace_deadline;
    let drain_deadline = state.drain_deadline;
    let stdout_limit = state.observed.next_read_len(READ_CHUNK_BYTES);
    let stderr_limit = stdout_limit;

    match (state.stdout_first, state.pipes_precede_housekeeping) {
        (true, true) => tokio::select! {
            biased;
            _ = wait_for_cancel(&control.cancellation, caller_enabled) => RunnerEvent::Caller,
            _ = wait_for_deadline(turn_deadline) => RunnerEvent::Turn,
            _ = wait_for_deadline(action_deadline) => RunnerEvent::Action,
            _ = wait_for_deadline(command_deadline) => RunnerEvent::Command,
            _ = wait_for_deadline(grace_deadline) => RunnerEvent::Grace,
            _ = wait_for_deadline(drain_deadline) => RunnerEvent::Drain,
            result = read_pipe(&mut state.stdout, stdout_limit) => RunnerEvent::Stdout(result),
            result = read_pipe(&mut state.stderr, stderr_limit) => RunnerEvent::Stderr(result),
            event = wait_for_housekeeping(&mut state.scan, &mut state.reap, ticker) => event,
        },
        (false, true) => tokio::select! {
            biased;
            _ = wait_for_cancel(&control.cancellation, caller_enabled) => RunnerEvent::Caller,
            _ = wait_for_deadline(turn_deadline) => RunnerEvent::Turn,
            _ = wait_for_deadline(action_deadline) => RunnerEvent::Action,
            _ = wait_for_deadline(command_deadline) => RunnerEvent::Command,
            _ = wait_for_deadline(grace_deadline) => RunnerEvent::Grace,
            _ = wait_for_deadline(drain_deadline) => RunnerEvent::Drain,
            result = read_pipe(&mut state.stderr, stderr_limit) => RunnerEvent::Stderr(result),
            result = read_pipe(&mut state.stdout, stdout_limit) => RunnerEvent::Stdout(result),
            event = wait_for_housekeeping(&mut state.scan, &mut state.reap, ticker) => event,
        },
        (true, false) => tokio::select! {
            biased;
            _ = wait_for_cancel(&control.cancellation, caller_enabled) => RunnerEvent::Caller,
            _ = wait_for_deadline(turn_deadline) => RunnerEvent::Turn,
            _ = wait_for_deadline(action_deadline) => RunnerEvent::Action,
            _ = wait_for_deadline(command_deadline) => RunnerEvent::Command,
            _ = wait_for_deadline(grace_deadline) => RunnerEvent::Grace,
            _ = wait_for_deadline(drain_deadline) => RunnerEvent::Drain,
            event = wait_for_housekeeping(&mut state.scan, &mut state.reap, ticker) => event,
            result = read_pipe(&mut state.stdout, stdout_limit) => RunnerEvent::Stdout(result),
            result = read_pipe(&mut state.stderr, stderr_limit) => RunnerEvent::Stderr(result),
        },
        (false, false) => tokio::select! {
            biased;
            _ = wait_for_cancel(&control.cancellation, caller_enabled) => RunnerEvent::Caller,
            _ = wait_for_deadline(turn_deadline) => RunnerEvent::Turn,
            _ = wait_for_deadline(action_deadline) => RunnerEvent::Action,
            _ = wait_for_deadline(command_deadline) => RunnerEvent::Command,
            _ = wait_for_deadline(grace_deadline) => RunnerEvent::Grace,
            _ = wait_for_deadline(drain_deadline) => RunnerEvent::Drain,
            event = wait_for_housekeeping(&mut state.scan, &mut state.reap, ticker) => event,
            result = read_pipe(&mut state.stderr, stderr_limit) => RunnerEvent::Stderr(result),
            result = read_pipe(&mut state.stdout, stdout_limit) => RunnerEvent::Stdout(result),
        },
    }
}

async fn wait_for_housekeeping(
    scan: &mut Option<JoinHandle<host::GroupScan>>,
    reap: &mut Option<JoinHandle<io::Result<ExitStatus>>>,
    ticker: &mut tokio::time::Interval,
) -> RunnerEvent {
    tokio::select! {
        biased;
        result = wait_for_scan(scan) => RunnerEvent::Scan(result),
        result = wait_for_reap(reap) => RunnerEvent::Reap(result),
        _ = ticker.tick() => RunnerEvent::Tick,
    }
}

fn poll_ready_pipes_before_leader(state: &mut RunningProcess) {
    let stdout_first = state.stdout_first;
    for stream in if stdout_first {
        [StreamKind::Stdout, StreamKind::Stderr]
    } else {
        [StreamKind::Stderr, StreamKind::Stdout]
    } {
        let limit = state.observed.next_read_len(READ_CHUNK_BYTES);
        if limit == 0 {
            break;
        }
        let result = match stream {
            StreamKind::Stdout => try_read_pipe(&mut state.stdout, limit),
            StreamKind::Stderr => try_read_pipe(&mut state.stderr, limit),
        };
        if let Some(result) = result {
            let got_bytes = matches!(result, Ok(PipeRead::Bytes(_)));
            handle_pipe_result(state, stream, result);
            if got_bytes {
                state.stdout_first = !matches!(stream, StreamKind::Stdout);
            }
        }
    }
}

fn try_read_pipe<T>(pipe: &mut Option<AsyncFd<T>>, limit: usize) -> Option<io::Result<PipeRead>>
where
    T: AsRawFd + Read,
{
    let pipe = pipe.as_mut()?;
    let mut bytes = vec![0_u8; limit];
    match pipe.try_io_mut(Interest::READABLE, |inner| inner.read(&mut bytes)) {
        Ok(0) => Some(Ok(PipeRead::Eof)),
        Ok(count) => {
            bytes.truncate(count);
            Some(Ok(PipeRead::Bytes(bytes)))
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
        Err(error) => Some(Err(error)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamKind {
    Stdout,
    Stderr,
}

fn handle_pipe_result(
    state: &mut RunningProcess,
    stream: StreamKind,
    result: io::Result<PipeRead>,
) {
    match result {
        Ok(PipeRead::Bytes(bytes)) => {
            match stream {
                StreamKind::Stdout => state.stdout_tail.push(&bytes),
                StreamKind::Stderr => state.stderr_tail.push(&bytes),
            }
            if state.observed.record(bytes.len()) {
                state.flags.output_limit_exceeded = true;
                state
                    .primary
                    .get_or_insert(ProcessPrimaryCause::OutputLimit);
                // The cap is an independent memory-safety escalation. Even
                // when another cause already owns the result, there is no
                // remaining TERM grace after the first over-limit byte.
                state.send_kill();
                state.close_pipes();
            }
        }
        Ok(PipeRead::Eof) => match stream {
            StreamKind::Stdout => state.stdout = None,
            StreamKind::Stderr => state.stderr = None,
        },
        Err(_) => {
            state.flags.pipe_read_failed = true;
            state
                .primary
                .get_or_insert(ProcessPrimaryCause::PipeReadFailed);
            match stream {
                StreamKind::Stdout => {
                    state.stdout = None;
                    state.stdout_tail.mark_truncated();
                }
                StreamKind::Stderr => {
                    state.stderr = None;
                    state.stderr_tail.mark_truncated();
                }
            }
        }
    }
}

async fn read_pipe<T>(pipe: &mut Option<AsyncFd<T>>, limit: usize) -> io::Result<PipeRead>
where
    T: AsRawFd + Read,
{
    let Some(pipe) = pipe.as_mut() else {
        return pending().await;
    };
    if limit == 0 {
        return pending().await;
    }
    loop {
        let mut readiness = pipe.readable_mut().await?;
        let mut bytes = vec![0_u8; limit];
        match readiness.try_io(|registered| registered.get_mut().read(&mut bytes)) {
            Ok(Ok(0)) => return Ok(PipeRead::Eof),
            Ok(Ok(count)) => {
                bytes.truncate(count);
                return Ok(PipeRead::Bytes(bytes));
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => continue,
        }
    }
}

async fn wait_for_cancel(cancellation: &CancellationToken, enabled: bool) {
    if enabled {
        cancellation.cancelled().await;
    } else {
        pending::<()>().await;
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending::<()>().await,
    }
}

async fn wait_for_scan(
    scan: &mut Option<JoinHandle<host::GroupScan>>,
) -> Result<host::GroupScan, JoinError> {
    match scan {
        Some(scan) => scan.await,
        None => pending().await,
    }
}

async fn wait_for_reap(
    reap: &mut Option<JoinHandle<io::Result<ExitStatus>>>,
) -> Result<io::Result<ExitStatus>, JoinError> {
    match reap {
        Some(reap) => reap.await,
        None => pending().await,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnchorState {
    Running,
    Exited(ProcessTermination),
    Unowned,
    Indeterminate,
}

fn observe_leader(leader: Pid) -> AnchorState {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    loop {
        match rustix::process::waitid(WaitId::Pid(leader), options) {
            Ok(None) => return AnchorState::Running,
            Ok(Some(status)) => {
                if let Some(code) = status.exit_status().filter(|code| *code >= 0) {
                    return AnchorState::Exited(ProcessTermination::ExitCode(code));
                }
                if let Some(signal) = status.terminating_signal().filter(|signal| *signal > 0) {
                    return AnchorState::Exited(ProcessTermination::Signal(signal));
                }
                return AnchorState::Indeterminate;
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::CHILD) => return AnchorState::Unowned,
            Err(_) => return AnchorState::Indeterminate,
        }
    }
}

fn termination_from_exit_status(status: ExitStatus) -> Option<ProcessTermination> {
    if let Some(code) = status.code().filter(|code| *code >= 0) {
        return Some(ProcessTermination::ExitCode(code));
    }
    status
        .signal()
        .filter(|signal| *signal > 0)
        .map(ProcessTermination::Signal)
}

async fn finish_ownership_lost(
    mut state: RunningProcess,
    mut anchor: AnchorState,
    control: &ProcessControl,
) -> ProcessOutcome {
    state.close_pipes();
    if let Some(mut scan) = state.scan.take() {
        loop {
            if let Some(stop) = sample_outer_stop(state.turn_stop, control) {
                state.observe_stop(stop);
            }
            let caller_enabled = state.turn_stop == ProcessTurnStop::None;
            tokio::select! {
                biased;
                _ = wait_for_cancel(&control.cancellation, caller_enabled) => {
                    state.observe_stop(ObservedStop::Caller);
                }
                _ = wait_for_deadline(caller_enabled.then_some(control.turn_deadline)) => {
                    state.observe_stop(ObservedStop::Turn);
                }
                _ = &mut scan => break,
            }
        }
    }
    if let Some(mut reap) = state.reap.take() {
        loop {
            if let Some(stop) = sample_outer_stop(state.turn_stop, control) {
                state.observe_stop(stop);
            }
            let caller_enabled = state.turn_stop == ProcessTurnStop::None;
            tokio::select! {
                biased;
                _ = wait_for_cancel(&control.cancellation, caller_enabled) => {
                    state.observe_stop(ObservedStop::Caller);
                }
                _ = wait_for_deadline(caller_enabled.then_some(control.turn_deadline)) => {
                    state.observe_stop(ObservedStop::Turn);
                }
                _ = &mut reap => break,
            }
        }
    }
    if matches!(anchor, AnchorState::Running | AnchorState::Exited(_)) {
        state.send_kill();
    }

    let mut ticker = tokio::time::interval(OBSERVER_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    while anchor == AnchorState::Running {
        if let Some(stop) = sample_outer_stop(state.turn_stop, control) {
            state.observe_stop(stop);
        }
        let caller_enabled = state.turn_stop == ProcessTurnStop::None;
        tokio::select! {
            biased;
            _ = wait_for_cancel(&control.cancellation, caller_enabled) => {
                state.observe_stop(ObservedStop::Caller);
            }
            _ = wait_for_deadline(caller_enabled.then_some(control.turn_deadline)) => {
                state.observe_stop(ObservedStop::Turn);
            }
            _ = ticker.tick() => {
                anchor = observe_leader(state.leader);
            }
        }
    }
    state.disarm();

    if let Some(mut child) = state.child.take() {
        let mut wait = tokio::task::spawn_blocking(move || child.wait());
        loop {
            if let Some(stop) = sample_outer_stop(state.turn_stop, control) {
                state.observe_stop(stop);
            }
            let caller_enabled = state.turn_stop == ProcessTurnStop::None;
            tokio::select! {
                biased;
                _ = wait_for_cancel(&control.cancellation, caller_enabled) => {
                    state.observe_stop(ObservedStop::Caller);
                }
                _ = wait_for_deadline(caller_enabled.then_some(control.turn_deadline)) => {
                    state.observe_stop(ObservedStop::Turn);
                }
                _ = &mut wait => break,
            }
        }
    }
    ProcessOutcome::StartedOwnershipLost {
        turn_stop: state.turn_stop,
    }
}

fn sample_outer_stop(current: ProcessTurnStop, control: &ProcessControl) -> Option<ObservedStop> {
    if current != ProcessTurnStop::None {
        None
    } else if control.cancellation.is_cancelled() {
        Some(ObservedStop::Caller)
    } else if Instant::now() >= control.turn_deadline {
        Some(ObservedStop::Turn)
    } else {
        None
    }
}

#[cfg(test)]
mod api_tests {
    use std::time::Instant as StdInstant;

    use super::*;

    async fn runner_request(
        runner: &ProcessRunner,
        command: &str,
        timeout: Duration,
    ) -> (ProcessRequest, ProcessControl) {
        let checker = runner.clone();
        let cancellation = CancellationToken::new();
        let check_token = cancellation.clone();
        let permit = tokio::task::spawn_blocking(move || checker.pre_spawn_check(&check_token))
            .await
            .unwrap()
            .unwrap();
        let directory: OwnedFd = std::fs::File::open(".").unwrap().into();
        let environment: Arc<[(OsString, OsString)]> = Arc::from([
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (OsString::from("TERM"), OsString::from("dumb")),
        ]);
        let request =
            ProcessRequest::new(command.to_owned(), directory, environment, timeout, permit)
                .unwrap();
        let control = ProcessControl::new(
            cancellation,
            Instant::now() + Duration::from_secs(20),
            Instant::now() + Duration::from_secs(10),
        );
        (request, control)
    }

    fn shell_quote(value: &std::path::Path) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
    }

    async fn wait_for_file(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(Instant::now() < deadline, "helper file was not created");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn signal_names_are_locale_independent_and_have_a_numeric_fallback() {
        assert_eq!(
            canonical_signal_name(libc::SIGTERM).as_deref(),
            Some("SIGTERM")
        );
        assert_eq!(canonical_signal_name(123).as_deref(), Some("SIG123"));
        assert_eq!(canonical_signal_name(0), None);
    }

    #[test]
    fn request_timeout_accepts_exactly_295_seconds_and_rejects_both_sides() {
        let runner = ProcessRunner::open().unwrap();
        let request = |timeout| {
            let permit = runner.pre_spawn_check(&CancellationToken::new()).unwrap();
            ProcessRequest::new(
                "true".to_owned(),
                std::fs::File::open(".").unwrap().into(),
                Arc::from([]),
                timeout,
                permit,
            )
        };
        assert_eq!(
            request(Duration::ZERO).unwrap_err(),
            ProcessRequestError::Timeout
        );
        assert!(request(Duration::from_millis(295_000)).is_ok());
        assert_eq!(
            request(Duration::from_millis(295_001)).unwrap_err(),
            ProcessRequestError::Timeout
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runner_keeps_streams_separate_and_reports_a_natural_nonzero_exit() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(
            &runner,
            "printf stdout; printf stderr >&2; exit 7",
            Duration::from_secs(2),
        )
        .await;
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("foreground process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::Natural);
        assert_eq!(report.termination(), ProcessTermination::ExitCode(7));
        assert_eq!(report.stdout().bytes(), b"stdout");
        assert_eq!(report.stderr().bytes(), b"stderr");
        assert!(!report.stdout().truncated());
        assert!(!report.stderr().truncated());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_deadline_terminates_and_reaps_the_group() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) =
            runner_request(&runner, "sleep 60", Duration::from_millis(25)).await;
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("timed out process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::CommandTimeout);
        assert_eq!(
            report.termination().canonical_signal_name().as_deref(),
            Some("SIGTERM")
        );
        assert_eq!(report.turn_stop(), ProcessTurnStop::None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_live_background_member_is_stopped_before_return() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(
            &runner,
            "sleep 60 >/dev/null 2>&1 &",
            Duration::from_secs(2),
        )
        .await;
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("background group did not settle");
        };
        assert_eq!(
            report.primary(),
            ProcessPrimaryCause::BackgroundNotSupported
        );
        assert_eq!(report.termination(), ProcessTermination::ExitCode(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_silent_background_member_holding_both_pipes_is_detected_by_the_tick() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) =
            runner_request(&runner, "sleep 60 &", Duration::from_secs(2)).await;
        let started = StdInstant::now();
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("silent inherited-pipe background group did not settle");
        };
        assert_eq!(
            report.primary(),
            ProcessPrimaryCause::BackgroundNotSupported
        );
        assert_eq!(report.termination(), ProcessTermination::ExitCode(0));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn combined_output_accepts_exactly_eight_mibibytes() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(
            &runner,
            "head -c 4194304 /dev/zero; head -c 4194304 /dev/zero >&2",
            Duration::from_secs(5),
        )
        .await;
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("exact-limit process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::Natural);
        assert!(!report.flags().output_limit_exceeded());
        assert_eq!(report.stdout().bytes().len(), RETAINED_TAIL_BYTES);
        assert_eq!(report.stderr().bytes().len(), RETAINED_TAIL_BYTES);
        assert!(report.stdout().truncated());
        assert!(report.stderr().truncated());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_byte_over_the_combined_limit_forces_immediate_kill() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(&runner, "yes x", Duration::from_secs(5)).await;
        let started = StdInstant::now();
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("over-limit process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::OutputLimit);
        assert!(report.flags().output_limit_exceeded());
        assert!(started.elapsed() < TERM_GRACE);
        assert!(report.stdout().truncated());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_cap_skips_grace_even_after_command_timeout_won() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let directory =
            std::env::temp_dir().join(format!("dsh-timeout-cap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let ready = directory.join("ready");
        let gate = directory.join("gate");
        let command = format!(
            "trap '' TERM; touch {}; while [ ! -f {} ]; do :; done; yes x",
            shell_quote(&ready),
            shell_quote(&gate)
        );
        let (request, control) =
            runner_request(&runner, &command, Duration::from_millis(500)).await;
        let task = tokio::spawn({
            let runner = runner.clone();
            async move { runner.run(request, control).await }
        });
        wait_for_file(&ready).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let started = StdInstant::now();
        std::fs::write(&gate, b"go").unwrap();
        let ProcessOutcome::StartedAndQuiescent(report) = task.await.unwrap() else {
            panic!("timeout-plus-cap process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::CommandTimeout);
        assert!(report.flags().output_limit_exceeded());
        assert!(started.elapsed() < TERM_GRACE);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_cap_skips_grace_even_after_caller_cancellation_won() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let directory = std::env::temp_dir().join(format!("dsh-cap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let ready = directory.join("ready");
        let gate = directory.join("gate");
        let command = format!(
            "trap '' TERM; touch {}; while [ ! -f {} ]; do :; done; yes x",
            shell_quote(&ready),
            shell_quote(&gate)
        );
        let (request, control) = runner_request(&runner, &command, Duration::from_secs(5)).await;
        let cancellation = control.cancellation.clone();
        let task = tokio::spawn({
            let runner = runner.clone();
            async move { runner.run(request, control).await }
        });
        wait_for_file(&ready).await;
        cancellation.cancel();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let started = StdInstant::now();
        std::fs::write(&gate, b"go").unwrap();
        let ProcessOutcome::StartedAndQuiescent(report) = task.await.unwrap() else {
            panic!("cancel-plus-cap process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::CallerCancelled);
        assert!(report.flags().output_limit_exceeded());
        assert!(started.elapsed() < TERM_GRACE);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn both_ready_streams_are_drained_without_deadlock_or_merging() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let command = "(yes O | head -c 131072) & (yes E | head -c 131072 >&2) & wait";
        let (request, control) = runner_request(&runner, command, Duration::from_secs(5)).await;
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("dual-stream process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::Natural);
        assert_eq!(report.stdout().bytes().len(), RETAINED_TAIL_BYTES);
        assert_eq!(report.stderr().bytes().len(), RETAINED_TAIL_BYTES);
        assert!(
            report
                .stdout()
                .bytes()
                .iter()
                .all(|byte| *byte == b'O' || *byte == b'\n')
        );
        assert!(
            report
                .stderr()
                .bytes()
                .iter()
                .all(|byte| *byte == b'E' || *byte == b'\n')
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn term_trapping_leader_and_same_group_descendant_reach_kill_after_the_fixed_grace() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let directory =
            std::env::temp_dir().join(format!("dsh-term-traps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let child_ready = directory.join("child-ready");
        let all_ready = directory.join("all-ready");
        let command = format!(
            "trap '' TERM; (trap '' TERM; touch {}; while :; do /bin/sleep 1; done) & while [ ! -f {} ]; do :; done; touch {}; while :; do /bin/sleep 1; done",
            shell_quote(&child_ready),
            shell_quote(&child_ready),
            shell_quote(&all_ready)
        );
        let (request, control) = runner_request(&runner, &command, Duration::from_secs(10)).await;
        let cancellation = control.cancellation.clone();
        let task = tokio::spawn({
            let runner = runner.clone();
            async move { runner.run(request, control).await }
        });
        wait_for_file(&all_ready).await;
        let started = StdInstant::now();
        cancellation.cancel();
        let ProcessOutcome::StartedAndQuiescent(report) = task.await.unwrap() else {
            panic!("TERM-ignoring process did not settle");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::CallerCancelled);
        assert_eq!(
            report.termination().canonical_signal_name().as_deref(),
            Some("SIGKILL")
        );
        assert!(started.elapsed() >= TERM_GRACE);
        assert!(started.elapsed() < TERM_GRACE + Duration::from_secs(2));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_a_spawned_sentinel_is_started_and_quiescent() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let directory = std::env::temp_dir().join(format!("dsh-process-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let sentinel = directory.join("spawned");
        let command = format!("touch {}; sleep 60", shell_quote(&sentinel));
        let (request, control) = runner_request(&runner, &command, Duration::from_secs(5)).await;
        let cancellation = control.cancellation.clone();
        let task = tokio::spawn({
            let runner = runner.clone();
            async move { runner.run(request, control).await }
        });
        wait_for_file(&sentinel).await;
        cancellation.cancel();
        let ProcessOutcome::StartedAndQuiescent(report) = task.await.unwrap() else {
            panic!("post-spawn cancellation lost ownership");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::CallerCancelled);
        assert_eq!(report.turn_stop(), ProcessTurnStop::CallerCancelled);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_run_starts_no_process() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(&runner, "exit 99", Duration::from_secs(1)).await;
        control.cancellation.cancel();
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::NotStarted {
                turn_stop: ProcessTurnStop::CallerCancelled,
                cause: ProcessStartFailure::CallerCancelled,
            }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn natural_status_disarms_the_command_deadline_during_a_slow_scan() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(&runner, "true", Duration::from_millis(25)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            scan_delay: Duration::from_millis(80),
            ..ProcessTestHooks::default()
        });
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("slow observer lost a natural result");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::Natural);
        assert_eq!(report.termination(), ProcessTermination::ExitCode(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ownership_loss_awaits_an_inflight_scanner() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            scan_delay: Duration::from_millis(80),
            scan_finished: Some(Arc::clone(&finished)),
            lose_anchor_while_scan_runs: true,
            ..ProcessTestHooks::default()
        });
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));
        assert!(finished.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ownership_loss_awaits_an_inflight_reaper() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            reap_delay: Duration::from_millis(80),
            reap_finished: Some(Arc::clone(&finished)),
            lose_ownership_while_reaping: true,
            ..ProcessTestHooks::default()
        });
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));
        assert!(finished.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ownership_loss_with_a_running_leader_still_kills_and_reaps_the_direct_child() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(
            &runner,
            "trap '' TERM; while :; do sleep 1; done",
            Duration::from_secs(2),
        )
        .await;
        let request = request.with_test_hooks(ProcessTestHooks {
            lose_ownership_while_running: true,
            ..ProcessTestHooks::default()
        });
        let started = StdInstant::now();
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn either_pipe_adapter_failure_stays_started_and_cleans_the_group() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        for stream in [StreamKind::Stdout, StreamKind::Stderr] {
            let (request, control) =
                runner_request(&runner, "sleep 60", Duration::from_secs(2)).await;
            let request = request.with_test_hooks(ProcessTestHooks {
                pipe_setup_failure: Some(stream),
                ..ProcessTestHooks::default()
            });
            let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await
            else {
                panic!("pipe setup failure lost process ownership");
            };
            assert_eq!(report.primary(), ProcessPrimaryCause::PipeSetupFailed);
            assert!(report.flags().pipe_setup_failed());
            assert!(report.stdout().truncated());
            assert!(report.stderr().truncated());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pipe_read_failure_stays_started_and_cleans_the_group() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) =
            runner_request(&runner, "printf data; sleep 60", Duration::from_secs(2)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            pipe_read_failure: Some(StreamKind::Stdout),
            ..ProcessTestHooks::default()
        });
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("pipe read failure lost process ownership");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::PipeReadFailed);
        assert!(report.flags().pipe_read_failed());
        assert!(report.stdout().truncated());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn signal_esrch_is_only_observer_evidence_but_eperm_sets_the_warning() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        for (injected, expected_flag) in [
            (InjectedSignalError::Search, false),
            (InjectedSignalError::Permission, true),
        ] {
            let (request, control) =
                runner_request(&runner, "sleep 60", Duration::from_millis(20)).await;
            let request = request.with_test_hooks(ProcessTestHooks {
                injected_signal_error: Some(injected),
                ..ProcessTestHooks::default()
            });
            let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await
            else {
                panic!("signal fault lost process ownership");
            };
            assert_eq!(report.primary(), ProcessPrimaryCause::CommandTimeout);
            assert_eq!(report.flags().signal_delivery_failed(), expected_flag);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observer_unknown_retries_but_ownership_loss_never_returns_a_result() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            scan_results: Some(Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::from([host::GroupScan::Unknown]),
            ))),
            ..ProcessTestHooks::default()
        });
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("one unknown observer pass was not retried");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::Natural);

        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            scan_results: Some(Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::from([host::GroupScan::OwnershipLost]),
            ))),
            ..ProcessTestHooks::default()
        });
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observer_and_reaper_panics_or_status_mismatch_never_fake_quiescence() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            scan_panics: true,
            ..ProcessTestHooks::default()
        });
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));

        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            reap_termination_override: Some(ProcessTermination::ExitCode(99)),
            ..ProcessTestHooks::default()
        });
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));

        let (request, control) = runner_request(&runner, "true", Duration::from_secs(1)).await;
        let request = request.with_test_hooks(ProcessTestHooks {
            reap_panics: true,
            ..ProcessTestHooks::default()
        });
        assert!(matches!(
            runner.run(request, control).await,
            ProcessOutcome::StartedOwnershipLost { .. }
        ));
    }

    #[test]
    fn escaped_pipe_helper_entry() {
        if std::env::var_os("DSH_PROCESS_ESCAPE_HELPER").is_none() {
            return;
        }
        rustix::process::setsid().unwrap();
        let ready = std::path::PathBuf::from(std::env::var_os("DSH_HELPER_READY").unwrap());
        let stop = std::path::PathBuf::from(std::env::var_os("DSH_HELPER_STOP").unwrap());
        let done = std::path::PathBuf::from(std::env::var_os("DSH_HELPER_DONE").unwrap());
        std::fs::write(&ready, b"ready").unwrap();
        let deadline = StdInstant::now() + Duration::from_secs(8);
        while !stop.exists() && StdInstant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(done, b"done").unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn escaped_session_pipe_holder_hits_the_bounded_drain_window() {
        let runner = tokio::task::spawn_blocking(ProcessRunner::open)
            .await
            .unwrap()
            .unwrap();
        let directory = std::env::temp_dir().join(format!("dsh-escape-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let ready = directory.join("ready");
        let stop = directory.join("stop");
        let done = directory.join("done");
        let executable = std::env::current_exe().unwrap();
        let command = format!(
            "{} --exact tools::process::api_tests::escaped_pipe_helper_entry --nocapture & \
             while [ ! -f {} ]; do :; done",
            shell_quote(&executable),
            shell_quote(&ready)
        );
        let (mut request, control) =
            runner_request(&runner, &command, Duration::from_secs(5)).await;
        request.environment = Arc::from([
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (
                OsString::from("DSH_PROCESS_ESCAPE_HELPER"),
                OsString::from("1"),
            ),
            (
                OsString::from("DSH_HELPER_READY"),
                ready.as_os_str().to_owned(),
            ),
            (
                OsString::from("DSH_HELPER_STOP"),
                stop.as_os_str().to_owned(),
            ),
            (
                OsString::from("DSH_HELPER_DONE"),
                done.as_os_str().to_owned(),
            ),
        ]);
        let started = StdInstant::now();
        let ProcessOutcome::StartedAndQuiescent(report) = runner.run(request, control).await else {
            panic!("escaped pipe holder lost original-group ownership");
        };
        assert_eq!(report.primary(), ProcessPrimaryCause::PipeDrainTimeout);
        assert!(report.flags().pipe_drain_timed_out());
        assert!(started.elapsed() >= PIPE_DRAIN_GRACE);
        assert!(started.elapsed() < Duration::from_secs(3));
        std::fs::write(&stop, b"stop").unwrap();
        wait_for_file(&done).await;
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_runtime_without_io_rejects_before_spawn() {
        let runner = ProcessRunner::open().unwrap();
        let permit = runner.pre_spawn_check(&CancellationToken::new()).unwrap();
        let request = ProcessRequest::new(
            "true".to_owned(),
            std::fs::File::open(".").unwrap().into(),
            Arc::from([]),
            Duration::from_secs(1),
            permit,
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let control = ProcessControl::new(
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
            Instant::now() + Duration::from_secs(4),
        );
        assert!(matches!(
            runtime.block_on(runner.run(request, control)),
            ProcessOutcome::NotStarted {
                cause: ProcessStartFailure::AsyncRuntimeUnavailable,
                ..
            }
        ));
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
