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

/// A field ends at a blank or at the end of the line.
fn is_field_end(c: char) -> bool {
    is_blank(c) || c == '\n'
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
    pub fn parse_prefix_with<'a, R: Rng>(
        line: &'a str,
        rng: &mut R,
        warnings: &mut Vec<String>,
    ) -> Result<(Schedule, &'a str), ScheduleError> {
        let line = trim_blanks(line);
        if let Some(rest) = line.strip_prefix('@') {
            let end = rest.find(is_field_end).unwrap_or(rest.len());
            let (name, remainder) = rest.split_at(end);
            let spec = match name {
                "reboot" => return Ok((Schedule::reboot(), trim_blanks(remainder))),
                "yearly" | "annually" => "0 0 1 1 *",
                "monthly" => "0 0 1 * *",
                "weekly" => "0 0 * * 0",
                "daily" | "midnight" => "0 0 * * *",
                "hourly" => "0 * * * *",
                _ => return Err(ScheduleError::BadTimeSpecifier),
            };
            let (schedule, _) = Self::parse_fields(spec, rng, warnings)?;
            return Ok((schedule, trim_blanks(remainder)));
        }
        Self::parse_fields(line, rng, warnings)
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

    fn parse_fields<'a, R: Rng>(
        line: &'a str,
        rng: &mut R,
        warnings: &mut Vec<String>,
    ) -> Result<(Schedule, &'a str), ScheduleError> {
        let mut rest = trim_blanks(line);
        let mut next = |field: Field| -> Result<(u64, bool), ScheduleError> {
            let end = rest.find(is_field_end).unwrap_or(rest.len());
            let (token, remainder) = rest.split_at(end);
            if token.is_empty() {
                return Err(field.error());
            }
            rest = trim_blanks(remainder);
            parse_list(token, field, rng, warnings)
        };
        let (minutes, minute_star) = next(Field::Minute)?;
        let (hours, hour_star) = next(Field::Hour)?;
        let (days_of_month, dom_star) = next(Field::DayOfMonth)?;
        let (months, _) = next(Field::Month)?;
        let (days_of_week, dow_star) = next(Field::DayOfWeek)?;
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
            rest,
        ))
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

fn parse_list<R: Rng>(
    token: &str,
    field: Field,
    rng: &mut R,
    warnings: &mut Vec<String>,
) -> Result<(u64, bool), ScheduleError> {
    let star = token.starts_with('*');
    let mut bits = 0u64;
    for part in token.split(',') {
        bits |= parse_range(part, field, rng, warnings)?;
    }
    Ok((bits, star))
}

/// Split off the leading run of ASCII alphanumerics (cronie's `get_number`
/// reads exactly this much).
fn take_alnum(s: &str) -> (&str, &str) {
    let end = s
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(s.len());
    s.split_at(end)
}

/// `(int) strtol(digits, NULL, 10)` on LP64: saturate to the `long` range,
/// then keep the low 32 bits.
fn c_int_from_digits(digits: &str) -> i32 {
    let mut value: i64 = 0;
    for b in digits.bytes() {
        value = value.saturating_mul(10).saturating_add(i64::from(b - b'0'));
    }
    value as i32
}

/// cronie's `get_number`: a decimal number, or an exact (case-insensitive)
/// three-letter name for fields that have names.
fn get_number(token: &str, field: Field) -> Result<i32, ScheduleError> {
    if token.is_empty() {
        return Err(field.error());
    }
    if token.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(c_int_from_digits(token));
    }
    let (low, _) = field.bounds();
    field
        .names()
        .and_then(|names| names.iter().position(|n| n.eq_ignore_ascii_case(token)))
        .map(|i| low + i as i32)
        .ok_or(field.error())
}

/// An optional `/step` after `*` or a range: digits only, non-zero as a C
/// `int`, and nothing after it.
fn optional_step(after: &str, field: Field) -> Result<i32, ScheduleError> {
    if after.is_empty() {
        return Ok(1);
    }
    let digits = after.strip_prefix('/').ok_or(field.error())?;
    let (token, rest) = take_alnum(digits);
    if token.is_empty() || !rest.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err(field.error());
    }
    match c_int_from_digits(token) {
        0 => Err(field.error()),
        step => Ok(step),
    }
}

fn parse_range<R: Rng>(
    part: &str,
    field: Field,
    rng: &mut R,
    warnings: &mut Vec<String>,
) -> Result<u64, ScheduleError> {
    let (low, high) = field.bounds();
    let err = field.error();

    let (first, last, step) = if let Some(rest) = part.strip_prefix('*') {
        (low, high, optional_step(rest, field)?)
    } else if let Some(rest) = part.strip_prefix('~') {
        let pick = random_pick(low, rest, field, rng)?;
        (pick, pick, 1)
    } else {
        let (tok1, after) = take_alnum(part);
        let a = get_number(tok1, field)?;
        if after.is_empty() {
            (a, a, 1)
        } else if let Some(r) = after.strip_prefix('-') {
            let (tok2, after2) = take_alnum(r);
            let b = get_number(tok2, field)?;
            (a, b, optional_step(after2, field)?)
        } else if let Some(r) = after.strip_prefix('~') {
            let pick = random_pick(a, r, field, rng)?;
            (pick, pick, 1)
        } else {
            return Err(err);
        }
    };

    let span = last.wrapping_sub(first);
    if step > 1 && step > span {
        let max = if span > 0 { span } else { 1 };
        warnings.push(format!(
            "Warning: Step size {step} higher than possible maximum of {max}"
        ));
    }
    // cronie's loop: `for (i = low_; i <= high_; i += step) set_element(i)`,
    // where set_element fails for values outside the field.
    let mut bits = 0u64;
    let mut i = first;
    while i <= last {
        if i < low || i > high {
            return Err(err);
        }
        bits |= 1u64 << i;
        i = i.wrapping_add(step);
    }
    Ok(bits)
}

/// `[a] "~" [b]`: cronie picks `random() % (b - a + 1) + a` when loading and
/// then checks the pick against the field.
fn random_pick<R: Rng>(
    start: i32,
    rest: &str,
    field: Field,
    rng: &mut R,
) -> Result<i32, ScheduleError> {
    let (_, high) = field.bounds();
    let end = if rest.is_empty() {
        high
    } else {
        let (tok, after) = take_alnum(rest);
        if !after.is_empty() {
            return Err(field.error());
        }
        get_number(tok, field)?
    };
    if start > end {
        return Err(field.error());
    }
    Ok(rng.random_range(i64::from(start)..=i64::from(end)) as i32)
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
