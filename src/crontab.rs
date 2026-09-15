//! Crontab file parsing, following cronie 1.7.2.
//!
//! * Lines end at `\n`. Leading spaces and tabs are skipped, and lines
//!   starting with `#` are comments. A final line without a newline is an
//!   error ("premature EOF"); cronie's daemon ignores it.
//! * Environment lines use a port of cronie's `load_env` state machine. The
//!   environment starts with the locale, `RANDOM_DELAY` and `MAILFROM`
//!   variables inherited from the parsing process.
//! * A `-` before the time fields hides the job from the log. Only system
//!   crontabs and root's crontab may use it.
//! * `-n` before the command mails output only when the job fails. It is the
//!   only job option and may appear once.
//! * `%` splits the command from its standard input.
//! * `CRON_TZ` and `RANDOM_DELAY` apply to the entries after them.
//! * As in C, a NUL byte ends an environment line and a command.
//!
//! [`Crontab::parse_with`] returns every valid entry together with warnings
//! and errors in file order, so the daemon can skip bad lines (as cronie's
//! `load_user` does) while `crontab` stops at the first error (as cronie's
//! `check_syntax` does).

use std::fmt;

use crate::schedule::{Schedule, ScheduleError, is_blank, trim_blanks};
use crate::tz::Zone;

/// Variables that a crontab may not override.
pub const PROTECTED_VARS: &[&str] = &["LOGNAME", "USER"];

/// Largest `RANDOM_DELAY` cronie accepts, in minutes.
pub const MAX_RANDOM_DELAY: u32 = 24 * 60;

/// Variables cronie copies from its own environment into every crontab
/// before the file's own assignments (`env_set_from_environ`).
pub const INHERITED_VARS: &[&str] = &[
    "LANG",
    "LC_CTYPE",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_MESSAGES",
    "LC_PAPER",
    "LC_NAME",
    "LC_ADDRESS",
    "LC_TELEPHONE",
    "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
    "LC_ALL",
    "LANGUAGE",
    "RANDOM_DELAY",
    "MAILFROM",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    Schedule(ScheduleError),
    BadCommand,
    BadOption,
    /// The file's last line has no terminating newline.
    PrematureEof,
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::Schedule(e) => e.fmt(f),
            EntryError::BadCommand => f.write_str("bad command"),
            EntryError::BadOption => f.write_str("bad option"),
            EntryError::PrematureEof => f.write_str("premature EOF"),
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

/// A non-fatal problem, such as a step larger than its range. The message
/// uses cronie's wording.
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

/// A warning or error, reported in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    Warning(ParseWarning),
    Error(ParseError),
}

/// The `RANDOM_DELAY` in effect for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandomDelay {
    Unset,
    /// Upper bound of the delay, in minutes.
    Minutes(u32),
    /// Set to a value cronie rejects (negative, overflowing or above
    /// [`MAX_RANDOM_DELAY`]); the daemon logs it and uses no delay.
    Invalid,
}

impl RandomDelay {
    /// cronie's `strtol`-based parsing: leading digits are used and a value
    /// with no digits is 0.
    pub fn from_value(value: &str) -> RandomDelay {
        let s = value.trim_start_matches(is_c_space);
        let (negative, s) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return RandomDelay::Minutes(0);
        }
        match s[..digits].parse::<u64>() {
            Ok(0) => RandomDelay::Minutes(0),
            Ok(_) if negative => RandomDelay::Invalid,
            Ok(n) if n <= u64::from(MAX_RANDOM_DELAY) => RandomDelay::Minutes(n as u32),
            _ => RandomDelay::Invalid,
        }
    }
}

/// One job line from a crontab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub schedule: Schedule,
    /// User to run as; `None` for user crontabs (the file owner runs it).
    /// The user is looked up when the job runs, as in cronie.
    pub user: Option<String>,
    /// The shell command, with `\%` unescaped and any `%` stdin part removed.
    pub command: String,
    /// Data fed to the command's standard input (from `%` sections).
    pub stdin: Option<String>,
    /// The full command text as written, for logging.
    pub raw_command: String,
    /// Environment in effect for this entry, in declaration order.
    pub env: Vec<(String, String)>,
    /// Zone from a non-empty `CRON_TZ`; `None` means the daemon's local time.
    pub tz: Option<Zone>,
    /// `-n` option: only mail output when the job fails.
    pub mail_on_failure_only: bool,
    /// Leading `-`: do not log the job.
    pub dont_log: bool,
    /// `RANDOM_DELAY` in effect for this entry.
    pub random_delay: RandomDelay,
    /// 1-based source line number.
    pub line: usize,
}

/// A parsed crontab file: its valid entries in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crontab {
    pub entries: Vec<Entry>,
}

/// Which crontab format a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `min hour dom mon dow command` — the owner runs the command.
    User,
    /// `min hour dom mon dow user command` (`/etc/crontab`, `/etc/cron.d`).
    System,
}

/// How to parse a crontab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOptions {
    pub format: Format,
    /// Allow the leading `-` that hides a job from the log. cronie allows it
    /// for system crontabs and for root.
    pub privileged: bool,
    /// Environment applied before the file's own assignments.
    pub inherited_env: Vec<(String, String)>,
}

impl ParseOptions {
    /// Options for `format`, privileged for system crontabs, with no
    /// inherited environment.
    pub fn new(format: Format) -> ParseOptions {
        ParseOptions {
            format,
            privileged: format == Format::System,
            inherited_env: Vec::new(),
        }
    }

    /// Set whether the leading `-` is allowed.
    pub fn privileged(mut self, privileged: bool) -> ParseOptions {
        self.privileged = privileged;
        self
    }

    /// Inherit [`INHERITED_VARS`] from this process's environment.
    pub fn inherit_process_env(mut self) -> ParseOptions {
        self.inherited_env = inherited_process_env();
        self
    }
}

/// The [`INHERITED_VARS`] set in this process's environment, in environment
/// order.
pub fn inherited_process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            if !INHERITED_VARS.contains(&k.as_str()) {
                return None;
            }
            Some((k, v.into_string().ok()?))
        })
        .collect()
}

/// The result of [`Crontab::parse_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOutput {
    /// Every entry that parsed; bad lines are left out.
    pub crontab: Crontab,
    /// Warnings and errors in file order.
    pub diagnostics: Vec<Diagnostic>,
}

impl ParseOutput {
    /// All errors, in file order.
    pub fn errors(&self) -> impl Iterator<Item = &ParseError> {
        self.diagnostics.iter().filter_map(|d| match d {
            Diagnostic::Error(e) => Some(e),
            Diagnostic::Warning(_) => None,
        })
    }

    /// True when no line had an error.
    pub fn is_valid(&self) -> bool {
        self.errors().next().is_none()
    }

    /// Diagnostics up to and including the first error. This is what cronie's
    /// `crontab` reports, because its syntax check stops at the first error.
    pub fn until_first_error(&self) -> &[Diagnostic] {
        let end = self
            .diagnostics
            .iter()
            .position(|d| matches!(d, Diagnostic::Error(_)))
            .map_or(self.diagnostics.len(), |i| i + 1);
        &self.diagnostics[..end]
    }
}

/// Parsing state carried from line to line.
struct State {
    env: Vec<(String, String)>,
    tz: Option<Zone>,
    random_delay: RandomDelay,
}

impl Crontab {
    /// Strictly parse crontab text with [`ParseOptions::new`]: any error
    /// fails the whole file.
    pub fn parse(text: &str, format: Format) -> Result<Crontab, Vec<ParseError>> {
        let out = Self::parse_with(text, &ParseOptions::new(format));
        let errors: Vec<ParseError> = out.errors().cloned().collect();
        if errors.is_empty() {
            Ok(out.crontab)
        } else {
            Err(errors)
        }
    }

    /// Parse crontab text, keeping every valid entry and reporting warnings
    /// and errors in file order.
    pub fn parse_with(text: &str, options: &ParseOptions) -> ParseOutput {
        let mut state = State {
            env: Vec::new(),
            tz: None,
            random_delay: RandomDelay::Unset,
        };
        for (name, value) in &options.inherited_env {
            state.assign(name.clone(), value.clone());
        }

        let mut entries = Vec::new();
        let mut diagnostics = Vec::new();
        let mut lines: Vec<&str> = text.split('\n').collect();
        let unterminated = lines.pop().unwrap_or("");
        for (idx, line) in lines.iter().enumerate() {
            parse_line(
                line,
                idx + 1,
                options,
                &mut state,
                &mut entries,
                &mut diagnostics,
            );
        }
        let tail = trim_blanks(unterminated);
        if !tail.is_empty() && !tail.starts_with('#') {
            diagnostics.push(Diagnostic::Error(ParseError {
                line: lines.len() + 1,
                error: EntryError::PrematureEof,
            }));
        }
        ParseOutput {
            crontab: Crontab { entries },
            diagnostics,
        }
    }
}

impl State {
    fn assign(&mut self, name: String, value: String) {
        match name.as_str() {
            // cronie applies CRON_TZ only when it is non-empty.
            "CRON_TZ" => self.tz = (!value.is_empty()).then(|| Zone::from_tz_value(&value)),
            "RANDOM_DELAY" => self.random_delay = RandomDelay::from_value(&value),
            _ => {}
        }
        if PROTECTED_VARS.contains(&name.as_str()) {
            return;
        }
        match self.env.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => self.env.push((name, value)),
        }
    }
}

fn parse_line(
    raw: &str,
    lineno: usize,
    options: &ParseOptions,
    state: &mut State,
    entries: &mut Vec<Entry>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let line = trim_blanks(raw);
    if line.is_empty() || line.starts_with('#') {
        return;
    }
    if let Some((name, value)) = parse_env_line(line) {
        state.assign(name, value);
        return;
    }
    let mut warnings = Vec::new();
    let result = parse_entry(line, lineno, options, state, &mut warnings);
    diagnostics.extend(warnings.into_iter().map(|message| {
        Diagnostic::Warning(ParseWarning {
            line: lineno,
            message,
        })
    }));
    match result {
        Ok(entry) => entries.push(entry),
        Err(error) => diagnostics.push(Diagnostic::Error(ParseError {
            line: lineno,
            error,
        })),
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

/// The part of `s` a C string would see.
fn c_str(s: &str) -> &str {
    s.find('\0').map_or(s, |i| &s[..i])
}

/// Try to interpret a line as an environment assignment, exactly as cronie's
/// `load_env` does after skipping leading blanks.
///
/// The name runs to the first blank or `=` and may be quoted. The value may
/// be quoted, in which case only whitespace may follow the closing quote; an
/// unquoted value runs to the end of the line with trailing whitespace
/// removed. A NUL byte ends the line. Returns `None` when the line is not an
/// assignment and should be parsed as a job.
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

    let mut chars = c_str(trim_blanks(line)).chars().peekable();
    let mut state = S::NameI;
    let mut quote: Option<char> = None;
    let mut name = String::new();
    let mut value = String::new();

    while state != S::Error {
        let Some(&c) = chars.peek() else { break };
        match state {
            S::NameI | S::ValueI => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    chars.next();
                }
                state = next(state);
            }
            S::Name | S::Value => {
                if let Some(q) = quote {
                    if c == q {
                        chars.next();
                        state = next(state);
                        continue;
                    }
                    if state == S::Name && c == '=' {
                        state = S::Error;
                        continue;
                    }
                } else if state == S::Name {
                    if is_c_space(c) {
                        chars.next();
                        state = next(state);
                        continue;
                    }
                    if c == '=' {
                        state = next(state);
                        continue;
                    }
                }
                chars.next();
                if state == S::Name {
                    name.push(c);
                } else {
                    value.push(c);
                }
            }
            S::Eq1 => {
                chars.next();
                if c == '=' {
                    state = next(state);
                    quote = None;
                } else if !is_c_space(c) {
                    state = S::Error;
                }
            }
            S::Eq2 | S::Fini => {
                if is_c_space(c) {
                    chars.next();
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
        let keep = value.trim_end_matches(is_c_space).len();
        value.truncate(keep);
    }
    Some((name, value))
}

fn parse_entry(
    line: &str,
    lineno: usize,
    options: &ParseOptions,
    state: &State,
    warnings: &mut Vec<String>,
) -> Result<Entry, EntryError> {
    let mut line = line;
    let mut dont_log = false;
    if let Some(rest) = line.strip_prefix('-') {
        if !options.privileged {
            return Err(EntryError::BadOption);
        }
        // cronie reads the minute field from the very next character.
        if rest.is_empty() || rest.starts_with(is_blank) {
            return Err(ScheduleError::BadMinute.into());
        }
        dont_log = true;
        line = rest;
    }

    let (schedule, rest) = Schedule::parse_prefix_with(line, &mut rand::rng(), warnings)?;
    // cronie: "check for premature EOL and catch a common typo".
    if rest.is_empty() || rest.starts_with('*') {
        return Err(EntryError::BadCommand);
    }

    let (user, mut rest) = match options.format {
        Format::User => (None, rest),
        Format::System => {
            let end = rest.find(is_blank).unwrap_or(rest.len());
            let (user, remainder) = rest.split_at(end);
            let remainder = trim_blanks(remainder);
            if remainder.is_empty() {
                return Err(EntryError::BadCommand);
            }
            (Some(c_str(user).to_string()), remainder)
        }
    };

    let mut mail_on_failure_only = false;
    while let Some(after_dash) = rest.strip_prefix('-') {
        let mut chars = after_dash.chars();
        match chars.next() {
            Some('n') if !mail_on_failure_only => mail_on_failure_only = true,
            _ => return Err(EntryError::BadOption),
        }
        let after = chars.as_str();
        if !after.starts_with(is_blank) {
            return Err(EntryError::BadOption);
        }
        rest = trim_blanks(after);
        if rest.is_empty() {
            return Err(EntryError::BadCommand);
        }
    }

    // The command is everything up to the newline, trailing blanks included.
    let raw_command = c_str(rest).to_string();
    let (command, stdin) = split_command(&raw_command);

    Ok(Entry {
        schedule,
        user,
        command,
        stdin,
        raw_command,
        env: state.env.clone(),
        tz: state.tz.clone(),
        mail_on_failure_only,
        dont_log,
        random_delay: state.random_delay,
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

    fn pair(n: &str, v: &str) -> Option<(String, String)> {
        Some((n.to_string(), v.to_string()))
    }

    fn user(text: &str) -> ParseOutput {
        Crontab::parse_with(text, &ParseOptions::new(Format::User))
    }

    fn first_error(out: &ParseOutput) -> Option<EntryError> {
        out.errors().next().map(|e| e.error.clone())
    }

    #[test]
    fn env_lines_follow_load_env() {
        assert_eq!(parse_env_line("FOO=bar"), pair("FOO", "bar"));
        assert_eq!(parse_env_line("  FOO=bar"), pair("FOO", "bar"));
        assert_eq!(parse_env_line("FOO = bar baz  "), pair("FOO", "bar baz"));
        assert_eq!(
            parse_env_line("FOO =  spaced value  "),
            pair("FOO", "spaced value")
        );
        assert_eq!(
            parse_env_line("FOO=\"  spaced  \""),
            pair("FOO", "  spaced  ")
        );
        assert_eq!(parse_env_line("FOO='single' "), pair("FOO", "single"));
        assert_eq!(parse_env_line("FOO=a=b"), pair("FOO", "a=b"));
        assert_eq!(parse_env_line("FOO=bar # c"), pair("FOO", "bar # c"));
        assert_eq!(parse_env_line("MAILTO="), pair("MAILTO", ""));
        assert_eq!(parse_env_line("MAILTO=\"\""), pair("MAILTO", ""));
        assert_eq!(parse_env_line("1FOO=bar"), pair("1FOO", "bar"));
        assert_eq!(parse_env_line("=bar"), pair("", "bar"));
        assert_eq!(parse_env_line("\"FOO BAR\"=baz"), pair("FOO BAR", "baz"));
        assert_eq!(parse_env_line("FOO=a\0b"), pair("FOO", "a"));
        assert_eq!(parse_env_line("FOO\0=bar"), None);
        assert_eq!(parse_env_line("FOO='unterminated"), None);
        assert_eq!(parse_env_line("FOO=\"a\" b"), None);
        assert_eq!(parse_env_line("FOO"), None);
        assert_eq!(parse_env_line("\"FO=O\"=x"), None);
        assert_eq!(parse_env_line("* * * * * echo a=b"), None);
        assert_eq!(parse_env_line("FOO bar=1"), None);
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
    }

    #[test]
    fn user_crontab() {
        let text = "# comment\nSHELL=/bin/bash\nMAILTO=alice\n  \t\n  # indented\n*/5 * * * * /usr/bin/backup --quick\nLOGNAME=evil\n0 3 * * 1 -n /usr/bin/backup --full\n\t@reboot echo booted\n";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(tab.entries.len(), 3);
        let e = &tab.entries[0];
        assert_eq!(
            (e.command.as_str(), e.user.clone(), e.line),
            ("/usr/bin/backup --quick", None, 6)
        );
        assert_eq!(
            e.env,
            vec![
                ("SHELL".into(), "/bin/bash".into()),
                ("MAILTO".into(), "alice".into())
            ]
        );
        let e = &tab.entries[1];
        assert!(e.mail_on_failure_only && !e.dont_log);
        assert_eq!(env_get(&e.env, "LOGNAME"), None);
        assert!(tab.entries[2].schedule.is_reboot());
    }

    #[test]
    fn bad_lines_are_skipped_and_reported_in_order() {
        let out = user("0 0 * * * a\n*/61 99 * * * b\n0 0 * * * c\n88 * * * * d\n");
        let cmds: Vec<_> = out
            .crontab
            .entries
            .iter()
            .map(|e| e.command.as_str())
            .collect();
        assert_eq!(cmds, vec!["a", "c"]);
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::Warning(ParseWarning {
                    line: 2,
                    message: "Warning: Step size 61 higher than possible maximum of 59".into()
                }),
                Diagnostic::Error(ParseError {
                    line: 2,
                    error: ScheduleError::BadHour.into()
                }),
                Diagnostic::Error(ParseError {
                    line: 4,
                    error: ScheduleError::BadMinute.into()
                }),
            ]
        );
        assert_eq!(out.until_first_error().len(), 2);
        assert!(!out.is_valid());
        assert_eq!(
            Crontab::parse("0 0 * * * a\n88 * * * * d\n", Format::User)
                .unwrap_err()
                .len(),
            1
        );
    }

    #[test]
    fn missing_final_newline_is_premature_eof() {
        let out = user("0 0 * * * a\n0 0 * * * b");
        assert_eq!(out.crontab.entries.len(), 1);
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::Error(ParseError {
                line: 2,
                error: EntryError::PrematureEof
            })]
        );
        assert!(user("0 0 * * * a\n# trailing comment").is_valid());
        assert!(user("0 0 * * * a\n  \t").is_valid());
        assert!(user("").is_valid());
    }

    #[test]
    fn premature_eol_and_star_typo_are_bad_commands() {
        for line in [
            "0 */5 * * * * cmd\n",
            "@daily *cmd\n",
            "0 0 * * *\n",
            "@daily   \n",
        ] {
            assert_eq!(
                first_error(&user(line)),
                Some(EntryError::BadCommand),
                "{line:?}"
            );
        }
        let sys = |t: &str| Crontab::parse_with(t, &ParseOptions::new(Format::System));
        for line in [
            "17 * * * *\n",
            "17 * * * * root\n",
            "17 * * * * root   \n",
            "@hourly * x\n",
        ] {
            assert_eq!(
                first_error(&sys(line)),
                Some(EntryError::BadCommand),
                "{line:?}"
            );
        }
    }

    #[test]
    fn commands_keep_trailing_blanks_and_stop_at_nul() {
        let tab = Crontab::parse(
            "0 0 * * * echo hi  \n0 0 * * * cat%in  \n0 0 * * * a\0b\n",
            Format::User,
        )
        .unwrap();
        assert_eq!(tab.entries[0].command, "echo hi  ");
        assert_eq!(tab.entries[1].stdin.as_deref(), Some("in  \n"));
        assert_eq!(tab.entries[2].command, "a");
        assert_eq!(
            first_error(&user("FOO\0=bar\n")),
            Some(ScheduleError::BadMinute.into())
        );
    }

    #[test]
    fn job_options() {
        let one = |line: &str| user(line);
        let out = one("* * * * * -n\tx\n");
        assert!(out.crontab.entries[0].mail_on_failure_only);
        assert_eq!(out.crontab.entries[0].command, "x");
        for bad in [
            "* * * * * -n -n x\n",
            "* * * * * -nx\n",
            "* * * * * -q x\n",
            "* * * * * -n\n",
            "* * * * * -x y\n",
        ] {
            assert_eq!(
                first_error(&one(bad)),
                Some(EntryError::BadOption),
                "{bad:?}"
            );
        }
        assert_eq!(
            first_error(&one("* * * * * -n   \n")),
            Some(EntryError::BadCommand)
        );
    }

    #[test]
    fn leading_dash() {
        assert_eq!(
            first_error(&user("-* * * * * x\n")),
            Some(EntryError::BadOption)
        );
        assert_eq!(
            first_error(&user("  -@daily x\n")),
            Some(EntryError::BadOption)
        );
        let root = ParseOptions::new(Format::User).privileged(true);
        let out = Crontab::parse_with("-*/5 * * * * x\n-@hourly y\n", &root);
        assert!(out.is_valid());
        assert!(out.crontab.entries.iter().all(|e| e.dont_log));
        for bad in ["- 0 * * * * x\n", "-\t@daily x\n", "-\n"] {
            let out = Crontab::parse_with(bad, &root);
            assert_eq!(
                first_error(&out),
                Some(ScheduleError::BadMinute.into()),
                "{bad:?}"
            );
        }
        let tab = Crontab::parse("-@hourly root x\n", Format::System).unwrap();
        assert!(tab.entries[0].dont_log);
    }

    #[test]
    fn system_crontab_users_are_not_checked_at_parse_time() {
        let tab = Crontab::parse(
            "17 * * * *\troot\trun-parts /etc/cron.hourly\n0 0 * * * nosuchuser x\n",
            Format::System,
        )
        .unwrap();
        assert_eq!(tab.entries[0].user.as_deref(), Some("root"));
        assert_eq!(tab.entries[1].user.as_deref(), Some("nosuchuser"));
        let tab = Crontab::parse("0 0 * * * root -n x\n", Format::System).unwrap();
        assert!(tab.entries[0].mail_on_failure_only);
    }

    #[test]
    fn cron_tz_empty_means_local() {
        let tokyo_exists = std::path::Path::new("/usr/share/zoneinfo/Asia/Tokyo").exists();
        let tab = Crontab::parse(
            "CRON_TZ=Asia/Tokyo\n0 9 * * * a\nCRON_TZ=\n0 9 * * * b\n0 9 * * * c\n",
            Format::User,
        )
        .unwrap();
        if tokyo_exists {
            assert_eq!(tab.entries[0].tz, Some(Zone::from_tz_value("Asia/Tokyo")));
        }
        assert!(tab.entries[0].tz.is_some());
        assert_eq!(tab.entries[1].tz, None);
        assert_eq!(env_get(&tab.entries[1].env, "CRON_TZ"), Some(""));
        assert_eq!(
            Crontab::parse("0 9 * * * x\n", Format::User)
                .unwrap()
                .entries[0]
                .tz,
            None
        );
    }

    #[test]
    fn random_delay() {
        use RandomDelay::*;
        assert_eq!(RandomDelay::from_value("15"), Minutes(15));
        assert_eq!(RandomDelay::from_value(" 15"), Minutes(15));
        assert_eq!(RandomDelay::from_value("10abc"), Minutes(10));
        assert_eq!(RandomDelay::from_value("abc"), Minutes(0));
        assert_eq!(RandomDelay::from_value("+7"), Minutes(7));
        assert_eq!(RandomDelay::from_value("1440"), Minutes(1440));
        assert_eq!(RandomDelay::from_value("1441"), Invalid);
        assert_eq!(RandomDelay::from_value("-1"), Invalid);
        assert_eq!(RandomDelay::from_value("-0"), Minutes(0));
        assert_eq!(RandomDelay::from_value("99999999999999999999"), Invalid);

        let tab = Crontab::parse("0 0 * * * before\nRANDOM_DELAY=30\n0 0 * * * after\nRANDOM_DELAY=5000\n0 0 * * * bad\n", Format::User).unwrap();
        let delays: Vec<_> = tab.entries.iter().map(|e| e.random_delay).collect();
        assert_eq!(delays, vec![Unset, Minutes(30), Invalid]);
    }

    #[test]
    fn inherited_environment_comes_first() {
        let options = ParseOptions {
            inherited_env: vec![
                ("RANDOM_DELAY".into(), "20".into()),
                ("LANG".into(), "C.UTF-8".into()),
            ],
            ..ParseOptions::new(Format::User)
        };
        let out = Crontab::parse_with("0 0 * * * a\nLANG=de_DE.UTF-8\n0 0 * * * b\n", &options);
        let e = &out.crontab.entries;
        assert_eq!(e[0].random_delay, RandomDelay::Minutes(20));
        assert_eq!(env_get(&e[0].env, "LANG"), Some("C.UTF-8"));
        assert_eq!(env_get(&e[1].env, "LANG"), Some("de_DE.UTF-8"));
        assert!(INHERITED_VARS.contains(&"MAILFROM"));
    }

    #[test]
    fn later_env_overrides_earlier() {
        let tab = Crontab::parse("A=1\nA=2\n* * * * * x\n", Format::User).unwrap();
        assert_eq!(tab.entries[0].env, vec![("A".to_string(), "2".to_string())]);
    }
}
