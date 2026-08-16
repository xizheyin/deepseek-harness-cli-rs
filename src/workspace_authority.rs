//! One opened workspace capability shared by sessions and local tools.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use cap_std::{ambient_authority, fs::Dir};

#[cfg(unix)]
use cap_std::fs::MetadataExt;

/// Stable identity of the already-open workspace directory.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl WorkspaceIdentity {
    pub(crate) fn from_raw(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub(crate) fn device(self) -> u64 {
        self.device
    }

    pub(crate) fn inode(self) -> u64 {
        self.inode
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(device: u64, inode: u64) -> Self {
        Self::from_raw(device, inode)
    }
}

/// A directory descriptor is the authority; path strings are display facts.
#[derive(Clone)]
pub(crate) struct WorkspaceAuthority {
    root: Arc<Dir>,
    canonical_path: Arc<PathBuf>,
    startup_path: Arc<PathBuf>,
    #[cfg(unix)]
    identity: WorkspaceIdentity,
}

impl std::fmt::Debug for WorkspaceAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceAuthority")
            .field("opened", &true)
            .field(
                "canonical_path_bytes",
                &self.canonical_path.as_os_str().len(),
            )
            .finish_non_exhaustive()
    }
}

impl WorkspaceAuthority {
    /// Open once, then prove that the canonical display path names that object.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let startup_path = std::path::absolute(path)?;
        // This open is the authorization linearization point. Canonicalization
        // happens afterwards and is accepted only when it still names the same
        // directory object.
        let root = Dir::open_ambient_dir(path, ambient_authority())?;
        let opened = root.dir_metadata()?;
        if !opened.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "workspace is not a directory",
            ));
        }
        let canonical_path = std::fs::canonicalize(path)?;
        let named = std::fs::metadata(&canonical_path)?;
        if !named.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "workspace is not a directory",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if named.dev() != opened.dev() || named.ino() != opened.ino() {
                return Err(io::Error::other(
                    "workspace changed while it was being opened",
                ));
            }
        }
        Ok(Self {
            root: Arc::new(root),
            canonical_path: Arc::new(canonical_path),
            startup_path: Arc::new(startup_path),
            #[cfg(unix)]
            identity: WorkspaceIdentity {
                device: opened.dev(),
                inode: opened.ino(),
            },
        })
    }

    pub(crate) fn root(&self) -> &Arc<Dir> {
        &self.root
    }

    pub(crate) fn canonical_path(&self) -> &Path {
        self.canonical_path.as_ref()
    }

    pub(crate) fn startup_path(&self) -> &Path {
        self.startup_path.as_ref()
    }

    #[cfg(unix)]
    pub(crate) fn identity(&self) -> WorkspaceIdentity {
        self.identity
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::WorkspaceAuthority;

    #[test]
    fn clones_share_one_opened_directory_identity() {
        let root = tempfile_path("workspace-authority");
        fs::create_dir(&root).unwrap();
        let authority = WorkspaceAuthority::open(&root).unwrap();
        let clone = authority.clone();

        assert_eq!(authority.canonical_path(), clone.canonical_path());
        #[cfg(unix)]
        assert_eq!(authority.identity(), clone.identity());

        fs::remove_dir(root).unwrap();
    }

    fn tempfile_path(label: &str) -> std::path::PathBuf {
        let id = uuid::Uuid::new_v4();
        std::env::temp_dir().join(format!("dsh-{label}-{id}"))
    }
}
