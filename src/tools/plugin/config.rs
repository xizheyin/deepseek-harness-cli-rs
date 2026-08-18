use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    os::{
        fd::OwnedFd,
        unix::{
            fs::MetadataExt as _,
            io::{AsRawFd as _, RawFd},
        },
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;

use crate::model::JsonValue;

use super::{MAX_PLUGINS, is_plugin_id, json::parse_strict_json};

const CONFIG_VERSION: u64 = 1;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_PLUGIN_ARGUMENTS: usize = 16;
const MAX_PLUGIN_ARGUMENT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum PluginConfigError {
    #[error("plugin configuration could not be opened")]
    PathUnavailable,
    #[error("plugin configuration file is not private and regular")]
    UnsafeConfigFile,
    #[error("plugin configuration exceeds its size limit")]
    ConfigTooLarge,
    #[error("plugin configuration changed while it was being read")]
    ConfigChanged,
    #[error("plugin configuration JSON is invalid")]
    InvalidJson,
    #[error("plugin configuration version is unsupported")]
    UnsupportedVersion,
    #[error("plugin configuration has too many entries")]
    TooManyPlugins,
    #[error("plugin configuration contains an invalid entry")]
    InvalidEntry,
    #[error("plugin configuration contains an invalid or duplicate ID")]
    InvalidPluginId,
    #[error("plugin program is not a safe executable")]
    InvalidProgram,
    #[error("plugin {plugin_id} configuration is invalid: {source}")]
    Plugin {
        plugin_id: String,
        #[source]
        source: PluginEntryError,
    },
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PluginEntryError {
    #[error("entry fields are invalid")]
    InvalidEntry,
    #[error("program is not a safe executable")]
    InvalidProgram,
    #[error("too many program arguments")]
    TooManyArguments,
    #[error("program arguments exceed their size limit")]
    ArgumentsTooLarge,
    #[error("plugin ID is duplicated")]
    DuplicateId,
}

pub(crate) struct PluginConfig {
    plugins: Box<[PluginProgram]>,
}

impl std::fmt::Debug for PluginConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginConfig")
            .field("plugin_count", &self.plugins.len())
            .finish()
    }
}

impl PluginConfig {
    pub(crate) fn load(
        startup_directory: &Path,
        configured_path: &Path,
    ) -> Result<Self, PluginConfigError> {
        let path = if configured_path.is_absolute() {
            configured_path.to_path_buf()
        } else {
            startup_directory.join(configured_path)
        };
        let value = read_config(&path)?;
        let fields = value
            .as_value()
            .as_object()
            .ok_or(PluginConfigError::InvalidJson)?;
        require_exact_keys(fields, &["version", "plugins"])?;
        if fields.get("version").and_then(serde_json::Value::as_u64) != Some(CONFIG_VERSION) {
            return Err(PluginConfigError::UnsupportedVersion);
        }
        let entries = fields
            .get("plugins")
            .and_then(serde_json::Value::as_array)
            .ok_or(PluginConfigError::InvalidEntry)?;
        if entries.len() > MAX_PLUGINS {
            return Err(PluginConfigError::TooManyPlugins);
        }
        let mut ids = BTreeSet::new();
        let mut plugins = Vec::new();
        plugins
            .try_reserve_exact(entries.len())
            .map_err(|_| PluginConfigError::InvalidEntry)?;
        for entry in entries {
            let plugin = PluginProgram::from_config(entry)?;
            if !ids.insert(plugin.id.clone()) {
                return Err(PluginConfigError::Plugin {
                    plugin_id: plugin.id,
                    source: PluginEntryError::DuplicateId,
                });
            }
            plugins.push(plugin);
        }
        Ok(Self {
            plugins: plugins.into_boxed_slice(),
        })
    }

    #[cfg(test)]
    pub(crate) fn plugins(&self) -> &[PluginProgram] {
        &self.plugins
    }

    pub(crate) fn into_plugins(self) -> Box<[PluginProgram]> {
        self.plugins
    }
}

#[derive(Clone)]
pub(crate) struct PluginProgram {
    id: String,
    path: PathBuf,
    arguments: Box<[String]>,
    identity: ProgramIdentity,
    descriptor: Arc<File>,
}

impl std::fmt::Debug for PluginProgram {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginProgram")
            .field("id", &self.id)
            .field("argument_count", &self.arguments.len())
            .finish_non_exhaustive()
    }
}

impl PluginProgram {
    fn from_config(value: &serde_json::Value) -> Result<Self, PluginConfigError> {
        let fields = value.as_object().ok_or(PluginConfigError::InvalidEntry)?;
        require_exact_keys(fields, &["id", "program", "args"])?;
        let id = fields
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| is_plugin_id(value))
            .ok_or(PluginConfigError::InvalidPluginId)?
            .to_owned();
        Self::from_valid_id(fields, id.clone()).map_err(|source| PluginConfigError::Plugin {
            plugin_id: id,
            source,
        })
    }

    fn from_valid_id(
        fields: &serde_json::Map<String, serde_json::Value>,
        id: String,
    ) -> Result<Self, PluginEntryError> {
        let path = fields
            .get("program")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or(PluginEntryError::InvalidProgram)?;
        let raw_arguments = fields
            .get("args")
            .and_then(serde_json::Value::as_array)
            .ok_or(PluginEntryError::InvalidEntry)?;
        if raw_arguments.len() > MAX_PLUGIN_ARGUMENTS {
            return Err(PluginEntryError::TooManyArguments);
        }
        let mut argument_bytes = 0_usize;
        let mut arguments = Vec::new();
        arguments
            .try_reserve_exact(raw_arguments.len())
            .map_err(|_| PluginEntryError::InvalidEntry)?;
        for value in raw_arguments {
            let value = value.as_str().ok_or(PluginEntryError::InvalidEntry)?;
            if value.contains('\0') {
                return Err(PluginEntryError::InvalidEntry);
            }
            argument_bytes = argument_bytes
                .checked_add(value.len())
                .ok_or(PluginEntryError::ArgumentsTooLarge)?;
            if argument_bytes > MAX_PLUGIN_ARGUMENT_BYTES {
                return Err(PluginEntryError::ArgumentsTooLarge);
            }
            arguments.push(value.to_owned());
        }
        let (path, identity, descriptor) =
            admit_program(&path).map_err(|_| PluginEntryError::InvalidProgram)?;
        Ok(Self {
            id,
            path,
            arguments: arguments.into_boxed_slice(),
            identity,
            descriptor: Arc::new(descriptor),
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub(crate) fn open_working_directory(&self) -> Result<OwnedFd, PluginConfigError> {
        let parent = self
            .path
            .parent()
            .ok_or(PluginConfigError::InvalidProgram)?;
        rustix::fs::open(
            parent,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| PluginConfigError::InvalidProgram)
    }

    pub(crate) fn revalidate(&self) -> Result<(), PluginConfigError> {
        let retained = self
            .descriptor
            .metadata()
            .map_err(|_| PluginConfigError::InvalidProgram)?;
        let (path, identity, _) = admit_program(&self.path)?;
        if path != self.path
            || identity != self.identity
            || ProgramIdentity::from_metadata(&retained) != self.identity
        {
            return Err(PluginConfigError::InvalidProgram);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProgramIdentity {
    device: u64,
    inode: u64,
    owner: u32,
    mode: u32,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl ProgramIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
            mode: metadata.mode(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ConfigFingerprint {
    device: u64,
    inode: u64,
    owner: u32,
    mode: u32,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl ConfigFingerprint {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
            mode: metadata.mode(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

fn read_config(path: &Path) -> Result<JsonValue, PluginConfigError> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| PluginConfigError::PathUnavailable)?;
    let mut file = File::from(descriptor);
    let before = file
        .metadata()
        .map_err(|_| PluginConfigError::PathUnavailable)?;
    validate_config_metadata(&before)?;
    reject_private_acl(file.as_raw_fd()).map_err(|_| PluginConfigError::UnsafeConfigFile)?;
    if before.len() > MAX_CONFIG_BYTES as u64 {
        return Err(PluginConfigError::ConfigTooLarge);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(before.len() as usize)
        .map_err(|_| PluginConfigError::ConfigTooLarge)?;
    (&mut file)
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| PluginConfigError::PathUnavailable)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(PluginConfigError::ConfigTooLarge);
    }
    let after = file
        .metadata()
        .map_err(|_| PluginConfigError::PathUnavailable)?;
    validate_config_metadata(&after)?;
    reject_private_acl(file.as_raw_fd()).map_err(|_| PluginConfigError::UnsafeConfigFile)?;
    if ConfigFingerprint::from_metadata(&before) != ConfigFingerprint::from_metadata(&after)
        || after.len() != bytes.len() as u64
    {
        return Err(PluginConfigError::ConfigChanged);
    }
    let raw = parse_strict_json(&bytes).map_err(|_| PluginConfigError::InvalidJson)?;
    if raw
        .as_object()
        .and_then(|fields| fields.get("version"))
        .is_some_and(|version| version.as_u64().is_none())
    {
        return Err(PluginConfigError::InvalidJson);
    }
    JsonValue::new(raw).map_err(|_| PluginConfigError::InvalidJson)
}

fn validate_config_metadata(metadata: &std::fs::Metadata) -> Result<(), PluginConfigError> {
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(PluginConfigError::UnsafeConfigFile);
    }
    Ok(())
}

fn admit_program(path: &Path) -> Result<(PathBuf, ProgramIdentity, File), PluginConfigError> {
    if !path.is_absolute() {
        return Err(PluginConfigError::InvalidProgram);
    }
    let named = std::fs::symlink_metadata(path).map_err(|_| PluginConfigError::InvalidProgram)?;
    if named.file_type().is_symlink() {
        return Err(PluginConfigError::InvalidProgram);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| PluginConfigError::InvalidProgram)?;
    if canonical != path {
        return Err(PluginConfigError::InvalidProgram);
    }
    validate_parent_chain(&canonical)?;
    let descriptor = rustix::fs::open(
        &canonical,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| PluginConfigError::InvalidProgram)?;
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|_| PluginConfigError::InvalidProgram)?;
    validate_program_metadata(&metadata)?;
    reject_extended_acl(file.as_raw_fd()).map_err(|_| PluginConfigError::InvalidProgram)?;
    rustix::fs::accessat(
        rustix::fs::CWD,
        &canonical,
        rustix::fs::Access::EXEC_OK,
        rustix::fs::AtFlags::EACCESS | rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|_| PluginConfigError::InvalidProgram)?;
    let identity = ProgramIdentity::from_metadata(&metadata);
    Ok((canonical, identity, file))
}

fn validate_parent_chain(path: &Path) -> Result<(), PluginConfigError> {
    let effective_user = rustix::process::geteuid().as_raw();
    let mut current = path.parent();
    while let Some(directory_path) = current {
        let descriptor = rustix::fs::open(
            directory_path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| PluginConfigError::InvalidProgram)?;
        let directory = File::from(descriptor);
        let metadata = directory
            .metadata()
            .map_err(|_| PluginConfigError::InvalidProgram)?;
        let mode = metadata.mode();
        if !metadata.is_dir()
            || (metadata.uid() != effective_user && metadata.uid() != 0)
            || (mode & 0o022 != 0 && mode & 0o1000 == 0)
        {
            return Err(PluginConfigError::InvalidProgram);
        }
        reject_extended_acl(directory.as_raw_fd())
            .map_err(|_| PluginConfigError::InvalidProgram)?;
        current = directory_path.parent();
    }
    Ok(())
}

fn validate_program_metadata(metadata: &std::fs::Metadata) -> Result<(), PluginConfigError> {
    let effective_user = rustix::process::geteuid().as_raw();
    if !metadata.is_file()
        || (metadata.uid() != effective_user && metadata.uid() != 0)
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o6000 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err(PluginConfigError::InvalidProgram);
    }
    Ok(())
}

fn require_exact_keys(
    fields: &serde_json::Map<String, serde_json::Value>,
    expected: &[&str],
) -> Result<(), PluginConfigError> {
    if fields.len() == expected.len() && fields.keys().all(|key| expected.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(PluginConfigError::InvalidEntry)
    }
}

#[cfg(not(target_os = "macos"))]
fn reject_extended_acl(_descriptor: RawFd) -> Result<(), ()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn reject_private_acl(_descriptor: RawFd) -> Result<(), ()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn reject_extended_acl(descriptor: RawFd) -> Result<(), ()> {
    macos_acl::reject_write_acl(descriptor)
}

#[cfg(target_os = "macos")]
fn reject_private_acl(descriptor: RawFd) -> Result<(), ()> {
    macos_acl::reject_any_allow_acl(descriptor)
}

#[cfg(target_os = "macos")]
mod macos_acl {
    #![allow(unsafe_code)]

    use std::{ffi::c_void, os::fd::RawFd, ptr};

    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = -1;
    const ACL_EXTENDED_ALLOW: libc::c_int = 1;
    const ACL_EXTENDED_DENY: libc::c_int = 2;
    const WRITE_PERMISSIONS: [libc::c_int; 8] = [
        1 << 2,  // write data / add file
        1 << 4,  // delete
        1 << 5,  // append data / add subdirectory
        1 << 6,  // delete child
        1 << 8,  // write attributes
        1 << 10, // write extended attributes
        1 << 12, // write security
        1 << 13, // change owner
    ];

    unsafe extern "C" {
        fn acl_get_fd_np(descriptor: libc::c_int, kind: libc::c_int) -> *mut c_void;
        fn acl_get_entry(
            acl: *mut c_void,
            entry_id: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset(entry: *mut c_void, permissions: *mut *mut c_void) -> libc::c_int;
        fn acl_get_perm_np(permissions: *mut c_void, permission: libc::c_int) -> libc::c_int;
        fn acl_free(value: *mut c_void) -> libc::c_int;
    }

    pub(super) fn reject_write_acl(descriptor: RawFd) -> Result<(), ()> {
        reject_acl(descriptor, false)
    }

    pub(super) fn reject_any_allow_acl(descriptor: RawFd) -> Result<(), ()> {
        reject_acl(descriptor, true)
    }

    fn reject_acl(descriptor: RawFd, reject_any_allow: bool) -> Result<(), ()> {
        // SAFETY: `descriptor` is an open file owned by the caller. The ACL
        // object returned by libc is inspected once and released on every path.
        let acl = unsafe { acl_get_fd_np(descriptor, ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                Ok(())
            } else {
                Err(())
            };
        }
        let mut entry_id = ACL_FIRST_ENTRY;
        let mut unsafe_grant = false;
        let mut invalid = false;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: `acl` is live and `entry` points to writable pointer storage.
            let status = unsafe { acl_get_entry(acl, entry_id, &mut entry) };
            if status == -1 {
                let error = std::io::Error::last_os_error().raw_os_error();
                if !matches!(error, Some(libc::ENOENT) | Some(libc::EINVAL)) {
                    invalid = true;
                }
                break;
            }
            if status != 0 || entry.is_null() {
                invalid = true;
                break;
            }
            let mut tag = 0;
            // SAFETY: `entry` was returned by the still-live ACL object.
            if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
                invalid = true;
                break;
            }
            if tag == ACL_EXTENDED_ALLOW {
                if reject_any_allow {
                    unsafe_grant = true;
                    break;
                }
                let mut permissions = ptr::null_mut();
                // SAFETY: the output pointer is writable and `entry` is live.
                if unsafe { acl_get_permset(entry, &mut permissions) } != 0 || permissions.is_null()
                {
                    invalid = true;
                    break;
                }
                for permission in WRITE_PERMISSIONS {
                    // SAFETY: `permissions` belongs to the live ACL entry.
                    match unsafe { acl_get_perm_np(permissions, permission) } {
                        1 => {
                            unsafe_grant = true;
                            break;
                        }
                        0 => {}
                        _ => {
                            invalid = true;
                            break;
                        }
                    }
                }
                if unsafe_grant || invalid {
                    break;
                }
            } else if tag != ACL_EXTENDED_DENY {
                invalid = true;
                break;
            }
            entry_id = ACL_NEXT_ENTRY;
        }
        // SAFETY: `acl` came from `acl_get_fd_np` and has not been freed yet.
        let freed = unsafe { acl_free(acl) };
        if freed != 0 || invalid || unsafe_grant {
            Err(())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::process::Command;
    use std::{
        fs::{self, OpenOptions},
        io::Write as _,
        os::unix::fs::{PermissionsExt as _, symlink},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{MAX_CONFIG_BYTES, PluginConfig, PluginConfigError};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("dsh-plugin-config-{}-{serial}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(path: &Path, bytes: &[u8], mode: u32) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.set_permissions(fs::Permissions::from_mode(mode))
            .unwrap();
    }

    fn fixture() -> (TempDirectory, PathBuf, PathBuf) {
        let root = TempDirectory::new();
        let named_program = root.path().join("plugin");
        write_file(&named_program, b"#!/bin/sh\nexit 0\n", 0o700);
        let program = fs::canonicalize(&named_program).unwrap();
        let config = root.path().join("plugins.json");
        let body = serde_json::json!({
            "version":1,
            "plugins":[{"id":"text-tools","program":program,"args":["--quiet"]}]
        });
        write_file(
            &config,
            serde_json::to_string(&body).unwrap().as_bytes(),
            0o600,
        );
        (root, program, config)
    }

    #[test]
    fn private_config_admits_a_canonical_executable_and_revalidates_it() {
        let (root, program, _) = fixture();
        let config = PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap();
        assert_eq!(config.plugins().len(), 1);
        let plugin = &config.plugins()[0];
        assert_eq!(plugin.id(), "text-tools");
        assert_eq!(plugin.path(), program);
        assert_eq!(plugin.arguments(), ["--quiet"]);
        plugin.revalidate().unwrap();
    }

    #[test]
    fn duplicate_keys_and_public_config_files_are_rejected() {
        let (root, _, config_path) = fixture();
        for mode in [0o644, 0o622] {
            fs::set_permissions(&config_path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
                PluginConfigError::UnsafeConfigFile
            );
        }

        fs::remove_file(&config_path).unwrap();
        write_file(
            &config_path,
            br#"{"version":1,"version":1,"plugins":[]}"#,
            0o600,
        );
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::InvalidJson
        );
    }

    #[test]
    fn version_literals_duplicate_ids_and_setid_programs_fail_closed() {
        let root = TempDirectory::new();
        let program = root.path().join("plugin");
        write_file(&program, b"#!/bin/sh\nexit 0\n", 0o700);
        let program = fs::canonicalize(program).unwrap();
        let config = root.path().join("plugins.json");

        for version in ["1.0", "1e0"] {
            let body = format!(r#"{{"version":{version},"plugins":[]}}"#);
            write_file(&config, body.as_bytes(), 0o600);
            assert_eq!(
                PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
                PluginConfigError::InvalidJson
            );
            fs::remove_file(&config).unwrap();
        }
        write_file(&config, br#"{"version":2,"plugins":[]}"#, 0o600);
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::UnsupportedVersion
        );

        fs::remove_file(&config).unwrap();
        let duplicate = serde_json::json!({
            "version":1,
            "plugins":[
                {"id":"same-plugin","program":program,"args":[]},
                {"id":"same-plugin","program":program,"args":[]}
            ]
        });
        write_file(
            &config,
            serde_json::to_string(&duplicate).unwrap().as_bytes(),
            0o600,
        );
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::Plugin {
                plugin_id: "same-plugin".to_owned(),
                source: super::PluginEntryError::DuplicateId,
            }
        );

        fs::remove_file(&config).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o4700)).unwrap();
        let setid = serde_json::json!({
            "version":1,
            "plugins":[{"id":"setid-plugin","program":program,"args":[]}]
        });
        write_file(
            &config,
            serde_json::to_string(&setid).unwrap().as_bytes(),
            0o600,
        );
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::Plugin {
                plugin_id: "setid-plugin".to_owned(),
                source: super::PluginEntryError::InvalidProgram,
            }
        );
    }

    #[test]
    fn config_byte_limit_accepts_exact_and_rejects_one_over_before_parsing() {
        let root = TempDirectory::new();
        let config = root.path().join("plugins.json");
        let prefix = br#"{"version":1,"plugins":[],"padding":""#;
        let suffix = br#""}"#;
        let padding = MAX_CONFIG_BYTES - prefix.len() - suffix.len();
        let mut exact = Vec::with_capacity(MAX_CONFIG_BYTES);
        exact.extend_from_slice(prefix);
        exact.extend(std::iter::repeat_n(b'x', padding));
        exact.extend_from_slice(suffix);
        assert_eq!(exact.len(), MAX_CONFIG_BYTES);
        write_file(&config, &exact, 0o600);
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::InvalidEntry
        );

        fs::remove_file(&config).unwrap();
        exact.insert(exact.len() - suffix.len(), b'x');
        write_file(&config, &exact, 0o600);
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::ConfigTooLarge
        );
    }

    #[test]
    fn program_symlinks_and_identity_replacement_are_rejected() {
        let (root, program, _) = fixture();
        let config = PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap();
        let original = &config.plugins()[0];
        fs::remove_file(&program).unwrap();
        write_file(&program, b"#!/bin/sh\nexit 1\n", 0o700);
        assert_eq!(
            original.revalidate().unwrap_err(),
            PluginConfigError::InvalidProgram
        );

        let target = root.path().join("target");
        write_file(&target, b"#!/bin/sh\nexit 0\n", 0o700);
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();
        let link_config = root.path().join("link.json");
        let body = serde_json::json!({
            "version":1,
            "plugins":[{"id":"linked","program":link,"args":[]}]
        });
        write_file(
            &link_config,
            serde_json::to_string(&body).unwrap().as_bytes(),
            0o600,
        );
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("link.json")).unwrap_err(),
            PluginConfigError::Plugin {
                plugin_id: "linked".to_owned(),
                source: super::PluginEntryError::InvalidProgram,
            }
        );
    }

    #[test]
    fn plugin_and_argument_limits_fail_closed() {
        let root = TempDirectory::new();
        let program = root.path().join("plugin");
        write_file(&program, b"#!/bin/sh\nexit 0\n", 0o700);
        let program = fs::canonicalize(program).unwrap();
        let config = root.path().join("plugins.json");
        let plugins = (0..9)
            .map(|index| {
                serde_json::json!({"id":format!("plugin-{index}"),"program":program,"args":[]})
            })
            .collect::<Vec<_>>();
        write_file(
            &config,
            serde_json::to_string(&serde_json::json!({"version":1,"plugins":plugins}))
                .unwrap()
                .as_bytes(),
            0o600,
        );
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::TooManyPlugins
        );

        fs::remove_file(&config).unwrap();
        let plugins = (0..8)
            .map(|index| {
                serde_json::json!({"id":format!("plugin-{index}"),"program":program,"args":[]})
            })
            .collect::<Vec<_>>();
        write_file(
            &config,
            serde_json::to_string(&serde_json::json!({"version":1,"plugins":plugins}))
                .unwrap()
                .as_bytes(),
            0o600,
        );
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json"))
                .unwrap()
                .plugins()
                .len(),
            8
        );

        for (arguments, expected) in [
            (vec!["x".repeat(2_048); 16], None),
            (
                vec!["x".to_owned(); 17],
                Some(super::PluginEntryError::TooManyArguments),
            ),
            (
                vec!["x".repeat(32 * 1024 + 1)],
                Some(super::PluginEntryError::ArgumentsTooLarge),
            ),
        ] {
            fs::remove_file(&config).unwrap();
            write_file(
                &config,
                serde_json::to_string(&serde_json::json!({
                    "version":1,
                    "plugins":[{"id":"argument-test","program":program,"args":arguments}]
                }))
                .unwrap()
                .as_bytes(),
                0o600,
            );
            match expected {
                None => assert!(PluginConfig::load(root.path(), Path::new("plugins.json")).is_ok()),
                Some(source) => assert_eq!(
                    PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
                    PluginConfigError::Plugin {
                        plugin_id: "argument-test".to_owned(),
                        source,
                    }
                ),
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn config_rejects_read_acl_grants_and_programs_reject_write_acl_grants() {
        let (root, program, config_path) = fixture();
        let grant = |path: &Path, permission: &str| {
            let rule = format!("everyone allow {permission}");
            assert!(
                Command::new("/bin/chmod")
                    .args(["+a", rule.as_str()])
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        grant(&config_path, "read");
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::UnsafeConfigFile
        );
        assert!(
            Command::new("/bin/chmod")
                .arg("-N")
                .arg(&config_path)
                .status()
                .unwrap()
                .success()
        );
        grant(&program, "write");
        assert_eq!(
            PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap_err(),
            PluginConfigError::Plugin {
                plugin_id: "text-tools".to_owned(),
                source: super::PluginEntryError::InvalidProgram,
            }
        );
    }
}
