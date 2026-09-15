//! Executing a single cron job the way cronie's `do_command` does: identity
//! switch, environment, stdin, output capture, and mail or syslog delivery.
//! Crontab-supplied values are passed to the OS as their original bytes.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

use nix::unistd::{User, geteuid};

use crate::crontab::{Entry, PROTECTED_VARS, env_get};
use crate::mail::{Mailer, Message, expand_envvar, safe_p};
use crate::privs::{Creds, chdir_failure, configure_child};

/// Upper bound on job output retained for mail/log (the rest is drained).
const MAX_OUTPUT: usize = 1024 * 1024;

/// cronie's `logbuf` size: a CMDOUT line is flushed when it fills.
const LOGBUF: usize = 1024;

/// Shared settings for running jobs.
#[derive(Debug, Clone)]
pub struct Runner {
    pub default_path: String,
    pub default_shell: String,
    /// `-P`: inherit `PATH` from the daemon's environment.
    pub inherit_path: bool,
    /// How output is mailed. `Mailer::Off` without `syslog_output` discards
    /// it (`-m off`).
    pub mailer: Mailer,
    /// `-s`, or no sendmail installed: log output lines as CMDOUT.
    pub syslog_output: bool,
    /// Charset for the default mail Content-Type (the daemon locale's codeset).
    pub mail_charset: String,
    pub hostname: String,
}

/// Result of running a job.
#[derive(Debug)]
pub struct Outcome {
    pub status: ExitStatus,
    pub output: Vec<u8>,
}

/// cronie's `mkprints`: control characters as `^X`, DEL as `^?`, bytes above
/// 0x7e as `\\ooo`, everything else unchanged.
pub fn printable(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0..=0x1f => {
                out.push('^');
                out.push((b + b'@') as char);
            }
            0x20..=0x7e => out.push(b as char),
            0x7f => out.push_str("^?"),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out
}

/// Split job output into CMDOUT lines as cronie does: carriage returns are
/// dropped, a line ends at a newline (not included) or when `LOGBUF - 1`
/// bytes have accumulated, and a trailing partial line is logged too.
fn cmdout_lines(output: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut buf: Vec<u8> = Vec::with_capacity(LOGBUF);
    // cronie never stores a newline that is the very first output byte, so
    // it produces no empty CMDOUT line.
    let output = output.strip_prefix(b"\n").unwrap_or(output);
    for &b in output.iter().filter(|&&b| b != b'\r') {
        buf.push(b);
        if b == b'\n' || buf.len() == LOGBUF - 1 {
            if b == b'\n' {
                buf.pop();
            }
            lines.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        lines.push(buf);
    }
    lines
}

/// `getpwnam` for a user name of arbitrary bytes.
fn lookup_user(name: &[u8]) -> nix::Result<Option<User>> {
    let Ok(cname) = CString::new(name) else {
        return Ok(None);
    };
    let mut buflen = 1024usize;
    loop {
        let mut buf = vec![0 as libc::c_char; buflen];
        // SAFETY: an all-zero passwd is a valid value (null pointers, zero ints).
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the given sizes for the call's duration.
        let rc = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr(),
                buflen,
                &mut result,
            )
        };
        if rc == libc::ERANGE && buflen < 1 << 20 {
            buflen *= 4;
            continue;
        }
        if rc != 0 {
            return Err(nix::errno::Errno::from_raw(rc));
        }
        if result.is_null() {
            return Ok(None);
        }
        return Ok(Some(User::from(&pwd)));
    }
}

/// cronie expands `$NAME` and `${NAME}` in MAILTO and MAILFROM from the
/// daemon's own environment, keeping the value as written (with a warning)
/// when the result is too long.
fn expand_mail_var(var: &str, value: &[u8]) -> Vec<u8> {
    let lookup = |name: &[u8]| std::env::var_os(OsStr::from_bytes(name)).map(OsString::into_vec);
    expand_envvar(value, lookup).unwrap_or_else(|| {
        log::warn!(
            "(CRON) WARNING (The environment variable '{var}' could not be expanded. The non-expanded value will be used.)"
        );
        value.to_vec()
    })
}

impl Runner {
    /// The job environment in cronie order: defaults, then crontab
    /// assignments (which may override `SHELL`, `PATH`, `HOME`), with
    /// `LOGNAME`/`USER` fixed to the running user. Values are raw bytes.
    pub fn build_env(&self, pw: &User, user: &[u8], entry: &Entry) -> Vec<(Vec<u8>, Vec<u8>)> {
        let path = if self.inherit_path {
            std::env::var_os("PATH")
                .map(OsString::into_vec)
                .unwrap_or_else(|| self.default_path.clone().into_bytes())
        } else {
            self.default_path.clone().into_bytes()
        };
        let mut env: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"SHELL".to_vec(), self.default_shell.clone().into_bytes()),
            (b"PATH".to_vec(), path),
            (b"HOME".to_vec(), pw.dir.clone().into_os_string().into_vec()),
            (b"LOGNAME".to_vec(), user.to_vec()),
            (b"USER".to_vec(), user.to_vec()),
        ];
        for (k, v) in &entry.env {
            if PROTECTED_VARS.iter().any(|p| p.as_bytes() == k.as_slice()) {
                continue;
            }
            match env.iter_mut().find(|(n, _)| n == k) {
                Some(slot) => slot.1 = v.clone(),
                None => env.push((k.clone(), v.clone())),
            }
        }
        env
    }

    /// Run `entry` as `user`, wait for it, and deliver its output. Every
    /// failure is logged here, once.
    pub fn run(&self, user: &[u8], entry: &Entry) -> io::Result<Outcome> {
        let label = String::from_utf8_lossy(user).into_owned();
        // cronie looks the user up when the job is due and skips it if the
        // user is unknown.
        let pw = match lookup_user(user) {
            Ok(Some(pw)) => pw,
            Ok(None) => {
                log::error!("({label}) ERROR (getpwnam() failed - user unknown)");
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "getpwnam() failed - user unknown",
                ));
            }
            Err(e) => {
                log::error!("({label}) ERROR (getpwnam() failed: {e})");
                return Err(io::Error::other(e));
            }
        };
        let switch = match should_switch(geteuid().as_raw(), pw.uid.as_raw()) {
            Ok(switch) => switch,
            Err(e) => {
                log::error!("({label}) ERROR (cannot run job: {e})");
                return Err(io::Error::other(format!("cannot run job as {label}: {e}")));
            }
        };
        if switch
            && pw.uid.as_raw() != 0
            && let Some(expire) = shadow_expire(user)
            && account_expired(expire, today_days())
        {
            log::error!("({label}) ACCOUNT EXPIRED (job not run)");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("account {label} has expired"),
            ));
        }
        let creds = if switch {
            match Creds::from_user(&pw) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::error!("({label}) ERROR (cannot resolve credentials: {e})");
                    return Err(e);
                }
            }
        } else {
            None
        };
        let env = self.build_env(&pw, user, entry);
        let shell = OsStr::from_bytes(env_get(&env, b"SHELL").unwrap_or(b"/bin/sh")).to_os_string();
        // cronie changes into the job environment's HOME (which a crontab may
        // set) and does not run the job if that fails.
        let home = env_get(&env, b"HOME").unwrap_or(b"").to_vec();
        let command = printable(&entry.command);

        if !entry.dont_log {
            log::info!("({label}) CMD ({command})");
        }
        let mailto = env_get(&env, b"MAILTO").map(|v| expand_mail_var("MAILTO", v));
        let mailfrom = env_get(&entry.env, b"MAILFROM").map(|v| expand_mail_var("MAILFROM", v));

        let pipes = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
            .map_err(io::Error::other)
            // try_clone uses F_DUPFD_CLOEXEC, so the duplicate is close-on-exec too.
            .and_then(|(r, w)| w.try_clone().map(|dup| (r, w, dup)));
        let (read_end, write_end, write_dup) = match pipes {
            Ok(p) => p,
            Err(e) => {
                log::error!("({label}) ERROR (can't create output pipe: {e})");
                return Err(e);
            }
        };

        let mut cmd = Command::new(&shell);
        cmd.arg0(&shell)
            .arg("-c")
            .arg(OsStr::from_bytes(&entry.command))
            .env_clear()
            .envs(
                env.iter()
                    .map(|(k, v)| (OsStr::from_bytes(k), OsStr::from_bytes(v))),
            )
            .stdout(Stdio::from(write_end))
            .stderr(Stdio::from(write_dup))
            .stdin(if entry.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let workdir = PathBuf::from(OsString::from_vec(home.clone()));
        if let Err(e) = configure_child(&mut cmd, creds, true, Some(workdir)) {
            log::error!("({label}) ERROR (can't prepare job: {e})");
            return Err(e);
        }

        let spawned = cmd.spawn();
        // Close our copies of the pipe's write end so EOF arrives when the
        // job (and anything it left running with the fds) exits.
        drop(cmd);
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                if let Some(err) = chdir_failure(&e) {
                    log::error!(
                        "(CRON) ERROR chdir failed ({}): {err}",
                        String::from_utf8_lossy(&home)
                    );
                    return Err(err);
                }
                log::error!(
                    "({label}) ERROR (can't execute {}: {e})",
                    shell.to_string_lossy()
                );
                return Err(e);
            }
        };

        let stdin_thread = match (child.stdin.take(), entry.stdin.clone()) {
            (Some(mut pipe), Some(data)) => Some(std::thread::spawn(move || -> io::Result<()> {
                pipe.write_all(&data)
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
                    log::error!("({label}) ERROR (reading job output: {e})");
                    break;
                }
            }
        }
        drop(reader);
        if discarded > 0 {
            log::warn!("({label}) ERROR (job output truncated: {discarded} bytes discarded)");
            output.extend_from_slice(
                format!("\n[crond: {discarded} bytes of output discarded]\n").as_bytes(),
            );
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(e) => {
                log::error!("({label}) ERROR (waiting for job: {e})");
                return Err(e);
            }
        };
        if let Some(t) = stdin_thread {
            match t.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => {}
                Ok(Err(e)) => log::error!("({label}) ERROR (writing job stdin: {e})"),
                Err(_) => log::error!("({label}) ERROR (job stdin writer panicked)"),
            }
        }

        self.deliver(
            &pw,
            user,
            &label,
            entry,
            &env,
            mailto.as_deref(),
            mailfrom.as_deref(),
            status,
            &output,
            switch,
        );

        if !entry.dont_log {
            if !status.success() {
                log::debug!("({label}) CMDSTATUS ({command}) {status}");
            }
            log::info!("({label}) CMDEND ({command})");
        }
        Ok(Outcome { status, output })
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &self,
        pw: &User,
        user: &[u8],
        label: &str,
        entry: &Entry,
        env: &[(Vec<u8>, Vec<u8>)],
        mailto: Option<&[u8]>,
        mailfrom: Option<&[u8]>,
        status: ExitStatus,
        output: &[u8],
        switch: bool,
    ) {
        if output.is_empty() {
            return;
        }
        // cronie's child_process, in its order: the sender is MAILFROM from
        // the crontab when set and safe, otherwise the account name.
        let mailfrom = match mailfrom {
            Some(v) if !v.is_empty() && self.safe_logged(label, v) => v.to_vec(),
            _ => pw.name.clone().into_bytes(),
        };
        // MAILTO present but empty means no mail; absent means the user.
        let mailto = match mailto {
            Some([]) => None,
            Some(v) => Some(v.to_vec()),
            None => Some(user.to_vec()),
        };
        let mailing = mailto
            .as_deref()
            .is_some_and(|m| self.safe_logged(label, m))
            && !self.mailer.is_off()
            && !self.syslog_output;

        if self.syslog_output {
            for line in cmdout_lines(output) {
                log::info!("({label}) CMDOUT ({})", String::from_utf8_lossy(&line));
            }
        }
        if !mailing || (entry.mail_on_failure_only && status.success()) {
            return;
        }
        let msg = Message {
            mailfrom,
            mailto: mailto.unwrap_or_default(),
            user: user.to_vec(),
            host: self
                .hostname
                .split('.')
                .next()
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
            command: entry.command.clone(),
            charset: self.mail_charset.clone().into_bytes(),
            content_type: env_get(env, b"CONTENT_TYPE").map(<[u8]>::to_vec),
            content_transfer_encoding: env_get(env, b"CONTENT_TRANSFER_ENCODING")
                .map(<[u8]>::to_vec),
            env: env.to_vec(),
            body: output.to_vec(),
        };
        let creds = if switch {
            match Creds::from_user(pw) {
                Ok(c) => Some(c),
                Err(e) => {
                    log::error!("({label}) MAIL ERROR (cannot resolve credentials: {e})");
                    return;
                }
            }
        } else {
            None
        };
        let bytes = output.iter().filter(|&&b| b != b'\r').count();
        match self.mailer.send(&msg, creds) {
            Ok(mail_status) if mail_status.success() => {}
            Ok(mail_status) => log::info!(
                "({label}) MAIL (mailed {bytes} byte{} of output but got status 0x{:04x})",
                if bytes == 1 { "" } else { "s" },
                mail_status.into_raw()
            ),
            Err(e) => log::error!("({label}) MAIL ERROR ({e})"),
        }
    }

    /// cronie's `safe_p`, which logs `UNSAFE` for a rejected value.
    fn safe_logged(&self, label: &str, value: &[u8]) -> bool {
        let safe = safe_p(value);
        if !safe {
            log::info!("({label}) UNSAFE ({})", String::from_utf8_lossy(value));
        }
        safe
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
fn shadow_expire(user: &[u8]) -> Option<i64> {
    let name = CString::new(user).ok()?;
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
    use crate::crontab::{Crontab, Format, ParseOptions};

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
    fn printable_matches_mkprints() {
        assert_eq!(printable(b"echo hi"), "echo hi");
        assert_eq!(printable(b"a\tb\x1b"), "a^Ib^[");
        assert_eq!(printable(b"\x7f"), "^?");
        assert_eq!(printable(&[b'c', b'a', b'f', 0xc3, 0xa9]), "caf\\303\\251");
    }

    #[test]
    fn cmdout_lines_match_cronie() {
        assert_eq!(
            cmdout_lines(b"one\r\ntwo\nthree"),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        let long = vec![b'x'; LOGBUF - 1];
        let mut out = long.clone();
        out.push(b'\n');
        assert_eq!(cmdout_lines(&out), vec![long, Vec::new()]);
    }

    #[test]
    fn non_root_cannot_run_as_root() {
        if geteuid().is_root() {
            return;
        }
        let err = runner(Mailer::Off)
            .run(b"root", &entry("* * * * * true\n"))
            .unwrap_err();
        assert!(
            err.to_string().contains("daemon is not running as root"),
            "{err}"
        );
    }

    #[test]
    fn unknown_user_is_rejected() {
        let err = runner(Mailer::Off)
            .run(b"no-such-user-xyz123", &entry("* * * * * true\n"))
            .unwrap_err();
        assert!(
            err.to_string().contains("getpwnam() failed - user unknown"),
            "{err}"
        );
    }

    #[test]
    fn oversized_output_is_truncated_with_notice() {
        let e = entry("* * * * * head -c 1100000 /dev/zero\n");
        let out = runner(Mailer::Off).run(me().name.as_bytes(), &e).unwrap();
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
            syslog_output: false,
            mail_charset: "UTF-8".into(),
            hostname: "testhost.example".into(),
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
        let out = r.run(me().name.as_bytes(), &e).unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.output).unwrap();
        let home = me().dir.canonicalize().unwrap();
        assert_eq!(
            text,
            format!("bar {} /bin/sh\n{}\nerr\n", me().name, home.display())
        );
    }

    #[test]
    fn crontab_home_is_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let e = entry(&format!("HOME={}\n* * * * * pwd\n", home.display()));
        let out = runner(Mailer::Off).run(me().name.as_bytes(), &e).unwrap();
        assert_eq!(
            String::from_utf8(out.output).unwrap(),
            format!("{}\n", home.display())
        );

        let e = entry("HOME=/nonexistent-crontab-home\n* * * * * true\n");
        let err = runner(Mailer::Off)
            .run(me().name.as_bytes(), &e)
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT), "{err}");
    }

    #[test]
    fn crontab_bytes_reach_the_job_unchanged() {
        let text = b"FOO=caf\xe9\n* * * * * echo \"$FOO\"\n";
        let e = Crontab::parse_bytes(text, &ParseOptions::new(Format::User))
            .crontab
            .entries
            .remove(0);
        let out = runner(Mailer::Off).run(me().name.as_bytes(), &e).unwrap();
        assert_eq!(out.output, b"caf\xe9\n");
    }

    #[test]
    fn stdin_from_percent() {
        let e = entry("* * * * * tr a-z A-Z%hello%world\n");
        let out = runner(Mailer::Off).run(me().name.as_bytes(), &e).unwrap();
        assert_eq!(out.output, b"HELLO\nWORLD\n");
    }

    #[test]
    fn mail_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mbox = dir.path().join("mbox");
        let r = runner(Mailer::Command(format!("cat >> '{}'", mbox.display())));
        let name = me().name;

        r.run(
            name.as_bytes(),
            &entry("MAILTO=someone\n* * * * * echo one\n"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&mbox).unwrap();
        assert!(
            text.contains(&format!("From: \"(Cron Daemon)\" <{name}>\n")),
            "{text}"
        );
        assert!(text.contains("To: someone\n"));
        assert!(
            text.contains(&format!("Subject: Cron <{name}@testhost> echo one\n")),
            "{text}"
        );
        assert!(text.contains("Content-Type: text/plain; charset=UTF-8\n"));
        assert!(text.ends_with("\n\none\n"));

        // MAILTO="" suppresses, -n suppresses on success, silent jobs send nothing,
        // and an unsafe MAILTO sends nothing.
        std::fs::remove_file(&mbox).unwrap();
        r.run(name.as_bytes(), &entry("MAILTO=\"\"\n* * * * * echo two\n"))
            .unwrap();
        r.run(name.as_bytes(), &entry("* * * * * -n echo three\n"))
            .unwrap();
        r.run(name.as_bytes(), &entry("* * * * * true\n")).unwrap();
        r.run(
            name.as_bytes(),
            &entry("MAILTO=\"a b\"\n* * * * * echo unsafe\n"),
        )
        .unwrap();
        assert!(!mbox.exists());

        let out = r
            .run(name.as_bytes(), &entry("* * * * * -n echo four; exit 3\n"))
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert!(std::fs::read_to_string(&mbox).unwrap().contains("four"));

        // An unsafe MAILFROM falls back to the account name.
        std::fs::remove_file(&mbox).unwrap();
        r.run(
            name.as_bytes(),
            &entry("MAILFROM=\"Backup Server\"\n* * * * * echo five\n"),
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&mbox)
                .unwrap()
                .contains(&format!("From: \"(Cron Daemon)\" <{name}>\n"))
        );
    }

    #[test]
    fn mail_addresses_expand_daemon_variables() {
        let dir = tempfile::tempdir().unwrap();
        let mbox = dir.path().join("mbox");
        let r = runner(Mailer::Command(format!("cat >> '{}'", mbox.display())));
        let name = me().name;
        r.run(
            name.as_bytes(),
            &entry("MAILTO=$NO_SUCH_VAR_XYZ\n* * * * * echo unset\n"),
        )
        .unwrap();
        assert!(
            !mbox.exists(),
            "an address expanding to nothing sends no mail"
        );
        r.run(
            name.as_bytes(),
            &entry("MAILTO=some${NO_SUCH_VAR_XYZ}one\n* * * * * echo six\n"),
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&mbox)
                .unwrap()
                .contains("To: someone\n")
        );
    }

    #[test]
    fn background_children_do_not_hang_on_closed_output() {
        let e = entry("* * * * * echo quick\n");
        let out = runner(Mailer::Off).run(me().name.as_bytes(), &e).unwrap();
        assert_eq!(out.output, b"quick\n");
    }
}
