use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ExitStatus},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use pty_process::{Size, blocking};
use rustix::process::{
    Pid, Signal, getpgid, getsid, kill_process, kill_process_group, test_kill_process_group,
};

const MAX_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const TEST_API_KEY: &str = "test-key-for-loopback-only";
const SECRET_WINDOW_BYTES: usize = 64;

fn open_test_pty() -> (blocking::Pty, blocking::Pts) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match blocking::open() {
            Ok(pair) => return pair,
            Err(error) if transient_pty_allocation_error(&error) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("PTY should open within its bounded retry window: {error:?}"),
        }
    }
}

fn transient_pty_allocation_error(error: &pty_process::Error) -> bool {
    match error {
        pty_process::Error::Rustix(error) => transient_pty_errno(error.raw_os_error()),
        pty_process::Error::Io(error) => error.raw_os_error().is_some_and(transient_pty_errno),
    }
}

fn transient_pty_errno(raw: i32) -> bool {
    // Darwin's `ptsname_r` can report a short-lived ENXIO as `-ENXIO` while
    // parallel tests are allocating PTYs. It is the same retryable condition;
    // normalize only the two allocation errors that this harness already
    // treats as transient.
    matches!(raw.unsigned_abs(), code if code == libc::ENXIO as u32 || code == libc::EAGAIN as u32)
}

struct TranscriptState {
    bytes: Vec<u8>,
    overflowed: bool,
    closed: bool,
    rolling: bool,
    secret_window: [u8; SECRET_WINDOW_BYTES],
    secret_window_len: usize,
    secret_window_next: usize,
    secret_seen: bool,
    reader_failure: Option<&'static str>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            overflowed: false,
            closed: false,
            rolling: false,
            secret_window: [0; SECRET_WINDOW_BYTES],
            secret_window_len: 0,
            secret_window_next: 0,
            secret_seen: false,
            reader_failure: None,
        }
    }
}

#[derive(Default)]
struct ReaderControlState {
    paused: bool,
    pause_acknowledged: bool,
    stop: bool,
}

type ReaderControl = Arc<(Mutex<ReaderControlState>, Condvar)>;

pub struct PtyHarness {
    master: Option<blocking::Pty>,
    child: Option<Child>,
    reader: Option<thread::JoinHandle<()>>,
    transcript: Arc<(Mutex<TranscriptState>, Condvar)>,
    reader_control: ReaderControl,
    color: bool,
    initial_terminal_state: String,
    _session_root: TestSessionRoot,
}

pub struct ObservedPtyReader {
    reader: File,
    transcript: Arc<(Mutex<TranscriptState>, Condvar)>,
}

#[derive(Clone, Copy)]
pub enum DisabledTerminalMode {
    EchoControls,
    OutputPostprocess,
    OutputNewlineMapping,
}

pub struct JobControlHarness {
    master: Option<blocking::Pty>,
    shell: Option<Child>,
    reader: Option<thread::JoinHandle<()>>,
    transcript: Arc<(Mutex<TranscriptState>, Condvar)>,
    workspace: PathBuf,
    shell_sid: Pid,
    dsh_pgid: Option<Pid>,
    approved_pgid: Option<Pid>,
    approved_guard: Option<Pid>,
    _session_root: TestSessionRoot,
}

struct PtyLaunch {
    color: bool,
    binary: PathBuf,
}

impl PtyLaunch {
    fn cargo(color: bool) -> Self {
        Self {
            color,
            binary: cargo_test_binary(),
        }
    }

    #[allow(dead_code)] // Used only by the separately compiled release-acceptance binary.
    fn installed(color: bool) -> Self {
        Self {
            color,
            binary: dsh_binary(),
        }
    }
}

#[derive(Clone)]
pub struct TestSessionRoot(Arc<TestSessionDirectory>);

struct TestSessionDirectory(PathBuf);

impl TestSessionRoot {
    pub fn new() -> Self {
        let parent = std::fs::canonicalize(std::env::temp_dir())
            .expect("test temp directory should canonicalize without symlinks");
        Self(Arc::new(TestSessionDirectory(
            parent.join(format!("dsh-pty-sessions-{}", uuid::Uuid::new_v4())),
        )))
    }

    pub fn path(&self) -> &Path {
        &self.0.0
    }
}

impl Drop for TestSessionDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[allow(dead_code)] // Used only by the separately compiled release-acceptance binary.
pub fn dsh_binary() -> PathBuf {
    std::env::var_os("DSH_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dsh")))
}

fn cargo_test_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dsh"))
}

impl PtyHarness {
    pub fn spawn(base_url: &str, workspace: &Path) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            workspace,
            false,
            None,
            None,
            None,
            PtyLaunch::cargo(false),
        )
    }

    pub fn spawn_color(base_url: &str, workspace: &Path) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            workspace,
            false,
            None,
            None,
            None,
            PtyLaunch::cargo(true),
        )
    }

    #[allow(dead_code)] // Used only by the separately compiled release-acceptance binary.
    pub fn spawn_color_with_session_root(
        base_url: &str,
        workspace: &Path,
        session_root: TestSessionRoot,
    ) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            workspace,
            false,
            None,
            Some(session_root),
            None,
            PtyLaunch::installed(true),
        )
    }

    pub fn spawn_rolling(base_url: &str, workspace: &Path) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            workspace,
            true,
            None,
            None,
            None,
            PtyLaunch::cargo(false),
        )
    }

    pub fn spawn_with_disabled_terminal_mode(
        base_url: &str,
        workspace: &Path,
        mode: DisabledTerminalMode,
    ) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            workspace,
            false,
            Some(mode),
            None,
            None,
            PtyLaunch::cargo(false),
        )
    }

    pub fn spawn_resume(
        base_url: &str,
        caller_workspace: &Path,
        session_root: TestSessionRoot,
        session_id: &str,
    ) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            caller_workspace,
            false,
            None,
            Some(session_root),
            Some(session_id),
            PtyLaunch::cargo(false),
        )
    }

    #[allow(dead_code)] // Used only by the separately compiled release-acceptance binary.
    pub fn spawn_resume_color(
        base_url: &str,
        caller_workspace: &Path,
        session_root: TestSessionRoot,
        session_id: &str,
    ) -> Self {
        Self::spawn_with_transcript_mode(
            base_url,
            caller_workspace,
            false,
            None,
            Some(session_root),
            Some(session_id),
            PtyLaunch::installed(true),
        )
    }

    fn spawn_with_transcript_mode(
        base_url: &str,
        workspace: &Path,
        rolling: bool,
        disabled_mode: Option<DisabledTerminalMode>,
        session_root: Option<TestSessionRoot>,
        resume_id: Option<&str>,
        launch: PtyLaunch,
    ) -> Self {
        let PtyLaunch { color, binary } = launch;
        let (master, slave) = open_test_pty();
        let mut termios =
            rustix::termios::tcgetattr(&slave).expect("PTY terminal settings should be readable");
        termios
            .input_modes
            .insert(rustix::termios::InputModes::ICRNL);
        termios.input_modes.remove(
            rustix::termios::InputModes::IGNCR
                | rustix::termios::InputModes::INLCR
                | rustix::termios::InputModes::ISTRIP,
        );
        termios.local_modes.insert(
            rustix::termios::LocalModes::ICANON
                | rustix::termios::LocalModes::ISIG
                | rustix::termios::LocalModes::ECHO
                | rustix::termios::LocalModes::ECHOCTL,
        );
        termios
            .local_modes
            .remove(rustix::termios::LocalModes::EXTPROC);
        termios
            .output_modes
            .insert(rustix::termios::OutputModes::OPOST | rustix::termios::OutputModes::ONLCR);
        #[cfg(target_os = "linux")]
        {
            termios
                .input_modes
                .remove(rustix::termios::InputModes::IUCLC);
            termios
                .local_modes
                .remove(rustix::termios::LocalModes::XCASE);
            termios
                .output_modes
                .remove(rustix::termios::OutputModes::OLCUC);
            termios.line_discipline = 0;
        }
        termios.special_codes[rustix::termios::SpecialCodeIndex::VINTR] = 0x03;
        termios.special_codes[rustix::termios::SpecialCodeIndex::VEOF] = 0x04;
        termios.special_codes[rustix::termios::SpecialCodeIndex::VSUSP] = 0x1a;
        termios.special_codes[rustix::termios::SpecialCodeIndex::VQUIT] = 0x1c;
        #[cfg(target_os = "macos")]
        let disabled = 0xff;
        #[cfg(target_os = "linux")]
        let disabled = 0x00;
        termios.special_codes[rustix::termios::SpecialCodeIndex::VEOL] = disabled;
        termios.special_codes[rustix::termios::SpecialCodeIndex::VEOL2] = disabled;
        if let Some(mode) = disabled_mode {
            match mode {
                DisabledTerminalMode::EchoControls => {
                    termios
                        .local_modes
                        .remove(rustix::termios::LocalModes::ECHOCTL);
                }
                DisabledTerminalMode::OutputPostprocess => {
                    termios
                        .output_modes
                        .remove(rustix::termios::OutputModes::OPOST);
                }
                DisabledTerminalMode::OutputNewlineMapping => {
                    termios
                        .output_modes
                        .remove(rustix::termios::OutputModes::ONLCR);
                }
            }
        }
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &termios)
            .expect("PTY terminal settings should initialize deterministically");
        let initial_terminal_state = format!(
            "{:?}",
            rustix::termios::tcgetattr(&master)
                .expect("initialized PTY terminal settings should be readable")
        );
        master
            .resize(Size::new(24, 120))
            .expect("PTY should resize");
        let reader_fd = rustix::io::dup(&master).expect("PTY master should duplicate");
        let transcript = Arc::new((
            Mutex::new(TranscriptState {
                rolling,
                ..TranscriptState::default()
            }),
            Condvar::new(),
        ));
        let reader_control = Arc::new((Mutex::new(ReaderControlState::default()), Condvar::new()));
        let reader_state = Arc::clone(&transcript);
        let reader_commands = Arc::clone(&reader_control);
        let reader = thread::spawn(move || {
            read_controlled_transcript(File::from(reader_fd), &reader_state, &reader_commands)
        });
        let session_root = session_root.unwrap_or_else(TestSessionRoot::new);
        let command = blocking::Command::new(binary);
        let command = if let Some(session_id) = resume_id {
            if color {
                command.args(["--resume", session_id])
            } else {
                command.args(["--resume", session_id, "--no-color"])
            }
        } else if color {
            command.args([
                "--model",
                "deepseek-chat",
                "--workspace",
                workspace
                    .to_str()
                    .expect("test workspace path should be Unicode"),
            ])
        } else {
            command.args([
                "--model",
                "deepseek-chat",
                "--workspace",
                workspace
                    .to_str()
                    .expect("test workspace path should be Unicode"),
                "--no-color",
            ])
        };
        let command = command
            .current_dir(workspace)
            .env_clear()
            .env("DEEPSEEK_BASE_URL", base_url)
            .env("DEEPSEEK_API_KEY", TEST_API_KEY)
            .env("DSH_SESSION_ROOT", session_root.path())
            .env("HOME", workspace)
            .env("PATH", "/usr/bin:/bin");
        let command = if color {
            command.env("TERM", "xterm-256color")
        } else {
            command.env("TERM", "dumb").env("NO_COLOR", "1")
        };
        let child = command.spawn(slave).expect("dsh should spawn on the PTY");
        Self {
            master: Some(master),
            child: Some(child),
            reader: Some(reader),
            transcript,
            reader_control,
            color,
            initial_terminal_state,
            _session_root: session_root,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let mut master = self.master.as_ref().expect("PTY master should exist");
        master.write_all(bytes).expect("PTY input should write");
        master.flush().expect("PTY input should flush");
    }

    pub fn signal(&mut self, signal: Signal) {
        let pid = Pid::from_child(self.child.as_ref().expect("PTY child should exist"));
        kill_process(pid, signal).expect("owned PTY child should accept the signal");
    }

    pub fn resize(&mut self, rows: u16, columns: u16) {
        self.master
            .as_ref()
            .expect("PTY master should exist")
            .resize(Size::new(rows, columns))
            .expect("PTY should resize");
    }

    pub fn pause_reading(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (state, changed) = &*self.reader_control;
        let mut state = state.lock().expect("reader control mutex should lock");
        state.paused = true;
        changed.notify_all();
        while !state.pause_acknowledged {
            let now = Instant::now();
            assert!(
                now < deadline,
                "PTY reader did not pause before its deadline"
            );
            let (next, timeout) = changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .expect("reader control wait should succeed");
            state = next;
            assert!(!timeout.timed_out(), "PTY reader did not acknowledge pause");
        }
    }

    pub fn resume_reading(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (state, changed) = &*self.reader_control;
        let mut state = state.lock().expect("reader control mutex should lock");
        state.paused = false;
        changed.notify_all();
        while state.pause_acknowledged {
            let now = Instant::now();
            assert!(
                now < deadline,
                "PTY reader did not resume before its deadline"
            );
            let (next, timeout) = changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .expect("reader control wait should succeed");
            state = next;
            assert!(
                !timeout.timed_out(),
                "PTY reader did not acknowledge resume"
            );
        }
    }

    pub fn duplicate_writer(&self) -> File {
        File::from(
            rustix::io::dup(self.master.as_ref().expect("PTY master should exist"))
                .expect("PTY writer should duplicate"),
        )
    }

    pub fn duplicate_observed_reader(&self) -> ObservedPtyReader {
        ObservedPtyReader {
            reader: File::from(
                rustix::io::dup(self.master.as_ref().expect("PTY master should exist"))
                    .expect("PTY reader should duplicate"),
            ),
            transcript: Arc::clone(&self.transcript),
        }
    }

    pub fn wait_until_stopped(&mut self) {
        use rustix::process::{WaitOptions, waitpid};

        let pid = Pid::from_child(self.child.as_ref().expect("PTY child should exist"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match waitpid(Some(pid), WaitOptions::NOHANG | WaitOptions::UNTRACED) {
                Ok(Some((_, status))) if status.stopped() => return,
                Ok(Some((_, status))) => {
                    panic!("PTY child changed state before stopping: {status:?}")
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => panic!("PTY child did not stop before its deadline"),
                Err(error) => panic!("PTY child stop status should be observable: {error}"),
            }
        }
    }

    pub fn terminal_state(&self) -> String {
        let state =
            rustix::termios::tcgetattr(self.master.as_ref().expect("PTY master should exist"))
                .expect("PTY terminal state should be readable");
        format!("{state:?}")
    }

    pub fn expect(&mut self, marker: &[u8]) {
        self.expect_occurrences(marker, 1);
    }

    pub fn expect_occurrences(&mut self, marker: &[u8], expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (state, changed) = &*self.transcript;
        let mut state = state.lock().expect("transcript mutex should lock");
        loop {
            if state.overflowed && !state.rolling {
                let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
                drop(state);
                panic!("PTY transcript exceeded its 1 MiB cap: {transcript}");
            }
            if state
                .bytes
                .windows(marker.len())
                .filter(|window| *window == marker)
                .count()
                >= expected
            {
                return;
            }
            if state.closed {
                let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
                drop(state);
                let status = self
                    .child
                    .as_mut()
                    .and_then(|child| child.try_wait().ok().flatten());
                panic!(
                    "PTY closed before marker appeared; status: {status:?}; transcript: {transcript}"
                );
            }
            let now = Instant::now();
            if now >= deadline {
                let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
                drop(state);
                panic!("timed out waiting for PTY marker; transcript: {transcript}");
            }
            let (next, timeout) = changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .expect("transcript wait should succeed");
            state = next;
            if timeout.timed_out() {
                let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
                drop(state);
                panic!("timed out waiting for PTY marker; transcript: {transcript}");
            }
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.transcript
            .0
            .lock()
            .expect("transcript mutex should lock")
            .bytes
            .clone()
    }

    pub fn approval_ready(&mut self) {
        self.approval_ready_occurrence(1);
    }

    pub fn approval_ready_occurrence(&mut self, expected: usize) {
        let title = if self.color {
            b"Approval required".as_slice()
        } else {
            b"[approval required]".as_slice()
        };
        self.expect_occurrences(title, expected);
        self.expect_occurrences(b"Enter confirm", expected);
        wait_for_selector_mode(self.master.as_ref().expect("PTY master should exist"));
    }

    pub fn exit_cleanly(mut self) -> (ExitStatus, Vec<u8>) {
        self.write(b"/exit\r");
        self.wait_for_exit(Duration::from_secs(5))
    }

    pub fn wait_for_exit(mut self, timeout: Duration) -> (ExitStatus, Vec<u8>) {
        let status = wait_child(
            self.child.as_mut().expect("PTY child should exist"),
            timeout,
        )
        .expect("dsh should exit within the bounded deadline");
        resume_reader_for_exit(&self.reader_control);
        wait_for_transcript_close(&self.transcript, Duration::from_secs(2));
        assert_eq!(
            self.terminal_state(),
            self.initial_terminal_state,
            "dsh must restore the exact PTY state before every exit"
        );
        self.master.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("PTY reader should join");
        }
        let transcript = self.snapshot();
        assert_api_key_absent(&self.transcript);
        self.child.take();
        (status, transcript)
    }
}

impl ObservedPtyReader {
    pub fn read_with_timeout(
        &mut self,
        bytes: &mut [u8],
        timeout: Duration,
    ) -> std::io::Result<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for PTY progress",
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let timeout = rustix::event::Timespec {
                tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(remaining.subsec_nanos()),
            };
            let mut fds = [rustix::event::PollFd::new(
                &self.reader,
                rustix::event::PollFlags::IN,
            )];
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(std::io::Error::from_raw_os_error(error.raw_os_error())),
            }
        }

        let count = self.reader.read(bytes)?;
        if count > 0 {
            retain_transcript_bytes(&self.transcript, &bytes[..count]);
        }
        Ok(count)
    }
}

impl JobControlHarness {
    pub const SHELL_PROMPT: &'static [u8] = b"JC_BASH> ";

    pub fn spawn(base_url: &str, workspace: &Path) -> Self {
        let (master, slave) = open_test_pty();
        master
            .resize(Size::new(24, 120))
            .expect("job-control PTY should resize");
        let reader_fd = rustix::io::dup(&master).expect("PTY master should duplicate");
        let transcript = Arc::new((Mutex::new(TranscriptState::default()), Condvar::new()));
        let reader_state = Arc::clone(&transcript);
        let reader = thread::spawn(move || read_transcript(File::from(reader_fd), &reader_state));
        let session_root = TestSessionRoot::new();
        let shell = blocking::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-i"])
            .current_dir(workspace)
            .env_clear()
            .env("PS1", String::from_utf8_lossy(Self::SHELL_PROMPT).as_ref())
            .env("PROMPT_COMMAND", "")
            .env("HISTFILE", "/dev/null")
            .env("BASH_SILENCE_DEPRECATION_WARNING", "1")
            .env("DSH_TEST_BIN", cargo_test_binary())
            .env("DSH_TEST_WORKSPACE", workspace)
            .env("DEEPSEEK_BASE_URL", base_url)
            .env("DEEPSEEK_API_KEY", TEST_API_KEY)
            .env("DSH_SESSION_ROOT", session_root.path())
            .env("HOME", workspace)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .spawn(slave)
            .expect("interactive Bash should spawn on the PTY");
        let shell_sid = Pid::from_child(&shell);
        assert_eq!(getpgid(Some(shell_sid)).ok(), Some(shell_sid));
        assert_eq!(getsid(Some(shell_sid)).ok(), Some(shell_sid));
        Self {
            master: Some(master),
            shell: Some(shell),
            reader: Some(reader),
            transcript,
            workspace: workspace.to_owned(),
            shell_sid,
            dsh_pgid: None,
            approved_pgid: None,
            approved_guard: None,
            _session_root: session_root,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let mut master = self.master.as_ref().expect("PTY master should exist");
        master.write_all(bytes).expect("PTY input should write");
        master.flush().expect("PTY input should flush");
    }

    pub fn expect(&mut self, marker: &[u8]) {
        self.expect_occurrences(marker, 1);
    }

    pub fn expect_occurrences(&mut self, marker: &[u8], expected: usize) {
        expect_transcript(&self.transcript, self.shell.as_mut(), marker, expected);
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.transcript
            .0
            .lock()
            .expect("transcript mutex should lock")
            .bytes
            .clone()
    }

    pub fn checkpoint(&self) -> usize {
        self.transcript
            .0
            .lock()
            .expect("transcript mutex should lock")
            .bytes
            .len()
    }

    pub fn expect_after(&mut self, offset: usize, marker: &[u8]) {
        expect_transcript_after(&self.transcript, self.shell.as_mut(), offset, marker);
    }

    pub fn start_dsh_job(&mut self) -> Pid {
        self.expect(Self::SHELL_PROMPT);
        self.write(
            b"/bin/bash --noprofile --norc -c 'printf \"%s\\n\" \"$$\" > \"$DSH_TEST_WORKSPACE/dsh-job.pid\"; kill -STOP \"$$\"; stty sane; : > \"$DSH_TEST_WORKSPACE/dsh-terminal-ready\"; while [ ! -e \"$DSH_TEST_WORKSPACE/dsh-foreground-ready\" ]; do /bin/sleep 0.01; done; exec \"$DSH_TEST_BIN\" --model deepseek-chat --workspace \"$DSH_TEST_WORKSPACE\" --no-color'\r",
        );
        self.expect_occurrences(Self::SHELL_PROMPT, 2);
        let pid = read_pid_file(&self.workspace.join("dsh-job.pid"));
        assert_eq!(getpgid(Some(pid)).ok(), Some(pid));
        assert_eq!(getsid(Some(pid)).ok(), Some(self.shell_sid));
        self.dsh_pgid = Some(pid);
        self.write(b"fg %1\r");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let foreground = self.foreground_group() == pid;
            let terminal_ready = self.workspace.join("dsh-terminal-ready").is_file();
            if foreground && terminal_ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Bash did not restore the foreground terminal before dsh startup; \
                 foreground={foreground}, terminal_ready={terminal_ready}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(self.workspace.join("dsh-foreground-ready"), b"")
            .expect("foreground startup gate should open");
        self.expect(b"dsh > ");
        assert_eq!(self.foreground_group(), pid);
        pid
    }

    pub fn approval_ready(&mut self) {
        self.expect(b"[approval required]");
        self.expect(b"Enter confirm");
        wait_for_selector_mode(self.master.as_ref().expect("PTY master should exist"));
    }

    pub fn remember_approved_group(&mut self) -> Pid {
        let pid = read_pid_file(&self.workspace.join("approved.pid"));
        let guard = read_pid_file(&self.workspace.join("approved.guard.pid"));
        assert_eq!(getpgid(Some(pid)).ok(), Some(pid));
        assert_eq!(getsid(Some(pid)).ok(), Some(pid));
        assert_eq!(getpgid(Some(guard)).ok(), Some(pid));
        assert_eq!(getsid(Some(guard)).ok(), Some(pid));
        self.approved_pgid = Some(pid);
        self.approved_guard = Some(guard);
        pid
    }

    pub fn foreground_group(&self) -> Pid {
        rustix::termios::tcgetpgrp(self.master.as_ref().expect("PTY master should exist"))
            .expect("PTY foreground group should be readable")
    }

    pub fn shell_group(&self) -> Pid {
        self.shell_sid
    }

    pub fn approved_group_is_gone(&self, timeout: Duration) -> bool {
        self.approved_pgid
            .is_some_and(|group| wait_group_gone(group, timeout))
    }

    pub fn finish_shell(mut self, timeout: Duration) -> (ExitStatus, Vec<u8>) {
        assert_eq!(self.foreground_group(), self.shell_sid);
        self.write(b"exit\r");
        let status = wait_child(
            self.shell.as_mut().expect("Bash child should exist"),
            timeout,
        )
        .expect("Bash should exit within the bounded deadline");
        self.master.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("PTY reader should join");
        }
        let transcript = self.snapshot();
        assert_api_key_absent(&self.transcript);
        self.shell.take();
        (status, transcript)
    }
}

fn assert_api_key_absent(transcript: &Arc<(Mutex<TranscriptState>, Condvar)>) {
    let state = transcript
        .0
        .lock()
        .expect("transcript mutex should lock for the secret check");
    assert_eq!(
        state.reader_failure, None,
        "the PTY transcript reader must not fail unexpectedly"
    );
    assert!(
        !state.secret_seen,
        "the fake API key must never reach the PTY transcript"
    );
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            stop_reader(&self.reader_control);
            self.master.take();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
            return;
        }
        if let Some(master) = self.master.as_ref() {
            let mut master = master;
            let _ = master.write_all(&[0x03, 0x04]);
        }
        let pid = Pid::from_child(child);
        if wait_child(child, Duration::from_millis(500)).is_none() {
            let _ = kill_process_group(pid, Signal::CONT);
            let _ = kill_process_group(pid, Signal::TERM);
        }
        if wait_child(child, Duration::from_millis(500)).is_none() {
            let _ = kill_process_group(pid, Signal::KILL);
        }
        let _ = child.wait();
        stop_reader(&self.reader_control);
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for JobControlHarness {
    fn drop(&mut self) {
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        if matches!(shell.try_wait(), Ok(Some(_))) {
            self.master.take();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
            return;
        }

        let _ = std::fs::write(self.workspace.join("cleanup-release"), b"");
        let _ = std::fs::write(self.workspace.join("guard.cancel"), b"");
        if let Some(dsh) = self.dsh_pgid {
            terminate_owned_group(dsh, dsh, self.shell_sid, Duration::from_millis(1_500));
        }
        if let Some(approved) = self.approved_pgid {
            terminate_owned_group(
                self.approved_guard.unwrap_or(approved),
                approved,
                approved,
                Duration::from_millis(500),
            );
        }
        terminate_owned_group(
            self.shell_sid,
            self.shell_sid,
            self.shell_sid,
            Duration::from_millis(500),
        );
        let _ = shell.wait();
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn expect_transcript(
    transcript: &Arc<(Mutex<TranscriptState>, Condvar)>,
    mut child: Option<&mut Child>,
    marker: &[u8],
    expected: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (state, changed) = &**transcript;
    let mut state = state.lock().expect("transcript mutex should lock");
    loop {
        if state.overflowed && !state.rolling {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            panic!("PTY transcript exceeded its 1 MiB cap: {transcript}");
        }
        if state
            .bytes
            .windows(marker.len())
            .filter(|window| *window == marker)
            .count()
            >= expected
        {
            return;
        }
        if state.closed {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            let status = child
                .as_deref_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            panic!(
                "PTY closed before marker appeared; status: {status:?}; transcript: {transcript}"
            );
        }
        let now = Instant::now();
        if now >= deadline {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            panic!("timed out waiting for PTY marker; transcript: {transcript}");
        }
        let (next, timeout) = changed
            .wait_timeout(state, deadline.saturating_duration_since(now))
            .expect("transcript wait should succeed");
        state = next;
        if timeout.timed_out() {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            panic!("timed out waiting for PTY marker; transcript: {transcript}");
        }
    }
}

fn expect_transcript_after(
    transcript: &Arc<(Mutex<TranscriptState>, Condvar)>,
    mut child: Option<&mut Child>,
    offset: usize,
    marker: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (state, changed) = &**transcript;
    let mut state = state.lock().expect("transcript mutex should lock");
    loop {
        if state.overflowed && !state.rolling {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            panic!("PTY transcript exceeded its 1 MiB cap: {transcript}");
        }
        if state
            .bytes
            .get(offset..)
            .unwrap_or_default()
            .windows(marker.len())
            .any(|window| window == marker)
        {
            return;
        }
        if state.closed {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            let status = child
                .as_deref_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            panic!(
                "PTY closed before marker appeared; status: {status:?}; transcript: {transcript}"
            );
        }
        let now = Instant::now();
        if now >= deadline {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            panic!("timed out waiting for PTY marker; transcript: {transcript}");
        }
        let (next, timeout) = changed
            .wait_timeout(state, deadline.saturating_duration_since(now))
            .expect("transcript wait should succeed");
        state = next;
        if timeout.timed_out() {
            let transcript = String::from_utf8_lossy(&state.bytes).into_owned();
            drop(state);
            panic!("timed out waiting for PTY marker; transcript: {transcript}");
        }
    }
}

fn read_pid_file(path: &Path) -> Pid {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            assert!(value.len() <= 32, "recorded PID must stay bounded");
            if let Ok(raw) = value.trim().parse::<i32>() {
                if let Some(pid) = Pid::from_raw(raw) {
                    return pid;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "PID file did not appear: {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_selector_mode(master: &blocking::Pty) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let termios = rustix::termios::tcgetattr(master)
            .expect("PTY terminal settings should remain readable");
        let canonical = termios
            .local_modes
            .contains(rustix::termios::LocalModes::ICANON);
        let echo = termios
            .local_modes
            .contains(rustix::termios::LocalModes::ECHO);
        if !canonical && !echo {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "approval selector did not enter cbreak/no-echo mode"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn owned_group(anchor: Pid, group: Pid, expected_session: Pid) -> bool {
    getpgid(Some(anchor)).is_ok_and(|actual| actual == group)
        && getsid(Some(anchor)).is_ok_and(|actual| actual == expected_session)
}

fn wait_group_gone(group: Pid, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if test_kill_process_group(group).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_owned_group(anchor: Pid, group: Pid, expected_session: Pid, grace: Duration) {
    if !owned_group(anchor, group, expected_session) {
        return;
    }
    let _ = kill_process_group(group, Signal::CONT);
    let _ = kill_process_group(group, Signal::TERM);
    if wait_group_gone(group, grace) || !owned_group(anchor, group, expected_session) {
        return;
    }
    let _ = kill_process_group(group, Signal::KILL);
    let _ = wait_group_gone(group, Duration::from_millis(500));
}

fn wait_child(child: &mut Child, duration: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + duration;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => return None,
        }
    }
}

fn stop_reader(control: &ReaderControl) {
    let (state, changed) = &**control;
    let mut state = state.lock().expect("reader control mutex should lock");
    state.stop = true;
    state.paused = false;
    changed.notify_all();
}

fn resume_reader_for_exit(control: &ReaderControl) {
    let (state, changed) = &**control;
    let mut state = state.lock().expect("reader control mutex should lock");
    state.paused = false;
    changed.notify_all();
}

fn wait_for_transcript_close(
    transcript: &Arc<(Mutex<TranscriptState>, Condvar)>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let (state, changed) = &**transcript;
    let mut state = state.lock().expect("transcript mutex should lock");
    while !state.closed {
        let now = Instant::now();
        assert!(
            now < deadline,
            "PTY reader did not drain through the child-owned close"
        );
        let (next, waited) = changed
            .wait_timeout(state, deadline.saturating_duration_since(now))
            .expect("transcript close wait should succeed");
        state = next;
        assert!(
            !waited.timed_out() || state.closed,
            "PTY reader did not drain through the child-owned close"
        );
    }
}

fn read_controlled_transcript(
    mut reader: File,
    shared: &Arc<(Mutex<TranscriptState>, Condvar)>,
    control: &ReaderControl,
) {
    let mut scratch = [0_u8; 8 * 1024];
    loop {
        {
            let (state, changed) = &**control;
            let mut state = state.lock().expect("reader control mutex should lock");
            while state.paused && !state.stop {
                state.pause_acknowledged = true;
                changed.notify_all();
                state = changed
                    .wait(state)
                    .expect("reader control wait should succeed");
            }
            if state.stop {
                break;
            }
            state.pause_acknowledged = false;
            changed.notify_all();
        }

        let ready = {
            let mut fds = [rustix::event::PollFd::new(
                &reader,
                rustix::event::PollFlags::IN,
            )];
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            rustix::event::poll(&mut fds, Some(&timeout))
        };
        match ready {
            Ok(0) => continue,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => {
                record_reader_failure(shared, "poll");
                break;
            }
        }
        match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(count) => retain_transcript_bytes(shared, &scratch[..count]),
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(_) => {
                record_reader_failure(shared, "read");
                break;
            }
        }
    }
    close_transcript(shared);
}

fn read_transcript(mut reader: File, shared: &Arc<(Mutex<TranscriptState>, Condvar)>) {
    let mut scratch = [0_u8; 8 * 1024];
    loop {
        match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(count) => retain_transcript_bytes(shared, &scratch[..count]),
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(_) => {
                record_reader_failure(shared, "read");
                break;
            }
        }
    }
    close_transcript(shared);
}

fn retain_transcript_bytes(shared: &Arc<(Mutex<TranscriptState>, Condvar)>, bytes: &[u8]) {
    let (state, changed) = &**shared;
    let mut state = state.lock().expect("transcript mutex should lock");
    observe_secret_bytes(&mut state, bytes);
    if state.rolling && state.bytes.len().saturating_add(bytes.len()) > MAX_TRANSCRIPT_BYTES {
        state.overflowed = true;
        if bytes.len() >= MAX_TRANSCRIPT_BYTES {
            state.bytes.clear();
            state
                .bytes
                .extend_from_slice(&bytes[bytes.len() - MAX_TRANSCRIPT_BYTES..]);
        } else {
            let keep = (MAX_TRANSCRIPT_BYTES / 2)
                .min(state.bytes.len())
                .min(MAX_TRANSCRIPT_BYTES - bytes.len());
            let start = state.bytes.len() - keep;
            state.bytes.copy_within(start.., 0);
            state.bytes.truncate(keep);
            state.bytes.extend_from_slice(bytes);
        }
    } else {
        let remaining = MAX_TRANSCRIPT_BYTES.saturating_sub(state.bytes.len());
        let retained = bytes.len().min(remaining);
        state.bytes.extend_from_slice(&bytes[..retained]);
        state.overflowed |= retained < bytes.len();
    }
    changed.notify_all();
}

fn observe_secret_bytes(state: &mut TranscriptState, bytes: &[u8]) {
    let key = TEST_API_KEY.as_bytes();
    debug_assert!(key.len() <= SECRET_WINDOW_BYTES);
    for byte in bytes {
        state.secret_window[state.secret_window_next] = *byte;
        state.secret_window_next = (state.secret_window_next + 1) % SECRET_WINDOW_BYTES;
        state.secret_window_len = (state.secret_window_len + 1).min(SECRET_WINDOW_BYTES);
        if state.secret_window_len < key.len() {
            continue;
        }
        let start =
            (state.secret_window_next + SECRET_WINDOW_BYTES - key.len()) % SECRET_WINDOW_BYTES;
        if key.iter().enumerate().all(|(offset, expected)| {
            state.secret_window[(start + offset) % SECRET_WINDOW_BYTES] == *expected
        }) {
            state.secret_seen = true;
        }
    }
}

fn close_transcript(shared: &Arc<(Mutex<TranscriptState>, Condvar)>) {
    let (state, changed) = &**shared;
    let mut state = state.lock().expect("transcript mutex should lock");
    state.closed = true;
    changed.notify_all();
}

fn record_reader_failure(shared: &Arc<(Mutex<TranscriptState>, Condvar)>, operation: &'static str) {
    let (state, changed) = &**shared;
    let mut state = state.lock().expect("transcript mutex should lock");
    state.reader_failure.get_or_insert(operation);
    changed.notify_all();
}

#[test]
fn fake_key_scanner_detects_a_value_split_across_reader_chunks() {
    let (reader, mut writer) =
        std::os::unix::net::UnixStream::pair().expect("test socket pair should open");
    let transcript = Arc::new((Mutex::new(TranscriptState::default()), Condvar::new()));
    let mut reader = ObservedPtyReader {
        reader: File::from(std::os::fd::OwnedFd::from(reader)),
        transcript: Arc::clone(&transcript),
    };
    let mut scratch = [0_u8; 64];

    writer
        .write_all(b"prefix test-key-for-")
        .expect("first test fragment should write");
    assert!(
        reader
            .read_with_timeout(&mut scratch, Duration::from_secs(1))
            .expect("first test fragment should read")
            > 0
    );
    assert!(
        !transcript
            .0
            .lock()
            .expect("transcript mutex should lock")
            .secret_seen
    );

    writer
        .write_all(b"loopback-only suffix")
        .expect("second test fragment should write");
    assert!(
        reader
            .read_with_timeout(&mut scratch, Duration::from_secs(1))
            .expect("second test fragment should read")
            > 0
    );
    assert!(
        transcript
            .0
            .lock()
            .expect("transcript mutex should lock")
            .secret_seen
    );
}

#[test]
fn transient_pty_errors_accept_darwins_signed_errno_form() {
    assert!(transient_pty_errno(libc::ENXIO));
    assert!(transient_pty_errno(-libc::ENXIO));
    assert!(transient_pty_errno(libc::EAGAIN));
    assert!(transient_pty_errno(-libc::EAGAIN));
    assert!(!transient_pty_errno(libc::EIO));
}
