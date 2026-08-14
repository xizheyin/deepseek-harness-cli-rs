#![allow(unsafe_code)]

use tokio_util::sync::CancellationToken;

use rustix::process::Pid;

#[cfg(target_os = "linux")]
use super::linux::PlatformHost;
#[cfg(target_os = "macos")]
use super::macos::PlatformHost;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostError {
    Unsupported,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GroupScan {
    Live,
    Complete,
    #[cfg(target_os = "linux")]
    Mutated,
    Unknown,
    OwnershipLost,
}

pub(super) struct Host {
    platform: PlatformHost,
}

impl Host {
    pub(super) fn open() -> Result<Self, HostError> {
        check_sigchld()?;
        let platform = PlatformHost::open()?;
        Ok(Self { platform })
    }

    pub(super) fn recheck(&self, cancellation: &CancellationToken) -> Result<(), HostError> {
        check_cancel(cancellation)?;
        check_sigchld()?;
        check_cancel(cancellation)?;
        self.platform.recheck(cancellation)
    }

    pub(super) fn scan_group(&self, leader: Pid, expected_session: Pid, harness: Pid) -> GroupScan {
        if check_sigchld().is_err() {
            return GroupScan::OwnershipLost;
        }
        self.platform.scan_group(leader, expected_session, harness)
    }
}

pub(super) fn check_cancel(cancellation: &CancellationToken) -> Result<(), HostError> {
    if cancellation.is_cancelled() {
        Err(HostError::Cancelled)
    } else {
        Ok(())
    }
}

fn check_sigchld() -> Result<(), HostError> {
    let mut old = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: This is the query-only form of sigaction. `old` points to enough
    // writable storage, and a zero return guarantees that libc initialized it.
    let result = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), old.as_mut_ptr()) };
    if result != 0 {
        return Err(HostError::Unsupported);
    }
    // SAFETY: The successful query above initialized the complete sigaction.
    let old = unsafe { old.assume_init() };
    if old.sa_sigaction == libc::SIG_IGN || old.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(HostError::Unsupported);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn ordinary_host_keeps_waitable_sigchld_status() {
        assert_eq!(check_sigchld(), Ok(()));
    }

    #[test]
    fn unsupported_sigchld_dispositions_are_rejected_in_isolated_hosts() {
        for mode in ["ignore", "no-cld-wait"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("tools::process::host::tests::sigchld_disposition_helper")
                .arg("--nocapture")
                .env("DSH_SIGCHLD_TEST_MODE", mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated SIGCHLD case {mode} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn sigchld_disposition_helper() {
        let Some(mode) = std::env::var_os("DSH_SIGCHLD_TEST_MODE") else {
            return;
        };
        let opened_before_change = Host::open().unwrap();
        match mode.to_str().unwrap() {
            "ignore" => {
                // SAFETY: This runs in a dedicated test subprocess that exits
                // immediately afterward. SIG_IGN is a valid SIGCHLD handler.
                let previous = unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
                assert_ne!(previous, libc::SIG_ERR);
            }
            "no-cld-wait" => {
                // SAFETY: A zeroed sigaction with an initialized empty mask,
                // SIG_DFL, and SA_NOCLDWAIT is a valid disposition. The
                // process is isolated so this cannot race the parent test
                // harness or another test's child ownership.
                let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
                action.sa_sigaction = libc::SIG_DFL;
                action.sa_flags = libc::SA_NOCLDWAIT;
                assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
                assert_eq!(
                    unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
                    0
                );
            }
            other => panic!("unknown isolated SIGCHLD mode {other}"),
        }

        assert_eq!(check_sigchld(), Err(HostError::Unsupported));
        assert_eq!(
            opened_before_change.recheck(&CancellationToken::new()),
            Err(HostError::Unsupported)
        );
        assert!(matches!(Host::open(), Err(HostError::Unsupported)));
    }

    #[test]
    fn cancelled_recheck_stops_before_platform_work() {
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(check_cancel(&token), Err(HostError::Cancelled));
    }
}
