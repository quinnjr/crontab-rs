//! Crontab file parsing (both user crontabs and system crontabs that carry a
//! user field), with Vixie/cronie semantics for environment lines, `%`
//! stdin splitting and the `-n`/`-q` command prefixes.

use std::fmt;
use std::str::FromStr;

use chrono_tz::Tz;

use crate::schedule::{Schedule, ScheduleError};

/// Variables that a crontab may not override.
pub const PROTECTED_VARS: &[&str] = &["LOGNAME", "USER"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    Schedule(ScheduleError),
    BadUsername,
    BadCommand,
    BadTimezone(String),
    BadRandomDelay(String),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::Schedule(e) => e.fmt(f),
            EntryError::BadUsername => f.write_str("bad username"),
            EntryError::BadCommand => f.write_str("bad command"),
            EntryError::BadTimezone(tz) => write!(f, "bad CRON_TZ value \"{tz}\""),
            EntryError::BadRandomDelay(v) => write!(f, "bad RANDOM_DELAY value \"{v}\""),
        }
    }
}

impl std::error::Error for EntryError {}

impl From<ScheduleError> for EntryError {
    fn from(e: ScheduleError) -> Self {
        EntryError::Schedule(e)
    }
}

/// A parse error with the 1-based line number it occurred on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub error: EntryError,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.error)
    }
}

impl std::error::Error for ParseError {}

/// One job line from a crontab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub schedule: Schedule,
    /// User to run as; `None` for user crontabs (the file owner runs it).
    pub user: Option<String>,
    /// The shell command, with `\%` unescaped and any `%` stdin part removed.
    pub command: String,
    /// Data fed to the command's standard input (from `%` sections).
    pub stdin: Option<String>,
    /// The full command text as written, for logging.
    pub raw_command: String,
    /// Environment in effect for this entry, in declaration order.
    pub env: Vec<(String, String)>,
    /// `CRON_TZ` in effect for this entry, if any.
    pub tz: Option<Tz>,
    /// `-n` prefix: only mail output when the job fails.
    pub mail_on_failure_only: bool,
    /// `-q` prefix: do not log the job start.
    pub quiet: bool,
    /// 1-based source line number.
    pub line: usize,
}

/// A parsed crontab file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crontab {
    pub entries: Vec<Entry>,
    /// `RANDOM_DELAY` in minutes (upper bound) declared in the file, if any.
    pub random_delay: Option<u32>,
}

/// Which crontab format a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `min hour dom mon dow command` — the owner runs the command.
    User,
    /// `min hour dom mon dow user command` (`/etc/crontab`, `/etc/cron.d`).
    System,
}

impl Crontab {
    /// Parse crontab text.  Returns every error found, one per bad line,
    /// so that `crontab -T` can report them all.
    pub fn parse(text: &str, format: Format) -> Result<Crontab, Vec<ParseError>> {
        let mut env: Vec<(String, String)> = Vec::new();
        let mut tz: Option<Tz> = None;
        let mut random_delay = None;
        let mut entries = Vec::new();
        let mut errors = Vec::new();

        for (idx, raw) in text.lines().enumerate() {
            let lineno = idx + 1;
            let line = raw.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((name, value)) = parse_env_line(line) {
                if name == "CRON_TZ" {
                    if value.is_empty() {
                        tz = None;
                    } else {
                        match Tz::from_str(&value) {
                            Ok(z) => tz = Some(z),
                            Err(_) => {
                                errors.push(ParseError {
                                    line: lineno,
                                    error: EntryError::BadTimezone(value.clone()),
                                });
                                continue;
                            }
                        }
                    }
                }
                if name == "RANDOM_DELAY" {
                    match value.parse::<u32>() {
                        Ok(v) => random_delay = Some(v),
                        Err(_) => {
                            errors.push(ParseError {
                                line: lineno,
                                error: EntryError::BadRandomDelay(value.clone()),
                            });
                            continue;
                        }
                    }
                }
                if PROTECTED_VARS.contains(&name.as_str()) {
                    // Silently ignored, as in cronie.
                    continue;
                }
                set_env(&mut env, name, value);
                continue;
            }
            match parse_entry(line, format, &env, tz, lineno) {
                Ok(entry) => entries.push(entry),
                Err(error) => errors.push(ParseError {
                    line: lineno,
                    error,
                }),
            }
        }

        if errors.is_empty() {
            Ok(Crontab {
                entries,
                random_delay,
            })
        } else {
            Err(errors)
        }
    }
}

fn set_env(env: &mut Vec<(String, String)>, name: String, value: String) {
    if let Some(slot) = env.iter_mut().find(|(n, _)| *n == name) {
        slot.1 = value;
    } else {
        env.push((name, value));
    }
}

/// Look up a variable in an entry's environment list.
pub fn env_get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// Try to interpret a line as `NAME = value`.
///
/// Returns `None` when the line is not an environment assignment (and so
/// should be parsed as a job).  Quotes around the value (matching single or
/// double) are stripped; a quoted value may only be followed by whitespace.
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim_start();
    let name_end = line.find(|c: char| c == '=' || c.is_whitespace())?;
    let name = &line[..name_end];
    if name.is_empty() || !is_valid_var_name(name) {
        return None;
    }
    let rest = line[name_end..].trim_start();
    let rest = rest.strip_prefix('=')?;
    let rest = rest.trim_start();
    let value = if let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let inner = &rest[1..];
        let close = inner.find(q)?;
        let after = &inner[close + 1..];
        if !after.trim().is_empty() {
            return None;
        }
        inner[..close].to_string()
    } else {
        rest.trim_end().to_string()
    };
    Some((name.to_string(), value))
}

fn is_valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_entry(
    line: &str,
    format: Format,
    env: &[(String, String)],
    tz: Option<Tz>,
    lineno: usize,
) -> Result<Entry, EntryError> {
    let (schedule, rest) = Schedule::parse_prefix(line)?;
    let (user, rest) = match format {
        Format::User => (None, rest),
        Format::System => {
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let (user, remainder) = rest.split_at(end);
            if user.is_empty() || !is_valid_username(user) {
                return Err(EntryError::BadUsername);
            }
            (Some(user.to_string()), remainder.trim_start())
        }
    };

    let mut rest = rest;
    let mut mail_on_failure_only = false;
    let mut quiet = false;
    loop {
        if let Some(r) = strip_flag(rest, "-n") {
            mail_on_failure_only = true;
            rest = r;
        } else if let Some(r) = strip_flag(rest, "-q") {
            quiet = true;
            rest = r;
        } else {
            break;
        }
    }

    let raw_command = rest.trim_end().to_string();
    if raw_command.is_empty() {
        return Err(EntryError::BadCommand);
    }
    let (command, stdin) = split_command(&raw_command);

    Ok(Entry {
        schedule,
        user,
        command,
        stdin,
        raw_command,
        env: env.to_vec(),
        tz,
        mail_on_failure_only,
        quiet,
        line: lineno,
    })
}

fn strip_flag<'a>(s: &'a str, flag: &str) -> Option<&'a str> {
    let r = s.strip_prefix(flag)?;
    if r.starts_with(|c: char| c.is_whitespace()) {
        Some(r.trim_start())
    } else {
        None
    }
}

fn is_valid_username(user: &str) -> bool {
    !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$' | '@'))
}

/// Split a raw command at the first unescaped `%`.  In both halves `\%`
/// becomes `%`; in the stdin half every further `%` becomes a newline.  The
/// stdin text always ends in a newline.
pub fn split_command(raw: &str) -> (String, Option<String>) {
    let mut command = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut stdin: Option<String> = None;
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'%') => {
                chars.next();
                command.push('%');
            }
            '%' => {
                let mut data = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' if chars.peek() == Some(&'%') => {
                            chars.next();
                            data.push('%');
                        }
                        '%' => data.push('\n'),
                        other => data.push(other),
                    }
                }
                if !data.ends_with('\n') {
                    data.push('\n');
                }
                stdin = Some(data);
                break;
            }
            other => command.push(other),
        }
    }
    (command, stdin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_lines() {
        assert_eq!(
            parse_env_line("FOO=bar"),
            Some(("FOO".into(), "bar".into()))
        );
        assert_eq!(
            parse_env_line("  FOO = bar baz  "),
            Some(("FOO".into(), "bar baz".into()))
        );
        assert_eq!(
            parse_env_line("FOO=\"  spaced  \""),
            Some(("FOO".into(), "  spaced  ".into()))
        );
        assert_eq!(
            parse_env_line("FOO='single' "),
            Some(("FOO".into(), "single".into()))
        );
        assert_eq!(
            parse_env_line("MAILTO="),
            Some(("MAILTO".into(), "".into()))
        );
        assert_eq!(
            parse_env_line("MAILTO=\"\""),
            Some(("MAILTO".into(), "".into()))
        );
        assert_eq!(parse_env_line("FOO='unterminated"), None);
        assert_eq!(parse_env_line("FOO='x' trailing"), None);
        assert_eq!(parse_env_line("* * * * * echo a=b"), None);
        assert_eq!(parse_env_line("FOO bar=1"), None);
        assert_eq!(parse_env_line("=bar"), None);
        assert_eq!(parse_env_line("1FOO=bar"), None);
        assert_eq!(parse_env_line("@daily x=y"), None);
    }

    #[test]
    fn split_percent() {
        assert_eq!(split_command("echo hi"), ("echo hi".into(), None));
        assert_eq!(
            split_command("cat%line1%line2"),
            ("cat".into(), Some("line1\nline2\n".into()))
        );
        assert_eq!(split_command("date +\\%Y"), ("date +%Y".into(), None));
        assert_eq!(
            split_command("cat%100\\%%done%"),
            ("cat".into(), Some("100%\ndone\n".into()))
        );
        assert_eq!(split_command("cat%"), ("cat".into(), Some("\n".into())));
    }

    #[test]
    fn user_crontab() {
        let text = "\
# comment
SHELL=/bin/bash
MAILTO=alice
  \t
*/5 * * * * /usr/bin/backup --quick
LOGNAME=evil
0 3 * * 1 -n -q /usr/bin/backup --full
@reboot echo booted
";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(tab.entries.len(), 3);
        let e = &tab.entries[0];
        assert_eq!(e.command, "/usr/bin/backup --quick");
        assert_eq!(e.user, None);
        assert_eq!(e.line, 5);
        assert_eq!(
            e.env,
            vec![
                ("SHELL".to_string(), "/bin/bash".to_string()),
                ("MAILTO".to_string(), "alice".to_string())
            ]
        );
        let e = &tab.entries[1];
        assert!(e.mail_on_failure_only);
        assert!(e.quiet);
        assert_eq!(e.command, "/usr/bin/backup --full");
        assert_eq!(e.raw_command, "/usr/bin/backup --full");
        assert_eq!(env_get(&e.env, "LOGNAME"), None);
        assert!(tab.entries[2].schedule.is_reboot());
        assert_eq!(tab.entries[2].command, "echo booted");
    }

    #[test]
    fn system_crontab() {
        let text = "PATH=/usr/local/sbin:/usr/bin\n17 * * * *\troot\trun-parts /etc/cron.hourly\n";
        let tab = Crontab::parse(text, Format::System).unwrap();
        assert_eq!(tab.entries[0].user.as_deref(), Some("root"));
        assert_eq!(tab.entries[0].command, "run-parts /etc/cron.hourly");

        let errs = Crontab::parse("17 * * * * root\n", Format::System).unwrap_err();
        assert_eq!(errs[0].error, EntryError::BadCommand);
        let errs = Crontab::parse("17 * * * * bad;user cmd\n", Format::System).unwrap_err();
        assert_eq!(errs[0].error, EntryError::BadUsername);
    }

    #[test]
    fn errors_report_all_lines() {
        let text = "* * * * * ok\n99 * * * * bad\n* * * * *\nFOO=bar\n* 25 * * * x\n";
        let errs = Crontab::parse(text, Format::User).unwrap_err();
        assert_eq!(errs.len(), 3);
        assert_eq!(errs[0].line, 2);
        assert_eq!(
            errs[0].error,
            EntryError::Schedule(ScheduleError::BadMinute)
        );
        assert_eq!(errs[1].line, 3);
        assert_eq!(errs[1].error, EntryError::BadCommand);
        assert_eq!(errs[2].line, 5);
        assert_eq!(errs[2].error, EntryError::Schedule(ScheduleError::BadHour));
        assert_eq!(errs[0].to_string(), "line 2: bad minute");
    }

    #[test]
    fn cron_tz_and_random_delay() {
        let text =
            "CRON_TZ=Asia/Tokyo\nRANDOM_DELAY=15\n0 9 * * * work\nCRON_TZ=\n0 9 * * * local\n";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(tab.random_delay, Some(15));
        assert_eq!(tab.entries[0].tz, Some(chrono_tz::Asia::Tokyo));
        assert_eq!(tab.entries[1].tz, None);
        // CRON_TZ is still exported to the job environment.
        assert_eq!(env_get(&tab.entries[0].env, "CRON_TZ"), Some("Asia/Tokyo"));

        let errs = Crontab::parse("CRON_TZ=Mars/Olympus\n", Format::User).unwrap_err();
        assert!(matches!(errs[0].error, EntryError::BadTimezone(_)));
        let errs = Crontab::parse("RANDOM_DELAY=lots\n", Format::User).unwrap_err();
        assert!(matches!(errs[0].error, EntryError::BadRandomDelay(_)));
    }

    #[test]
    fn later_env_overrides_earlier() {
        let text = "A=1\nA=2\n* * * * * x\n";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(tab.entries[0].env, vec![("A".to_string(), "2".to_string())]);
    }
}
