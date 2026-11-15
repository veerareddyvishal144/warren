//! Pure schedule arithmetic: when a five-field cron expression fires next in a
//! given IANA timezone.
//!
//! Warren stores classic five-field expressions (`minute hour day-of-month month
//! day-of-week`). The `cron` crate wants a leading seconds field, so `0 ` is
//! prepended before parsing; evaluation happens in the cron's timezone so that
//! "09:00 every day" stays 09:00 local across daylight-saving changes. Local
//! times that do not exist (the spring-forward gap) are skipped to the next
//! occurrence; local times that exist twice (the fall-back hour) fire once, at
//! the earlier instant.
//!
//! Day-of-week numbering follows the `cron` crate: `1`-`7` is Sunday-Saturday,
//! and the names `SUN`-`SAT` (or `MON-FRI` ranges) are accepted; `0` is rejected.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use warren_core::{Error, Result};

/// Number of fields in the cron expressions Warren accepts.
pub const FIELDS: usize = 5;

/// Parse an IANA timezone name such as `Europe/Berlin`. Empty (after trimming)
/// means UTC.
pub fn parse_timezone(timezone: &str) -> Result<Tz> {
    let name = timezone.trim();
    if name.is_empty() {
        return Ok(Tz::UTC);
    }
    name.parse::<Tz>().map_err(|_| {
        Error::invalid(format!(
            "unknown timezone {name:?}; use an IANA name such as Europe/Berlin or UTC"
        ))
    })
}

/// Parse a five-field cron expression into the `cron` crate's `Schedule` by
/// prepending a `0` seconds field. Extra whitespace between fields is tolerated;
/// any other field count is rejected.
pub fn parse_schedule(schedule: &str) -> Result<cron::Schedule> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != FIELDS {
        return Err(Error::invalid(format!(
            "schedule must have {FIELDS} fields (minute hour day-of-month month day-of-week), got {}",
            fields.len()
        )));
    }
    let expression = warren_core::cronexpr::to_cron_crate(&fields.join(" "))?;
    cron::Schedule::from_str(&expression)
        .map_err(|e| Error::invalid(format!("invalid cron schedule {schedule:?}: {e}")))
}

/// The next unix second strictly after `after_unix` at which `schedule` fires
/// in `timezone` (empty = UTC).
///
/// Errors (`invalid_request`) when the expression or the timezone does not
/// parse, when `after_unix` is outside chrono's range, or when the schedule has
/// no occurrence at all before the year 2100 (for example `0 0 30 2 *`).
pub fn next_run(schedule: &str, timezone: &str, after_unix: i64) -> Result<i64> {
    let tz = parse_timezone(timezone)?;
    let parsed = parse_schedule(schedule)?;
    let after = DateTime::<Utc>::from_timestamp(after_unix, 0)
        .ok_or_else(|| Error::invalid(format!("timestamp {after_unix} is out of range")))?
        .with_timezone(&tz);
    let next = parsed.after(&after).next().ok_or_else(|| {
        Error::invalid(format!(
            "schedule {schedule:?} never fires again in timezone {}",
            tz.name()
        ))
    })?;
    Ok(next.timestamp())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use warren_core::errors::CODE_INVALID_REQUEST;

    use super::*;

    /// Unix seconds of a UTC wall-clock time.
    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid utc time")
            .timestamp()
    }

    /// Unix seconds of an unambiguous local wall-clock time in `tz`.
    fn local(tz: &str, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        tz.parse::<Tz>()
            .expect("known timezone")
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("unambiguous local time")
            .timestamp()
    }

    #[test]
    fn every_fifteen_minutes_in_utc() {
        assert_eq!(
            next_run("*/15 * * * *", "", utc(2026, 3, 1, 0, 7)).unwrap(),
            utc(2026, 3, 1, 0, 15)
        );
        assert_eq!(
            next_run("*/15 * * * *", "UTC", utc(2026, 3, 1, 0, 59)).unwrap(),
            utc(2026, 3, 1, 1, 0)
        );
    }

    #[test]
    fn result_is_strictly_after_the_input() {
        // Exactly on a firing: the next one, not the same one.
        assert_eq!(
            next_run("*/15 * * * *", "", utc(2026, 3, 1, 0, 15)).unwrap(),
            utc(2026, 3, 1, 0, 30)
        );
        // One second before a firing: that firing.
        assert_eq!(
            next_run("*/15 * * * *", "", utc(2026, 3, 1, 0, 15) - 1).unwrap(),
            utc(2026, 3, 1, 0, 15)
        );
    }

    #[test]
    fn empty_timezone_means_utc() {
        let after = utc(2026, 3, 1, 12, 0);
        let a = next_run("0 9 * * *", "", after).unwrap();
        assert_eq!(a, next_run("0 9 * * *", "UTC", after).unwrap());
        assert_eq!(a, next_run("0 9 * * *", " Etc/UTC ", after).unwrap());
        assert_eq!(a, utc(2026, 3, 2, 9, 0));
    }

    #[test]
    fn new_york_nine_am_in_winter_and_summer() {
        // EST is UTC-5, EDT is UTC-4: the same local time is a different instant.
        assert_eq!(
            next_run("0 9 * * *", "America/New_York", utc(2026, 1, 15, 0, 0)).unwrap(),
            utc(2026, 1, 15, 14, 0)
        );
        assert_eq!(
            next_run("0 9 * * *", "America/New_York", utc(2026, 7, 15, 0, 0)).unwrap(),
            utc(2026, 7, 15, 13, 0)
        );
    }

    #[test]
    fn london_across_the_spring_transition() {
        // Clocks go forward on 2026-03-29 in the UK.
        assert_eq!(
            next_run("0 8 * * *", "Europe/London", utc(2026, 3, 28, 0, 0)).unwrap(),
            utc(2026, 3, 28, 8, 0)
        );
        assert_eq!(
            next_run("0 8 * * *", "Europe/London", utc(2026, 3, 29, 0, 0)).unwrap(),
            utc(2026, 3, 29, 7, 0)
        );
    }

    #[test]
    fn half_hour_offset_timezone() {
        // Midnight IST (UTC+5:30) on June 2 is 18:30Z on June 1.
        assert_eq!(
            next_run("0 0 * * *", "Asia/Kolkata", utc(2026, 6, 1, 0, 0)).unwrap(),
            utc(2026, 6, 1, 18, 30)
        );
    }

    #[test]
    fn southern_hemisphere_daylight_saving() {
        // Sydney is UTC+11 in January (AEDT) and UTC+10 in July (AEST).
        assert_eq!(
            next_run("0 9 * * *", "Australia/Sydney", utc(2026, 1, 10, 0, 0)).unwrap(),
            utc(2026, 1, 10, 22, 0)
        );
        assert_eq!(
            next_run("0 9 * * *", "Australia/Sydney", utc(2026, 7, 10, 0, 0)).unwrap(),
            utc(2026, 7, 10, 23, 0)
        );
    }

    #[test]
    fn spring_forward_gap_skips_to_the_next_day() {
        // 2026-03-08 02:30 does not exist in New York (02:00 EST -> 03:00 EDT).
        let midnight_before_gap = local("America/New_York", 2026, 3, 8, 0, 0);
        assert_eq!(
            next_run("30 2 * * *", "America/New_York", midnight_before_gap).unwrap(),
            utc(2026, 3, 9, 6, 30)
        );
        // The day before, 02:30 EST exists and is 07:30Z.
        let midnight_day_before = local("America/New_York", 2026, 3, 7, 0, 0);
        assert_eq!(
            next_run("30 2 * * *", "America/New_York", midnight_day_before).unwrap(),
            utc(2026, 3, 7, 7, 30)
        );
    }

    #[test]
    fn fall_back_ambiguous_hour_fires_once_at_the_earlier_instant() {
        // 2026-11-01 01:30 happens twice in New York: 05:30Z (EDT) and 06:30Z (EST).
        let midnight = utc(2026, 11, 1, 4, 0); // 00:00 EDT
        let first = next_run("30 1 * * *", "America/New_York", midnight).unwrap();
        assert_eq!(first, utc(2026, 11, 1, 5, 30));
        // After the first occurrence the second one is skipped: next is the following day.
        assert_eq!(
            next_run("30 1 * * *", "America/New_York", first).unwrap(),
            utc(2026, 11, 2, 6, 30)
        );
    }

    #[test]
    fn day_of_week_names_and_cron_crate_numbering() {
        // 2026-06-03 is a Wednesday; the next Monday is June 8. Noon CEST is 10:00Z.
        let after = utc(2026, 6, 3, 0, 0);
        let monday = next_run("0 12 * * MON", "Europe/Berlin", after).unwrap();
        assert_eq!(monday, utc(2026, 6, 8, 10, 0));
        // POSIX numbering: 1 is Monday, 0 and 7 are Sunday (translated for the cron crate).
        assert_eq!(
            next_run("0 12 * * 1", "Europe/Berlin", after).unwrap(),
            monday
        );
        assert_eq!(
            next_run("0 12 * * MON-FRI", "Europe/Berlin", after).unwrap(),
            utc(2026, 6, 3, 10, 0)
        );
        let sunday = utc(2026, 6, 7, 10, 0);
        assert_eq!(
            next_run("0 12 * * 0", "Europe/Berlin", after).unwrap(),
            sunday
        );
        assert_eq!(
            next_run("0 12 * * 7", "Europe/Berlin", after).unwrap(),
            sunday
        );
        assert_eq!(
            next_run("0 12 * * 1-5", "Europe/Berlin", after).unwrap(),
            utc(2026, 6, 3, 10, 0)
        );
        assert!(next_run("0 12 * * 8", "Europe/Berlin", after).is_err());
    }

    #[test]
    fn rolls_over_months_and_years() {
        assert_eq!(
            next_run("0 0 1 1 *", "", utc(2026, 5, 5, 0, 0)).unwrap(),
            utc(2027, 1, 1, 0, 0)
        );
        assert_eq!(
            next_run("0 0 31 * *", "", utc(2026, 2, 1, 0, 0)).unwrap(),
            utc(2026, 3, 31, 0, 0)
        );
        assert_eq!(
            next_run("45 23 * * *", "Asia/Tokyo", utc(2026, 12, 31, 14, 0)).unwrap(),
            utc(2026, 12, 31, 14, 45)
        );
    }

    #[test]
    fn tolerates_extra_whitespace() {
        assert_eq!(
            next_run("  */5   * * *   * ", "", utc(2026, 3, 1, 0, 1)).unwrap(),
            utc(2026, 3, 1, 0, 5)
        );
    }

    #[test]
    fn rejects_bad_expressions_timezones_and_timestamps() {
        let after = utc(2026, 3, 1, 0, 0);
        for bad in [
            "",
            "foo",
            "* * * *",
            "0 0 0 * * *",
            "60 * * * *",
            "0 25 * * *",
            "0 0 30 2 *",
            "@daily",
        ] {
            let err = next_run(bad, "", after).unwrap_err();
            assert_eq!(err.code, CODE_INVALID_REQUEST, "{bad:?} -> {err}");
        }
        let err = next_run("0 9 * * *", "Mars/Phobos", after).unwrap_err();
        assert_eq!(err.code, CODE_INVALID_REQUEST);
        assert!(err.message.contains("Mars/Phobos"));
        assert_eq!(
            next_run("0 9 * * *", "", i64::MAX).unwrap_err().code,
            CODE_INVALID_REQUEST
        );
        // Pre-1970 inputs are fine: the first match on or after 1970 comes back.
        assert!(next_run("0 9 * * *", "", -1).is_ok());
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_timezone("").unwrap(), Tz::UTC);
        assert_eq!(
            parse_timezone("America/Chicago").unwrap().name(),
            "America/Chicago"
        );
        assert!(parse_timezone("chicago").is_err());
        assert!(parse_schedule("*/10 * * * *").is_ok());
        assert!(parse_schedule("*/10 * * *").is_err());
    }
}
