//! Cron time-specification parsing and matching with Vixie/cronie semantics.
//!
//! A schedule has five fields (minute, hour, day-of-month, month, day-of-week)
//! or one of the `@reboot`, `@yearly`, `@annually`, `@monthly`, `@weekly`,
//! `@daily`, `@midnight`, `@hourly` shortcuts.
//!
//! Field grammar (each field is a comma-separated list of ranges):
//!
//! ```text
//! range  := '*' | value | value '-' value | [value] '~' [value]
//!           followed optionally by '/' step
//! value  := number | name (months and weekdays, 3-letter prefixes accepted)
//! ```
//!
//! `~` picks a random value in the range once, at parse time (cronie
//! extension).  `value/step` is treated as `value-max/step`.
//!
//! Day-of-month and day-of-week follow the classic rule: if both fields are
//! restricted (neither begins with `*`), a time matches when *either* field
//! matches; otherwise both must match.

use std::fmt;

use chrono::{Datelike, NaiveDateTime, Timelike};
use rand::Rng;

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

impl Field {
    fn bounds(self) -> (u32, u32) {
        match self {
            Field::Minute => (0, 59),
            Field::Hour => (0, 23),
            Field::DayOfMonth => (1, 31),
            Field::Month => (1, 12),
            // 7 is accepted as an alias for Sunday and folded to 0.
            Field::DayOfWeek => (0, 7),
        }
    }

    fn names(self) -> Option<&'static [&'static str]> {
        match self {
            Field::Month => Some(&[
                "january",
                "february",
                "march",
                "april",
                "may",
                "june",
                "july",
                "august",
                "september",
                "october",
                "november",
                "december",
            ]),
            Field::DayOfWeek => Some(&[
                "sunday",
                "monday",
                "tuesday",
                "wednesday",
                "thursday",
                "friday",
                "saturday",
            ]),
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

impl Schedule {
    /// Parse the time specification at the start of `line`.
    ///
    /// Returns the schedule and the remainder of the line (leading whitespace
    /// removed), which holds the user and/or command.
    pub fn parse_prefix(line: &str) -> Result<(Schedule, &str), ScheduleError> {
        Self::parse_prefix_with(line, &mut rand::rng())
    }

    /// Like [`parse_prefix`](Self::parse_prefix) but with a caller-supplied
    /// RNG for `~` ranges (used by tests for determinism).
    pub fn parse_prefix_with<'a, R: Rng>(
        line: &'a str,
        rng: &mut R,
    ) -> Result<(Schedule, &'a str), ScheduleError> {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix('@') {
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let (name, remainder) = rest.split_at(end);
            let spec = match name.to_ascii_lowercase().as_str() {
                "reboot" => {
                    return Ok((Schedule::reboot(), remainder.trim_start()));
                }
                "yearly" | "annually" => "0 0 1 1 *",
                "monthly" => "0 0 1 * *",
                "weekly" => "0 0 * * 0",
                "daily" | "midnight" => "0 0 * * *",
                "hourly" => "0 * * * *",
                _ => return Err(ScheduleError::BadTimeSpecifier),
            };
            let (schedule, _) = Self::parse_fields(spec, rng)?;
            return Ok((schedule, remainder.trim_start()));
        }
        Self::parse_fields(line, rng)
    }

    /// Parse a complete five-field specification (or `@shortcut`) with no
    /// trailing text.
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
    ) -> Result<(Schedule, &'a str), ScheduleError> {
        let mut rest = line.trim_start();
        let mut next = |field: Field| -> Result<(u64, bool), ScheduleError> {
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let (token, remainder) = rest.split_at(end);
            if token.is_empty() {
                return Err(field.error());
            }
            rest = remainder.trim_start();
            parse_field(token, field, rng)
        };
        let (minutes, minute_star) = next(Field::Minute)?;
        let (hours, hour_star) = next(Field::Hour)?;
        let (days_of_month, dom_star) = next(Field::DayOfMonth)?;
        let (months, month_star) = next(Field::Month)?;
        let _ = month_star;
        let (days_of_week, dow_star) = next(Field::DayOfWeek)?;
        // Fold Sunday-as-7 onto bit 0.
        let mut dow = days_of_week as u8;
        if days_of_week & (1 << 7) != 0 {
            dow = (dow & 0x7f) | 1;
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
        let dom_ok = self.days_of_month & (1u32 << t.day()) != 0;
        let dow_ok = self.days_of_week & (1u8 << t.weekday().num_days_from_sunday()) != 0;
        let day_ok = if self.dom_star || self.dow_star {
            dom_ok && dow_ok
        } else {
            dom_ok || dow_ok
        };
        minute_ok && hour_ok && month_ok && day_ok
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
                // Jump to the first minute of next month.
                let (y, m) = if t.month() == 12 {
                    (t.year() + 1, 1)
                } else {
                    (t.year(), t.month() + 1)
                };
                t = chrono::NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            let dom_ok = self.days_of_month & (1u32 << t.day()) != 0;
            let dow_ok = self.days_of_week & (1u8 << t.weekday().num_days_from_sunday()) != 0;
            let day_ok = if self.dom_star || self.dow_star {
                dom_ok && dow_ok
            } else {
                dom_ok || dow_ok
            };
            if !day_ok {
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

fn parse_field<R: Rng>(
    token: &str,
    field: Field,
    rng: &mut R,
) -> Result<(u64, bool), ScheduleError> {
    let star = token.starts_with('*');
    let mut bits = 0u64;
    for part in token.split(',') {
        if part.is_empty() {
            return Err(field.error());
        }
        bits |= parse_range(part, field, rng)?;
    }
    Ok((bits, star))
}

fn parse_range<R: Rng>(part: &str, field: Field, rng: &mut R) -> Result<u64, ScheduleError> {
    let (min, max) = field.bounds();
    let (base, step) = match part.split_once('/') {
        Some((b, s)) => {
            let step = parse_plain_number(s).ok_or(field.error())?;
            if step == 0 {
                return Err(field.error());
            }
            (b, Some(step))
        }
        None => (part, None),
    };

    let (low, high) = if base == "*" {
        (min, max)
    } else if let Some((lo, hi)) = base.split_once('~') {
        let low = if lo.is_empty() {
            min
        } else {
            parse_value(lo, field)?
        };
        let high = if hi.is_empty() {
            max
        } else {
            parse_value(hi, field)?
        };
        if low > high {
            return Err(field.error());
        }
        let chosen = rng.random_range(low..=high);
        return match step {
            None => Ok(1u64 << chosen),
            Some(step) => {
                // Random offset within the first step window, then stride.
                let start = rng.random_range(low..=(low + step - 1).min(high));
                Ok(bits_between(start, high, step))
            }
        };
    } else if let Some((lo, hi)) = base.split_once('-') {
        let low = parse_value(lo, field)?;
        let high = parse_value(hi, field)?;
        if low > high {
            return Err(field.error());
        }
        (low, high)
    } else {
        let value = parse_value(base, field)?;
        match step {
            None => return Ok(1u64 << value),
            Some(_) => (value, max),
        }
    };

    Ok(bits_between(low, high, step.unwrap_or(1)))
}

fn bits_between(low: u32, high: u32, step: u32) -> u64 {
    let mut bits = 0u64;
    let mut v = low;
    while v <= high {
        bits |= 1u64 << v;
        v += step;
    }
    bits
}

fn parse_plain_number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn parse_value(s: &str, field: Field) -> Result<u32, ScheduleError> {
    let (min, max) = field.bounds();
    if let Some(n) = parse_plain_number(s) {
        if n < min || n > max {
            return Err(field.error());
        }
        return Ok(n);
    }
    let names = field.names().ok_or(field.error())?;
    if s.len() < 3 || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(field.error());
    }
    let lower = s.to_ascii_lowercase();
    for (i, name) in names.iter().enumerate() {
        if name.starts_with(&lower) {
            return Ok(min + i as u32);
        }
    }
    Err(field.error())
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
        assert!(s.matches(&at(2026, 9, 14, 9, 15))); // Monday
        assert!(!s.matches(&at(2026, 9, 14, 9, 16)));
        assert!(!s.matches(&at(2026, 9, 13, 9, 15))); // Sunday
        assert!(!s.matches(&at(2026, 9, 14, 18, 0)));

        let s = sched("*/15 */2 * * *");
        assert!(s.matches(&at(2026, 1, 1, 0, 0)));
        assert!(s.matches(&at(2026, 1, 1, 4, 45)));
        assert!(!s.matches(&at(2026, 1, 1, 3, 0)));
        assert!(!s.matches(&at(2026, 1, 1, 4, 10)));
        assert!(s.is_wild());

        let s = sched("5/20 * * * *");
        for m in [5, 25, 45] {
            assert!(s.matches(&at(2026, 1, 1, 0, m)), "{m}");
        }
        assert!(!s.matches(&at(2026, 1, 1, 0, 0)));

        let s = sched("1-10/3 * * * *");
        for m in [1, 4, 7, 10] {
            assert!(s.matches(&at(2026, 1, 1, 0, m)), "{m}");
        }
        assert!(!s.matches(&at(2026, 1, 1, 0, 13)));
    }

    #[test]
    fn names() {
        let s = sched("0 0 * jan,MAR,december mon-Fri");
        assert!(s.matches(&at(2026, 1, 5, 0, 0)));
        assert!(s.matches(&at(2026, 3, 2, 0, 0)));
        assert!(s.matches(&at(2026, 12, 7, 0, 0)));
        assert!(!s.matches(&at(2026, 2, 2, 0, 0)));
        assert!(!s.matches(&at(2026, 1, 4, 0, 0))); // Sunday
        assert_eq!(
            Schedule::parse("0 0 * * foo"),
            Err(ScheduleError::BadDayOfWeek)
        );
        assert_eq!(Schedule::parse("0 0 * xyz *"), Err(ScheduleError::BadMonth));
    }

    #[test]
    fn sunday_seven() {
        let s = sched("0 0 * * 7");
        assert!(s.matches(&at(2026, 9, 13, 0, 0)));
        let s = sched("0 0 * * 5-7");
        assert!(s.matches(&at(2026, 9, 13, 0, 0)));
        assert!(s.matches(&at(2026, 9, 11, 0, 0)));
        assert!(!s.matches(&at(2026, 9, 10, 0, 0)));
    }

    #[test]
    fn dom_dow_or_semantics() {
        // Both restricted: fires on the 13th OR on Fridays.
        let s = sched("0 0 13 * 5");
        assert!(s.matches(&at(2026, 9, 13, 0, 0))); // 13th (Sunday)
        assert!(s.matches(&at(2026, 9, 11, 0, 0))); // Friday 11th
        assert!(!s.matches(&at(2026, 9, 14, 0, 0)));

        // dom is `*/2` -> star flag set -> AND semantics.
        let s = sched("0 0 */2 * 5");
        assert!(!s.matches(&at(2026, 9, 13, 0, 0)));
        assert!(s.matches(&at(2026, 9, 11, 0, 0))); // Friday, odd day 11
        assert!(!s.matches(&at(2026, 9, 18, 0, 0))); // Friday, even day 18
    }

    #[test]
    fn shortcuts() {
        assert!(sched("@hourly").matches(&at(2026, 5, 5, 7, 0)));
        assert!(!sched("@hourly").matches(&at(2026, 5, 5, 7, 1)));
        assert!(sched("@daily").matches(&at(2026, 5, 5, 0, 0)));
        assert!(sched("@midnight").matches(&at(2026, 5, 5, 0, 0)));
        assert!(sched("@weekly").matches(&at(2026, 9, 13, 0, 0)));
        assert!(!sched("@weekly").matches(&at(2026, 9, 14, 0, 0)));
        assert!(sched("@monthly").matches(&at(2026, 9, 1, 0, 0)));
        assert!(sched("@yearly").matches(&at(2026, 1, 1, 0, 0)));
        assert!(sched("@ANNUALLY").matches(&at(2026, 1, 1, 0, 0)));
        assert!(!sched("@yearly").matches(&at(2026, 2, 1, 0, 0)));
        let r = sched("@reboot");
        assert!(r.is_reboot());
        assert!(!r.matches(&at(2026, 1, 1, 0, 0)));
        assert_eq!(
            Schedule::parse("@bogus"),
            Err(ScheduleError::BadTimeSpecifier)
        );
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
            Schedule::parse("*/0 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("5-3 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(
            Schedule::parse("1,,2 * * * *"),
            Err(ScheduleError::BadMinute)
        );
        assert_eq!(Schedule::parse("* * * *"), Err(ScheduleError::BadDayOfWeek));
        assert_eq!(
            Schedule::parse("* * * * * extra"),
            Err(ScheduleError::BadTimeSpecifier)
        );
        assert_eq!(Schedule::parse("a * * * *"), Err(ScheduleError::BadMinute));
    }

    #[test]
    fn random_ranges() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for _ in 0..50 {
            let (s, _) = Schedule::parse_prefix_with("10~20 * * * *", &mut rng).unwrap();
            let fired: Vec<u32> = (0..60)
                .filter(|m| s.matches(&at(2026, 1, 1, 0, *m)))
                .collect();
            assert_eq!(fired.len(), 1);
            assert!((10..=20).contains(&fired[0]));
        }
        let (s, _) = Schedule::parse_prefix_with("~ ~ * * *", &mut rng).unwrap();
        let fired = (0..60)
            .filter(|m| s.matches(&at(2026, 1, 1, 0, *m)))
            .count();
        assert!(fired <= 1);
        assert!(!s.is_wild());

        let (s, _) = Schedule::parse_prefix_with("0~59/10 * * * *", &mut rng).unwrap();
        let fired: Vec<u32> = (0..60)
            .filter(|m| s.matches(&at(2026, 1, 1, 0, *m)))
            .collect();
        assert_eq!(fired.len(), 6, "{fired:?}");
        assert!(fired[0] < 10);
        assert!(fired.windows(2).all(|w| w[1] - w[0] == 10));

        assert_eq!(
            Schedule::parse_prefix_with("20~10 * * * *", &mut rng).map(|_| ()),
            Err(ScheduleError::BadMinute)
        );
    }

    #[test]
    fn next_after_search() {
        let s = sched("30 2 * * *");
        assert_eq!(
            s.next_after(&at(2026, 9, 14, 2, 30), 5),
            Some(at(2026, 9, 15, 2, 30))
        );
        assert_eq!(
            s.next_after(&at(2026, 9, 14, 2, 29), 5),
            Some(at(2026, 9, 14, 2, 30))
        );
        let s = sched("0 0 29 2 *");
        assert_eq!(
            s.next_after(&at(2026, 1, 1, 0, 0), 5),
            Some(at(2028, 2, 29, 0, 0))
        );
        let s = sched("0 0 31 2 *");
        assert_eq!(s.next_after(&at(2026, 1, 1, 0, 0), 5), None);
        let s = sched("* * * * *");
        assert_eq!(
            s.next_after(&at(2026, 12, 31, 23, 59), 5),
            Some(at(2027, 1, 1, 0, 0))
        );
        assert_eq!(sched("@reboot").next_after(&at(2026, 1, 1, 0, 0), 5), None);
    }
}
