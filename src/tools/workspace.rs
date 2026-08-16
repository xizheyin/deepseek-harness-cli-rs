use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

#[cfg(unix)]
use std::os::fd::OwnedFd;

use cap_std::fs::{Dir, OpenOptions};
#[cfg(unix)]
use cap_std::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use tokio::task;
use tokio_util::sync::CancellationToken;

use crate::{entropy::EntropySource, workspace_authority::WorkspaceAuthority};

use super::{
    MAX_DIRECTORY_DEPTH, MAX_READ_CHUNK_BYTES, MAX_TRAVERSAL_PATH_BYTES,
    arguments::MAX_TOOL_ARGUMENT_STRING_BYTES,
    error::{ToolCallError, ToolCallResult, ToolRegistryBuildError},
};

const DIRECTORY_BATCH_ENTRIES: usize = 256;

#[derive(Clone)]
pub(crate) struct Workspace {
    authority: WorkspaceAuthority,
    mutation_lock: Arc<Mutex<()>>,
    entropy: EntropySource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathSymlinks {
    None,
    Final,
    Intermediate,
}

#[derive(Clone)]
pub(crate) struct WorkspaceEntry {
    pub(crate) relative: PathBuf,
    pub(crate) display: String,
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<SystemTime>,
}

#[derive(Clone)]
pub(crate) struct WorkspaceFile {
    pub(crate) relative: PathBuf,
    pub(crate) display: String,
    pub(crate) modified: SystemTime,
}

pub(crate) struct ReadFile {
    pub(crate) bytes: Vec<u8>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceMutationOperation {
    Create,
    Update,
}

#[cfg(unix)]
pub(crate) struct PreparedWorkspaceMutation {
    root: Arc<Dir>,
    parent: Arc<Dir>,
    parent_relative: PathBuf,
    parent_dev: u64,
    parent_ino: u64,
    target_name: OsString,
    display: String,
    operation: WorkspaceMutationOperation,
    baseline: Option<Vec<u8>>,
    snapshot: Option<MutationSnapshot>,
    mutation_lock: Arc<Mutex<()>>,
    entropy: EntropySource,
    #[cfg(test)]
    test_commit_hook: Option<MutationCommitTestHook>,
}

/// Directory authority retained between shell approval preparation and spawn.
///
/// The path text is display metadata only. The held directory and its identity
/// are the authority; immediately before spawn we reopen the relative path and
/// compare identities so a replacement cannot redirect the command.
#[cfg(unix)]
pub(crate) struct PreparedShellWorkdir {
    root: Arc<Dir>,
    directory: Dir,
    relative: PathBuf,
    display: String,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl std::fmt::Debug for PreparedShellWorkdir {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedShellWorkdir")
            .field("display_bytes", &self.display.len())
            .field("component_count", &shell_component_count(&self.relative))
            .field("directory_capability", &true)
            .finish()
    }
}

#[cfg(unix)]
impl PreparedShellWorkdir {
    pub(crate) fn display(&self) -> &str {
        &self.display
    }

    /// Reopen and compare the approved directory immediately before spawn.
    pub(crate) fn revalidate(self, cancellation: &CancellationToken) -> ToolCallResult<OwnedFd> {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        let held_metadata = self
            .directory
            .dir_metadata()
            .map_err(|_| ToolCallError::shell_workdir_changed())?;
        if held_metadata.dev() != self.dev || held_metadata.ino() != self.ino {
            return Err(ToolCallError::shell_workdir_changed());
        }
        let reopened =
            open_shell_directory_no_follow(&self.root, &self.relative, Some(cancellation))
                .map_err(|error| map_shell_workdir_open_error(&error, cancellation, true))?;
        let reopened_metadata = reopened
            .dir_metadata()
            .map_err(|_| ToolCallError::shell_workdir_changed())?;
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        if reopened_metadata.dev() != self.dev || reopened_metadata.ino() != self.ino {
            return Err(ToolCallError::shell_workdir_changed());
        }
        Ok(OwnedFd::from(reopened.into_std_file()))
    }
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationCommitTestPhase {
    StagingCreated,
    StagingChunkWritten,
    BeforeStagingSync,
    BeforeLateRevalidate,
    BeforePublish,
    AfterPublish,
    BeforeCleanup,
    BeforeParentSync,
}

#[cfg(all(test, unix))]
type MutationCommitTestHook = Arc<
    dyn Fn(MutationCommitTestPhase, &CancellationToken, Option<&Dir>, &OsString) -> io::Result<()>
        + Send
        + Sync,
>;

#[cfg(unix)]
pub(crate) enum WorkspaceCommitStatus {
    Committed {
        durability_uncertain: bool,
        cleanup_warning: bool,
    },
    NotCommitted {
        error: ToolCallError,
        cleanup_warning: bool,
    },
}

#[cfg(unix)]
impl WorkspaceCommitStatus {
    fn not_committed(error: ToolCallError) -> Self {
        Self::NotCommitted {
            error,
            cleanup_warning: false,
        }
    }

    fn not_committed_after_cleanup(error: ToolCallError, cleanup: io::Result<()>) -> Self {
        Self::NotCommitted {
            error,
            cleanup_warning: cleanup.is_err(),
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct MutationSnapshot {
    dev: u64,
    ino: u64,
    len: u64,
    mode: u32,
    nlink: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

#[cfg(unix)]
impl PreparedWorkspaceMutation {
    pub(crate) fn baseline(&self) -> Option<&[u8]> {
        self.baseline.as_deref()
    }

    #[cfg(test)]
    fn run_test_commit_phase(
        &self,
        phase: MutationCommitTestPhase,
        cancellation: &CancellationToken,
        stage: Option<&Dir>,
        stage_name: &OsString,
    ) -> io::Result<()> {
        self.test_commit_hook
            .as_ref()
            .map_or(Ok(()), |hook| hook(phase, cancellation, stage, stage_name))
    }

    pub(crate) fn commit(
        self,
        candidate: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<WorkspaceCommitStatus, ToolExecutorCommitError> {
        let _guard = loop {
            if cancellation.is_cancelled() {
                return Ok(WorkspaceCommitStatus::not_committed(
                    ToolCallError::aborted(),
                ));
            }
            match self.mutation_lock.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::park_timeout(Duration::from_millis(5));
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(ToolExecutorCommitError);
                }
            }
        };
        if cancellation.is_cancelled() {
            return Ok(WorkspaceCommitStatus::not_committed(
                ToolCallError::aborted(),
            ));
        }
        if let Some(error) = self.revalidate(cancellation) {
            return Ok(WorkspaceCommitStatus::not_committed(error));
        }

        let stage_id = match self.entropy.uuid_v4() {
            Ok(id) => id,
            Err(_) => {
                return Ok(WorkspaceCommitStatus::not_committed(ToolCallError::model(
                    "FsError",
                    "FS_IO_ERROR",
                    "could not create a private staging directory",
                )));
            }
        };
        let stage_name = OsString::from(format!(".dsh-stage-{stage_id}"));
        let mut builder = cap_std::fs::DirBuilder::new();
        builder.mode(0o700);
        if self.parent.create_dir_with(&stage_name, &builder).is_err() {
            return Ok(WorkspaceCommitStatus::not_committed(ToolCallError::model(
                "FsError",
                "FS_IO_ERROR",
                "could not create a private staging directory",
            )));
        }
        let stage = match open_child_directory_no_follow(&self.parent, Path::new(&stage_name)) {
            Ok(stage) => stage,
            Err(_) => {
                let cleanup = self.parent.remove_dir(&stage_name);
                return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                    ToolCallError::model(
                        "FsError",
                        "FS_IO_ERROR",
                        "could not open the private staging directory",
                    ),
                    cleanup,
                ));
            }
        };
        #[cfg(test)]
        if self
            .run_test_commit_phase(
                MutationCommitTestPhase::StagingCreated,
                cancellation,
                Some(&stage),
                &stage_name,
            )
            .is_err()
        {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                injected_commit_failure(),
                cleanup,
            ));
        }
        if cancellation.is_cancelled() {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                ToolCallError::aborted(),
                cleanup,
            ));
        }
        let staged = match create_staging_file(&stage) {
            Ok(file) => file,
            Err(_) => {
                // We never acquired `candidate`, so do not delete an object a
                // racing same-user process may have inserted. Removing the
                // now-nonempty directory simply fails closed.
                drop(stage);
                let cleanup = self.parent.remove_dir(&stage_name);
                return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                    ToolCallError::model(
                        "FsError",
                        "FS_IO_ERROR",
                        "could not create a private staging file",
                    ),
                    cleanup,
                ));
            }
        };
        let staged_result = write_staging_file(
            staged,
            candidate,
            self.snapshot
                .map_or(0o600, |snapshot| snapshot.mode & 0o777),
            cancellation,
            #[cfg(test)]
            self.test_commit_hook.as_ref(),
            #[cfg(test)]
            &stage,
            #[cfg(test)]
            &stage_name,
        );
        if let Err(error) = staged_result {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                error, cleanup,
            ));
        }
        if cancellation.is_cancelled() {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                ToolCallError::aborted(),
                cleanup,
            ));
        }
        #[cfg(test)]
        if self
            .run_test_commit_phase(
                MutationCommitTestPhase::BeforeLateRevalidate,
                cancellation,
                Some(&stage),
                &stage_name,
            )
            .is_err()
        {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                injected_commit_failure(),
                cleanup,
            ));
        }
        if let Some(error) = self.revalidate(cancellation) {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                error, cleanup,
            ));
        }
        #[cfg(test)]
        if self
            .run_test_commit_phase(
                MutationCommitTestPhase::BeforePublish,
                cancellation,
                Some(&stage),
                &stage_name,
            )
            .is_err()
        {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                injected_commit_failure(),
                cleanup,
            ));
        }
        // This is the last cooperative cancellation point before the single
        // publication syscall. Once link/rename starts, its observed outcome
        // is authoritative even if cancellation arrives concurrently.
        if cancellation.is_cancelled() {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                ToolCallError::aborted(),
                cleanup,
            ));
        }

        let published = match self.operation {
            WorkspaceMutationOperation::Create => {
                stage.hard_link("candidate", &self.parent, &self.target_name)
            }
            WorkspaceMutationOperation::Update => {
                stage.rename("candidate", &self.parent, &self.target_name)
            }
        };
        if let Err(error) = published {
            let cleanup = cleanup_staging(stage, &self.parent, &stage_name);
            let failure = publication_error(self.operation, error, &self.display);
            return Ok(WorkspaceCommitStatus::not_committed_after_cleanup(
                failure, cleanup,
            ));
        }

        #[cfg(test)]
        let post_publish_result = self.run_test_commit_phase(
            MutationCommitTestPhase::AfterPublish,
            cancellation,
            Some(&stage),
            &stage_name,
        );
        #[cfg(test)]
        let cleanup_result = self
            .run_test_commit_phase(
                MutationCommitTestPhase::BeforeCleanup,
                cancellation,
                Some(&stage),
                &stage_name,
            )
            .and_then(|()| cleanup_staging(stage, &self.parent, &stage_name));
        #[cfg(not(test))]
        let cleanup_result = cleanup_staging(stage, &self.parent, &stage_name);
        #[cfg(test)]
        let sync_result = post_publish_result
            .and_then(|()| {
                self.run_test_commit_phase(
                    MutationCommitTestPhase::BeforeParentSync,
                    cancellation,
                    None,
                    &stage_name,
                )
            })
            .and_then(|()| sync_directory(&self.parent));
        #[cfg(not(test))]
        let sync_result = sync_directory(&self.parent);
        Ok(WorkspaceCommitStatus::Committed {
            durability_uncertain: sync_result.is_err(),
            cleanup_warning: cleanup_result.is_err(),
        })
    }

    fn revalidate(&self, cancellation: &CancellationToken) -> Option<ToolCallError> {
        if cancellation.is_cancelled() {
            return Some(ToolCallError::aborted());
        }
        let reopened =
            match open_parent_no_follow(&self.root, &self.parent_relative, Some(cancellation)) {
                Ok(parent) => parent,
                Err(_) if cancellation.is_cancelled() => {
                    return Some(ToolCallError::aborted());
                }
                Err(_) => {
                    return Some(ToolCallError::model(
                        "FileConflictError",
                        "FILE_CONFLICT",
                        format!(
                            "workspace parent for `{}` changed after preparation",
                            self.display
                        ),
                    ));
                }
            };
        let same_parent = reopened.dir_metadata().is_ok_and(|metadata| {
            metadata.dev() == self.parent_dev && metadata.ino() == self.parent_ino
        });
        if !same_parent {
            return Some(ToolCallError::model(
                "FileConflictError",
                "FILE_CONFLICT",
                format!(
                    "workspace parent for `{}` changed after preparation",
                    self.display
                ),
            ));
        }
        if cancellation.is_cancelled() {
            return Some(ToolCallError::aborted());
        }
        match self.operation {
            WorkspaceMutationOperation::Create => {
                match self.parent.symlink_metadata(&self.target_name) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Ok(_) => Some(ToolCallError::model(
                        "FileConflictError",
                        "FILE_ALREADY_EXISTS",
                        format!("workspace file `{}` already exists", self.display),
                    )),
                    Err(error) => Some(ToolCallError::io(&error, &self.display, false)),
                }
            }
            WorkspaceMutationOperation::Update => {
                let baseline = self.baseline.as_deref().unwrap_or_default();
                let Some(snapshot) = self.snapshot else {
                    return Some(ToolCallError::model(
                        "ToolError",
                        "FILE_CONFLICT",
                        "prepared update lost its baseline identity",
                    ));
                };
                match read_mutation_target(
                    &self.parent,
                    &self.target_name,
                    baseline.len(),
                    Some(cancellation),
                ) {
                    Ok((current, current_snapshot))
                        if current == baseline
                            && current_snapshot.dev == snapshot.dev
                            && current_snapshot.ino == snapshot.ino
                            && current_snapshot.len == snapshot.len
                            && current_snapshot.mode & 0o777 == snapshot.mode & 0o777
                            && current_snapshot.nlink == snapshot.nlink
                            && current_snapshot.mtime == snapshot.mtime
                            && current_snapshot.mtime_nsec == snapshot.mtime_nsec
                            && current_snapshot.ctime == snapshot.ctime
                            && current_snapshot.ctime_nsec == snapshot.ctime_nsec =>
                    {
                        None
                    }
                    Ok(_) => Some(ToolCallError::model(
                        "FileConflictError",
                        "FILE_CONFLICT",
                        format!(
                            "workspace file `{}` changed after preparation",
                            self.display
                        ),
                    )),
                    Err(error) if error.has_code("ABORTED") => Some(error),
                    Err(_) => Some(ToolCallError::model(
                        "FileConflictError",
                        "FILE_CONFLICT",
                        format!(
                            "workspace file `{}` changed after preparation",
                            self.display
                        ),
                    )),
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
fn injected_commit_failure() -> ToolCallError {
    ToolCallError::model(
        "FsError",
        "FS_IO_ERROR",
        "injected workspace mutation failure",
    )
}

#[cfg(unix)]
fn publication_error(
    operation: WorkspaceMutationOperation,
    error: io::Error,
    display: &str,
) -> ToolCallError {
    if operation == WorkspaceMutationOperation::Create
        && error.kind() == io::ErrorKind::AlreadyExists
    {
        return ToolCallError::model(
            "FileConflictError",
            "FILE_ALREADY_EXISTS",
            format!("workspace file `{display}` already exists"),
        );
    }
    if error.kind() == io::ErrorKind::PermissionDenied {
        return ToolCallError::model(
            "FsError",
            "FS_PERMISSION_DENIED",
            format!("permission denied while publishing workspace file `{display}`"),
        );
    }
    ToolCallError::model(
        "FsError",
        "FS_IO_ERROR",
        format!("could not publish workspace file `{display}`"),
    )
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolExecutorCommitError;

impl Workspace {
    pub(crate) fn open(path: &Path) -> Result<Self, ToolRegistryBuildError> {
        let authority = WorkspaceAuthority::open(path).map_err(|source| {
            ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source,
            }
        })?;
        Ok(Self::from_authority(authority))
    }

    pub(crate) fn from_authority(authority: WorkspaceAuthority) -> Self {
        Self {
            authority,
            mutation_lock: Arc::new(Mutex::new(())),
            entropy: EntropySource::system(),
        }
    }

    pub(crate) fn display_root(&self) -> &Path {
        self.authority.canonical_path()
    }

    #[cfg(unix)]
    pub(crate) async fn prepare_mutation(
        &self,
        path: ResolvedPath,
        operation: WorkspaceMutationOperation,
        maximum_bytes: usize,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<PreparedWorkspaceMutation> {
        check_cancel(cancellation)?;
        let root = Arc::clone(self.authority.root());
        let mutation_lock = Arc::clone(&self.mutation_lock);
        let entropy = self.entropy;
        let token = cancellation.clone();
        let prepared = task::spawn_blocking(move || {
            if token.is_cancelled() {
                return Err(ToolCallError::aborted());
            }
            let parent_relative = path.relative.parent().unwrap_or_else(|| Path::new("."));
            let target_name = path
                .relative
                .file_name()
                .ok_or_else(ToolCallError::workspace_denied)?
                .to_os_string();
            let parent = Arc::new(
                open_parent_no_follow(&root, parent_relative, Some(&token)).map_err(|error| {
                    if token.is_cancelled() {
                        ToolCallError::aborted()
                    } else {
                        ToolCallError::io(&error, &path.display, true)
                    }
                })?,
            );
            let parent_metadata = parent
                .dir_metadata()
                .map_err(|error| ToolCallError::io(&error, &path.display, true))?;
            if token.is_cancelled() {
                return Err(ToolCallError::aborted());
            }
            let (baseline, snapshot) = match operation {
                WorkspaceMutationOperation::Create => match parent.symlink_metadata(&target_name) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (None, None),
                    Ok(metadata) if metadata.is_symlink() => {
                        return Err(ToolCallError::workspace_denied());
                    }
                    Ok(_) => {
                        return Err(ToolCallError::model(
                            "FileConflictError",
                            "FILE_ALREADY_EXISTS",
                            format!("workspace file `{}` already exists", path.display),
                        ));
                    }
                    Err(error) => {
                        return Err(ToolCallError::io(&error, &path.display, false));
                    }
                },
                WorkspaceMutationOperation::Update => {
                    let (bytes, snapshot) =
                        read_mutation_target(&parent, &target_name, maximum_bytes, Some(&token))?;
                    (Some(bytes), Some(snapshot))
                }
            };
            Ok(PreparedWorkspaceMutation {
                root,
                parent,
                parent_relative: parent_relative.to_owned(),
                parent_dev: parent_metadata.dev(),
                parent_ino: parent_metadata.ino(),
                target_name,
                display: path.display,
                operation,
                baseline,
                snapshot,
                mutation_lock,
                entropy,
                #[cfg(test)]
                test_commit_hook: None,
            })
        })
        .await
        .map_err(|_| ToolCallError::Infrastructure)??;
        check_cancel(cancellation)?;
        Ok(prepared)
    }

    pub(crate) fn resolve(&self, input: &str) -> ToolCallResult<ResolvedPath> {
        if input.len() > MAX_TOOL_ARGUMENT_STRING_BYTES
            || input.is_empty()
            || input.chars().any(char::is_control)
        {
            return Err(ToolCallError::invalid_args(
                "workspace path is empty, overlong, or contains a control character",
            ));
        }
        let supplied = Path::new(input);
        let relative = if supplied.is_absolute() {
            let normalized = normalize_absolute(supplied)?;
            normalized
                .strip_prefix(self.authority.canonical_path())
                .or_else(|_| normalized.strip_prefix(self.authority.startup_path()))
                .map_err(|_| ToolCallError::workspace_denied())?
                .to_owned()
        } else {
            normalize_relative(supplied)?
        };
        let relative = if relative.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            relative
        };
        let display = display_path(&relative)
            .map_err(|error| map_blocking_error(error, "the requested workspace path", false))?;
        Ok(ResolvedPath { relative, display })
    }

    /// Resolve a shell working directory without touching the filesystem.
    /// The sealed Action setup performs the blocking capability open later.
    #[cfg(unix)]
    pub(crate) fn resolve_shell_workdir(&self, input: &str) -> ToolCallResult<ResolvedPath> {
        let resolved = self.resolve(input).map_err(|error| {
            if error.has_code("WORKSPACE_PATH_DENIED") {
                ToolCallError::shell_workdir_outside_workspace()
            } else {
                error
            }
        })?;
        if shell_component_count(&resolved.relative) > MAX_DIRECTORY_DEPTH {
            return Err(ToolCallError::invalid_args(format!(
                "bash.workdir must contain at most {MAX_DIRECTORY_DEPTH} path components"
            )));
        }
        Ok(resolved)
    }

    /// Perform the blocking, capability-relative workdir open owned by the
    /// sealed shell setup job. No caller should invoke this on a Tokio worker.
    #[cfg(unix)]
    pub(crate) fn prepare_shell_workdir(
        &self,
        path: ResolvedPath,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<PreparedShellWorkdir> {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        let directory = open_shell_directory_no_follow(
            self.authority.root(),
            &path.relative,
            Some(cancellation),
        )
        .map_err(|error| map_shell_workdir_open_error(&error, cancellation, false))?;
        let metadata = directory
            .dir_metadata()
            .map_err(|error| map_shell_workdir_open_error(&error, cancellation, false))?;
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        Ok(PreparedShellWorkdir {
            root: Arc::clone(self.authority.root()),
            directory,
            relative: path.relative,
            display: path.display,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    pub(crate) async fn classify(
        &self,
        path: &ResolvedPath,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<EntryKind> {
        check_cancel(cancellation)?;
        let root = Arc::clone(self.authority.root());
        let relative = path.relative.clone();
        let display = path.display.clone();
        let result = task::spawn_blocking(move || {
            let symlinks = path_symlinks(&root, &relative)?;
            if symlinks == PathSymlinks::Intermediate {
                return Err(BlockingError::UnsafeSymlink);
            }
            let metadata = root.metadata(&relative).map_err(map_resolve_error)?;
            Ok::<_, BlockingError>(if symlinks == PathSymlinks::Final && metadata.is_dir() {
                EntryKind::Symlink
            } else if metadata.is_file() {
                EntryKind::File
            } else if metadata.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::Other
            })
        })
        .await
        .map_err(|_| ToolCallError::Infrastructure)?;
        check_cancel(cancellation)?;
        result.map_err(|error| map_blocking_error(error, &display, false))
    }

    pub(crate) async fn read_directory(
        &self,
        path: &ResolvedPath,
        maximum_entries: usize,
        maximum_path_bytes: usize,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<Vec<WorkspaceEntry>> {
        check_cancel(cancellation)?;
        let root = Arc::clone(self.authority.root());
        let relative = path.relative.clone();
        let display = path.display.clone();
        let cursor = task::spawn_blocking(move || {
            if path_symlinks(&root, &relative)? != PathSymlinks::None {
                return Err(BlockingError::UnsafeSymlink);
            }
            let metadata = root.metadata(&relative).map_err(BlockingError::Io)?;
            if !metadata.is_dir() {
                return Err(BlockingError::NotDirectory);
            }
            root.read_dir(&relative).map_err(BlockingError::Io)
        })
        .await
        .map_err(|_| ToolCallError::Infrastructure)?;
        let mut cursor = cursor.map_err(|error| map_blocking_error(error, &display, true))?;
        check_cancel(cancellation)?;

        let mut collected = Vec::new();
        let mut retained_path_bytes = 0_usize;
        loop {
            let token = cancellation.clone();
            let relative = path.relative.clone();
            let already_collected = collected.len();
            let batch = task::spawn_blocking(move || {
                let mut batch = Vec::new();
                let mut batch_path_bytes = 0_usize;
                let mut exhausted = false;
                for _ in 0..DIRECTORY_BATCH_ENTRIES {
                    if token.is_cancelled() {
                        return Err(BlockingError::Aborted);
                    }
                    let Some(item) = cursor.next() else {
                        exhausted = true;
                        break;
                    };
                    if already_collected + batch.len() >= maximum_entries {
                        return Err(BlockingError::TooManyEntries);
                    }
                    let item = item.map_err(BlockingError::Io)?;
                    let name_os = item.file_name();
                    let name = os_string_to_utf8(name_os)?;
                    let entry_relative = relative.join(&name);
                    let entry_display = display_path(&entry_relative)?;
                    batch_path_bytes = batch_path_bytes
                        .checked_add(entry_display.len())
                        .ok_or(BlockingError::TooManyPathBytes)?;
                    if retained_path_bytes.saturating_add(batch_path_bytes) > maximum_path_bytes {
                        return Err(BlockingError::TooManyPathBytes);
                    }
                    let file_type = item.file_type().map_err(BlockingError::Io)?;
                    let kind = if file_type.is_symlink() {
                        EntryKind::Symlink
                    } else if file_type.is_file() {
                        EntryKind::File
                    } else if file_type.is_dir() {
                        EntryKind::Directory
                    } else {
                        EntryKind::Other
                    };
                    let (size, modified) = if matches!(kind, EntryKind::File) {
                        let metadata = item.metadata().map_err(BlockingError::Io)?;
                        (
                            Some(metadata.len()),
                            metadata.modified().ok().map(|value| value.into_std()),
                        )
                    } else {
                        (None, None)
                    };
                    batch.push(WorkspaceEntry {
                        relative: entry_relative,
                        display: entry_display,
                        name,
                        kind,
                        size,
                        modified,
                    });
                }
                Ok((cursor, batch, batch_path_bytes, exhausted))
            })
            .await
            .map_err(|_| ToolCallError::Infrastructure)?;
            let (next_cursor, batch, batch_path_bytes, exhausted) =
                batch.map_err(|error| map_blocking_error(error, &display, true))?;
            cursor = next_cursor;
            retained_path_bytes += batch_path_bytes;
            collected.extend(batch);
            check_cancel(cancellation)?;
            if exhausted {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(collected)
    }

    pub(crate) async fn walk_files(
        &self,
        start: &ResolvedPath,
        maximum_entries: usize,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<Vec<WorkspaceFile>> {
        let kind = self.classify(start, cancellation).await?;
        if kind != EntryKind::Directory {
            return Err(ToolCallError::not_directory(&start.display));
        }

        let mut visited = 0_usize;
        let mut retained_path_bytes = 0_usize;
        let mut queue = VecDeque::from([(start.clone(), 0_usize)]);
        let mut files = Vec::new();
        while let Some((directory, depth)) = queue.pop_front() {
            check_cancel(cancellation)?;
            if depth > MAX_DIRECTORY_DEPTH {
                return Err(ToolCallError::search_limit(format!(
                    "directory traversal exceeds depth {MAX_DIRECTORY_DEPTH}"
                )));
            }
            let remaining = maximum_entries.saturating_sub(visited);
            let mut entries = self
                .read_directory(
                    &directory,
                    remaining,
                    MAX_TRAVERSAL_PATH_BYTES.saturating_sub(retained_path_bytes),
                    cancellation,
                )
                .await?;
            entries.sort_by(|left, right| left.display.as_bytes().cmp(right.display.as_bytes()));
            for entry in entries {
                retained_path_bytes = retained_path_bytes
                    .checked_add(entry.display.len())
                    .ok_or_else(|| {
                        ToolCallError::search_limit("directory path byte count overflow")
                    })?;
                visited = visited
                    .checked_add(1)
                    .ok_or_else(|| ToolCallError::search_limit("directory entry count overflow"))?;
                match entry.kind {
                    EntryKind::File => files.push(WorkspaceFile {
                        relative: entry.relative,
                        display: entry.display,
                        modified: entry.modified.unwrap_or(SystemTime::UNIX_EPOCH),
                    }),
                    EntryKind::Directory if !is_vcs_directory(&entry.name) => queue.push_back((
                        ResolvedPath {
                            relative: entry.relative,
                            display: entry.display,
                        },
                        depth + 1,
                    )),
                    EntryKind::Directory | EntryKind::Symlink | EntryKind::Other => {}
                }
            }
            tokio::task::yield_now().await;
        }
        Ok(files)
    }

    pub(crate) async fn read_file(
        &self,
        path: &ResolvedPath,
        maximum_bytes: usize,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<ReadFile> {
        check_cancel(cancellation)?;
        let root = Arc::clone(self.authority.root());
        let relative = path.relative.clone();
        let display = path.display.clone();
        let opened = task::spawn_blocking(move || {
            let symlinks = path_symlinks(&root, &relative)?;
            if symlinks == PathSymlinks::Intermediate {
                return Err(BlockingError::UnsafeSymlink);
            }
            let metadata = root.metadata(&relative).map_err(map_resolve_error)?;
            if !metadata.is_file() {
                return Err(BlockingError::NotRegularFile);
            }
            if metadata.len() > u64::try_from(maximum_bytes).unwrap_or(u64::MAX) {
                return Err(BlockingError::TooLarge);
            }

            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
            let file = root
                .open_with(&relative, &options)
                .map_err(map_resolve_error)?
                .into_std();
            let metadata = file.metadata().map_err(BlockingError::Io)?;
            if !metadata.is_file() {
                return Err(BlockingError::NotRegularFile);
            }
            if metadata.len() > u64::try_from(maximum_bytes).unwrap_or(u64::MAX) {
                return Err(BlockingError::TooLarge);
            }
            Ok::<_, BlockingError>(OpenedFile {
                file,
                initial_len: metadata.len(),
                initial_modified: metadata.modified().ok(),
            })
        })
        .await
        .map_err(|_| ToolCallError::Infrastructure)?;
        let mut opened = opened.map_err(|error| map_file_error(error, &display))?;
        check_cancel(cancellation)?;

        let initial_capacity = usize::try_from(opened.initial_len)
            .unwrap_or(maximum_bytes)
            .min(maximum_bytes);
        let mut bytes = Vec::with_capacity(initial_capacity);
        loop {
            check_cancel(cancellation)?;
            let mut file = opened.file;
            let chunk = task::spawn_blocking(move || {
                let mut buffer = vec![0_u8; MAX_READ_CHUNK_BYTES];
                let read = file.read(&mut buffer).map_err(BlockingError::Io)?;
                buffer.truncate(read);
                Ok::<_, BlockingError>((file, buffer))
            })
            .await
            .map_err(|_| ToolCallError::Infrastructure)?;
            let (file, chunk) = chunk.map_err(|error| map_file_error(error, &display))?;
            opened.file = file;
            check_cancel(cancellation)?;
            if chunk.is_empty() {
                break;
            }
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| ToolCallError::too_large(&display))?;
            if next_len > maximum_bytes {
                return Err(ToolCallError::too_large(&display));
            }
            bytes.extend_from_slice(&chunk);
        }

        check_cancel(cancellation)?;
        let file = opened.file;
        let final_metadata = task::spawn_blocking(move || file.metadata())
            .await
            .map_err(|_| ToolCallError::Infrastructure)?
            .map_err(|error| ToolCallError::io(&error, &display, false))?;
        check_cancel(cancellation)?;
        if file_changed(
            opened.initial_len,
            opened.initial_modified,
            bytes.len(),
            final_metadata.len(),
            final_metadata.modified().ok(),
        ) {
            return Err(ToolCallError::changed(&display));
        }
        Ok(ReadFile { bytes })
    }
}

#[cfg(unix)]
fn open_parent_no_follow(
    root: &Dir,
    relative: &Path,
    cancellation: Option<&CancellationToken>,
) -> io::Result<Dir> {
    let mut current = root.try_clone()?;
    for component in relative.components() {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace mutation was cancelled",
            ));
        }
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                current = open_child_directory_no_follow(&current, Path::new(name))?;
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "mutation parent is outside the workspace capability",
                ));
            }
        }
    }
    Ok(current)
}

#[cfg(unix)]
fn open_child_directory_no_follow(parent: &Dir, name: &Path) -> io::Result<Dir> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| {
        if parent
            .symlink_metadata(name)
            .is_ok_and(|metadata| metadata.is_symlink())
        {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace mutation path crosses a symbolic link",
            )
        } else {
            io::Error::from(error)
        }
    })?;
    Ok(Dir::from_std_file(std::fs::File::from(descriptor)))
}

#[cfg(unix)]
fn open_shell_directory_no_follow(
    root: &Dir,
    relative: &Path,
    cancellation: Option<&CancellationToken>,
) -> io::Result<Dir> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "shell workdir preparation was cancelled",
        ));
    }
    if relative == Path::new(".") || relative.as_os_str().is_empty() {
        open_child_directory_no_follow(root, Path::new("."))
    } else {
        // This helper opens every normal component with O_NOFOLLOW; despite its
        // historical mutation-oriented name it traverses the complete path.
        open_parent_no_follow(root, relative, cancellation)
    }
}

#[cfg(unix)]
fn map_shell_workdir_open_error(
    error: &io::Error,
    cancellation: &CancellationToken,
    revalidating: bool,
) -> ToolCallError {
    if cancellation.is_cancelled() || error.kind() == io::ErrorKind::Interrupted {
        return ToolCallError::aborted();
    }
    if revalidating {
        return ToolCallError::shell_workdir_changed();
    }
    match error.kind() {
        io::ErrorKind::NotFound => ToolCallError::shell_workdir_not_found(),
        io::ErrorKind::NotADirectory => ToolCallError::shell_workdir_not_directory(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput => {
            ToolCallError::shell_workdir_outside_workspace()
        }
        _ => ToolCallError::shell_workdir_changed(),
    }
}

#[cfg(unix)]
fn sync_directory(directory: &Dir) -> io::Result<()> {
    // cap-std intentionally opens ambient directory capabilities with O_PATH
    // on Linux.  O_PATH is suitable as an openat base but cannot be fsynced,
    // so obtain a read-only handle to the same capability-relative directory
    // before asking the kernel to make its entries durable.
    open_child_directory_no_follow(directory, Path::new("."))?
        .into_std_file()
        .sync_all()
}

#[cfg(unix)]
fn read_mutation_target(
    parent: &Dir,
    name: &OsString,
    maximum_bytes: usize,
    cancellation: Option<&CancellationToken>,
) -> ToolCallResult<(Vec<u8>, MutationSnapshot)> {
    let metadata = parent.symlink_metadata(name).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ToolCallError::model(
                "FsError",
                "FILE_NOT_FOUND",
                "the update target does not exist",
            )
        } else {
            ToolCallError::io(&error, "the update target", false)
        }
    })?;
    if metadata.is_symlink() {
        return Err(ToolCallError::workspace_denied());
    }
    if !metadata.is_file() {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_NOT_REGULAR",
            "the update target is not a regular file",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_HARDLINK_DENIED",
            "the update target has more than one hard link",
        ));
    }
    if metadata.len() > u64::try_from(maximum_bytes).unwrap_or(u64::MAX) {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_TOO_LARGE",
            "the update target exceeds the mutation file limit",
        ));
    }
    let descriptor = rustix::fs::openat(
        parent,
        Path::new(name),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| ToolCallError::io(&io::Error::from(error), "the update target", false))?;
    let mut file = std::fs::File::from(descriptor);
    let opened = file
        .metadata()
        .map_err(|error| ToolCallError::io(&error, "the update target", false))?;
    if !opened.is_file() {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_NOT_REGULAR",
            "the update target is not a regular file",
        ));
    }
    if std::os::unix::fs::MetadataExt::nlink(&opened) != 1 {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_HARDLINK_DENIED",
            "the update target has more than one hard link",
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(opened.len())
            .unwrap_or(maximum_bytes)
            .min(maximum_bytes),
    );
    loop {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(ToolCallError::aborted());
        }
        let mut chunk = [0_u8; MAX_READ_CHUNK_BYTES];
        let count = file
            .read(&mut chunk)
            .map_err(|error| ToolCallError::io(&error, "the update target", false))?;
        if count == 0 {
            break;
        }
        let next = bytes.len().checked_add(count).ok_or_else(|| {
            ToolCallError::model(
                "FsError",
                "FILE_TOO_LARGE",
                "the update target exceeds the mutation file limit",
            )
        })?;
        if next > maximum_bytes {
            return Err(ToolCallError::model(
                "FsError",
                "FILE_TOO_LARGE",
                "the update target exceeds the mutation file limit",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let final_metadata = file
        .metadata()
        .map_err(|error| ToolCallError::io(&error, "the update target", false))?;
    let snapshot = MutationSnapshot {
        dev: std::os::unix::fs::MetadataExt::dev(&opened),
        ino: std::os::unix::fs::MetadataExt::ino(&opened),
        len: opened.len(),
        mode: std::os::unix::fs::MetadataExt::mode(&opened),
        nlink: std::os::unix::fs::MetadataExt::nlink(&opened),
        mtime: std::os::unix::fs::MetadataExt::mtime(&opened),
        mtime_nsec: std::os::unix::fs::MetadataExt::mtime_nsec(&opened),
        ctime: std::os::unix::fs::MetadataExt::ctime(&opened),
        ctime_nsec: std::os::unix::fs::MetadataExt::ctime_nsec(&opened),
    };
    if final_metadata.len() != opened.len()
        || std::os::unix::fs::MetadataExt::dev(&final_metadata) != snapshot.dev
        || std::os::unix::fs::MetadataExt::ino(&final_metadata) != snapshot.ino
        || std::os::unix::fs::MetadataExt::mode(&final_metadata) & 0o777 != snapshot.mode & 0o777
        || std::os::unix::fs::MetadataExt::nlink(&final_metadata) != snapshot.nlink
        || std::os::unix::fs::MetadataExt::mtime(&final_metadata) != snapshot.mtime
        || std::os::unix::fs::MetadataExt::mtime_nsec(&final_metadata) != snapshot.mtime_nsec
        || std::os::unix::fs::MetadataExt::ctime(&final_metadata) != snapshot.ctime
        || std::os::unix::fs::MetadataExt::ctime_nsec(&final_metadata) != snapshot.ctime_nsec
        || snapshot.nlink != 1
        || u64::try_from(bytes.len()).ok() != Some(snapshot.len)
    {
        return Err(ToolCallError::model(
            "FileConflictError",
            "FILE_CONFLICT",
            "the update target changed while it was being prepared",
        ));
    }
    Ok((bytes, snapshot))
}

#[cfg(unix)]
fn create_staging_file(stage: &Dir) -> io::Result<cap_std::fs::File> {
    let descriptor = rustix::fs::openat(
        stage,
        "candidate",
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_bits_retain(0o600),
    )?;
    Ok(cap_std::fs::File::from_std(std::fs::File::from(descriptor)))
}

#[cfg(unix)]
fn cleanup_staging(stage: Dir, parent: &Dir, stage_name: &OsString) -> io::Result<()> {
    match stage.remove_file("candidate") {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    drop(stage);
    parent.remove_dir(stage_name)
}

#[cfg(unix)]
fn write_staging_file(
    mut file: cap_std::fs::File,
    candidate: &[u8],
    mode: u32,
    cancellation: &CancellationToken,
    #[cfg(test)] test_commit_hook: Option<&MutationCommitTestHook>,
    #[cfg(test)] stage: &Dir,
    #[cfg(test)] stage_name: &OsString,
) -> ToolCallResult<()> {
    for chunk in candidate.chunks(MAX_READ_CHUNK_BYTES) {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        file.write_all(chunk)
            .map_err(|error| ToolCallError::io(&error, "the private staging file", false))?;
        #[cfg(test)]
        if let Some(hook) = test_commit_hook {
            hook(
                MutationCommitTestPhase::StagingChunkWritten,
                cancellation,
                Some(stage),
                stage_name,
            )
            .map_err(|_| injected_commit_failure())?;
        }
    }
    if cancellation.is_cancelled() {
        return Err(ToolCallError::aborted());
    }
    file.set_permissions(cap_std::fs::Permissions::from_mode(mode & 0o777))
        .map_err(|error| ToolCallError::io(&error, "the private staging file", false))?;
    #[cfg(test)]
    if let Some(hook) = test_commit_hook {
        hook(
            MutationCommitTestPhase::BeforeStagingSync,
            cancellation,
            Some(stage),
            stage_name,
        )
        .map_err(|_| injected_commit_failure())?;
    }
    file.sync_all()
        .map_err(|error| ToolCallError::io(&error, "the private staging file", false))?;
    Ok(())
}

fn file_changed(
    initial_len: u64,
    initial_modified: Option<SystemTime>,
    bytes_read: usize,
    final_len: u64,
    final_modified: Option<SystemTime>,
) -> bool {
    final_len != initial_len
        || u64::try_from(bytes_read).ok() != Some(initial_len)
        || (initial_modified.is_some() && final_modified != initial_modified)
}

#[derive(Clone)]
pub(crate) struct ResolvedPath {
    pub(crate) relative: PathBuf,
    pub(crate) display: String,
}

struct OpenedFile {
    file: std::fs::File,
    initial_len: u64,
    initial_modified: Option<SystemTime>,
}

#[derive(Debug)]
enum BlockingError {
    Io(io::Error),
    Resolve(io::Error),
    Aborted,
    InvalidName,
    UnsafeSymlink,
    TooManyEntries,
    TooManyPathBytes,
    TooLarge,
    NotDirectory,
    NotRegularFile,
}

fn map_blocking_error(error: BlockingError, path: &str, directory: bool) -> ToolCallError {
    match error {
        BlockingError::Resolve(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
            ) =>
        {
            ToolCallError::workspace_denied()
        }
        BlockingError::Resolve(error) | BlockingError::Io(error) => {
            ToolCallError::io(&error, path, directory)
        }
        BlockingError::Aborted => ToolCallError::aborted(),
        BlockingError::InvalidName => ToolCallError::model(
            "FsError",
            "FS_INVALID_NAME",
            "the workspace contains a non-UTF-8 or control-character file name",
        ),
        BlockingError::UnsafeSymlink => ToolCallError::workspace_denied(),
        BlockingError::TooManyEntries => {
            ToolCallError::search_limit("directory traversal exceeds the configured entry limit")
        }
        BlockingError::TooManyPathBytes => ToolCallError::search_limit(format!(
            "directory traversal retains more than {MAX_TRAVERSAL_PATH_BYTES} path bytes"
        )),
        BlockingError::TooLarge => ToolCallError::too_large(path),
        BlockingError::NotDirectory => ToolCallError::not_directory(path),
        BlockingError::NotRegularFile => ToolCallError::not_regular_file(path),
    }
}

fn map_file_error(error: BlockingError, path: &str) -> ToolCallError {
    match error {
        BlockingError::TooLarge => ToolCallError::too_large(path),
        BlockingError::NotRegularFile => ToolCallError::not_regular_file(path),
        other => map_blocking_error(other, path, false),
    }
}

fn check_cancel(cancellation: &CancellationToken) -> ToolCallResult<()> {
    if cancellation.is_cancelled() {
        Err(ToolCallError::aborted())
    } else {
        Ok(())
    }
}

fn os_string_to_utf8(value: OsString) -> Result<String, BlockingError> {
    value.into_string().map_err(|_| BlockingError::InvalidName)
}

fn map_resolve_error(error: io::Error) -> BlockingError {
    // cap-primitives 3.4.5 deliberately emits this stable synthetic
    // PermissionDenied error for an attempted capability escape.  Ordinary
    // EACCES/EPERM failures keep their OS error and must remain distinguishable.
    if error.kind() == io::ErrorKind::PermissionDenied
        && error.to_string() == "a path led outside of the filesystem"
    {
        BlockingError::Resolve(error)
    } else {
        BlockingError::Io(error)
    }
}

fn path_symlinks(root: &Dir, path: &Path) -> Result<PathSymlinks, BlockingError> {
    let mut prefix = PathBuf::new();
    let mut components = path.components().filter_map(|component| {
        let Component::Normal(part) = component else {
            return None;
        };
        Some(part)
    });
    while let Some(part) = components.next() {
        prefix.push(part);
        let metadata = root.symlink_metadata(&prefix).map_err(BlockingError::Io)?;
        if metadata.file_type().is_symlink() {
            return Ok(if components.next().is_some() {
                PathSymlinks::Intermediate
            } else {
                PathSymlinks::Final
            });
        }
    }
    Ok(PathSymlinks::None)
}

fn normalize_relative(path: &Path) -> ToolCallResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ToolCallError::workspace_denied());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolCallError::workspace_denied());
            }
        }
    }
    Ok(normalized)
}

fn normalize_absolute(path: &Path) -> ToolCallResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ToolCallError::workspace_denied());
                }
            }
        }
    }
    Ok(normalized)
}

fn display_path(path: &Path) -> Result<String, BlockingError> {
    if path == Path::new(".") || path.as_os_str().is_empty() {
        return Ok(".".to_owned());
    }
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let part = part.to_str().ok_or(BlockingError::InvalidName)?;
        if part.chars().any(char::is_control) {
            return Err(BlockingError::InvalidName);
        }
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(part);
    }
    Ok(output)
}

#[cfg(unix)]
fn shell_component_count(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
}

fn is_vcs_directory(name: &str) -> bool {
    matches!(name, ".git" | ".svn" | ".hg" | ".bzr" | ".jj" | ".sl")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use tokio_util::sync::CancellationToken;

    #[cfg(unix)]
    use super::{
        MutationCommitTestPhase, PreparedWorkspaceMutation, WorkspaceCommitStatus,
        WorkspaceMutationOperation, publication_error,
    };
    use super::{Workspace, file_changed};
    use crate::tools::error::ToolCallError;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let ordinal = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "dsh-workspace-unit-{}-{nanos}-{ordinal}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    async fn prepare_mutation(
        workspace: &Workspace,
        path: &str,
        operation: WorkspaceMutationOperation,
    ) -> PreparedWorkspaceMutation {
        let cancellation = CancellationToken::new();
        workspace
            .prepare_mutation(
                workspace.resolve(path).unwrap(),
                operation,
                1_024,
                &cancellation,
            )
            .await
            .unwrap()
    }

    #[cfg(unix)]
    async fn commit_mutation(
        prepared: PreparedWorkspaceMutation,
        candidate: Vec<u8>,
        cancellation: CancellationToken,
    ) -> WorkspaceCommitStatus {
        tokio::task::spawn_blocking(move || prepared.commit(&candidate, &cancellation).unwrap())
            .await
            .unwrap()
    }

    #[test]
    fn file_change_detection_checks_length_bytes_read_and_timestamp() {
        let timestamp = UNIX_EPOCH + std::time::Duration::from_secs(10);
        assert!(!file_changed(3, Some(timestamp), 3, 3, Some(timestamp)));
        assert!(file_changed(3, Some(timestamp), 3, 4, Some(timestamp)));
        assert!(file_changed(3, Some(timestamp), 2, 3, Some(timestamp)));
        assert!(file_changed(
            3,
            Some(timestamp),
            3,
            3,
            Some(timestamp + std::time::Duration::from_secs(1))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn publication_errors_distinguish_conflict_permission_and_io_failures() {
        let (_, code, _) = publication_error(
            WorkspaceMutationOperation::Create,
            std::io::Error::from(std::io::ErrorKind::AlreadyExists),
            "file.txt",
        )
        .into_model_parts()
        .unwrap();
        assert_eq!(code, "FILE_ALREADY_EXISTS");

        let (_, code, _) = publication_error(
            WorkspaceMutationOperation::Update,
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            "file.txt",
        )
        .into_model_parts()
        .unwrap();
        assert_eq!(code, "FS_PERMISSION_DENIED");

        let (_, code, _) = publication_error(
            WorkspaceMutationOperation::Update,
            std::io::Error::other("injected publication failure"),
            "file.txt",
        )
        .into_model_parts()
        .unwrap();
        assert_eq!(code, "FS_IO_ERROR");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_staging_or_before_publish_keeps_target_unchanged() {
        for phase in [
            MutationCommitTestPhase::StagingCreated,
            MutationCommitTestPhase::BeforePublish,
        ] {
            let root = TempRoot::new();
            let target = root.0.join("target.txt");
            fs::write(&target, b"old contents\n").unwrap();
            let workspace = Workspace::open(&root.0).unwrap();
            let mut prepared =
                prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Update)
                    .await;
            prepared.test_commit_hook = Some(Arc::new(move |seen, cancellation, _, _| {
                if seen == phase {
                    cancellation.cancel();
                }
                Ok(())
            }));
            let cancellation = CancellationToken::new();
            let observe_cancellation = cancellation.clone();

            let status = commit_mutation(prepared, b"new contents\n".to_vec(), cancellation).await;

            assert!(observe_cancellation.is_cancelled());
            match status {
                WorkspaceCommitStatus::NotCommitted {
                    error,
                    cleanup_warning,
                } => {
                    assert!(error.has_code("ABORTED"));
                    assert!(!cleanup_warning);
                }
                WorkspaceCommitStatus::Committed { .. } => {
                    panic!("cancellation before publication must not commit")
                }
            }
            assert_eq!(fs::read(&target).unwrap(), b"old contents\n");
            assert!(!fs::read_dir(&root.0).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dsh-stage-")
            }));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_first_staging_chunk_stops_before_the_next_chunk() {
        let root = TempRoot::new();
        let target = root.0.join("target.txt");
        fs::write(&target, b"old contents\n").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Update).await;
        let chunk_events = Arc::new(AtomicU64::new(0));
        let observe_chunk_events = Arc::clone(&chunk_events);
        let staged_bytes = Arc::new(AtomicU64::new(0));
        let observe_staged_bytes = Arc::clone(&staged_bytes);
        prepared.test_commit_hook = Some(Arc::new(move |phase, cancellation, stage, _| {
            if phase == MutationCommitTestPhase::StagingChunkWritten {
                observe_chunk_events.fetch_add(1, Ordering::SeqCst);
                let stage = stage.ok_or_else(|| {
                    std::io::Error::other("chunk hook did not receive the staging directory")
                })?;
                observe_staged_bytes.store(stage.metadata("candidate")?.len(), Ordering::SeqCst);
                cancellation.cancel();
            }
            Ok(())
        }));
        let cancellation = CancellationToken::new();
        let observe_cancellation = cancellation.clone();
        let candidate = vec![b'x'; super::MAX_READ_CHUNK_BYTES * 2 + 1];

        let status = commit_mutation(prepared, candidate, cancellation).await;

        assert!(observe_cancellation.is_cancelled());
        assert_eq!(chunk_events.load(Ordering::SeqCst), 1);
        assert_eq!(
            staged_bytes.load(Ordering::SeqCst),
            u64::try_from(super::MAX_READ_CHUNK_BYTES).unwrap()
        );
        match status {
            WorkspaceCommitStatus::NotCommitted {
                error,
                cleanup_warning,
            } => {
                assert!(error.has_code("ABORTED"));
                assert!(!cleanup_warning);
            }
            WorkspaceCommitStatus::Committed { .. } => {
                panic!("cancellation while staging must not commit")
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"old contents\n");
        assert!(!fs::read_dir(&root.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dsh-stage-")
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn late_revalidation_preserves_an_external_update() {
        let root = TempRoot::new();
        let target = root.0.join("target.txt");
        fs::write(&target, b"old contents\n").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Update).await;
        let external_target = target.clone();
        prepared.test_commit_hook = Some(Arc::new(move |phase, _, _, _| {
            if phase == MutationCommitTestPhase::BeforeLateRevalidate {
                fs::write(&external_target, b"external winner\n")?;
            }
            Ok(())
        }));

        let status = commit_mutation(
            prepared,
            b"agent candidate\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        match status {
            WorkspaceCommitStatus::NotCommitted {
                error,
                cleanup_warning,
            } => {
                assert!(error.has_code("FILE_CONFLICT"));
                assert!(!cleanup_warning);
            }
            WorkspaceCommitStatus::Committed { .. } => {
                panic!("the late external update must win")
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"external winner\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_publication_race_preserves_the_external_winner() {
        let root = TempRoot::new();
        let target = root.0.join("target.txt");
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Create).await;
        let external_target = target.clone();
        prepared.test_commit_hook = Some(Arc::new(move |phase, _, _, _| {
            if phase == MutationCommitTestPhase::BeforePublish {
                fs::write(&external_target, b"external winner\n")?;
            }
            Ok(())
        }));

        let status = commit_mutation(
            prepared,
            b"agent candidate\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        match status {
            WorkspaceCommitStatus::NotCommitted {
                error,
                cleanup_warning,
            } => {
                assert!(error.has_code("FILE_ALREADY_EXISTS"));
                assert!(!cleanup_warning);
            }
            WorkspaceCommitStatus::Committed { .. } => {
                panic!("guarded create must not replace the external winner")
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"external winner\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_file_sync_failure_never_publishes() {
        let root = TempRoot::new();
        let target = root.0.join("target.txt");
        fs::write(&target, b"old contents\n").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Update).await;
        prepared.test_commit_hook = Some(Arc::new(|phase, _, _, _| {
            if phase == MutationCommitTestPhase::BeforeStagingSync {
                return Err(std::io::Error::other("injected staging sync failure"));
            }
            Ok(())
        }));

        let status = commit_mutation(
            prepared,
            b"agent candidate\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        match status {
            WorkspaceCommitStatus::NotCommitted {
                error,
                cleanup_warning,
            } => {
                assert!(error.has_code("FS_IO_ERROR"));
                assert!(!cleanup_warning);
            }
            WorkspaceCommitStatus::Committed { .. } => {
                panic!("an unsynchronized staging file must not publish")
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"old contents\n");
        assert!(!fs::read_dir(&root.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dsh-stage-")
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_name_entropy_failure_is_definitely_not_committed() {
        fn failing_entropy(_bytes: &mut [u8]) -> Result<(), crate::entropy::EntropyError> {
            Err(crate::entropy::EntropyError)
        }

        let root = TempRoot::new();
        let target = root.0.join("target.txt");
        fs::write(&target, b"old contents\n").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Update).await;
        prepared.entropy = crate::entropy::EntropySource::injected(failing_entropy);

        let status = commit_mutation(
            prepared,
            b"agent candidate\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        match status {
            WorkspaceCommitStatus::NotCommitted {
                error,
                cleanup_warning,
            } => {
                assert!(error.has_code("FS_IO_ERROR"));
                assert!(!cleanup_warning);
            }
            WorkspaceCommitStatus::Committed { .. } => {
                panic!("entropy failure must happen before publication")
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"old contents\n");
        assert!(!fs::read_dir(&root.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".dsh-stage-")
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn precommit_cleanup_failure_warns_and_never_deletes_unknown_content() {
        let root = TempRoot::new();
        let target = root.0.join("target.txt");
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Create).await;
        let retained_stage = Arc::new(Mutex::new(None::<PathBuf>));
        let record_stage = Arc::clone(&retained_stage);
        let root_path = root.0.clone();
        prepared.test_commit_hook = Some(Arc::new(move |phase, _, stage, stage_name| {
            if phase == MutationCommitTestPhase::StagingCreated {
                let stage = stage.ok_or_else(|| {
                    std::io::Error::other("staging hook did not receive its directory")
                })?;
                stage.write("foreign.txt", b"must remain\n")?;
                *record_stage.lock().unwrap() = Some(root_path.join(stage_name));
                return Err(std::io::Error::other("injected precommit failure"));
            }
            Ok(())
        }));

        let status = commit_mutation(
            prepared,
            b"agent candidate\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        match status {
            WorkspaceCommitStatus::NotCommitted {
                error,
                cleanup_warning,
            } => {
                assert!(error.has_code("FS_IO_ERROR"));
                assert!(cleanup_warning);
            }
            WorkspaceCommitStatus::Committed { .. } => {
                panic!("precommit failure must not publish")
            }
        }
        assert!(!target.exists());
        let stage = retained_stage.lock().unwrap().clone().unwrap();
        assert_eq!(
            fs::read(stage.join("foreign.txt")).unwrap(),
            b"must remain\n"
        );
        assert!(!stage.join("candidate").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_publish_still_reports_committed() {
        let root = TempRoot::new();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Create).await;
        prepared.test_commit_hook = Some(Arc::new(|phase, cancellation, _, _| {
            if phase == MutationCommitTestPhase::AfterPublish {
                cancellation.cancel();
            }
            Ok(())
        }));
        let cancellation = CancellationToken::new();
        let observe_cancellation = cancellation.clone();

        let status =
            commit_mutation(prepared, b"published contents\n".to_vec(), cancellation).await;

        assert!(observe_cancellation.is_cancelled());
        assert!(matches!(
            status,
            WorkspaceCommitStatus::Committed {
                durability_uncertain: false,
                cleanup_warning: false,
            }
        ));
        assert_eq!(
            fs::read(root.0.join("target.txt")).unwrap(),
            b"published contents\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parent_sync_failure_reports_committed_with_uncertain_durability() {
        let root = TempRoot::new();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Create).await;
        prepared.test_commit_hook = Some(Arc::new(|phase, _, _, _| {
            if phase == MutationCommitTestPhase::BeforeParentSync {
                return Err(std::io::Error::other("injected parent sync failure"));
            }
            Ok(())
        }));

        let status = commit_mutation(
            prepared,
            b"published contents\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            status,
            WorkspaceCommitStatus::Committed {
                durability_uncertain: true,
                cleanup_warning: false,
            }
        ));
        assert_eq!(
            fs::read(root.0.join("target.txt")).unwrap(),
            b"published contents\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_failure_warns_without_recursively_deleting_unknown_content() {
        let root = TempRoot::new();
        let workspace = Workspace::open(&root.0).unwrap();
        let mut prepared =
            prepare_mutation(&workspace, "target.txt", WorkspaceMutationOperation::Create).await;
        let retained_stage = Arc::new(Mutex::new(None::<PathBuf>));
        let record_stage = Arc::clone(&retained_stage);
        let root_path = root.0.clone();
        prepared.test_commit_hook = Some(Arc::new(move |phase, _, stage, stage_name| {
            if phase == MutationCommitTestPhase::BeforeCleanup {
                let stage = stage.ok_or_else(|| {
                    std::io::Error::other("cleanup hook did not receive the staging directory")
                })?;
                stage.write("foreign.txt", b"must remain\n")?;
                *record_stage.lock().unwrap() = Some(root_path.join(stage_name));
            }
            Ok(())
        }));

        let status = commit_mutation(
            prepared,
            b"published contents\n".to_vec(),
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            status,
            WorkspaceCommitStatus::Committed {
                durability_uncertain: false,
                cleanup_warning: true,
            }
        ));
        assert_eq!(
            fs::read(root.0.join("target.txt")).unwrap(),
            b"published contents\n"
        );
        let stage = retained_stage.lock().unwrap().clone().unwrap();
        assert_eq!(
            fs::read(stage.join("foreign.txt")).unwrap(),
            b"must remain\n"
        );
        assert!(!stage.join("candidate").exists());
    }

    #[tokio::test]
    async fn file_and_directory_limits_accept_exactly_the_budget() {
        let root = TempRoot::new();
        fs::write(root.0.join("abc"), b"abc").unwrap();
        fs::write(root.0.join("de"), b"de").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let file = workspace.resolve("abc").unwrap();
        let token = CancellationToken::new();

        assert_eq!(
            workspace.read_file(&file, 3, &token).await.unwrap().bytes,
            b"abc"
        );
        assert!(matches!(
            workspace.read_file(&file, 2, &token).await,
            Err(ToolCallError::Model {
                code: "FS_TOO_LARGE",
                ..
            })
        ));

        let directory = workspace.resolve(".").unwrap();
        assert_eq!(
            workspace
                .read_directory(&directory, 2, 5, &token)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            workspace
                .read_directory(&directory, 1, 5, &token)
                .await
                .is_err()
        );
        assert!(
            workspace
                .read_directory(&directory, 2, 4, &token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn traversal_accepts_an_empty_directory_at_the_exact_entry_limit() {
        let root = TempRoot::new();
        fs::write(root.0.join("file"), b"x").unwrap();
        fs::create_dir(root.0.join("empty")).unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let directory = workspace.resolve(".").unwrap();
        let token = CancellationToken::new();

        assert_eq!(
            workspace
                .walk_files(&directory, 2, &token)
                .await
                .unwrap()
                .len(),
            1
        );

        fs::write(root.0.join("empty/over"), b"x").unwrap();
        assert!(workspace.walk_files(&directory, 2, &token).await.is_err());
    }

    #[tokio::test]
    async fn traversal_depth_accepts_the_limit_and_rejects_one_more_level() {
        let root = TempRoot::new();
        let mut deepest = root.0.clone();
        for index in 1..=super::MAX_DIRECTORY_DEPTH {
            deepest.push(format!("d{index}"));
            fs::create_dir(&deepest).unwrap();
        }
        fs::write(deepest.join("inside"), b"x").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let directory = workspace.resolve(".").unwrap();
        let token = CancellationToken::new();

        assert_eq!(
            workspace
                .walk_files(&directory, 1_000, &token)
                .await
                .unwrap()
                .len(),
            1
        );

        fs::create_dir(deepest.join("one-over")).unwrap();
        assert!(
            workspace
                .walk_files(&directory, 1_000, &token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_is_checked_before_new_filesystem_work() {
        let root = TempRoot::new();
        fs::write(root.0.join("sentinel"), b"secret").unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let file = workspace.resolve("sentinel").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            workspace.read_file(&file, 16, &token).await,
            Err(ToolCallError::Model {
                code: "ABORTED",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn cancellation_after_dispatch_stops_before_the_next_read_chunk() {
        let root = TempRoot::new();
        fs::write(root.0.join("sentinel"), vec![b'x'; 256 * 1024]).unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let file = workspace.resolve("sentinel").unwrap();
        let token = CancellationToken::new();
        let cancel = token.clone();
        let (result, ()) =
            tokio::join!(workspace.read_file(&file, 512 * 1024, &token), async move {
                // `join!` polls the read branch first, so its initial open is
                // dispatched before this sibling cancels the child token.
                cancel.cancel();
            });
        assert!(matches!(
            result,
            Err(ToolCallError::Model {
                code: "ABORTED",
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn shell_workdir_retains_and_revalidates_one_directory_capability() {
        let root = TempRoot::new();
        fs::create_dir(root.0.join("nested")).unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let token = CancellationToken::new();
        let resolved = workspace.resolve_shell_workdir("nested").unwrap();
        let prepared = workspace.prepare_shell_workdir(resolved, &token).unwrap();
        assert_eq!(prepared.display(), "nested");
        assert!(prepared.revalidate(&token).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn shell_workdir_rejects_symlinks_and_detects_replacement() {
        use std::os::unix::fs::symlink;

        let root = TempRoot::new();
        fs::create_dir(root.0.join("original")).unwrap();
        symlink("original", root.0.join("linked")).unwrap();
        let workspace = Workspace::open(&root.0).unwrap();
        let token = CancellationToken::new();

        let linked = workspace.resolve_shell_workdir("linked").unwrap();
        let error = workspace.prepare_shell_workdir(linked, &token).unwrap_err();
        assert!(error.has_code("SHELL_WORKDIR_OUTSIDE_WORKSPACE"));

        let original = workspace.resolve_shell_workdir("original").unwrap();
        let prepared = workspace.prepare_shell_workdir(original, &token).unwrap();
        fs::rename(root.0.join("original"), root.0.join("moved")).unwrap();
        fs::create_dir(root.0.join("original")).unwrap();
        let error = prepared.revalidate(&token).unwrap_err();
        assert!(error.has_code("SHELL_WORKDIR_CHANGED"));
    }

    #[cfg(unix)]
    #[test]
    fn shell_workdir_component_limit_is_exact() {
        let root = TempRoot::new();
        let workspace = Workspace::open(&root.0).unwrap();
        let exact = (0..super::MAX_DIRECTORY_DEPTH)
            .map(|index| format!("d{index}"))
            .collect::<Vec<_>>()
            .join("/");
        assert!(workspace.resolve_shell_workdir(&exact).is_ok());
        let one_over = format!("{exact}/extra");
        let error = match workspace.resolve_shell_workdir(&one_over) {
            Err(error) => error,
            Ok(_) => panic!("one-over shell path component count was accepted"),
        };
        assert!(error.has_code("INVALID_ARGS"));
    }
}
