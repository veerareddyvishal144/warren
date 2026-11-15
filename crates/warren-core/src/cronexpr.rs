//! Five-field POSIX cron expressions, as users write them, translated to the
//! six-field form the `cron` crate parses. The only semantic difference is the
//! day-of-week field: POSIX counts Sunday as 0 (or 7) and Monday as 1, while the
//! `cron` crate counts Sunday as 1 and Saturday as 7. Names (MON..SUN) are the
//! same in both and pass through.

use crate::{Error, Result};

const DAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// Validate a five-field expression and return the six-field equivalent with a
/// leading `0` seconds field and the day-of-week numbers shifted for the `cron`
/// crate. Shorthands such as `@daily` are rejected.
pub fn to_cron_crate(expr: &str) -> Result<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(Error::invalid(
            "schedule must have exactly five fields: minute hour day-of-month month day-of-week",
        ));
    }
    let dow = translate_dow(fields[4])?;
    Ok(format!(
        "0 {} {} {} {} {}",
        fields[0], fields[1], fields[2], fields[3], dow
    ))
}

fn translate_dow(field: &str) -> Result<String> {
    let mut items = Vec::new();
    for item in field.split(',') {
        if item.is_empty() {
            return Err(Error::invalid("day-of-week field has an empty list item"));
        }
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => (b, Some(s)),
            None => (item, None),
        };
        let translated = if base == "*" || base == "?" || base.contains('#') || base.contains('L') {
            base.to_string()
        } else if let Some((a, b)) = base.split_once('-') {
            format!("{}-{}", map_token(a)?, map_token(b)?)
        } else {
            map_token(base)?
        };
        items.push(match step {
            Some(s) => format!("{translated}/{s}"),
            None => translated,
        });
    }
    Ok(items.join(","))
}

fn map_token(token: &str) -> Result<String> {
    if let Ok(n) = token.parse::<u8>() {
        if n > 7 {
            return Err(Error::invalid(format!(
                "day-of-week {n} is out of range 0-7"
            )));
        }
        // 0 and 7 are Sunday in POSIX; the cron crate wants 1 for Sunday.
        return Ok(((n % 7) + 1).to_string());
    }
    let upper = token.to_ascii_uppercase();
    if DAY_NAMES.contains(&upper.as_str()) {
        return Ok(upper);
    }
    Err(Error::invalid(format!(
        "invalid day-of-week token {token:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_numeric_days() {
        assert_eq!(to_cron_crate("* * * * 1-5").unwrap(), "0 * * * * 2-6");
        assert_eq!(to_cron_crate("0 9 * * 0").unwrap(), "0 0 9 * * 1");
        assert_eq!(to_cron_crate("0 9 * * 7").unwrap(), "0 0 9 * * 1");
        assert_eq!(to_cron_crate("30 8 1 * 1,3,5").unwrap(), "0 30 8 1 * 2,4,6");
        assert_eq!(to_cron_crate("*/5 * * * *").unwrap(), "0 */5 * * * *");
        assert_eq!(
            to_cron_crate("0 0 * * mon-fri").unwrap(),
            "0 0 0 * * MON-FRI"
        );
        assert_eq!(to_cron_crate("0 0 * * */2").unwrap(), "0 0 0 * * */2");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(to_cron_crate("* * * *").is_err());
        assert!(to_cron_crate("@daily").is_err());
        assert!(to_cron_crate("* * * * 8").is_err());
        assert!(to_cron_crate("* * * * funday").is_err());
        assert!(to_cron_crate("* * * * 1,,2").is_err());
    }
}
