//! Small helpers shared by the harness: harness errors, polling, query
//! encoding, cookie parsing, free ports and a self-removing temp directory.

use std::{
    future::Future,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use warren_core::{Error, Result, json::ErrorEnvelope};

/// Error code the harness uses for its own failures (assertions, process
/// management, transport). Errors answered by the daemon keep their own code.
pub const CODE_E2E: &str = "e2e";

/// Build a harness-level error. Its HTTP status is 0, so tests can tell it apart
/// from an error the daemon answered.
pub fn fail(msg: impl Into<String>) -> Error {
    Error::new(CODE_E2E, 0, msg)
}

/// Wrap a transport or I/O failure with context.
pub fn transport(context: &str, e: impl std::fmt::Display) -> Error {
    fail(format!("{context}: {e}"))
}

/// Return early with a harness error when `cond` is false.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err($crate::fail(format!($($arg)+)));
        }
    };
}

/// Map a non-2xx response to an `Error`. The daemon's code and message survive
/// when the body is the standard `{"error": {...}}` envelope; a plain-text body
/// (for example the edge's 502) becomes code `http_<status>`.
pub fn api_error(status: u16, body: &[u8]) -> Error {
    match serde_json::from_slice::<ErrorEnvelope>(body) {
        Ok(envelope) => {
            let mut e = envelope.error;
            e.status = status;
            e
        }
        Err(_) => Error::new(&format!("http_{status}"), status, preview_bytes(body)),
    }
}

/// Decode a JSON value with a readable error. `from` is normally
/// `serde_json::from_value`; passing it as a function keeps this crate free of
/// a direct `serde` dependency.
pub fn decode<T>(what: &str, value: Value, from: fn(Value) -> serde_json::Result<T>) -> Result<T> {
    let shown = preview_json(&value);
    from(value).map_err(|e| {
        fail(format!(
            "{what}: cannot decode response: {e}; body: {shown}"
        ))
    })
}

/// First 300 characters of a JSON value for error messages.
pub fn preview_json(v: &Value) -> String {
    truncate(&v.to_string(), 300)
}

/// First 300 characters of a body for error messages.
pub fn preview_bytes(body: &[u8]) -> String {
    truncate(String::from_utf8_lossy(body).trim(), 300)
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("...");
    }
    out
}

/// Percent-encode a query value. Unreserved characters and `/` pass through so
/// file paths stay readable; everything else (spaces, `?`, `&`, `%`, ...) is escaped.
pub fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Host part of an http(s) URL, lower-cased and without port or userinfo.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        format!("[{}]", v6.split(']').next()?)
    } else {
        authority.split(':').next()?.to_string()
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Value of cookie `name` in one `Set-Cookie` header value, if that header sets it.
pub fn cookie_value<'a>(set_cookie: &'a str, name: &str) -> Option<&'a str> {
    let first = set_cookie.split(';').next()?.trim();
    let (k, v) = first.split_once('=')?;
    if k.trim() == name {
        Some(v.trim())
    } else {
        None
    }
}

/// Bind port 0 on the dual-stack wildcard address to find a free TCP port.
pub fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("[::]:0")
        .or_else(|_| TcpListener::bind("0.0.0.0:0"))
        .map_err(|e| transport("bind a free port", e))?;
    Ok(listener
        .local_addr()
        .map_err(|e| transport("local_addr", e))?
        .port())
}

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Call `f` every `interval` until it yields `Some`, failing after `timeout`.
/// Errors from `f` end the wait immediately; map tolerated errors to `Ok(None)`.
pub async fn poll_until<T, F, Fut>(
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut f: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let start = Instant::now();
    loop {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        if start.elapsed() >= timeout {
            return Err(fail(format!(
                "timed out after {timeout:?} waiting for {what}"
            )));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Last `n` lines of a text file; empty when the file cannot be read.
pub fn tail_lines(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// A fresh directory under `$WARREN_E2E_TMP` (default: the system temp dir, which
/// `scripts/env.sh` points at `/data/tmp`), removed on drop unless
/// `WARREN_E2E_KEEP=1` or [`TempRoot::keep`] was called.
pub struct TempRoot {
    path: PathBuf,
    keep: bool,
}

impl TempRoot {
    /// Create `<tmp>/<prefix>-<pid>-<n>-<nanos>`.
    pub fn new(prefix: &str) -> Result<TempRoot> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::var_os("WARREN_E2E_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = base.join(format!("{prefix}-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path)
            .map_err(|e| transport(&format!("create {}", path.display()), e))?;
        let keep = std::env::var("WARREN_E2E_KEEP")
            .map(|v| v == "1")
            .unwrap_or(false);
        Ok(TempRoot { path, keep })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the directory after drop (its path is printed).
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if self.keep {
            eprintln!("[e2e] kept {}", self.path.display());
            return;
        }
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // Files the gateway wrote as root into a bind-mounted home are
                // not removable by the dev account; a throwaway container is.
                eprintln!(
                    "[e2e] {} holds files this user cannot remove ({e}); removing them through a container",
                    self.path.display()
                );
                if let Err(e) = remove_with_docker(&self.path) {
                    eprintln!("[e2e] could not remove {}: {e}", self.path.display());
                }
            }
            Err(e) => eprintln!("[e2e] could not remove {}: {e}", self.path.display()),
        }
    }
}

/// Remove a directory tree owned by another user by running `rm -rf` in a
/// root container with the directory bind-mounted, then remove the (now empty)
/// directory itself.
fn remove_with_docker(path: &Path) -> std::io::Result<()> {
    let mount = format!("{}:/cleanup", path.display());
    let status = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &mount,
            "alpine:3.20",
            "sh",
            "-c",
            "rm -rf /cleanup/* /cleanup/.[!.]* 2>/dev/null; true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "docker run exited with {status}"
        )));
    }
    std::fs::remove_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode_query("notes/a.txt"), "notes/a.txt");
        assert_eq!(encode_query("../x y?&%"), "../x%20y%3F%26%25");
        assert_eq!(encode_query(""), "");
    }

    #[test]
    fn extracts_hosts() {
        assert_eq!(
            host_of("http://api.warren.localhost:7337/v1/health").as_deref(),
            Some("api.warren.localhost")
        );
        assert_eq!(
            host_of("http://AB12cd34ef-7681.warren.localhost:1/?x=1").as_deref(),
            Some("ab12cd34ef-7681.warren.localhost")
        );
        assert_eq!(
            host_of("http://user:pw@example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:7337/").as_deref(), Some("[::1]"));
        assert_eq!(host_of("http:///nohost"), None);
    }

    #[test]
    fn parses_cookies() {
        let header = "warren_session=abc.def; HttpOnly; SameSite=Lax; Path=/";
        assert_eq!(cookie_value(header, "warren_session"), Some("abc.def"));
        assert_eq!(cookie_value(header, "other"), None);
        assert_eq!(cookie_value("garbage", "warren_session"), None);
    }

    #[test]
    fn maps_api_errors() {
        let body = br#"{"error":{"code":"instance_limit_reached","message":"limit","details":{"limit":3}}}"#;
        let e = api_error(409, body);
        assert_eq!(e.status, 409);
        assert_eq!(e.code, "instance_limit_reached");
        assert_eq!(e.message, "limit");
        assert_eq!(
            e.details.as_ref().and_then(|d| d["limit"].as_u64()),
            Some(3)
        );
        let e = api_error(502, b"this template has no default port");
        assert_eq!(e.code, "http_502");
        assert_eq!(e.status, 502);
        assert_eq!(e.message, "this template has no default port");
    }

    #[test]
    fn decodes_with_context() {
        let ok: Vec<u32> =
            decode("numbers", serde_json::json!([1, 2]), serde_json::from_value).unwrap();
        assert_eq!(ok, vec![1, 2]);
        let err = decode::<Vec<u32>>(
            "numbers",
            serde_json::json!({"x": 1}),
            serde_json::from_value,
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_E2E);
        assert!(err.message.contains("numbers"), "{err}");
    }

    #[test]
    fn finds_a_free_port() {
        let p = free_port().unwrap();
        assert_ne!(p, 0);
        TcpListener::bind(("127.0.0.1", p)).expect("the port is free right after probing");
    }

    #[test]
    fn temp_root_is_removed_on_drop() {
        let path = {
            let root = TempRoot::new("warren-e2e-test").unwrap();
            assert!(root.path().is_dir());
            std::fs::write(root.path().join("f"), b"x").unwrap();
            root.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[test]
    fn tails_files() {
        let root = TempRoot::new("warren-e2e-tail").unwrap();
        let p = root.path().join("log");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        assert_eq!(tail_lines(&p, 2), "b\nc");
        assert_eq!(tail_lines(&p, 10), "a\nb\nc");
        assert_eq!(tail_lines(&root.path().join("missing"), 2), "");
    }

    #[tokio::test]
    async fn polls_until_some() {
        let mut n = 0;
        let v = poll_until(
            "three calls",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                n += 1;
                let done = n >= 3;
                async move { Ok(if done { Some(n) } else { None }) }
            },
        )
        .await
        .unwrap();
        assert_eq!(v, 3);
        let err = poll_until::<(), _, _>(
            "never",
            Duration::from_millis(5),
            Duration::from_millis(1),
            || async { Ok(None) },
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("never"), "{err}");
        let err = poll_until::<(), _, _>(
            "boom",
            Duration::from_secs(1),
            Duration::from_millis(1),
            || async { Err(fail("boom")) },
        )
        .await
        .unwrap_err();
        assert_eq!(err.message, "boom");
    }
}
