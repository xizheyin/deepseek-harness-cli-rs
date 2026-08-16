//! Owned prepare/warn/commit lifecycle for one durable session resume.

use std::{
    fs::File,
    io::{Seek as _, SeekFrom},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
};

use cap_std::fs::Dir;
use tokio::sync::oneshot;

use crate::{resident_credit::ResidentCreditPool, workspace_authority::WorkspaceAuthority};

use super::{
    Clock, EventSeq, Session, SessionId, StoreError, UnixMillis,
    journal::{
        JournalCursor, JournalHandoff, JournalWriter, commit_recovery_suffix, handoff_channel,
    },
    jsonl::encode_event_line,
    path_policy::RootPlan,
    recovery::{
        ColdScan, RecoveredSeed, RecoveryPlan, RecoveryReport, scan_jsonl_validating_header,
    },
    store::{lock_error, named_journal_still_matches, validate_opened_journal},
};

const RESUME_COMMAND_CAPACITY: usize = 1;

pub(crate) struct PreparingResume {
    core: Option<ResumeCore>,
    ready: Option<oneshot::Receiver<Result<RecoveryReport, StoreError>>>,
    ready_result: Option<Result<RecoveryReport, StoreError>>,
}

pub(crate) struct PreparedSession {
    core: Option<ResumeCore>,
    report: RecoveryReport,
    commit: CommitState,
}

impl Drop for PreparedSession {
    fn drop(&mut self) {
        // A successful commit ack contains the only handoff sender. Close that
        // sender before ResumeCore joins the worker; otherwise the worker can
        // be waiting in JournalInbox while this thread waits for it to exit.
        drop(std::mem::replace(&mut self.commit, CommitState::Closed));
        drop(self.core.take());
    }
}

pub(crate) struct RecoveredSession {
    session: Session,
    workspace: WorkspaceAuthority,
}

impl RecoveredSession {
    pub(crate) fn into_parts(self) -> (Session, WorkspaceAuthority) {
        (self.session, self.workspace)
    }
}

struct ResumeCore {
    cancel: Arc<AtomicBool>,
    control: Option<SyncSender<ResumeCommand>>,
    exit: Option<oneshot::Receiver<()>>,
    join: Option<thread::JoinHandle<()>>,
    clock: Option<Box<dyn Clock>>,
}

impl ResumeCore {
    async fn settle_exit(&mut self) -> Result<(), StoreError> {
        if let Some(exit) = self.exit.as_mut() {
            let _ = exit.await;
        }
        self.exit.take();
        self.control.take();
        if self.join.take().is_some_and(|join| join.join().is_err()) {
            return Err(StoreError::WriterStopped);
        }
        Ok(())
    }
}

impl Drop for ResumeCore {
    fn drop(&mut self) {
        // Abnormal fallback only. Production paths call the explicit async
        // shutdown methods so a current-thread runtime is never blocked here.
        self.cancel.store(true, Ordering::Release);
        self.control.take();
        self.exit.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

enum CommitState {
    Idle,
    Flight(oneshot::Receiver<Result<CommittedParts, StoreError>>),
    Ready(Result<CommittedParts, StoreError>),
    CleanupWriter(JournalWriter),
    Closed,
}

enum ResumeCommand {
    Commit {
        ack: oneshot::Sender<Result<CommittedParts, StoreError>>,
    },
    Abort,
}

struct CommittedParts {
    seed: RecoveredSeed,
    workspace: WorkspaceAuthority,
    handoff: JournalHandoff,
    cursor: JournalCursor,
    resident_pool: ResidentCreditPool,
}

struct LockedResume {
    root: Arc<Dir>,
    filename: String,
    file: Option<File>,
    workspace: Option<WorkspaceAuthority>,
    prepared: PreparedWork,
    resident_pool: ResidentCreditPool,
}

struct PreparedWork {
    token: PrepareToken,
    suffix: Vec<u8>,
    report: RecoveryReport,
    seed: RecoveredSeed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrepareToken {
    journal_device: u64,
    journal_inode: u64,
    physical_bytes: u64,
    valid_bytes: u64,
    physical_sha256: [u8; 32],
    suffix_sha256: [u8; 32],
}

pub(super) fn begin(
    root: RootPlan,
    filename: String,
    id: SessionId,
    asserted_workspace: Option<PathBuf>,
    clock: Box<dyn Clock>,
) -> Result<PreparingResume, StoreError> {
    let recovery_time = clock.now().map_err(|_| StoreError::Io)?;
    if recovery_time.get() < 0 {
        return Err(StoreError::Io);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let resident_pool = ResidentCreditPool::for_durable_session();
    let worker_cancel = Arc::clone(&cancel);
    let (control, commands) = mpsc::sync_channel(RESUME_COMMAND_CAPACITY);
    let (ready_ack, ready) = oneshot::channel();
    let (exit_ack, exit) = oneshot::channel();
    let join = thread::Builder::new()
        .name("dsh-session-resume".to_owned())
        .spawn(move || {
            resume_worker(
                root,
                filename,
                id,
                asserted_workspace,
                recovery_time,
                worker_cancel,
                resident_pool,
                ready_ack,
                commands,
            );
            let _ = exit_ack.send(());
        })
        .map_err(|_| StoreError::WriterStopped)?;
    Ok(PreparingResume {
        core: Some(ResumeCore {
            cancel,
            control: Some(control),
            exit: Some(exit),
            join: Some(join),
            clock: Some(clock),
        }),
        ready: Some(ready),
        ready_result: None,
    })
}

impl PreparingResume {
    pub(crate) async fn wait_ready(&mut self) -> Result<(), StoreError> {
        if let Some(result) = self.ready_result.as_ref() {
            return result.as_ref().map(|_| ()).map_err(|error| *error);
        }
        let result = self
            .ready
            .as_mut()
            .ok_or(StoreError::WriterStopped)?
            .await
            .unwrap_or(Err(StoreError::WriterStopped));
        self.ready.take();
        self.ready_result = Some(result);
        self.ready_result
            .as_ref()
            .ok_or(StoreError::WriterStopped)?
            .as_ref()
            .map(|_| ())
            .map_err(|error| *error)
    }

    pub(crate) fn finish(mut self) -> Result<PreparedSession, StoreError> {
        let result = self.ready_result.take().ok_or(StoreError::WriterStopped)?;
        let report = result?;
        Ok(PreparedSession {
            core: self.core.take(),
            report,
            commit: CommitState::Idle,
        })
    }

    pub(crate) async fn cancel_and_shutdown(&mut self) -> Result<(), StoreError> {
        let core = self.core.as_mut().ok_or(StoreError::WriterStopped)?;
        core.cancel.store(true, Ordering::Release);
        if let Some(control) = core.control.as_ref() {
            match control.try_send(ResumeCommand::Abort) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        core.settle_exit().await
    }
}

impl PreparedSession {
    pub(crate) fn recovery_report(&self) -> &RecoveryReport {
        &self.report
    }

    pub(crate) fn begin_commit(&mut self) -> Result<(), StoreError> {
        if !matches!(self.commit, CommitState::Idle) {
            return Err(StoreError::WriterStopped);
        }
        let control = self
            .core
            .as_ref()
            .and_then(|core| core.control.as_ref())
            .ok_or(StoreError::WriterStopped)?;
        let (ack, receiver) = oneshot::channel();
        self.commit = CommitState::Flight(receiver);
        let command = ResumeCommand::Commit { ack };
        let result = control.try_send(command);
        if result.is_err() {
            self.commit = CommitState::Idle;
            return Err(StoreError::WriterStopped);
        }
        Ok(())
    }

    pub(crate) async fn wait_commit(&mut self) -> Result<(), StoreError> {
        match &self.commit {
            CommitState::Ready(result) => {
                return result.as_ref().map(|_| ()).map_err(|error| *error);
            }
            CommitState::Idle | CommitState::CleanupWriter(_) | CommitState::Closed => {
                return Err(StoreError::WriterStopped);
            }
            CommitState::Flight(_) => {}
        }
        let result = match &mut self.commit {
            CommitState::Flight(receiver) => {
                receiver.await.unwrap_or(Err(StoreError::WriterStopped))
            }
            _ => return Err(StoreError::WriterStopped),
        };
        self.commit = CommitState::Ready(result);
        match &self.commit {
            CommitState::Ready(result) => result.as_ref().map(|_| ()).map_err(|error| *error),
            _ => Err(StoreError::WriterStopped),
        }
    }

    pub(crate) fn finish_commit(mut self) -> Result<RecoveredSession, StoreError> {
        let result = match std::mem::replace(&mut self.commit, CommitState::Closed) {
            CommitState::Ready(result) => result,
            state => {
                self.commit = state;
                return Err(StoreError::WriterStopped);
            }
        };
        let committed = result?;
        let mut core = self.core.take().ok_or(StoreError::WriterStopped)?;
        core.control.take();
        core.exit.take();
        let join = core.join.take().ok_or(StoreError::WriterStopped)?;
        let clock = core.clock.take().ok_or(StoreError::WriterStopped)?;
        let resident_pool = committed.resident_pool;
        let writer = JournalWriter::from_handoff(
            committed.handoff,
            join,
            committed.cursor,
            resident_pool.clone(),
        );
        let session = Session::new_recovered(committed.seed, clock, writer, resident_pool);
        Ok(RecoveredSession {
            session,
            workspace: committed.workspace,
        })
    }

    pub(crate) async fn cancel_and_shutdown(&mut self) -> Result<(), StoreError> {
        if matches!(self.commit, CommitState::Idle) {
            let core = self.core.as_mut().ok_or(StoreError::WriterStopped)?;
            core.cancel.store(true, Ordering::Release);
            if let Some(control) = core.control.as_ref() {
                let _ = control.try_send(ResumeCommand::Abort);
            }
            return core.settle_exit().await;
        }
        if matches!(self.commit, CommitState::Flight(_)) {
            let _ = self.wait_commit().await;
        }
        if matches!(self.commit, CommitState::Ready(Ok(_))) {
            self.install_cleanup_writer()?;
        }
        if let CommitState::CleanupWriter(writer) = &mut self.commit {
            let result = writer.finish().await.map_err(StoreError::from);
            self.commit = CommitState::Closed;
            return result.map(|_| ());
        }
        let result = match &self.commit {
            CommitState::Ready(Err(error)) => Err(*error),
            CommitState::Closed => Ok(()),
            _ => Err(StoreError::WriterStopped),
        };
        if let Some(core) = self.core.as_mut() {
            let cleanup = core.settle_exit().await;
            if result.is_ok() {
                return cleanup;
            }
        }
        result
    }

    fn install_cleanup_writer(&mut self) -> Result<(), StoreError> {
        let result = match std::mem::replace(&mut self.commit, CommitState::Closed) {
            CommitState::Ready(result) => result,
            state => {
                self.commit = state;
                return Err(StoreError::WriterStopped);
            }
        };
        let committed = result?;
        let core = self.core.as_mut().ok_or(StoreError::WriterStopped)?;
        core.control.take();
        core.exit.take();
        core.clock.take();
        let join = core.join.take().ok_or(StoreError::WriterStopped)?;
        self.commit = CommitState::CleanupWriter(JournalWriter::from_handoff(
            committed.handoff,
            join,
            committed.cursor,
            committed.resident_pool,
        ));
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn resume_worker(
    root_plan: RootPlan,
    filename: String,
    id: SessionId,
    asserted_workspace: Option<PathBuf>,
    recovery_time: UnixMillis,
    cancel: Arc<AtomicBool>,
    resident_pool: ResidentCreditPool,
    ready_ack: oneshot::Sender<Result<RecoveryReport, StoreError>>,
    commands: mpsc::Receiver<ResumeCommand>,
) {
    let locked = match prepare_locked(
        root_plan,
        filename,
        &id,
        asserted_workspace,
        recovery_time,
        cancel.as_ref(),
        resident_pool,
    ) {
        Ok(locked) => locked,
        Err(error) => {
            let _ = ready_ack.send(Err(error));
            return;
        }
    };
    if ready_ack.send(Ok(locked.prepared.report.clone())).is_err() {
        return;
    }
    match commands.recv() {
        Ok(ResumeCommand::Abort) | Err(_) => {}
        Ok(ResumeCommand::Commit { ack }) => match commit_locked(locked, &id, recovery_time) {
            Ok((committed, file, inbox)) => {
                let durable_offset = committed.cursor.durable_offset;
                if ack.send(Ok(committed)).is_ok() {
                    inbox.run(file, durable_offset);
                }
            }
            Err(error) => {
                let _ = ack.send(Err(error));
            }
        },
    }
}

fn prepare_locked(
    root_plan: RootPlan,
    filename: String,
    id: &SessionId,
    asserted_workspace: Option<PathBuf>,
    recovery_time: UnixMillis,
    cancel: &AtomicBool,
    resident_pool: ResidentCreditPool,
) -> Result<LockedResume, StoreError> {
    let root = root_plan.open_for_listing()?.ok_or(StoreError::NotFound)?;
    let mut file = open_and_lock(root.as_ref(), &filename)?;
    file.seek(SeekFrom::Start(0)).map_err(|_| StoreError::Io)?;
    let mut workspace = None;
    let scan = scan_jsonl_validating_header(&mut file, id, cancel, |header, identity| {
        let path = asserted_workspace
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(header.cwd().unwrap_or("")));
        let opened = WorkspaceAuthority::open(path).map_err(|_| StoreError::WorkspaceMismatch)?;
        if opened.identity() != identity {
            return Err(StoreError::WorkspaceMismatch);
        }
        workspace = Some(opened);
        Ok(())
    })?;
    revalidate_prepare(root.as_ref(), &filename, &file, scan.physical_bytes())?;
    let identity = journal_identity(&file)?;
    let prepared = finish_preparation(scan, recovery_time, identity)?;
    Ok(LockedResume {
        root,
        filename,
        file: Some(file),
        workspace: Some(workspace.ok_or(StoreError::WorkspaceMismatch)?),
        prepared,
        resident_pool,
    })
}

fn commit_locked(
    mut locked: LockedResume,
    id: &SessionId,
    recovery_time: UnixMillis,
) -> Result<(CommittedParts, File, super::journal::JournalInbox), StoreError> {
    let file = locked.file.as_mut().ok_or(StoreError::WriterStopped)?;
    revalidate_commit(
        locked.root.as_ref(),
        &locked.filename,
        file,
        &locked.prepared.token,
    )?;
    file.seek(SeekFrom::Start(0)).map_err(|_| StoreError::Io)?;
    let expected_header = &locked.prepared.seed.header;
    let expected_workspace = locked
        .workspace
        .as_ref()
        .ok_or(StoreError::WriterStopped)?
        .identity();
    let never_cancel = AtomicBool::new(false);
    let scan = scan_jsonl_validating_header(&mut *file, id, &never_cancel, |header, identity| {
        if header != expected_header || identity != expected_workspace {
            return Err(StoreError::Changed);
        }
        Ok(())
    })
    .map_err(changed_after_ready)?;
    let identity = journal_identity(file).map_err(changed_after_ready)?;
    let rescanned =
        finish_preparation(scan, recovery_time, identity).map_err(changed_after_ready)?;
    if rescanned.token != locked.prepared.token
        || rescanned.suffix != locked.prepared.suffix
        || rescanned.report != locked.prepared.report
    {
        return Err(StoreError::Changed);
    }
    revalidate_commit(
        locked.root.as_ref(),
        &locked.filename,
        file,
        &locked.prepared.token,
    )?;
    let (handoff, inbox) = handoff_channel();
    let cursor = commit_recovery_suffix(
        file,
        rescanned.token.valid_bytes,
        rescanned.suffix.as_slice(),
    )
    .map_err(StoreError::from)?;
    let file = locked.file.take().ok_or(StoreError::WriterStopped)?;
    let workspace = locked.workspace.take().ok_or(StoreError::WriterStopped)?;
    Ok((
        CommittedParts {
            seed: rescanned.seed,
            workspace,
            handoff,
            cursor,
            resident_pool: locked.resident_pool,
        },
        file,
        inbox,
    ))
}

fn finish_preparation(
    scan: ColdScan,
    recovery_time: UnixMillis,
    identity: JournalIdentity,
) -> Result<PreparedWork, StoreError> {
    let (plan, mut projection) = scan.prepare_recovery(recovery_time)?;
    let report = scan.recovery_report(&plan)?;
    let encoded = encode_plan(&plan, scan.valid_bytes())?;
    if !projection.bind_recovery_tool_result_rows(encoded.rows.iter().copied()) {
        return Err(StoreError::Corrupt);
    }
    let suffix = encoded.bytes;
    let suffix_len = u64::try_from(suffix.len()).map_err(|_| StoreError::Limit)?;
    let accepted_journal_bytes = scan
        .valid_bytes()
        .checked_add(suffix_len)
        .ok_or(StoreError::Limit)?;
    let added_events = u64::try_from(plan.events().len()).map_err(|_| StoreError::Limit)?;
    let logical_event_count = scan
        .logical_events()
        .checked_add(added_events)
        .ok_or(StoreError::Limit)?;
    let next_seq = plan.events().last().map_or(scan.next_seq(), |event| {
        event
            .seq()
            .get()
            .checked_add(1)
            .and_then(|value| EventSeq::new(value).ok())
    });
    let seed = RecoveredSeed::new(
        scan.header().clone(),
        projection,
        next_seq,
        plan.resume_seed_len(),
        logical_event_count,
        accepted_journal_bytes,
    )?;
    let token = PrepareToken {
        journal_device: identity.device,
        journal_inode: identity.inode,
        physical_bytes: scan.physical_bytes(),
        valid_bytes: scan.valid_bytes(),
        physical_sha256: *scan.physical_sha256(),
        suffix_sha256: sha256(&suffix),
    };
    Ok(PreparedWork {
        token,
        suffix,
        report,
        seed,
    })
}

struct EncodedRecoveryPlan {
    bytes: Vec<u8>,
    rows: Vec<super::journal_row::JournalRowLocator>,
}

fn encode_plan(plan: &RecoveryPlan, start_offset: u64) -> Result<EncodedRecoveryPlan, StoreError> {
    let maximum =
        usize::try_from(super::DURABLE_REPAIR_RESERVED_BYTES).map_err(|_| StoreError::Limit)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(maximum)
        .map_err(|_| StoreError::Limit)?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(plan.events().len())
        .map_err(|_| StoreError::Limit)?;
    for event in plan.events() {
        let line = encode_event_line(event).map_err(|_| StoreError::Limit)?;
        let next = bytes
            .len()
            .checked_add(line.len())
            .ok_or(StoreError::Limit)?;
        if next > maximum {
            return Err(StoreError::Limit);
        }
        let offset = start_offset
            .checked_add(u64::try_from(bytes.len()).map_err(|_| StoreError::Limit)?)
            .ok_or(StoreError::Limit)?;
        let row = super::journal_row::JournalRowLocator::new(event.seq(), offset, &line)
            .ok_or(StoreError::Limit)?;
        rows.push(row);
        bytes.extend_from_slice(&line);
    }
    Ok(EncodedRecoveryPlan { bytes, rows })
}

fn open_and_lock(root: &Dir, filename: &str) -> Result<File, StoreError> {
    let descriptor = rustix::fs::openat(
        root,
        filename,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::NOENT => StoreError::NotFound,
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => StoreError::UnsafeRoot,
        _ => StoreError::Io,
    })?;
    let file = File::from(descriptor);
    validate_opened_journal(&file)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(lock_error)?;
    if !named_journal_still_matches(root, filename.as_ref(), &file)? {
        return Err(StoreError::NotFound);
    }
    Ok(file)
}

fn revalidate_prepare(
    root: &Dir,
    filename: &str,
    file: &File,
    expected_len: u64,
) -> Result<(), StoreError> {
    validate_opened_journal(file)?;
    if file.metadata().map_err(|_| StoreError::Io)?.len() != expected_len {
        return Err(StoreError::Changed);
    }
    if !named_journal_still_matches(root, filename.as_ref(), file)? {
        return Err(StoreError::Changed);
    }
    Ok(())
}

fn revalidate_commit(
    root: &Dir,
    filename: &str,
    file: &File,
    token: &PrepareToken,
) -> Result<(), StoreError> {
    validate_opened_journal(file).map_err(changed_after_ready)?;
    let identity = journal_identity(file).map_err(changed_after_ready)?;
    if identity.device != token.journal_device
        || identity.inode != token.journal_inode
        || identity.length != token.physical_bytes
    {
        return Err(StoreError::Changed);
    }
    match named_journal_still_matches(root, filename.as_ref(), file) {
        Ok(true) => Ok(()),
        Ok(false) => Err(StoreError::Changed),
        Err(StoreError::Io) => Err(StoreError::Io),
        Err(_) => Err(StoreError::Changed),
    }
}

#[derive(Clone, Copy)]
struct JournalIdentity {
    device: u64,
    inode: u64,
    length: u64,
}

fn journal_identity(file: &File) -> Result<JournalIdentity, StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata().map_err(|_| StoreError::Io)?;
    Ok(JournalIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
    })
}

fn changed_after_ready(error: StoreError) -> StoreError {
    if error == StoreError::Io {
        StoreError::Io
    } else {
        StoreError::Changed
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    let mut output = [0_u8; 32];
    output.copy_from_slice(digest.as_ref());
    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write as _,
        os::unix::fs::PermissionsExt as _,
        path::PathBuf,
    };
    use std::{sync::mpsc, time::Duration};

    use tokio::sync::mpsc::error::TryRecvError;

    use crate::workspace_authority::WorkspaceAuthority;

    use super::*;
    use crate::session::{
        ClockError, CommittedUiKind, EventKind, NewEvent, SessionStore, TurnEndReason, TurnId,
    };

    #[derive(Clone, Copy)]
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            UnixMillis::new(self.0).map_err(|error| ClockError::new(error.to_string()))
        }
    }

    struct OpenTurnFixture {
        root: PathBuf,
        workspace: PathBuf,
        store: SessionStore,
        id: SessionId,
        path: PathBuf,
        original: Vec<u8>,
    }

    impl OpenTurnFixture {
        async fn new(label: &str) -> Self {
            let root = private_dir(&format!("{label}-root"));
            let workspace = private_dir(&format!("{label}-workspace"));
            let store = SessionStore::open_existing(&root).unwrap();
            let authority = WorkspaceAuthority::open(&workspace).unwrap();
            let id = SessionId::new("session-a50e8400-e29b-41d4-a716-446655440000");
            let mut session = store
                .prepare_new(id.clone(), &authority, FixedClock(1_000))
                .unwrap();
            session.materialize_if_needed().await.unwrap();
            session
                .append_settled(NewEvent::log(EventKind::turn_start(turn(1))))
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

        fn begin(&self, time: i64) -> PreparingResume {
            self.store
                .begin_resume(
                    self.id.clone(),
                    Some(self.workspace.clone()),
                    FixedClock(time),
                )
                .unwrap()
        }

        fn cleanup(self) {
            drop(self.store);
            fs::remove_file(self.path).unwrap();
            fs::remove_dir(self.root).unwrap();
            fs::remove_dir(self.workspace).unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prepared_resume_is_read_only_and_abort_releases_the_lock() {
        let fixture = OpenTurnFixture::new("resume-abort").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();

        assert!(prepared.recovery_report().closes_turn());
        assert!(prepared.recovery_report().adds_seed_marker());
        assert_eq!(fs::read(&fixture.path).unwrap(), fixture.original);

        let mut competing = fixture.begin(2_001);
        assert_eq!(competing.wait_ready().await.unwrap_err(), StoreError::Busy);
        competing.cancel_and_shutdown().await.unwrap();

        prepared.cancel_and_shutdown().await.unwrap();
        assert_eq!(fs::read(&fixture.path).unwrap(), fixture.original);

        let mut after_abort = fixture.begin(2_002);
        after_abort.wait_ready().await.unwrap();
        after_abort.cancel_and_shutdown().await.unwrap();
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_resume_keeps_the_lock_and_observes_only_new_live_events() {
        let fixture = OpenTurnFixture::new("resume-commit").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();
        prepared.begin_commit().unwrap();
        prepared.wait_commit().await.unwrap();

        let mut before_handoff = fixture.begin(2_001);
        assert_eq!(
            before_handoff.wait_ready().await.unwrap_err(),
            StoreError::Busy
        );
        before_handoff.cancel_and_shutdown().await.unwrap();

        let recovered = prepared.finish_commit().unwrap();
        let (mut session, workspace) = recovered.into_parts();

        assert_eq!(workspace.canonical_path(), fixture.workspace.as_path());
        assert_eq!(session.state().open_turn(), None);
        assert_eq!(session.next_seq(), EventSeq::new(3).ok());
        assert_eq!(session.logical_event_count(), 3);
        let mut observer = session.attach_ui_observer().unwrap();
        assert!(matches!(observer.try_recv(), Err(TryRecvError::Empty)));

        let mut competing = fixture.begin(2_002);
        assert_eq!(competing.wait_ready().await.unwrap_err(), StoreError::Busy);
        competing.cancel_and_shutdown().await.unwrap();

        session
            .append_settled(NewEvent::log(EventKind::turn_start(turn(2))))
            .await
            .unwrap();
        session
            .append_settled(NewEvent::log(EventKind::turn_end(
                turn(2),
                TurnEndReason::Completed,
            )))
            .await
            .unwrap();
        session.flush_barrier().await.unwrap();

        assert!(matches!(
            observer.try_recv().unwrap().kind,
            CommittedUiKind::TurnStart { turn: value } if value == turn(2)
        ));
        assert!(matches!(
            observer.try_recv().unwrap().kind,
            CommittedUiKind::TurnEnd { turn: value, .. } if value == turn(2)
        ));
        assert!(matches!(observer.try_recv(), Err(TryRecvError::Empty)));
        session.shutdown().await.unwrap();

        let mut after_shutdown = fixture.begin(2_003);
        after_shutdown.wait_ready().await.unwrap();
        after_shutdown.cancel_and_shutdown().await.unwrap();

        let bytes = fs::read(&fixture.path).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 6);
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_a_commit_flight_closes_its_ack_before_joining() {
        let fixture = OpenTurnFixture::new("resume-drop-flight").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();
        prepared.begin_commit().unwrap();

        drop_on_thread_with_deadline(prepared);
        let mut after_drop = fixture.begin(2_001);
        after_drop.wait_ready().await.unwrap();
        after_drop.cancel_and_shutdown().await.unwrap();
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_a_ready_commit_closes_the_handoff_before_joining() {
        let fixture = OpenTurnFixture::new("resume-drop-ready").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();
        prepared.begin_commit().unwrap();
        prepared.wait_commit().await.unwrap();

        drop_on_thread_with_deadline(prepared);
        let mut after_drop = fixture.begin(2_001);
        after_drop.wait_ready().await.unwrap();
        after_drop.cancel_and_shutdown().await.unwrap();
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workspace_mismatch_wins_before_the_body_is_scanned_or_changed() {
        let fixture = OpenTurnFixture::new("resume-workspace-mismatch").await;
        let other_workspace = private_dir("resume-other-workspace");
        let mut bytes = fixture.original.clone();
        bytes.extend_from_slice(b"this body row is deliberately invalid\n");
        fs::write(&fixture.path, &bytes).unwrap();

        let mut preparing = fixture
            .store
            .begin_resume(
                fixture.id.clone(),
                Some(other_workspace.clone()),
                FixedClock(2_000),
            )
            .unwrap();
        assert_eq!(
            preparing.wait_ready().await.unwrap_err(),
            StoreError::WorkspaceMismatch
        );
        preparing.cancel_and_shutdown().await.unwrap();
        assert_eq!(fs::read(&fixture.path).unwrap(), bytes);

        fs::remove_dir(other_workspace).unwrap();
        fixture.cleanup();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn warning_gate_changes_are_detected_without_mutating_the_attackers_bytes() {
        let fixture = OpenTurnFixture::new("resume-changed").await;
        let mut preparing = fixture.begin(2_000);
        preparing.wait_ready().await.unwrap();
        let mut prepared = preparing.finish().unwrap();

        let mut attacker = OpenOptions::new().append(true).open(&fixture.path).unwrap();
        attacker.write_all(b"changed-after-warning").unwrap();
        drop(attacker);
        let attacked = fs::read(&fixture.path).unwrap();

        prepared.begin_commit().unwrap();
        assert_eq!(
            prepared.wait_commit().await.unwrap_err(),
            StoreError::Changed
        );
        prepared.cancel_and_shutdown().await.unwrap_err();
        assert_eq!(fs::read(&fixture.path).unwrap(), attacked);

        let mut after_failure = fixture.begin(2_001);
        after_failure.wait_ready().await.unwrap();
        after_failure.cancel_and_shutdown().await.unwrap();
        fixture.cleanup();
    }

    fn drop_on_thread_with_deadline(prepared: PreparedSession) {
        let (done, finished) = mpsc::sync_channel(1);
        let join = std::thread::spawn(move || {
            drop(prepared);
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(2))
            .expect("dropping the owner must not deadlock its worker join");
        join.join().unwrap();
    }

    fn turn(value: u64) -> TurnId {
        TurnId::new(value).unwrap()
    }

    fn private_dir(label: &str) -> PathBuf {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = parent.join(format!("dsh-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
