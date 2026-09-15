//! Crontab file parsing, following cronie 1.7.2.
//!
//! The parser is a port of the way cronie's `load_user` reads a crontab: its
//! `skip_comments`, `load_env` and `load_entry` functions working on one
//! stdio stream through `get_char`/`unget_char`/`get_string`, with the same
//! global line counter. Everything is bytes; nothing is decoded.
//!
//! * Lines end at `\n`. Leading spaces and tabs are skipped, and lines
//!   starting with `#` are comments. A final line without a newline is an
//!   error ("premature EOF"); cronie's daemon ignores it.
//! * Environment lines follow cronie's `load_env` state machine, working in
//!   place on a buffer that persists from line to line like cronie's
//!   `envstr` (a quote at the very end of a line reads the stale bytes an
//!   earlier line left behind). The environment starts with the locale,
//!   `RANDOM_DELAY` and `MAILFROM` variables inherited from the parsing
//!   process.
//! * A `-` before the time fields hides the job from the log. Only system
//!   crontabs and root's crontab may use it.
//! * `-n` before the command mails output only when the job fails. It is the
//!   only job option and may appear once.
//! * `%` splits the command from its standard input, as cronie's
//!   `do_command` does.
//! * `CRON_TZ` and `RANDOM_DELAY` apply to the entries after them. `CRON_TZ`
//!   is kept as bytes; the parser never resolves zones or touches the
//!   filesystem.
//! * NUL bytes behave as in cronie. `get_string` stops at a NUL (it is found
//!   by `strchr` in the terminator set) and consumes it, so a NUL ends an
//!   environment assignment, a system crontab's user name and a command, and
//!   reading continues right after it as if a new line started there
//!   (`0 0 * * * a\0 0 0 * * * b` is two jobs). A NUL inside the time fields
//!   is a syntax error.
//! * cronie's buffer limits are ported byte for byte: an environment line is
//!   examined only up to [`MAX_ENVSTR`]` - 1` bytes, a user name and a
//!   command keep at most [`MAX_COMMAND`]` - 1` bytes, and after a bad job
//!   line at most [`MAX_COMMAND`] more characters are discarded (the rest of
//!   a longer line is read as a new line).
//! * More than [`MAX_GARBAGE`] characters of blank/comment content reported
//!   as [`Diagnostic::TooMuchGarbage`]; parsing then continues from the
//!   character right after the one that crossed the limit, as `load_user`
//!   does for system crontabs.
//! * Line numbers in diagnostics are the ones cronie's `crontab` prints,
//!   including its quirks: cronie's `get_number` pushes the character after
//!   a bad word back twice, so a bad word at the end of line N reports line
//!   N - 1.
//!
//! [`Crontab::parse_bytes`] returns every valid entry together with warnings
//! and errors in file order, so the daemon can skip bad lines (as cronie's
//! `load_user` does) while `crontab` stops at the first error (as cronie's
//! `check_syntax` does).

use std::fmt;
use std::os::unix::ffi::OsStrExt;

use crate::schedule::{
    CharStream, EOF, Schedule, ScheduleError, get_string, load_schedule, skip_blanks,
};

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

const NL: i32 = b'\n' as i32;

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

/// A parse error with the line number cronie's `crontab` prints for it.
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
/// uses cronie's wording; `line` is the line the job starts on.
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
    /// The RANDOM_DELAY in effect for the job starting on `line` is out of range. cronie's
    /// daemon logs "bad value of RANDOM_DELAY" at this point in load_entry (after the time
    /// fields and user field parse successfully, before the -n option is parsed); `crontab`
    /// prints nothing for it.
    BadRandomDelay {
        line: usize,
    },
    /// More than MAX_GARBAGE characters of blank/comment content were read in one run of
    /// cronie's skip_comments (it returned FALSE). Reading continues with the next character,
    /// so this can be reported several times.
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
    pub fn from_value(value: &[u8]) -> RandomDelay {
        let value = c_str(value);
        let start = value
            .iter()
            .position(|&b| !is_c_space(b))
            .unwrap_or(value.len());
        let s = &value[start..];
        let (negative, s) = match s.first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
        let n = s[..digits].iter().try_fold(0u64, |n, &b| {
            n.checked_mul(10)?.checked_add(u64::from(b - b'0'))
        });
        match n {
            Some(0) => RandomDelay::Minutes(0),
            Some(_) if negative => RandomDelay::Invalid,
            Some(n) if n <= u64::from(MAX_RANDOM_DELAY) => RandomDelay::Minutes(n as u32),
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
    pub user: Option<Vec<u8>>,
    /// Command with `\%` unescaped and any `%` stdin part removed.
    pub command: Vec<u8>,
    /// Data fed to the command's standard input (from `%` sections).
    pub stdin: Option<Vec<u8>>,
    /// The command text as read (after MAX_COMMAND truncation), before the `%` split.
    pub raw_command: Vec<u8>,
    /// Environment in effect for this entry, in declaration order.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// `CRON_TZ` in effect for this entry: `None` when unset, `Some(b"")` when
    /// explicitly set to the empty string. Never resolved by the parser.
    pub cron_tz: Option<Vec<u8>>,
    /// `-n` option: only mail output when the job fails.
    pub mail_on_failure_only: bool,
    /// Leading `-`: do not log the job.
    pub dont_log: bool,
    /// `RANDOM_DELAY` in effect for this entry.
    pub random_delay: RandomDelay,
    /// 1-based line number the entry starts on.
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
    pub inherited_env: Vec<(Vec<u8>, Vec<u8>)>,
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
/// order, with their exact bytes.
pub fn inherited_process_env() -> Vec<(Vec<u8>, Vec<u8>)> {
    std::env::vars_os()
        .filter(|(k, _)| INHERITED_VARS.iter().any(|n| n.as_bytes() == k.as_bytes()))
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect()
}

/// The result of [`Crontab::parse_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOutput {
    /// Every entry that parsed; bad lines are left out.
    pub crontab: Crontab,
    /// Warnings and errors in file order.
    pub diagnostics: Vec<Diagnostic>,
    /// 1-based line numbers where environment assignments start, in file order.
    pub env_lines: Vec<usize>,
    /// 1-based line numbers where every job, valid or not, starts, in file order
    /// (what cronie's load_user counts against MAX_USER_ENTRIES).
    pub entry_lines: Vec<usize>,
    /// First load_user limit reached, in reading order.
    load_user_stop: Option<(usize, &'static str)>,
    /// Variables and valid entries read when check_syntax's loop stops.
    check_syntax_counts: (usize, usize),
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

    /// cronie's load_user limits for USER crontabs: the first (by line) of too much comment
    /// content, more than MAX_USER_ENTRIES job lines, more than MAX_USER_ENVS variables, as
    /// (line, cronie's log text: "too many garbage characters" | "too many entries" |
    /// "too many environment variables"). `line` is where loading stops.
    ///
    /// "First" is reading order, which is line order except in cronie's line-number quirks.
    /// For garbage `line` is the [`Diagnostic::TooMuchGarbage`] line; for the others it is the
    /// line the surplus job or assignment starts on.
    pub fn load_user_limit(&self) -> Option<(usize, &'static str)> {
        self.load_user_stop
    }

    /// cronie's check_syntax post-loop limits, counting only variables and valid entries read
    /// before the first error/garbage stop: Some("There are too many environment variables in
    /// the crontab file. Limit: 1000") or Some("There are too many entries in the crontab file.
    /// Limit: 10000"), checked in that order.
    pub fn check_syntax_limit(&self) -> Option<&'static str> {
        let (envs, entries) = self.check_syntax_counts;
        if envs > MAX_USER_ENVS {
            Some("There are too many environment variables in the crontab file. Limit: 1000")
        } else if entries > MAX_USER_ENTRIES {
            Some("There are too many entries in the crontab file. Limit: 10000")
        } else {
            None
        }
    }
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

    /// [`parse_bytes`](Self::parse_bytes) on the bytes of `text`.
    pub fn parse_with(text: &str, options: &ParseOptions) -> ParseOutput {
        Self::parse_bytes(text.as_bytes(), options)
    }

    /// Parse a crontab, keeping every valid entry and reporting warnings and
    /// errors in file order.
    pub fn parse_bytes(bytes: &[u8], options: &ParseOptions) -> ParseOutput {
        let mut parser = Parser {
            options,
            r: Reader {
                data: bytes,
                pos: 0,
                back: Vec::new(),
                eof: false,
                line: 1,
            },
            envstr: vec![0; MAX_ENVSTR],
            env: Env::default(),
            entries: Vec::new(),
            diagnostics: Vec::new(),
            env_lines: Vec::new(),
            entry_lines: Vec::new(),
            load_user_stop: None,
            check_syntax_counts: None,
        };
        for (name, value) in &options.inherited_env {
            let mut s = name.clone();
            s.push(b'=');
            s.extend_from_slice(value);
            parser.env.set(c_str(&s));
        }
        parser.run();
        let check_syntax_counts = parser
            .check_syntax_counts
            .unwrap_or((parser.env_lines.len(), parser.entries.len()));
        ParseOutput {
            crontab: Crontab {
                entries: parser.entries,
            },
            diagnostics: parser.diagnostics,
            env_lines: parser.env_lines,
            entry_lines: parser.entry_lines,
            load_user_stop: parser.load_user_stop,
            check_syntax_counts,
        }
    }
}

/// A FILE opened on the crontab, with cronie's global `LineNumber`.
struct Reader<'a> {
    data: &'a [u8],
    /// Offset of the next byte of the file.
    pos: usize,
    /// ungetc pushback that differs from the file content, last pushed last.
    back: Vec<i32>,
    /// feof(): a read hit the end of the file since the last ungetc/fseek.
    eof: bool,
    /// cronie's LineNumber; cronie's quirks can take it to 0.
    line: i64,
}

impl CharStream for Reader<'_> {
    /// cronie's get_char: getc, counting newlines.
    fn get_char(&mut self) -> i32 {
        let ch = if let Some(ch) = self.back.pop() {
            ch
        } else if let Some(&b) = self.data.get(self.pos) {
            self.pos += 1;
            i32::from(b)
        } else {
            self.eof = true;
            return EOF;
        };
        if ch == NL {
            self.line += 1;
        }
        ch
    }

    /// cronie's unget_char: ungetc, uncounting newlines. glibc accepts
    /// several pushbacks; pushing back the byte just read only steps back.
    fn unget_char(&mut self, ch: i32) {
        if ch == EOF {
            return;
        }
        self.eof = false;
        if self.back.is_empty() && self.pos > 0 && i32::from(self.data[self.pos - 1]) == ch {
            self.pos -= 1;
        } else {
            self.back.push(ch);
        }
        if ch == NL {
            self.line -= 1;
        }
    }
}

impl Reader<'_> {
    fn tell(&self) -> usize {
        self.pos.saturating_sub(self.back.len())
    }

    fn seek(&mut self, pos: usize) {
        self.pos = pos;
        self.back.clear();
        self.eof = false;
    }
}

/// cronie's `char **envp`: `NAME=value` strings, where a string without `=`
/// (possible only through load_env's stale-buffer reads) is kept, hidden from
/// lookups, as `(string, None)`.
#[derive(Default)]
struct Env(Vec<(Vec<u8>, Option<Vec<u8>>)>);

impl Env {
    /// cronie's env_set with a C string.
    fn set(&mut self, s: &[u8]) {
        let (name, value) = match s.iter().position(|&b| b == b'=') {
            Some(i) => (&s[..i], Some(s[i + 1..].to_vec())),
            None => (s, None),
        };
        // load_entry replaces these with the job's user; they are left out.
        if PROTECTED_VARS.iter().any(|p| p.as_bytes() == name) {
            return;
        }
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name.to_vec(), value)),
        }
    }

    fn visible(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.0
            .iter()
            .filter_map(|(n, v)| Some((n.clone(), v.clone()?)))
            .collect()
    }
}

struct Parser<'o, 'a> {
    options: &'o ParseOptions,
    r: Reader<'a>,
    /// load_user's `envstr`, which keeps bytes from earlier lines.
    envstr: Vec<u8>,
    env: Env,
    entries: Vec<Entry>,
    diagnostics: Vec<Diagnostic>,
    env_lines: Vec<usize>,
    entry_lines: Vec<usize>,
    load_user_stop: Option<(usize, &'static str)>,
    check_syntax_counts: Option<(usize, usize)>,
}

/// LineNumber as a reported line.
fn line_of(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

impl Parser<'_, '_> {
    /// load_user's loop, continuing past skip_comments failures as it does
    /// for system crontabs.
    fn run(&mut self) {
        loop {
            if !self.skip_comments() {
                let line = line_of(self.r.line - 1);
                self.diagnostics.push(Diagnostic::TooMuchGarbage { line });
                self.stop_check_syntax();
                self.stop_load_user(line, "too many garbage characters");
            }

            // load_env
            let filepos = self.r.tell();
            let fileline = self.r.line;
            let (ch, len) = self.read_envstr();
            if ch == EOF {
                if len > 0 {
                    self.diagnostics.push(Diagnostic::Error(ParseError {
                        line: line_of(self.r.line),
                        error: EntryError::PrematureEof,
                    }));
                    self.stop_check_syntax();
                }
                break;
            }
            let line = line_of(fileline);
            if load_env(&mut self.envstr) {
                self.env_lines.push(line);
                if self.env_lines.len() > MAX_USER_ENVS {
                    self.stop_load_user(line, "too many environment variables");
                }
                let s = c_str(&self.envstr).to_vec();
                self.env.set(&s);
            } else {
                self.r.seek(filepos);
                self.r.line = fileline;
                self.entry_lines.push(line);
                if self.entry_lines.len() > MAX_USER_ENTRIES {
                    self.stop_load_user(line, "too many entries");
                }
                self.load_entry();
            }
        }
    }

    fn stop_check_syntax(&mut self) {
        if self.check_syntax_counts.is_none() {
            self.check_syntax_counts = Some((self.env_lines.len(), self.entries.len()));
        }
    }

    fn stop_load_user(&mut self, line: usize, why: &'static str) {
        if self.load_user_stop.is_none() {
            self.load_user_stop = Some((line, why));
        }
    }

    /// get_string(envstr, MAX_ENVSTR, file, "\n") into the persistent buffer.
    fn read_envstr(&mut self) -> (i32, usize) {
        let mut n = 0;
        loop {
            let ch = self.r.get_char();
            if ch == EOF || ch == 0 || ch == NL {
                self.envstr[n] = 0;
                return (ch, n);
            }
            if n + 1 < MAX_ENVSTR {
                self.envstr[n] = ch as u8;
                n += 1;
            }
        }
    }

    /// cronie's skip_comments.
    fn skip_comments(&mut self) -> bool {
        let is_blank = |ch: i32| ch == i32::from(b' ') || ch == i32::from(b'\t');
        let mut n = 0usize;
        let mut ch;
        loop {
            ch = self.r.get_char();
            if ch == EOF {
                break;
            }
            n += 1;
            if n > MAX_GARBAGE {
                return false;
            }
            while is_blank(ch) {
                ch = self.r.get_char();
                n += 1;
                if n > MAX_GARBAGE {
                    return false;
                }
            }
            if ch == EOF {
                break;
            }
            if ch != NL && ch != i32::from(b'#') {
                break;
            }
            while ch != NL && ch != EOF {
                ch = self.r.get_char();
                n += 1;
                if n > MAX_GARBAGE {
                    return false;
                }
            }
        }
        if ch != EOF {
            self.r.unget_char(ch);
        }
        true
    }

    /// cronie's load_entry, including its discarding of the rest of a bad
    /// line.
    fn load_entry(&mut self) {
        let line = line_of(self.r.line);
        let ch = self.r.get_char();
        if ch == EOF {
            return;
        }
        match self.entry(ch, line) {
            Ok(Some(entry)) => self.entries.push(entry),
            Ok(None) => {}
            Err((error, mut ch)) => {
                let mut i = 0;
                while i < MAX_COMMAND && ch != NL && !self.r.eof {
                    ch = self.r.get_char();
                    i += 1;
                }
                self.diagnostics.push(Diagnostic::Error(ParseError {
                    line: line_of(self.r.line - 1),
                    error,
                }));
                self.stop_check_syntax();
            }
        }
    }

    /// The body of load_entry from its first character; errors carry
    /// cronie's `ch` at `goto eof`.
    fn entry(&mut self, mut ch: i32, line: usize) -> Result<Option<Entry>, (EntryError, i32)> {
        let is_dash = |ch: i32| ch == i32::from(b'-');
        let mut dont_log = false;
        if is_dash(ch) {
            if !self.options.privileged {
                return Err((EntryError::BadOption, ch));
            }
            dont_log = true;
            ch = self.r.get_char();
            if ch == EOF {
                return Ok(None);
            }
        }

        let mut warnings = Vec::new();
        let parsed = load_schedule(ch, &mut self.r, &mut rand::rng(), &mut warnings);
        self.diagnostics.extend(
            warnings
                .into_iter()
                .map(|message| Diagnostic::Warning(ParseWarning { line, message })),
        );
        let (schedule, ch) = parsed.map_err(|(e, ch)| (EntryError::Schedule(e), ch))?;
        // cronie: "check for permature EOL and catch a common typo" (EOF is
        // checked first for @shortcuts).
        if ch == EOF || ch == NL || ch == i32::from(b'*') {
            return Err((EntryError::BadCommand, ch));
        }
        self.r.unget_char(ch);

        let user = match self.options.format {
            Format::User => None,
            Format::System => {
                let (ch, name) = get_string(&mut self.r, MAX_COMMAND, b" \t\n");
                if ch == EOF || ch == NL || ch == i32::from(b'*') {
                    return Err((EntryError::BadCommand, ch));
                }
                let ch = skip_blanks(&mut self.r, ch);
                if ch == EOF || ch == NL {
                    return Err((EntryError::BadCommand, ch));
                }
                self.r.unget_char(ch);
                Some(name)
            }
        };

        let env = self.env.visible();
        let random_delay =
            env_get(&env, b"RANDOM_DELAY").map_or(RandomDelay::Unset, RandomDelay::from_value);
        if random_delay == RandomDelay::Invalid {
            self.diagnostics.push(Diagnostic::BadRandomDelay { line });
        }

        let mut mail_on_failure_only = false;
        let mut ch = self.r.get_char();
        while is_dash(ch) {
            ch = self.r.get_char();
            if ch == i32::from(b'n') && !mail_on_failure_only {
                mail_on_failure_only = true;
            } else {
                return Err((EntryError::BadOption, ch));
            }
            ch = self.r.get_char();
            if ch != i32::from(b'\t') && ch != i32::from(b' ') {
                return Err((EntryError::BadOption, ch));
            }
            ch = skip_blanks(&mut self.r, ch);
            if ch == EOF || ch == NL {
                return Err((EntryError::BadCommand, ch));
            }
        }
        self.r.unget_char(ch);

        let (ch, raw_command) = get_string(&mut self.r, MAX_COMMAND, b"\n");
        if ch == EOF {
            return Err((EntryError::BadCommand, ch));
        }
        let (command, stdin) = split_command(&raw_command);
        let cron_tz = env_get(&env, b"CRON_TZ").map(<[u8]>::to_vec);

        Ok(Some(Entry {
            schedule,
            user,
            command,
            stdin,
            raw_command,
            env,
            cron_tz,
            mail_on_failure_only,
            dont_log,
            random_delay,
            line,
        }))
    }
}

/// Look up a variable in an entry's environment list.
pub fn env_get<'a>(env: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Option<&'a [u8]> {
    env.iter()
        .find(|(n, _)| n.as_slice() == name)
        .map(|(_, v)| v.as_slice())
}

/// C `isspace` in the "C" locale.
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}

/// The part of `s` a C string would see.
fn c_str(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |i| &s[..i])
}

/// cronie's load_env parse of the NUL-terminated string in `buf`, rewriting
/// it in place into `NAME=value` exactly as cronie does (including reading
/// past the terminator after a quote that ends the string). Bytes beyond
/// `buf` read as NUL. Returns whether it is an assignment.
fn load_env(buf: &mut [u8]) -> bool {
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
    let at = |buf: &[u8], i: usize| buf.get(i).copied().unwrap_or(0);
    let put = |buf: &mut [u8], i: usize, b: u8| {
        if let Some(slot) = buf.get_mut(i) {
            *slot = b;
        }
    };

    let (mut c, mut str, mut val) = (0usize, 0usize, 0usize);
    let mut state = S::NameI;
    let mut quote = 0u8;
    while state != S::Error && at(buf, c) != 0 {
        if matches!(state, S::NameI | S::ValueI) {
            if matches!(at(buf, c), b'\'' | b'"') {
                quote = at(buf, c);
                c += 1;
            }
            state = next(state);
            // FALLTHROUGH, even onto the terminating NUL.
        }
        match state {
            S::Name | S::Value => {
                let ch = at(buf, c);
                if quote != 0 {
                    if ch == quote {
                        state = next(state);
                        c += 1;
                        continue;
                    }
                    if state == S::Name && ch == b'=' {
                        state = S::Error;
                        continue;
                    }
                } else if state == S::Name {
                    if is_c_space(ch) {
                        c += 1;
                        state = next(state);
                        continue;
                    }
                    if ch == b'=' {
                        state = next(state);
                        continue;
                    }
                }
                put(buf, str, ch);
                str += 1;
                c += 1;
            }
            S::Eq1 => {
                let ch = at(buf, c);
                if ch == b'=' {
                    state = next(state);
                    quote = 0;
                    put(buf, str, ch);
                    str += 1;
                    val = str;
                } else if !is_c_space(ch) {
                    state = S::Error;
                }
                c += 1;
            }
            S::Eq2 | S::Fini => {
                if is_c_space(at(buf, c)) {
                    c += 1;
                } else {
                    state = next(state);
                }
            }
            S::NameI | S::ValueI | S::Error => unreachable!(),
        }
    }
    if state != S::Fini && state != S::Eq2 && !(state == S::Value && quote == 0) {
        return false;
    }
    put(buf, str, 0);
    if state == S::Value {
        while str > val && is_c_space(at(buf, str - 1)) {
            str -= 1;
            put(buf, str, 0);
        }
    }
    true
}

/// Try to interpret a line as an environment assignment, as cronie's
/// `load_env` does on a fresh buffer after skipping leading blanks.
///
/// The line ends at the first newline or NUL byte. The name runs to the
/// first blank or `=` and may be quoted. The value may be quoted, in which
/// case only whitespace may follow the closing quote; an unquoted value runs
/// to the end of the line with trailing whitespace removed. Returns `None`
/// when the line is not an assignment and would be parsed as a job.
pub fn parse_env_line(line: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let start = line
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(line.len());
    let line = &line[start..];
    let end = line
        .iter()
        .position(|&b| b == 0 || b == b'\n')
        .unwrap_or(line.len());
    let mut buf = line[..end].to_vec();
    buf.push(0);
    if !load_env(&mut buf) {
        return None;
    }
    let s = c_str(&buf);
    let eq = s.iter().position(|&b| b == b'=')?;
    Some((s[..eq].to_vec(), s[eq + 1..].to_vec()))
}

/// Split a raw command as cronie's `do_command` does: the command ends at the
/// first `%` not preceded by a backslash, and `\%` there becomes `%` (other
/// backslashes stay). The text after that `%`, if not empty, is the job's
/// standard input: `%` becomes a newline, `\%` becomes `%`, other backslashes
/// stay, and a newline is added unless the last byte written is one. A NUL
/// ends the command.
pub fn split_command(raw: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let raw = c_str(raw);
    let mut command = Vec::with_capacity(raw.len());
    let mut escaped = false;
    let mut input: &[u8] = &[];
    for (i, &ch) in raw.iter().enumerate() {
        if escaped {
            if ch == b'%' {
                command.pop();
            }
            command.push(ch);
            escaped = false;
            continue;
        }
        if ch == b'%' {
            input = &raw[i + 1..];
            break;
        }
        command.push(ch);
        escaped = ch == b'\\';
    }
    if input.is_empty() {
        return (command, None);
    }

    let mut data = Vec::with_capacity(input.len() + 1);
    let mut need_newline = false;
    let mut escaped = false;
    for &b in input {
        let mut ch = b;
        if escaped {
            if ch != b'%' {
                data.push(b'\\');
            }
        } else if ch == b'%' {
            ch = b'\n';
        }
        escaped = ch == b'\\';
        if !escaped {
            data.push(ch);
            need_newline = ch != b'\n';
        }
    }
    if escaped {
        data.push(b'\\');
    }
    if need_newline {
        data.push(b'\n');
    }
    (command, Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(n: &str, v: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        Some((n.as_bytes().to_vec(), v.as_bytes().to_vec()))
    }

    fn user(text: &str) -> ParseOutput {
        Crontab::parse_with(text, &ParseOptions::new(Format::User))
    }

    fn user_bytes(bytes: &[u8]) -> ParseOutput {
        Crontab::parse_bytes(bytes, &ParseOptions::new(Format::User))
    }

    fn system_bytes(bytes: &[u8]) -> ParseOutput {
        Crontab::parse_bytes(bytes, &ParseOptions::new(Format::System))
    }

    fn first_error(out: &ParseOutput) -> Option<EntryError> {
        out.errors().next().map(|e| e.error.clone())
    }

    fn err(line: usize, error: impl Into<EntryError>) -> Diagnostic {
        Diagnostic::Error(ParseError {
            line,
            error: error.into(),
        })
    }

    fn commands(out: &ParseOutput) -> Vec<&[u8]> {
        out.crontab
            .entries
            .iter()
            .map(|e| e.command.as_slice())
            .collect()
    }

    fn get<'a>(e: &'a Entry, name: &str) -> Option<&'a [u8]> {
        env_get(&e.env, name.as_bytes())
    }

    #[test]
    fn env_lines_follow_load_env() {
        assert_eq!(parse_env_line(b"FOO=bar"), pair("FOO", "bar"));
        assert_eq!(parse_env_line(b"  FOO=bar"), pair("FOO", "bar"));
        assert_eq!(parse_env_line(b"FOO = bar baz  "), pair("FOO", "bar baz"));
        assert_eq!(
            parse_env_line(b"FOO =  spaced value  "),
            pair("FOO", "spaced value")
        );
        assert_eq!(
            parse_env_line(b"FOO=\"  spaced  \""),
            pair("FOO", "  spaced  ")
        );
        assert_eq!(parse_env_line(b"FOO='single' "), pair("FOO", "single"));
        assert_eq!(parse_env_line(b"FOO=a=b"), pair("FOO", "a=b"));
        assert_eq!(parse_env_line(b"FOO=bar # c"), pair("FOO", "bar # c"));
        assert_eq!(parse_env_line(b"MAILTO="), pair("MAILTO", ""));
        assert_eq!(parse_env_line(b"MAILTO=\"\""), pair("MAILTO", ""));
        assert_eq!(parse_env_line(b"1FOO=bar"), pair("1FOO", "bar"));
        assert_eq!(parse_env_line(b"=bar"), pair("", "bar"));
        assert_eq!(parse_env_line(b"\"FOO BAR\"=baz"), pair("FOO BAR", "baz"));
        assert_eq!(parse_env_line(b"FOO=a\0b"), pair("FOO", "a"));
        assert_eq!(
            parse_env_line(b"FOO=caf\xe9"),
            pair("FOO", "caf\u{e9}").map(|_| (b"FOO".to_vec(), b"caf\xe9".to_vec()))
        );
        assert_eq!(parse_env_line(b"FOO\0=bar"), None);
        assert_eq!(parse_env_line(b"FOO='unterminated"), None);
        assert_eq!(parse_env_line(b"FOO=\""), None);
        assert_eq!(parse_env_line(b"\""), None);
        assert_eq!(parse_env_line(b"FOO=\"a\" b"), None);
        assert_eq!(parse_env_line(b"FOO"), None);
        assert_eq!(parse_env_line(b"\"FO=O\"=x"), None);
        assert_eq!(parse_env_line(b"* * * * * echo a=b"), None);
        assert_eq!(parse_env_line(b"FOO bar=1"), None);
    }

    #[test]
    fn env_buffer_keeps_stale_bytes_like_cronie() {
        // Oracle (cronie 1.7.2 `crontab -T`): `B="` alone is a bad job, but
        // after `Q=12xyz"` load_env reads on past its terminator into `xyz"`
        // and accepts it (B is set to the empty string).
        let out = user("Q=12xyz\"\nB=\"\n0 0 * * * x\n");
        assert!(out.is_valid(), "{:?}", out.diagnostics);
        assert_eq!(out.env_lines, vec![1, 2]);
        assert_eq!(get(&out.crontab.entries[0], "B"), Some(&b""[..]));
        let out = user("Q=12xyz\nB=\"\n");
        assert_eq!(out.diagnostics, vec![err(2, ScheduleError::BadMinute)]);
        // A line in between overwrites the stale bytes.
        let out = user("Q=12xyz\"\n0 0 * * * x\nB=\"\n");
        assert_eq!(out.diagnostics, vec![err(3, ScheduleError::BadMinute)]);
        // `"` after `XXab"=1` becomes a string without `=`: accepted, and
        // invisible in the environment.
        let out = user("XXab\"=1\n\"\n0 0 * * * x\n");
        assert!(out.is_valid());
        assert_eq!(out.env_lines, vec![1, 2]);
        assert_eq!(
            out.crontab.entries[0].env,
            vec![(b"XXab\"".to_vec(), b"1".to_vec())]
        );
        assert_eq!(
            user("\"\n").diagnostics,
            vec![err(1, ScheduleError::BadMinute)]
        );
    }

    #[test]
    fn split_percent() {
        let s = |raw: &[u8]| split_command(raw);
        assert_eq!(s(b"echo hi"), (b"echo hi".to_vec(), None));
        assert_eq!(
            s(b"cat%line1%line2"),
            (b"cat".to_vec(), Some(b"line1\nline2\n".to_vec()))
        );
        assert_eq!(s(b"date +\\%Y"), (b"date +%Y".to_vec(), None));
        assert_eq!(
            s(b"cat%100\\%%done%"),
            (b"cat".to_vec(), Some(b"100%\ndone\n".to_vec()))
        );
        // do_command: the byte after a backslash is never a separator, and
        // other backslashes stay.
        assert_eq!(s(b"a\\\\%b"), (b"a\\\\".to_vec(), Some(b"b\n".to_vec())));
        assert_eq!(s(b"a\\x"), (b"a\\x".to_vec(), None));
        // An empty stdin part feeds nothing.
        assert_eq!(s(b"cat%"), (b"cat".to_vec(), None));
        assert_eq!(s(b"x%a\\"), (b"x".to_vec(), Some(b"a\\\n".to_vec())));
        assert_eq!(s(b"x%a%\\"), (b"x".to_vec(), Some(b"a\n\\".to_vec())));
        assert_eq!(s(b"x%\\\\y"), (b"x".to_vec(), Some(b"\\\\y\n".to_vec())));
    }

    #[test]
    fn user_crontab() {
        let text = "# comment\nSHELL=/bin/bash\nMAILTO=alice\n  \t\n  # indented\n*/5 * * * * /usr/bin/backup --quick\nLOGNAME=evil\n0 3 * * 1 -n /usr/bin/backup --full\n\t@reboot echo booted\n";
        let tab = Crontab::parse(text, Format::User).unwrap();
        assert_eq!(tab.entries.len(), 3);
        let e = &tab.entries[0];
        assert_eq!(
            (e.command.as_slice(), e.user.clone(), e.line),
            (&b"/usr/bin/backup --quick"[..], None, 6)
        );
        assert_eq!(
            e.env,
            vec![
                (b"SHELL".to_vec(), b"/bin/bash".to_vec()),
                (b"MAILTO".to_vec(), b"alice".to_vec())
            ]
        );
        let e = &tab.entries[1];
        assert!(e.mail_on_failure_only && !e.dont_log);
        assert_eq!(get(e, "LOGNAME"), None);
        assert!(tab.entries[2].schedule.is_reboot());
    }

    #[test]
    fn bad_lines_are_skipped_and_reported_in_order() {
        let out = user("0 0 * * * a\n*/61 99 * * * b\n0 0 * * * c\n88 * * * * d\n");
        assert_eq!(commands(&out), vec![b"a", b"c"]);
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::Warning(ParseWarning {
                    line: 2,
                    message: "Warning: Step size 61 higher than possible maximum of 59".into()
                }),
                err(2, ScheduleError::BadHour),
                err(4, ScheduleError::BadMinute),
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
    fn bad_word_at_end_of_line_reports_previous_line() {
        // Oracle: cronie 1.7.2 `crontab -T`. get_number pushes the newline
        // after a word that is neither a number nor a name back twice.
        let first = |t: &str| user(t).diagnostics[0].clone();
        assert_eq!(first("0 0 * * x\n"), err(0, ScheduleError::BadDayOfWeek));
        assert_eq!(
            first("0 0 * * * x\n0 0 * * x\n0 0 * * * y\n"),
            err(1, ScheduleError::BadDayOfWeek)
        );
        assert_eq!(
            first("0 0 * * * x\n1 2 3 4 abc\n"),
            err(1, ScheduleError::BadDayOfWeek)
        );
        assert_eq!(
            first("0 0 * * * x\n\n\n0 0 * jan,xyz\n"),
            err(3, ScheduleError::BadMonth)
        );
        // Not at the end of the line: no effect.
        assert_eq!(
            first("0 0 * * * x\n*/5x * * * *\n"),
            err(2, ScheduleError::BadMinute)
        );
        assert_eq!(
            first("0 0 * * * x\n*/q * * * * y\n"),
            err(2, ScheduleError::BadMinute)
        );
        // Following lines keep their numbers.
        let out = user("0 0 * * x\n0 0 * * * y\n99 * * * * z\n");
        assert_eq!(commands(&out), vec![b"y"]);
        assert_eq!(out.crontab.entries[0].line, 2);
        assert_eq!(
            out.diagnostics,
            vec![
                err(0, ScheduleError::BadDayOfWeek),
                err(3, ScheduleError::BadMinute)
            ]
        );
    }

    #[test]
    fn missing_final_newline_is_premature_eof() {
        let out = user("0 0 * * * a\n0 0 * * * b");
        assert_eq!(out.crontab.entries.len(), 1);
        assert_eq!(out.diagnostics, vec![err(2, EntryError::PrematureEof)]);
        assert_eq!(out.entry_lines, vec![1]);
        // Oracle: `abc` without a newline is "premature EOF" on line 1.
        assert_eq!(
            user("abc").diagnostics,
            vec![err(1, EntryError::PrematureEof)]
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
        for line in [
            "17 * * * *\n",
            "17 * * * * root\n",
            "17 * * * * root   \n",
            "@hourly * x\n",
        ] {
            assert_eq!(
                first_error(&system_bytes(line.as_bytes())),
                Some(EntryError::BadCommand),
                "{line:?}"
            );
        }
    }

    #[test]
    fn commands_keep_trailing_blanks() {
        let tab =
            Crontab::parse("0 0 * * * echo hi  \n0 0 * * * cat%in  \n", Format::User).unwrap();
        assert_eq!(tab.entries[0].command, b"echo hi  ");
        assert_eq!(tab.entries[1].stdin.as_deref(), Some(&b"in  \n"[..]));
        assert_eq!(tab.entries[1].raw_command, b"cat%in  ");
    }

    #[test]
    fn nul_bytes_follow_cronie_user_crontabs() {
        // Oracle: cronie 1.7.2 `crontab -T` for the diagnostics and
        // `cronnext -c` (load_user) for the jobs.

        // A NUL ends the command; the rest of the line is read as a new line.
        let out = user_bytes(b"0 0 * * * a\0b\n");
        assert_eq!(commands(&out), vec![b"a"]);
        assert_eq!(out.diagnostics, vec![err(0, ScheduleError::BadMinute)]);
        assert_eq!(out.entry_lines, vec![1, 1]);

        let out =
            user_bytes(b"0 0 * * * a\0 0 0 * * * b\nFOO=x\0 0 0 * * * c%in\n0 0 * * * d\0e\n");
        assert_eq!(commands(&out), vec![b"a", b"b", b"c", b"d"]);
        let lines: Vec<usize> = out.crontab.entries.iter().map(|e| e.line).collect();
        assert_eq!(lines, vec![1, 1, 2, 3]);
        assert_eq!(out.crontab.entries[2].raw_command, b"c%in");
        assert_eq!(get(&out.crontab.entries[2], "FOO"), Some(&b"x"[..]));
        assert_eq!(out.env_lines, vec![2]);
        assert_eq!(out.diagnostics, vec![err(2, ScheduleError::BadMinute)]);

        // Both jobs of every line count against the check_syntax limit:
        // 5000 such lines are 10000 jobs; one more job is too many.
        let two = b"0 0 * * * a\0 0 0 * * * b\n".repeat(5000);
        let out = user_bytes(&two);
        assert!(out.is_valid());
        assert_eq!(out.crontab.entries.len(), 10000);
        assert_eq!(out.check_syntax_limit(), None);
        let mut more = two.clone();
        more.extend_from_slice(b"0 0 * * * c\n");
        assert_eq!(
            user_bytes(&more).check_syntax_limit(),
            Some("There are too many entries in the crontab file. Limit: 10000")
        );
        // A NUL ends an assignment too.
        let mut envs = b"A=1\0B=2\n".repeat(500);
        envs.extend_from_slice(b"0 0 * * * c\n");
        let out = user_bytes(&envs);
        assert_eq!(out.env_lines.len(), 1000);
        assert_eq!(out.check_syntax_limit(), None);
        let mut envs = b"A=1\0B=2\n".repeat(500);
        envs.extend_from_slice(b"C=1\n0 0 * * * c\n");
        assert_eq!(
            user_bytes(&envs).check_syntax_limit(),
            Some("There are too many environment variables in the crontab file. Limit: 1000")
        );

        // A command ended by a NUL has a terminator, so a missing final
        // newline is not reported; a line starting with NUL is not content.
        let out = user_bytes(b"0 0 * * * a\0");
        assert!(out.diagnostics.is_empty());
        assert_eq!(commands(&out), vec![b"a"]);
        assert_eq!(
            user_bytes(b"\0abc").diagnostics,
            vec![err(0, ScheduleError::BadMinute)]
        );

        // NUL inside the time fields, a name or an option.
        assert_eq!(
            user_bytes(b"0\0 0 * * * x\n").diagnostics,
            vec![err(1, ScheduleError::BadMinute)]
        );
        assert_eq!(
            user_bytes(b"FOO\0=bar\n").diagnostics,
            vec![err(1, ScheduleError::BadMinute)]
        );
        assert_eq!(
            user_bytes(b"* * * * * -n\0x\n").diagnostics,
            vec![err(1, EntryError::BadOption)]
        );
        // A shortcut ended by NUL has an empty command.
        let out = user_bytes(b"0 0 * * * x\n@daily\0\n");
        assert!(out.is_valid());
        assert_eq!(commands(&out), vec![&b"x"[..], &b""[..]]);
    }

    #[test]
    fn nul_bytes_follow_cronie_system_crontabs() {
        // Oracle: cronie 1.7.2 `cronnext -c` (load_user) on /etc/cron.d.
        let out = system_bytes(b"0 0 * * * ro\0ot cmd1\n0 0 * * * root x\0 0 0 * * * root y\n");
        let jobs: Vec<(&[u8], &[u8])> = out
            .crontab
            .entries
            .iter()
            .map(|e| (e.user.as_deref().unwrap(), e.command.as_slice()))
            .collect();
        assert_eq!(
            jobs,
            vec![
                (&b"ro"[..], &b""[..]),
                (&b"root"[..], &b"x"[..]),
                (&b"root"[..], &b"y"[..])
            ]
        );
        // "ot cmd1" is read as a line of its own.
        assert_eq!(out.diagnostics, vec![err(1, ScheduleError::BadMinute)]);
        assert_eq!(out.entry_lines, vec![1, 1, 2, 2]);
    }

    #[test]
    fn bad_line_discards_at_most_max_command_characters() {
        // load_entry discards MAX_COMMAND characters after a bad field; the
        // rest of the line is read as a new line. Oracle: `cronnext -c`
        // loads the "tail" job.
        let mut line = b"99 ".to_vec();
        line.extend(std::iter::repeat_n(b'y', MAX_COMMAND - 1));
        line.extend_from_slice(b"0 0 * * * tail\n");
        let out = user_bytes(&line);
        assert_eq!(commands(&out), vec![b"tail"]);
        assert_eq!(out.diagnostics, vec![err(0, ScheduleError::BadMinute)]);
    }

    #[test]
    fn job_options() {
        let out = user("* * * * * -n\tx\n");
        assert!(out.crontab.entries[0].mail_on_failure_only);
        assert_eq!(out.crontab.entries[0].command, b"x");
        for bad in [
            "* * * * * -n -n x\n",
            "* * * * * -nx\n",
            "* * * * * -q x\n",
            "* * * * * -n\n",
            "* * * * * -x y\n",
        ] {
            assert_eq!(
                first_error(&user(bad)),
                Some(EntryError::BadOption),
                "{bad:?}"
            );
        }
        assert_eq!(
            first_error(&user("* * * * * -n   \n")),
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
        assert_eq!(tab.entries[0].user.as_deref(), Some(&b"root"[..]));
        assert_eq!(tab.entries[1].user.as_deref(), Some(&b"nosuchuser"[..]));
        let tab = Crontab::parse("0 0 * * * root -n x\n", Format::System).unwrap();
        assert!(tab.entries[0].mail_on_failure_only);
    }

    #[test]
    fn cron_tz_is_kept_as_bytes() {
        let tab = Crontab::parse(
            "0 9 * * * u\nCRON_TZ=Asia/Tokyo\n0 9 * * * a\nCRON_TZ=\n0 9 * * * b\n0 9 * * * c\n",
            Format::User,
        )
        .unwrap();
        let tzs: Vec<_> = tab.entries.iter().map(|e| e.cron_tz.as_deref()).collect();
        assert_eq!(
            tzs,
            vec![
                None,
                Some(&b"Asia/Tokyo"[..]),
                Some(&b""[..]),
                Some(&b""[..])
            ]
        );
        assert_eq!(get(&tab.entries[1], "CRON_TZ"), Some(&b"Asia/Tokyo"[..]));
        assert_eq!(get(&tab.entries[2], "CRON_TZ"), Some(&b""[..]));
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
        assert_eq!(
            tab.entries[0].cron_tz.as_deref(),
            Some(&b"/nonexistent/fifo"[..])
        );
        assert_eq!(tab.entries[1].cron_tz.as_deref(), Some(fifo_str.as_bytes()));
    }

    #[test]
    fn non_utf8_bytes_are_preserved() {
        let input: &[u8] = b"A=caf\xe9\nCRON_TZ=Z\xff\n0 0 * * * us\xe9r echo caf\xe9 %in\xe9\n";
        let out = system_bytes(input);
        assert!(out.is_valid());
        let e = &out.crontab.entries[0];
        assert_eq!(e.user.as_deref(), Some(&b"us\xe9r"[..]));
        assert_eq!(e.command, b"echo caf\xe9 ");
        assert_eq!(e.stdin.as_deref(), Some(&b"in\xe9\n"[..]));
        assert_eq!(e.raw_command, b"echo caf\xe9 %in\xe9");
        assert_eq!(get(e, "A"), Some(&b"caf\xe9"[..]));
        assert_eq!(e.cron_tz.as_deref(), Some(&b"Z\xff"[..]));

        let out = Crontab::parse_with("0 0 * * * echo café\n", &ParseOptions::new(Format::User));
        assert_eq!(out.crontab.entries[0].command, "echo café".as_bytes());
    }

    #[test]
    fn bad_random_delay_is_reported_where_load_entry_checks_it() {
        let out = user("RANDOM_DELAY=5000\n0 0 * * * -x cmd\n");
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::BadRandomDelay { line: 2 },
                err(2, EntryError::BadOption),
            ]
        );
        assert_eq!(out.until_first_error(), vec![&out.diagnostics[1]]);
        assert_eq!(out.first_error_line(), Some(2));

        let out = user("RANDOM_DELAY=5000\n99 0 * * * x\n");
        assert_eq!(out.diagnostics, vec![err(2, ScheduleError::BadMinute)]);

        let out = system_bytes(b"RANDOM_DELAY=-1\n0 0 * * * root\n");
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
        let tmg = |line| Diagnostic::TooMuchGarbage { line };
        // A skipped line counts its characters plus its newline; the content
        // line then counts its first character. Oracle: cronie 1.7.2 `crontab -T`
        // for the first diagnostic of each file.
        let ok = user(&format!("{}{job}", comment(m - 2)));
        assert!(ok.diagnostics.is_empty());
        assert_eq!(ok.first_error_line(), None);

        // One more character trips on the job's first character: cronie
        // prints line 1. Reading continues after that `0`.
        let out = user(&format!("{}{job}", comment(m - 1)));
        assert_eq!(
            out.diagnostics,
            vec![tmg(1), err(2, ScheduleError::BadMinute)]
        );
        assert!(out.crontab.entries.is_empty());
        assert_eq!(out.first_error_line(), Some(1));
        assert_eq!(out.until_first_error(), vec![&tmg(1)]);

        // Tripping on a comment's newline prints that line.
        let out = user(&format!("x=1\n{}{job}", comment(m)));
        assert_eq!(out.diagnostics, vec![tmg(2)]);
        assert_eq!(out.crontab.entries.len(), 1);
        // Tripping inside a comment prints the line before it; the rest of
        // the comment is then a bad job ending in a bad word.
        let out = user(&format!("x=1\n{}{job}", comment(m + 11)));
        assert_eq!(
            out.diagnostics,
            vec![tmg(1), err(1, ScheduleError::BadMinute)]
        );
        assert_eq!(out.crontab.entries[0].line, 3);
        let out = user(&format!("{}{job}", comment(m + 11)));
        assert_eq!(
            out.diagnostics,
            vec![tmg(0), err(0, ScheduleError::BadMinute)]
        );

        // Leading blanks of the content line count too.
        let blanks = "\n".repeat(m - 1);
        assert!(user(&format!("{blanks}{job}")).diagnostics.is_empty());
        let out = user(&format!("{blanks} {job}"));
        assert_eq!(
            out.diagnostics,
            vec![tmg(m - 1), err(m, ScheduleError::BadMinute)]
        );

        // An unterminated final comment counts the EOF read.
        assert!(
            user(&format!("{job}#{}", "a".repeat(m - 2)))
                .diagnostics
                .is_empty()
        );
        let out = user(&format!("{job}#{}", "a".repeat(m - 1)));
        assert_eq!(out.diagnostics, vec![tmg(1)]);

        // Every trip is reported.
        let out = user(&format!("{}{}{job}99 * * * * y\n", comment(m), comment(m)));
        assert_eq!(
            out.diagnostics,
            vec![
                tmg(1),
                err(2, ScheduleError::BadMinute),
                err(4, ScheduleError::BadMinute)
            ]
        );
        assert_eq!(out.until_first_error(), vec![&out.diagnostics[0]]);
        assert_eq!(out.first_error_line(), Some(1));
        assert_eq!(out.crontab.entries.len(), 1);

        // Bytes, not characters, are counted.
        let latin = |n: usize| {
            let mut v = b"#".to_vec();
            v.extend(std::iter::repeat_n(0xe9u8, n - 1));
            v.extend_from_slice(b"\n0 0 * * * x\n");
            user_bytes(&v)
        };
        assert!(latin(m - 2).diagnostics.is_empty());
        assert_eq!(latin(m - 1).diagnostics[0], tmg(1));
    }

    #[test]
    fn system_crontab_reading_continues_after_garbage() {
        // Oracle: cronie 1.7.2 `cronnext -c` (load_user on /etc/cron.d)
        // loads both jobs; the diagnostics follow skip_comments/load_entry.

        // The limit trips on `Z` (inside line 1, so cronie's number is 0);
        // reading resumes right after it, mid-line.
        let mut hidden = b"#".to_vec();
        hidden.extend(std::iter::repeat_n(b'x', MAX_GARBAGE - 1));
        hidden.extend_from_slice(b"Z* * * * * joseph touch /tmp/hidden-ran\n");
        let out = system_bytes(&hidden);
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::TooMuchGarbage { line: 0 }]
        );
        let e = &out.crontab.entries[0];
        assert_eq!(e.user.as_deref(), Some(&b"joseph"[..]));
        assert_eq!(e.command, b"touch /tmp/hidden-ran");
        assert_eq!(e.line, 1);

        // 40000 blank lines: the limit trips on the newline ending line
        // 32769, the next empty line is a bad job ("bad minute"), and the
        // count starts again for the rest.
        let mut blanks = b"\n".repeat(40000);
        blanks.extend_from_slice(b"* * * * * joseph blanks-job\n");
        let out = system_bytes(&blanks);
        assert_eq!(
            out.diagnostics,
            vec![
                Diagnostic::TooMuchGarbage {
                    line: MAX_GARBAGE + 1
                },
                err(MAX_GARBAGE + 2, ScheduleError::BadMinute)
            ]
        );
        assert_eq!(out.entry_lines, vec![MAX_GARBAGE + 2, 40001]);
        assert_eq!(commands(&out), vec![b"blanks-job"]);
        assert_eq!(out.crontab.entries[0].line, 40001);

        // A comment long enough to trip twice.
        let mut long = b"#".to_vec();
        long.extend(std::iter::repeat_n(b'#', 2 * MAX_GARBAGE + 5));
        long.extend_from_slice(b"\n0 0 * * * root x\n");
        let out = system_bytes(&long);
        assert_eq!(out.diagnostics[0], Diagnostic::TooMuchGarbage { line: 0 });
        assert_eq!(commands(&out), vec![b"x"]);
    }

    #[test]
    fn load_user_limit_reports_the_first_limit_reached() {
        assert_eq!(user("0 0 * * * x\n").load_user_limit(), None);

        let out = user(&format!(
            "{}{}",
            "A=1\n".repeat(MAX_USER_ENVS),
            "0 0 * * * x\n"
        ));
        assert_eq!(out.load_user_limit(), None);
        let out = user(&"A=1\n".repeat(MAX_USER_ENVS + 1));
        assert_eq!(
            out.load_user_limit(),
            Some((MAX_USER_ENVS + 1, "too many environment variables"))
        );

        // Bad jobs count too.
        let jobs = "99 * * * * x\n".repeat(MAX_USER_ENTRIES);
        assert_eq!(user(&jobs).load_user_limit(), None);
        let out = user(&format!("{jobs}0 0 * * * y\nA=1\n"));
        assert_eq!(
            out.load_user_limit(),
            Some((MAX_USER_ENTRIES + 1, "too many entries"))
        );

        let garbage = format!("0 0 * * * x\n{}", comment(MAX_GARBAGE));
        assert_eq!(
            user(&garbage).load_user_limit(),
            Some((2, "too many garbage characters"))
        );
        // Garbage after the surplus job is not reached.
        let out = user(&format!("{jobs}0 0 * * * y\n{}", comment(MAX_GARBAGE)));
        assert_eq!(
            out.load_user_limit(),
            Some((MAX_USER_ENTRIES + 1, "too many entries"))
        );
    }

    #[test]
    fn check_syntax_limit_counts_until_the_first_stop() {
        let envs = "A=1\n".repeat(MAX_USER_ENVS + 1);
        assert_eq!(
            user(&envs).check_syntax_limit(),
            Some("There are too many environment variables in the crontab file. Limit: 1000")
        );
        let jobs = "0 0 * * * x\n".repeat(MAX_USER_ENTRIES + 1);
        assert_eq!(
            user(&jobs).check_syntax_limit(),
            Some("There are too many entries in the crontab file. Limit: 10000")
        );
        // Variables are checked first.
        assert_eq!(
            user(&format!("{envs}{jobs}")).check_syntax_limit(),
            Some("There are too many environment variables in the crontab file. Limit: 1000")
        );
        // Bad jobs are not counted; nothing after the first error is.
        let bad = "99 * * * * x\n".repeat(MAX_USER_ENTRIES + 1);
        assert_eq!(user(&bad).check_syntax_limit(), None);
        assert_eq!(
            user(&format!("99 * * * * x\n{jobs}{envs}")).check_syntax_limit(),
            None
        );
        assert_eq!(
            user(&format!("{}{jobs}", comment(MAX_GARBAGE))).check_syntax_limit(),
            None
        );
        // An error at the very end still leaves the counts above the limit.
        assert_eq!(
            user(&format!("{jobs}99 * * * * x\n")).check_syntax_limit(),
            Some("There are too many entries in the crontab file. Limit: 10000")
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
        assert_eq!(get(&out.crontab.entries[0], "A").map(<[u8]>::len), Some(k));
        let out = user(&format!("A=\"{}\" junk\n{job}", "x".repeat(k + 1)));
        assert_eq!(first_error(&out), Some(ScheduleError::BadMinute.into()));
        // `A=` pushes `=` back twice; MAX_COMMAND discarded characters end
        // before " junk", which is then read as another bad job.
        assert_eq!(
            out.diagnostics,
            vec![
                err(0, ScheduleError::BadMinute),
                err(0, ScheduleError::BadMinute)
            ]
        );
        assert_eq!(out.entry_lines, vec![1, 1, 2]);

        // An unquoted value keeps MAX_ENVSTR - 1 bytes of the whole line.
        let out = user(&format!("  A={}\n{job}", "v".repeat(MAX_ENVSTR)));
        assert_eq!(
            get(&out.crontab.entries[0], "A").map(<[u8]>::len),
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
        assert_eq!(e.stdin.as_ref().map(Vec::len), Some(mc - 3 + 1));

        let out = system_bytes(format!("0 0 * * * {} cmd\n", "u".repeat(mc + 5)).as_bytes());
        let e = &out.crontab.entries[0];
        assert_eq!(e.user.as_ref().map(Vec::len), Some(mc - 1));
        assert_eq!(e.command, b"cmd");

        // Truncation counts bytes, even inside a UTF-8 character.
        let out = user(&format!("0 0 * * * {}é\n", "a".repeat(mc - 2)));
        let got = &out.crontab.entries[0].command;
        assert_eq!(got.len(), mc - 1);
        assert_eq!(got.last(), Some(&0xc3));
    }

    #[test]
    fn random_delay() {
        use RandomDelay::*;
        let v = |s: &str| RandomDelay::from_value(s.as_bytes());
        assert_eq!(v("15"), Minutes(15));
        assert_eq!(v(" 15"), Minutes(15));
        assert_eq!(v("10abc"), Minutes(10));
        assert_eq!(v("abc"), Minutes(0));
        assert_eq!(v("+7"), Minutes(7));
        assert_eq!(v("1440"), Minutes(1440));
        assert_eq!(v("1441"), Invalid);
        assert_eq!(v("-1"), Invalid);
        assert_eq!(v("-0"), Minutes(0));
        assert_eq!(v("99999999999999999999"), Invalid);
        assert_eq!(RandomDelay::from_value(b"12\xff"), Minutes(12));

        let tab = Crontab::parse("0 0 * * * before\nRANDOM_DELAY=30\n0 0 * * * after\nRANDOM_DELAY=5000\n0 0 * * * bad\n", Format::User).unwrap();
        let delays: Vec<_> = tab.entries.iter().map(|e| e.random_delay).collect();
        assert_eq!(delays, vec![Unset, Minutes(30), Invalid]);
    }

    #[test]
    fn inherited_environment_comes_first() {
        let options = ParseOptions {
            inherited_env: vec![
                (b"RANDOM_DELAY".to_vec(), b"20".to_vec()),
                (b"LANG".to_vec(), b"C.UTF-8".to_vec()),
                (b"LC_NAME".to_vec(), b"x\xff".to_vec()),
            ],
            ..ParseOptions::new(Format::User)
        };
        let out = Crontab::parse_with("0 0 * * * a\nLANG=de_DE.UTF-8\n0 0 * * * b\n", &options);
        let e = &out.crontab.entries;
        assert_eq!(e[0].random_delay, RandomDelay::Minutes(20));
        assert_eq!(get(&e[0], "LANG"), Some(&b"C.UTF-8"[..]));
        assert_eq!(get(&e[0], "LC_NAME"), Some(&b"x\xff"[..]));
        assert_eq!(get(&e[1], "LANG"), Some(&b"de_DE.UTF-8"[..]));
        assert!(INHERITED_VARS.contains(&"MAILFROM"));
    }

    #[test]
    fn inherited_process_env_keeps_non_utf8_values() {
        use std::ffi::OsStr;
        const VAR: &str = "LC_IDENTIFICATION";
        let saved = std::env::var_os(VAR);
        // SAFETY: only this test touches LC_IDENTIFICATION, and no test reads
        // the C locale from it.
        unsafe { std::env::set_var(VAR, OsStr::from_bytes(b"ab\xff\xfe")) };
        let env = inherited_process_env();
        match saved {
            // SAFETY: as above.
            Some(v) => unsafe { std::env::set_var(VAR, v) },
            None => unsafe { std::env::remove_var(VAR) },
        }
        assert!(env.contains(&(VAR.as_bytes().to_vec(), b"ab\xff\xfe".to_vec())));
        assert!(
            env.iter()
                .all(|(k, _)| INHERITED_VARS.iter().any(|n| n.as_bytes() == k.as_slice()))
        );
    }

    #[test]
    fn later_env_overrides_earlier() {
        let tab = Crontab::parse("A=1\nA=2\n* * * * * x\n", Format::User).unwrap();
        assert_eq!(tab.entries[0].env, vec![(b"A".to_vec(), b"2".to_vec())]);
    }
}
