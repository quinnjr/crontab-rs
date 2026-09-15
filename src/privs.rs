//! Helpers for switching identity in child processes and temporarily
//! dropping effective privileges in a set-user-ID binary.

use std::ffi::CString;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

use nix::unistd::{Gid, Uid, User, getegid, geteuid, getgid, getuid, setegid, seteuid};

/// Credentials a child process should assume.
#[derive(Debug, Clone)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    pub name: CString,
}

impl Creds {
    pub fn from_user(user: &User) -> Creds {
        Creds {
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            name: CString::new(user.name.as_bytes()).unwrap_or_default(),
        }
    }
}

/// Arrange for `cmd` to optionally start a new session and assume `creds`
/// (supplementary groups, gid, then uid) before exec.
pub fn configure_child(cmd: &mut Command, creds: Option<Creds>, new_session: bool) {
    // SAFETY: the closure runs between fork and exec and only calls libc
    // functions on data prepared before the fork.
    unsafe {
        cmd.pre_exec(move || {
            if new_session {
                libc::setsid();
            }
            if let Some(c) = &creds {
                if libc::initgroups(c.name.as_ptr(), c.gid as libc::gid_t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setgid(c.gid as libc::gid_t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setuid(c.uid as libc::uid_t) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Run `f` with the effective uid/gid set to the real uid/gid, restoring
/// afterwards.  A no-op when the process is not set-ID.
pub fn as_real_user<T>(f: impl FnOnce() -> T) -> T {
    let (euid, egid) = (geteuid(), getegid());
    let (ruid, rgid) = (getuid(), getgid());
    if euid == ruid && egid == rgid {
        return f();
    }
    let dropped = setegid(rgid).is_ok() && seteuid(ruid).is_ok();
    if !dropped {
        eprintln!("crontab: unable to drop privileges");
        std::process::exit(1);
    }
    let r = f();
    if seteuid(euid).is_err() || setegid(egid).is_err() {
        eprintln!("crontab: unable to restore privileges");
        std::process::exit(1);
    }
    r
}

/// True when running with effective uid 0.
pub fn is_root() -> bool {
    geteuid() == Uid::from_raw(0)
}

/// Group id helper for chown calls.
pub fn gid(raw: u32) -> Gid {
    Gid::from_raw(raw)
}
