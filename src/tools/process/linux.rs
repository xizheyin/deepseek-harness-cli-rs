use std::{
    ffi::OsStr,
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use cap_std::{ambient_authority, fs::Dir};
use rustix::{
    fs::{AtFlags, PROC_SUPER_MAGIC, StatxFlags},
    process::{Pid, WaitOptions},
};
use tokio_util::sync::CancellationToken;

use super::{
    host::{GroupScan, HostError, check_cancel},
    mountinfo,
    proc_stat::{self, ProcStat},
};

const MAX_PID: u32 = 4_194_304;
const STAT_BYTES: usize = 4_096;
const PID_MAX_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupMember {
    Outside,
    Live,
    LeaderZombie,
    OwnedZombie(u32),
    OtherZombie,
}

pub(super) struct PlatformHost {
    procfs: Dir,
    mount_id: u64,
    harness: Pid,
    pid_max: AtomicU32,
}

impl PlatformHost {
    pub(super) fn open() -> Result<Self, HostError> {
        let procfs = Dir::open_ambient_dir("/proc", ambient_authority())
            .map_err(|_| HostError::Unsupported)?;
        let mount_id = proc_mount_id(&procfs)?;
        let harness = rustix::process::getpid();
        validate_harness_pid(harness)?;
        let host = Self {
            procfs,
            mount_id,
            harness,
            pid_max: AtomicU32::new(1),
        };
        let pid_max = host.validate_identity(None)?;
        host.pid_max.store(pid_max, Ordering::Relaxed);
        host.probe_process_table(pid_max)?;
        Ok(host)
    }

    pub(super) fn recheck(&self, cancellation: &CancellationToken) -> Result<(), HostError> {
        let pid_max = self.validate_identity(Some(cancellation))?;
        self.pid_max.store(pid_max, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn scan_group(&self, leader: Pid, expected_session: Pid, harness: Pid) -> GroupScan {
        if harness != self.harness || self.validate_identity(None).is_err() {
            return GroupScan::OwnershipLost;
        }
        let entries = match self.procfs.entries() {
            Ok(entries) => entries,
            Err(_) => return GroupScan::Unknown,
        };
        let leader_raw = match u32::try_from(leader.as_raw_pid()) {
            Ok(pid) => pid,
            Err(_) => return GroupScan::OwnershipLost,
        };
        let session_raw = match u32::try_from(expected_session.as_raw_pid()) {
            Ok(pid) => pid,
            Err(_) => return GroupScan::OwnershipLost,
        };
        let harness_raw = match u32::try_from(harness.as_raw_pid()) {
            Ok(pid) => pid,
            Err(_) => return GroupScan::OwnershipLost,
        };
        let mut numeric_entries = 0_u32;
        let mut saw_leader = false;
        let mut saw_live = false;
        let mut mutated = false;

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return GroupScan::Unknown,
            };
            let Some(pid) = parse_pid(entry.file_name().as_os_str()) else {
                continue;
            };
            numeric_entries = match numeric_entries.checked_add(1) {
                Some(count) => count,
                None => return GroupScan::Unknown,
            };
            let stat = match self.read_stat(pid) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return GroupScan::Unknown,
            };
            if stat.pid != pid {
                return GroupScan::Unknown;
            }
            match classify_group_member(&stat, leader_raw, session_raw, harness_raw) {
                Ok(GroupMember::Outside | GroupMember::OtherZombie) => {}
                Ok(GroupMember::Live) => saw_live = true,
                Ok(GroupMember::LeaderZombie) => saw_leader = true,
                Ok(GroupMember::OwnedZombie(pid)) => {
                    if exact_reap(pid).is_err() {
                        return GroupScan::OwnershipLost;
                    }
                    mutated = true;
                }
                Err(()) => return GroupScan::Unknown,
            }
        }

        let snapshot = self.pid_max.load(Ordering::Relaxed);
        if numeric_entries > snapshot {
            let refreshed = match self.read_pid_max() {
                Ok(value) => value,
                Err(_) => return GroupScan::Unknown,
            };
            self.pid_max.store(refreshed, Ordering::Relaxed);
            if numeric_entries > refreshed {
                return GroupScan::Unknown;
            }
        }
        if saw_live {
            GroupScan::Live
        } else if !saw_leader {
            GroupScan::Unknown
        } else if mutated {
            GroupScan::Mutated
        } else {
            GroupScan::Complete
        }
    }

    fn validate_identity(
        &self,
        cancellation: Option<&CancellationToken>,
    ) -> Result<u32, HostError> {
        maybe_cancel(cancellation)?;
        if proc_mount_id(&self.procfs)? != self.mount_id {
            return Err(HostError::Unsupported);
        }
        maybe_cancel(cancellation)?;
        let self_stat = read_bounded(&self.procfs, Path::new("self/stat"), STAT_BYTES)
            .map_err(|_| HostError::Unsupported)?;
        let self_stat = proc_stat::parse(&self_stat).ok_or(HostError::Unsupported)?;
        validate_harness_identity(self_stat.pid, self.harness)?;
        maybe_cancel(cancellation)?;
        let mountinfo_file = self
            .procfs
            .open("self/mountinfo")
            .map_err(|_| HostError::Unsupported)?;
        mountinfo::validate(mountinfo_file, self.mount_id).map_err(|_| HostError::Unsupported)?;
        maybe_cancel(cancellation)?;
        validate_child_subreaper(
            rustix::process::child_subreaper().map_err(|_| HostError::Unsupported)?,
        )?;
        maybe_cancel(cancellation)?;
        self.read_pid_max()
    }

    fn read_pid_max(&self) -> Result<u32, HostError> {
        let bytes = read_bounded(&self.procfs, Path::new("sys/kernel/pid_max"), PID_MAX_BYTES)
            .map_err(|_| HostError::Unsupported)?;
        let value = parse_decimal(trim_ascii(&bytes)).ok_or(HostError::Unsupported)?;
        if value == 0 || value > MAX_PID {
            Err(HostError::Unsupported)
        } else {
            Ok(value)
        }
    }

    fn read_stat(&self, pid: u32) -> io::Result<ProcStat> {
        let mut path = PathBuf::from(pid.to_string());
        path.push("stat");
        let bytes = read_bounded(&self.procfs, &path, STAT_BYTES)?;
        proc_stat::parse(&bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid proc stat"))
    }

    fn probe_process_table(&self, pid_max: u32) -> Result<(), HostError> {
        let entries = self.procfs.entries().map_err(|_| HostError::Unsupported)?;
        let mut numeric = 0_u32;
        for entry in entries {
            let entry = entry.map_err(|_| HostError::Unsupported)?;
            let Some(pid) = parse_pid(entry.file_name().as_os_str()) else {
                continue;
            };
            numeric = numeric.checked_add(1).ok_or(HostError::Unsupported)?;
            match self.read_stat(pid) {
                Ok(stat) if stat.pid == pid => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(HostError::Unsupported),
            }
        }
        if numeric > pid_max {
            Err(HostError::Unsupported)
        } else {
            Ok(())
        }
    }
}

fn validate_harness_pid(harness: Pid) -> Result<u32, HostError> {
    let raw = u32::try_from(harness.as_raw_pid()).map_err(|_| HostError::Unsupported)?;
    if raw == 1 {
        Err(HostError::Unsupported)
    } else {
        Ok(raw)
    }
}

fn validate_harness_identity(observed: u32, harness: Pid) -> Result<(), HostError> {
    if observed == validate_harness_pid(harness)? {
        Ok(())
    } else {
        Err(HostError::Unsupported)
    }
}

fn validate_child_subreaper(setting: Option<Pid>) -> Result<(), HostError> {
    if setting.is_none() {
        Ok(())
    } else {
        Err(HostError::Unsupported)
    }
}

fn classify_group_member(
    stat: &ProcStat,
    leader: u32,
    expected_session: u32,
    harness: u32,
) -> Result<GroupMember, ()> {
    if stat.process_group != leader {
        return Ok(GroupMember::Outside);
    }
    if stat.session != expected_session {
        return Err(());
    }
    let zombie = stat.state == b'Z' || stat.state == b'X';
    if !zombie || stat.threads > 1 {
        return Ok(GroupMember::Live);
    }
    if stat.threads != 1 {
        return Err(());
    }
    if stat.pid == leader {
        Ok(GroupMember::LeaderZombie)
    } else if stat.parent == harness {
        Ok(GroupMember::OwnedZombie(stat.pid))
    } else {
        Ok(GroupMember::OtherZombie)
    }
}

fn proc_mount_id(procfs: &Dir) -> Result<u64, HostError> {
    let stats = rustix::fs::fstatfs(procfs).map_err(|_| HostError::Unsupported)?;
    if stats.f_type != PROC_SUPER_MAGIC {
        return Err(HostError::Unsupported);
    }
    let statx = rustix::fs::statx(procfs, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map_err(|_| HostError::Unsupported)?;
    if !StatxFlags::from_bits_retain(statx.stx_mask).contains(StatxFlags::MNT_ID)
        || statx.stx_mnt_id == 0
    {
        Err(HostError::Unsupported)
    } else {
        Ok(statx.stx_mnt_id)
    }
}

fn exact_reap(raw_pid: u32) -> Result<(), HostError> {
    let raw_pid = i32::try_from(raw_pid).map_err(|_| HostError::Unsupported)?;
    let pid = Pid::from_raw(raw_pid).ok_or(HostError::Unsupported)?;
    let options = WaitOptions::NOHANG | WaitOptions::from_bits_retain(libc::__WALL as u32);
    loop {
        match rustix::process::waitpid(Some(pid), options) {
            Ok(Some((actual, status)))
                if actual == pid && (status.exited() || status.signaled()) =>
            {
                return Ok(());
            }
            Ok(_) | Err(rustix::io::Errno::CHILD) => return Err(HostError::Unsupported),
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return Err(HostError::Unsupported),
        }
    }
}

fn read_bounded(dir: &Dir, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut file = dir.open(path)?;
    let capacity = limit
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid read limit"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(u64::try_from(capacity).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > limit {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pseudo-file is empty or overlong",
        ))
    } else {
        Ok(bytes)
    }
}

fn parse_pid(name: &OsStr) -> Option<u32> {
    parse_decimal(name.as_bytes()).filter(|pid| *pid != 0)
}

fn parse_decimal(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        value.checked_mul(10)?.checked_add(u32::from(*byte - b'0'))
    })
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn maybe_cancel(cancellation: Option<&CancellationToken>) -> Result<(), HostError> {
    match cancellation {
        Some(cancellation) => check_cancel(cancellation),
        None => Ok(()),
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use std::{
        ffi::c_void,
        path::PathBuf,
        process::{Child, Command},
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    const SUBREAPER_MODE: &str = "DSH_LINUX_SUBREAPER_TEST_MODE";
    const CLONE_PARENT_MODE: &str = "DSH_LINUX_CLONE_PARENT_TEST_MODE";
    const CLONE_PARENT_PID_FILE: &str = "DSH_LINUX_CLONE_PARENT_PID_FILE";

    #[test]
    fn opens_and_rechecks_the_real_procfs_host() {
        let host = PlatformHost::open().unwrap();
        host.recheck(&CancellationToken::new()).unwrap();
    }

    #[test]
    fn pid_parser_rejects_signs_zero_and_overflow() {
        assert_eq!(parse_pid(OsStr::new("42")), Some(42));
        assert_eq!(parse_pid(OsStr::new("0")), None);
        assert_eq!(parse_pid(OsStr::new("-1")), None);
        assert_eq!(parse_pid(OsStr::new("4294967296")), None);
    }

    #[test]
    fn namespace_pid_one_is_rejected_by_the_production_identity_check() {
        assert_eq!(validate_harness_pid(Pid::INIT), Err(HostError::Unsupported));
        assert_eq!(
            validate_harness_identity(41, Pid::from_raw(42).unwrap()),
            Err(HostError::Unsupported)
        );
        assert_eq!(
            validate_harness_identity(42, Pid::from_raw(42).unwrap()),
            Ok(())
        );
    }

    #[test]
    fn explicit_child_subreaper_is_rejected_in_isolated_hosts() {
        for mode in ["open", "recheck"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("tools::process::linux::tests::child_subreaper_rejection_helper")
                .arg("--nocapture")
                .env(SUBREAPER_MODE, mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated child-subreaper case {mode} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn child_subreaper_rejection_helper() {
        let Some(mode) = std::env::var_os(SUBREAPER_MODE) else {
            return;
        };
        assert_eq!(rustix::process::child_subreaper().unwrap(), None);
        let host = if mode == "recheck" {
            Some(PlatformHost::open().unwrap())
        } else {
            None
        };

        rustix::process::set_child_subreaper(Some(Pid::INIT)).unwrap();
        assert_eq!(
            validate_child_subreaper(rustix::process::child_subreaper().unwrap()),
            Err(HostError::Unsupported)
        );

        match mode.to_str().unwrap() {
            "open" => assert!(matches!(PlatformHost::open(), Err(HostError::Unsupported))),
            "recheck" => assert_eq!(
                host.unwrap().recheck(&CancellationToken::new()),
                Err(HostError::Unsupported)
            ),
            other => panic!("unknown isolated child-subreaper mode {other}"),
        }
    }

    #[test]
    fn zombie_with_another_thread_is_live_and_never_reapable() {
        let stat = ProcStat {
            pid: 43,
            state: b'Z',
            parent: 7,
            process_group: 42,
            session: 42,
            threads: 2,
        };
        assert_eq!(
            classify_group_member(&stat, 42, 42, 7),
            Ok(GroupMember::Live)
        );
    }

    #[test]
    fn same_group_clone_parent_zombies_are_exactly_reaped() {
        for mode in ["sigchld", "no-exit-signal"] {
            assert_clone_parent_zombie_is_reaped(mode);
        }
    }

    fn assert_clone_parent_zombie_is_reaped(mode: &str) {
        let pid_file = clone_pid_file(mode);
        let _ = std::fs::remove_file(&pid_file);
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tools::process::linux::tests::clone_parent_zombie_helper")
            .arg("--nocapture")
            .env(CLONE_PARENT_MODE, mode)
            .env(CLONE_PARENT_PID_FILE, &pid_file)
            .spawn()
            .unwrap();
        let leader_raw = helper.id();
        let clone_raw = wait_for_clone_pid(&mut helper, &pid_file);
        let leader_stat = wait_for_zombie(leader_raw);
        let clone_stat = wait_for_zombie(clone_raw);
        assert_eq!(leader_stat.process_group, leader_raw);
        assert_eq!(leader_stat.session, leader_raw);
        assert_eq!(clone_stat.parent, std::process::id());
        assert_eq!(clone_stat.process_group, leader_raw);
        assert_eq!(clone_stat.session, leader_raw);

        let host = PlatformHost::open().unwrap();
        let leader = Pid::from_raw(i32::try_from(leader_raw).unwrap()).unwrap();
        let clone = Pid::from_raw(i32::try_from(clone_raw).unwrap()).unwrap();
        let harness = rustix::process::getpid();
        let first = host.scan_group(leader, leader, harness);
        let post_scan_wait = rustix::process::waitpid(
            Some(clone),
            WaitOptions::NOHANG | WaitOptions::from_bits_retain(libc::__WALL as u32),
        );
        let second = host.scan_group(leader, leader, harness);
        let helper_status = helper.wait().unwrap();
        let _ = std::fs::remove_file(pid_file);

        assert_eq!(first, GroupScan::Mutated, "mode {mode}");
        assert!(
            matches!(post_scan_wait, Err(rustix::io::Errno::CHILD)),
            "the production scan must have reaped the exact clone child in mode {mode}: \
             {post_scan_wait:?}"
        );
        assert_eq!(second, GroupScan::Complete, "mode {mode}");
        assert!(helper_status.success(), "helper failed in mode {mode}");
    }

    #[test]
    fn clone_parent_zombie_helper() {
        let Some(mode) = std::env::var_os(CLONE_PARENT_MODE) else {
            return;
        };
        let pid_file = PathBuf::from(std::env::var_os(CLONE_PARENT_PID_FILE).unwrap());
        rustix::process::setsid().unwrap();
        let mut stack = vec![0_u128; 4_096];
        let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()) }.cast::<c_void>();
        let exit_signal = match mode.to_str().unwrap() {
            "sigchld" => libc::SIGCHLD,
            "no-exit-signal" => 0,
            other => panic!("unknown CLONE_PARENT mode {other}"),
        };
        // SAFETY: This helper is a dedicated subprocess. The clone does not
        // share memory, files, or signal handlers, receives a 16-byte-aligned
        // private stack, and its C callback immediately returns without using
        // Rust runtime state. CLONE_PARENT is the behavior under test.
        let clone = unsafe {
            libc::clone(
                cloned_child_exit,
                stack_top,
                libc::CLONE_PARENT | exit_signal,
                std::ptr::null_mut::<c_void>(),
            )
        };
        assert!(clone > 0, "clone failed: {}", io::Error::last_os_error());
        std::fs::write(pid_file, clone.to_string()).unwrap();
    }

    extern "C" fn cloned_child_exit(_: *mut c_void) -> libc::c_int {
        0
    }

    fn clone_pid_file(mode: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dsh-phase6-clone-parent-{}-{mode}.pid",
            std::process::id()
        ))
    }

    fn wait_for_clone_pid(helper: &mut Child, path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::read_to_string(path) {
                Ok(value) => return value.parse().unwrap(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot read clone PID file: {error}"),
            }
            if Instant::now() >= deadline {
                let _ = helper.kill();
                let status = helper.wait().unwrap();
                panic!("clone helper did not publish its child PID: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_zombie(raw_pid: u32) -> ProcStat {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let bytes = std::fs::read(format!("/proc/{raw_pid}/stat")).unwrap();
            let stat = proc_stat::parse(&bytes).unwrap();
            if stat.state == b'Z' || stat.state == b'X' {
                return stat;
            }
            assert!(
                Instant::now() < deadline,
                "process {raw_pid} did not become a zombie"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
