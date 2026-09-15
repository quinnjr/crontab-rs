//! Executing a single cron job: identity switch, environment, stdin,
//! output capture and mail delivery.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};

use nix::unistd::{User, geteuid};

use crate::crontab::{Entry, PROTECTED_VARS, env_get};
use crate::mail::{Mailer, Message, mailto_is_safe};
use crate::privs::{Creds, configure_child};

/// Upper bound on job output retained for mail/log (the rest is drained).
const MAX_OUTPUT: usize = 1024 * 1024;

/// Shared settings for running jobs.
#[derive(Debug, Clone)]
pub struct Runner {
    pub default_path: String,
    pub default_shell: String,
    /// `-P`: inherit `PATH` from the daemon's environment.
    pub inherit_path: bool,
    pub mailer: Mailer,
    pub hostname: String,
}

/// Result of running a job.
#[derive(Debug)]
pub struct Outcome {
    pub status: ExitStatus,
    pub output: Vec<u8>,
}

impl Runner {
    /// Build the job's environment in cronie order: defaults, then crontab
    /// assignments (which may override `SHELL`, `PATH`, `HOME`), with
    /// `LOGNAME`/`USER` fixed to the running user.
    pub fn build_env(&self, pw: &User, entry: &Entry) -> Vec<(String, String)> {
        let path = if self.inherit_path {
            std::env::var("PATH").unwrap_or_else(|_| self.default_path.clone())
        } else {
            self.default_path.clone()
        };
        let mut env: Vec<(String, String)> = vec![
            ("SHELL".into(), self.default_shell.clone()),
            ("PATH".into(), path),
            ("HOME".into(), pw.dir.to_string_lossy().into_owned()),
            ("LOGNAME".into(), pw.name.clone()),
            ("USER".into(), pw.name.clone()),
        ];
        for (k, v) in &entry.env {
            if PROTECTED_VARS.contains(&k.as_str()) {
                continue;
            }
            match env.iter_mut().find(|(n, _)| n == k) {
                Some(slot) => slot.1 = v.clone(),
                None => env.push((k.clone(), v.clone())),
            }
        }
        env
    }

    /// Run `entry` as `user`, wait for it, and deliver its output.
    pub fn run(&self, user: &str, entry: &Entry) -> io::Result<Outcome> {
        let pw = User::from_name(user)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other(format!("no passwd entry for {user}")))?;
        let switch = should_switch(geteuid().as_raw(), pw.uid.as_raw())
            .map_err(|e| io::Error::other(format!("cannot run job as {user}: {e}")))?;
        if switch
            && pw.uid.as_raw() != 0
            && let Some(expire) = shadow_expire(user)
            && account_expired(expire, today_days())
        {
            log::error!("({user}) ACCOUNT EXPIRED (job not run)");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("account {user} has expired"),
            ));
        }
        let creds = if switch {
            match Creds::from_user(&pw) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::error!("({user}) ERROR (cannot resolve credentials: {e})");
                    return Err(e);
                }
            }
        } else {
            None
        };
        let env = self.build_env(&pw, entry);
        let shell = env_get(&env, "SHELL").unwrap_or("/bin/sh").to_string();

        if !entry.quiet {
            log::info!("({user}) CMD ({})", entry.raw_command);
        }

        let (read_end, write_end) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(io::Error::other)?;
        // try_clone uses F_DUPFD_CLOEXEC, so the duplicate is close-on-exec too.
        let write_dup = write_end.try_clone()?;

        let mut cmd = Command::new(&shell);
        cmd.arg0(&shell)
            .arg("-c")
            .arg(&entry.command)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::from(write_end))
            .stderr(Stdio::from(write_dup))
            .stdin(if entry.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        if let Err(e) = configure_child(&mut cmd, creds, true, Some(pw.dir.clone())) {
            log::error!("({user}) ERROR (can't prepare job: {e})");
            return Err(e);
        }

        let spawned = cmd.spawn();
        // Close our copies of the pipe's write end so EOF arrives when the
        // job (and anything it left running with the fds) exits.
        drop(cmd);
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                log::error!("({user}) ERROR (can't execute {shell}: {e})");
                return Err(e);
            }
        };

        let stdin_thread = match (child.stdin.take(), entry.stdin.clone()) {
            (Some(mut pipe), Some(data)) => Some(std::thread::spawn(move || -> io::Result<()> {
                pipe.write_all(data.as_bytes())
            })),
            _ => None,
        };

        let mut output = Vec::new();
        let mut discarded: u64 = 0;
        let mut reader = File::from(read_end);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(output.len());
                    let keep = n.min(room);
                    output.extend_from_slice(&buf[..keep]);
                    discarded += (n - keep) as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::error!("({user}) ERROR (reading job output: {e})");
                    break;
                }
            }
        }
        drop(reader);
        if discarded > 0 {
            log::warn!("({user}) ERROR (job output truncated: {discarded} bytes discarded)");
            output.extend_from_slice(
                format!("\n[crond: {discarded} bytes of output discarded]\n").as_bytes(),
            );
        }
        let status = child.wait()?;
        if let Some(t) = stdin_thread {
            match t.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => {}
                Ok(Err(e)) => log::error!("({user}) ERROR (writing job stdin: {e})"),
                Err(_) => log::error!("({user}) ERROR (job stdin writer panicked)"),
            }
        }

        if !entry.quiet {
            log::info!("({user}) CMDEND ({})", entry.raw_command);
        }
        if !status.success() {
            log::debug!("({user}) CMDSTATUS ({}) {status}", entry.raw_command);
        }

        self.deliver(&pw, entry, &env, status, &output, switch);
        Ok(Outcome { status, output })
    }

    fn deliver(
        &self,
        pw: &User,
        entry: &Entry,
        env: &[(String, String)],
        status: ExitStatus,
        output: &[u8],
        switch: bool,
    ) {
        if output.is_empty() {
            return;
        }
        if entry.mail_on_failure_only && status.success() {
            return;
        }
        let user = pw.name.as_str();
        let mailto = env_get(env, "MAILTO").unwrap_or(user).trim().to_string();
        if mailto.is_empty() {
            return;
        }
        if self.mailer.is_off() {
            for line in String::from_utf8_lossy(output).lines() {
                log::info!("({user}) CMDOUT ({line})");
            }
            return;
        }
        let mailfrom = env_get(env, "MAILFROM").unwrap_or("").trim().to_string();
        if !mailto_is_safe(&mailto) || !mailto_is_safe(&mailfrom) {
            log::error!("({user}) ERROR (unsafe MAILTO/MAILFROM, output discarded)");
            return;
        }
        let from = if mailfrom.is_empty() {
            format!("{user} (Cron Daemon)")
        } else {
            mailfrom.clone()
        };
        let content_type = env_get(env, "CONTENT_TYPE")
            .filter(|v| mailto_is_safe(v))
            .unwrap_or("text/plain; charset=UTF-8")
            .to_string();
        let subject: String = format!("Cron <{user}@{}> {}", self.hostname, entry.raw_command)
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let msg = Message {
            from,
            envelope_from: (!mailfrom.is_empty()).then(|| mailfrom.clone()),
            to: mailto.clone(),
            subject,
            content_type,
            env: env
                .iter()
                .filter(|(_, v)| mailto_is_safe(v))
                .cloned()
                .collect(),
            body: output.to_vec(),
        };
        let creds = if switch {
            match Creds::from_user(pw) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::error!("({user}) MAIL ERROR (cannot resolve credentials: {e})");
                    return;
                }
            }
        } else {
            None
        };
        match self.mailer.send(&msg, creds) {
            Ok(()) => log::debug!("({user}) MAIL (mailed {} bytes to {mailto})", output.len()),
            Err(e) => log::error!("({user}) MAIL ERROR ({e})"),
        }
    }
}

/// Decide whether a job for `target_uid` needs an identity switch when the
/// daemon runs with effective uid `euid`.
pub fn should_switch(euid: u32, target_uid: u32) -> Result<bool, String> {
    if euid == 0 {
        Ok(true)
    } else if euid == target_uid {
        Ok(false)
    } else {
        Err("daemon is not running as root".into())
    }
}

/// Is an account with shadow `sp_expire` (days since the epoch; `<= 0`
/// meaning never) expired on day `today_days`?
fn account_expired(sp_expire: i64, today_days: i64) -> bool {
    sp_expire > 0 && today_days >= sp_expire
}

/// Days since the Unix epoch, UTC.
fn today_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

/// The shadow `sp_expire` field for `user`, or `None` when the shadow
/// database can't be read or has no entry (which never blocks a job).
fn shadow_expire(user: &str) -> Option<i64> {
    let name = std::ffi::CString::new(user).ok()?;
    let mut buflen = 1024usize;
    loop {
        let mut buf = vec![0 as libc::c_char; buflen];
        // SAFETY: an all-zero spwd is a valid value (null pointers, zero ints).
        let mut sp: libc::spwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::spwd = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the given sizes for the call's duration.
        let rc = unsafe {
            libc::getspnam_r(
                name.as_ptr(),
                &mut sp,
                buf.as_mut_ptr(),
                buflen,
                &mut result,
            )
        };
        if rc == libc::ERANGE && buflen < 1 << 20 {
            buflen *= 4;
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        return Some(sp.sp_expire as i64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crontab::{Crontab, Format};

    #[test]
    fn switch_decision() {
        assert_eq!(should_switch(0, 1000), Ok(true));
        assert_eq!(should_switch(0, 0), Ok(true));
        assert_eq!(should_switch(1000, 1000), Ok(false));
        assert!(
            should_switch(1000, 0)
                .unwrap_err()
                .contains("daemon is not running as root")
        );
    }

    #[test]
    fn account_expiry() {
        assert!(!account_expired(-1, 20_000));
        assert!(!account_expired(0, 20_000));
        assert!(!account_expired(20_001, 20_000));
        assert!(account_expired(20_000, 20_000));
        assert!(account_expired(19_999, 20_000));
    }

    #[test]
    fn non_root_cannot_run_as_root() {
        if geteuid().is_root() {
            return;
        }
        let err = runner(Mailer::Off)
            .run("root", &entry("* * * * * true\n"))
            .unwrap_err();
        assert!(
            err.to_string().contains("daemon is not running as root"),
            "{err}"
        );
    }

    #[test]
    fn unknown_user_is_rejected() {
        let err = runner(Mailer::Off)
            .run("no-such-user-xyz123", &entry("* * * * * true\n"))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no passwd entry for no-such-user-xyz123"),
            "{err}"
        );
    }

    #[test]
    fn oversized_output_is_truncated_with_notice() {
        let e = entry("* * * * * head -c 1100000 /dev/zero\n");
        let out = runner(Mailer::Off).run(&me().name, &e).unwrap();
        let tail = format!(
            "\n[crond: {} bytes of output discarded]\n",
            1_100_000 - MAX_OUTPUT
        );
        assert!(out.output.ends_with(tail.as_bytes()));
        assert_eq!(out.output.len(), MAX_OUTPUT + tail.len());
    }

    fn me() -> User {
        User::from_uid(nix::unistd::getuid()).unwrap().unwrap()
    }

    fn runner(mailer: Mailer) -> Runner {
        Runner {
            default_path: "/usr/bin:/bin".into(),
            default_shell: "/bin/sh".into(),
            inherit_path: false,
            mailer,
            hostname: "testhost".into(),
        }
    }

    fn entry(text: &str) -> Entry {
        Crontab::parse(text, Format::User)
            .unwrap()
            .entries
            .remove(0)
    }

    #[test]
    fn environment_and_output() {
        let r = runner(Mailer::Off);
        let e = entry(
            "FOO=bar\nLOGNAME=evil\n* * * * * echo \"$FOO $LOGNAME $SHELL\"; pwd; echo err >&2\n",
        );
        let out = r.run(&me().name, &e).unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.output).unwrap();
        let home = me().dir.canonicalize().unwrap();
        assert_eq!(
            text,
            format!("bar {} /bin/sh\n{}\nerr\n", me().name, home.display())
        );
    }

    #[test]
    fn stdin_from_percent() {
        let r = runner(Mailer::Off);
        let e = entry("* * * * * tr a-z A-Z%hello%world\n");
        let out = r.run(&me().name, &e).unwrap();
        assert_eq!(out.output, b"HELLO\nWORLD\n");
    }

    #[test]
    fn mail_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mbox = dir.path().join("mbox");
        let r = runner(Mailer::Command(format!("cat >> '{}'", mbox.display())));

        r.run(&me().name, &entry("MAILTO=someone\n* * * * * echo one\n"))
            .unwrap();
        let text = std::fs::read_to_string(&mbox).unwrap();
        assert!(text.contains("To: someone\n"));
        assert!(text.contains("Subject: Cron <"));
        assert!(text.contains("@testhost> echo one\n"));
        assert!(text.ends_with("\n\none\n"));

        // MAILTO="" suppresses, -n suppresses on success, silent jobs send nothing.
        std::fs::remove_file(&mbox).unwrap();
        r.run(&me().name, &entry("MAILTO=\"\"\n* * * * * echo two\n"))
            .unwrap();
        r.run(&me().name, &entry("* * * * * -n echo three\n"))
            .unwrap();
        r.run(&me().name, &entry("* * * * * true\n")).unwrap();
        assert!(!mbox.exists());

        let out = r
            .run(&me().name, &entry("* * * * * -n echo four; exit 3\n"))
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert!(std::fs::read_to_string(&mbox).unwrap().contains("four"));
    }

    #[test]
    fn background_children_do_not_hang_on_closed_output() {
        let r = runner(Mailer::Off);
        let e = entry("* * * * * echo quick\n");
        let out = r.run(&me().name, &e).unwrap();
        assert_eq!(out.output, b"quick\n");
    }
}
