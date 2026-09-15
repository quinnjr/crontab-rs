//! Crontab file parsing, following cronie 1.7.
//!
//! * Environment lines use a port of cronie's `load_env` state machine.
//! * A `-` before the time fields hides the job from the log. Only system
//!   crontabs and root's crontab may use it.
//! * `-n` before the command mails output only when the job fails. It is the
//!   only job option and may appear once.
//! * `%` splits the command from its standard input.
//! * `CRON_TZ` and `RANDOM_DELAY` apply to the entries that follow them.

use std::fmt;
use std::str::FromStr;

use chrono::FixedOffset;
use chrono_tz::Tz;

use crate::schedule::{Schedule, ScheduleError};

/// Variables that a crontab may not override.
pub const PROTECTED_VARS: &[&str] = &["LOGNAME", "USER"];

/// Largest `RANDOM_DELAY` cronie accepts, in minutes.
pub const MAX_RANDOM_DELAY: u32 = 24 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    Schedule(ScheduleError),
    BadUsername,
    BadCommand,
    BadOption,
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::Schedule(e) => e.fmt(f),
            EntryError::BadUsername => f.write_str("bad username"),
            EntryError::BadCommand => f.write_str("bad command"),
            EntryError::BadOption => f.write_str("bad option"),
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

/// A non-fatal problem found while parsing, such as a step larger than its
/// range. The message uses cronie's wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseWarning {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The time zone selected by a `CRON_TZ` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTz {
    /// An IANA zone such as `Asia/Tokyo`.
    Named(Tz),
    /// A fixed offset: a POSIX `stdoffset` string, or UTC for values glibc
    /// does not recognise.
    Fixed(FixedOffset),
}

impl JobTz {
    /// Interpret a `CRON_TZ` value the way glibc interprets `TZ`. An IANA
    /// name, optionally prefixed with `:`, selects that zone. A POSIX
    /// `std offset` string selects its standard offset. Anything else,
    /// including an empty value, is UTC.
    pub fn from_cron_tz(value: &str) -> JobTz {
        let name = value.strip_prefix(':').unwrap_or(value);
        if !name.is_empty()
            && let Ok(tz) = Tz::from_str(name)
        {
            return JobTz::Named(tz);
        }
        if let Some(offset) = posix_std_offset(value) {
            return JobTz::Fixed(offset);
        }
        JobTz::Fixed(FixedOffset::east_opt(0).expect("zero offset is valid"))
    }
}

/// The standard-time offset of a POSIX TZ string such as `EST5` or
/// `<+0330>-3:30`. Any daylight-saving part is ignored.
fn posix_std_offset(value: &str) -> Option<FixedOffset> {
    let rest = if let Some(r) = value.strip_prefix('<') {
        let end = r.find('>')?;
        &r[end + 1..]
    } else {
        let n = value.bytes().take_while(u8::is_ascii_alphabetic).count();
        if n < 3 {
            return None;
        }
        &value[n..]
    };
    let (sign, rest) = match rest.as_bytes().first()? {
        b'+' => (1i32, &rest[1..]),
        b'-' => (-1i32, &rest[1..]),
        b'0'..=b'9' => (1i32, rest),
        _ => return None,
    };
    let len = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b':')
        .count();
    let mut parts = rest[..len].split(':');
    let hours: i32 = parts.next()?.parse().ok()?;
    let minutes: i32 = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    let seconds: i32 = match parts.next() {
        Some(s) => s.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hours > 24 || minutes > 59 || seconds > 59 {
        return None;
    }
    // POSIX offsets count westward from UTC.
    FixedOffset::east_opt(-sign * (hours * 3600 + minutes * 60 + seconds))
}

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
    /// Time zone from the `CRON_TZ` in effect for this entry, if any.
    pub tz: Option<JobTz>,
    /// `-n` option: only mail output when the job fails.
    pub mail_on_failure_only: bool,
    /// Leading `-`: do not log the job.
    pub dont_log: bool,
    /// `RANDOM_DELAY` in effect for this entry, in minutes. `None` when it is
    /// unset or out of range (the daemon logs the latter).
    pub random_delay: Option<u32>,
    /// 1-based source line number.
    pub line: usize,
}

/// A parsed crontab file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crontab {
    pub entries: Vec<Entry>,
    /// Non-fatal problems, in line order.
    pub warnings: Vec<ParseWarning>,
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
    /// Parse crontab text. System crontabs may hide jobs from the log with a
    /// leading `-`; user crontabs may not (see [`parse_as`](Self::parse_as)).
    pub fn parse(text: &str, format: Format) -> Result<Crontab, Vec<ParseError>> {
        Self::parse_as(text, format, format == Format::System)
    }

    /// Parse crontab text. `privileged` allows the leading `-` that hides a
    /// job from the log, which cronie permits for system crontabs and root.
    ///
    /// Returns every error found, one per bad line, so that `crontab -T` can
    /// report them all.
    pub fn parse_as(
        text: &str,
        format: Format,
        privileged: bool,
    ) -> Result<Crontab, Vec<ParseError>> {
        let mut env: Vec<(String, String)> = Vec::new();
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        for (idx, raw) in text.lines().enumerate() {
            let lineno = idx + 1;
            let line = raw.trim_start_matches([' ', '\t']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((name, value)) = parse_env_line(line) {
                if !PROTECTED_VARS.contains(&name.as_str()) {
                    set_env(&mut env, name, value);
                }
                continue;
            }
            match parse_entry(line, format, &env, privileged, lineno, &mut warnings) {
                Ok(entry) => entries.push(entry),
                Err(error) => errors.push(ParseError {
                    line: lineno,
                    error,
                }),
            }
        }

        if errors.is_empty() {
            Ok(Crontab { entries, warnings })
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

/// C `isspace` in the "C" locale.
fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Try to interpret a line as an environment assignment, exactly as cronie's
/// `load_env` does.
///
/// The name runs to the first blank or `=` and may be quoted. The value may
/// be quoted, in which case only blanks may follow the closing quote; an
/// unquoted value runs to the end of the line with trailing blanks removed.
/// Returns `None` when the line is not an assignment and should be parsed
/// as a job.
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum S {
        NameI,
        Name,
        Eq1,
        Eq2,
        ValueI,
        Value,
        Fini,
        Error,
    }
    fn next(s: S) -> S {
        match s {
            S::NameI => S::Name,
            S::Name => S::Eq1,
            S::Eq1 => S::Eq2,
            S::Eq2 => S::ValueI,
            S::ValueI => S::Value,
            S::Value => S::Fini,
            S::Fini | S::Error => S::Error,
        }
    }

    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut state = S::NameI;
    let mut quote: Option<char> = None;
    let mut name = String::new();
    let mut value = String::new();

    while state != S::Error && i < chars.len() {
        let c = chars[i];
        match state {
            S::NameI | S::ValueI => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    i += 1;
                }
                state = next(state);
            }
            S::Name | S::Value => {
                if let Some(q) = quote {
                    if c == q {
                        state = next(state);
                        i += 1;
                        continue;
                    }
                    if state == S::Name && c == '=' {
                        state = S::Error;
                        continue;
                    }
                } else if state == S::Name {
                    if is_c_space(c) {
                        i += 1;
                        state = next(state);
                        continue;
                    }
                    if c == '=' {
                        state = next(state);
                        continue;
                    }
                }
                if state == S::Name {
                    name.push(c);
                } else {
                    value.push(c);
                }
                i += 1;
            }
            S::Eq1 => {
                if c == '=' {
                    state = next(state);
                    quote = None;
                } else if !is_c_space(c) {
                    state = S::Error;
                }
                i += 1;
            }
            S::Eq2 | S::Fini => {
                if is_c_space(c) {
                    i += 1;
                } else {
                    state = next(state);
                }
            }
            S::Error => break,
        }
    }

    let valid = state == S::Fini || state == S::Eq2 || (state == S::Value && quote.is_none());
    if !valid {
        return None;
    }
    if state == S::Value {
        let trimmed = value.trim_end_matches(is_c_space).len();
        value.truncate(trimmed);
    }
    Some((name, value))
}

/// cronie's `strtol`-based `RANDOM_DELAY` parsing: leading digits are used,
/// a value with no digits is 0, and anything negative, overflowing or above
/// [`MAX_RANDOM_DELAY`] is rejected.
pub fn parse_random_delay(value: &str) -> Option<u32> {
    let s = value.trim_start_matches(is_c_space);
    let (negative, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Some(0);
    }
    let n: u64 = s[..digits].parse().ok()?;
    if negative {
        return (n == 0).then_some(0);
    }
    (n <= MAX_RANDOM_DELAY as u64).then_some(n as u32)
}

fn parse_entry(
    line: &str,
    format: Format,
    env: &[(String, String)],
    privileged: bool,
    lineno: usize,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Entry, EntryError> {
    let mut line = line;
    let mut dont_log = false;
    if let Some(rest) = line.strip_prefix('-') {
        if !privileged {
            return Err(EntryError::BadOption);
        }
        dont_log = true;
        line = rest;
    }

    let (schedule, rest, schedule_warnings) = Schedule::parse_prefix_with_warnings(line)?;
    warnings.extend(schedule_warnings.into_iter().map(|message| ParseWarning {
        line: lineno,
        message,
    }));

    let (user, rest) = match format {
        Format::User => (None, rest),
        Format::System => {
            let end = rest.find([' ', '\t']).unwrap_or(rest.len());
            let (user, remainder) = rest.split_at(end);
            if user.is_empty() {
                return Err(EntryError::BadUsername);
            }
            (
                Some(user.to_string()),
                remainder.trim_start_matches([' ', '\t']),
            )
        }
    };

    let mut rest = rest;
    let mut mail_on_failure_only = false;
    while let Some(after_dash) = rest.strip_prefix('-') {
        let mut chars = after_dash.chars();
        match chars.next() {
            Some('n') if !mail_on_failure_only => mail_on_failure_only = true,
            _ => return Err(EntryError::BadOption),
        }
        let after = chars.as_str();
        if !after.starts_with([' ', '\t']) {
            return Err(EntryError::BadOption);
        }
        rest = after.trim_start_matches([' ', '\t']);
        if rest.is_empty() {
            return Err(EntryError::BadCommand);
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
        tz: env_get(env, "CRON_TZ").map(JobTz::from_cron_tz),
        mail_on_failure_only,
        dont_log,
        random_delay: env_get(env, "RANDOM_DELAY").and_then(parse_random_delay),
        line: lineno,
    })
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

    fn env(line: &str) -> Option<(String, String)> {
        parse_env_line(line)
    }

    fn pair(n: &str, v: &str) -> Option<(String, String)> {
        Some((n.to_string(), v.to_string()))
    }

    #[test]
    fn env_lines_follow_load_env() {
        assert_eq!(env("FOO=bar"), pair("FOO", "bar"));
        assert_eq!(env("FOO = bar baz  "), pair("FOO", "bar baz"));
        assert_eq!(env("FOO =  spaced value  "), pair("FOO", "spaced value"));
        assert_eq!(env("FOO=\"  spaced  \""), pair("FOO", "  spaced  "));
        assert_eq!(env("FOO='single' "), pair("FOO", "single"));
        assert_eq!(env("FOO=a=b"), pair("FOO", "a=b"));
        assert_eq!(env("FOO=bar # c"), pair("FOO", "bar # c"));
        assert_eq!(env("MAILTO="), pair("MAILTO", ""));
        assert_eq!(env("MAILTO= "), pair("MAILTO", ""));
        assert_eq!(env("MAILTO=\"\""), pair("MAILTO", ""));
        assert_eq!(env("1FOO=bar"), pair("1FOO", "bar"));
        assert_eq!(env("=bar"), pair("", "bar"));
        assert_eq!(env("\"FOO BAR\"=baz"), pair("FOO BAR", "baz"));
        assert_eq!(env("FOO='unterminated"), None);
        assert_eq!(env("FOO=\"a\" b"), None);
        assert_eq!(env("FOO"), None);
        assert_eq!(env("\"FO=O\"=x"), None);
        assert_eq!(env("* * * * * echo a=b"), None);
        assert_eq!(env("FOO bar=1"), None);
        assert_eq!(env("@daily x=y"), None);
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
  # indented comment
*/5 * * * * /usr/bin/backup --quick
LOGNAME=evil
0 3 * * 1 -n /usr/bin/backup --full
\t@reboot echo booted
";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(tab.entries.len(), 3);
        let e = &tab.entries[0];
        assert_eq!(e.command, "/usr/bin/backup --quick");
        assert_eq!(e.user, None);
        assert_eq!(e.line, 6);
        assert_eq!(
            e.env,
            vec![
                ("SHELL".to_string(), "/bin/bash".to_string()),
                ("MAILTO".to_string(), "alice".to_string())
            ]
        );
        let e = &tab.entries[1];
        assert!(e.mail_on_failure_only);
        assert!(!e.dont_log);
        assert_eq!(e.command, "/usr/bin/backup --full");
        assert_eq!(env_get(&e.env, "LOGNAME"), None);
        assert!(tab.entries[2].schedule.is_reboot());
        assert_eq!(tab.entries[2].command, "echo booted");
    }

    #[test]
    fn job_options() {
        let one = |line: &str| Crontab::parse(line, Format::User).map(|t| t.entries[0].clone());
        let e = one("* * * * * -n\tx").unwrap();
        assert!(e.mail_on_failure_only);
        assert_eq!(e.command, "x");
        for bad in [
            "* * * * * -n -n x",
            "* * * * * -nx",
            "* * * * * -q x",
            "* * * * * -n",
            "* * * * * -x y",
        ] {
            assert_eq!(
                one(bad).unwrap_err()[0].error,
                EntryError::BadOption,
                "{bad}"
            );
        }
        assert_eq!(
            one("* * * * * -n   ").unwrap_err()[0].error,
            EntryError::BadCommand
        );
    }

    #[test]
    fn leading_dash_hides_job_for_privileged_crontabs_only() {
        let errs = Crontab::parse("-* * * * * x\n", Format::User).unwrap_err();
        assert_eq!(errs[0].error, EntryError::BadOption);
        let errs = Crontab::parse("  -@daily x\n", Format::User).unwrap_err();
        assert_eq!(errs[0].error, EntryError::BadOption);

        let tab = Crontab::parse_as("-*/5 * * * * x\n", Format::User, true).unwrap();
        assert!(tab.entries[0].dont_log);
        let tab = Crontab::parse("-@hourly root x\n", Format::System).unwrap();
        assert!(tab.entries[0].dont_log);
        assert_eq!(tab.entries[0].user.as_deref(), Some("root"));
    }

    #[test]
    fn system_crontab() {
        let text = "PATH=/usr/local/sbin:/usr/bin\n17 * * * *\troot\trun-parts /etc/cron.hourly\n";
        let tab = Crontab::parse(text, Format::System).unwrap();
        assert_eq!(tab.entries[0].user.as_deref(), Some("root"));
        assert_eq!(tab.entries[0].command, "run-parts /etc/cron.hourly");
        let errs = Crontab::parse("17 * * * * root\n", Format::System).unwrap_err();
        assert_eq!(errs[0].error, EntryError::BadCommand);
        let errs = Crontab::parse("17 * * * *\n", Format::System).unwrap_err();
        assert_eq!(errs[0].error, EntryError::BadUsername);
        let tab = Crontab::parse("0 0 * * * root -n x\n", Format::System).unwrap();
        assert!(tab.entries[0].mail_on_failure_only);
    }

    #[test]
    fn errors_report_all_lines() {
        let text = "* * * * * ok\n99 * * * * bad\n* * * * *\nFOO=bar\n* 25 * * * x\n";
        let errs = Crontab::parse(text, Format::User).unwrap_err();
        assert_eq!(errs.len(), 3);
        assert_eq!(
            (errs[0].line, &errs[0].error),
            (2, &EntryError::Schedule(ScheduleError::BadMinute))
        );
        assert_eq!((errs[1].line, &errs[1].error), (3, &EntryError::BadCommand));
        assert_eq!(
            (errs[2].line, &errs[2].error),
            (5, &EntryError::Schedule(ScheduleError::BadHour))
        );
        assert_eq!(errs[0].to_string(), "line 2: bad minute");
    }

    #[test]
    fn step_warnings_carry_line_numbers() {
        let tab = Crontab::parse("MAILTO=\"\"\n*/61 * * * * x\n", Format::User).unwrap();
        assert_eq!(
            tab.warnings,
            vec![ParseWarning {
                line: 2,
                message: "Warning: Step size 61 higher than possible maximum of 59".into()
            }]
        );
    }

    #[test]
    fn cron_tz_resolves_like_glibc() {
        let utc = JobTz::Fixed(FixedOffset::east_opt(0).unwrap());
        assert_eq!(
            JobTz::from_cron_tz("Asia/Tokyo"),
            JobTz::Named(chrono_tz::Asia::Tokyo)
        );
        assert_eq!(
            JobTz::from_cron_tz(":Europe/Paris"),
            JobTz::Named(chrono_tz::Europe::Paris)
        );
        assert_eq!(JobTz::from_cron_tz("UTC"), JobTz::Named(chrono_tz::UTC));
        assert_eq!(JobTz::from_cron_tz(""), utc);
        assert_eq!(JobTz::from_cron_tz("Mars/Olympus"), utc);
        assert_eq!(JobTz::from_cron_tz("XY5"), utc);
        assert_eq!(
            JobTz::from_cron_tz("ABC5"),
            JobTz::Fixed(FixedOffset::west_opt(5 * 3600).unwrap())
        );
        assert_eq!(
            JobTz::from_cron_tz("<+0330>-3:30"),
            JobTz::Fixed(FixedOffset::east_opt(3 * 3600 + 1800).unwrap())
        );

        let text = "CRON_TZ=Asia/Tokyo\n0 9 * * * work\nCRON_TZ=\n0 9 * * * utc\nCRON_TZ=Mars/Olympus\n0 9 * * * mars\n";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(
            tab.entries[0].tz,
            Some(JobTz::Named(chrono_tz::Asia::Tokyo))
        );
        assert_eq!(tab.entries[1].tz, Some(utc));
        assert_eq!(tab.entries[2].tz, Some(utc));
        assert_eq!(env_get(&tab.entries[0].env, "CRON_TZ"), Some("Asia/Tokyo"));
        assert_eq!(
            Crontab::parse("0 9 * * * x\n", Format::User)
                .unwrap()
                .entries[0]
                .tz,
            None
        );
    }

    #[test]
    fn random_delay_uses_strtol_and_applies_forward() {
        assert_eq!(parse_random_delay("15"), Some(15));
        assert_eq!(parse_random_delay(" 15"), Some(15));
        assert_eq!(parse_random_delay("10abc"), Some(10));
        assert_eq!(parse_random_delay("abc"), Some(0));
        assert_eq!(parse_random_delay("+7"), Some(7));
        assert_eq!(parse_random_delay("1440"), Some(1440));
        assert_eq!(parse_random_delay("1441"), None);
        assert_eq!(parse_random_delay("-1"), None);
        assert_eq!(parse_random_delay("-0"), Some(0));
        assert_eq!(parse_random_delay("99999999999999999999"), None);

        let text = "0 0 * * * before\nRANDOM_DELAY=30\n0 0 * * * after\nRANDOM_DELAY=5000\n0 0 * * * bad\n";
        let tab = Crontab::parse(text, Format::User).unwrap();
        let delays: Vec<_> = tab.entries.iter().map(|e| e.random_delay).collect();
        assert_eq!(delays, vec![None, Some(30), None]);
    }

    #[test]
    fn later_env_overrides_earlier() {
        let tab = Crontab::parse("A=1\nA=2\n* * * * * x\n", Format::User).unwrap();
        assert_eq!(tab.entries[0].env, vec![("A".to_string(), "2".to_string())]);
    }
}
