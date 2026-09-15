//! Helpers for switching identity in child processes and temporarily
//! dropping effective privileges in a set-user-ID binary.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use nix::unistd::{Gid, Uid, User, getegid, geteuid, getgid, getuid, setegid, seteuid};

/// Credentials a child process should assume, fully resolved (including
/// supplementary groups) before any fork so the child never touches NSS.
#[derive(Debug, Clone)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

impl Creds {
    /// Resolve `user`'s supplementary groups (via `getgrouplist`) before fork.
    pub fn from_user(user: &User) -> io::Result<Creds> {
        let name = CString::new(user.name.as_bytes())
            .map_err(|_| io::Error::other(format!("invalid user name {:?}", user.name)))?;
        let gid = user.gid.as_raw();
        let mut cap: libc::c_int = 32;
        loop {
            let mut groups: Vec<libc::gid_t> = vec![0; cap as usize];
            let mut n = cap;
            // SAFETY: `groups` has room for `n` entries; `name` is a valid C string.
            let rc = unsafe {
                libc::getgrouplist(
                    name.as_ptr(),
                    gid as libc::gid_t,
                    groups.as_mut_ptr(),
                    &mut n,
                )
            };
            if rc >= 0 {
                groups.truncate(n.max(0) as usize);
                return Ok(Creds {
                    uid: user.uid.as_raw(),
                    gid,
                    groups,
                });
            }
            // glibc stores the required count in `n` when the buffer is too small.
            let next = if n > cap { n } else { cap.saturating_mul(2) };
            if next > 65536 {
                return Err(io::Error::other(format!(
                    "cannot resolve supplementary groups for {}",
                    user.name
                )));
            }
            cap = next;
        }
    }

    /// Real uid/gid of this process plus its current supplementary groups.
    pub fn real_user() -> io::Result<Creds> {
        // SAFETY: getgroups with size 0 only returns the count.
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
        // SAFETY: `groups` has room for `n` entries.
        let m = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
        if m < 0 {
            return Err(io::Error::last_os_error());
        }
        groups.truncate(m as usize);
        Ok(Creds {
            uid: getuid().as_raw(),
            gid: getgid().as_raw(),
            groups,
        })
    }
}

/// Arrange for `cmd` to optionally start a new session, assume `creds`
/// (supplementary groups, gid, then uid, verified afterwards) and change to
/// `workdir` (falling back to `/`) before exec.
///
/// Fails closed: when this process is a set-ID binary, `creds` is mandatory.
pub fn configure_child(
    cmd: &mut Command,
    creds: Option<Creds>,
    new_session: bool,
    workdir: Option<PathBuf>,
) -> io::Result<()> {
    if creds.is_none() && crate::config::is_privileged_binary() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to run child without dropping privileges",
        ));
    }
    let workdir = workdir
        .map(|p| {
            CString::new(p.as_os_str().as_bytes())
                .map_err(|_| io::Error::other(format!("invalid working directory {p:?}")))
        })
        .transpose()?;
    let creds = creds.map(|c| {
        let groups: Vec<libc::gid_t> = c.groups.iter().map(|&g| g as libc::gid_t).collect();
        (c.uid as libc::uid_t, c.gid as libc::gid_t, groups)
    });

    // SAFETY: the closure runs between fork and exec; it only calls
    // async-signal-safe libc functions on data prepared before the fork and
    // performs no allocation.
    unsafe {
        cmd.pre_exec(move || {
            if new_session {
                libc::setsid();
            }
            if let Some((uid, gid, groups)) = &creds {
                let (uid, gid) = (*uid, *gid);
                // Supplementary groups can only be changed with euid 0; an
                // unprivileged process keeps its own (it cannot gain others).
                if libc::geteuid() == 0
                    && libc::setgroups(groups.len() as libc::size_t, groups.as_ptr()) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::setresgid(gid, gid, gid) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setresuid(uid, uid, uid) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::geteuid() != uid || libc::getegid() != gid {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
                if uid != 0 && libc::setuid(0) == 0 {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
            }
            if let Some(dir) = &workdir
                && libc::chdir(dir.as_ptr()) != 0
                && libc::chdir(c"/".as_ptr()) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn as_real_user_calls_closure_once() {
        let mut calls = 0;
        let v = as_real_user(|| {
            calls += 1;
            42
        });
        assert_eq!(v, 42);
        assert_eq!(calls, 1);
    }

    #[test]
    fn real_user_matches_process_ids() {
        let c = Creds::real_user().unwrap();
        assert_eq!(c.uid, getuid().as_raw());
        assert_eq!(c.gid, getgid().as_raw());
    }

    #[test]
    fn from_user_includes_primary_group() {
        let me = User::from_uid(getuid()).unwrap().unwrap();
        let c = Creds::from_user(&me).unwrap();
        assert_eq!(c.uid, me.uid.as_raw());
        assert!(c.groups.contains(&me.gid.as_raw()));
    }

    #[test]
    fn missing_workdir_falls_back_to_root() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("pwd").stdout(Stdio::piped());
        configure_child(
            &mut cmd,
            Some(Creds::real_user().unwrap()),
            false,
            Some("/nonexistent-crond-test-dir".into()),
        )
        .unwrap();
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"/\n");
    }
}
