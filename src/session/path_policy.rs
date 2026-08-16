//! Capability-oriented policy for the private session-store directory.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use cap_std::fs::Dir;

use super::store::StoreError;

const PRIVATE_DIRECTORY_MODE: u64 = 0o700;

/// A store root that is either already open or will be bootstrapped lazily.
#[derive(Clone)]
pub(super) enum RootPlan {
    Deferred {
        target: Arc<PathBuf>,
        required_existing_components: usize,
        effective_owner_from: usize,
        private_suffix_components: usize,
    },
}

pub(super) struct MaterializedRoot {
    pub(super) root: Arc<Dir>,
    pub(super) sync_file: File,
}

impl RootPlan {
    #[cfg(test)]
    pub(super) fn open_existing(path: &Path) -> Result<Self, StoreError> {
        let target = require_absolute_path(path.to_owned())?;
        let target_components = component_count(&target)?;
        Self::deferred_with_policy(target, target_components, target_components, 1)
    }

    pub(super) fn from_process_environment() -> Result<Self, StoreError> {
        if let Some(override_root) = std::env::var_os("DSH_SESSION_ROOT") {
            let target = require_absolute_path(PathBuf::from(override_root))?;
            let target_components = component_count(&target)?;
            return Self::deferred_with_policy(target, 0, target_components, 1);
        }

        #[cfg(target_os = "macos")]
        {
            let home = required_existing_home()?;
            let base = home.join("Library").join("Application Support");
            let target = base.join("dsh").join("sessions");
            let home_components = component_count(&home)?;
            return Self::deferred_with_policy(target, home_components, home_components + 1, 2);
        }

        #[cfg(target_os = "linux")]
        {
            let (base, required_existing_components, effective_owner_from) =
                match std::env::var_os("XDG_STATE_HOME") {
                    Some(value) if !value.is_empty() => {
                        let base = require_absolute_path(PathBuf::from(value))?;
                        let effective_owner_from = component_count(&base)?;
                        (base, 0, effective_owner_from)
                    }
                    Some(_) | None => {
                        let home = required_existing_home()?;
                        let home_components = component_count(&home)?;
                        (
                            home.join(".local").join("state"),
                            home_components,
                            home_components + 1,
                        )
                    }
                };
            let target = base.join("dsh").join("sessions");
            return Self::deferred_with_policy(
                target,
                required_existing_components,
                effective_owner_from,
                2,
            );
        }

        #[allow(unreachable_code)]
        Err(StoreError::RootUnavailable)
    }

    #[cfg(test)]
    fn deferred(
        target: PathBuf,
        required_existing_components: usize,
        private_suffix_components: usize,
    ) -> Result<Self, StoreError> {
        Self::deferred_with_policy(
            target,
            required_existing_components,
            required_existing_components.max(1),
            private_suffix_components,
        )
    }

    fn deferred_with_policy(
        target: PathBuf,
        required_existing_components: usize,
        effective_owner_from: usize,
        private_suffix_components: usize,
    ) -> Result<Self, StoreError> {
        let target = require_absolute_path(target)?;
        let target_components = component_count(&target)?;
        if required_existing_components > target_components
            || effective_owner_from == 0
            || effective_owner_from > target_components
            || private_suffix_components == 0
            || private_suffix_components > target_components
        {
            return Err(StoreError::RootUnavailable);
        }
        Ok(Self::Deferred {
            target: Arc::new(target),
            required_existing_components,
            effective_owner_from,
            private_suffix_components,
        })
    }

    pub(super) fn display_root(&self) -> &Path {
        match self {
            Self::Deferred { target, .. } => target.as_ref(),
        }
    }

    pub(super) fn materialize(&self) -> Result<MaterializedRoot, StoreError> {
        match self {
            Self::Deferred {
                target,
                required_existing_components,
                effective_owner_from,
                private_suffix_components,
            } => bootstrap_root(
                target,
                *required_existing_components,
                *effective_owner_from,
                *private_suffix_components,
            ),
        }
    }

    /// Open an existing store without creating, repairing, or synchronizing
    /// any path component. Listing uses this read-only path so merely looking
    /// for sessions cannot turn an absent store into filesystem state.
    pub(super) fn open_for_listing(&self) -> Result<Option<Arc<Dir>>, StoreError> {
        match self {
            Self::Deferred {
                target,
                required_existing_components,
                effective_owner_from,
                private_suffix_components,
            } => open_existing_root_read_only(
                target,
                *required_existing_components,
                *effective_owner_from,
                *private_suffix_components,
            ),
        }
    }
}

fn required_absolute_environment_path(name: &'static str) -> Result<PathBuf, StoreError> {
    let value = std::env::var_os(name).ok_or(StoreError::RootUnavailable)?;
    require_absolute_path(PathBuf::from(value))
}

fn required_existing_home() -> Result<PathBuf, StoreError> {
    let home = required_absolute_environment_path("HOME")?;
    let metadata = std::fs::symlink_metadata(&home).map_err(|_| StoreError::RootUnavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::UnsafeRoot);
    }
    use std::os::unix::fs::MetadataExt as _;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(home)
}

fn require_absolute_path(path: PathBuf) -> Result<PathBuf, StoreError> {
    if !path.is_absolute() || component_names(&path)?.is_empty() {
        return Err(StoreError::RootUnavailable);
    }
    Ok(path)
}

fn component_count(path: &Path) -> Result<usize, StoreError> {
    Ok(component_names(path)?.len())
}

fn component_names(path: &Path) -> Result<Vec<OsString>, StoreError> {
    if !path.is_absolute() {
        return Err(StoreError::RootUnavailable);
    }
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => names.push(name.to_owned()),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(StoreError::RootUnavailable);
            }
        }
    }
    Ok(names)
}

fn open_existing_root_read_only(
    target: &Path,
    required_existing_components: usize,
    effective_owner_from: usize,
    private_suffix_components: usize,
) -> Result<Option<Arc<Dir>>, StoreError> {
    let names = component_names(target)?;
    let private_start = names
        .len()
        .checked_sub(private_suffix_components)
        .ok_or(StoreError::RootUnavailable)?;
    let root_fd = rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| StoreError::RootUnavailable)?;
    let mut current = File::from(root_fd);
    validate_trusted_ancestor(&current, false)?;

    for (index, name) in names.iter().enumerate() {
        let component_number = index + 1;
        let opened = match open_directory_at(&current, name) {
            Ok(opened) => opened,
            Err(rustix::io::Errno::NOENT) => {
                match rustix::fs::statat(&current, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
                    Err(rustix::io::Errno::NOENT)
                        if component_number > required_existing_components =>
                    {
                        return Ok(None);
                    }
                    Err(rustix::io::Errno::NOENT) => {
                        return Err(StoreError::RootUnavailable);
                    }
                    Ok(_) | Err(_) => return Err(StoreError::UnsafeRoot),
                }
            }
            Err(_) => return Err(StoreError::UnsafeRoot),
        };
        if index >= private_start {
            validate_private_directory(&opened)?;
        } else {
            validate_trusted_ancestor(&opened, component_number >= effective_owner_from)?;
        }
        current = opened;
    }

    Ok(Some(Arc::new(Dir::from_std_file(current))))
}

fn bootstrap_root(
    target: &Path,
    required_existing_components: usize,
    effective_owner_from: usize,
    private_suffix_components: usize,
) -> Result<MaterializedRoot, StoreError> {
    let names = component_names(target)?;
    let private_start = names
        .len()
        .checked_sub(private_suffix_components)
        .ok_or(StoreError::RootUnavailable)?;
    let root_fd = rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| StoreError::RootUnavailable)?;
    let mut current = File::from(root_fd);
    validate_trusted_ancestor(&current, false)?;
    let mut creating_private_chain = false;

    for (index, name) in names.iter().enumerate() {
        let component_number = index + 1;
        let require_effective_owner = component_number >= effective_owner_from;
        let may_create = component_number > required_existing_components;
        let private_component = index >= private_start;
        if private_component || creating_private_chain {
            current = match rustix::fs::statat(
                &current,
                name.as_os_str(),
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(_) => converge_private_directory(&current, name)?,
                Err(rustix::io::Errno::NOENT) => {
                    if !may_create {
                        return Err(StoreError::RootUnavailable);
                    }
                    match rustix::fs::mkdirat(&current, name.as_os_str(), rustix::fs::Mode::RWXU) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(_) => return Err(StoreError::RootUnavailable),
                    }
                    converge_private_directory(&current, name)?
                }
                Err(_) => return Err(StoreError::UnsafeRoot),
            };
            continue;
        }
        let opened = open_directory_at(&current, name);
        current = match opened {
            Ok(directory) => {
                validate_trusted_ancestor(&directory, require_effective_owner)?;
                if may_create {
                    match private_convergence(&directory)? {
                        PrivateConvergence::Ordinary => directory,
                        PrivateConvergence::SyncExact => {
                            sync_exact_private_directory(&current, name, directory)?
                        }
                        PrivateConvergence::RepairSubset => {
                            normalize_private_directory(&current, name)?
                        }
                    }
                } else {
                    directory
                }
            }
            Err(error) if error == rustix::io::Errno::NOENT => {
                if !may_create {
                    return Err(StoreError::RootUnavailable);
                }
                match rustix::fs::mkdirat(&current, name.as_os_str(), rustix::fs::Mode::RWXU) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(_) => return Err(StoreError::RootUnavailable),
                }
                creating_private_chain = true;
                converge_private_directory(&current, name)?
            }
            Err(error) if error == rustix::io::Errno::ACCESS && may_create => {
                creating_private_chain = true;
                normalize_private_directory(&current, name)?
            }
            Err(_) => return Err(StoreError::UnsafeRoot),
        };
    }

    validate_private_directory(&current)?;
    sync_directory(&current)?;
    let sync_file = current
        .try_clone()
        .map_err(|_| StoreError::RootUnavailable)?;
    Ok(MaterializedRoot {
        root: Arc::new(Dir::from_std_file(current)),
        sync_file,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrivateConvergence {
    Ordinary,
    SyncExact,
    RepairSubset,
}

fn private_convergence(directory: &File) -> Result<PrivateConvergence, StoreError> {
    let stat = rustix::fs::fstat(directory).map_err(|_| StoreError::UnsafeRoot)?;
    let mode = u64::from(stat.st_mode & 0o7777);
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || mode & !PRIVATE_DIRECTORY_MODE != 0
    {
        return Ok(PrivateConvergence::Ordinary);
    }
    Ok(if mode == PRIVATE_DIRECTORY_MODE {
        PrivateConvergence::SyncExact
    } else {
        PrivateConvergence::RepairSubset
    })
}

fn converge_private_directory(parent: &File, name: &OsStr) -> Result<File, StoreError> {
    let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| StoreError::UnsafeRoot)?;
    validate_private_stat(&named, true)?;
    if u64::from(named.st_mode & 0o7777) == PRIVATE_DIRECTORY_MODE {
        let directory = open_directory_at(parent, name).map_err(|_| StoreError::UnsafeRoot)?;
        sync_exact_private_directory(parent, name, directory)
    } else {
        normalize_private_directory(parent, name)
    }
}

fn sync_exact_private_directory(
    parent: &File,
    name: &OsStr,
    directory: File,
) -> Result<File, StoreError> {
    let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| StoreError::UnsafeRoot)?;
    let opened = rustix::fs::fstat(&directory).map_err(|_| StoreError::UnsafeRoot)?;
    validate_private_stat(&named, false)?;
    validate_private_stat(&opened, false)?;
    if !same_stat_identity(&named, &opened) {
        return Err(StoreError::UnsafeRoot);
    }
    sync_directory(&directory)?;
    sync_directory(parent)?;
    let named_after = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| StoreError::UnsafeRoot)?;
    if !same_stat_identity(&named_after, &opened) {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(directory)
}

fn open_directory_at(parent: &File, name: &OsStr) -> Result<File, rustix::io::Errno> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
}

fn normalize_private_directory(parent: &File, name: &OsStr) -> Result<File, StoreError> {
    #[cfg(target_os = "linux")]
    let captured_stat = {
        let captured = rustix::fs::openat(
            parent,
            name,
            rustix::fs::OFlags::PATH
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| StoreError::UnsafeRoot)?;
        let metadata = rustix::fs::fstat(&captured).map_err(|_| StoreError::UnsafeRoot)?;
        validate_private_stat(&metadata, true)?;
        linux_fchmodat2_empty_path(&captured)?;
        metadata
    };

    #[cfg(target_os = "macos")]
    let captured_stat = {
        let metadata = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| StoreError::UnsafeRoot)?;
        validate_private_stat(&metadata, true)?;
        rustix::fs::chmodat(
            parent,
            name,
            rustix::fs::Mode::RWXU,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|_| StoreError::UnsafeRoot)?;
        metadata
    };

    let directory = open_directory_at(parent, name).map_err(|_| StoreError::UnsafeRoot)?;
    let opened_stat = rustix::fs::fstat(&directory).map_err(|_| StoreError::UnsafeRoot)?;
    if !same_stat_identity(&captured_stat, &opened_stat) {
        return Err(StoreError::UnsafeRoot);
    }
    rustix::fs::fchmod(&directory, rustix::fs::Mode::RWXU).map_err(|_| StoreError::UnsafeRoot)?;
    validate_private_directory(&directory)?;
    let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| StoreError::UnsafeRoot)?;
    let opened = rustix::fs::fstat(&directory).map_err(|_| StoreError::UnsafeRoot)?;
    if !same_stat_identity(&named, &opened) {
        return Err(StoreError::UnsafeRoot);
    }
    sync_directory(&directory)?;
    sync_directory(parent)?;
    Ok(directory)
}

fn validate_trusted_ancestor(file: &File, require_effective_owner: bool) -> Result<(), StoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata().map_err(|_| StoreError::UnsafeRoot)?;
    if !metadata.is_dir() {
        return Err(StoreError::UnsafeRoot);
    }
    let effective_uid = rustix::process::geteuid().as_raw();
    let mode = metadata.mode() & 0o7777;
    let owner_ok = if require_effective_owner {
        metadata.uid() == effective_uid
    } else {
        metadata.uid() == 0 || metadata.uid() == effective_uid
    };
    let root_sticky_exception = metadata.uid() == 0 && mode & 0o1000 != 0;
    if !owner_ok || (mode & 0o022 != 0 && !root_sticky_exception) {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(())
}

fn validate_private_directory(file: &File) -> Result<(), StoreError> {
    let stat = rustix::fs::fstat(file).map_err(|_| StoreError::UnsafeRoot)?;
    validate_private_stat(&stat, false)
}

fn validate_private_stat(
    stat: &rustix::fs::Stat,
    allow_cleared_owner_bits: bool,
) -> Result<(), StoreError> {
    let mode = u64::from(stat.st_mode & 0o7777);
    let mode_ok = if allow_cleared_owner_bits {
        mode & !PRIVATE_DIRECTORY_MODE == 0
    } else {
        mode == PRIVATE_DIRECTORY_MODE
    };
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || !mode_ok
    {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(())
}

fn same_stat_identity(left: &rustix::fs::Stat, right: &rustix::fs::Stat) -> bool {
    stat_device(left) == stat_device(right) && left.st_ino == right.st_ino
}

#[cfg(target_os = "macos")]
fn stat_device(stat: &rustix::fs::Stat) -> Option<u64> {
    u64::try_from(stat.st_dev).ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn stat_device(stat: &rustix::fs::Stat) -> Option<u64> {
    Some(stat.st_dev)
}

#[cfg(target_os = "linux")]
fn linux_fchmodat2_empty_path(fd: &std::os::fd::OwnedFd) -> Result<(), StoreError> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `fd` is an owned O_PATH descriptor captured with NOFOLLOW. The
    // empty path is a fixed NUL-terminated byte string, no pointer is retained,
    // and the syscall result is checked before the descriptor is reused.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::syscall(
            libc::SYS_fchmodat2,
            fd.as_raw_fd(),
            c"".as_ptr(),
            rustix::fs::Mode::RWXU.as_raw_mode() as libc::mode_t,
            libc::AT_EMPTY_PATH,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(StoreError::UnsafeRoot)
    }
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
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use super::RootPlan;

    #[test]
    fn missing_private_suffix_is_created_exactly_and_symlinks_are_rejected() {
        let parent = private_dir("path-policy-parent");
        let target = parent.join("missing").join("sessions");
        let plan = RootPlan::deferred(target.clone(), component_count(&parent), 2).unwrap();
        let materialized = plan.materialize().unwrap();
        assert_eq!(
            materialized
                .sync_file
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(parent.join("missing"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        drop(materialized);

        let sentinel = private_dir("path-policy-sentinel");
        let link = parent.join("link");
        std::os::unix::fs::symlink(&sentinel, &link).unwrap();
        let bad = RootPlan::deferred(link.join("sessions"), component_count(&parent), 2).unwrap();
        assert!(bad.materialize().is_err());
        assert_eq!(
            fs::metadata(&sentinel).unwrap().permissions().mode() & 0o7777,
            0o700
        );

        fs::remove_file(link).unwrap();
        fs::remove_dir_all(parent).unwrap();
        fs::remove_dir(sentinel).unwrap();
    }

    #[test]
    fn multiple_missing_components_below_a_trusted_parent_converge_privately() {
        let parent = private_dir("path-policy-multi-parent");
        let first = parent.join("first");
        let second = first.join("second");
        let target = second.join("sessions");
        let plan = RootPlan::deferred_with_policy(
            target.clone(),
            component_count(&parent),
            component_count(&target),
            1,
        )
        .unwrap();

        drop(plan.materialize().unwrap());
        drop(plan.materialize().unwrap());

        for directory in [&first, &second, &target] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o7777,
                0o700
            );
        }
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn a_bit_cleared_intermediate_from_a_crashed_creator_is_repaired() {
        let parent = private_dir("path-policy-crash-parent");
        let interrupted = parent.join("interrupted");
        fs::create_dir(&interrupted).unwrap();
        fs::set_permissions(&interrupted, fs::Permissions::from_mode(0o000)).unwrap();
        let target = interrupted.join("next").join("sessions");
        let plan = RootPlan::deferred_with_policy(
            target.clone(),
            component_count(&parent),
            component_count(&target),
            1,
        )
        .unwrap();

        drop(plan.materialize().unwrap());

        assert_eq!(
            fs::metadata(&interrupted).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn an_override_does_not_chmod_existing_private_temp_ancestors() {
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let workspace = temp.join(format!(
            "dsh-path-policy-workspace-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&workspace).unwrap();
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o755)).unwrap();
        let target = workspace.join("sessions");
        let target_components = component_count(&target);
        let plan = RootPlan::deferred_with_policy(target.clone(), 0, target_components, 1).unwrap();

        drop(plan.materialize().unwrap());

        assert_eq!(
            fs::metadata(&workspace).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn read_only_listing_open_never_creates_or_repairs_components() {
        let parent = private_dir("path-policy-list-parent");
        let target = parent.join("missing").join("sessions");
        let plan = RootPlan::deferred(target.clone(), component_count(&parent), 2).unwrap();
        assert!(plan.open_for_listing().unwrap().is_none());
        assert!(!target.exists());
        assert!(!parent.join("missing").exists());

        fs::create_dir(target.parent().unwrap()).unwrap();
        fs::set_permissions(target.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(plan.open_for_listing().is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        fs::remove_dir_all(parent).unwrap();
    }

    fn component_count(path: &std::path::Path) -> usize {
        path.components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count()
    }

    fn private_dir(label: &str) -> std::path::PathBuf {
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = temp.join(format!("dsh-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
