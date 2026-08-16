//! One-owner blocking journal writer with cancellation-safe async waits.

use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::unix::fs::FileExt as _,
    thread,
};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::journal_row::{JournalRowLocator, RawRowHasher};

const COMMAND_CAPACITY: usize = 1;
pub(super) const MAX_PRUNE_PREFIX_BYTES: usize = 10 * 1024 * 1024;
const MAX_PRUNE_PREFIX_ROWS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalCursor {
    pub(crate) physical_offset: u64,
    pub(crate) durable_offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum JournalError {
    #[error("the session journal is poisoned")]
    Poisoned,
    #[error("the session journal writer stopped")]
    WriterStopped,
    #[error("the session journal has no staged append")]
    NothingStaged,
    #[error("the session journal already has a staged append")]
    AlreadyStaged,
    #[error("the session journal must settle its in-flight command first")]
    FlightInProgress,
}

enum Command {
    Append {
        bytes: Vec<u8>,
        ack: oneshot::Sender<Result<JournalCursor, JournalError>>,
    },
    AppendPrunePrefix {
        bytes: Vec<u8>,
        rows: usize,
        ack: oneshot::Sender<Result<JournalCursor, JournalError>>,
    },
    Barrier {
        ack: oneshot::Sender<Result<JournalCursor, JournalError>>,
    },
    ReadRow {
        locator: JournalRowLocator,
        cancellation: CancellationToken,
        ack: oneshot::Sender<Result<Vec<u8>, ReadCommandError>>,
    },
    Finish {
        ack: oneshot::Sender<Result<JournalCursor, JournalError>>,
    },
}

/// Client/server halves used when a recovery worker becomes the normal writer
/// without releasing its file descriptor or advisory lock in between.
pub(super) struct JournalHandoff {
    sender: mpsc::Sender<Command>,
}

pub(super) struct JournalInbox {
    receiver: mpsc::Receiver<Command>,
}

pub(super) fn handoff_channel() -> (JournalHandoff, JournalInbox) {
    let (sender, receiver) = mpsc::channel(COMMAND_CAPACITY);
    (JournalHandoff { sender }, JournalInbox { receiver })
}

impl JournalInbox {
    pub(super) fn run(self, file: File, durable_offset: u64) {
        writer_main(file, durable_offset, self.receiver);
    }
}

struct Flight {
    kind: FlightKind,
    ack: oneshot::Receiver<Result<JournalCursor, JournalError>>,
}

struct ReadFlight {
    locator: JournalRowLocator,
    cancellation: CancellationToken,
    ack: oneshot::Receiver<Result<Vec<u8>, ReadCommandError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadCommandError {
    Cancelled,
    Writer(JournalError),
}

enum PendingWrite {
    Ordinary(Vec<u8>),
    PrunePrefix { bytes: Vec<u8>, rows: usize },
}

impl PendingWrite {
    fn len(&self) -> usize {
        match self {
            Self::Ordinary(bytes) | Self::PrunePrefix { bytes, .. } => bytes.len(),
        }
    }

    fn kind(&self) -> FlightKind {
        match self {
            Self::Ordinary(_) => FlightKind::Append,
            Self::PrunePrefix { .. } => FlightKind::PrunePrefix,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlightKind {
    Append,
    PrunePrefix,
    Barrier,
    Finish,
}

/// Owns a not-yet-settled bootstrap thread before any async wait begins.
///
/// Dropping the wait future leaves the receiver and thread handle here, so a
/// later wait or shutdown can settle the same physical creation operation.
pub(super) struct DeferredWriter<E> {
    sender: Option<mpsc::Sender<Command>>,
    startup: Option<oneshot::Receiver<Result<JournalCursor, E>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl<E: Send + 'static> DeferredWriter<E> {
    pub(super) fn start(
        factory: impl FnOnce() -> Result<(File, u64), E> + Send + 'static,
    ) -> Result<Self, JournalError> {
        let (sender, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (startup_ack, startup) = oneshot::channel();
        let join = thread::Builder::new()
            .name("dsh-session-journal".to_owned())
            .spawn(move || match factory() {
                Ok((file, durable_offset)) => {
                    let cursor = JournalCursor {
                        physical_offset: durable_offset,
                        durable_offset,
                    };
                    if startup_ack.send(Ok(cursor)).is_ok() {
                        writer_main(file, durable_offset, receiver);
                    }
                }
                Err(error) => {
                    let _ = startup_ack.send(Err(error));
                }
            })
            .map_err(|_| JournalError::WriterStopped)?;
        Ok(Self {
            sender: Some(sender),
            startup: Some(startup),
            join: Some(join),
        })
    }

    pub(super) async fn wait_ready(&mut self) -> Result<Result<JournalWriter, E>, JournalError> {
        let startup = self.startup.as_mut().ok_or(JournalError::WriterStopped)?;
        let result = startup.await.map_err(|_| JournalError::WriterStopped);
        self.startup = None;
        match result {
            Ok(Ok(cursor)) => Ok(Ok(JournalWriter::from_running(
                self.sender.take().ok_or(JournalError::WriterStopped)?,
                self.join.take().ok_or(JournalError::WriterStopped)?,
                cursor,
            ))),
            Ok(Err(error)) => {
                self.sender.take();
                self.join_worker()?;
                Ok(Err(error))
            }
            Err(error) => {
                self.sender.take();
                let _ = self.join_worker();
                Err(error)
            }
        }
    }

    fn join_worker(&mut self) -> Result<(), JournalError> {
        if self.join.take().is_some_and(|join| join.join().is_err()) {
            return Err(JournalError::WriterStopped);
        }
        Ok(())
    }
}

impl<E> Drop for DeferredWriter<E> {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Sole async handle for one standard thread, fd, and advisory lock.
pub(crate) struct JournalWriter {
    sender: Option<mpsc::Sender<Command>>,
    pending: Option<PendingWrite>,
    flight: Option<Flight>,
    read_flight: Option<ReadFlight>,
    join: Option<thread::JoinHandle<()>>,
    cursor: JournalCursor,
    poisoned: bool,
    finished: bool,
    finish_error: Option<JournalError>,
}

impl std::fmt::Debug for JournalWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JournalWriter")
            .field("pending", &self.pending.as_ref().map(PendingWrite::len))
            .field("flight", &self.flight.is_some())
            .field("read_flight", &self.read_flight.is_some())
            .field("cursor", &self.cursor)
            .field("poisoned", &self.poisoned)
            .field("finished", &self.finished)
            .field("finish_error", &self.finish_error)
            .finish()
    }
}

impl JournalWriter {
    #[cfg(test)]
    pub(crate) fn start(file: File, durable_offset: u64) -> Result<Self, JournalError> {
        let (sender, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let join = thread::Builder::new()
            .name("dsh-session-journal".to_owned())
            .spawn(move || writer_main(file, durable_offset, receiver))
            .map_err(|_| JournalError::WriterStopped)?;
        Ok(Self::from_running(
            sender,
            join,
            JournalCursor {
                physical_offset: durable_offset,
                durable_offset,
            },
        ))
    }

    fn from_running(
        sender: mpsc::Sender<Command>,
        join: thread::JoinHandle<()>,
        cursor: JournalCursor,
    ) -> Self {
        Self {
            sender: Some(sender),
            pending: None,
            flight: None,
            read_flight: None,
            join: Some(join),
            cursor,
            poisoned: false,
            finished: false,
            finish_error: None,
        }
    }

    pub(super) fn from_handoff(
        handoff: JournalHandoff,
        join: thread::JoinHandle<()>,
        cursor: JournalCursor,
    ) -> Self {
        Self::from_running(handoff.sender, join, cursor)
    }

    /// Move an already bounded command into owner state before any await.
    pub(crate) fn stage(&mut self, bytes: Vec<u8>) -> Result<(), JournalError> {
        self.ensure_stageable()?;
        self.pending = Some(PendingWrite::Ordinary(bytes));
        Ok(())
    }

    /// Stage one already validated marker-only or marker/replacement prefix.
    pub(crate) fn stage_prune_prefix(
        &mut self,
        bytes: Vec<u8>,
        rows: usize,
    ) -> Result<(), JournalError> {
        self.ensure_stageable()?;
        if !valid_prune_prefix(&bytes, rows) {
            self.poisoned = true;
            self.finish_error.get_or_insert(JournalError::Poisoned);
            return Err(JournalError::Poisoned);
        }
        self.pending = Some(PendingWrite::PrunePrefix { bytes, rows });
        Ok(())
    }

    pub(crate) fn ensure_stageable(&self) -> Result<(), JournalError> {
        self.ensure_usable()?;
        if self.flight.is_some() {
            return Err(JournalError::FlightInProgress);
        }
        if self.read_flight.is_some() {
            return Err(JournalError::FlightInProgress);
        }
        if self.pending.is_some() {
            return Err(JournalError::AlreadyStaged);
        }
        Ok(())
    }

    pub(super) fn latch_poison(&mut self) {
        self.poisoned = true;
        self.finish_error.get_or_insert(JournalError::Poisoned);
    }

    /// Send and settle the owner-held staged bytes.
    ///
    /// Cancelling this wait leaves either `pending` or `flight` inside `self`,
    /// so a later barrier/shutdown can continue the same operation.
    pub(crate) async fn flush_staged(&mut self) -> Result<JournalCursor, JournalError> {
        self.ensure_usable()?;
        if let Some(kind) = self.flight.as_ref().map(|flight| flight.kind) {
            let cursor = self.settle_flight().await?;
            if matches!(kind, FlightKind::Append | FlightKind::PrunePrefix) {
                return Ok(cursor);
            }
        }
        if self.pending.is_none() {
            return Err(JournalError::NothingStaged);
        }
        let sender = self.sender.as_ref().ok_or(JournalError::WriterStopped)?;
        let permit = sender
            .reserve()
            .await
            .map_err(|_| JournalError::WriterStopped)?;
        let pending = self.pending.take().ok_or(JournalError::NothingStaged)?;
        let kind = pending.kind();
        let (ack, receiver) = oneshot::channel();
        permit.send(match pending {
            PendingWrite::Ordinary(bytes) => Command::Append { bytes, ack },
            PendingWrite::PrunePrefix { bytes, rows } => {
                Command::AppendPrunePrefix { bytes, rows, ack }
            }
        });
        self.flight = Some(Flight {
            kind,
            ack: receiver,
        });
        self.settle_flight().await
    }

    /// Settle any command already owned by this writer before staging bytes.
    pub(crate) async fn settle_before_stage(&mut self) -> Result<JournalCursor, JournalError> {
        self.ensure_usable()?;
        if self.read_flight.is_some() {
            self.settle_read_before_other_command().await?;
        }
        if self.pending.is_some() {
            self.flush_staged().await
        } else {
            self.settle_flight().await
        }
    }

    pub(crate) async fn barrier(&mut self) -> Result<JournalCursor, JournalError> {
        self.ensure_usable()?;
        if self.read_flight.is_some() {
            self.settle_read_before_other_command().await?;
        }
        if self.pending.is_some() {
            self.flush_staged().await?;
        } else if let Some(kind) = self.flight.as_ref().map(|flight| flight.kind) {
            let cursor = self.settle_flight().await?;
            if kind == FlightKind::Barrier {
                return Ok(cursor);
            }
            if kind == FlightKind::Finish {
                return Err(self.finish_error.unwrap_or(JournalError::WriterStopped));
            }
        }
        if self.finished {
            return Err(self.finish_error.unwrap_or(JournalError::WriterStopped));
        }
        self.send_control(FlightKind::Barrier).await
    }

    pub(crate) async fn finish(&mut self) -> Result<JournalCursor, JournalError> {
        if self.finished {
            return self.finish_error.map_or(Ok(self.cursor), Err);
        }
        let settle = if self.read_flight.is_some() {
            self.settle_read_before_other_command()
                .await
                .map(|()| self.cursor)
        } else if self.pending.is_some() {
            self.flush_staged().await
        } else {
            self.settle_flight().await
        };
        if let Err(error) = settle {
            self.finish_error.get_or_insert(error);
            self.pending.take();
        }
        if !self.finished {
            if let Err(error) = self.send_control(FlightKind::Finish).await {
                self.finish_error.get_or_insert(error);
            }
        }
        self.sender.take();
        if let Err(error) = self.join_worker() {
            self.finish_error.get_or_insert(error);
        }
        self.finished = true;
        self.finish_error.map_or(Ok(self.cursor), Err)
    }

    /// Read one already durable event row through the same fd and owner thread.
    ///
    /// If this wait is cancelled, the receiver remains in `self`; retrying the
    /// same locator settles the original physical read instead of issuing a
    /// duplicate command.
    pub(super) async fn read_durable_row(
        &mut self,
        locator: JournalRowLocator,
        cancellation: CancellationToken,
    ) -> Result<Vec<u8>, JournalReadError> {
        self.ensure_usable().map_err(JournalReadError::Writer)?;
        if let Some(active) = self.read_flight.as_ref() {
            if active.locator == locator {
                return self.settle_read_flight().await;
            }
            active.cancellation.cancel();
            match self.settle_read_flight().await {
                Ok(_) | Err(JournalReadError::Cancelled) => {}
                Err(error) => return Err(error),
            }
        }
        if self.pending.is_some() {
            self.flush_staged()
                .await
                .map_err(JournalReadError::Writer)?;
        } else if self.flight.is_some() {
            self.settle_flight()
                .await
                .map_err(JournalReadError::Writer)?;
        }
        if cancellation.is_cancelled() {
            return Err(JournalReadError::Cancelled);
        }
        if self.cursor.physical_offset != self.cursor.durable_offset {
            self.send_control(FlightKind::Barrier)
                .await
                .map_err(JournalReadError::Writer)?;
            if cancellation.is_cancelled() {
                return Err(JournalReadError::Cancelled);
            }
        }
        if locator
            .end()
            .is_none_or(|end| end > self.cursor.durable_offset)
        {
            return Err(JournalReadError::NotDurable);
        }

        let sender = self
            .sender
            .as_ref()
            .ok_or(JournalReadError::Writer(JournalError::WriterStopped))?;
        let permit = sender
            .reserve()
            .await
            .map_err(|_| JournalReadError::Writer(JournalError::WriterStopped))?;
        let (ack, receiver) = oneshot::channel();
        let owned_cancellation = cancellation.child_token();
        permit.send(Command::ReadRow {
            locator,
            cancellation: owned_cancellation.clone(),
            ack,
        });
        self.read_flight = Some(ReadFlight {
            locator,
            cancellation: owned_cancellation,
            ack: receiver,
        });
        self.settle_read_flight().await
    }

    async fn send_control(&mut self, kind: FlightKind) -> Result<JournalCursor, JournalError> {
        if kind != FlightKind::Finish {
            self.ensure_usable()?;
        }
        let sender = self.sender.as_ref().ok_or(JournalError::WriterStopped)?;
        let permit = sender
            .reserve()
            .await
            .map_err(|_| JournalError::WriterStopped)?;
        let (ack, receiver) = oneshot::channel();
        permit.send(match kind {
            FlightKind::Finish => Command::Finish { ack },
            FlightKind::Barrier => Command::Barrier { ack },
            FlightKind::Append | FlightKind::PrunePrefix => {
                return Err(JournalError::WriterStopped);
            }
        });
        self.flight = Some(Flight {
            kind,
            ack: receiver,
        });
        self.settle_flight().await
    }

    async fn settle_flight(&mut self) -> Result<JournalCursor, JournalError> {
        let Some(flight) = self.flight.as_mut() else {
            return Ok(self.cursor);
        };
        let result = (&mut flight.ack).await;
        let kind = self
            .flight
            .as_ref()
            .map(|flight| flight.kind)
            .ok_or(JournalError::WriterStopped)?;
        self.flight = None;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                self.poisoned = true;
                self.sender.take();
                let _ = self.join_worker();
                if kind == FlightKind::Finish {
                    self.finished = true;
                    self.finish_error.get_or_insert(JournalError::WriterStopped);
                }
                return Err(JournalError::WriterStopped);
            }
        };
        match result {
            Ok(cursor) => {
                self.cursor = cursor;
                if kind == FlightKind::Finish {
                    self.finished = true;
                    self.sender.take();
                    if let Err(error) = self.join_worker() {
                        self.finish_error.get_or_insert(error);
                        return Err(error);
                    }
                }
                Ok(cursor)
            }
            Err(error) => {
                self.poisoned = true;
                if kind == FlightKind::Finish {
                    self.finished = true;
                    self.finish_error.get_or_insert(error);
                    self.sender.take();
                    let _ = self.join_worker();
                }
                Err(error)
            }
        }
    }

    async fn settle_read_flight(&mut self) -> Result<Vec<u8>, JournalReadError> {
        let Some(flight) = self.read_flight.as_mut() else {
            return Err(JournalReadError::Writer(JournalError::FlightInProgress));
        };
        let result = (&mut flight.ack).await;
        self.read_flight = None;
        match result {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(ReadCommandError::Cancelled)) => Err(JournalReadError::Cancelled),
            Ok(Err(ReadCommandError::Writer(error))) => {
                self.poisoned = true;
                Err(JournalReadError::Writer(error))
            }
            Err(_) => {
                self.poisoned = true;
                self.sender.take();
                let _ = self.join_worker();
                Err(JournalReadError::Writer(JournalError::WriterStopped))
            }
        }
    }

    async fn settle_read_before_other_command(&mut self) -> Result<(), JournalError> {
        if let Some(flight) = &self.read_flight {
            flight.cancellation.cancel();
        }
        match self.settle_read_flight().await {
            Ok(_) | Err(JournalReadError::Cancelled) => Ok(()),
            Err(JournalReadError::NotDurable) => Err(JournalError::Poisoned),
            Err(JournalReadError::Writer(error)) => Err(error),
        }
    }

    fn join_worker(&mut self) -> Result<(), JournalError> {
        if self.join.take().is_some_and(|join| join.join().is_err()) {
            self.poisoned = true;
            return Err(JournalError::WriterStopped);
        }
        Ok(())
    }

    fn ensure_usable(&self) -> Result<(), JournalError> {
        if self.poisoned {
            Err(JournalError::Poisoned)
        } else if self.finished {
            Err(JournalError::WriterStopped)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum JournalReadError {
    #[error("the requested journal row is not durable")]
    NotDurable,
    #[error("the requested journal row read was cancelled")]
    Cancelled,
    #[error(transparent)]
    Writer(#[from] JournalError),
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        // Abnormal fallback: dropping the sole sender lets the worker finish
        // already queued work and then release its fd/flock. Pending unsent
        // bytes are deliberately not claimed durable.
        if let Some(flight) = &self.read_flight {
            flight.cancellation.cancel();
        }
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn writer_main(mut file: File, initial_offset: u64, mut receiver: mpsc::Receiver<Command>) {
    let mut cursor = JournalCursor {
        physical_offset: initial_offset,
        durable_offset: initial_offset,
    };
    let mut poisoned = false;
    while let Some(command) = receiver.blocking_recv() {
        match command {
            Command::Append { bytes, ack } => {
                let result = if poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    append_bytes(&mut file, &mut cursor, &bytes).inspect_err(|_| {
                        poisoned = true;
                    })
                };
                let _ = ack.send(result);
            }
            Command::AppendPrunePrefix { bytes, rows, ack } => {
                let result = if poisoned || !valid_prune_prefix(&bytes, rows) {
                    poisoned = true;
                    Err(JournalError::Poisoned)
                } else {
                    append_bytes(&mut file, &mut cursor, &bytes).inspect_err(|_| {
                        poisoned = true;
                    })
                };
                let _ = ack.send(result);
            }
            Command::Barrier { ack } => {
                let result = if poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    barrier_file(&mut file, &mut cursor).inspect_err(|_| {
                        poisoned = true;
                    })
                };
                let _ = ack.send(result);
            }
            Command::ReadRow {
                locator,
                cancellation,
                ack,
            } => {
                let result = if poisoned {
                    Err(ReadCommandError::Writer(JournalError::Poisoned))
                } else {
                    read_row(&file, cursor, locator, &cancellation).inspect_err(|error| {
                        poisoned |= matches!(error, ReadCommandError::Writer(_));
                    })
                };
                let _ = ack.send(result);
            }
            Command::Finish { ack } => {
                let result = if poisoned {
                    Err(JournalError::Poisoned)
                } else {
                    barrier_file(&mut file, &mut cursor).inspect_err(|_| {
                        poisoned = true;
                    })
                };
                drop(file);
                let _ = ack.send(result);
                return;
            }
        }
    }
}

fn valid_prune_prefix(bytes: &[u8], rows: usize) -> bool {
    if !(1..=MAX_PRUNE_PREFIX_ROWS).contains(&rows)
        || bytes.is_empty()
        || bytes.len() > MAX_PRUNE_PREFIX_BYTES
        || bytes.last() != Some(&b'\n')
    {
        return false;
    }
    let mut row_count = 0_usize;
    let mut row_start = 0_usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let row_len = index + 1 - row_start;
        if row_len == 1 || row_len > super::jsonl::MAX_JOURNAL_EVENT_LINE_BYTES {
            return false;
        }
        row_count += 1;
        row_start = index + 1;
    }
    row_count == rows && row_start == bytes.len()
}

fn read_row(
    file: &File,
    cursor: JournalCursor,
    locator: JournalRowLocator,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ReadCommandError> {
    read_row_with_chunk_observer(file, cursor, locator, cancellation, |_| {})
}

fn read_row_with_chunk_observer(
    file: &File,
    cursor: JournalCursor,
    locator: JournalRowLocator,
    cancellation: &CancellationToken,
    mut chunk_observer: impl FnMut(usize),
) -> Result<Vec<u8>, ReadCommandError> {
    if cancellation.is_cancelled() {
        return Err(ReadCommandError::Cancelled);
    }
    if locator.end().is_none_or(|end| end > cursor.durable_offset) {
        return Err(ReadCommandError::Writer(JournalError::Poisoned));
    }
    let length = usize::try_from(locator.length())
        .map_err(|_| ReadCommandError::Writer(JournalError::Poisoned))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| ReadCommandError::Writer(JournalError::Poisoned))?;
    bytes.resize(length, 0);
    const READ_CHUNK_BYTES: usize = 64 * 1024;
    let mut hasher = RawRowHasher::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        if cancellation.is_cancelled() {
            return Err(ReadCommandError::Cancelled);
        }
        let end = offset.saturating_add(READ_CHUNK_BYTES).min(bytes.len());
        let physical_offset = locator
            .offset()
            .checked_add(
                u64::try_from(offset)
                    .map_err(|_| ReadCommandError::Writer(JournalError::Poisoned))?,
            )
            .ok_or(ReadCommandError::Writer(JournalError::Poisoned))?;
        file.read_exact_at(&mut bytes[offset..end], physical_offset)
            .map_err(|_| ReadCommandError::Writer(JournalError::Poisoned))?;
        for (relative, byte) in bytes[offset..end].iter().enumerate() {
            let index = offset + relative;
            if (*byte == b'\n') != (index + 1 == length) {
                return Err(ReadCommandError::Writer(JournalError::Poisoned));
            }
        }
        hasher.update(&bytes[offset..end]);
        offset = end;
        chunk_observer(offset);
        if offset < bytes.len() && cancellation.is_cancelled() {
            return Err(ReadCommandError::Cancelled);
        }
    }
    if hasher.finish() != locator.full_sha256() {
        return Err(ReadCommandError::Writer(JournalError::Poisoned));
    }
    if cancellation.is_cancelled() {
        return Err(ReadCommandError::Cancelled);
    }
    Ok(bytes)
}

/// Commit a pre-encoded recovery suffix before this same thread becomes the
/// ordinary journal writer.
///
/// Recovery first makes the selected valid prefix durable, then appends the
/// complete prevalidated suffix and synchronizes it. No serialization,
/// timestamps, IDs, or capacity decisions remain after truncation starts.
pub(super) fn commit_recovery_suffix(
    file: &mut File,
    valid_offset: u64,
    suffix: &[u8],
) -> Result<JournalCursor, JournalError> {
    let suffix_bytes = u64::try_from(suffix.len()).map_err(|_| JournalError::Poisoned)?;
    let final_offset = valid_offset
        .checked_add(suffix_bytes)
        .ok_or(JournalError::Poisoned)?;
    file.set_len(valid_offset)
        .map_err(|_| JournalError::Poisoned)?;
    file.seek(SeekFrom::Start(valid_offset))
        .map_err(|_| JournalError::Poisoned)?;
    sync_durable(file).map_err(|_| JournalError::Poisoned)?;
    if file.write_all(suffix).is_err() {
        let _ = file.set_len(valid_offset);
        let _ = file.seek(SeekFrom::Start(valid_offset));
        let _ = sync_durable(file);
        return Err(JournalError::Poisoned);
    }
    if sync_durable(file).is_err() {
        return Err(JournalError::Poisoned);
    }
    Ok(JournalCursor {
        physical_offset: final_offset,
        durable_offset: final_offset,
    })
}

fn append_bytes(
    file: &mut File,
    cursor: &mut JournalCursor,
    bytes: &[u8],
) -> Result<JournalCursor, JournalError> {
    let byte_count = u64::try_from(bytes.len()).map_err(|_| JournalError::Poisoned)?;
    let next_physical_offset = cursor
        .physical_offset
        .checked_add(byte_count)
        .ok_or(JournalError::Poisoned)?;
    file.seek(SeekFrom::Start(cursor.physical_offset))
        .map_err(|_| poison_and_rollback(file, cursor))?;
    if file.write_all(bytes).is_err() {
        return Err(poison_and_rollback(file, cursor));
    }
    cursor.physical_offset = next_physical_offset;
    Ok(*cursor)
}

fn barrier_file(
    file: &mut File,
    cursor: &mut JournalCursor,
) -> Result<JournalCursor, JournalError> {
    if sync_durable(file).is_err() {
        return Err(poison_and_rollback(file, cursor));
    }
    cursor.durable_offset = cursor.physical_offset;
    Ok(*cursor)
}

fn poison_and_rollback(file: &mut File, cursor: &mut JournalCursor) -> JournalError {
    let rollback = file
        .set_len(cursor.durable_offset)
        .and_then(|()| file.seek(SeekFrom::Start(cursor.durable_offset)).map(drop))
        .and_then(|()| sync_durable(file));
    cursor.physical_offset = cursor.durable_offset;
    let _ = rollback;
    JournalError::Poisoned
}

#[cfg(target_os = "macos")]
fn sync_durable(file: &File) -> std::io::Result<()> {
    rustix::fs::fcntl_fullfsync(file).map_err(std::io::Error::from)
}

#[cfg(not(target_os = "macos"))]
fn sync_durable(file: &File) -> std::io::Result<()> {
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        future::{Future as _, poll_fn},
        io::{Read, Write},
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        task::Poll,
        thread,
        time::Duration,
    };

    use tokio::sync::{mpsc as tokio_mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use crate::model::{ContentBlock, Message};
    use crate::session::{
        AppendError, BarrierError, ClaimedAppend, Clock, ClockError, EventKind, EventSeq, NewEvent,
        PrunePairAppendError, Session, SessionMode, StepId, SurfaceIntent, SystemClock, TodoItem,
        TodoStatus, ToolResultPruneConfig, TurnEndReason, TurnId, UnixMillis,
        journal_row::JournalRowLocator,
    };

    use super::{
        COMMAND_CAPACITY, Command, FlightKind, JournalCursor, JournalError, JournalReadError,
        JournalWriter, ReadCommandError, append_bytes, barrier_file, read_row,
        read_row_with_chunk_observer, valid_prune_prefix,
    };

    #[tokio::test]
    async fn append_advances_physical_and_barrier_advances_durable() {
        let (path, file) = test_file("journal-cursors");
        let mut writer = JournalWriter::start(file, 0).unwrap();
        writer.stage(b"one\n".to_vec()).unwrap();
        let written = writer.flush_staged().await.unwrap();
        assert_eq!((written.physical_offset, written.durable_offset), (4, 0));
        let durable = writer.barrier().await.unwrap();
        assert_eq!((durable.physical_offset, durable.durable_offset), (4, 4));
        writer.finish().await.unwrap();

        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"one\n");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn durable_row_reads_do_not_move_the_append_cursor() {
        let (path, file) = test_file("journal-read-cursor");
        let mut writer = JournalWriter::start(file, 0).unwrap();
        let row = b"{\"type\":\"session/end-seed\"}\n".to_vec();
        let locator = JournalRowLocator::new(EventSeq::new(0).unwrap(), 0, &row).unwrap();
        writer.stage(row.clone()).unwrap();
        assert_eq!(writer.flush_staged().await.unwrap().durable_offset, 0);
        assert_eq!(
            writer
                .read_durable_row(locator, CancellationToken::new())
                .await
                .unwrap(),
            row
        );
        writer.stage(b"next\n".to_vec()).unwrap();
        let cursor = writer.flush_staged().await.unwrap();
        assert_eq!(cursor.physical_offset, locator.end().unwrap() + 5);
        writer.finish().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn durable_row_read_checks_cancellation_at_each_64_kib_boundary() {
        fn run(
            label: &str,
            length: usize,
            cancel_at: Option<usize>,
        ) -> (Result<Vec<u8>, ReadCommandError>, Vec<usize>) {
            let (path, mut file) = test_file(label);
            let mut row = vec![b'x'; length - 1];
            row.push(b'\n');
            file.write_all(&row).unwrap();
            file.sync_all().unwrap();
            let locator = JournalRowLocator::new(EventSeq::new(0).unwrap(), 0, &row).unwrap();
            let cancellation = CancellationToken::new();
            let control = cancellation.clone();
            let mut boundaries = Vec::new();
            let result = read_row_with_chunk_observer(
                &file,
                JournalCursor {
                    physical_offset: row.len() as u64,
                    durable_offset: row.len() as u64,
                },
                locator,
                &cancellation,
                |offset| {
                    boundaries.push(offset);
                    if cancel_at == Some(offset) {
                        control.cancel();
                    }
                },
            );
            std::fs::remove_file(path).unwrap();
            (result, boundaries)
        }

        let (exact, exact_boundaries) = run("journal-read-chunk-exact", 64 * 1024, None);
        assert!(exact.is_ok());
        assert_eq!(exact_boundaries, vec![64 * 1024]);

        let (one_over, one_over_boundaries) =
            run("journal-read-chunk-one-over", 64 * 1024 + 1, None);
        assert!(one_over.is_ok());
        assert_eq!(one_over_boundaries, vec![64 * 1024, 64 * 1024 + 1]);

        let (mid_read, mid_boundaries) =
            run("journal-read-chunk-cancel", 64 * 1024 + 2, Some(64 * 1024));
        assert_eq!(mid_read, Err(ReadCommandError::Cancelled));
        assert_eq!(mid_boundaries, vec![64 * 1024]);

        let (at_end, end_boundaries) = run(
            "journal-read-complete-cancel",
            64 * 1024 + 1,
            Some(64 * 1024 + 1),
        );
        assert_eq!(at_end, Err(ReadCommandError::Cancelled));
        assert_eq!(end_boundaries, vec![64 * 1024, 64 * 1024 + 1]);
    }

    #[test]
    fn prune_prefix_shape_and_byte_limits_are_exact() {
        assert!(valid_prune_prefix(b"a\n", 1));
        assert!(valid_prune_prefix(b"a\nb\n", 2));
        assert!(!valid_prune_prefix(b"", 1));
        assert!(!valid_prune_prefix(b"a\n", 0));
        assert!(!valid_prune_prefix(b"a\n", 3));
        assert!(!valid_prune_prefix(b"a", 1));
        assert!(!valid_prune_prefix(b"\n", 1));
        assert!(!valid_prune_prefix(b"a\n\n", 2));
        assert!(!valid_prune_prefix(b"a\nb\nc\n", 2));

        let mut exact_row = vec![b'x'; super::super::jsonl::MAX_JOURNAL_EVENT_LINE_BYTES - 1];
        exact_row.push(b'\n');
        assert!(valid_prune_prefix(&exact_row, 1));
        exact_row.insert(exact_row.len() - 1, b'x');
        assert!(!valid_prune_prefix(&exact_row, 1));

        let half = super::MAX_PRUNE_PREFIX_BYTES / 2;
        let mut exact_pair = vec![b'x'; half - 1];
        exact_pair.push(b'\n');
        exact_pair.extend(std::iter::repeat_n(b'y', half - 1));
        exact_pair.push(b'\n');
        assert_eq!(exact_pair.len(), super::MAX_PRUNE_PREFIX_BYTES);
        assert!(valid_prune_prefix(&exact_pair, 2));
        exact_pair.insert(exact_pair.len() - 1, b'y');
        assert!(!valid_prune_prefix(&exact_pair, 2));
    }

    #[tokio::test]
    async fn invalid_prune_prefix_stays_poisoned_through_finish() {
        let (path, file) = test_file("journal-invalid-prune-prefix");
        let mut writer = JournalWriter::start(file, 0).unwrap();
        assert_eq!(
            writer.stage_prune_prefix(b"\n".to_vec(), 1),
            Err(JournalError::Poisoned)
        );
        assert_eq!(
            writer.stage(b"later\n".to_vec()),
            Err(JournalError::Poisoned)
        );
        assert_eq!(writer.barrier().await, Err(JournalError::Poisoned));
        assert_eq!(writer.finish().await, Err(JournalError::Poisoned));
        assert!(std::fs::read(&path).unwrap().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_prune_pair_is_one_adjacent_owned_writer_command() {
        let (path, file) = test_file("session-prune-pair");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::PrunePrefix);
        let (mut session, result_seq) =
            prunable_session("session-prune-pair", SystemClock, writer).await;

        let mut reservation = session.reservation();
        let raw = reservation
            .read_validated_surface_row(result_seq, CancellationToken::new())
            .await
            .unwrap();
        let replacement = raw
            .prune(ToolResultPruneConfig::new(50, 4, 3).unwrap())
            .unwrap()
            .unwrap();
        let receipt = reservation.append_prune_pair(replacement).unwrap();
        assert_eq!(
            receipt.marker().seq().get() + 1,
            receipt.replacement().seq().get()
        );
        assert_eq!(receipt.outcome().original_code_points, 51);
        assert_eq!(receipt.outcome().pruned_code_points, 46);

        {
            let mut barrier = Box::pin(reservation.flush_barrier());
            poll_fn(|context| match barrier.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("prune prefix unexpectedly settled: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::PrunePrefix
        );
        release.send(()).unwrap();
        reservation.flush_barrier().await.unwrap();
        drop(reservation);
        session.shutdown().await.unwrap();

        assert_eq!(counts.prune_prefix.load(Ordering::SeqCst), 1);
        let events = std::fs::read(&path).unwrap();
        let rows = events
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty())
            .map(|row| serde_json::from_slice::<serde_json::Value>(row).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows[5]["type"], "compaction/prune");
        assert_eq!(rows[6]["type"], "tool/result");
        assert_eq!(rows[6]["sourceEventSeqs"], serde_json::json!([4]));
        assert_eq!(
            rows[6]["surfaceOp"],
            serde_json::json!({"op":"replace","start":4,"end":4})
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn marker_only_failure_keeps_closure_claims_usable() {
        let (path, file) = test_file("session-prune-marker-only");
        let writer = JournalWriter::start(file, 0).unwrap();
        let clock = FailingClock::new();
        let (mut session, result_seq) =
            prunable_session("session-prune-marker-only", clock.clone(), writer).await;
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut reservation = session.reservation();
        let mut closure = reservation
            .claim_batch([
                NewEvent::log(EventKind::step_end(turn, step)),
                NewEvent::log(EventKind::turn_end(turn, TurnEndReason::Completed)),
            ])
            .unwrap();
        let raw = reservation
            .read_validated_surface_row(result_seq, CancellationToken::new())
            .await
            .unwrap();
        let replacement = raw
            .prune(ToolResultPruneConfig::new(50, 4, 3).unwrap())
            .unwrap()
            .unwrap();
        clock.fail_after(1);
        let marker_seq = match reservation.append_prune_pair(replacement).unwrap_err() {
            PrunePairAppendError::MarkerCommitted {
                marker,
                source: AppendError::Clock(_),
            } => marker.seq(),
            error => panic!("unexpected prune failure: {error:?}"),
        };
        assert_eq!(
            reservation.session().state().surface_nodes().last(),
            Some(&result_seq)
        );

        reservation
            .settle_exact_settled(&mut closure[0])
            .await
            .unwrap();
        reservation
            .settle_exact_settled(&mut closure[1])
            .await
            .unwrap();
        reservation.flush_barrier().await.unwrap();
        drop(reservation);
        session.shutdown().await.unwrap();

        let events = std::fs::read(&path).unwrap();
        let rows = events
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty())
            .map(|row| serde_json::from_slice::<serde_json::Value>(row).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            rows[usize::try_from(marker_seq.get()).unwrap()]["type"],
            "compaction/prune"
        );
        assert_eq!(rows.last().unwrap()["type"], "turn/end");
        assert_eq!(
            rows.iter()
                .filter(|row| row["type"] == "tool/result")
                .count(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_prune_pass_handles_multiple_results_in_surface_order_and_is_idempotent() {
        let (path, file) = test_file("session-prune-pass");
        let writer = JournalWriter::start(file, 0).unwrap();
        let (mut session, result_seqs) = prunable_session_with_text_lengths(
            "session-prune-pass",
            SystemClock,
            writer,
            &[8_193, 8_194],
        )
        .await;

        let mut reservation = session.reservation();
        let report = reservation
            .prune_oversized_tool_results(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.replacements(), 2);
        assert_eq!(report.original_code_points(), 16_387);
        assert_eq!(report.pruned_code_points(), 10_318);
        assert_eq!(
            reservation
                .prune_oversized_tool_results(&CancellationToken::new())
                .await
                .unwrap()
                .replacements(),
            0
        );
        let state = reservation.session().state();
        let surface = state.surface_nodes();
        assert!(!surface.contains(&result_seqs[0]));
        assert!(!surface.contains(&result_seqs[1]));
        drop(reservation);
        session.shutdown().await.unwrap();

        let rows = std::fs::read(&path)
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty())
            .map(|row| serde_json::from_slice::<serde_json::Value>(row).unwrap())
            .collect::<Vec<_>>();
        let pairs = rows
            .windows(2)
            .filter(|pair| {
                pair[0]["type"] == "compaction/prune" && pair[1]["type"] == "tool/result"
            })
            .collect::<Vec<_>>();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0][1]["sourceEventSeqs"], serde_json::json!([4]));
        assert_eq!(pairs[1][1]["sourceEventSeqs"], serde_json::json!([6]));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancellation_between_prune_pairs_stops_new_work_and_a_later_pass_converges() {
        let (path, file) = test_file("session-prune-pass-cancel");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::PrunePrefix);
        let (mut session, result_seqs) = prunable_session_with_text_lengths(
            "session-prune-pass-cancel",
            SystemClock,
            writer,
            &[8_193, 8_194],
        )
        .await;
        let cancellation = CancellationToken::new();
        let mut reservation = session.reservation();
        let mut pass = Box::pin(reservation.prune_oversized_tool_results(&cancellation));
        let arrived = tokio::task::spawn_blocking(move || {
            arrived.recv_timeout(Duration::from_secs(10)).unwrap()
        });
        tokio::select! {
            kind = arrived => assert_eq!(kind.unwrap(), FlightKind::PrunePrefix),
            result = &mut pass => panic!("prune pass completed before its first pair was gated: {result:?}"),
        }
        cancellation.cancel();
        release.send(()).unwrap();
        let error = pass.as_mut().await.unwrap_err();
        assert_eq!(
            error.cause(),
            &crate::session::ToolResultPrunePassCause::Cancelled
        );
        assert_eq!(error.progress().replacements(), 1);
        drop(pass);
        let state = reservation.session().state();
        let surface = state.surface_nodes();
        assert!(!surface.contains(&result_seqs[0]));
        assert!(surface.contains(&result_seqs[1]));
        assert_eq!(counts.prune_prefix.load(Ordering::SeqCst), 1);

        let retry = reservation
            .prune_oversized_tool_results(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(retry.replacements(), 1);
        assert_eq!(counts.prune_prefix.load(Ordering::SeqCst), 2);
        assert_eq!(
            reservation
                .prune_oversized_tool_results(&CancellationToken::new())
                .await
                .unwrap()
                .replacements(),
            0
        );
        drop(reservation);
        session.shutdown().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_dropped_row_read_settles_the_same_physical_command_once() {
        let (path, mut file) = test_file("journal-read-cancel-safe");
        let row = b"{\"type\":\"session/end-seed\"}\n".to_vec();
        file.write_all(&row).unwrap();
        file.sync_all().unwrap();
        let offset = row.len() as u64;
        let locator = JournalRowLocator::new(EventSeq::new(0).unwrap(), 0, &row).unwrap();
        let (sender, mut receiver) = tokio_mpsc::channel(COMMAND_CAPACITY);
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let reads = Arc::new(AtomicUsize::new(0));
        let worker_reads = Arc::clone(&reads);
        let join = thread::spawn(move || {
            let mut cursor = JournalCursor {
                physical_offset: offset,
                durable_offset: offset,
            };
            while let Some(command) = receiver.blocking_recv() {
                match command {
                    Command::ReadRow {
                        locator,
                        cancellation,
                        ack,
                    } => {
                        worker_reads.fetch_add(1, Ordering::SeqCst);
                        arrived_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        let _ = ack.send(read_row(&file, cursor, locator, &cancellation));
                    }
                    Command::Finish { ack } => {
                        let result = barrier_file(&mut file, &mut cursor);
                        drop(file);
                        let _ = ack.send(result);
                        return;
                    }
                    Command::Append { bytes, ack } => {
                        let _ = ack.send(append_bytes(&mut file, &mut cursor, &bytes));
                    }
                    Command::AppendPrunePrefix { bytes, rows, ack } => {
                        let result = if valid_prune_prefix(&bytes, rows) {
                            append_bytes(&mut file, &mut cursor, &bytes)
                        } else {
                            Err(JournalError::Poisoned)
                        };
                        let _ = ack.send(result);
                    }
                    Command::Barrier { ack } => {
                        let _ = ack.send(barrier_file(&mut file, &mut cursor));
                    }
                }
            }
        });
        let mut writer = JournalWriter::from_running(
            sender,
            join,
            JournalCursor {
                physical_offset: offset,
                durable_offset: offset,
            },
        );
        {
            let mut read = Box::pin(writer.read_durable_row(locator, CancellationToken::new()));
            poll_fn(|context| match read.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("read unexpectedly completed: {result:?}"),
            })
            .await;
        }
        arrived_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(
            writer
                .read_durable_row(locator, CancellationToken::new())
                .await
                .unwrap(),
            row
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        writer.finish().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn finish_cancels_an_abandoned_row_read_and_joins_once() {
        let (path, mut file) = test_file("journal-read-finish-cancel");
        let row = b"{\"type\":\"session/end-seed\"}\n".to_vec();
        file.write_all(&row).unwrap();
        file.sync_all().unwrap();
        let offset = row.len() as u64;
        let locator = JournalRowLocator::new(EventSeq::new(0).unwrap(), 0, &row).unwrap();
        let (sender, mut receiver) = tokio_mpsc::channel(COMMAND_CAPACITY);
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let reads = Arc::new(AtomicUsize::new(0));
        let finishes = Arc::new(AtomicUsize::new(0));
        let worker_reads = Arc::clone(&reads);
        let worker_finishes = Arc::clone(&finishes);
        let join = thread::spawn(move || {
            let mut cursor = JournalCursor {
                physical_offset: offset,
                durable_offset: offset,
            };
            while let Some(command) = receiver.blocking_recv() {
                match command {
                    Command::ReadRow {
                        locator,
                        cancellation,
                        ack,
                    } => {
                        worker_reads.fetch_add(1, Ordering::SeqCst);
                        arrived_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        let _ = ack.send(read_row(&file, cursor, locator, &cancellation));
                    }
                    Command::Finish { ack } => {
                        worker_finishes.fetch_add(1, Ordering::SeqCst);
                        let result = barrier_file(&mut file, &mut cursor);
                        drop(file);
                        let _ = ack.send(result);
                        return;
                    }
                    Command::Append { bytes, ack } => {
                        let _ = ack.send(append_bytes(&mut file, &mut cursor, &bytes));
                    }
                    Command::AppendPrunePrefix { bytes, rows, ack } => {
                        let result = if valid_prune_prefix(&bytes, rows) {
                            append_bytes(&mut file, &mut cursor, &bytes)
                        } else {
                            Err(JournalError::Poisoned)
                        };
                        let _ = ack.send(result);
                    }
                    Command::Barrier { ack } => {
                        let _ = ack.send(barrier_file(&mut file, &mut cursor));
                    }
                }
            }
        });
        let mut writer = JournalWriter::from_running(
            sender,
            join,
            JournalCursor {
                physical_offset: offset,
                durable_offset: offset,
            },
        );
        {
            let mut read = Box::pin(writer.read_durable_row(locator, CancellationToken::new()));
            poll_fn(|context| match read.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("read unexpectedly completed: {result:?}"),
            })
            .await;
        }
        arrived_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut finish = Box::pin(writer.finish());
        poll_fn(|context| match finish.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(result) => panic!("finish unexpectedly completed: {result:?}"),
        })
        .await;
        release_tx.send(()).unwrap();
        finish.await.unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(finishes.load(Ordering::SeqCst), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_pre_cancelled_row_read_starts_no_command_and_keeps_the_writer_usable() {
        let (path, mut file) = test_file("journal-read-pre-cancel");
        let row = b"{\"type\":\"session/end-seed\"}\n".to_vec();
        file.write_all(&row).unwrap();
        file.sync_all().unwrap();
        let offset = row.len() as u64;
        let locator = JournalRowLocator::new(EventSeq::new(0).unwrap(), 0, &row).unwrap();
        let mut writer = JournalWriter::start(file, offset).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert_eq!(
            writer.read_durable_row(locator, cancellation).await,
            Err(JournalReadError::Cancelled)
        );
        assert!(writer.read_flight.is_none());
        writer.stage(b"later\n".to_vec()).unwrap();
        assert_eq!(writer.barrier().await.unwrap().durable_offset, offset + 6);
        writer.finish().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn durable_corruption_remains_sticky_after_the_first_barrier_reports_it() {
        let (path, file) = test_file("session-read-corruption-sticky");
        let writer = JournalWriter::start(file, 0).unwrap();
        let mut session =
            Session::new_active_for_test("session-read-corruption-sticky", SystemClock, writer)
                .unwrap();
        session.latch_durable_corruption();
        assert_eq!(
            session.flush_barrier().await,
            Err(BarrierError::Append(AppendError::DurablePoisoned))
        );
        assert_eq!(
            session
                .append_settled(NewEvent::log(EventKind::turn_start(
                    TurnId::new(1).unwrap(),
                )))
                .await,
            Err(AppendError::DurablePoisoned)
        );
        assert!(session.shutdown().await.is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_row_digest_mismatch_poison_is_not_lost_when_the_read_returns() {
        let (path, mut file) = test_file("journal-read-corrupt");
        let actual = b"{\"x\":1}\n";
        file.write_all(actual).unwrap();
        file.sync_all().unwrap();
        let mut writer = JournalWriter::start(file, actual.len() as u64).unwrap();
        let wrong = JournalRowLocator::new(EventSeq::new(0).unwrap(), 0, b"{\"x\":2}\n").unwrap();
        assert_eq!(
            writer
                .read_durable_row(wrong, CancellationToken::new())
                .await,
            Err(JournalReadError::Writer(JournalError::Poisoned))
        );
        assert_eq!(writer.barrier().await, Err(JournalError::Poisoned));
        assert_eq!(writer.finish().await, Err(JournalError::Poisoned));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_dropped_flush_wait_leaves_the_owned_flight_settleable() {
        let (path, file) = test_file("journal-cancel-safe");
        drop(file);
        let GatedWriter {
            mut writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Append);
        writer.stage(vec![b'x'; 64 * 1024]).unwrap();
        {
            let mut wait = Box::pin(writer.flush_staged());
            poll_fn(|context| match wait.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("flush unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Append
        );
        release.send(()).unwrap();
        let cursor = writer.barrier().await.unwrap();
        assert_eq!(cursor.durable_offset, 64 * 1024);
        writer.finish().await.unwrap();
        assert_eq!(counts.append.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 * 1024);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_dropped_finish_wait_settles_the_same_finish_command() {
        let (path, file) = test_file("journal-finish-cancel-safe");
        drop(file);
        let GatedWriter {
            mut writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Finish);
        {
            let mut wait = Box::pin(writer.finish());
            poll_fn(|context| match wait.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("finish unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Finish
        );
        release.send(()).unwrap();
        assert_eq!(writer.finish().await.unwrap().durable_offset, 0);
        assert_eq!(counts.finish.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancelled_batch_flush_keeps_the_exact_prepared_operation_until_resumed() {
        let (path, file) = test_file("session-batch-cancel-safe");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            ..
        } = gated_writer(&path, FlightKind::Append);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut session = Session::new_active_for_test(
            "session-batch-cancel-safe",
            CountingClock(Arc::clone(&calls)),
            writer,
        )
        .unwrap();
        let mut observer = session.attach_ui_observer_for_test(2).unwrap();

        let first = session
            .append_settled(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .await
            .unwrap();
        assert_eq!(first.seq(), EventSeq::new(0).unwrap());
        assert_eq!(observer.recv().await.unwrap().seq, first.seq());

        {
            let mut second = Box::pin(session.append_settled(NewEvent::log(
                EventKind::step_start(TurnId::new(1).unwrap(), StepId::new(1).unwrap()),
            )));
            poll_fn(|context| match second.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("second append unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Append
        );
        assert_eq!(session.logical_event_count(), 1);
        assert_eq!(session.next_seq(), EventSeq::new(1).ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(observer.try_recv().is_err());

        release.send(()).unwrap();
        session.flush_barrier().await.unwrap();
        assert_eq!(session.logical_event_count(), 2);
        assert_eq!(session.next_seq(), EventSeq::new(2).ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            observer.recv().await.unwrap().seq,
            EventSeq::new(1).unwrap()
        );
        session.shutdown().await.unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
        let second_line = bytes.split(|byte| *byte == b'\n').nth(1).unwrap();
        let event: serde_json::Value = serde_json::from_slice(second_line).unwrap();
        assert_eq!(event["type"], "step/start");
        assert_eq!(event["data"]["turn"], 1);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancelled_claim_settlement_resumes_the_same_candidate_once() {
        let (path, file) = test_file("session-claim-cancel-safe");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Append);
        let mut session =
            Session::new_active_for_test("session-claim-cancel-safe", SystemClock, writer).unwrap();
        session
            .append_settled(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .await
            .unwrap();

        let mut reservation = session.reservation();
        let mut claims = reservation
            .claim_batch([NewEvent::log(EventKind::step_start(
                TurnId::new(1).unwrap(),
                StepId::new(1).unwrap(),
            ))])
            .unwrap();
        let mut claim = claims.remove(0);
        {
            let mut settlement = Box::pin(reservation.settle_exact_settled(&mut claim));
            poll_fn(|context| match settlement.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("claim unexpectedly settled: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Append
        );
        assert_eq!(
            reservation.release(&mut claim),
            Err(crate::session::AppendError::NeedsAppendSettle)
        );
        assert_eq!(
            reservation.rebind_claim_fallback(
                &mut claim,
                NewEvent::log(EventKind::step_start(
                    TurnId::new(1).unwrap(),
                    StepId::new(1).unwrap(),
                )),
            ),
            Err(crate::session::AppendError::NeedsAppendSettle)
        );

        release.send(()).unwrap();
        let receipt = reservation.settle_exact_settled(&mut claim).await.unwrap();
        assert_eq!(receipt.seq(), EventSeq::new(1).unwrap());
        reservation.flush_barrier().await.unwrap();
        drop(reservation);
        session.shutdown().await.unwrap();

        assert_eq!(counts.append.load(Ordering::SeqCst), 2);
        assert_eq!(counts.finish.load(Ordering::SeqCst), 1);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn durable_settlement_uses_unclaimed_global_space_for_a_preferred_result() {
        let (path, file) = test_file("session-preferred-global-space");
        let writer = JournalWriter::start(file, 0).unwrap();
        let mut session =
            Session::new_active_for_test("session-preferred-global-space", SystemClock, writer)
                .unwrap();
        session
            .append_settled(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .await
            .unwrap();
        let mut reservation = session.reservation();
        let mut claims = reservation
            .claim_batch([NewEvent::log(EventKind::EndSeed)])
            .unwrap();
        let mut claim = claims.remove(0);
        let preferred = NewEvent::log(EventKind::TodoWrite {
            todos: vec![TodoItem {
                content: "preferred result is larger than its tiny fallback".repeat(32),
                status: TodoStatus::Pending,
            }],
        });

        let settlement = reservation
            .settle_settled(&mut claim, preferred)
            .await
            .unwrap();
        assert!(matches!(settlement, ClaimedAppend::Preferred(_)));
        reservation.flush_barrier().await.unwrap();
        drop(reservation);
        session.shutdown().await.unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let event: serde_json::Value = serde_json::from_slice(
            bytes
                .split(|byte| *byte == b'\n')
                .nth(1)
                .expect("the preferred event should be written"),
        )
        .unwrap();
        assert_eq!(event["type"], "todo/write");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn barrier_reports_a_sticky_observer_fault_after_the_event_is_durable() {
        let (path, file) = test_file("session-observer-barrier");
        let writer = JournalWriter::start(file, 0).unwrap();
        let mut session =
            Session::new_active_for_test("session-observer-barrier", SystemClock, writer).unwrap();
        let observer = session.attach_ui_observer_for_test(1).unwrap();
        observer.fail_next_projection_for_test();

        let receipt = session
            .append_settled(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .await
            .unwrap();
        assert!(receipt.observer_faulted());
        assert_eq!(
            session.flush_barrier().await,
            Err(BarrierError::ObserverUnavailable)
        );
        session.shutdown().await.unwrap();
        assert_eq!(
            std::fs::read(&path)
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_dropped_barrier_wait_settles_the_same_barrier_command() {
        let (path, file) = test_file("journal-barrier-cancel-safe");
        drop(file);
        let GatedWriter {
            mut writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Barrier);
        writer.stage(b"fact\n".to_vec()).unwrap();
        assert_eq!(writer.flush_staged().await.unwrap().durable_offset, 0);
        {
            let mut barrier = Box::pin(writer.barrier());
            poll_fn(|context| match barrier.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("barrier unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Barrier
        );
        release.send(()).unwrap();
        assert_eq!(writer.barrier().await.unwrap().durable_offset, 5);
        assert_eq!(counts.barrier.load(Ordering::SeqCst), 1);
        writer.finish().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancelled_session_finish_stays_owned_and_is_sent_once() {
        let (path, file) = test_file("session-finish-cancel-safe");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Finish);
        let mut session =
            Session::new_active_for_test("session-finish-cancel-safe", SystemClock, writer)
                .unwrap();
        session
            .append_settled(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .await
            .unwrap();
        session.flush_barrier().await.unwrap();

        {
            let mut shutdown = Box::pin(session.shutdown());
            poll_fn(|context| match shutdown.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("shutdown unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Finish
        );
        release.send(()).unwrap();
        session.shutdown().await.unwrap();

        assert_eq!(counts.append.load(Ordering::SeqCst), 1);
        assert_eq!(counts.finish.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(&path)
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_barrier_cannot_reopen_a_cancelled_session_finish() {
        let (path, file) = test_file("session-finish-barrier-order");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Finish);
        let mut session =
            Session::new_active_for_test("session-finish-barrier-order", SystemClock, writer)
                .unwrap();

        {
            let mut shutdown = Box::pin(session.shutdown());
            poll_fn(|context| match shutdown.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("shutdown unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Finish
        );
        release.send(()).unwrap();
        assert_eq!(
            session.flush_barrier().await,
            Err(BarrierError::Storage(
                crate::session::StoreError::WriterStopped
            ))
        );
        session.shutdown().await.unwrap();
        assert_eq!(counts.finish.load(Ordering::SeqCst), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn shutdown_reports_an_abandoned_invalid_append_after_joining() {
        let (path, file) = test_file("session-invalid-pending-shutdown");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Append);
        let mut session =
            Session::new_active_for_test("session-invalid-pending-shutdown", SystemClock, writer)
                .unwrap();
        session
            .append_settled(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .await
            .unwrap();

        {
            let mut invalid = Box::pin(session.append_settled(NewEvent::log(EventKind::turn_end(
                TurnId::new(2).unwrap(),
                TurnEndReason::Completed,
            ))));
            poll_fn(|context| match invalid.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("invalid append unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Append
        );
        release.send(()).unwrap();
        let error = session.shutdown().await.unwrap_err();
        assert!(matches!(
            error,
            crate::session::SessionIoError::Append(crate::session::AppendError::Validation(_))
        ));
        assert_eq!(counts.append.load(Ordering::SeqCst), 1);
        assert_eq!(counts.finish.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(&path)
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn cancelled_barrier_retains_the_first_pending_append_error() {
        let (path, file) = test_file("session-barrier-error-cancel-safe");
        drop(file);
        let GatedWriter {
            writer,
            arrived,
            release,
            counts,
        } = gated_writer(&path, FlightKind::Barrier);
        let mut session =
            Session::new_active_for_test("session-barrier-error-cancel-safe", SystemClock, writer)
                .unwrap();
        let invalid = Session::prepare_event(NewEvent::log(EventKind::turn_end(
            TurnId::new(1).unwrap(),
            TurnEndReason::Completed,
        )))
        .unwrap();
        let SessionMode::Durable {
            pending_operation, ..
        } = &mut session.mode
        else {
            panic!("test session must be durable");
        };
        *pending_operation = Some(crate::session::PendingDurableOperation {
            prepared: invalid,
            protected_events: 0,
            protected_row_bytes: 0,
            owner: crate::session::DurableOperationOwner::Ordinary,
        });

        {
            let mut barrier = Box::pin(session.flush_barrier());
            poll_fn(|context| match barrier.as_mut().poll(context) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => panic!("barrier unexpectedly completed: {result:?}"),
            })
            .await;
        }
        assert_eq!(
            arrived.recv_timeout(Duration::from_secs(1)).unwrap(),
            FlightKind::Barrier
        );
        release.send(()).unwrap();
        assert!(matches!(
            session.flush_barrier().await,
            Err(BarrierError::Append(
                crate::session::AppendError::Validation(_)
            ))
        ));
        assert_eq!(counts.barrier.load(Ordering::SeqCst), 1);
        session.shutdown().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    struct GatedWriter {
        writer: JournalWriter,
        arrived: mpsc::Receiver<FlightKind>,
        release: mpsc::Sender<()>,
        counts: Arc<CommandCounts>,
    }

    #[derive(Default)]
    struct CommandCounts {
        append: AtomicUsize,
        prune_prefix: AtomicUsize,
        barrier: AtomicUsize,
        finish: AtomicUsize,
    }

    struct CompletedCommand {
        kind: FlightKind,
        result: Result<JournalCursor, JournalError>,
        ack: oneshot::Sender<Result<JournalCursor, JournalError>>,
        finish: bool,
    }

    impl CommandCounts {
        fn increment(&self, kind: FlightKind) {
            match kind {
                FlightKind::Append => &self.append,
                FlightKind::PrunePrefix => &self.prune_prefix,
                FlightKind::Barrier => &self.barrier,
                FlightKind::Finish => &self.finish,
            }
            .fetch_add(1, Ordering::SeqCst);
        }
    }

    fn gated_writer(path: &PathBuf, target: FlightKind) -> GatedWriter {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let (sender, mut receiver) = tokio_mpsc::channel(COMMAND_CAPACITY);
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let counts = Arc::new(CommandCounts::default());
        let worker_counts = Arc::clone(&counts);
        let join = thread::spawn(move || {
            let mut cursor = JournalCursor {
                physical_offset: 0,
                durable_offset: 0,
            };
            let mut gated = false;
            while let Some(command) = receiver.blocking_recv() {
                let completed = match command {
                    Command::Append { bytes, ack } => CompletedCommand {
                        kind: FlightKind::Append,
                        result: append_bytes(&mut file, &mut cursor, &bytes),
                        ack,
                        finish: false,
                    },
                    Command::AppendPrunePrefix { bytes, rows, ack } => CompletedCommand {
                        kind: FlightKind::PrunePrefix,
                        result: if valid_prune_prefix(&bytes, rows) {
                            append_bytes(&mut file, &mut cursor, &bytes)
                        } else {
                            Err(JournalError::Poisoned)
                        },
                        ack,
                        finish: false,
                    },
                    Command::Barrier { ack } => CompletedCommand {
                        kind: FlightKind::Barrier,
                        result: barrier_file(&mut file, &mut cursor),
                        ack,
                        finish: false,
                    },
                    Command::ReadRow {
                        locator,
                        cancellation,
                        ack,
                    } => {
                        let _ = ack.send(read_row(&file, cursor, locator, &cancellation));
                        continue;
                    }
                    Command::Finish { ack } => CompletedCommand {
                        kind: FlightKind::Finish,
                        result: barrier_file(&mut file, &mut cursor),
                        ack,
                        finish: true,
                    },
                };
                worker_counts.increment(completed.kind);
                if !gated && completed.kind == target {
                    arrived_tx.send(completed.kind).unwrap();
                    if release_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                        return;
                    }
                    gated = true;
                }
                let _ = completed.ack.send(completed.result);
                if completed.finish {
                    return;
                }
            }
        });
        GatedWriter {
            writer: JournalWriter::from_running(
                sender,
                join,
                JournalCursor {
                    physical_offset: 0,
                    durable_offset: 0,
                },
            ),
            arrived: arrived_rx,
            release: release_tx,
            counts,
        }
    }

    #[derive(Clone)]
    struct CountingClock(Arc<AtomicUsize>);

    impl Clock for CountingClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            let next = self.0.fetch_add(1, Ordering::SeqCst);
            UnixMillis::new(i64::try_from(next).unwrap()).map_err(|_| ClockError::new("clock"))
        }
    }

    #[derive(Clone)]
    struct FailingClock {
        calls: Arc<AtomicUsize>,
        fail_at: Arc<AtomicUsize>,
    }

    impl FailingClock {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                fail_at: Arc::new(AtomicUsize::new(usize::MAX)),
            }
        }

        fn fail_after(&self, successful_calls: usize) {
            self.fail_at.store(
                self.calls.load(Ordering::SeqCst) + successful_calls,
                Ordering::SeqCst,
            );
        }
    }

    impl Clock for FailingClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == self.fail_at.load(Ordering::SeqCst) {
                return Err(ClockError::new("injected clock failure"));
            }
            UnixMillis::new(i64::try_from(call).unwrap()).map_err(|_| ClockError::new("clock"))
        }
    }

    async fn prunable_session(
        id: &str,
        clock: impl Clock + 'static,
        writer: JournalWriter,
    ) -> (Session, EventSeq) {
        let (session, mut results) =
            prunable_session_with_text_lengths(id, clock, writer, &[51]).await;
        (session, results.remove(0))
    }

    async fn prunable_session_with_text_lengths(
        id: &str,
        clock: impl Clock + 'static,
        writer: JournalWriter,
        text_lengths: &[usize],
    ) -> (Session, Vec<EventSeq>) {
        let mut session = Session::new_active_for_test(id, clock, writer).unwrap();
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        session
            .append_settled(NewEvent::log(EventKind::turn_start(turn)))
            .await
            .unwrap();
        session
            .append_settled(NewEvent::log(EventKind::step_start(turn, step)))
            .await
            .unwrap();
        let calls = text_lengths
            .iter()
            .enumerate()
            .map(|(index, _)| {
                ContentBlock::tool_call(format!("call-{}", index + 1), "read", "{}").unwrap()
            })
            .collect();
        let assistant = Message::assistant("assistant-1", calls, "mock", "mock-model").unwrap();
        session
            .append_settled(NewEvent::surface(
                EventKind::assistant_message(turn, step, assistant),
                SurfaceIntent::append(),
            ))
            .await
            .unwrap();
        let mut results = Vec::with_capacity(text_lengths.len());
        for (index, text_length) in text_lengths.iter().copied().enumerate() {
            let call_id = format!("call-{}", index + 1);
            let call = session
                .append_settled(NewEvent::log(EventKind::tool_call(
                    turn,
                    step,
                    call_id.clone(),
                    "read",
                    "{}",
                )))
                .await
                .unwrap();
            let result = Message::tool_result(
                format!("result-{}", index + 1),
                call_id,
                vec![ContentBlock::text("x".repeat(text_length)).unwrap()],
                false,
            )
            .unwrap();
            let result = session
                .append_settled(NewEvent::surface(
                    EventKind::tool_result(turn, step, result),
                    SurfaceIntent::append().with_sources(vec![call.seq()]),
                ))
                .await
                .unwrap();
            results.push(result.seq());
        }
        (session, results)
    }

    fn test_file(label: &str) -> (PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!("dsh-{label}-{}", uuid::Uuid::new_v4()));
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (path, file)
    }
}
