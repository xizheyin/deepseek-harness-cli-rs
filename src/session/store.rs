//! Deferred creation of one private durable session journal.

use std::{
    fs::File,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use cap_std::fs::Dir;
use thiserror::Error;

use crate::workspace_authority::{WorkspaceAuthority, WorkspaceIdentity};

use super::{
    Clock, MAX_SAFE_INTEGER, SESSION_FORMAT_VERSION, Session, SessionHeader, SessionId, UnixMillis,
    journal::{DeferredWriter, JournalError, JournalWriter},
    jsonl::{JsonlEncodeError, MAX_JOURNAL_HEADER_LINE_BYTES, encode_header_line},
    path_policy::RootPlan,
};

/// Stable storage failures intentionally omit host paths and OS diagnostics.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StoreError {
    #[error("CLI_SESSION_ROOT_UNAVAILABLE")]
    RootUnavailable,
    #[error("CLI_SESSION_ROOT_UNSAFE")]
    UnsafeRoot,
    #[error("CLI_SESSION_ID_INVALID")]
    InvalidSessionId,
    #[error("CLI_SESSION_HEADER_INVALID")]
    InvalidHeader,
    #[error("CLI_SESSION_BUSY")]
    Busy,
    #[error("CLI_SESSION_STORE_BUSY")]
    StoreBusy,
    #[error("CLI_SESSION_NOT_FOUND")]
    NotFound,
    #[error("CLI_SESSION_CHANGED")]
    Changed,
    #[error("CLI_SESSION_WORKSPACE_MISMATCH")]
    WorkspaceMismatch,
    #[error("CLI_SESSION_UNSUPPORTED")]
    Unsupported,
    #[error("CLI_SESSION_CORRUPT")]
    Corrupt,
    #[error("the session operation was cancelled")]
    Cancelled,
    #[error("CLI_SESSION_LIMIT")]
    Limit,
    #[error("CLI_SESSION_IO")]
    Io,
    #[error("CLI_SESSION_WRITER_STOPPED")]
    WriterStopped,
    #[error("CLI_SESSION_POISONED")]
    Poisoned,
}

impl From<JsonlEncodeError> for StoreError {
    fn from(_: JsonlEncodeError) -> Self {
        Self::InvalidHeader
    }
}

impl From<JournalError> for StoreError {
    fn from(error: JournalError) -> Self {
        match error {
            JournalError::Poisoned => Self::Poisoned,
            JournalError::WriterStopped
            | JournalError::NothingStaged
            | JournalError::AlreadyStaged
            | JournalError::FlightInProgress => Self::WriterStopped,
        }
    }
}

/// An already-open private store root. This first substrate accepts an
/// existing root; lazy secure bootstrap is added by `path_policy` before CLI
/// activation.
#[derive(Clone)]
pub struct SessionStore {
    root: RootPlan,
}

/// Safe header-only facts shown by `--list-sessions`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionMetadata {
    id: SessionId,
    created_at: UnixMillis,
    workspace: String,
    workspace_device: u64,
    workspace_inode: u64,
}

impl SessionMetadata {
    fn new(
        id: SessionId,
        created_at: UnixMillis,
        workspace: String,
        workspace_device: u64,
        workspace_inode: u64,
    ) -> Self {
        Self {
            id,
            created_at,
            workspace,
            workspace_device,
            workspace_inode,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        id: impl Into<SessionId>,
        created_at: UnixMillis,
        workspace: impl Into<String>,
    ) -> Self {
        Self::new(id.into(), created_at, workspace.into(), 0, 0)
    }

    pub(crate) fn id(&self) -> &SessionId {
        &self.id
    }

    pub(crate) fn created_at(&self) -> UnixMillis {
        self.created_at
    }

    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionStore")
            .field("opened", &true)
            .field("root_bytes", &self.root.display_root().as_os_str().len())
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Internal test seam for an already-opened exact-mode root.
    ///
    /// Production callers use `open_default`, whose component-by-component
    /// policy rejects symlinked or writable ancestors before returning a
    /// capability. Keeping this narrower seam crate-private prevents callers
    /// from mistaking final-node validation for the full path policy.
    #[cfg(test)]
    pub(crate) fn open_existing(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Ok(Self {
            root: RootPlan::open_existing(root.as_ref())?,
        })
    }

    /// Resolve the platform store policy without creating directories.
    pub fn open_default() -> Result<Self, StoreError> {
        Ok(Self {
            root: RootPlan::from_process_environment()?,
        })
    }

    /// Read bounded session headers without creating or repairing the store.
    pub(crate) fn list(
        &self,
        workspace: Option<WorkspaceIdentity>,
    ) -> Result<Vec<SessionMetadata>, StoreError> {
        let Some(root) = self.root.open_for_listing()? else {
            return Ok(Vec::new());
        };
        list_metadata(root.as_ref(), workspace)
    }

    /// Build a deferred Session. This performs all header checks but creates
    /// no journal until `Session::materialize_if_needed` is awaited.
    pub(crate) fn prepare_new(
        &self,
        id: SessionId,
        workspace: &WorkspaceAuthority,
        clock: impl Clock + 'static,
    ) -> Result<Session, StoreError> {
        let filename = canonical_filename(&id)?;
        let created_at = clock.now().map_err(|_| StoreError::InvalidHeader)?;
        let cwd = workspace
            .canonical_path()
            .to_str()
            .ok_or(StoreError::InvalidHeader)?
            .to_owned();
        let header = SessionHeader::new_durable(id, created_at, cwd, workspace.identity())
            .map_err(|_| StoreError::InvalidHeader)?;
        let header_line = encode_header_line(&header)?;
        let header_bytes =
            u64::try_from(header_line.len()).map_err(|_| StoreError::InvalidHeader)?;
        Ok(Session::new_deferred(
            header,
            clock,
            DeferredJournal::new(MaterializePlan {
                root: self.root.clone(),
                filename,
                header_line,
            }),
            header_bytes,
        ))
    }

    /// Start one owned, read-only recovery preparation. The returned object
    /// owns its worker, file lock, and cancellation flag before any async wait.
    pub(crate) fn begin_resume(
        &self,
        id: SessionId,
        asserted_workspace: Option<PathBuf>,
        clock: impl Clock + 'static,
    ) -> Result<super::resume::PreparingResume, StoreError> {
        let filename = canonical_filename(&id)?;
        super::resume::begin(
            self.root.clone(),
            filename,
            id,
            asserted_workspace,
            Box::new(clock),
        )
    }
}

pub(super) enum SessionStorage {
    Deferred(DeferredJournal),
    Active(JournalWriter),
    Finishing(JournalWriter),
    Failed(StoreError),
    Closed,
}

impl std::fmt::Debug for SessionStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Deferred(_) => "Deferred",
            Self::Active(_) => "Active",
            Self::Finishing(_) => "Finishing",
            Self::Failed(_) => "Failed",
            Self::Closed => "Closed",
        })
    }
}

pub(super) struct DeferredJournal {
    plan: Option<MaterializePlan>,
    startup: Option<DeferredWriter<StoreError>>,
}

impl DeferredJournal {
    fn new(plan: MaterializePlan) -> Self {
        Self {
            plan: Some(plan),
            startup: None,
        }
    }

    pub(super) async fn wait_ready(&mut self) -> Result<JournalWriter, StoreError> {
        if self.startup.is_none() {
            let plan = self.plan.take().ok_or(StoreError::WriterStopped)?;
            self.startup =
                Some(DeferredWriter::start(move || materialize(plan)).map_err(StoreError::from)?);
        }
        let result = self
            .startup
            .as_mut()
            .ok_or(StoreError::WriterStopped)?
            .wait_ready()
            .await
            .map_err(StoreError::from)?;
        self.startup.take();
        result
    }

    pub(super) fn has_started(&self) -> bool {
        self.startup.is_some()
    }
}

#[derive(Clone)]
struct MaterializePlan {
    root: RootPlan,
    filename: String,
    header_line: Vec<u8>,
}

fn materialize(plan: MaterializePlan) -> Result<(File, u64), StoreError> {
    let materialized_root = plan.root.materialize()?;
    let root = materialized_root.root;
    let root_sync = materialized_root.sync_file;
    #[cfg(unix)]
    lock_store_root(&root_sync)?;
    check_creation_capacity(root.as_ref())?;
    #[cfg(unix)]
    let descriptor = rustix::fs::openat(
        root.as_ref(),
        plan.filename.as_str(),
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            StoreError::Busy
        } else {
            StoreError::Io
        }
    })?;
    #[cfg(unix)]
    let mut file = File::from(descriptor);
    #[cfg(not(unix))]
    let mut file = return Err(StoreError::RootUnavailable);

    let mut journal_locked = false;
    let setup = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(0o600))
                .map_err(|_| StoreError::Io)?;
            let metadata = file.metadata().map_err(|_| StoreError::Io)?;
            if !metadata.is_file()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o7777 != 0o600
                || metadata.nlink() != 1
            {
                return Err(StoreError::UnsafeRoot);
            }
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .map_err(lock_error)?;
            journal_locked = true;
        }
        file.write_all(&plan.header_line)
            .map_err(|_| StoreError::Io)?;
        sync_file(&file)?;
        sync_directory(&root_sync)?;
        validate_named_journal(root.as_ref(), &plan, &file)?;
        Ok(())
    })();
    if let Err(error) = setup {
        if journal_locked {
            cleanup_created(root.as_ref(), &plan, &root_sync, &file);
        }
        return Err(error);
    }
    let offset = u64::try_from(plan.header_line.len()).map_err(|_| StoreError::InvalidHeader)?;
    Ok((file, offset))
}

const MAX_STORE_ENTRIES: usize = 256;
const MAX_CANONICAL_SESSION_SLOTS: usize = 128;
const STORE_LOCK_DEADLINE: Duration = Duration::from_millis(250);
const STORE_LOCK_RETRY: Duration = Duration::from_millis(5);

#[cfg(unix)]
fn lock_store_root(root: &File) -> Result<(), StoreError> {
    let deadline = Instant::now() + STORE_LOCK_DEADLINE;
    loop {
        match rustix::fs::flock(root, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(error)
                if error == rustix::io::Errno::WOULDBLOCK || error == rustix::io::Errno::AGAIN =>
            {
                if Instant::now() >= deadline {
                    return Err(StoreError::StoreBusy);
                }
                std::thread::sleep(STORE_LOCK_RETRY);
            }
            Err(_) => return Err(StoreError::Io),
        }
    }
}

fn check_creation_capacity(root: &Dir) -> Result<(), StoreError> {
    let entries = root.entries().map_err(|_| StoreError::Io)?;
    let mut total = 0_usize;
    let mut canonical = 0_usize;
    for entry in entries {
        let entry = entry.map_err(|_| StoreError::Io)?;
        total = total.checked_add(1).ok_or(StoreError::Limit)?;
        if total > MAX_STORE_ENTRIES {
            return Err(StoreError::Limit);
        }
        if is_canonical_session_filename(&entry.file_name()) {
            canonical = canonical.checked_add(1).ok_or(StoreError::Limit)?;
            if canonical > MAX_CANONICAL_SESSION_SLOTS {
                return Err(StoreError::Limit);
            }
        }
    }
    if total >= MAX_STORE_ENTRIES || canonical >= MAX_CANONICAL_SESSION_SLOTS {
        return Err(StoreError::Limit);
    }
    Ok(())
}

fn list_metadata(
    root: &Dir,
    workspace: Option<WorkspaceIdentity>,
) -> Result<Vec<SessionMetadata>, StoreError> {
    let entries = root.entries().map_err(|_| StoreError::Io)?;
    let mut total = 0_usize;
    let mut canonical = 0_usize;
    let mut metadata = Vec::new();
    metadata
        .try_reserve_exact(MAX_CANONICAL_SESSION_SLOTS)
        .map_err(|_| StoreError::Limit)?;

    for entry in entries {
        let entry = entry.map_err(|_| StoreError::Io)?;
        total = total.checked_add(1).ok_or(StoreError::Limit)?;
        if total > MAX_STORE_ENTRIES {
            return Err(StoreError::Limit);
        }
        let name = entry.file_name();
        let Some(id) = canonical_session_id(&name) else {
            continue;
        };
        canonical = canonical.checked_add(1).ok_or(StoreError::Limit)?;
        if canonical > MAX_CANONICAL_SESSION_SLOTS {
            return Err(StoreError::Limit);
        }
        let Some(candidate) = read_session_metadata(root, &name, id)? else {
            continue;
        };
        if workspace.is_none_or(|workspace| {
            candidate.workspace_device == workspace.device()
                && candidate.workspace_inode == workspace.inode()
        }) {
            metadata.push(candidate);
        }
    }

    metadata.sort_by(|left, right| {
        right
            .created_at()
            .cmp(&left.created_at())
            .then_with(|| left.id().cmp(right.id()))
    });
    Ok(metadata)
}

fn read_session_metadata(
    root: &Dir,
    name: &std::ffi::OsStr,
    expected_id: SessionId,
) -> Result<Option<SessionMetadata>, StoreError> {
    let descriptor = match rustix::fs::openat(
        root,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error)
            if error == rustix::io::Errno::LOOP
                || error == rustix::io::Errno::NOTDIR
                || error == rustix::io::Errno::ACCESS =>
        {
            return Err(StoreError::UnsafeRoot);
        }
        Err(_) => return Err(StoreError::Io),
    };
    let mut file = File::from(descriptor);
    validate_opened_journal(&file)?;
    if !named_journal_still_matches(root, name, &file)? {
        return Ok(None);
    }
    let Some(line) = read_complete_header_line(&mut file)? else {
        return Ok(None);
    };
    if !named_journal_still_matches(root, name, &file)? {
        return Ok(None);
    }
    Ok(parse_session_metadata(&line, &expected_id))
}

fn read_complete_header_line(file: &mut File) -> Result<Option<Vec<u8>>, StoreError> {
    let mut line = Vec::new();
    line.try_reserve_exact(MAX_JOURNAL_HEADER_LINE_BYTES)
        .map_err(|_| StoreError::Limit)?;
    let mut scratch = [0_u8; 8 * 1024];
    while line.len() < MAX_JOURNAL_HEADER_LINE_BYTES {
        let remaining = MAX_JOURNAL_HEADER_LINE_BYTES - line.len();
        let read_length = remaining.min(scratch.len());
        let count = file
            .read(&mut scratch[..read_length])
            .map_err(|_| StoreError::Io)?;
        if count == 0 {
            return Ok(None);
        }
        let bytes = &scratch[..count];
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&bytes[..=newline]);
            return Ok(Some(line));
        }
        line.extend_from_slice(bytes);
    }
    Ok(None)
}

fn parse_session_metadata(line: &[u8], expected_id: &SessionId) -> Option<SessionMetadata> {
    let payload = line.strip_suffix(b"\n")?;
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let fields = value.as_object()?;
    if fields.get("type")?.as_str()? != "session" {
        return None;
    }
    let version = safe_nonnegative_json_u64(fields.get("version")?)?;
    if version == SESSION_FORMAT_VERSION {
        let mut untagged = value.clone();
        untagged.as_object_mut()?.remove("type")?;
        let header = SessionHeader::from_value(untagged).ok()?;
        if header.id() != expected_id || header.cwd().is_none() {
            return None;
        }
    } else if !valid_future_metadata_fields(fields) {
        return None;
    }
    let id = SessionId::new(fields.get("id")?.as_str()?);
    if &id != expected_id {
        return None;
    }
    let created_number = fields.get("createdAt")?.as_number()?;
    if created_number.to_string() == "-0" {
        return None;
    }
    let created_at = created_number.as_i64()?;
    if created_at < 0 {
        return None;
    }
    let created_at = UnixMillis::new(created_at).ok()?;
    let workspace = fields.get("cwd")?.as_str()?.to_owned();
    if !Path::new(&workspace).is_absolute() {
        return None;
    }
    let _delegation_depth = safe_nonnegative_json_u64(fields.get("delegationDepth")?)?;
    let identity = fields.get("rustWorkspaceIdentity")?.as_object()?;
    let workspace_device = canonical_hex_u64(identity.get("device")?.as_str()?)?;
    let workspace_inode = canonical_hex_u64(identity.get("inode")?.as_str()?)?;
    Some(SessionMetadata::new(
        id,
        created_at,
        workspace,
        workspace_device,
        workspace_inode,
    ))
}

fn valid_future_metadata_fields(fields: &serde_json::Map<String, serde_json::Value>) -> bool {
    fields
        .get("parentSession")
        .is_none_or(serde_json::Value::is_string)
        && fields
            .get("seedLength")
            .is_none_or(|value| safe_nonnegative_json_u64(value).is_some())
        && fields
            .get("origin")
            .is_none_or(|value| value.as_str() == Some("subagent"))
        && fields
            .get("agentPreset")
            .is_none_or(serde_json::Value::is_string)
}

fn safe_nonnegative_json_u64(value: &serde_json::Value) -> Option<u64> {
    let number = value.as_number()?;
    if number.to_string() == "-0" {
        return None;
    }
    number.as_u64().filter(|value| *value <= MAX_SAFE_INTEGER)
}

fn canonical_hex_u64(value: &str) -> Option<u64> {
    if value.is_empty() {
        return None;
    }
    let parsed = u64::from_str_radix(value, 16).ok()?;
    (format!("{parsed:x}") == value).then_some(parsed)
}

pub(super) fn validate_opened_journal(file: &File) -> Result<(), StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata().map_err(|_| StoreError::Io)?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(())
}

pub(super) fn named_journal_still_matches(
    root: &Dir,
    name: &std::ffi::OsStr,
    file: &File,
) -> Result<bool, StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let opened = file.metadata().map_err(|_| StoreError::Io)?;
    let named = match rustix::fs::statat(root, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(_) => return Err(StoreError::Io),
    };
    if !rustix::fs::FileType::from_raw_mode(named.st_mode).is_file()
        || named.st_uid != rustix::process::geteuid().as_raw()
        || named.st_mode & 0o7777 != 0o600
        || named.st_nlink != 1
        || stat_device(&named) != Some(opened.dev())
        || named.st_ino != opened.ino()
    {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(true)
}

fn is_canonical_session_filename(name: &std::ffi::OsStr) -> bool {
    canonical_session_id(name).is_some()
}

fn canonical_session_id(name: &std::ffi::OsStr) -> Option<SessionId> {
    let name = name.to_str()?;
    let id = name.strip_suffix(".jsonl")?;
    let id = SessionId::new(id);
    canonical_filename(&id)
        .is_ok_and(|canonical| canonical == name)
        .then_some(id)
}

#[cfg(unix)]
pub(super) fn lock_error(error: rustix::io::Errno) -> StoreError {
    if error == rustix::io::Errno::WOULDBLOCK || error == rustix::io::Errno::AGAIN {
        StoreError::Busy
    } else {
        StoreError::Io
    }
}

fn cleanup_created(root: &Dir, plan: &MaterializePlan, root_sync: &File, file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(opened) = file.metadata() else {
            return;
        };
        let Ok(named) = rustix::fs::statat(
            root,
            plan.filename.as_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) else {
            return;
        };
        if rustix::fs::FileType::from_raw_mode(named.st_mode).is_file()
            && named.st_uid == rustix::process::geteuid().as_raw()
            && named.st_mode & 0o7777 == 0o600
            && named.st_nlink == 1
            && stat_device(&named) == Some(opened.dev())
            && named.st_ino == opened.ino()
        {
            let _ =
                rustix::fs::unlinkat(root, plan.filename.as_str(), rustix::fs::AtFlags::empty());
            let _ = sync_directory(root_sync);
        }
    }
}

#[cfg(unix)]
fn validate_named_journal(
    root: &Dir,
    plan: &MaterializePlan,
    file: &File,
) -> Result<(), StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let opened = file.metadata().map_err(|_| StoreError::Io)?;
    let named = rustix::fs::statat(
        root,
        plan.filename.as_str(),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|_| StoreError::UnsafeRoot)?;
    if !rustix::fs::FileType::from_raw_mode(named.st_mode).is_file()
        || named.st_uid != rustix::process::geteuid().as_raw()
        || named.st_mode & 0o7777 != 0o600
        || named.st_nlink != 1
        || stat_device(&named) != Some(opened.dev())
        || named.st_ino != opened.ino()
    {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn stat_device(stat: &rustix::fs::Stat) -> Option<u64> {
    u64::try_from(stat.st_dev).ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn stat_device(stat: &rustix::fs::Stat) -> Option<u64> {
    Some(stat.st_dev)
}

pub(super) fn canonical_filename(id: &SessionId) -> Result<String, StoreError> {
    let suffix = id
        .as_str()
        .strip_prefix("session-")
        .ok_or(StoreError::InvalidSessionId)?;
    let parsed = uuid::Uuid::parse_str(suffix).map_err(|_| StoreError::InvalidSessionId)?;
    if parsed.get_variant() != uuid::Variant::RFC4122
        || parsed.get_version() != Some(uuid::Version::Random)
        || suffix != parsed.hyphenated().to_string()
    {
        return Err(StoreError::InvalidSessionId);
    }
    Ok(format!("{id}.jsonl"))
}

#[cfg(target_os = "macos")]
fn sync_file(file: &File) -> Result<(), StoreError> {
    rustix::fs::fcntl_fullfsync(file).map_err(|_| StoreError::Io)
}

#[cfg(not(target_os = "macos"))]
fn sync_file(file: &File) -> Result<(), StoreError> {
    file.sync_all().map_err(|_| StoreError::Io)
}

#[cfg(target_os = "macos")]
fn sync_directory(directory: &File) -> Result<(), StoreError> {
    rustix::fs::fcntl_fullfsync(directory).map_err(|_| StoreError::Io)
}

#[cfg(not(target_os = "macos"))]
fn sync_directory(directory: &File) -> Result<(), StoreError> {
    rustix::fs::fsync(directory).map_err(|_| StoreError::Io)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write as _,
        os::unix::fs::PermissionsExt as _,
    };

    use crate::{
        session::{
            EventKind, EventSeq, NewEvent, SessionHeader, SessionId, SystemClock, TurnEndReason,
            TurnId, UnixMillis,
        },
        workspace_authority::WorkspaceAuthority,
    };

    use super::{SessionStore, StoreError, encode_header_line};

    #[tokio::test]
    async fn preparation_is_lazy_and_materialization_publishes_one_durable_header() {
        let root = private_dir("store-root");
        let workspace = private_dir("store-workspace");
        let store = SessionStore::open_existing(&root).unwrap();
        let authority = WorkspaceAuthority::open(&workspace).unwrap();
        let mut session = store
            .prepare_new(
                SessionId::new("session-550e8400-e29b-41d4-a716-446655440000"),
                &authority,
                SystemClock,
            )
            .unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        session.materialize_if_needed().await.unwrap();
        let path = root.join(format!("{}.jsonl", session.id()));
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "session");
        session.shutdown().await.unwrap();

        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
        fs::remove_dir(workspace).unwrap();
    }

    #[tokio::test]
    async fn durable_journal_continues_past_the_old_memory_event_ceiling() {
        let root = private_dir("store-long-root");
        let workspace = private_dir("store-long-workspace");
        let store = SessionStore::open_existing(&root).unwrap();
        let authority = WorkspaceAuthority::open(&workspace).unwrap();
        let mut session = store
            .prepare_new(
                SessionId::new("session-650e8400-e29b-41d4-a716-446655440000"),
                &authority,
                SystemClock,
            )
            .unwrap();
        session.materialize_if_needed().await.unwrap();

        for raw_turn in 1..=2_050_u64 {
            let turn = TurnId::new(raw_turn).unwrap();
            session
                .append_settled(NewEvent::log(EventKind::turn_start(turn)))
                .await
                .unwrap();
            session
                .append_settled(NewEvent::log(EventKind::turn_end(
                    turn,
                    TurnEndReason::Completed,
                )))
                .await
                .unwrap();
        }
        session.flush_barrier().await.unwrap();
        assert_eq!(session.logical_event_count(), 4_100);
        assert_eq!(session.next_seq(), EventSeq::new(4_100).ok());
        session.shutdown().await.unwrap();

        let path = root.join(format!("{}.jsonl", session.id()));
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 4_101);
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
        fs::remove_dir(workspace).unwrap();
    }

    #[test]
    fn root_and_session_id_policy_fail_before_creation() {
        assert!(matches!(
            SessionStore::open_existing("relative"),
            Err(StoreError::RootUnavailable)
        ));
        let root = private_dir("store-id-root");
        let workspace = private_dir("store-id-workspace");
        let store = SessionStore::open_existing(&root).unwrap();
        let authority = WorkspaceAuthority::open(&workspace).unwrap();
        let result = store.prepare_new(
            SessionId::new("550e8400-e29b-41d4-a716-446655440000"),
            &authority,
            SystemClock,
        );
        assert!(matches!(result, Err(StoreError::InvalidSessionId)));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir(root).unwrap();
        fs::remove_dir(workspace).unwrap();
    }

    #[test]
    fn listing_reads_only_valid_bounded_headers_and_sorts_deterministically() {
        let root = private_dir("store-list-root");
        let workspace_a = private_dir("store-list-workspace-a");
        let workspace_b = private_dir("store-list-workspace-b");
        let authority_a = WorkspaceAuthority::open(&workspace_a).unwrap();
        let authority_b = WorkspaceAuthority::open(&workspace_b).unwrap();
        let store = SessionStore::open_existing(&root).unwrap();
        let id_a = SessionId::new("session-550e8400-e29b-41d4-a716-446655440000");
        let id_b = SessionId::new("session-650e8400-e29b-41d4-a716-446655440000");
        let id_c = SessionId::new("session-750e8400-e29b-41d4-a716-446655440000");

        let locked_path = write_header(&root, &id_b, 20, &authority_a, b"{not-json}\n");
        let locked = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&locked_path)
            .unwrap();
        rustix::fs::flock(
            &locked,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .unwrap();
        write_header(&root, &id_a, 20, &authority_a, b"");
        write_header(
            &root,
            &id_c,
            30,
            &authority_b,
            b"secret body that list must ignore\n",
        );
        write_private_file(
            &root.join("session-850e8400-e29b-41d4-a716-446655440000.jsonl"),
            b"{\"type\":\"session\"}",
        );
        write_private_file(
            &root.join("session-950e8400-e29b-41d4-a716-446655440000.jsonl"),
            b"",
        );

        let all = store.list(None).unwrap();
        assert_eq!(
            all.iter()
                .map(|meta| meta.id().as_str())
                .collect::<Vec<_>>(),
            vec![id_c.as_str(), id_a.as_str(), id_b.as_str()]
        );
        let filtered = store.list(Some(authority_a.identity())).unwrap();
        assert_eq!(
            filtered
                .iter()
                .map(|meta| meta.id().as_str())
                .collect::<Vec<_>>(),
            vec![id_a.as_str(), id_b.as_str()]
        );

        drop(locked);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir(workspace_a).unwrap();
        fs::remove_dir(workspace_b).unwrap();
    }

    #[test]
    fn listing_counts_invalid_slots_and_enforces_both_directory_limits() {
        let canonical_root = private_dir("store-list-canonical-limit");
        let store = SessionStore::open_existing(&canonical_root).unwrap();
        for _ in 0..128 {
            let name = format!("session-{}.jsonl", uuid::Uuid::new_v4());
            write_private_file(&canonical_root.join(name), b"");
        }
        assert!(store.list(None).unwrap().is_empty());
        write_private_file(
            &canonical_root.join(format!("session-{}.jsonl", uuid::Uuid::new_v4())),
            b"",
        );
        assert!(matches!(store.list(None), Err(StoreError::Limit)));
        fs::remove_dir_all(canonical_root).unwrap();

        let total_root = private_dir("store-list-total-limit");
        let store = SessionStore::open_existing(&total_root).unwrap();
        for index in 0..256 {
            write_private_file(&total_root.join(format!("artifact-{index}")), b"");
        }
        assert!(store.list(None).unwrap().is_empty());
        write_private_file(&total_root.join("artifact-256"), b"");
        assert!(matches!(store.list(None), Err(StoreError::Limit)));
        fs::remove_dir_all(total_root).unwrap();
    }

    #[test]
    fn listing_rejects_a_canonical_symlink_without_following_it() {
        let root = private_dir("store-list-symlink-root");
        let sentinel = root.join("sentinel");
        write_private_file(&sentinel, b"do not read");
        let link = root.join("session-a50e8400-e29b-41d4-a716-446655440000.jsonl");
        std::os::unix::fs::symlink(&sentinel, &link).unwrap();
        let store = SessionStore::open_existing(&root).unwrap();
        assert!(matches!(store.list(None), Err(StoreError::UnsafeRoot)));
        assert_eq!(fs::read(&sentinel).unwrap(), b"do not read");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_accepts_the_exact_header_line_limit_and_omits_one_over() {
        let root = private_dir("store-list-header-limit");
        let workspace = private_dir("store-list-header-workspace");
        let authority = WorkspaceAuthority::open(&workspace).unwrap();
        let exact_id = SessionId::new("session-a50e8400-e29b-41d4-a716-446655440000");
        let over_id = SessionId::new("session-b50e8400-e29b-41d4-a716-446655440000");
        let exact = padded_header_line(&exact_id, &authority, super::MAX_JOURNAL_HEADER_LINE_BYTES);
        let over = padded_header_line(
            &over_id,
            &authority,
            super::MAX_JOURNAL_HEADER_LINE_BYTES + 1,
        );
        write_private_file(&root.join(format!("{exact_id}.jsonl")), &exact);
        write_private_file(&root.join(format!("{over_id}.jsonl")), &over);

        let listed = SessionStore::open_existing(&root)
            .unwrap()
            .list(None)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id(), &exact_id);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir(workspace).unwrap();
    }

    #[test]
    fn listing_omits_current_headers_with_invalid_optional_fields() {
        let root = private_dir("store-list-invalid-fields");
        let workspace = private_dir("store-list-invalid-workspace");
        let authority = WorkspaceAuthority::open(&workspace).unwrap();
        for (id, field, invalid) in [
            (
                "session-d50e8400-e29b-41d4-a716-446655440000",
                "agentPreset",
                serde_json::json!(7),
            ),
            (
                "session-e50e8400-e29b-41d4-a716-446655440000",
                "origin",
                serde_json::json!("bad"),
            ),
            (
                "session-f50e8400-e29b-41d4-a716-446655440000",
                "seedLength",
                serde_json::json!(-1),
            ),
        ] {
            let id = SessionId::new(id);
            let header = SessionHeader::new_durable(
                id.clone(),
                UnixMillis::new(1).unwrap(),
                authority.canonical_path().to_str().unwrap().to_owned(),
                authority.identity(),
            )
            .unwrap();
            let mut value: serde_json::Value =
                serde_json::from_slice(&encode_header_line(&header).unwrap()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), invalid);
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            write_private_file(&root.join(format!("{id}.jsonl")), &bytes);
        }

        assert!(
            SessionStore::open_existing(&root)
                .unwrap()
                .list(None)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir(workspace).unwrap();
    }

    fn write_header(
        root: &std::path::Path,
        id: &SessionId,
        created_at: i64,
        authority: &WorkspaceAuthority,
        body: &[u8],
    ) -> std::path::PathBuf {
        let header = SessionHeader::new_durable(
            id.clone(),
            UnixMillis::new(created_at).unwrap(),
            authority.canonical_path().to_str().unwrap().to_owned(),
            authority.identity(),
        )
        .unwrap();
        let mut bytes = encode_header_line(&header).unwrap();
        bytes.extend_from_slice(body);
        let path = root.join(format!("{id}.jsonl"));
        write_private_file(&path, &bytes);
        path
    }

    fn write_private_file(path: &std::path::Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn padded_header_line(
        id: &SessionId,
        authority: &WorkspaceAuthority,
        complete_line_bytes: usize,
    ) -> Vec<u8> {
        let mut value = serde_json::json!({
            "type": "session",
            "version": 0,
            "id": id,
            "createdAt": 1,
            "cwd": authority.canonical_path().to_str().unwrap(),
            "delegationDepth": 0,
            "rustWorkspaceIdentity": {
                "device": format!("{:x}", authority.identity().device()),
                "inode": format!("{:x}", authority.identity().inode()),
            },
            "padding": "",
        });
        let base = serde_json::to_vec(&value).unwrap();
        let padding = complete_line_bytes.checked_sub(base.len() + 1).unwrap();
        value["padding"] = serde_json::Value::String("x".repeat(padding));
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        assert_eq!(bytes.len(), complete_line_bytes);
        bytes
    }

    fn private_dir(label: &str) -> std::path::PathBuf {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = parent.join(format!("dsh-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
