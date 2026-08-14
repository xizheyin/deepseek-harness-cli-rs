#![allow(unsafe_code)]

use std::{
    ffi::OsString,
    io,
    os::{fd::OwnedFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
};

pub(super) fn shell(
    command: String,
    workdir: OwnedFd,
    environment: &[(OsString, OsString)],
) -> io::Result<Child> {
    let mut process = Command::new("/bin/bash");
    process
        .arg0("bash")
        .args(["--noprofile", "--norc", "-c"])
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(environment.iter().map(|(name, value)| (name, value)));

    // SAFETY: The closure runs after fork and before exec. It performs only
    // `setsid` and `fchdir`, both async-signal-safe syscalls, and converts the
    // returned errno without allocating, locking, formatting, or logging.
    unsafe {
        process.pre_exec(move || {
            rustix::process::setsid().map_err(io::Error::from)?;
            rustix::process::fchdir(&workdir).map_err(io::Error::from)
        });
    }
    process.spawn()
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use super::*;

    #[test]
    fn fixed_bash_has_null_stdin_and_uses_the_held_directory() {
        let directory: OwnedFd = std::fs::File::open(".").unwrap().into();
        let mut child = shell(
            "read ignored || printf 'stdin-closed:%s' \"$0\"".to_owned(),
            directory,
            &[],
        )
        .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(bytes, b"stdin-closed:bash");
    }
}
