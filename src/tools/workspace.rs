use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use tokio::task;
use tokio_util::sync::CancellationToken;

use super::{
    MAX_DIRECTORY_DEPTH, MAX_READ_CHUNK_BYTES, MAX_TRAVERSAL_PATH_BYTES,
    arguments::MAX_TOOL_ARGUMENT_STRING_BYTES,
    error::{ToolCallError, ToolCallResult, ToolRegistryBuildError},
};

const DIRECTORY_BATCH_ENTRIES: usize = 256;

#[derive(Clone)]
pub(crate) struct Workspace {
    root: Arc<Dir>,
    display_root: Arc<PathBuf>,
    startup_root: Arc<PathBuf>,
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

impl Workspace {
    pub(crate) fn open(path: &Path) -> Result<Self, ToolRegistryBuildError> {
        let startup_root = std::path::absolute(path).map_err(|source| {
            ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source,
            }
        })?;
        // Opening the capability is the authorization linearization point.  Do
        // this before resolving a display path so a rename between a pathname
        // check and the open cannot silently grant authority to a replacement.
        let root = Dir::open_ambient_dir(path, ambient_authority()).map_err(|source| {
            ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source,
            }
        })?;
        let opened_metadata =
            root.dir_metadata()
                .map_err(|source| ToolRegistryBuildError::InvalidWorkspace {
                    path: path.to_owned(),
                    source,
                })?;
        if !opened_metadata.is_dir() {
            return Err(ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source: io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "workspace is not a directory",
                ),
            });
        }

        let display_root = std::fs::canonicalize(path).map_err(|source| {
            ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source,
            }
        })?;
        let expected_metadata = std::fs::metadata(&display_root).map_err(|source| {
            ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source,
            }
        })?;
        if !expected_metadata.is_dir() {
            return Err(ToolRegistryBuildError::InvalidWorkspace {
                path: path.to_owned(),
                source: io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "workspace is not a directory",
                ),
            });
        }
        #[cfg(unix)]
        {
            if std::os::unix::fs::MetadataExt::dev(&expected_metadata)
                != cap_std::fs::MetadataExt::dev(&opened_metadata)
                || std::os::unix::fs::MetadataExt::ino(&expected_metadata)
                    != cap_std::fs::MetadataExt::ino(&opened_metadata)
            {
                return Err(ToolRegistryBuildError::InvalidWorkspace {
                    path: path.to_owned(),
                    source: io::Error::other("workspace changed while it was being opened"),
                });
            }
        }
        Ok(Self {
            root: Arc::new(root),
            display_root: Arc::new(display_root),
            startup_root: Arc::new(startup_root),
        })
    }

    pub(crate) fn display_root(&self) -> &Path {
        self.display_root.as_ref()
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
                .strip_prefix(self.display_root.as_ref())
                .or_else(|_| normalized.strip_prefix(self.startup_root.as_ref()))
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

    pub(crate) async fn classify(
        &self,
        path: &ResolvedPath,
        cancellation: &CancellationToken,
    ) -> ToolCallResult<EntryKind> {
        check_cancel(cancellation)?;
        let root = Arc::clone(&self.root);
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
        let root = Arc::clone(&self.root);
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
        let root = Arc::clone(&self.root);
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

fn is_vcs_directory(name: &str) -> bool {
    matches!(name, ".git" | ".svn" | ".hg" | ".bzr" | ".jj" | ".sl")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use tokio_util::sync::CancellationToken;

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
}
