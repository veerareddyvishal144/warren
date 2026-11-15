//! Cron schedule validation and next-run computation.
//!
//! Schedules are five-field cron expressions (minute hour day-of-month month
//! day-of-week). They are validated and evaluated by prepending a seconds field
//! (`"0 "`) and handing the six-field string to the `cron` crate, exactly as the
//! scheduler crate does when it fires them, so both agree on every instant.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use warren_core::{Error, Result};

/// Longest accepted cron prompt.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// Parse a five-field cron expression into a `cron::Schedule`.
pub fn parse_schedule(expr: &str) -> Result<Schedule> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(Error::invalid(format!(
            "schedule must have five fields (minute hour day-of-month month day-of-week); got {}",
            fields.len()
        )));
    }
    let six = warren_core::cronexpr::to_cron_crate(&fields.join(" "))?;
    Schedule::from_str(&six)
        .map_err(|e| Error::invalid(format!("invalid cron schedule {expr:?}: {e}")))
}

/// Collapse whitespace so equal schedules store equally.
pub fn normalize_schedule(expr: &str) -> String {
    expr.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse an IANA timezone name; empty means UTC.
pub fn parse_timezone(name: &str) -> Result<Tz> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(Tz::UTC);
    }
    Tz::from_str(name).map_err(|_| {
        Error::invalid(format!(
            "unknown timezone {name:?}; use an IANA name such as Europe/Berlin or UTC"
        ))
    })
}

/// Unix seconds of the first firing strictly after `now` (unix seconds),
/// evaluated in `timezone`.
pub fn next_run_at(schedule: &str, timezone: &str, now: i64) -> Result<i64> {
    let sched = parse_schedule(schedule)?;
    let tz = parse_timezone(timezone)?;
    let now_utc = DateTime::<Utc>::from_timestamp(now, 0)
        .ok_or_else(|| Error::invalid("current time is out of range"))?;
    let now_in_tz = now_utc.with_timezone(&tz);
    sched
        .after(&now_in_tz)
        .next()
        .map(|d| d.timestamp())
        .ok_or_else(|| Error::invalid(format!("schedule {schedule:?} never fires again")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z

    #[test]
    fn five_fields_required() {
        assert!(parse_schedule("*/5 * * * *").is_ok());
        assert!(parse_schedule("0 9 * * MON-FRI").is_ok());
        assert!(parse_schedule("* * * *").is_err());
        assert!(parse_schedule("0 0 * * * *").is_err());
        assert!(parse_schedule("@hourly").is_err());
        assert!(parse_schedule("99 * * * *").is_err());
        assert!(parse_schedule("").is_err());
    }

    #[test]
    fn timezones() {
        assert_eq!(parse_timezone("").unwrap(), Tz::UTC);
        assert_eq!(parse_timezone(" UTC ").unwrap(), Tz::UTC);
        assert!(parse_timezone("Europe/Berlin").is_ok());
        assert!(parse_timezone("Mars/Olympus").is_err());
    }

    #[test]
    fn next_run_in_utc_and_in_a_zone() {
        // Every five minutes: strictly after midnight is 00:05.
        assert_eq!(next_run_at("*/5 * * * *", "", NOW).unwrap(), NOW + 300);
        // 09:30 New York on 2026-01-01 is 14:30 UTC.
        assert_eq!(
            next_run_at("30 9 * * *", "America/New_York", NOW).unwrap(),
            NOW + 14 * 3600 + 30 * 60
        );
        // Exactly at a firing instant the next one is strictly later.
        assert_eq!(next_run_at("0 0 * * *", "UTC", NOW).unwrap(), NOW + 86_400);
    }

    #[test]
    fn normalizes_whitespace() {
        assert_eq!(normalize_schedule("  0   9 * *   *"), "0 9 * * *");
    }
}
