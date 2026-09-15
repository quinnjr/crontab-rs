//! Vixie-style "local minute" clock.
//!
//! The daemon counts time in minutes of *local wall-clock* time (`(epoch +
//! gmtoff) / 60`).  A DST transition therefore shows up as a jump in the
//! minute counter, which is what lets the daemon run skipped jobs or avoid
//! repeating them.

use chrono::{DateTime, Local, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;

/// The current local minute and the UTC offset (seconds) in effect.
pub fn now() -> (i64, i32) {
    at_epoch(Utc::now().timestamp())
}

/// Local minute and offset for an epoch second.
pub fn at_epoch(secs: i64) -> (i64, i32) {
    let local: DateTime<Local> = Local.timestamp_opt(secs, 0).single().unwrap_or_else(|| {
        // Ambiguous/gap instants cannot occur for a UTC epoch, but be safe.
        Local.timestamp_opt(secs, 0).earliest().unwrap()
    });
    let gmtoff = local.offset().fix().local_minus_utc();
    ((secs + gmtoff as i64).div_euclid(60), gmtoff)
}

/// Wall-clock fields for a local minute (interpreting the minute counter as
/// wall time directly, as Vixie cron does).
pub fn wall_time(minute: i64) -> NaiveDateTime {
    DateTime::<Utc>::from_timestamp(minute * 60, 0)
        .expect("minute in range")
        .naive_utc()
}

/// Wall-clock fields for a local minute as seen in another time zone.
pub fn wall_time_in_tz(minute: i64, gmtoff: i32, tz: Tz) -> NaiveDateTime {
    let instant = minute * 60 - gmtoff as i64;
    tz.timestamp_opt(instant, 0)
        .single()
        .unwrap_or_else(|| tz.timestamp_opt(instant, 0).earliest().unwrap())
        .naive_local()
}

/// Seconds since the epoch at the start of the *next* local minute after
/// `secs`, given the current offset.
pub fn next_minute_epoch(secs: i64, gmtoff: i32) -> i64 {
    let local = secs + gmtoff as i64;
    let next_local = (local.div_euclid(60) + 1) * 60;
    next_local - gmtoff as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn wall_time_roundtrip() {
        let t = wall_time(29_820_249); // some minute
        assert_eq!(t.second(), 0);
        let (m, _) = at_epoch(0);
        let w = wall_time(m);
        // 1970-01-01 in whatever local zone the test runs in.
        assert!(w.year() == 1970 || w.year() == 1969);
    }

    #[test]
    fn tz_conversion() {
        // Minute counter expressed with gmtoff 0 == UTC.
        let m = 29_820_240; // 2026-09-12 12:00 UTC
        let w = wall_time_in_tz(m, 0, chrono_tz::Asia::Tokyo);
        assert_eq!((w.day(), w.hour(), w.minute()), (12, 21, 0));
        let w = wall_time_in_tz(m, 3600, chrono_tz::UTC); // 12:00 local at +01:00
        assert_eq!(w.hour(), 11);
    }

    #[test]
    fn next_minute() {
        assert_eq!(next_minute_epoch(100, 0), 120);
        assert_eq!(next_minute_epoch(120, 0), 180);
        // local=130 -> next local 180 -> epoch 150
        assert_eq!(next_minute_epoch(100, 30), 150);
    }
}
