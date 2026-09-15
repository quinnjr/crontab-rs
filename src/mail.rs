//! Delivery of job output by mail.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
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

/// cronie's `MAILARG`: the only sendmail it looks for.
const SENDMAIL: &str = "/usr/sbin/sendmail";

impl Mailer {
    /// cronie mails through `/usr/sbin/sendmail` when it is executable;
    /// otherwise [`Mailer::Off`] (and cronie logs output to syslog instead).
    pub fn detect() -> Mailer {
        let path = Path::new(SENDMAIL);
        if nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok() {
            Mailer::Sendmail(path.to_path_buf())
        } else {
            Mailer::Off
        }
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
    /// Returns the mailer's exit status (cronie logs a non-zero status);
    /// errors are for mailers that could not be run, timed out, or stopped
    /// reading while exiting successfully.
    pub fn send(&self, msg: &Message, creds: Option<Creds>) -> io::Result<ExitStatus> {
        let mut cmd = match self {
            Mailer::Off => return Ok(std::os::unix::process::ExitStatusExt::from_raw(0)),
            Mailer::Sendmail(path) => {
                // cronie's MAILFMT: "%s -FCronDaemon -i -odi -oem -oi -t -f %s".
                let mut c = Command::new(path);
                c.args(["-FCronDaemon", "-i", "-odi", "-oem", "-oi", "-t", "-f"])
                    .arg(OsStr::from_bytes(&msg.mailfrom));
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
        if status.success()
            && let Err(e) = write_result
        {
            let stderr = String::from_utf8_lossy(&stderr);
            return Err(io::Error::new(
                e.kind(),
                format!("writing to mailer: {e} {}", stderr.trim()),
            ));
        }
        Ok(status)
    }
}

/// A job-output message in cronie's layout. Every field is raw bytes, as
/// cronie writes them.
#[derive(Debug, Clone)]
pub struct Message {
    /// Sender: MAILFROM when set and safe, otherwise the account name.
    pub mailfrom: Vec<u8>,
    pub mailto: Vec<u8>,
    /// The crontab user, for the Subject.
    pub user: Vec<u8>,
    /// Hostname up to its first dot, for the Subject.
    pub host: Vec<u8>,
    /// The job's command (before any `%` input), for the Subject.
    pub command: Vec<u8>,
    /// Charset for the default Content-Type.
    pub charset: Vec<u8>,
    pub content_type: Option<Vec<u8>>,
    pub content_transfer_encoding: Option<Vec<u8>>,
    /// The job environment, one `X-Cron-Env` header per variable.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// Job output. Carriage returns are dropped when rendering, as in cronie.
    pub body: Vec<u8>,
}

impl Message {
    /// The message exactly as cronie's `child_process` writes it to the
    /// mailer.
    pub fn render(&self) -> Vec<u8> {
        fn line(out: &mut Vec<u8>, parts: &[&[u8]]) {
            for part in parts {
                out.extend_from_slice(part);
            }
            out.push(b'\n');
        }
        let mut out = Vec::with_capacity(self.body.len() + 1024);
        line(
            &mut out,
            &[b"From: \"(Cron Daemon)\" <", &self.mailfrom, b">"],
        );
        line(&mut out, &[b"To: ", &self.mailto]);
        line(
            &mut out,
            &[
                b"Subject: Cron <",
                &self.user,
                b"@",
                &self.host,
                b"> ",
                &self.command,
            ],
        );
        line(&mut out, &[b"MIME-Version: 1.0"]);
        match &self.content_type {
            None => line(
                &mut out,
                &[b"Content-Type: text/plain; charset=", &self.charset],
            ),
            Some(v) => line(&mut out, &[b"Content-Type: ", &newlines_to_spaces(v)]),
        }
        match &self.content_transfer_encoding {
            None => line(&mut out, &[b"Content-Transfer-Encoding: 8bit"]),
            Some(v) => line(
                &mut out,
                &[b"Content-Transfer-Encoding: ", &newlines_to_spaces(v)],
            ),
        }
        line(&mut out, &[b"Auto-Submitted: auto-generated"]);
        line(&mut out, &[b"Precedence: bulk"]);
        for (k, v) in &self.env {
            line(&mut out, &[b"X-Cron-Env: <", k, b"=", v, b">"]);
        }
        out.push(b'\n');
        out.extend(self.body.iter().copied().filter(|&b| b != b'\r'));
        out
    }
}

/// cronie replaces newlines in user-supplied Content-Type and
/// Content-Transfer-Encoding values so they cannot add headers.
fn newlines_to_spaces(v: &[u8]) -> Vec<u8> {
    v.iter()
        .map(|&b| if b == b'\n' { b' ' } else { b })
        .collect()
}

/// Size of cronie's `mailto_expanded`/`mailfrom_expanded` buffers
/// (`MAX_EMAILSTR`, including the terminating NUL).
const MAX_EMAILSTR: usize = 255;

/// cronie's `find_envvar`: the offset and length of the first `$NAME` or
/// `${NAME}` in `source`, with cronie's quirks (a digit in the first two
/// positions ends the search, and a lone `$` counts as an empty name).
fn find_envvar(source: &[u8]) -> Option<(usize, usize)> {
    let start = source.iter().position(|&b| b == b'$')?;
    let mut size = 1;
    let mut waiting_close = false;
    for &c in &source[start + 1..] {
        if c == b'_' || c.is_ascii_alphanumeric() {
            if size <= 2 && c.is_ascii_digit() {
                return None;
            }
            size += 1;
        } else if c == b'{' {
            if size != 1 {
                return None;
            }
            size += 1;
            waiting_close = true;
        } else if c == b'}' {
            if (waiting_close && size == 2) || size == 1 {
                return None;
            }
            if waiting_close {
                size += 1;
            }
            waiting_close = false;
            break;
        } else {
            break;
        }
    }
    (!waiting_close).then_some((start, size))
}

/// cronie's `expand_envvar` for MAILTO and MAILFROM. Each `$NAME` or
/// `${NAME}` becomes `lookup(NAME)`, or nothing when that is unset. Scanning
/// stops at the first `$` that doesn't start a name cronie accepts, and the
/// rest is copied unchanged. Returns `None` when the result would not fit
/// cronie's buffer, in which case cronie keeps the value as written.
pub fn expand_envvar(source: &[u8], lookup: impl Fn(&[u8]) -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    let mut rest = source;
    while let Some((start, len)) = find_envvar(rest) {
        if result.len() + start + 1 > MAX_EMAILSTR {
            return None;
        }
        result.extend_from_slice(&rest[..start]);
        let mut name = &rest[start + 1..start + len];
        if name.first() == Some(&b'{') {
            name = &name[1..name.len() - 1];
        }
        rest = &rest[start + len..];
        if !name.is_empty()
            && let Some(value) = lookup(name)
        {
            if result.len() + value.len() + 1 > MAX_EMAILSTR {
                return None;
            }
            result.extend_from_slice(&value);
        }
    }
    if result.len() + rest.len() + 1 > MAX_EMAILSTR {
        return None;
    }
    result.extend_from_slice(rest);
    Some(result)
}

/// cronie's `safe_p` for MAILTO and MAILFROM: printable ASCII letters and
/// digits, plus `@!:%-.,_+` after the first character.
pub fn safe_p(s: &[u8]) -> bool {
    const SAFE_DELIM: &[u8] = b"@!:%-.,_+";
    s.iter()
        .enumerate()
        .all(|(i, &c)| c.is_ascii_alphanumeric() || (i > 0 && SAFE_DELIM.contains(&c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        Message {
            mailfrom: b"alice".to_vec(),
            mailto: b"alice".to_vec(),
            user: b"alice".to_vec(),
            host: b"box".to_vec(),
            command: b"echo hi".to_vec(),
            charset: b"UTF-8".to_vec(),
            content_type: None,
            content_transfer_encoding: None,
            env: vec![(b"SHELL".to_vec(), b"/bin/sh".to_vec())],
            body: b"hi\r\n".to_vec(),
        }
    }

    #[test]
    fn render_matches_cronie() {
        assert_eq!(
            sample().render(),
            b"From: \"(Cron Daemon)\" <alice>\nTo: alice\nSubject: Cron <alice@box> echo hi\n\
MIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\n\
Auto-Submitted: auto-generated\nPrecedence: bulk\nX-Cron-Env: <SHELL=/bin/sh>\n\nhi\n"
                .to_vec()
        );
        let custom = Message {
            content_type: Some(b"text/html\nBcc: eve".to_vec()),
            content_transfer_encoding: Some(b"base64".to_vec()),
            body: vec![0xe9, b'\n'],
            ..sample()
        };
        let text = custom.render();
        assert!(
            text.windows(29)
                .any(|w| w == b"Content-Type: text/html Bcc: ")
        );
        assert!(text.ends_with(&[b'\n', b'\n', 0xe9, b'\n']));
    }

    #[test]
    fn command_mailer_receives_message() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("mail.txt");
        let mailer = Mailer::Command(format!("cat > '{}'", out.display()));
        let status = mailer.send(&sample(), None).unwrap();
        assert!(status.success());
        assert!(std::fs::read(out).unwrap().ends_with(b"\n\nhi\n"));
        assert_eq!(Mailer::from_arg("OFF"), Mailer::Off);
        assert!(Mailer::Off.send(&sample(), None).unwrap().success());
    }

    #[test]
    fn failing_mailer_status_is_returned() {
        let status = Mailer::Command("false".into())
            .send(&sample(), None)
            .unwrap();
        assert_eq!(status.code(), Some(1));
    }

    #[test]
    fn mailer_exiting_without_reading_is_reaped() {
        let m = Message {
            body: vec![b'a'; 1 << 20],
            ..sample()
        };
        let status = Mailer::Command("exit 3".into()).send(&m, None).unwrap();
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn expand_envvar_matches_cronie() {
        let lookup = |name: &[u8]| match name {
            b"USER" => Some(b"alice".to_vec()),
            b"DOM" => Some(b"example.com".to_vec()),
            b"A1" => Some(b"x".to_vec()),
            b"LONG" => Some(vec![b'y'; 255]),
            _ => None,
        };
        let ex = |s: &[u8]| expand_envvar(s, lookup);
        assert_eq!(ex(b"$USER@$DOM").unwrap(), b"alice@example.com");
        assert_eq!(
            ex(b"${USER}+cron@${DOM}").unwrap(),
            b"alice+cron@example.com"
        );
        assert_eq!(ex(b"$UNSET").unwrap(), b"");
        assert_eq!(ex(b"a$ b").unwrap(), b"a b");
        // A digit in the first two name positions stops expansion entirely.
        assert_eq!(ex(b"$A1 $USER").unwrap(), b"$A1 $USER");
        assert_eq!(ex(b"$9").unwrap(), b"$9");
        assert_eq!(ex(b"${A1}").unwrap(), b"x");
        assert_eq!(ex(b"${USER").unwrap(), b"${USER");
        assert_eq!(ex(b"${}").unwrap(), b"${}");
        assert_eq!(ex(b"$USER}").unwrap(), b"alice}");
        assert_eq!(ex(&[b'a'; 254]).unwrap(), vec![b'a'; 254]);
        assert_eq!(ex(&[b'a'; 255]), None);
        assert_eq!(ex(b"$LONG"), None);
    }

    #[test]
    fn safe_p_matches_cronie() {
        assert!(safe_p(b"alice@example.com"));
        assert!(safe_p(b"a-b.c_d+e:f!g%h,i"));
        assert!(!safe_p(b"-alice"));
        assert!(!safe_p(b"alice bob"));
        assert!(!safe_p(b"alice\nBcc: eve"));
        assert!(!safe_p(b"eve;rm"));
        assert!(!safe_p(&[b'j', 0xf6, b'r', b'g']));
    }
}
