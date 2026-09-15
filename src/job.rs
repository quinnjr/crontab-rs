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
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

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
        let switch = geteuid().is_root();
        if !switch && pw.uid != geteuid() {
            return Err(io::Error::other(format!(
                "cannot run job as {user}: daemon is not running as root"
            )));
        }
        let env = self.build_env(&pw, entry);
        let shell = env_get(&env, "SHELL").unwrap_or("/bin/sh").to_string();

        if !entry.quiet {
            log::info!("({user}) CMD ({})", entry.raw_command);
        }

        let (read_end, write_end) = nix::unistd::pipe().map_err(io::Error::other)?;
        let write_dup = write_end.try_clone()?;

        let mut cmd = Command::new(&shell);
        cmd.arg0(&shell)
            .arg("-c")
            .arg(&entry.command)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&pw.dir)
            .stdout(Stdio::from(write_end))
            .stderr(Stdio::from(write_dup))
            .stdin(if entry.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        configure_child(&mut cmd, switch.then(|| Creds::from_user(&pw)), true);

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
            (Some(mut pipe), Some(data)) => Some(std::thread::spawn(move || {
                let _ = pipe.write_all(data.as_bytes());
            })),
            _ => None,
        };

        let mut output = Vec::new();
        let mut reader = File::from(read_end);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(output.len());
                    output.extend_from_slice(&buf[..n.min(room)]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let status = child.wait()?;
        if let Some(t) = stdin_thread {
            let _ = t.join();
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
        let creds = switch.then(|| Creds::from_user(pw));
        match self.mailer.send(&msg, creds) {
            Ok(()) => log::debug!("({user}) MAIL (mailed {} bytes to {mailto})", output.len()),
            Err(e) => log::error!("({user}) MAIL ERROR ({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crontab::{Crontab, Format};

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
