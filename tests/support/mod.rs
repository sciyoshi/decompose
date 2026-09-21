/// Native liveness observation for integration tests. In particular, an
/// unreaped zombie is finished even though kill(pid, 0) can still succeed.
#[cfg(target_os = "linux")]
pub fn process_alive(pid: u32) -> bool {
    let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(e) => panic!("inspect process {pid}: {e}"),
    };
    let state = status
        .lines()
        .find(|line| line.starts_with("State:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("process state");
    !matches!(state, "Z" | "X" | "x")
}

#[cfg(target_os = "macos")]
pub fn process_alive(pid: u32) -> bool {
    use std::mem::{MaybeUninit, size_of};
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    // SAFETY: info is correctly sized/aligned writable storage; arg=0 asks
    // for live processes only. Only read it after a full successful write.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdshortinfo>() as i32,
        )
    };
    if bytes == 0 {
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.raw_os_error(),
            Some(libc::ESRCH),
            "inspect process {pid}: {error}"
        );
        return false;
    }
    assert_eq!(bytes as usize, size_of::<libc::proc_bsdshortinfo>());
    // SAFETY: the exact-size return above guarantees initialization.
    unsafe { info.assume_init().pbsi_status != libc::SZOMB }
}
