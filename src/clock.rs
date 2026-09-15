//! Vixie-style "local minute" clock.
//!
//! The daemon counts time in minutes of *local wall-clock* time (`(epoch +
//! gmtoff) / 60`).  A DST transition therefore shows up as a jump in the
//! minute counter, which is what lets the daemon run skipped jobs or avoid
//! repeating them.

use chrono::{DateTime, Local, NaiveDateTime, Offset, TimeZone, Utc};

use crate::tz::Zone;

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
/// wall time directly, as cronie's `gmtime(virtualSecond)` does).
pub fn wall_time(minute: i64) -> NaiveDateTime {
    DateTime::<Utc>::from_timestamp(minute * 60, 0)
        .expect("minute in range")
        .naive_utc()
}

/// Wall-clock fields for a local minute as seen in `zone`, where `gmtoff` is
/// the local UTC offset the minute counter was computed with (cronie's
/// `localtime(virtualSecond - vGMToff)` under `TZ=CRON_TZ`).
pub fn wall_time_in_tz(minute: i64, gmtoff: i32, zone: &Zone) -> NaiveDateTime {
    zone.wall_time(minute * 60 - i64::from(gmtoff))
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
        let t = wall_time(29_820_249);
        assert_eq!(t.second(), 0);
        let (m, _) = at_epoch(0);
        let w = wall_time(m);
        assert!(w.year() == 1970 || w.year() == 1969);
    }

    #[test]
    fn tz_conversion() {
        let m = 29_820_240; // 2026-09-12 12:00 UTC
        let w = wall_time_in_tz(m, 3600, &Zone::utc()); // 12:00 local at +01:00
        assert_eq!(w.hour(), 11);
        let w = wall_time_in_tz(m, 0, &Zone::from_tz_value("ABC5"));
        assert_eq!((w.day(), w.hour()), (12, 7));
        if std::path::Path::new("/usr/share/zoneinfo/Asia/Tokyo").exists() {
            let w = wall_time_in_tz(m, 0, &Zone::from_tz_value("Asia/Tokyo"));
            assert_eq!((w.day(), w.hour(), w.minute()), (12, 21, 0));
        }
    }

    #[test]
    fn next_minute() {
        assert_eq!(next_minute_epoch(100, 0), 120);
        assert_eq!(next_minute_epoch(120, 0), 180);
        assert_eq!(next_minute_epoch(100, 30), 150);
    }
}
