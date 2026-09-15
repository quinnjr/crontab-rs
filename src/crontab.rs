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
//! * `CRON_TZ` and `RANDOM_DELAY` apply to the entries after them. `CRON_TZ`
//!   is kept as text; the parser never resolves zones or touches the
//!   filesystem.
//! * As in C, a NUL byte ends an environment line and a command.
//! * cronie's buffer limits are ported: an environment line is examined only
//!   up to [`MAX_ENVSTR`]` - 1` bytes, a system crontab's user name and a
//!   command keep at most [`MAX_COMMAND`]` - 1` bytes, and more than
//!   [`MAX_GARBAGE`] characters of blank/comment content between two content
//!   lines is reported as [`Diagnostic::TooMuchGarbage`].
//! * Limits count bytes of the original input. For UTF-8 input a truncation
//!   stops at the last character boundary within the limit, where cronie would
//!   cut a multi-byte character in half; this is the only divergence in the
//!   limits.
//! * Known divergence: after a bad job line cronie discards at most
//!   [`MAX_COMMAND`] characters of the rest of that line, so it re-parses the
//!   tail of an over-long bad line as a new line; this parser always discards
//!   the whole line.
//!
//! [`Crontab::parse_with`] and [`Crontab::parse_bytes`] return every valid
//! entry together with warnings and errors in file order, so the daemon can
//! skip bad lines (as cronie's `load_user` does) while `crontab` stops at the
//! first error (as cronie's `check_syntax` does).

use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;

use crate::schedule::{Schedule, ScheduleError, is_blank, trim_blanks};

/// Variables that a crontab may not override.
pub const PROTECTED_VARS: &[&str] = &["LOGNAME", "USER"];

/// Largest `RANDOM_DELAY` cronie accepts, in minutes.
pub const MAX_RANDOM_DELAY: u32 = 24 * 60;

/// Most environment assignments cronie accepts in a user crontab.
pub const MAX_USER_ENVS: usize = 1000;
/// Most job lines cronie accepts in a user crontab.
pub const MAX_USER_ENTRIES: usize = 10000;
/// Most blank/comment characters cronie skips between two content lines.
pub const MAX_GARBAGE: usize = 32768;
/// Size of cronie's environment line buffer (terminating NUL included).
pub const MAX_ENVSTR: usize = 131072;
/// Size of cronie's command and user name buffer (terminating NUL included).
pub const MAX_COMMAND: usize = 131072;

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

/// How a crontab's bytes were decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Latin1,
}

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
    /// The RANDOM_DELAY in effect for the job on `line` is out of range. cronie's daemon logs
    /// "bad value of RANDOM_DELAY" at this point in load_entry (after the time fields and user
    /// field parse successfully, before the -n option is parsed); `crontab` prints nothing for it.
    BadRandomDelay {
        line: usize,
    },
    /// More than MAX_GARBAGE characters of blank/comment content were skipped before a content
    /// line (cronie's skip_comments returning FALSE). Emitted at most once per gap.
    ///
    /// `line` is the number cronie's `crontab` prints (`LineNumber - 1`), not a plain line
    /// number: if the character that exceeded the limit is the newline ending line N, it is N;
    /// otherwise (a character inside line N, the first non-blank character of the content line
    /// N, or the end of file after an unterminated line N) it is N - 1, so it can be 0.
    TooMuchGarbage {
        line: usize,
    },
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
    /// `CRON_TZ` in effect for this entry: `None` when unset, `Some("")` when
    /// explicitly set to the empty string. Never resolved by the parser.
    pub cron_tz: Option<String>,
    /// `-n` option: only mail output when the job fails.
    pub mail_on_failure_only: bool,
    /// Leading `-`: do not log the job.
    pub dont_log: bool,
    /// `RANDOM_DELAY` in effect for this entry.
    pub random_delay: RandomDelay,
    /// 1-based source line number.
    pub line: usize,
    /// How the crontab's bytes were decoded.
    pub encoding: TextEncoding,
}

impl Entry {
    /// The original crontab bytes of a string taken from this entry (command, raw_command, stdin, env names/values, user).
    pub fn bytes_of(&self, s: &str) -> Vec<u8> {
        match self.encoding {
            TextEncoding::Utf8 => s.as_bytes().to_vec(),
            // Latin-1 decoding maps every byte to the char with that code
            // point, so every char is <= U+00FF.
            TextEncoding::Latin1 => s.chars().map(|c| c as u32 as u8).collect(),
        }
    }

    /// [`bytes_of`](Self::bytes_of) as an `OsString`.
    pub fn os_of(&self, s: &str) -> OsString {
        OsString::from_vec(self.bytes_of(s))
    }
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
    /// 1-based line numbers of environment assignment lines, in file order.
    pub env_lines: Vec<usize>,
    /// 1-based line numbers of every line parsed as a job, valid or not, in file order
    /// (what cronie's load_user counts against MAX_USER_ENTRIES).
    pub entry_lines: Vec<usize>,
}

impl ParseOutput {
    /// All errors, in file order.
    pub fn errors(&self) -> impl Iterator<Item = &ParseError> {
        self.diagnostics.iter().filter_map(|d| match d {
            Diagnostic::Error(e) => Some(e),
            _ => None,
        })
    }

    /// True when no line had an error.
    pub fn is_valid(&self) -> bool {
        self.errors().next().is_none()
    }

    /// Diagnostics up to and including the first `Error` or `TooMuchGarbage`,
    /// without `BadRandomDelay` entries. This is what cronie's `crontab`
    /// reports, because its syntax check stops at either.
    pub fn until_first_error(&self) -> Vec<&Diagnostic> {
        let mut out = Vec::new();
        for d in &self.diagnostics {
            match d {
                Diagnostic::BadRandomDelay { .. } => {}
                Diagnostic::Error(_) | Diagnostic::TooMuchGarbage { .. } => {
                    out.push(d);
                    break;
                }
                Diagnostic::Warning(_) => out.push(d),
            }
        }
        out
    }

    /// The line of the first `Error` or `TooMuchGarbage` (for the latter, the
    /// number cronie prints; see [`Diagnostic::TooMuchGarbage`]).
    pub fn first_error_line(&self) -> Option<usize> {
        self.diagnostics.iter().find_map(|d| match d {
            Diagnostic::Error(e) => Some(e.line),
            Diagnostic::TooMuchGarbage { line } => Some(*line),
            _ => None,
        })
    }
}

/// Parsing state carried from line to line.
struct State {
    env: Vec<(String, String)>,
    cron_tz: Option<String>,
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
    /// and errors in file order. Entries are tagged [`TextEncoding::Utf8`].
    pub fn parse_with(text: &str, options: &ParseOptions) -> ParseOutput {
        parse_text(text, options, TextEncoding::Utf8)
    }

    /// Parse raw crontab bytes: valid UTF-8 is parsed as text; otherwise every byte is mapped to
    /// the char with that code point (Latin-1) so the grammar (all ASCII) and byte-exact
    /// commands are preserved, and entries are tagged TextEncoding::Latin1.
    pub fn parse_bytes(bytes: &[u8], options: &ParseOptions) -> ParseOutput {
        match std::str::from_utf8(bytes) {
            Ok(text) => parse_text(text, options, TextEncoding::Utf8),
            Err(_) => {
                let text: String = bytes.iter().map(|&b| char::from(b)).collect();
                parse_text(&text, options, TextEncoding::Latin1)
            }
        }
    }
}

fn parse_text(text: &str, options: &ParseOptions, encoding: TextEncoding) -> ParseOutput {
    let mut state = State {
        env: Vec::new(),
        cron_tz: None,
        random_delay: RandomDelay::Unset,
    };
    for (name, value) in &options.inherited_env {
        state.assign(name.clone(), value.clone());
    }

    let mut parser = Parser {
        options,
        encoding,
        state,
        entries: Vec::new(),
        diagnostics: Vec::new(),
        env_lines: Vec::new(),
        entry_lines: Vec::new(),
        garbage: 0,
        garbage_reported: false,
    };
    let mut lines: Vec<&str> = text.split('\n').collect();
    let unterminated = lines.pop().unwrap_or("");
    for (idx, line) in lines.iter().enumerate() {
        parser.line(line, idx + 1);
    }
    if !unterminated.is_empty() {
        let lineno = lines.len() + 1;
        if parser.skip(unterminated, lineno, false).is_some() {
            parser.diagnostics.push(Diagnostic::Error(ParseError {
                line: lineno,
                error: EntryError::PrematureEof,
            }));
        }
    }
    ParseOutput {
        crontab: Crontab {
            entries: parser.entries,
        },
        diagnostics: parser.diagnostics,
        env_lines: parser.env_lines,
        entry_lines: parser.entry_lines,
    }
}

struct Parser<'o> {
    options: &'o ParseOptions,
    encoding: TextEncoding,
    state: State,
    entries: Vec<Entry>,
    diagnostics: Vec<Diagnostic>,
    env_lines: Vec<usize>,
    entry_lines: Vec<usize>,
    /// Characters `skip_comments` has counted in the current gap.
    garbage: usize,
    /// `TooMuchGarbage` was already emitted for the current gap.
    garbage_reported: bool,
}

impl Parser<'_> {
    /// Count `n` characters read by cronie's `skip_comments` on line
    /// `lineno`; `ends_with_newline` says whether the last of them is the
    /// line's newline.
    fn count_garbage(&mut self, n: usize, lineno: usize, ends_with_newline: bool) {
        if !self.garbage_reported && self.garbage + n > MAX_GARBAGE {
            // 1-based index, within these n, of the character that pushed the
            // count over the limit. cronie prints `LineNumber - 1`, and
            // LineNumber has only moved past this line if that character is
            // its newline.
            let tripping = MAX_GARBAGE + 1 - self.garbage;
            let line = if ends_with_newline && tripping == n {
                lineno
            } else {
                lineno - 1
            };
            self.diagnostics.push(Diagnostic::TooMuchGarbage { line });
            self.garbage_reported = true;
        }
        self.garbage = self.garbage.saturating_add(n);
    }

    /// cronie's `skip_comments` applied to one line: blank and comment lines
    /// are counted as garbage and `None` is returned; for a content line the
    /// leading blanks and first character are counted, the gap ends and the
    /// line without its leading blanks is returned.
    fn skip<'t>(&mut self, raw: &'t str, lineno: usize, terminated: bool) -> Option<&'t str> {
        let line = trim_blanks(raw);
        if line.is_empty() || line.starts_with('#') {
            // Every character, plus the newline (or the EOF read after an
            // unterminated last line).
            let n = byte_len(raw, self.encoding) + 1;
            self.count_garbage(n, lineno, terminated);
            return None;
        }
        // Leading blanks are ASCII, so their byte count is the same in both
        // encodings.
        self.count_garbage(raw.len() - line.len() + 1, lineno, false);
        self.garbage = 0;
        self.garbage_reported = false;
        Some(line)
    }

    fn line(&mut self, raw: &str, lineno: usize) {
        let Some(line) = self.skip(raw, lineno, true) else {
            return;
        };
        // cronie's load_env sees at most MAX_ENVSTR - 1 bytes of the line.
        let env_view = truncate_bytes(line, MAX_ENVSTR - 1, self.encoding);
        if let Some((name, value)) = parse_env_line(env_view) {
            self.env_lines.push(lineno);
            self.state.assign(name, value);
            return;
        }
        self.entry_lines.push(lineno);
        let result = parse_entry(
            line,
            lineno,
            self.options,
            &self.state,
            self.encoding,
            &mut self.diagnostics,
        );
        match result {
            Ok(entry) => self.entries.push(entry),
            Err(error) => self.diagnostics.push(Diagnostic::Error(ParseError {
                line: lineno,
                error,
            })),
        }
    }
}

impl State {
    fn assign(&mut self, name: String, value: String) {
        match name.as_str() {
            "CRON_TZ" => self.cron_tz = Some(value.clone()),
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

/// Number of original input bytes in `s`.
fn byte_len(s: &str, encoding: TextEncoding) -> usize {
    match encoding {
        TextEncoding::Utf8 => s.len(),
        TextEncoding::Latin1 => s.chars().count(),
    }
}

/// The longest prefix of `s` holding at most `max` original input bytes, as
/// cronie's `get_string` keeps. UTF-8 input is cut at a character boundary.
fn truncate_bytes(s: &str, max: usize, encoding: TextEncoding) -> &str {
    match encoding {
        TextEncoding::Utf8 => &s[..s.floor_char_boundary(max)],
        TextEncoding::Latin1 => match s.char_indices().nth(max) {
            Some((i, _)) => &s[..i],
            None => s,
        },
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
    encoding: TextEncoding,
    diagnostics: &mut Vec<Diagnostic>,
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

    let mut warnings = Vec::new();
    let parsed = Schedule::parse_prefix_with(line, &mut rand::rng(), &mut warnings);
    diagnostics.extend(warnings.into_iter().map(|message| {
        Diagnostic::Warning(ParseWarning {
            line: lineno,
            message,
        })
    }));
    let (schedule, rest) = parsed?;
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
            // get_string(username, MAX_COMMAND, ..) keeps MAX_COMMAND - 1 bytes.
            let user = truncate_bytes(c_str(user), MAX_COMMAND - 1, encoding);
            (Some(user.to_string()), remainder)
        }
    };

    // load_entry checks RANDOM_DELAY here, before the job options.
    if state.random_delay == RandomDelay::Invalid {
        diagnostics.push(Diagnostic::BadRandomDelay { line: lineno });
    }

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

    // The command is everything up to the newline, trailing blanks included,
    // limited like get_string(cmd, MAX_COMMAND, ..).
    let raw_command = truncate_bytes(c_str(rest), MAX_COMMAND - 1, encoding).to_string();
    let (command, stdin) = split_command(&raw_command);

    Ok(Entry {
        schedule,
        user,
        command,
        stdin,
        raw_command,
        env: state.env.clone(),
        cron_tz: state.cron_tz.clone(),
        mail_on_failure_only,
        dont_log,
        random_delay: state.random_delay,
        line: lineno,
        encoding,
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
    use std::os::unix::ffi::OsStrExt;

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
        assert_eq!(e.encoding, TextEncoding::Utf8);
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
        assert_eq!(out.first_error_line(), Some(2));
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
        assert_eq!(out.entry_lines, vec![1]);
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
    fn cron_tz_is_kept_as_text() {
        let tab = Crontab::parse(
            "0 9 * * * u\nCRON_TZ=Asia/Tokyo\n0 9 * * * a\nCRON_TZ=\n0 9 * * * b\n0 9 * * * c\n",
            Format::User,
        )
        .unwrap();
        let tzs: Vec<_> = tab.entries.iter().map(|e| e.cron_tz.as_deref()).collect();
        assert_eq!(tzs, vec![None, Some("Asia/Tokyo"), Some(""), Some("")]);
        assert_eq!(env_get(&tab.entries[1].env, "CRON_TZ"), Some("Asia/Tokyo"));
        assert_eq!(env_get(&tab.entries[2].env, "CRON_TZ"), Some(""));
    }

    #[test]
    fn parsing_never_opens_cron_tz_files() {
        let dir = std::env::temp_dir().join(format!(
            "crontab-rs-cron-tz-fifo-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let fifo_str = fifo.to_str().unwrap().to_string();
        let text =
            format!("CRON_TZ=/nonexistent/fifo\n0 0 * * * a\nCRON_TZ={fifo_str}\n0 0 * * * b\n");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Crontab::parse(&text, Format::User));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
        let tab = result.expect("parsing blocked on a CRON_TZ file").unwrap();
        assert_eq!(tab.entries[0].cron_tz.as_deref(), Some("/nonexistent/fifo"));
        assert_eq!(tab.entries[1].cron_tz.as_deref(), Some(fifo_str.as_str()));
    }

    #[test]
    fn parse_bytes_keeps_original_bytes() {
        let options = ParseOptions::new(Format::User);
        let input: &[u8] = b"A=caf\xe9\n0 0 * * * echo caf\xe9 %in\xe9\n";
        let out = Crontab::parse_bytes(input, &options);
        assert!(out.is_valid());
        let e = &out.crontab.entries[0];
        assert_eq!(e.encoding, TextEncoding::Latin1);
        assert_eq!(e.bytes_of(&e.command), b"echo caf\xe9 ");
        assert_eq!(e.bytes_of(e.stdin.as_deref().unwrap()), b"in\xe9\n");
        assert_eq!(e.os_of(&e.raw_command).as_bytes(), b"echo caf\xe9 %in\xe9");
        assert_eq!(e.bytes_of(env_get(&e.env, "A").unwrap()), b"caf\xe9");

        let out = Crontab::parse_bytes("0 0 * * * echo café\n".as_bytes(), &options);
        let e = &out.crontab.entries[0];
        assert_eq!(e.encoding, TextEncoding::Utf8);
        assert_eq!(e.command, "echo café");
        assert_eq!(e.bytes_of(&e.command), "echo café".as_bytes());
        assert_eq!(e.os_of(&e.command).as_bytes(), "echo café".as_bytes());
    }

    #[test]
    fn bad_random_delay_is_reported_where_load_entry_checks_it() {
        let out = user("RANDOM_DELAY=5000\n0 0 * * * -x cmd\n");
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::BadRandomDelay { line: 2 },
                Diagnostic::Error(ParseError {
                    line: 2,
                    error: EntryError::BadOption
                }),
            ]
        );
        assert_eq!(out.until_first_error(), vec![&out.diagnostics[1]]);
        assert_eq!(out.first_error_line(), Some(2));

        let out = user("RANDOM_DELAY=5000\n99 0 * * * x\n");
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::Error(ParseError {
                line: 2,
                error: ScheduleError::BadMinute.into()
            })]
        );

        let sys = ParseOptions::new(Format::System);
        let out = Crontab::parse_with("RANDOM_DELAY=-1\n0 0 * * * root\n", &sys);
        assert_eq!(first_error(&out), Some(EntryError::BadCommand));
        assert_eq!(out.diagnostics.len(), 1);

        let out = user("RANDOM_DELAY=-1\n0 0 * * * x\n@daily y\n");
        assert!(out.is_valid());
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::BadRandomDelay { line: 2 },
                Diagnostic::BadRandomDelay { line: 3 }
            ]
        );
        assert!(out.until_first_error().is_empty());
        assert_eq!(out.first_error_line(), None);
    }

    fn comment(len: usize) -> String {
        format!("#{}\n", "a".repeat(len - 1))
    }

    #[test]
    fn garbage_limit_follows_skip_comments() {
        let job = "0 0 * * * x\n";
        let m = MAX_GARBAGE;
        // A skipped line counts its characters plus its newline; the content
        // line then counts its first character. Oracle: cronie 1.7.2 `crontab -T`.
        let ok = user(&format!("{}{job}", comment(m - 2)));
        assert!(ok.diagnostics.is_empty());
        assert_eq!(ok.first_error_line(), None);

        // One more character trips on the job's first character: cronie
        // prints line 1.
        let out = user(&format!("{}{job}", comment(m - 1)));
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 1 }]
        );
        assert!(out.is_valid());
        assert_eq!(out.crontab.entries.len(), 1);
        assert_eq!(out.first_error_line(), Some(1));

        // Tripping on a comment's newline prints that line.
        let out = user(&format!("x=1\n{}{job}", comment(m)));
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 2 }]
        );
        // Tripping inside a comment prints the line before it.
        let out = user(&format!("x=1\n{}{job}", comment(m + 11)));
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 1 }]
        );
        let out = user(&format!("{}{job}", comment(m + 11)));
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 0 }]
        );

        // Leading blanks of the content line count too.
        let blanks = "\n".repeat(m - 1);
        assert!(user(&format!("{blanks}{job}")).diagnostics.is_empty());
        let out = user(&format!("{blanks} {job}"));
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: m - 1 }]
        );

        // An unterminated final comment counts the EOF read.
        assert!(
            user(&format!("{job}#{}", "a".repeat(m - 2)))
                .diagnostics
                .is_empty()
        );
        let out = user(&format!("{job}#{}", "a".repeat(m - 1)));
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 1 }]
        );

        // Once per gap, and it stops until_first_error.
        let out = user(&format!("{}{}{job}99 * * * * y\n", comment(m), comment(m)));
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::TooMuchGarbage { line: 1 },
                Diagnostic::Error(ParseError {
                    line: 4,
                    error: ScheduleError::BadMinute.into()
                })
            ]
        );
        assert_eq!(out.until_first_error(), vec![&out.diagnostics[0]]);
        assert_eq!(out.first_error_line(), Some(1));
        assert_eq!(out.crontab.entries.len(), 1);

        // Latin-1 input counts bytes.
        let options = ParseOptions::new(Format::User);
        let latin = |n: usize| {
            let mut v = b"#".to_vec();
            v.extend(std::iter::repeat_n(0xe9u8, n - 1));
            v.extend_from_slice(b"\n0 0 * * * x\n");
            Crontab::parse_bytes(&v, &options)
        };
        assert!(latin(m - 2).diagnostics.is_empty());
        assert_eq!(
            latin(m - 1).diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 1 }]
        );
    }

    #[test]
    fn env_and_entry_lines() {
        let out = user(
            "# c\nA=1\n0 0 * * * a\n\n99 * * * * bad\nB=2\n  @daily c\n* * * * * -q x\n0 0 * * * tail",
        );
        assert_eq!(out.env_lines, vec![2, 6]);
        assert_eq!(out.entry_lines, vec![3, 5, 7, 8]);
    }

    #[test]
    fn env_line_is_examined_up_to_max_envstr() {
        let job = "0 0 * * * x\n";
        // `A="` + k + `"` is exactly MAX_ENVSTR - 1 bytes: the junk after the
        // closing quote is cut off and the line is an assignment (oracle: no
        // error). One more byte cuts the closing quote: a bad job.
        let k = MAX_ENVSTR - 1 - 4;
        let out = user(&format!("A=\"{}\" junk\n{job}", "x".repeat(k)));
        assert!(out.is_valid());
        assert_eq!(out.env_lines, vec![1]);
        assert_eq!(
            env_get(&out.crontab.entries[0].env, "A").map(str::len),
            Some(k)
        );
        let out = user(&format!("A=\"{}\" junk\n{job}", "x".repeat(k + 1)));
        assert_eq!(first_error(&out), Some(ScheduleError::BadMinute.into()));
        assert_eq!(out.entry_lines, vec![1, 2]);

        // An unquoted value keeps MAX_ENVSTR - 1 bytes of the whole line.
        let out = user(&format!("  A={}\n{job}", "v".repeat(MAX_ENVSTR)));
        assert_eq!(
            env_get(&out.crontab.entries[0].env, "A").map(str::len),
            Some(MAX_ENVSTR - 1 - 2)
        );
    }

    #[test]
    fn user_and_command_keep_max_command_minus_one_bytes() {
        let mc = MAX_COMMAND;
        let out = user(&format!("0 0 * * * {}\n", "c".repeat(mc + 10)));
        assert_eq!(out.crontab.entries[0].command.len(), mc - 1);

        // The stdin split happens on the truncated command.
        let out = user(&format!("0 0 * * * a%{}\n", "b".repeat(mc)));
        let e = &out.crontab.entries[0];
        assert_eq!(e.raw_command.len(), mc - 1);
        assert_eq!(e.stdin.as_ref().map(String::len), Some(mc - 3 + 1));

        let sys = ParseOptions::new(Format::System);
        let out = Crontab::parse_with(&format!("0 0 * * * {} cmd\n", "u".repeat(mc + 5)), &sys);
        let e = &out.crontab.entries[0];
        assert_eq!(e.user.as_ref().map(String::len), Some(mc - 1));
        assert_eq!(e.command, "cmd");

        // UTF-8 input is cut at a character boundary.
        let out = user(&format!("0 0 * * * {}é\n", "a".repeat(mc - 2)));
        assert_eq!(out.crontab.entries[0].command, "a".repeat(mc - 2));
        // Latin-1 input counts one byte per character.
        let mut bytes = b"0 0 * * * ".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', mc - 2));
        bytes.extend_from_slice(b"\xe9\xe9\n");
        let out = Crontab::parse_bytes(&bytes, &ParseOptions::new(Format::User));
        let e = &out.crontab.entries[0];
        let got = e.bytes_of(&e.command);
        assert_eq!(got.len(), mc - 1);
        assert_eq!(got.last(), Some(&0xe9));
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
