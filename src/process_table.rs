//! Native process-group inspection for the supported Unix platforms.
//!
//! Linux reads procfs; macOS queries libproc. Neither path launches a helper.
use std::io;

/// Whether a group has any non-zombie members. Missing groups are finished;
/// inspection failures remain errors rather than being mistaken for exit.
pub(crate) fn group_alive(pgid: u32) -> io::Result<bool> {
    let pgid = i32::try_from(pgid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group id"))?;
    platform::group_alive(pgid)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn group_alive(pgid: i32) -> io::Result<bool> {
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            // Only read stat files for our group: unrelated processes may have
            // unreadable procfs entries. Recheck membership in stat to handle
            // exit, PID reuse, and group changes between these two snapshots.
            // SAFETY: getpgid takes a numeric PID and writes no user memory.
            let group = unsafe { libc::getpgid(pid) };
            if group == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(error);
            }
            if group != pgid {
                continue;
            }
            let stat = match std::fs::read(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let (group, live) = parse_stat(&stat)?;
            if group == pgid && live {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Fields 3 (state) and 5 (pgrp) follow the parenthesized comm field. comm
/// can contain spaces, newlines, parentheses, and non-UTF-8 bytes; parse from
/// its final ')' rather than treating the whole record as whitespace fields.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &[u8]) -> io::Result<(i32, bool)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "malformed /proc PID stat");
    let end = stat.iter().rposition(|b| *b == b')').ok_or_else(invalid)?;
    let mut fields = stat[end + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|s| !s.is_empty());
    let state = fields.next().filter(|s| s.len() == 1).ok_or_else(invalid)?[0];
    fields.next().ok_or_else(invalid)?; // ppid
    let group = std::str::from_utf8(fields.next().ok_or_else(invalid)?)
        .map_err(|_| invalid())?
        .parse::<i32>()
        .map_err(|_| invalid())?;
    Ok((group, !matches!(state, b'Z' | b'X' | b'x')))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::mem::{MaybeUninit, size_of};

    pub(super) fn group_alive(pgid: i32) -> io::Result<bool> {
        let mut pids = vec![0i32; 64];
        // The group can grow between queries. Retry a full buffer rather
        // than assuming a sizing query or a truncated snapshot is complete.
        for _ in 0..12 {
            nix::errno::Errno::clear();
            // SAFETY: pids is initialized, aligned storage for buffersize
            // bytes, and libproc writes at most that many bytes. Unlike
            // proc_listpids, proc_listpgrppids returns a PID count, not bytes.
            let count = unsafe {
                libc::proc_listpgrppids(
                    pgid,
                    pids.as_mut_ptr().cast(),
                    (pids.len() * size_of::<i32>()) as i32,
                )
            };
            if count <= 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) | Some(libc::ESRCH) => Ok(false),
                    _ => Err(error),
                };
            }
            if count as usize >= pids.len() {
                pids.resize(pids.len() * 2, 0);
                continue;
            }
            for &pid in &pids[..count as usize] {
                let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
                // SAFETY: the buffer has the exact size and alignment of
                // PROC_PIDT_SHORTBSDINFO. arg=0 excludes zombies (which
                // return ESRCH just like exited processes). Read the
                // struct only after libproc confirms a complete write.
                let bytes = unsafe {
                    libc::proc_pidinfo(
                        pid,
                        libc::PROC_PIDT_SHORTBSDINFO,
                        0,
                        info.as_mut_ptr().cast(),
                        size_of::<libc::proc_bsdshortinfo>() as i32,
                    )
                };
                if bytes == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        continue;
                    }
                    return Err(error);
                }
                if bytes as usize != size_of::<libc::proc_bsdshortinfo>() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "incomplete libproc process info",
                    ));
                }
                // SAFETY: the exact-size return above guarantees initialization.
                let info = unsafe { info.assume_init() };
                if info.pbsi_pgid == pgid as u32 && info.pbsi_status != libc::SZOMB {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        Err(io::Error::other(
            "process group grew beyond the libproc snapshot limit",
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    pub(super) fn group_alive(_pgid: i32) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native process inspection requires Linux or macOS",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn proc_stat_handles_arbitrary_command_names_and_process_states() {
        for comm in [b"worker".as_slice(), b"a ) (b)\n c\xff"] {
            for (state, live) in [(b'S', true), (b'T', true), (b'Z', false), (b'X', false)] {
                let mut stat = b"123 (".to_vec();
                stat.extend_from_slice(comm);
                stat.extend_from_slice(b") ");
                stat.push(state);
                stat.extend_from_slice(b" 1 321 321 0 -1 0\n");
                assert_eq!(parse_stat(&stat).unwrap(), (321, live));
            }
        }
    }

    #[test]
    fn malformed_proc_stat_is_an_error_not_an_exit() {
        for stat in [
            b"".as_slice(),
            b"123 (foo)",
            b"123 (foo) Z 1",
            b"123 (foo) S 1 invalid",
        ] {
            assert_eq!(
                parse_stat(stat).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn native_inspection_finds_live_group() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read value"])
            .stdin(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let result = group_alive(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(result.unwrap());
        assert!(!group_alive(child.id()).unwrap());
    }

    #[test]
    fn native_inspection_ignores_unreaped_zombie_group() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        // SAFETY: waitid initializes info and WNOWAIT leaves this exact child
        // unreaped, so the test observes a zombie without racing init's reaper.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        let result = group_alive(child.id());
        child.wait().unwrap();
        assert_eq!(waited, 0);
        assert!(!result.unwrap());
    }
}
