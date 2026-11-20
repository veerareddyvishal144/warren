//! `warren shell`: an interactive terminal inside an instance over the
//! gateway's `GET /v1/pty` WebSocket, and the terminal plumbing `warren run`
//! uses for its command.
//!
//! Through the edge the socket is opened at `ws://<id>.<base>[:port]/v1/pty`
//! with the workspace key in `X-Warren-Key`, the way every other instance
//! request authenticates. The TCP connection goes to the Sandbox API's host
//! and port (the daemon's single listener, the address the CLI already talks
//! to) while the request carries the instance host in `Host`, so the edge
//! dispatches it like any instance request; this also sidesteps resolvers
//! that do not answer for `*.localhost` names (macOS). The hidden
//! `--gateway-url`/`--gateway-token` pair connects straight to a gateway with
//! `X-Warren-Instance-Token`, for testing without a daemon.
//!
//! Locally the terminal is put in raw mode while the session runs, stdin is
//! forwarded as binary frames, output is written to stdout unchanged,
//! `SIGWINCH` becomes a resize frame and the remote exit code becomes the
//! process exit code.

use std::{
    io::{Read as _, Write as _},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::fd::AsFd,
    time::Duration,
};

use futures::{SinkExt, StreamExt};
use nix::sys::termios::{self, SetArg, Termios};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    signal::unix::{SignalKind, signal},
    sync::mpsc,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{self, Bytes, Message, client::IntoClientRequest, http::Uri},
};
use warren_cli::{Failure, cli::ShellArgs, util::shell_join};
use warren_core::{Error, HEADER_API_KEY, HEADER_INSTANCE_TOKEN, errors};

use super::Ctx;

/// The WebSocket to a PTY, plain or over TLS.
pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Time to establish the TCP connection to one address.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Terminal size sent when the local size is unknown (stdout is not a tty).
pub const DEFAULT_COLS: u16 = 80;
/// See [`DEFAULT_COLS`].
pub const DEFAULT_ROWS: u16 = 24;

// The window-size ioctl through nix's macro so the request number has the
// right type on glibc, musl and macOS.
nix::ioctl_read_bad!(tiocgwinsz, nix::libc::TIOCGWINSZ, nix::libc::winsize);

/// Where the PTY WebSocket is opened and how it authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyTarget {
    /// `ws://` or `wss://` URL of `/v1/pty`, without a query string.
    pub url: String,
    /// Host and port the TCP connection goes to; the URL's own authority when `None`.
    pub connect_to: Option<(String, u16)>,
    /// Request headers added to the handshake.
    pub headers: Vec<(&'static str, String)>,
}

/// `warren shell [ID] [--cwd PATH] [-- COMMAND...]`
pub async fn shell(ctx: &Ctx, args: ShellArgs) -> Result<i32, Failure> {
    let cmd = (!args.command.is_empty()).then(|| shell_join(&args.command));
    let target = match (&args.gateway_url, &args.gateway_token) {
        (Some(url), Some(token)) => gateway_target(url, token)?,
        _ => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| Failure::usage("an instance id is required"))?;
            let api = ctx.api()?;
            let inst = api.get_instance(id).await?;
            instance_target(ctx, &inst)?
        }
    };
    run_command(&target, cmd.as_deref(), args.cwd.as_deref()).await
}

/// The target for an instance object as the Sandbox API returns it: its
/// `url` through the edge, authenticated with the configured key. A stopped
/// instance is refused here (the edge would answer `409`) with a hint.
pub fn instance_target(ctx: &Ctx, inst: &Value) -> Result<PtyTarget, Failure> {
    let id = str_field(inst, "id");
    let status = str_field(inst, "status");
    let url = str_field(inst, "url");
    if status == "stopped" {
        return Err(Failure::Api(Error::conflict(format!(
            "instance {id} is stopped; start it with 'warren instances start {id}'"
        ))));
    }
    if url.is_empty() {
        return Err(Failure::Api(Error::conflict(format!(
            "instance {id} has no URL yet (status {status})"
        ))));
    }
    let key = ctx.settings.api_key.as_deref().unwrap_or_default();
    edge_target(url, &ctx.settings.api_url, key)
}

/// The target for an instance through the edge: the instance URL's host in
/// the request, the Sandbox API's host and port on the wire.
pub fn edge_target(instance_url: &str, api_url: &str, key: &str) -> Result<PtyTarget, Failure> {
    let url = ws_url(instance_url)?;
    let api: Uri = api_url
        .trim()
        .parse()
        .map_err(|e| Failure::usage(format!("invalid API URL {api_url:?}: {e}")))?;
    let host = api
        .host()
        .ok_or_else(|| Failure::usage(format!("API URL {api_url:?} has no host")))?
        .to_string();
    let port = api
        .port_u16()
        .unwrap_or_else(|| default_port(api.scheme_str()));
    Ok(PtyTarget {
        url,
        connect_to: Some((host, port)),
        headers: vec![(HEADER_API_KEY, key.to_string())],
    })
}

/// The target for a gateway reached directly (no daemon, no edge).
pub fn gateway_target(gateway_url: &str, token: &str) -> Result<PtyTarget, Failure> {
    Ok(PtyTarget {
        url: ws_url(gateway_url)?,
        connect_to: None,
        headers: vec![(HEADER_INSTANCE_TOKEN, token.to_string())],
    })
}

/// `http(s)://host[:port][/...]` (or `ws(s)://`) to `ws(s)://host[:port]/v1/pty`.
pub fn ws_url(http_url: &str) -> Result<String, Failure> {
    let uri: Uri = http_url
        .trim()
        .parse()
        .map_err(|e| Failure::usage(format!("invalid URL {http_url:?}: {e}")))?;
    let scheme = match uri.scheme_str() {
        Some("http") | Some("ws") => "ws",
        Some("https") | Some("wss") => "wss",
        other => {
            return Err(Failure::usage(format!(
                "unsupported URL scheme {:?} in {http_url:?}",
                other.unwrap_or("")
            )));
        }
    };
    let authority = uri
        .authority()
        .ok_or_else(|| Failure::usage(format!("URL {http_url:?} has no host")))?;
    Ok(format!("{scheme}://{authority}/v1/pty"))
}

fn default_port(scheme: Option<&str>) -> u16 {
    match scheme {
        Some("https") | Some("wss") => 443,
        _ => 80,
    }
}

/// The query string of the PTY request.
pub fn pty_query(
    cmd: Option<&str>,
    cwd: Option<&str>,
    cols: u16,
    rows: u16,
    term: Option<&str>,
) -> String {
    let mut q = vec![("cols", cols.to_string()), ("rows", rows.to_string())];
    if let Some(c) = cmd.filter(|c| !c.is_empty()) {
        q.push(("cmd", c.to_string()));
    }
    if let Some(d) = cwd.filter(|d| !d.is_empty()) {
        q.push(("cwd", d.to_string()));
    }
    if let Some(t) = term.filter(|t| !t.is_empty()) {
        q.push(("term", t.to_string()));
    }
    q.iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode everything but RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
    }
    out
}

/// A `TERM` value the gateway accepts (letters, digits, `-_.+`, at most 64).
pub fn valid_term(term: &str) -> bool {
    !term.is_empty()
        && term.len() <= 64
        && term
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
}

/// `localhost` and every name under it always mean this machine (RFC 6761),
/// whatever the resolver says.
pub fn is_local_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost")
}

/// Open the terminal with the local size and `TERM`, run `cmd` (or a login
/// shell) and return its exit code.
pub async fn run_command(
    target: &PtyTarget,
    cmd: Option<&str>,
    cwd: Option<&str>,
) -> Result<i32, Failure> {
    let (cols, rows) = terminal_size().unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    let term = std::env::var("TERM").ok().filter(|t| valid_term(t));
    let query = pty_query(cmd, cwd, cols, rows, term.as_deref());
    let ws = connect(target, &query).await?;
    run_terminal(ws).await
}

/// Connect the TCP socket, run the WebSocket handshake with the target's
/// headers and return the open socket.
pub async fn connect(target: &PtyTarget, query: &str) -> Result<Ws, Failure> {
    let url = if query.is_empty() {
        target.url.clone()
    } else {
        format!("{}?{query}", target.url)
    };
    let uri: Uri = url
        .parse()
        .map_err(|e| Failure::local(format!("invalid WebSocket URL {url:?}: {e}")))?;
    let tls = uri.scheme_str() == Some("wss");
    let (host, port) = match &target.connect_to {
        Some((h, p)) => (h.clone(), *p),
        None => (
            uri.host().unwrap_or("localhost").to_string(),
            uri.port_u16().unwrap_or(if tls { 443 } else { 80 }),
        ),
    };
    let tcp = connect_tcp(&host, port).await?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| Failure::local(format!("cannot build the WebSocket request: {e}")))?;
    for (name, value) in &target.headers {
        let v = value
            .parse()
            .map_err(|_| Failure::usage(format!("header {name} has an invalid value")))?;
        request.headers_mut().insert(*name, v);
    }
    let result = if tls {
        tokio_tungstenite::client_async_tls(request, tcp).await
    } else {
        tokio_tungstenite::client_async(request, MaybeTlsStream::Plain(tcp)).await
    };
    result.map(|(ws, _response)| ws).map_err(ws_failure)
}

/// Connect to `host:port`: every resolved address in turn, then the loopback
/// addresses for names under `localhost`.
async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, Failure> {
    let mut last: Option<String> = None;
    let mut candidates: Vec<SocketAddr> = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            last = Some(e.to_string());
            Vec::new()
        }
    };
    if is_local_name(host) {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let addr = SocketAddr::new(ip, port);
            if !candidates.contains(&addr) {
                candidates.push(addr);
            }
        }
    }
    for addr in candidates {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(e)) => last = Some(format!("{addr}: {e}")),
            Err(_) => last = Some(format!("{addr}: connect timed out")),
        }
    }
    Err(Failure::local(format!(
        "cannot connect to {host}:{port}: {}",
        last.unwrap_or_else(|| "no address".to_string())
    )))
}

/// A failed handshake carrying the daemon's or gateway's JSON error envelope
/// becomes the same `Failure::Api` every other command reports.
fn ws_failure(e: tungstenite::Error) -> Failure {
    match e {
        tungstenite::Error::Http(resp) => {
            let status = resp.status().as_u16();
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            Failure::Api(api_error(status, &body))
        }
        other => Failure::local(format!("websocket: {other}")),
    }
}

/// The error for an HTTP status and body: the decoded envelope when the body
/// is one, else a generic error for the status.
pub fn api_error(status: u16, body: &str) -> Error {
    if let Ok(v) = serde_json::from_str::<Value>(body)
        && let Some(inner) = v.get("error")
        && let Ok(mut e) = serde_json::from_value::<Error>(inner.clone())
    {
        e.status = status;
        return e;
    }
    let message = if body.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {}", body.trim())
    };
    Error::new(code_for_status(status), status, message)
}

/// The contract's error code for an HTTP status.
pub fn code_for_status(status: u16) -> &'static str {
    match status {
        400 => errors::CODE_INVALID_REQUEST,
        401 => errors::CODE_INVALID_API_KEY,
        403 => errors::CODE_FORBIDDEN,
        404 => errors::CODE_NOT_FOUND,
        409 => errors::CODE_CONFLICT,
        502 => errors::CODE_BAD_GATEWAY,
        503 => errors::CODE_TRY_AGAIN,
        504 => errors::CODE_TIMEOUT,
        _ => errors::CODE_INTERNAL,
    }
}

/// A text frame from the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// The process exited with this code.
    Exit(i32),
    /// The gateway could not start the process.
    Error(String),
    /// Anything else (ignored).
    Other,
}

/// Decode a text frame.
pub fn control(text: &str) -> Control {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Control::Other;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("exit") => Control::Exit(
            v.get("code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok())
                .unwrap_or(1),
        ),
        Some("error") => Control::Error(
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        ),
        _ => Control::Other,
    }
}

/// The resize frame for a new local size.
pub fn resize_frame(cols: u16, rows: u16) -> Message {
    Message::Text(
        json!({ "type": "resize", "cols": cols, "rows": rows })
            .to_string()
            .into(),
    )
}

/// Columns and rows of the local terminal (stdout, else stdin, else
/// stderr); `None` when none of them is a terminal.
pub fn terminal_size() -> Option<(u16, u16)> {
    let mut size = nix::libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    for fd in [1, 0, 2] {
        // SAFETY: `TIOCGWINSZ` writes one `winsize` into the pointer, which
        // outlives the call; `fd` is one of the standard descriptors.
        let ok = unsafe { tiocgwinsz(fd, &mut size) }.is_ok();
        if ok && size.ws_col > 0 && size.ws_row > 0 {
            return Some((size.ws_col, size.ws_row));
        }
    }
    None
}

/// Raw mode on stdin for the duration of the session; the original settings
/// come back when this is dropped.
struct RawMode {
    original: Termios,
}

impl RawMode {
    /// Enable raw mode; `None` when stdin is not a terminal.
    fn enable() -> std::io::Result<Option<RawMode>> {
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        if !nix::unistd::isatty(fd).unwrap_or(false) {
            return Ok(None);
        }
        let original = termios::tcgetattr(fd)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSANOW, &raw)?;
        Ok(Some(RawMode { original }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        let _ = termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &self.original);
    }
}

/// Blocking stdin reader; ends at EOF or when the session is gone.
fn read_stdin(tx: mpsc::Sender<Vec<u8>>) {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Drive an open PTY socket with the local terminal until the remote
/// process exits or the connection ends. Returns the remote exit code.
pub async fn run_terminal(mut ws: Ws) -> Result<i32, Failure> {
    let raw = RawMode::enable()?;
    let stdin_is_tty = raw.is_some();
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || read_stdin(in_tx));
    let mut winch = signal(SignalKind::window_change())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut stdout = std::io::stdout();
    let mut code: Option<i32> = None;
    let mut remote_error: Option<String> = None;
    let mut stdin_open = true;
    let outcome: Result<(), Failure> = loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Err(e) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
                        break Err(e.into());
                    }
                }
                Some(Ok(Message::Text(text))) => match control(text.as_str()) {
                    Control::Exit(c) => code = Some(c),
                    Control::Error(m) => remote_error = Some(m),
                    Control::Other => {}
                },
                Some(Ok(Message::Close(_))) | None => break Ok(()),
                Some(Err(e)) => {
                    break if code.is_some() {
                        Ok(())
                    } else {
                        Err(Failure::local(format!("connection lost: {e}")))
                    };
                }
                Some(Ok(_)) => {}
            },
            data = in_rx.recv(), if stdin_open => match data {
                Some(bytes) => {
                    if let Err(e) = ws.send(Message::Binary(Bytes::from(bytes))).await {
                        break if code.is_some() { Ok(()) } else { Err(ws_failure(e)) };
                    }
                }
                None => {
                    stdin_open = false;
                    if !stdin_is_tty {
                        // Piped input ended: hand the terminal an end-of-input.
                        let _ = ws.send(Message::Binary(Bytes::from_static(b"\x04"))).await;
                    }
                }
            },
            _ = winch.recv() => {
                if let Some((cols, rows)) = terminal_size() {
                    let _ = ws.send(resize_frame(cols, rows)).await;
                }
            }
            _ = terminate.recv() => break Err(Failure::local("terminated")),
        }
    };
    // The terminal settings come back before anything is printed.
    drop(raw);
    outcome?;
    if let Some(m) = remote_error {
        return Err(Failure::local(format!(
            "the instance could not start the command: {m}"
        )));
    }
    code.ok_or_else(|| Failure::local("the connection closed before the command finished"))
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_targets() {
        assert_eq!(
            ws_url("http://ab12cd34ef.warren.localhost:7337").unwrap(),
            "ws://ab12cd34ef.warren.localhost:7337/v1/pty"
        );
        assert_eq!(
            ws_url("https://ab12cd34ef.warren.example.com/").unwrap(),
            "wss://ab12cd34ef.warren.example.com/v1/pty"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:38737/v1/").unwrap(),
            "ws://127.0.0.1:38737/v1/pty"
        );
        assert!(ws_url("ftp://x").is_err());
        assert!(ws_url("not a url").is_err());

        let t = edge_target(
            "http://ab12cd34ef.warren.localhost:7337",
            "http://api.warren.localhost:7337",
            "sk_live_k",
        )
        .unwrap();
        assert_eq!(t.url, "ws://ab12cd34ef.warren.localhost:7337/v1/pty");
        assert_eq!(
            t.connect_to,
            Some(("api.warren.localhost".to_string(), 7337))
        );
        assert_eq!(t.headers, vec![(HEADER_API_KEY, "sk_live_k".to_string())]);
        let t = edge_target(
            "https://ab12cd34ef.warren.example.com",
            "https://warren.example.com",
            "k",
        )
        .unwrap();
        assert_eq!(t.url, "wss://ab12cd34ef.warren.example.com/v1/pty");
        assert_eq!(t.connect_to, Some(("warren.example.com".to_string(), 443)));

        let g = gateway_target("http://127.0.0.1:38737", "tok").unwrap();
        assert_eq!(g.url, "ws://127.0.0.1:38737/v1/pty");
        assert_eq!(g.connect_to, None);
        assert_eq!(g.headers, vec![(HEADER_INSTANCE_TOKEN, "tok".to_string())]);
    }

    #[test]
    fn query_encoding() {
        assert_eq!(pty_query(None, None, 80, 24, None), "cols=80&rows=24");
        assert_eq!(
            pty_query(
                Some("echo hi; exit 4"),
                Some("work/app"),
                132,
                50,
                Some("xterm-256color")
            ),
            "cols=132&rows=50&cmd=echo%20hi%3B%20exit%204&cwd=work%2Fapp&term=xterm-256color"
        );
        assert_eq!(
            pty_query(Some(""), Some(""), 1, 1, Some("")),
            "cols=1&rows=1"
        );
        assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert!(valid_term("xterm-256color"));
        assert!(valid_term("screen.xterm"));
        assert!(!valid_term(""));
        assert!(!valid_term("a b"));
        assert!(!valid_term(&"x".repeat(65)));
    }

    #[test]
    fn local_names() {
        assert!(is_local_name("localhost"));
        assert!(is_local_name("api.warren.localhost"));
        assert!(is_local_name("API.WARREN.LOCALHOST."));
        assert!(!is_local_name("localhost.example.com"));
        assert!(!is_local_name("127.0.0.1"));
        assert!(!is_local_name("warren.example.com"));
    }

    #[test]
    fn control_frames_and_errors() {
        assert_eq!(control(r#"{"type":"exit","code":4}"#), Control::Exit(4));
        assert_eq!(control(r#"{"type":"exit"}"#), Control::Exit(1));
        assert_eq!(
            control(r#"{"type":"error","message":"no shell"}"#),
            Control::Error("no shell".into())
        );
        assert_eq!(control(r#"{"type":"resize"}"#), Control::Other);
        assert_eq!(control("garbage"), Control::Other);
        let Message::Text(t) = resize_frame(100, 30) else {
            panic!()
        };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(v, json!({ "type": "resize", "cols": 100, "rows": 30 }));

        let e = api_error(
            401,
            r#"{"error":{"code":"invalid_api_key","message":"missing key"}}"#,
        );
        assert_eq!((e.status, e.code.as_str()), (401, "invalid_api_key"));
        assert_eq!(e.message, "missing key");
        let e = api_error(502, "this template has no default port");
        assert_eq!((e.status, e.code.as_str()), (502, "bad_gateway"));
        assert!(e.message.contains("no default port"));
        let e = api_error(500, "");
        assert_eq!((e.status, e.code.as_str()), (500, "internal"));
        assert_eq!(code_for_status(404), "not_found");
        assert_eq!(code_for_status(409), "conflict");
        assert_eq!(code_for_status(504), "timeout");
        assert_eq!(code_for_status(418), "internal");
    }
}
