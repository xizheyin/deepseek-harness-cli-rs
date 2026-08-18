use std::{
    ffi::CStr,
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use rustix::{
    fs::{FileType, Mode, OFlags, fstat},
    termios::{
        InputModes, LocalModes, OptionalActions, OutputModes, QueueSelector, SpecialCodeIndex,
        Termios, isatty, tcflush, tcgetattr, tcgetpgrp, tcgetsid, tcgetwinsize, tcsetattr,
    },
};
use thiserror::Error;
use tokio::io::{Interest, unix::AsyncFd};

pub(super) const TERMINAL_READ_BYTES: usize = 8 * 1024;
#[cfg(any(target_os = "macos", test))]
const MIN_MACOS_CANONICAL_BYTES: i64 = 1_001;
#[cfg(any(target_os = "linux", test))]
const LINUX_N_TTY: u8 = 0;
const TTY_PATH_BYTES: usize = 4 * 1024;

struct TtyPath {
    bytes: [u8; TTY_PATH_BYTES],
    length_with_nul: usize,
}

impl TtyPath {
    fn as_c_str(&self) -> Result<&CStr, TerminalError> {
        CStr::from_bytes_with_nul(&self.bytes[..self.length_with_nul])
            .map_err(|_| TerminalError::Unsupported)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum TerminalError {
    #[error("CLI_TERMINAL_UNAVAILABLE")]
    Unavailable,
    #[error("CLI_TERMINAL_UNSUPPORTED")]
    Unsupported,
}

pub(super) struct OpenTerminal {
    input: OwnedFd,
    output: OwnedFd,
}

impl OpenTerminal {
    pub(super) fn open_and_validate() -> Result<Self, TerminalError> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let stderr = io::stderr();
        let input = reopen_terminal(stdin.as_fd(), OFlags::RDONLY)?;
        let output = reopen_terminal(stdout.as_fd(), OFlags::WRONLY)?;
        validate_same_terminal_device(input.as_fd(), output.as_fd())?;
        validate_same_terminal_device(input.as_fd(), stderr.as_fd())?;
        validate_descriptors(input.as_fd(), output.as_fd())?;
        Ok(Self { input, output })
    }

    /// Tokio requires an active runtime with its I/O driver enabled, so
    /// registration is deliberately separate from the synchronous open.
    pub(super) fn register(self) -> Result<AsyncTerminal, TerminalError> {
        let input = AsyncFd::with_interest(self.input, Interest::READABLE)
            .map_err(|_| TerminalError::Unsupported)?;
        let output = AsyncFd::with_interest(self.output, Interest::WRITABLE)
            .map_err(|_| TerminalError::Unsupported)?;
        Ok(AsyncTerminal { input, output })
    }
}

fn reopen_terminal(inherited: BorrowedFd<'_>, access: OFlags) -> Result<OwnedFd, TerminalError> {
    let tty_path = platform::tty_path(inherited)?;
    let reopened = rustix::fs::open(
        tty_path.as_c_str()?,
        access | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| TerminalError::Unavailable)?;
    validate_same_terminal_device(inherited, reopened.as_fd())?;
    Ok(reopened)
}

fn validate_same_terminal_device(
    inherited_stdout: BorrowedFd<'_>,
    reopened_output: BorrowedFd<'_>,
) -> Result<(), TerminalError> {
    let inherited = fstat(inherited_stdout).map_err(|_| TerminalError::Unsupported)?;
    let reopened = fstat(reopened_output).map_err(|_| TerminalError::Unsupported)?;
    if !FileType::from_raw_mode(inherited.st_mode).is_char_device()
        || !FileType::from_raw_mode(reopened.st_mode).is_char_device()
        || inherited.st_dev != reopened.st_dev
        || inherited.st_ino != reopened.st_ino
        || inherited.st_rdev != reopened.st_rdev
    {
        return Err(TerminalError::Unsupported);
    }
    Ok(())
}

pub(super) struct AsyncTerminal {
    input: AsyncFd<OwnedFd>,
    output: AsyncFd<OwnedFd>,
}

impl AsyncTerminal {
    pub(super) fn revalidate(&self) -> Result<(), TerminalError> {
        validate_descriptors(self.input.get_ref().as_fd(), self.output.get_ref().as_fd())
    }

    pub(super) fn flush_input(&self) -> Result<(), TerminalError> {
        tcflush(self.input.get_ref(), QueueSelector::IFlush).map_err(|_| TerminalError::Unsupported)
    }

    /// Window size changes presentation only. Some otherwise usable terminals
    /// report zero or reject this optional ioctl, so callers must fall back to
    /// the compact layout rather than failing an approval.
    pub(super) fn columns(&self) -> Option<u16> {
        tcgetwinsize(self.output.get_ref())
            .ok()
            .map(|size| size.ws_col)
            .filter(|columns| *columns != 0)
    }

    pub(super) fn enter_approval_mode(&self) -> Result<ApprovalTerminalMode<'_>, TerminalError> {
        self.revalidate()?;
        let original = tcgetattr(self.input.get_ref()).map_err(|_| TerminalError::Unsupported)?;
        let selector = selector_termios(&original);
        // Input has already been flushed by the approval fence. `Now` avoids
        // TCSAFLUSH's implicit output drain, which could block an async runtime
        // thread forever when the terminal stops consuming output.
        tcsetattr(self.input.get_ref(), OptionalActions::Now, &selector)
            .map_err(|_| TerminalError::Unsupported)?;
        Ok(ApprovalTerminalMode {
            terminal: self,
            original: Some(original),
        })
    }

    pub(super) fn is_foreground(&self) -> Result<bool, TerminalError> {
        let expected_session =
            rustix::process::getsid(None).map_err(|_| TerminalError::Unsupported)?;
        let expected_group = rustix::process::getpgrp();
        for descriptor in [self.input.get_ref().as_fd(), self.output.get_ref().as_fd()] {
            if tcgetsid(descriptor).map_err(|_| TerminalError::Unsupported)? != expected_session
                || tcgetpgrp(descriptor).map_err(|_| TerminalError::Unsupported)? != expected_group
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) async fn read_once(
        &self,
        scratch: &mut [u8; TERMINAL_READ_BYTES],
    ) -> io::Result<usize> {
        loop {
            let mut ready = self.input.readable().await?;
            match ready.try_io(|registered| {
                rustix::io::read(registered.get_ref(), &mut scratch[..]).map_err(io::Error::from)
            }) {
                Ok(result) => return normalize_read(result),
                Err(_) => continue,
            }
        }
    }

    /// Writes at most one kernel chunk. The dispatcher owns write-all,
    /// fairness, cancellation, and the absolute frame deadline.
    pub(super) async fn write_once(&self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.output.writable().await?;
            match ready.try_io(|registered| {
                rustix::io::write(registered.get_ref(), bytes).map_err(io::Error::from)
            }) {
                Ok(Ok(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "terminal write made no progress",
                    ));
                }
                Ok(result) => return result,
                Err(_) => continue,
            }
        }
    }
}

/// Owns the one temporary terminal-mode change used by the approval selector.
/// Normal control flow calls `restore`; Drop is only a panic/unwind backstop.
pub(super) struct ApprovalTerminalMode<'a> {
    terminal: &'a AsyncTerminal,
    original: Option<Termios>,
}

impl ApprovalTerminalMode<'_> {
    pub(super) fn restore(mut self) -> Result<(), TerminalError> {
        let original = self.original.as_ref().ok_or(TerminalError::Unsupported)?;
        tcsetattr(
            self.terminal.input.get_ref(),
            OptionalActions::Now,
            original,
        )
        .map_err(|_| TerminalError::Unsupported)?;
        self.original = None;
        // Restore canonical mode first, then discard bytes typed against the
        // old selector. A flush failure is reported, but it can no longer leave
        // the terminal in cbreak/no-echo mode.
        self.terminal.flush_input()?;
        self.terminal.revalidate()
    }
}

impl Drop for ApprovalTerminalMode<'_> {
    fn drop(&mut self) {
        let Some(original) = self.original.as_ref() else {
            return;
        };
        let _ = tcsetattr(
            self.terminal.input.get_ref(),
            OptionalActions::Now,
            original,
        );
    }
}

fn selector_termios(original: &Termios) -> Termios {
    let mut selector = original.clone();
    selector
        .local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ECHONL);
    selector.local_modes.insert(LocalModes::ISIG);
    selector.special_codes[SpecialCodeIndex::VMIN] = 1;
    selector.special_codes[SpecialCodeIndex::VTIME] = 0;
    selector
}

fn normalize_read(result: io::Result<usize>) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    if result.as_ref().err().and_then(io::Error::raw_os_error) == Some(libc::EIO) {
        return Ok(0);
    }
    result
}

fn validate_descriptors(
    terminal_input: BorrowedFd<'_>,
    terminal_output: BorrowedFd<'_>,
) -> Result<(), TerminalError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let descriptors = [
        stdin.as_fd(),
        stdout.as_fd(),
        stderr.as_fd(),
        terminal_input,
        terminal_output,
    ];
    if descriptors.iter().any(|descriptor| !isatty(descriptor)) {
        return Err(TerminalError::Unavailable);
    }

    let expected_session = rustix::process::getsid(None).map_err(|_| TerminalError::Unsupported)?;
    let expected_group = rustix::process::getpgrp();
    for descriptor in descriptors {
        if tcgetsid(descriptor).map_err(|_| TerminalError::Unsupported)? != expected_session
            || tcgetpgrp(descriptor).map_err(|_| TerminalError::Unsupported)? != expected_group
        {
            return Err(TerminalError::Unsupported);
        }
    }

    let termios = tcgetattr(terminal_input).map_err(|_| TerminalError::Unsupported)?;
    let disabled = platform::path_value(terminal_input, libc::_PC_VDISABLE)
        .and_then(|value| u8::try_from(value).ok())
        .ok_or(TerminalError::Unsupported)?;
    #[cfg(target_os = "macos")]
    let canonical = CanonicalEvidence::Macos(
        platform::path_value(terminal_input, libc::_PC_MAX_CANON)
            .ok_or(TerminalError::Unsupported)?,
    );
    #[cfg(target_os = "linux")]
    let canonical = CanonicalEvidence::Linux(termios.line_discipline);
    TerminalFacts::from_termios(&termios, disabled, canonical).validate()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CanonicalEvidence {
    #[cfg(any(target_os = "macos", test))]
    Macos(i64),
    #[cfg(any(target_os = "linux", test))]
    Linux(u8),
}

#[derive(Clone, Copy, Debug)]
struct TerminalFacts {
    canonical: bool,
    signals: bool,
    echo: bool,
    echo_controls: bool,
    map_cr_to_lf: bool,
    postprocess_output: bool,
    map_output_lf_to_crlf: bool,
    extproc: bool,
    ignore_cr: bool,
    map_lf_to_cr: bool,
    strip_input: bool,
    case_convert_input: bool,
    case_convert_local: bool,
    case_convert_output: bool,
    interrupt: u8,
    eof: u8,
    suspend: u8,
    quit: u8,
    eol: u8,
    eol2: u8,
    disabled: u8,
    canonical_evidence: CanonicalEvidence,
}

impl TerminalFacts {
    fn from_termios(
        termios: &Termios,
        disabled: u8,
        canonical_evidence: CanonicalEvidence,
    ) -> Self {
        #[cfg(target_os = "linux")]
        let (case_convert_input, case_convert_local, case_convert_output) = (
            termios.input_modes.contains(InputModes::IUCLC),
            termios.local_modes.contains(LocalModes::XCASE),
            termios
                .output_modes
                .contains(rustix::termios::OutputModes::OLCUC),
        );
        #[cfg(target_os = "macos")]
        let (case_convert_input, case_convert_local, case_convert_output) = (false, false, false);

        Self {
            canonical: termios.local_modes.contains(LocalModes::ICANON),
            signals: termios.local_modes.contains(LocalModes::ISIG),
            echo: termios.local_modes.contains(LocalModes::ECHO),
            echo_controls: termios.local_modes.contains(LocalModes::ECHOCTL),
            map_cr_to_lf: termios.input_modes.contains(InputModes::ICRNL),
            postprocess_output: termios.output_modes.contains(OutputModes::OPOST),
            map_output_lf_to_crlf: termios.output_modes.contains(OutputModes::ONLCR),
            extproc: termios.local_modes.contains(LocalModes::EXTPROC),
            ignore_cr: termios.input_modes.contains(InputModes::IGNCR),
            map_lf_to_cr: termios.input_modes.contains(InputModes::INLCR),
            strip_input: termios.input_modes.contains(InputModes::ISTRIP),
            case_convert_input,
            case_convert_local,
            case_convert_output,
            interrupt: termios.special_codes[SpecialCodeIndex::VINTR],
            eof: termios.special_codes[SpecialCodeIndex::VEOF],
            suspend: termios.special_codes[SpecialCodeIndex::VSUSP],
            quit: termios.special_codes[SpecialCodeIndex::VQUIT],
            eol: termios.special_codes[SpecialCodeIndex::VEOL],
            eol2: termios.special_codes[SpecialCodeIndex::VEOL2],
            disabled,
            canonical_evidence,
        }
    }

    fn validate(self) -> Result<(), TerminalError> {
        let required_modes = self.canonical
            && self.signals
            && self.echo
            && self.echo_controls
            && self.map_cr_to_lf
            && self.postprocess_output
            && self.map_output_lf_to_crlf;
        let forbidden_modes = self.extproc
            || self.ignore_cr
            || self.map_lf_to_cr
            || self.strip_input
            || self.case_convert_input
            || self.case_convert_local
            || self.case_convert_output;
        let controls = self.interrupt == 0x03
            && self.eof == 0x04
            && self.suspend == 0x1a
            && self.quit == 0x1c
            && self.eol == self.disabled
            && self.eol2 == self.disabled;
        let capacity = match self.canonical_evidence {
            #[cfg(any(target_os = "macos", test))]
            CanonicalEvidence::Macos(value) => value >= MIN_MACOS_CANONICAL_BYTES,
            #[cfg(any(target_os = "linux", test))]
            CanonicalEvidence::Linux(line_discipline) => line_discipline == LINUX_N_TTY,
        };
        if required_modes && !forbidden_modes && controls && capacity {
            Ok(())
        } else {
            Err(TerminalError::Unsupported)
        }
    }

    #[cfg(test)]
    fn supported(canonical_evidence: CanonicalEvidence) -> Self {
        Self {
            canonical: true,
            signals: true,
            echo: true,
            echo_controls: true,
            map_cr_to_lf: true,
            postprocess_output: true,
            map_output_lf_to_crlf: true,
            extproc: false,
            ignore_cr: false,
            map_lf_to_cr: false,
            strip_input: false,
            case_convert_input: false,
            case_convert_local: false,
            case_convert_output: false,
            interrupt: 0x03,
            eof: 0x04,
            suspend: 0x1a,
            quit: 0x1c,
            eol: 0xff,
            eol2: 0xff,
            disabled: 0xff,
            canonical_evidence,
        }
    }
}

mod platform {
    #![allow(unsafe_code)]

    use std::os::fd::{AsRawFd, BorrowedFd};

    use super::{TTY_PATH_BYTES, TerminalError, TtyPath};

    pub(super) fn tty_path(fd: BorrowedFd<'_>) -> Result<TtyPath, TerminalError> {
        let mut bytes = [0_u8; TTY_PATH_BYTES];
        // SAFETY: `BorrowedFd` keeps the descriptor valid, the array is writable
        // for exactly the supplied length, and ttyname_r does not retain either
        // pointer. A fixed buffer keeps this external lookup allocation-free.
        let result = unsafe {
            libc::ttyname_r(
                fd.as_raw_fd(),
                bytes.as_mut_ptr().cast::<libc::c_char>(),
                bytes.len(),
            )
        };
        if result != 0 {
            return Err(ttyname_error(result));
        }
        let nul = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(TerminalError::Unsupported)?;
        if nul == 0 {
            return Err(TerminalError::Unsupported);
        }
        Ok(TtyPath {
            bytes,
            length_with_nul: nul + 1,
        })
    }

    const fn ttyname_error(result: libc::c_int) -> TerminalError {
        if result == libc::ERANGE {
            TerminalError::Unsupported
        } else {
            TerminalError::Unavailable
        }
    }

    #[cfg(test)]
    pub(super) const fn ttyname_error_for_test(result: libc::c_int) -> TerminalError {
        ttyname_error(result)
    }

    pub(super) fn path_value(fd: BorrowedFd<'_>, name: libc::c_int) -> Option<i64> {
        // SAFETY: BorrowedFd proves the descriptor stays valid for this call;
        // fpathconf neither retains nor mutates the descriptor.
        let value = unsafe { libc::fpathconf(fd.as_raw_fd(), name) };
        (value >= 0).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::fd::{AsFd, OwnedFd},
        os::unix::net::UnixStream,
    };

    use pty_process::blocking;
    use tokio::io::{Interest, unix::AsyncFd};

    use super::{
        AsyncTerminal, CanonicalEvidence, TerminalError, TerminalFacts, platform,
        validate_same_terminal_device,
    };

    fn rejected(facts: TerminalFacts) {
        assert_eq!(facts.validate(), Err(TerminalError::Unsupported));
    }

    #[test]
    fn macos_capacity_and_linux_n_tty_evidence_are_distinct() {
        assert!(
            TerminalFacts::supported(CanonicalEvidence::Macos(1_001))
                .validate()
                .is_ok()
        );
        rejected(TerminalFacts::supported(CanonicalEvidence::Macos(1_000)));
        assert!(
            TerminalFacts::supported(CanonicalEvidence::Linux(0))
                .validate()
                .is_ok()
        );
        rejected(TerminalFacts::supported(CanonicalEvidence::Linux(1)));
    }

    #[test]
    fn fixed_tty_path_buffer_rejects_erange_without_growing() {
        assert_eq!(
            platform::ttyname_error_for_test(libc::ERANGE),
            TerminalError::Unsupported
        );
        assert_eq!(
            platform::ttyname_error_for_test(libc::EBADF),
            TerminalError::Unavailable
        );
    }

    #[test]
    fn independently_opened_terminal_devices_must_match() {
        // Linux exposes every PTY master through the same `/dev/ptmx` clone
        // device, so comparing masters does not prove that two terminal
        // endpoints differ. Slave descriptors identify the actual terminal on
        // both supported platforms and exercise the production identity check.
        let (_first_master, first_slave) = blocking::open().expect("first PTY should open");
        let duplicate = rustix::io::dup(&first_slave).expect("first PTY should duplicate");
        let (_second_master, second_slave) = blocking::open().expect("second PTY should open");

        assert!(validate_same_terminal_device(first_slave.as_fd(), duplicate.as_fd()).is_ok());
        assert_eq!(
            validate_same_terminal_device(first_slave.as_fd(), second_slave.as_fd()),
            Err(TerminalError::Unsupported)
        );
    }

    #[tokio::test]
    async fn input_flush_failure_is_fail_closed() {
        let (input, output) = UnixStream::pair().expect("test socket pair should open");
        let terminal = AsyncTerminal {
            input: AsyncFd::with_interest(OwnedFd::from(input), Interest::READABLE)
                .expect("input should register"),
            output: AsyncFd::with_interest(OwnedFd::from(output), Interest::WRITABLE)
                .expect("output should register"),
        };

        assert_eq!(terminal.flush_input(), Err(TerminalError::Unsupported));
    }

    #[test]
    fn every_required_mode_is_fail_closed() {
        for mutate in [
            |facts: &mut TerminalFacts| facts.canonical = false,
            |facts: &mut TerminalFacts| facts.signals = false,
            |facts: &mut TerminalFacts| facts.echo = false,
            |facts: &mut TerminalFacts| facts.echo_controls = false,
            |facts: &mut TerminalFacts| facts.map_cr_to_lf = false,
            |facts: &mut TerminalFacts| facts.postprocess_output = false,
            |facts: &mut TerminalFacts| facts.map_output_lf_to_crlf = false,
        ] {
            let mut facts = TerminalFacts::supported(CanonicalEvidence::Macos(1_001));
            mutate(&mut facts);
            rejected(facts);
        }
    }

    #[test]
    fn every_unsafe_or_transforming_mode_is_fail_closed() {
        for mutate in [
            |facts: &mut TerminalFacts| facts.extproc = true,
            |facts: &mut TerminalFacts| facts.ignore_cr = true,
            |facts: &mut TerminalFacts| facts.map_lf_to_cr = true,
            |facts: &mut TerminalFacts| facts.strip_input = true,
            |facts: &mut TerminalFacts| facts.case_convert_input = true,
            |facts: &mut TerminalFacts| facts.case_convert_local = true,
            |facts: &mut TerminalFacts| facts.case_convert_output = true,
        ] {
            let mut facts = TerminalFacts::supported(CanonicalEvidence::Macos(1_001));
            mutate(&mut facts);
            rejected(facts);
        }
    }

    #[test]
    fn every_special_key_mapping_is_exact() {
        for mutate in [
            |facts: &mut TerminalFacts| facts.interrupt = 0,
            |facts: &mut TerminalFacts| facts.eof = 0,
            |facts: &mut TerminalFacts| facts.suspend = 0,
            |facts: &mut TerminalFacts| facts.quit = 0,
            |facts: &mut TerminalFacts| facts.eol = b'x',
            |facts: &mut TerminalFacts| facts.eol2 = b'x',
        ] {
            let mut facts = TerminalFacts::supported(CanonicalEvidence::Macos(1_001));
            mutate(&mut facts);
            rejected(facts);
        }
    }

    #[test]
    fn linux_eio_is_normalized_only_on_linux() {
        let error = std::io::Error::from_raw_os_error(libc::EIO);
        #[cfg(target_os = "linux")]
        assert_eq!(super::normalize_read(Err(error)).unwrap(), 0);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(
            super::normalize_read(Err(error))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EIO)
        );
    }
}
