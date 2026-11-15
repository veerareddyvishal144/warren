//! Small parsers and formatters shared by the commands.

use std::collections::BTreeMap;
use std::path::PathBuf;

use warren_core::{Error, MICROS_PER_DOLLAR, Result, ids};

/// Split `K=V` at the first `=`; the key must be non-empty.
pub fn parse_kv(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| Error::invalid(format!("expected K=V, got {s:?}")))?;
    if k.is_empty() {
        return Err(Error::invalid(format!("empty key in {s:?}")));
    }
    Ok((k.to_string(), v.to_string()))
}

/// Parse repeated `--env K=V` flags. Keys must be environment variable names
/// (`[A-Z_][A-Z0-9_]*`), as the API requires.
pub fn parse_env_pairs(items: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for item in items {
        let (k, v) = parse_kv(item)?;
        if !ids::valid_env_key(&k) {
            return Err(Error::invalid(format!(
                "invalid environment variable name {k:?}: use [A-Z_][A-Z0-9_]*"
            )));
        }
        out.insert(k, v);
    }
    Ok(out)
}

/// Parse repeated `K=V` flags without key validation (metadata).
pub fn parse_pairs(items: &[String]) -> Result<BTreeMap<String, String>> {
    items.iter().map(|s| parse_kv(s)).collect()
}

/// Parse a dollar amount (`5`, `12.50`, `$0.25`) into micros. At most six
/// decimals; the amount must be positive. Returns `String` so clap can use it
/// as a value parser.
pub fn parse_usd(s: &str) -> std::result::Result<i64, String> {
    let t = s.trim().trim_start_matches('$');
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() && frac.is_empty() {
        return Err(format!("{s:?} is not a dollar amount"));
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{s:?} is not a dollar amount; use digits like 5 or 12.50"
        ));
    }
    if frac.len() > 6 {
        return Err("at most six decimal places are allowed".to_string());
    }
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| "amount too large".to_string())?
    };
    let frac_micros: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}")
            .parse()
            .map_err(|_| "amount too large".to_string())?
    };
    let total = whole
        .checked_mul(MICROS_PER_DOLLAR)
        .and_then(|m| m.checked_add(frac_micros))
        .ok_or_else(|| "amount too large".to_string())?;
    if total <= 0 {
        return Err("amount must be positive".to_string());
    }
    Ok(total)
}

/// Quote one word for POSIX `sh`.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:=@%+,".contains(&b));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Build the shell command line for `exec`: a single argument is the command
/// itself (so pipes and globs work); several arguments are quoted and joined.
pub fn shell_join(args: &[String]) -> String {
    match args {
        [one] => one.clone(),
        many => many
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Unix seconds (milliseconds are detected and converted) as `YYYY-MM-DD HH:MM:SS`
/// UTC; zero or negative renders as `-`.
pub fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }
    let secs = if ts > 100_000_000_000 { ts / 1000 } else { ts };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Bytes as a short human string: `512 B`, `1.5 KB`, `12.0 GB`.
pub fn format_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n.unsigned_abs() as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let s = if i == 0 {
        format!("{} B", n.unsigned_abs())
    } else {
        format!("{v:.1} {}", UNITS[i])
    };
    if n < 0 { format!("-{s}") } else { s }
}

/// Micros as dollars with at least two decimals: `$5.00`, `-$0.000123`.
pub fn format_money(micros: i64) -> String {
    let abs = micros.unsigned_abs();
    let mut frac = format!("{:06}", abs % 1_000_000);
    while frac.len() > 2 && frac.ends_with('0') {
        frac.pop();
    }
    format!(
        "{}${}.{frac}",
        if micros < 0 { "-" } else { "" },
        abs / 1_000_000
    )
}

/// Cut to `max` characters with an ellipsis, folding newlines into spaces.
pub fn truncate(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let keep = max.saturating_sub(1);
    let mut out: String = flat.chars().take(keep).collect();
    out.push('…');
    out
}

/// Locate an executable on `PATH`.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_pairs() {
        assert_eq!(
            parse_kv("A=b=c").unwrap(),
            ("A".to_string(), "b=c".to_string())
        );
        assert_eq!(parse_kv("A=").unwrap(), ("A".to_string(), String::new()));
        assert!(parse_kv("=v").is_err());
        assert!(parse_kv("novalue").is_err());
        let env = parse_env_pairs(&["OPENAI_API_KEY=sk".to_string(), "APP_ENV=prod".to_string()])
            .unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env["APP_ENV"], "prod");
        assert!(parse_env_pairs(&["lower=1".to_string()]).is_err());
        assert!(parse_pairs(&["plan=pro".to_string()]).is_ok());
    }

    #[test]
    fn usd_to_micros() {
        assert_eq!(parse_usd("5").unwrap(), 5_000_000);
        assert_eq!(parse_usd("12.50").unwrap(), 12_500_000);
        assert_eq!(parse_usd("$0.25").unwrap(), 250_000);
        assert_eq!(parse_usd(".5").unwrap(), 500_000);
        assert_eq!(parse_usd("0.000001").unwrap(), 1);
        assert!(parse_usd("0").is_err());
        assert!(parse_usd("-5").is_err());
        assert!(parse_usd("abc").is_err());
        assert!(parse_usd("1.0000001").is_err());
        assert!(parse_usd("").is_err());
        assert!(parse_usd("99999999999999999999").is_err());
    }

    #[test]
    fn shell_joining() {
        assert_eq!(shell_join(&["ls | wc -l".to_string()]), "ls | wc -l");
        assert_eq!(
            shell_join(&["ls".to_string(), "-la".to_string(), "/tmp".to_string()]),
            "ls -la /tmp"
        );
        assert_eq!(
            shell_join(&["echo".to_string(), "it's here".to_string()]),
            "echo 'it'\\''s here'"
        );
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn timestamps() {
        assert_eq!(format_timestamp(0), "-");
        assert_eq!(format_timestamp(1_763_000_000), "2025-11-13 02:13:20");
        assert_eq!(format_timestamp(1_763_000_000_000), "2025-11-13 02:13:20");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(format_timestamp(86_399), "1970-01-01 23:59:59");
    }

    #[test]
    fn bytes_and_money() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(4 << 30), "4.0 GB");
        assert_eq!(format_money(5_000_000), "$5.00");
        assert_eq!(format_money(-123), "-$0.000123");
        assert_eq!(format_money(12_500_000), "$12.50");
        assert_eq!(truncate("hello\nworld", 8), "hello w…");
        assert_eq!(truncate("short", 8), "short");
    }
}
