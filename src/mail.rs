//! Delivery of job output by mail.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::privs::{Creds, configure_child};

/// How job output is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mailer {
    /// Invoke a sendmail-compatible binary with `-t`.
    Sendmail(PathBuf),
    /// Invoke an arbitrary shell command that reads the message on stdin.
    Command(String),
    /// Discard output.
    Off,
}

const SENDMAIL_CANDIDATES: &[&str] = &[
    "/usr/sbin/sendmail",
    "/usr/lib/sendmail",
    "/usr/bin/sendmail",
    "/sbin/sendmail",
];

impl Mailer {
    /// Locate a sendmail binary; [`Mailer::Off`] when there is none.
    pub fn detect() -> Mailer {
        SENDMAIL_CANDIDATES
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .map(|p| Mailer::Sendmail(p.to_path_buf()))
            .unwrap_or(Mailer::Off)
    }

    /// Parse a `-m` argument: `off` disables mail, anything else is a shell
    /// command.
    pub fn from_arg(arg: &str) -> Mailer {
        if arg.eq_ignore_ascii_case("off") {
            Mailer::Off
        } else {
            Mailer::Command(arg.to_string())
        }
    }

    pub fn is_off(&self) -> bool {
        matches!(self, Mailer::Off)
    }

    /// Deliver a message, running the mailer with `creds` when given.
    pub fn send(&self, msg: &Message, creds: Option<Creds>) -> std::io::Result<()> {
        let mut cmd = match self {
            Mailer::Off => return Ok(()),
            Mailer::Sendmail(path) => {
                let mut c = Command::new(path);
                c.args(["-FCronDaemon", "-i", "-odi", "-oem", "-oi", "-t"]);
                if let Some(from) = &msg.envelope_from {
                    c.arg("-f").arg(from);
                }
                c
            }
            Mailer::Command(shell) => {
                let mut c = Command::new("/bin/sh");
                c.arg("-c").arg(shell);
                c
            }
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_child(&mut cmd, creds, false);
        let mut child = cmd.spawn()?;
        {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(&msg.render())?;
        }
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "mailer exited with {status}"
            )))
        }
    }
}

/// A job-output message.
#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub envelope_from: Option<String>,
    pub to: String,
    pub subject: String,
    pub content_type: String,
    pub env: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    /// RFC 822 text as sendmail `-t` expects it.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 512);
        let _ = writeln!(out, "From: {}", self.from);
        let _ = writeln!(out, "To: {}", self.to);
        let _ = writeln!(out, "Subject: {}", self.subject);
        let _ = writeln!(out, "Content-Type: {}", self.content_type);
        let _ = writeln!(out, "Auto-Submitted: auto-generated");
        let _ = writeln!(out, "Precedence: bulk");
        let _ = writeln!(out, "MIME-Version: 1.0");
        for (k, v) in &self.env {
            let _ = writeln!(out, "X-Cron-Env: <{k}={v}>");
        }
        out.push(b'\n');
        out.extend_from_slice(&self.body);
        if !self.body.ends_with(b"\n") {
            out.push(b'\n');
        }
        out
    }
}

/// Is this a safe `MAILTO` value (no control characters that could inject
/// headers)?
pub fn mailto_is_safe(value: &str) -> bool {
    !value.chars().any(|c| c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_message() {
        let m = Message {
            from: "root (Cron Daemon)".into(),
            envelope_from: None,
            to: "alice".into(),
            subject: "Cron <alice@host> echo hi".into(),
            content_type: "text/plain; charset=UTF-8".into(),
            env: vec![("SHELL".into(), "/bin/sh".into())],
            body: b"hi".to_vec(),
        };
        let text = String::from_utf8(m.render()).unwrap();
        assert!(text.starts_with(
            "From: root (Cron Daemon)\nTo: alice\nSubject: Cron <alice@host> echo hi\n"
        ));
        assert!(text.contains("X-Cron-Env: <SHELL=/bin/sh>\n\nhi\n"));
    }

    #[test]
    fn command_mailer_receives_message() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("mail.txt");
        let mailer = Mailer::Command(format!("cat > '{}'", out.display()));
        let m = Message {
            from: "x".into(),
            envelope_from: None,
            to: "y".into(),
            subject: "s".into(),
            content_type: "text/plain".into(),
            env: vec![],
            body: b"body\n".to_vec(),
        };
        mailer.send(&m, None).unwrap();
        let text = std::fs::read_to_string(out).unwrap();
        assert!(text.ends_with("\n\nbody\n"));
        assert_eq!(Mailer::from_arg("OFF"), Mailer::Off);
        assert!(Mailer::Off.send(&m, None).is_ok());
    }

    #[test]
    fn mailto_safety() {
        assert!(mailto_is_safe("alice@example.com, bob"));
        assert!(!mailto_is_safe("alice\nBcc: eve"));
    }
}
