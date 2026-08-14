#![allow(unsafe_code)]

use std::{
    ffi::c_void,
    mem::{MaybeUninit, size_of},
    sync::atomic::{AtomicUsize, Ordering},
};

use rustix::process::Pid;
use tokio_util::sync::CancellationToken;

use super::host::{GroupScan, HostError, check_cancel};

const MAX_PROCESS_COUNT: usize = 4_194_304;

pub(super) struct PlatformHost {
    slots: AtomicUsize,
}

impl PlatformHost {
    pub(super) fn open() -> Result<Self, HostError> {
        let slots = process_slot_ceiling()?;
        probe_self()?;
        Ok(Self {
            slots: AtomicUsize::new(slots),
        })
    }

    pub(super) fn recheck(&self, cancellation: &CancellationToken) -> Result<(), HostError> {
        check_cancel(cancellation)?;
        let slots = process_slot_ceiling()?;
        check_cancel(cancellation)?;
        probe_self()?;
        self.slots.fetch_max(slots, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn scan_group(
        &self,
        leader: Pid,
        _expected_session: Pid,
        _harness: Pid,
    ) -> GroupScan {
        let mut capacity = self.slots.load(Ordering::Relaxed);
        for _ in 0..2 {
            let mut pids = Vec::<libc::pid_t>::new();
            if pids.try_reserve_exact(capacity).is_err() {
                return GroupScan::Unknown;
            }
            pids.resize(capacity, 0);
            let byte_len = match capacity
                .checked_mul(size_of::<libc::pid_t>())
                .and_then(|bytes| libc::c_int::try_from(bytes).ok())
            {
                Some(bytes) => bytes,
                None => return GroupScan::Unknown,
            };

            clear_errno();
            // SAFETY: `pids` owns `byte_len` initialized writable bytes for the
            // duration of the call. libproc returns a count of pid slots.
            let returned = unsafe {
                libc::proc_listpgrppids(
                    leader.as_raw_pid(),
                    pids.as_mut_ptr().cast::<c_void>(),
                    byte_len,
                )
            };
            if returned <= 0 || current_errno() != 0 {
                return GroupScan::Unknown;
            }
            let count = match usize::try_from(returned) {
                Ok(count) if count <= capacity => count,
                _ => return GroupScan::Unknown,
            };
            if count == capacity {
                let Ok(refreshed) = process_slot_ceiling() else {
                    return GroupScan::Unknown;
                };
                self.slots.fetch_max(refreshed, Ordering::Relaxed);
                if refreshed <= capacity {
                    return GroupScan::Live;
                }
                capacity = refreshed;
                continue;
            }

            pids.truncate(count);
            pids.sort_unstable();
            if pids.iter().any(|pid| *pid <= 0) || pids.windows(2).any(|pair| pair[0] == pair[1]) {
                return GroupScan::Unknown;
            }

            let expected = match u32::try_from(leader.as_raw_pid()) {
                Ok(expected) => expected,
                Err(_) => return GroupScan::Unknown,
            };
            let mut saw_leader_zombie = false;
            let mut saw_live = false;
            for pid in pids {
                let Some(info) = short_info(pid) else {
                    return GroupScan::Unknown;
                };
                let Ok(requested) = u32::try_from(pid) else {
                    return GroupScan::Unknown;
                };
                if info.pbsi_pid != requested || info.pbsi_pgid != expected {
                    return GroupScan::Unknown;
                }
                if info.pbsi_status == libc::SZOMB {
                    if info.pbsi_pid == expected {
                        saw_leader_zombie = true;
                    }
                } else {
                    saw_live = true;
                }
            }
            return if saw_live {
                GroupScan::Live
            } else if saw_leader_zombie {
                GroupScan::Complete
            } else {
                GroupScan::Unknown
            };
        }
        GroupScan::Live
    }
}

fn process_slot_ceiling() -> Result<usize, HostError> {
    let hint = process_count_hint()?;
    let maxproc = max_processes()?;
    hint.max(maxproc)
        .checked_add(1)
        .filter(|slots| *slots <= MAX_PROCESS_COUNT.saturating_add(1))
        .ok_or(HostError::Unsupported)
}

fn process_count_hint() -> Result<usize, HostError> {
    clear_errno();
    // SAFETY: A null buffer with zero length is libproc's documented sizing query.
    let value = unsafe { libc::proc_listpgrppids(0, std::ptr::null_mut(), 0) };
    if value <= 0 || current_errno() != 0 {
        return Err(HostError::Unsupported);
    }
    usize::try_from(value)
        .ok()
        .filter(|value| *value <= MAX_PROCESS_COUNT)
        .ok_or(HostError::Unsupported)
}

fn max_processes() -> Result<usize, HostError> {
    let mut value = MaybeUninit::<libc::c_int>::uninit();
    let mut length = size_of::<libc::c_int>();
    // SAFETY: The name is NUL terminated; `value` and `length` are valid for
    // this read-only sysctl and no output buffer is supplied.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.maxproc".as_ptr(),
            value.as_mut_ptr().cast::<c_void>(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || length != size_of::<libc::c_int>() {
        return Err(HostError::Unsupported);
    }
    // SAFETY: A successful exact-size sysctl initialized `value`.
    let value = unsafe { value.assume_init() };
    usize::try_from(value)
        .ok()
        .filter(|value| *value != 0 && *value <= MAX_PROCESS_COUNT)
        .ok_or(HostError::Unsupported)
}

fn probe_self() -> Result<(), HostError> {
    let pid = rustix::process::getpid().as_raw_pid();
    let info = short_info(pid).ok_or(HostError::Unsupported)?;
    if info.pbsi_pid == u32::try_from(pid).map_err(|_| HostError::Unsupported)? {
        Ok(())
    } else {
        Err(HostError::Unsupported)
    }
}

fn short_info(pid: libc::pid_t) -> Option<libc::proc_bsdshortinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    clear_errno();
    // SAFETY: `info` points to an exactly sized writable object. The nonzero
    // arg asks XNU to retain a zombie reference during the lookup.
    let returned = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            1,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if returned != size || current_errno() != 0 {
        return None;
    }
    // SAFETY: libproc reported that it initialized the entire object.
    Some(unsafe { info.assume_init() })
}

fn clear_errno() {
    // SAFETY: `__error` returns this thread's valid errno location on macOS.
    unsafe { *libc::__error() = 0 };
}

fn current_errno() -> libc::c_int {
    // SAFETY: `__error` returns this thread's valid errno location on macOS.
    unsafe { *libc::__error() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_host_process_limits_and_self_probe_are_usable() {
        let ceiling = process_slot_ceiling().unwrap();
        assert!((2..=MAX_PROCESS_COUNT + 1).contains(&ceiling));
        probe_self().unwrap();
    }

    #[test]
    fn process_runner_host_opens_on_the_supported_macos_test_host() {
        PlatformHost::open().unwrap();
    }
}
