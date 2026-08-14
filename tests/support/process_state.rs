use rustix::process::Pid;

#[cfg(target_os = "linux")]
pub fn is_stopped(pid: Pid) -> Option<bool> {
    let bytes = std::fs::read(format!("/proc/{}/stat", pid.as_raw_pid())).ok()?;
    // Linux permits spaces and ')' inside the process name, so the last ')'
    // is the only safe boundary before the one-byte state field.
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    let state = *bytes.get(close.checked_add(2)?)?;
    Some(matches!(state, b'T' | b't'))
}

#[cfg(target_os = "macos")]
pub fn is_stopped(pid: Pid) -> Option<bool> {
    use std::{
        ffi::c_void,
        mem::{MaybeUninit, size_of},
    };

    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    // SAFETY: `info` is an exact-size writable object and libproc does not
    // retain the pointer after this call.
    let returned = unsafe {
        libc::proc_pidinfo(
            pid.as_raw_pid(),
            libc::PROC_PIDT_SHORTBSDINFO,
            1,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if returned != size {
        return None;
    }
    // SAFETY: an exact-size successful return initialized the whole object.
    let info = unsafe { info.assume_init() };
    (info.pbsi_pid == u32::try_from(pid.as_raw_pid()).ok()?)
        .then_some(info.pbsi_status == libc::SSTOP)
}
