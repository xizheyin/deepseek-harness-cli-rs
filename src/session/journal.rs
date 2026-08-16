//! One-owner blocking journal writer with cancellation-safe async waits.

use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    thread,
};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

const COMMAND_CAPACITY: usize = 1;

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
    Barrier {
        ack: oneshot::Sender<Result<JournalCursor, JournalError>>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlightKind {
    Append,
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
    pending: Option<Vec<u8>>,
    flight: Option<Flight>,
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
            .field("pending", &self.pending.as_ref().map(Vec::len))
            .field("flight", &self.flight.is_some())
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
        self.pending = Some(bytes);
        Ok(())
    }

    pub(crate) fn ensure_stageable(&self) -> Result<(), JournalError> {
        self.ensure_usable()?;
        if self.flight.is_some() {
            return Err(JournalError::FlightInProgress);
        }
        if self.pending.is_some() {
            return Err(JournalError::AlreadyStaged);
        }
        Ok(())
    }

    /// Send and settle the owner-held staged bytes.
    ///
    /// Cancelling this wait leaves either `pending` or `flight` inside `self`,
    /// so a later barrier/shutdown can continue the same operation.
    pub(crate) async fn flush_staged(&mut self) -> Result<JournalCursor, JournalError> {
        self.ensure_usable()?;
        if let Some(kind) = self.flight.as_ref().map(|flight| flight.kind) {
            let cursor = self.settle_flight().await?;
            if kind == FlightKind::Append {
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
        let bytes = self.pending.take().ok_or(JournalError::NothingStaged)?;
        let (ack, receiver) = oneshot::channel();
        permit.send(Command::Append { bytes, ack });
        self.flight = Some(Flight {
            kind: FlightKind::Append,
            ack: receiver,
        });
        self.settle_flight().await
    }

    /// Settle any command already owned by this writer before staging bytes.
    pub(crate) async fn settle_before_stage(&mut self) -> Result<JournalCursor, JournalError> {
        self.ensure_usable()?;
        if self.pending.is_some() {
            self.flush_staged().await
        } else {
            self.settle_flight().await
        }
    }

    pub(crate) async fn barrier(&mut self) -> Result<JournalCursor, JournalError> {
        self.ensure_usable()?;
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
        let settle = if self.pending.is_some() {
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
            FlightKind::Append => return Err(JournalError::WriterStopped),
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

impl Drop for JournalWriter {
    fn drop(&mut self) {
        // Abnormal fallback: dropping the sole sender lets the worker finish
        // already queued work and then release its fd/flock. Pending unsent
        // bytes are deliberately not claimed durable.
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
        io::Read,
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

    use crate::session::{
        BarrierError, ClaimedAppend, Clock, ClockError, EventKind, EventSeq, NewEvent, Session,
        SessionMode, StepId, SystemClock, TodoItem, TodoStatus, TurnEndReason, TurnId, UnixMillis,
    };

    use super::{
        COMMAND_CAPACITY, Command, FlightKind, JournalCursor, JournalError, JournalWriter,
        append_bytes, barrier_file,
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
                    Command::Barrier { ack } => CompletedCommand {
                        kind: FlightKind::Barrier,
                        result: barrier_file(&mut file, &mut cursor),
                        ack,
                        finish: false,
                    },
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
