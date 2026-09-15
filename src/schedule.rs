//! Cron time-specification parsing and matching, following cronie 1.7.2.
//!
//! A schedule has five fields (minute, hour, day-of-month, month, day-of-week)
//! or one of the `@reboot`, `@yearly`, `@annually`, `@monthly`, `@weekly`,
//! `@daily`, `@midnight`, `@hourly` shortcuts. Shortcut names are
//! case-sensitive.
//!
//! Field grammar, ported from cronie's `get_list`/`get_range`:
//!
//! ```text
//! list   := range {"," range}
//! range  := "*" ["/" step]
//!         | number ["-" number ["/" step]]
//!         | [number] "~" [number]
//! number := digits | name
//! ```
//!
//! * Names are the exact three-letter month or weekday abbreviations,
//!   matched case-insensitively.
//! * Numbers behave like cronie's `(int) strtol`: huge values saturate and
//!   then keep their low 32 bits, so `4294967296` is 0.
//! * A step may only follow `*` or an `a-b` range, and must not be 0. A step
//!   larger than its range is accepted with a warning.
//! * A reversed range such as `5-3` is accepted but selects no values. Every
//!   value a range actually selects must be within the field.
//! * `~` picks one random value when the crontab is loaded; like cronie, the
//!   pick itself must be within the field.
//! * Day-of-week `7` is Sunday.
//!
//! Day-of-month and day-of-week follow cronie's rule: when either field
//! starts with `*`, both must match; otherwise either may match.

use std::fmt;

use chrono::{Datelike, NaiveDateTime, Timelike};
use rand::{Rng, RngExt};

use crate::crontab::MAX_COMMAND;

/// Size of cronie's `get_number` buffer: a number or name token of this many
/// characters or more is rejected.
pub const MAX_TEMPSTR: usize = 131072;

/// Error produced while parsing a time specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    BadMinute,
    BadHour,
    BadDayOfMonth,
    BadMonth,
    BadDayOfWeek,
    BadTimeSpecifier,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ScheduleError::BadMinute => "bad minute",
            ScheduleError::BadHour => "bad hour",
            ScheduleError::BadDayOfMonth => "bad day-of-month",
            ScheduleError::BadMonth => "bad month",
            ScheduleError::BadDayOfWeek => "bad day-of-week",
            ScheduleError::BadTimeSpecifier => "bad time specifier",
        };
        f.write_str(s)
    }
}

impl std::error::Error for ScheduleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

const MONTH_NAMES: &[&str] = &[
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DOW_NAMES: &[&str] = &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

impl Field {
    fn bounds(self) -> (i32, i32) {
        match self {
            Field::Minute => (0, 59),
            Field::Hour => (0, 23),
            Field::DayOfMonth => (1, 31),
            Field::Month => (1, 12),
            Field::DayOfWeek => (0, 7),
        }
    }

    fn names(self) -> Option<&'static [&'static str]> {
        match self {
            Field::Month => Some(MONTH_NAMES),
            Field::DayOfWeek => Some(DOW_NAMES),
            _ => None,
        }
    }

    fn error(self) -> ScheduleError {
        match self {
            Field::Minute => ScheduleError::BadMinute,
            Field::Hour => ScheduleError::BadHour,
            Field::DayOfMonth => ScheduleError::BadDayOfMonth,
            Field::Month => ScheduleError::BadMonth,
            Field::DayOfWeek => ScheduleError::BadDayOfWeek,
        }
    }
}

/// A parsed cron time specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u32,
    days_of_month: u32,
    months: u16,
    days_of_week: u8,
    /// Field started with `*` (with or without a step).
    minute_star: bool,
    hour_star: bool,
    dom_star: bool,
    dow_star: bool,
    /// `@reboot`: run once when the daemon starts.
    reboot: bool,
}

/// cronie's `Skip_Blanks` characters.
pub(crate) fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Strip leading spaces and tabs.
pub(crate) fn trim_blanks(s: &str) -> &str {
    s.trim_start_matches(is_blank)
}

/// C's `EOF` as returned by `getc`.
pub(crate) const EOF: i32 = -1;
const NL: i32 = b'\n' as i32;

/// A C stdio stream as cronie's parser reads it: `get_char` returns a byte
/// (0..=255) or [`EOF`], and `unget_char` pushes one back (a no-op for `EOF`).
pub(crate) trait CharStream {
    fn get_char(&mut self) -> i32;
    fn unget_char(&mut self, ch: i32);
}

/// cronie's `Skip_Blanks` macro.
pub(crate) fn skip_blanks<S: CharStream>(s: &mut S, mut ch: i32) -> i32 {
    while ch == i32::from(b' ') || ch == i32::from(b'\t') {
        ch = s.get_char();
    }
    ch
}

/// cronie's `get_string(str, size, file, terms)`: reads up to (and consuming)
/// EOF, a byte in `terms` or a NUL byte (`strchr` finds the terminator of
/// `terms`), keeping at most `size - 1` bytes. Returns the terminator and the
/// kept bytes.
pub(crate) fn get_string<S: CharStream>(s: &mut S, size: usize, terms: &[u8]) -> (i32, Vec<u8>) {
    let mut out = Vec::new();
    loop {
        let ch = s.get_char();
        if ch == EOF || ch == 0 || terms.contains(&(ch as u8)) {
            return (ch, out);
        }
        if out.len() + 1 < size {
            out.push(ch as u8);
        }
    }
}

impl Schedule {
    /// Parse the time specification at the start of `line`.
    ///
    /// Returns the schedule and the remainder of the line (leading blanks
    /// removed), which holds the user and/or command. Warnings are dropped;
    /// use [`parse_prefix_with`](Self::parse_prefix_with) to collect them.
    pub fn parse_prefix(line: &str) -> Result<(Schedule, &str), ScheduleError> {
        Self::parse_prefix_with(line, &mut rand::rng(), &mut Vec::new())
    }

    /// Like [`parse_prefix`](Self::parse_prefix), with a caller-supplied RNG
    /// for `~` ranges and a sink for warnings such as cronie's "Step size N
    /// higher than possible maximum of M". Warnings found before an error are
    /// kept, as cronie prints them before the error.
    ///
    /// The text is read exactly as cronie's `load_entry` reads a line, with
    /// the end of `line` acting as its newline.
    pub fn parse_prefix_with<'a, R: Rng>(
        line: &'a str,
        rng: &mut R,
        warnings: &mut Vec<String>,
    ) -> Result<(Schedule, &'a str), ScheduleError> {
        let line = trim_blanks(line);
        let mut stream = StrStream {
            bytes: line.as_bytes(),
            pos: 0,
            back: Vec::new(),
            newline_read: false,
        };
        let ch = stream.get_char();
        let (schedule, ch) = load_schedule(ch, &mut stream, rng, warnings).map_err(|(e, _)| e)?;
        stream.unget_char(ch);
        let rest = if stream.back.is_empty() {
            line.get(stream.pos..).unwrap_or("")
        } else {
            ""
        };
        Ok((schedule, rest))
    }

    /// Parse a complete five-field specification (or `@shortcut`) with no
    /// trailing text other than whitespace.
    pub fn parse(spec: &str) -> Result<Schedule, ScheduleError> {
        let (schedule, rest) = Self::parse_prefix(spec)?;
        if rest.trim().is_empty() {
            Ok(schedule)
        } else {
            Err(ScheduleError::BadTimeSpecifier)
        }
    }

    /// The `@reboot` schedule.
    pub fn reboot() -> Schedule {
        Schedule {
            minutes: 0,
            hours: 0,
            days_of_month: 0,
            months: 0,
            days_of_week: 0,
            minute_star: false,
            hour_star: false,
            dom_star: false,
            dow_star: false,
            reboot: true,
        }
    }

    /// True for `@reboot` entries.
    pub fn is_reboot(&self) -> bool {
        self.reboot
    }

    /// A "wildcard" job has `*` in its minute or hour field.  Vixie cron uses
    /// this to decide which jobs to run when the clock jumps.
    pub fn is_wild(&self) -> bool {
        self.minute_star || self.hour_star
    }

    /// Does this schedule fire at the given wall-clock time?  Seconds are
    /// ignored.
    pub fn matches(&self, t: &NaiveDateTime) -> bool {
        if self.reboot {
            return false;
        }
        let minute_ok = self.minutes & (1u64 << t.minute()) != 0;
        let hour_ok = self.hours & (1u32 << t.hour()) != 0;
        let month_ok = self.months & (1u16 << t.month()) != 0;
        let day_ok = self.day_matches(t.day(), t.weekday().num_days_from_sunday());
        minute_ok && hour_ok && month_ok && day_ok
    }

    /// cronie's day rule (`cron.c`): if either day field started with `*`,
    /// both must match; otherwise either may match.
    fn day_matches(&self, day_of_month: u32, weekday_from_sunday: u32) -> bool {
        let dom_ok = self.days_of_month & (1u32 << day_of_month) != 0;
        let dow_ok = self.days_of_week & (1u8 << weekday_from_sunday) != 0;
        if self.dom_star || self.dow_star {
            dom_ok && dow_ok
        } else {
            dom_ok || dow_ok
        }
    }

    /// The first wall-clock minute strictly after `after` at which this
    /// schedule fires, searching at most `max_years` years ahead.
    pub fn next_after(&self, after: &NaiveDateTime, max_years: u32) -> Option<NaiveDateTime> {
        if self.reboot {
            return None;
        }
        let limit = after.checked_add_months(chrono::Months::new(12 * max_years))?;
        let mut t = after
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(chrono::Duration::minutes(1))?;
        while t <= limit {
            if self.months & (1u16 << t.month()) == 0 {
                let (y, m) = if t.month() == 12 {
                    (t.year() + 1, 1)
                } else {
                    (t.year(), t.month() + 1)
                };
                t = chrono::NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.day_matches(t.day(), t.weekday().num_days_from_sunday()) {
                t = (t.date() + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)?;
                continue;
            }
            if self.hours & (1u32 << t.hour()) == 0 {
                t = t
                    .with_minute(0)?
                    .checked_add_signed(chrono::Duration::hours(1))?;
                continue;
            }
            if self.minutes & (1u64 << t.minute()) == 0 {
                t = t.checked_add_signed(chrono::Duration::minutes(1))?;
                continue;
            }
            return Some(t);
        }
        None
    }
}

/// A `&str` read as a stream whose end is a newline (read once) followed by
/// EOF.
struct StrStream<'a> {
    bytes: &'a [u8],
    pos: usize,
    back: Vec<i32>,
    newline_read: bool,
}

impl CharStream for StrStream<'_> {
    fn get_char(&mut self) -> i32 {
        if let Some(ch) = self.back.pop() {
            return ch;
        }
        if let Some(&b) = self.bytes.get(self.pos) {
            self.pos += 1;
            i32::from(b)
        } else if !self.newline_read {
            self.newline_read = true;
            NL
        } else {
            EOF
        }
    }

    fn unget_char(&mut self, ch: i32) {
        if ch == EOF {
            return;
        }
        if self.back.is_empty() {
            if ch == NL && self.newline_read && self.pos == self.bytes.len() {
                self.newline_read = false;
                return;
            }
            if self.pos > 0 && i32::from(self.bytes[self.pos - 1]) == ch {
                self.pos -= 1;
                return;
            }
        }
        self.back.push(ch);
    }
}

const ALL_HOURS: u32 = (1 << 24) - 1;
const ALL_DOM: u32 = !1;
const ALL_MONTHS: u16 = 0x1ffe;
const ALL_DOW: u8 = 0x7f;

/// The time part of cronie's `load_entry`, from `ch` (the first character of
/// the specification, already read) on. On success returns the character
/// after the specification and its trailing blanks (still consumed, as in
/// cronie); on error returns the error and cronie's `ch` at its `goto eof`.
pub(crate) fn load_schedule<S: CharStream, R: Rng>(
    ch: i32,
    s: &mut S,
    rng: &mut R,
    warnings: &mut Vec<String>,
) -> Result<(Schedule, i32), (ScheduleError, i32)> {
    if ch == i32::from(b'@') {
        let (ch, name) = get_string(s, MAX_COMMAND, b" \t\n");
        let mut e = Schedule {
            minutes: 1,
            hours: 1,
            days_of_month: ALL_DOM,
            months: ALL_MONTHS,
            days_of_week: ALL_DOW,
            minute_star: false,
            hour_star: false,
            dom_star: false,
            dow_star: false,
            reboot: false,
        };
        match name.as_slice() {
            b"reboot" => e = Schedule::reboot(),
            b"yearly" | b"annually" => {
                e.days_of_month = 1 << 1;
                e.months = 1 << 1;
                e.dow_star = true;
            }
            b"monthly" => {
                e.days_of_month = 1 << 1;
                e.dow_star = true;
            }
            b"weekly" => {
                e.days_of_week = 1;
                e.dom_star = true;
            }
            b"daily" | b"midnight" => {}
            b"hourly" => {
                e.hours = ALL_HOURS;
                e.hour_star = true;
            }
            _ => return Err((ScheduleError::BadTimeSpecifier, ch)),
        }
        return Ok((e, skip_blanks(s, ch)));
    }

    let star = |ch: i32| ch == i32::from(b'*');
    let minute_star = star(ch);
    let (minutes, ch) = get_list(Field::Minute, ch, s, rng, warnings)?;
    let hour_star = star(ch);
    let (hours, ch) = get_list(Field::Hour, ch, s, rng, warnings)?;
    let dom_star = star(ch);
    let (days_of_month, ch) = get_list(Field::DayOfMonth, ch, s, rng, warnings)?;
    let (months, ch) = get_list(Field::Month, ch, s, rng, warnings)?;
    let dow_star = star(ch);
    let (days_of_week, ch) = get_list(Field::DayOfWeek, ch, s, rng, warnings)?;
    // Day-of-week 0 and 7 are both Sunday.
    let mut dow = (days_of_week & 0x7f) as u8;
    if days_of_week & (1 << 7) != 0 {
        dow |= 1;
    }
    Ok((
        Schedule {
            minutes,
            hours: hours as u32,
            days_of_month: days_of_month as u32,
            months: months as u16,
            days_of_week: dow,
            minute_star,
            hour_star,
            dom_star,
            dow_star,
            reboot: false,
        },
        ch,
    ))
}

fn is_separator(ch: i32) -> bool {
    matches!(u8::try_from(ch), Ok(b'\t' | b'\n' | b' ' | b','))
}

/// cronie's `get_list`: ranges separated by `,`, then the rest of the field
/// and the following blanks are skipped.
fn get_list<S: CharStream, R: Rng>(
    field: Field,
    ch: i32,
    s: &mut S,
    rng: &mut R,
    warnings: &mut Vec<String>,
) -> Result<(u64, i32), (ScheduleError, i32)> {
    let mut bits = 0u64;
    s.unget_char(ch);
    let mut ch;
    loop {
        ch = get_range(&mut bits, field, s, rng, warnings);
        if ch == EOF {
            return Err((field.error(), EOF));
        }
        if ch != i32::from(b',') {
            break;
        }
    }
    // Skip_Nonblanks, then Skip_Blanks.
    while ch != i32::from(b'\t') && ch != i32::from(b' ') && ch != NL && ch != EOF {
        ch = s.get_char();
    }
    Ok((bits, skip_blanks(s, ch)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RangeState {
    Start,
    Ast,
    Step,
    Terms,
    Num1,
    Range,
    RangeNum2,
    Random,
    Finish,
}

/// cronie's `get_range` state machine. Returns the separator that ended the
/// range or [`EOF`] on error.
fn get_range<S: CharStream, R: Rng>(
    bits: &mut u64,
    field: Field,
    s: &mut S,
    rng: &mut R,
    warnings: &mut Vec<String>,
) -> i32 {
    use RangeState::*;
    let (low, high) = field.bounds();
    let names = field.names();
    let mut step = 1i32;
    let (mut low_, mut high_) = (0i32, 0i32);
    let mut state = Start;
    let mut ch = EOF;

    while state != Finish {
        ch = s.get_char();
        if ch == EOF {
            break;
        }
        match state {
            Start => {
                if ch == i32::from(b'*') {
                    low_ = low;
                    high_ = high;
                    state = Ast;
                } else if ch == i32::from(b'~') {
                    low_ = low;
                    state = Random;
                } else {
                    s.unget_char(ch);
                    match get_number(low, names, s) {
                        Some(n) => {
                            low_ = n;
                            state = Num1;
                        }
                        None => return EOF,
                    }
                }
            }
            Ast | RangeNum2 => {
                if ch == i32::from(b'/') {
                    state = Step;
                } else if is_separator(ch) {
                    state = Finish;
                } else {
                    return EOF;
                }
            }
            Step => {
                s.unget_char(ch);
                match get_number(0, None, s) {
                    Some(n) if n != 0 => {
                        step = n;
                        state = Terms;
                    }
                    _ => return EOF,
                }
            }
            Terms => {
                if is_separator(ch) {
                    state = Finish;
                } else {
                    return EOF;
                }
            }
            Num1 => {
                if ch == i32::from(b'-') {
                    state = Range;
                } else if ch == i32::from(b'~') {
                    state = Random;
                } else if is_separator(ch) {
                    high_ = low_;
                    state = Finish;
                } else {
                    return EOF;
                }
            }
            Range => {
                s.unget_char(ch);
                match get_number(low, names, s) {
                    Some(n) => {
                        high_ = n;
                        state = RangeNum2;
                    }
                    None => return EOF,
                }
            }
            Random => {
                if is_separator(ch) {
                    high_ = high;
                    state = Finish;
                } else {
                    s.unget_char(ch);
                    match get_number(low, names, s) {
                        Some(n) => {
                            high_ = n;
                            state = Terms;
                        }
                        None => return EOF,
                    }
                }
                if low_ > high_ {
                    return EOF;
                }
                // cronie: random() % (high_ - low_ + 1) + low_
                let pick = rng.random_range(i64::from(low_)..=i64::from(high_)) as i32;
                low_ = pick;
                high_ = pick;
            }
            Finish => unreachable!(),
        }
    }
    if state != Finish || ch == EOF {
        return EOF;
    }

    let span = high_.wrapping_sub(low_);
    if step > 1 && step > span {
        let max = if span > 0 { span } else { 1 };
        warnings.push(format!(
            "Warning: Step size {step} higher than possible maximum of {max}"
        ));
    }
    // for (i = low_; i <= high_; i += step) set_element(i)
    let mut i = low_;
    while i <= high_ {
        if i < low || i > high {
            s.unget_char(ch);
            return EOF;
        }
        *bits |= 1u64 << i;
        i = i.wrapping_add(step);
    }
    ch
}

/// `(int) strtol(digits, NULL, 10)` on LP64: saturate to the `long` range,
/// then keep the low 32 bits.
fn c_int_from_digits(digits: &[u8]) -> i32 {
    let mut value: i64 = 0;
    for &b in digits {
        value = value.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    value as i32
}

/// cronie's `get_number`: a run of ASCII alphanumerics that is a decimal
/// number or, for fields with names, an exact (case-insensitive) three-letter
/// name. Like cronie, a token that is neither pushes its terminator back
/// twice.
fn get_number<S: CharStream>(
    low: i32,
    names: Option<&'static [&'static str]>,
    s: &mut S,
) -> Option<i32> {
    let mut temp = Vec::new();
    let mut ch;
    loop {
        ch = s.get_char();
        match u8::try_from(ch) {
            Ok(b) if b.is_ascii_alphanumeric() => {
                // if (++len >= MAX_TEMPSTR) goto bad;
                if temp.len() + 1 >= MAX_TEMPSTR {
                    s.unget_char(ch);
                    return None;
                }
                temp.push(b);
            }
            _ => break,
        }
    }
    if temp.is_empty() {
        s.unget_char(ch);
        return None;
    }
    s.unget_char(ch);
    if temp.iter().all(u8::is_ascii_digit) {
        return Some(c_int_from_digits(&temp));
    }
    if let Some(i) = names.and_then(|names| {
        names
            .iter()
            .position(|n| n.as_bytes().eq_ignore_ascii_case(&temp))
    }) {
        return Some(low + i as i32);
    }
    s.unget_char(ch);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn sched(s: &str) -> Schedule {
        Schedule::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn minutes_fired(s: &Schedule) -> Vec<u32> {
        (0..60)
            .filter(|m| s.matches(&at(2026, 1, 1, 0, *m)))
            .collect()
    }

    fn parse_warn(s: &str) -> (Result<Schedule, ScheduleError>, Vec<String>) {
        let mut w = Vec::new();
        let r = Schedule::parse_prefix_with(s, &mut rand::rng(), &mut w).map(|(s, _)| s);
        (r, w)
    }

    #[test]
    fn every_minute() {
        let s = sched("* * * * *");
        assert!(s.matches(&at(2026, 9, 14, 17, 9)));
        assert!(s.is_wild());
    }

    #[test]
    fn specific_time() {
        let s = sched("30 2 * * *");
        assert!(s.matches(&at(2026, 9, 14, 2, 30)));
        assert!(!s.matches(&at(2026, 9, 14, 2, 31)));
        assert!(!s.matches(&at(2026, 9, 14, 3, 30)));
        assert!(!s.is_wild());
    }

    #[test]
    fn lists_ranges_steps() {
        let s = sched("0,15,30,45 9-17 * * 1-5");
        assert!(s.matches(&at(2026, 9, 14, 9, 15)));
        assert!(!s.matches(&at(2026, 9, 14, 9, 16)));
        assert!(!s.matches(&at(2026, 9, 13, 9, 15)));
        let s = sched("*/15 */2 * * *");
        assert!(s.matches(&at(2026, 1, 1, 4, 45)));
        assert!(!s.matches(&at(2026, 1, 1, 3, 0)));
        assert!(s.is_wild());
        assert_eq!(minutes_fired(&sched("1-10/3 * * * *")), vec![1, 4, 7, 10]);
        assert_eq!(
            minutes_fired(&sched("1-5,50-59/4 * * * *")),
            vec![1, 2, 3, 4, 5, 50, 54, 58]
        );
        assert_eq!(minutes_fired(&sched("007 * * * *")), vec![7]);
    }

    #[test]
    fn trailing_newline_ends_the_last_field() {
        assert!(Schedule::parse("*/5 * * * *\n").is_ok());
        assert!(Schedule::parse("@daily\n").is_ok());
        let (_, rest) = Schedule::parse_prefix("0 0 * * *\n").unwrap();
        assert_eq!(rest, "\n");
    }

    #[test]
    fn step_only_after_star_or_range() {
        assert_eq!(
            Schedule::parse("5/10 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("0 0 * * 1/2"),
            Err(ScheduleError::BadDayOfWeek)
        );
        assert_eq!(Schedule::parse("*/ * * * *"), Err(ScheduleError::BadMinute));
        assert_eq!(
            Schedule::parse("*/5x * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(Schedule::parse("*5 * * * *"), Err(ScheduleError::BadMinute));
        assert_eq!(
            Schedule::parse("*/0 * * * *"),
            Err(ScheduleError::BadMinute)
        );
    }

    #[test]
    fn oversized_steps_warn_but_parse() {
        let (s, w) = parse_warn("*/61 * * * *");
        assert_eq!(minutes_fired(&s.unwrap()), vec![0]);
        assert_eq!(
            w,
            vec!["Warning: Step size 61 higher than possible maximum of 59"]
        );
        let (s, w) = parse_warn("10-59/61 * * * *");
        assert_eq!(minutes_fired(&s.unwrap()), vec![10]);
        assert_eq!(w.len(), 1);
        let (_, w) = parse_warn("0-59/60 * * * *");
        assert_eq!(w.len(), 1);
        let (_, w) = parse_warn("0-59/59 * * * *");
        assert!(w.is_empty());
    }

    #[test]
    fn numbers_use_c_int_truncation() {
        // Values observed from cronie 1.7.2's `crontab -T`.
        assert_eq!(minutes_fired(&sched("4294967296 * * * *")), vec![0]);
        assert_eq!(minutes_fired(&sched("*/4294967297 * * * *")).len(), 60);
        assert_eq!(
            Schedule::parse("*/2147483648 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("*/4294967290 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("2147483648 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        let (r, w) = parse_warn("59-59/2147483647 * * * *");
        assert_eq!(r, Err(ScheduleError::BadMinute));
        assert_eq!(
            w,
            vec!["Warning: Step size 2147483647 higher than possible maximum of 1"]
        );
        let (r, w) = parse_warn("5-3/4294967290 * * * *");
        assert!(minutes_fired(&r.unwrap()).is_empty());
        assert!(w.is_empty());
        assert!(minutes_fired(&sched("*/2147483647 * * * *")) == vec![0]);
        assert_eq!(
            Schedule::parse("99999999999999999999999 * * * *"),
            Err(ScheduleError::BadMinute)
        );
    }

    #[test]
    fn tokens_reaching_max_tempstr_are_rejected() {
        // Oracle: cronie 1.7.2 `crontab -T` accepts the 131071-character
        // tokens and reports "bad minute" for the 131072-character ones.
        let num = |len: usize| format!("{}5", "0".repeat(len - 1));
        let s = sched(&format!("{} * * * *", num(MAX_TEMPSTR - 1)));
        assert_eq!(minutes_fired(&s), vec![5]);
        assert_eq!(
            Schedule::parse(&format!("{} * * * *", num(MAX_TEMPSTR))),
            Err(ScheduleError::BadMinute)
        );
        let s = sched(&format!("*/{} * * * *", num(MAX_TEMPSTR - 1)));
        assert_eq!(minutes_fired(&s).len(), 12);
        assert_eq!(
            Schedule::parse(&format!("*/{} * * * *", num(MAX_TEMPSTR))),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse(&format!("0-{} * * * *", num(MAX_TEMPSTR))),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse(&format!("@{}", "d".repeat(MAX_COMMAND + 5))),
            Err(ScheduleError::BadTimeSpecifier)
        );
    }

    #[test]
    fn warnings_before_an_error_are_kept() {
        let (r, w) = parse_warn("*/61 25 * * *");
        assert_eq!(r, Err(ScheduleError::BadHour));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn reversed_ranges_select_nothing() {
        assert!(minutes_fired(&sched("5-3 * * * *")).is_empty());
        assert!(minutes_fired(&sched("99-3 * * * *")).is_empty());
        let s = sched("0 0 * * sat-sun");
        assert!((1..=31).all(|d| !s.matches(&at(2026, 1, d, 0, 0))));
        let s = sched("0 0 * * sun-sat");
        assert!((1..=7).all(|d| s.matches(&at(2026, 9, d, 0, 0))));
        assert_eq!(
            Schedule::parse("70-80 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("58-61 * * * *"),
            Err(ScheduleError::BadMinute)
        );
    }

    #[test]
    fn names_are_exact_abbreviations() {
        let s = sched("0 0 * jan,MAR,Dec mon-Fri");
        assert!(s.matches(&at(2026, 1, 5, 0, 0)));
        assert!(s.matches(&at(2026, 12, 7, 0, 0)));
        assert!(!s.matches(&at(2026, 2, 2, 0, 0)));
        assert!(!s.matches(&at(2026, 1, 4, 0, 0)));
        assert_eq!(
            Schedule::parse("0 0 * * monday"),
            Err(ScheduleError::BadDayOfWeek)
        );
        assert_eq!(
            Schedule::parse("0 0 * JANUARY *"),
            Err(ScheduleError::BadMonth)
        );
        assert_eq!(
            Schedule::parse("0 0 * * mo"),
            Err(ScheduleError::BadDayOfWeek)
        );
        assert_eq!(
            Schedule::parse("jan * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(Schedule::parse("1a * * * *"), Err(ScheduleError::BadMinute));
    }

    #[test]
    fn sunday_seven() {
        assert!(sched("0 0 * * 7").matches(&at(2026, 9, 13, 0, 0)));
        let s = sched("0 0 * * 5-7");
        assert!(s.matches(&at(2026, 9, 13, 0, 0)));
        assert!(s.matches(&at(2026, 9, 11, 0, 0)));
        assert!(!s.matches(&at(2026, 9, 10, 0, 0)));
    }

    #[test]
    fn dom_dow_rule_matches_cron_c() {
        let s = sched("0 0 13 * 5");
        assert!(s.matches(&at(2026, 9, 13, 0, 0)));
        assert!(s.matches(&at(2026, 9, 11, 0, 0)));
        assert!(!s.matches(&at(2026, 9, 14, 0, 0)));
        let s = sched("0 0 */2 * 5");
        assert!(!s.matches(&at(2026, 9, 13, 0, 0)));
        assert!(s.matches(&at(2026, 9, 11, 0, 0)));
        assert!(!s.matches(&at(2026, 9, 18, 0, 0)));
    }

    #[test]
    fn shortcuts_are_case_sensitive() {
        assert!(sched("@hourly").matches(&at(2026, 5, 5, 7, 0)));
        assert!(!sched("@hourly").matches(&at(2026, 5, 5, 7, 1)));
        assert!(sched("@hourly").is_wild());
        assert!(sched("@daily").matches(&at(2026, 5, 5, 0, 0)));
        assert!(sched("@midnight").matches(&at(2026, 5, 5, 0, 0)));
        assert!(sched("@weekly").matches(&at(2026, 9, 13, 0, 0)));
        assert!(sched("@monthly").matches(&at(2026, 9, 1, 0, 0)));
        assert!(sched("@yearly").matches(&at(2026, 1, 1, 0, 0)));
        assert!(sched("@annually").matches(&at(2026, 1, 1, 0, 0)));
        assert!(sched("@reboot").is_reboot());
        for bad in ["@HOURLY", "@Daily", "@ANNUALLY", "@bogus", "@every"] {
            assert_eq!(
                Schedule::parse(bad),
                Err(ScheduleError::BadTimeSpecifier),
                "{bad}"
            );
        }
    }

    #[test]
    fn prefix_returns_remainder() {
        let (s, rest) = Schedule::parse_prefix("  5 4 * * *   root  /bin/echo hi").unwrap();
        assert_eq!(rest, "root  /bin/echo hi");
        assert!(s.matches(&at(2026, 1, 1, 4, 5)));
        let (_, rest) = Schedule::parse_prefix("@daily\tfoo").unwrap();
        assert_eq!(rest, "foo");
    }

    #[test]
    fn errors() {
        assert_eq!(Schedule::parse("60 * * * *"), Err(ScheduleError::BadMinute));
        assert_eq!(Schedule::parse("* 24 * * *"), Err(ScheduleError::BadHour));
        assert_eq!(
            Schedule::parse("* * 0 * *"),
            Err(ScheduleError::BadDayOfMonth)
        );
        assert_eq!(
            Schedule::parse("* * 32 * *"),
            Err(ScheduleError::BadDayOfMonth)
        );
        assert_eq!(Schedule::parse("* * * 13 *"), Err(ScheduleError::BadMonth));
        assert_eq!(
            Schedule::parse("* * * * 8"),
            Err(ScheduleError::BadDayOfWeek)
        );
        assert_eq!(
            Schedule::parse("1,,2 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(Schedule::parse(",1 * * * *"), Err(ScheduleError::BadMinute));
        assert_eq!(Schedule::parse("-1 * * * *"), Err(ScheduleError::BadMinute));
        assert_eq!(Schedule::parse("* * * *"), Err(ScheduleError::BadDayOfWeek));
        assert_eq!(
            Schedule::parse("* * * * * extra"),
            Err(ScheduleError::BadTimeSpecifier)
        );
        assert_eq!(
            Schedule::parse("0\0 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("0 0 L * *"),
            Err(ScheduleError::BadDayOfMonth)
        );
        assert_eq!(
            Schedule::parse("0 0 * * 5#3"),
            Err(ScheduleError::BadDayOfWeek)
        );
    }

    #[test]
    fn random_ranges() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut w = Vec::new();
        let mut parse = |s: &str, rng: &mut rand::rngs::StdRng| {
            Schedule::parse_prefix_with(s, rng, &mut w).map(|(s, _)| s)
        };
        for _ in 0..50 {
            let fired = minutes_fired(&parse("10~20 * * * *", &mut rng).unwrap());
            assert_eq!(fired.len(), 1);
            assert!((10..=20).contains(&fired[0]));
        }
        let s = parse("~ ~ * * *", &mut rng).unwrap();
        assert!(minutes_fired(&s).len() <= 1);
        assert!(!s.is_wild());
        assert!(minutes_fired(&parse("~5 * * * *", &mut rng).unwrap())[0] <= 5);

        // cronie range-checks only the value it picks.
        let (mut ok, mut bad) = (0, 0);
        for _ in 0..300 {
            match parse("50~70 * * * *", &mut rng) {
                Ok(s) => {
                    ok += 1;
                    assert!((50..=59).contains(&minutes_fired(&s)[0]));
                }
                Err(e) => {
                    bad += 1;
                    assert_eq!(e, ScheduleError::BadMinute);
                }
            }
        }
        assert!(ok > 0 && bad > 0, "ok={ok} bad={bad}");

        for bad in [
            "20~10 * * * *",
            "0~59/10 * * * *",
            "~/5 * * * *",
            "1~2~3 * * * *",
        ] {
            assert_eq!(
                parse(bad, &mut rng).map(|_| ()),
                Err(ScheduleError::BadMinute),
                "{bad}"
            );
        }
    }

    #[test]
    fn next_after_search() {
        let s = sched("30 2 * * *");
        assert_eq!(
            s.next_after(&at(2026, 9, 14, 2, 30), 5),
            Some(at(2026, 9, 15, 2, 30))
        );
        let s = sched("0 0 29 2 *");
        assert_eq!(
            s.next_after(&at(2026, 1, 1, 0, 0), 5),
            Some(at(2028, 2, 29, 0, 0))
        );
        assert_eq!(
            sched("0 0 31 2 *").next_after(&at(2026, 1, 1, 0, 0), 5),
            None
        );
        assert_eq!(sched("@reboot").next_after(&at(2026, 1, 1, 0, 0), 5), None);
        assert_eq!(
            sched("5-3 * * * *").next_after(&at(2026, 1, 1, 0, 0), 1),
            None
        );
    }
}
