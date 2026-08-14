use std::{
    io::{self, Read},
    os::fd::AsFd,
    thread,
    time::Duration,
};

use thiserror::Error;
use tokio::{sync::oneshot, time::Instant};

use super::{
    args::MAX_PROMPT_BYTES,
    script::ScriptTurnSummary,
    signal::{DriverMode, SignalLatch, SignalStreams, UiSignal, self_suspend},
};

const IO_CHUNK_BYTES: usize = 8 * 1024;
const FINAL_OUTPUT_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ScriptInputError {
    #[error("CLI_INPUT_INVALID")]
    Invalid,
    #[error("CLI_INPUT_TOO_LARGE")]
    TooLarge,
}

pub(super) fn read_bounded_prompt(mut reader: impl Read) -> Result<String, ScriptInputError> {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; IO_CHUNK_BYTES];
    loop {
        let remaining = MAX_PROMPT_BYTES.saturating_sub(bytes.len());
        let read_limit = scratch.len().min(remaining.saturating_add(1));
        let count = reader
            .read(&mut scratch[..read_limit])
            .map_err(|_| ScriptInputError::Invalid)?;
        if count == 0 {
            break;
        }
        if count > remaining {
            return Err(ScriptInputError::TooLarge);
        }
        bytes
            .try_reserve(count)
            .map_err(|_| ScriptInputError::TooLarge)?;
        bytes.extend_from_slice(&scratch[..count]);
    }
    let prompt = String::from_utf8(bytes).map_err(|_| ScriptInputError::Invalid)?;
    if prompt.trim().is_empty() {
        return Err(ScriptInputError::Invalid);
    }
    Ok(prompt)
}

pub(super) async fn read_piped_prompt_or_exit(
    signals: &mut SignalStreams,
) -> Result<String, ScriptInputError> {
    let (sender, receiver) = oneshot::channel();
    let worker = thread::Builder::new()
        .name("dsh-script-input".to_owned())
        .spawn(move || {
            let result = read_bounded_prompt(io::stdin().lock());
            let _ = sender.send(result);
        })
        .map_err(|_| ScriptInputError::Invalid)?;
    tokio::pin!(receiver);
    tokio::select! {
        biased;
        signal = signals.next() => exit_for_input_signal(signal, signals, worker),
        result = &mut receiver => {
            worker.join().map_err(|_| ScriptInputError::Invalid)?;
            let result = result.map_err(|_| ScriptInputError::Invalid)?;
            tokio::task::yield_now().await;
            let mut latch = SignalLatch::default();
            signals.drain_ready(DriverMode::Script, &mut latch);
            if let Some(signal) = latch.observed() {
                exit_after_completed_io_signal(signal, signals);
            }
            result
        }
    }
}

fn exit_for_input_signal(
    signal: UiSignal,
    signals: &mut SignalStreams,
    _owned_worker: thread::JoinHandle<()>,
) -> ! {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            std::process::exit(1);
        }
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Script, signal);
        signals.drain_ready(DriverMode::Script, &mut latch);
        if let Some(code) = latch.observed().and_then(UiSignal::exit_code) {
            std::process::exit(code.into());
        }
        std::process::exit(148);
    }
    std::process::exit(signal.exit_code().unwrap_or(1).into());
}

fn exit_after_completed_io_signal(signal: UiSignal, signals: &mut SignalStreams) -> ! {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            std::process::exit(1);
        }
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Script, signal);
        signals.drain_ready(DriverMode::Script, &mut latch);
        std::process::exit(
            latch
                .observed()
                .and_then(UiSignal::exit_code)
                .unwrap_or(148)
                .into(),
        );
    }
    std::process::exit(signal.exit_code().unwrap_or(1).into());
}

#[derive(Debug)]
pub(super) struct ScriptOutputFrames {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ScriptOutputFrames {
    pub(super) fn from_summary(summary: &ScriptTurnSummary<'_>) -> Result<Self, ScriptOutputError> {
        let mut stdout = Vec::new();
        summary
            .write_stdout(|chunk| append_chunk(&mut stdout, chunk))
            .map_err(|_| ScriptOutputError)?;
        let mut stderr = Vec::new();
        summary
            .write_stderr(|chunk| append_chunk(&mut stderr, chunk))
            .map_err(|_| ScriptOutputError)?;
        Ok(Self { stdout, stderr })
    }

    pub(super) fn agent_failure() -> Result<Self, ScriptOutputError> {
        let message = b"dsh: CLI_AGENT_UNAVAILABLE\n";
        let mut stderr = Vec::new();
        stderr
            .try_reserve_exact(message.len())
            .map_err(|_| ScriptOutputError)?;
        stderr.extend_from_slice(message);
        Ok(Self {
            stdout: Vec::new(),
            stderr,
        })
    }

    #[cfg(test)]
    fn from_bytes(stdout: &[u8], stderr: &[u8]) -> Self {
        Self {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }
}

fn append_chunk(target: &mut Vec<u8>, chunk: &str) -> Result<(), ScriptOutputError> {
    target
        .try_reserve(chunk.len())
        .map_err(|_| ScriptOutputError)?;
    target.extend_from_slice(chunk.as_bytes());
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("CLI_OUTPUT_FAILED")]
pub(super) struct ScriptOutputError;

pub(super) async fn write_final_output_or_exit(
    frames: ScriptOutputFrames,
    signals: &mut SignalStreams,
) -> Result<(), ScriptOutputError> {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let stdout = rustix::io::dup(stdout.as_fd()).map_err(|_| ScriptOutputError)?;
    let stderr = rustix::io::dup(stderr.as_fd()).map_err(|_| ScriptOutputError)?;
    let (sender, receiver) = oneshot::channel();
    let worker = thread::Builder::new()
        .name("dsh-final-output".to_owned())
        .spawn(move || {
            let result = write_all(&stdout, &frames.stdout)
                .and_then(|()| write_all(&stderr, &frames.stderr));
            let _ = sender.send(result);
        })
        .map_err(|_| ScriptOutputError)?;
    let deadline = Instant::now() + FINAL_OUTPUT_DEADLINE;
    tokio::pin!(receiver);
    tokio::select! {
        biased;
        signal = signals.next() => exit_for_output_signal(signal, signals, worker),
        () = tokio::time::sleep_until(deadline) => exit_with_owned_writer(worker, 1),
        result = &mut receiver => {
            worker.join().map_err(|_| ScriptOutputError)?;
            let result = result.map_err(|_| ScriptOutputError)?;
            tokio::task::yield_now().await;
            let mut latch = SignalLatch::default();
            signals.drain_ready(DriverMode::Script, &mut latch);
            if let Some(signal) = latch.observed() {
                exit_after_completed_io_signal(signal, signals);
            }
            result?;
            Ok(())
        }
    }
}

fn exit_for_output_signal(
    signal: UiSignal,
    signals: &mut SignalStreams,
    owned_worker: thread::JoinHandle<()>,
) -> ! {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            exit_with_owned_writer(owned_worker, 1);
        }
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Script, signal);
        signals.drain_ready(DriverMode::Script, &mut latch);
        let code = latch
            .observed()
            .and_then(UiSignal::exit_code)
            .unwrap_or(148);
        exit_with_owned_writer(owned_worker, code);
    }
    exit_with_owned_writer(owned_worker, signal.exit_code().unwrap_or(1));
}

fn exit_with_owned_writer(_owned_worker: thread::JoinHandle<()>, code: u8) -> ! {
    // `process::exit` is intentional here: a kernel-stuck final writer cannot
    // be cancelled or joined, and returning would silently detach it while the
    // product continued. All Agent/tool state has already settled.
    std::process::exit(code.into())
}

fn write_all(fd: &impl AsFd, mut bytes: &[u8]) -> Result<(), ScriptOutputError> {
    while !bytes.is_empty() {
        let chunk = &bytes[..bytes.len().min(IO_CHUNK_BYTES)];
        match rustix::io::write(fd, chunk) {
            Ok(0) => return Err(ScriptOutputError),
            Ok(count) => bytes = &bytes[count..],
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return Err(ScriptOutputError),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Error, ErrorKind, Read};

    use super::{MAX_PROMPT_BYTES, ScriptInputError, ScriptOutputFrames, read_bounded_prompt};

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(Error::new(ErrorKind::Other, "sentinel secret"))
        }
    }

    #[test]
    fn piped_prompt_accepts_exact_limit_and_rejects_one_extra_byte() {
        let exact = vec![b'x'; MAX_PROMPT_BYTES];
        assert_eq!(
            read_bounded_prompt(Cursor::new(exact)).unwrap().len(),
            MAX_PROMPT_BYTES
        );
        let over = vec![b'x'; MAX_PROMPT_BYTES + 1];
        assert_eq!(
            read_bounded_prompt(Cursor::new(over)),
            Err(ScriptInputError::TooLarge)
        );
    }

    #[test]
    fn piped_prompt_rejects_empty_whitespace_invalid_utf8_and_read_errors() {
        for input in [&b""[..], b"  \n\t", &[0xff]] {
            assert_eq!(
                read_bounded_prompt(Cursor::new(input)),
                Err(ScriptInputError::Invalid)
            );
        }
        let error = read_bounded_prompt(FailingReader).unwrap_err();
        assert_eq!(error, ScriptInputError::Invalid);
        assert!(!error.to_string().contains("sentinel"));
    }

    #[test]
    fn immutable_final_frames_keep_stdout_and_stderr_separate() {
        let frames = ScriptOutputFrames::from_bytes(b"answer\n", b"error\n");
        assert_eq!(frames.stdout, b"answer\n");
        assert_eq!(frames.stderr, b"error\n");
    }
}
