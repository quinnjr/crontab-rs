//! Delivery of job output by mail.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::privs::{Creds, configure_child};

/// How long a mailer may run before it is killed.
const MAILER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How much mailer stderr is kept for error reports.
const MAX_STDERR: usize = 1024;

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
            .stderr(Stdio::piped());
        configure_child(&mut cmd, creds, false, None)?;
        let mut child = cmd.spawn()?;

        // Feed stdin and drain stderr on helper threads so neither a mailer
        // that stops reading nor one that floods stderr can block us past
        // the deadline.
        let (wtx, wrx) = mpsc::channel();
        if let Some(mut stdin) = child.stdin.take() {
            let data = msg.render();
            std::thread::spawn(move || {
                let r = stdin.write_all(&data);
                drop(stdin);
                let _ = wtx.send(r);
            });
        } else {
            let _ = wtx.send(Ok(()));
        }
        let (etx, erx) = mpsc::channel();
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut kept = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let room = MAX_STDERR.saturating_sub(kept.len());
                            kept.extend_from_slice(&buf[..n.min(room)]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = etx.send(kept);
            });
        } else {
            let _ = etx.send(Vec::new());
        }

        let deadline = Instant::now() + MAILER_TIMEOUT;
        let mut delay = Duration::from_millis(5);
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("mailer timed out after {}s", MAILER_TIMEOUT.as_secs()),
                ));
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(200));
        };

        // Descendants may still hold the pipes; don't wait on them forever.
        let write_result = wrx.recv_timeout(Duration::from_secs(1)).unwrap_or(Ok(()));
        let stderr = erx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim();

        if !status.success() {
            return Err(io::Error::other(if stderr.is_empty() {
                format!("mailer exited with {status}")
            } else {
                format!("mailer exited with {status}: {stderr}")
            }));
        }
        write_result.map_err(|e| io::Error::new(e.kind(), format!("writing to mailer: {e}")))
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

    fn sample() -> Message {
        Message {
            from: "x".into(),
            envelope_from: None,
            to: "y".into(),
            subject: "s".into(),
            content_type: "text/plain".into(),
            env: vec![],
            body: b"body\n".to_vec(),
        }
    }

    #[test]
    fn failing_mailer_reports_status() {
        let err = Mailer::Command("false".into())
            .send(&sample(), None)
            .unwrap_err();
        assert!(err.to_string().contains("mailer exited with"), "{err}");
    }

    #[test]
    fn mailer_exiting_without_reading_is_reaped() {
        let m = Message {
            body: vec![b'a'; 1 << 20],
            ..sample()
        };
        let err = Mailer::Command("echo oops >&2; exit 3".into())
            .send(&m, None)
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("mailer exited with"), "{text}");
        assert!(text.contains("oops"), "{text}");
    }

    #[test]
    fn mailto_safety() {
        assert!(mailto_is_safe("alice@example.com, bob"));
        assert!(!mailto_is_safe("alice\nBcc: eve"));
    }
}
