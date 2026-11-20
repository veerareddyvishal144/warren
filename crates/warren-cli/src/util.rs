//! Small parsers and formatters shared by the commands.

use std::collections::BTreeMap;
use std::path::PathBuf;

use warren_core::{Error, Result, SecretSpec, ids};

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

/// One `--secret` flag: `NAME=VALUE@UPSTREAM#HEADER#ENV`. Parsed from the
/// right (`ENV`, then `HEADER`, then the last `@https://`), so a value may
/// hold `=` and `@` but not `#`. The daemon validates the parts again.
pub fn parse_secret_spec(s: &str) -> Result<SecretSpec> {
    let usage = || {
        Error::invalid(format!(
            "secret {s:?} does not match NAME=VALUE@UPSTREAM#HEADER#ENV, e.g. \
             ANTHROPIC_API_KEY=sk-ant-...@https://api.anthropic.com#x-api-key#ANTHROPIC_BASE_URL"
        ))
    };
    let (name, rest) = s.split_once('=').ok_or_else(usage)?;
    let (rest, env) = rest.rsplit_once('#').ok_or_else(usage)?;
    let (rest, header) = rest.rsplit_once('#').ok_or_else(usage)?;
    let at = rest.rfind("@https://").ok_or_else(usage)?;
    let (value, upstream) = (&rest[..at], &rest[at + 1..]);
    let spec = SecretSpec {
        name: name.trim().to_string(),
        upstream: upstream.trim().to_string(),
        header: header.trim().to_string(),
        value: value.to_string(),
        env: env.trim().to_string(),
    };
    for (what, var) in [("NAME", &spec.name), ("ENV", &spec.env)] {
        if !ids::valid_env_key(var) {
            return Err(Error::invalid(format!(
                "secret {what} {var:?} is not a variable name ([A-Z_][A-Z0-9_]*)"
            )));
        }
    }
    if spec.value.is_empty() || spec.header.is_empty() {
        return Err(usage());
    }
    Ok(spec)
}

/// Parse every `--secret` flag.
pub fn parse_secret_specs(items: &[String]) -> Result<Vec<SecretSpec>> {
    items.iter().map(|s| parse_secret_spec(s)).collect()
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
    fn secret_specs() {
        let s = parse_secret_spec(
            "ANTHROPIC_API_KEY=sk-ant-a=b@c@https://api.anthropic.com/v1#x-api-key#ANTHROPIC_BASE_URL",
        )
        .unwrap();
        assert_eq!(s.name, "ANTHROPIC_API_KEY");
        assert_eq!(s.value, "sk-ant-a=b@c");
        assert_eq!(s.upstream, "https://api.anthropic.com/v1");
        assert_eq!(s.header, "x-api-key");
        assert_eq!(s.env, "ANTHROPIC_BASE_URL");
        let s = parse_secret_spec("GH_TOKEN=ghp_x@https://api.github.com#Authorization#GH_API")
            .unwrap();
        assert_eq!(
            (s.header.as_str(), s.env.as_str()),
            ("Authorization", "GH_API")
        );
        for bad in [
            "NAME",
            "NAME=value",
            "NAME=value@https://x.example.com",
            "NAME=value@https://x.example.com#hdr",
            "NAME=value@http://x.example.com#hdr#ENV",
            "lower=value@https://x.example.com#hdr#ENV",
            "NAME=value@https://x.example.com#hdr#bad env",
            "NAME=@https://x.example.com#hdr#ENV",
            "NAME=value@https://x.example.com##ENV",
        ] {
            assert!(parse_secret_spec(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_secret_specs(&[]).unwrap().len(), 0);
    }

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
    fn bytes_and_truncation() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(4 << 30), "4.0 GB");
        assert_eq!(truncate("hello\nworld", 8), "hello w…");
        assert_eq!(truncate("short", 8), "short");
    }
}
