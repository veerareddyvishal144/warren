//! `GET /v1/pty`: an interactive terminal over a WebSocket.
//!
//! The request upgrades to a WebSocket and the gateway starts `/bin/sh -l`
//! (or `?cmd=<shell command>` through `/bin/sh -lc`) on a pseudo-terminal,
//! as the home owner and with the same environment hiding as `/v1/exec`
//! (see `exec.rs`). Binary frames carry terminal bytes in both directions.
//! A text frame `{"type":"resize","cols":N,"rows":N}` resizes the terminal.
//! When the process exits the gateway sends the text frame
//! `{"type":"exit","code":N}` and closes the socket; when the client closes
//! first, the process group behind the terminal is hung up and then killed.
//!
//! Query parameters: `cmd` (shell command; absent for an interactive shell),
//! `cwd` (working directory relative to the home), `cols` and `rows` (initial
//! size, default 80x24) and `term` (the `TERM` value, default
//! `xterm-256color`). The instance token is enforced by the router's
//! middleware like every other route.

use std::{
    io,
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{
        RawQuery, State,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, close_code},
    },
    response::Response,
};
use nix::{
    fcntl::{FcntlArg, OFlag, fcntl},
    libc,
    pty::{Winsize, openpty},
    sys::{
        signal::{Signal, killpg},
        termios::Termios,
    },
    unistd::Pid,
};
use serde::Deserialize;
use serde_json::json;
use tokio::{io::unix::AsyncFd, process::Child, sync::mpsc};
use warren_core::{Error, Result};

use crate::{
    exec::{exit_code, kill_tree, owner_command, owner_shell},
    extract::query_param,
    owner::HomeOwner,
    server::SharedState,
};

/// Terminal width when the client names none.
pub const DEFAULT_COLS: u16 = 80;
/// Terminal height when the client names none.
pub const DEFAULT_ROWS: u16 = 24;
/// `TERM` when the client names none.
pub const DEFAULT_TERM: &str = "xterm-256color";
/// Longest accepted `TERM` value.
const MAX_TERM_LEN: usize = 64;
/// After the process exits, pending terminal output is forwarded for at
/// most this long before the exit frame is sent.
const DRAIN: Duration = Duration::from_millis(500);
/// After the client closes, the process group gets SIGHUP and this long to
/// go away before SIGKILL.
const HANGUP_GRACE: Duration = Duration::from_millis(500);
/// Bytes read from the terminal at once.
const READ_CHUNK: usize = 16 * 1024;
/// Output frames buffered between the terminal reader and the socket.
const OUTPUT_QUEUE: usize = 64;

// Terminal ioctls through nix's macros so the request number has the right
// type on glibc, musl and the BSDs (macOS included).
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);
nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);

/// What a client asked for in the query string of `GET /v1/pty`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyRequest {
    /// Shell command run through `/bin/sh -lc`; empty means an interactive
    /// `/bin/sh -l`.
    pub cmd: String,
    /// Working directory relative to the home; empty means the home itself.
    pub cwd: String,
    /// Initial width in columns.
    pub cols: u16,
    /// Initial height in rows.
    pub rows: u16,
    /// Value of `TERM` for the process.
    pub term: String,
}

impl Default for PtyRequest {
    fn default() -> Self {
        PtyRequest {
            cmd: String::new(),
            cwd: String::new(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            term: DEFAULT_TERM.to_string(),
        }
    }
}

/// Parse the raw query string of `GET /v1/pty`.
pub fn parse_request(raw: Option<&str>) -> Result<PtyRequest> {
    let mut req = PtyRequest {
        cmd: query_param(raw, "cmd")?.unwrap_or_default(),
        cwd: query_param(raw, "cwd")?
            .unwrap_or_default()
            .trim()
            .to_string(),
        ..PtyRequest::default()
    };
    if req.cmd.contains('\0') || req.cwd.contains('\0') {
        return Err(Error::invalid("cmd and cwd must not contain NUL bytes"));
    }
    req.cols = dimension(raw, "cols", DEFAULT_COLS)?;
    req.rows = dimension(raw, "rows", DEFAULT_ROWS)?;
    if let Some(term) = query_param(raw, "term")?.filter(|t| !t.trim().is_empty()) {
        let term = term.trim();
        let ok = term.len() <= MAX_TERM_LEN
            && term
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b));
        if !ok {
            return Err(Error::invalid(format!("invalid term {term:?}")));
        }
        req.term = term.to_string();
    }
    Ok(req)
}

fn dimension(raw: Option<&str>, key: &str, default: u16) -> Result<u16> {
    match query_param(raw, key)? {
        None => Ok(default),
        Some(v) if v.trim().is_empty() => Ok(default),
        Some(v) => match v.trim().parse::<u16>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(Error::invalid(format!(
                "{key} must be a positive integer below 65536, got {v:?}"
            ))),
        },
    }
}

/// The working directory for `cwd`: the home when empty, else `home/cwd`,
/// which must exist (the same rule as exec's `workdir`).
pub fn resolve_workdir(home: &Path, cwd: &str) -> Result<PathBuf> {
    if cwd.is_empty() {
        return Ok(home.to_path_buf());
    }
    let dir = home.join(cwd);
    if !dir.is_dir() {
        return Err(Error::invalid(format!("cwd {cwd} does not exist")));
    }
    Ok(dir)
}

/// `GET /v1/pty`: validate the request, upgrade, then run the terminal
/// session until the process exits or the client goes away.
pub async fn pty_handler(
    State(state): State<SharedState>,
    RawQuery(q): RawQuery,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let req = parse_request(q.as_deref())?;
    let workdir = resolve_workdir(&state.home, &req.cwd)?;
    let home = state.home.clone();
    let owner = state.owner.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        match spawn(&owner, &home, &workdir, &req) {
            Ok(session) => run_session(socket, session).await,
            Err(e) => {
                tracing::warn!(error = %e, "cannot start the terminal process");
                let mut socket = socket;
                let _ = socket
                    .send(control_frame(
                        json!({ "type": "error", "message": e.message }),
                    ))
                    .await;
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::ERROR,
                        reason: Utf8Bytes::from_static("cannot start process"),
                    })))
                    .await;
            }
        }
    }))
}

/// A running terminal process and the master side of its pseudo-terminal.
struct Session {
    child: Child,
    master: Arc<AsyncFd<OwnedFd>>,
}

/// Open a pseudo-terminal and start the process on its slave side, as the
/// home owner, in its own session with the terminal as controlling tty.
fn spawn(owner: &HomeOwner, home: &Path, workdir: &Path, req: &PtyRequest) -> Result<Session> {
    let size = winsize(req.cols, req.rows);
    let pty = openpty(&size, None::<&Termios>).map_err(|e| {
        Error::internal(format!(
            "cannot open a pseudo-terminal: {}",
            io::Error::from(e)
        ))
    })?;
    let mut cmd = if req.cmd.trim().is_empty() {
        owner_command(owner, &["/bin/sh".to_string(), "-l".to_string()])?
    } else {
        owner_shell(owner, &req.cmd)
    };
    cmd.current_dir(workdir)
        .env("HOME", home)
        .env("TERM", &req.term)
        .kill_on_drop(true);
    let slave = pty.slave;
    let dup = |fd: &OwnedFd| -> Result<Stdio> {
        Ok(Stdio::from(fd.try_clone().map_err(|e| {
            Error::internal(format!("cannot duplicate the terminal descriptor: {e}"))
        })?))
    };
    cmd.stdin(dup(&slave)?).stdout(dup(&slave)?);
    cmd.stderr(Stdio::from(slave));
    // SAFETY: the closure runs in the forked child before `exec`. `setsid`
    // and the `TIOCSCTTY` ioctl are async-signal-safe and act on the child
    // only: they start a new session and make the slave (fd 0) its
    // controlling terminal, so job control and SIGHUP on close work.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(io::Error::from)?;
            tiocsctty(0, 0).map_err(io::Error::from)?;
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(spawn_error)?;
    // The command still holds the slave descriptors; close them here so the
    // master reports end-of-file once the process (and its children) exit.
    drop(cmd);
    fcntl(&pty.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(|e| {
        Error::internal(format!(
            "cannot set the terminal non-blocking: {}",
            io::Error::from(e)
        ))
    })?;
    // SAFETY: `pty.master` is an `OwnedFd`, so the descriptor stays open and
    // unchanged for as long as the `AsyncFd` owns it; nothing else closes it.
    let master = unsafe { AsyncFd::register(pty.master) }
        .map_err(|e| Error::internal(format!("cannot register the terminal: {e}")))?;
    Ok(Session {
        child,
        master: Arc::new(master),
    })
}

fn spawn_error(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::NotFound => Error::invalid(format!("cannot start command: {e}")),
        io::ErrorKind::PermissionDenied => Error::forbidden(format!("cannot start command: {e}")),
        _ => Error::internal(format!("cannot start command: {e}")),
    }
}

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Set the terminal size on the master descriptor.
pub fn resize(master: &OwnedFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = winsize(cols, rows);
    // SAFETY: `TIOCSWINSZ` reads one `winsize` struct from the pointer, which
    // outlives the call.
    unsafe { tiocswinsz(master.as_raw_fd(), &size) }
        .map(|_| ())
        .map_err(io::Error::from)
}

/// A control message from the client (text frames carry JSON).
#[derive(Debug, Deserialize)]
struct Control {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
}

/// Apply a text frame: `resize` changes the terminal size; anything else is
/// ignored. Returns what was done, for logging and tests.
pub fn apply_control(master: &OwnedFd, text: &str) -> io::Result<Option<(u16, u16)>> {
    let Ok(c) = serde_json::from_str::<Control>(text) else {
        return Ok(None);
    };
    if c.kind == "resize" && c.cols > 0 && c.rows > 0 {
        resize(master, c.cols, c.rows)?;
        return Ok(Some((c.cols, c.rows)));
    }
    Ok(None)
}

fn control_frame(v: serde_json::Value) -> Message {
    Message::Text(Utf8Bytes::from(v.to_string()))
}

/// Why the session's main loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The process exited with this code.
    Exited(i32),
    /// The terminal reported end-of-file (every holder of the slave is gone).
    TerminalClosed,
    /// The client closed the socket or the socket failed.
    ClientGone,
}

/// Shuttle bytes until the process exits or the client leaves, then report
/// the exit code and clean up.
async fn run_session(mut socket: WebSocket, mut session: Session) {
    let (tx, mut rx) = mpsc::channel::<Bytes>(OUTPUT_QUEUE);
    let reader = tokio::spawn(read_terminal(session.master.clone(), tx));
    let end = loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(bytes) => {
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        break End::ClientGone;
                    }
                }
                None => break End::TerminalClosed,
            },
            status = session.child.wait() => {
                break End::Exited(status.map(exit_code).unwrap_or(1));
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    if let Err(e) = write_terminal(&session.master, &data).await {
                        tracing::debug!(error = %e, "terminal write failed");
                        break End::TerminalClosed;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    match apply_control(session.master.get_ref(), text.as_str()) {
                        Ok(Some((cols, rows))) => tracing::debug!(cols, rows, "terminal resized"),
                        Ok(None) => tracing::debug!(frame = %text.as_str(), "ignored control frame"),
                        Err(e) => tracing::debug!(error = %e, "resize failed"),
                    }
                }
                Some(Ok(Message::Close(_))) | None => break End::ClientGone,
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "websocket error");
                    break End::ClientGone;
                }
                Some(Ok(_)) => {} // pings and pongs are answered by axum
            }
        }
    };

    let code = match end {
        End::Exited(code) => Some(code),
        End::TerminalClosed => {
            // Every slave descriptor was closed; the process should be gone
            // or about to be.
            match tokio::time::timeout(DRAIN, session.child.wait()).await {
                Ok(Ok(status)) => Some(exit_code(status)),
                _ => None,
            }
        }
        End::ClientGone => None,
    };

    if end != End::ClientGone {
        // Forward what the process printed before it exited, then the code.
        while let Ok(Some(bytes)) = tokio::time::timeout(DRAIN, rx.recv()).await {
            if socket.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let code = code.unwrap_or(1);
        let _ = socket
            .send(control_frame(json!({ "type": "exit", "code": code })))
            .await;
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: close_code::NORMAL,
                reason: Utf8Bytes::from_static("exit"),
            })))
            .await;
    }
    reader.abort();
    hang_up(&mut session.child, code.is_some()).await;
}

/// Hang up the process group behind the terminal (what a closing terminal
/// does), give it a moment, then kill whatever is left and reap the child.
async fn hang_up(child: &mut Child, exited: bool) {
    if let Some(pid) = child.id() {
        let pgid = Pid::from_raw(pid as i32);
        let _ = killpg(pgid, Signal::SIGHUP);
        if !exited {
            let _ = tokio::time::timeout(HANGUP_GRACE, child.wait()).await;
        }
    }
    kill_tree(child).await;
}

/// Read the terminal until end-of-file and forward every chunk. A pseudo-
/// terminal master reports `EIO` (Linux) or zero bytes (macOS) once the
/// slave side has no holders left.
async fn read_terminal(master: Arc<AsyncFd<OwnedFd>>, tx: mpsc::Sender<Bytes>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let mut guard = match master.readable().await {
            Ok(g) => g,
            Err(_) => return,
        };
        match guard.try_io(|fd| nix::unistd::read(fd.get_ref(), &mut buf).map_err(io::Error::from))
        {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => {
                if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                    return;
                }
            }
            Ok(Err(_)) => return,
            Err(_would_block) => continue,
        }
    }
}

/// Write all of `data` to the terminal, waiting for room when the process
/// is not reading.
async fn write_terminal(master: &AsyncFd<OwnedFd>, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let mut guard = master.writable().await?;
        match guard.try_io(|fd| nix::unistd::write(fd.get_ref(), data).map_err(io::Error::from)) {
            Ok(Ok(n)) => data = &data[n..],
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{self, client::IntoClientRequest},
    };
    use warren_core::types::HEADER_INSTANCE_TOKEN;

    use crate::{
        config::GatewayConfig,
        server::{build_state, serve_listener},
    };

    const TOKEN: &str = "tok";

    struct Server {
        ws_base: String,
        home: PathBuf,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    async fn start() -> Server {
        let home =
            std::env::temp_dir().join(format!("warren-gw-pty-{}", warren_core::ids::new_id()));
        let config = GatewayConfig {
            home: home.clone(),
            instance_token: TOKEN.to_string(),
            terminal: false,
            ..GatewayConfig::default()
        };
        let state = build_state(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            serve_listener(state, listener, std::future::pending())
                .await
                .unwrap();
        });
        Server {
            ws_base: format!("ws://{addr}"),
            home,
        }
    }

    type Ws = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect(server: &Server, query: &str, token: Option<&str>) -> tungstenite::Result<Ws> {
        let mut req = format!("{}/v1/pty{query}", server.ws_base)
            .into_client_request()
            .unwrap();
        if let Some(t) = token {
            req.headers_mut()
                .insert(HEADER_INSTANCE_TOKEN, t.parse().unwrap());
        }
        connect_async(req).await.map(|(ws, _)| ws)
    }

    /// Everything the server sent: the terminal bytes and the exit code.
    async fn collect(ws: &mut Ws) -> (String, Option<i64>) {
        let mut out = Vec::new();
        let mut exit = None;
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(tungstenite::Message::Binary(b)) => out.extend_from_slice(&b),
                Ok(tungstenite::Message::Text(t)) => {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == "exit" {
                        exit = v["code"].as_i64();
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        (String::from_utf8_lossy(&out).into_owned(), exit)
    }

    fn text_frame(v: Value) -> tungstenite::Message {
        tungstenite::Message::Text(v.to_string().into())
    }

    #[tokio::test]
    async fn runs_a_command_reports_the_exit_code_and_accepts_resizes() {
        let server = start().await;
        let query = format!(
            "?cmd={}&cols=120&rows=40",
            urlencoding::encode("read x; stty size; echo hello; exit 3")
        );
        let mut ws = connect(&server, &query, Some(TOKEN)).await.unwrap();
        // The resize is queued before the newline that lets `read` return,
        // so `stty size` sees the new size.
        ws.send(text_frame(
            json!({ "type": "resize", "cols": 100, "rows": 30 }),
        ))
        .await
        .unwrap();
        ws.send(tungstenite::Message::Binary(Bytes::from_static(b"\n")))
            .await
            .unwrap();
        let (out, exit) = collect(&mut ws).await;
        assert!(out.contains("hello"), "{out:?}");
        assert!(out.contains("30 100"), "{out:?}");
        assert_eq!(exit, Some(3));
    }

    #[tokio::test]
    async fn interactive_shell_has_a_terminal_and_the_home() {
        let server = start().await;
        let mut ws = connect(&server, "", Some(TOKEN)).await.unwrap();
        ws.send(tungstenite::Message::Binary(Bytes::from_static(
            b"echo TERM=$TERM; tty >/dev/null && echo is-a-tty; pwd; exit 5\n",
        )))
        .await
        .unwrap();
        let (out, exit) = collect(&mut ws).await;
        assert!(out.contains("TERM=xterm-256color"), "{out:?}");
        assert!(out.contains("is-a-tty"), "{out:?}");
        let home = server.home.canonicalize().unwrap();
        assert!(out.contains(&home.display().to_string()), "{out:?}");
        assert_eq!(exit, Some(5));
    }

    #[tokio::test]
    async fn closing_the_socket_kills_the_process_group() {
        let server = start().await;
        let pidfile = server.home.join("pid");
        let query = format!(
            "?cmd={}",
            urlencoding::encode(&format!("echo $$ > {}; sleep 30", pidfile.display()))
        );
        let mut ws = connect(&server, &query, Some(TOKEN)).await.unwrap();
        let pid = loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        ws.close(None).await.unwrap();
        drop(ws);
        let gone = async {
            loop {
                if nix::sys::signal::kill(Pid::from_raw(pid), None).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), gone)
            .await
            .expect("the shell behind the terminal is gone after the socket closes");
    }

    #[tokio::test]
    async fn requires_the_token_and_validates_the_query() {
        let server = start().await;
        let err = connect(&server, "", None).await.unwrap_err();
        match err {
            tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 401),
            other => panic!("{other:?}"),
        }
        let err = connect(&server, "?cols=0", Some(TOKEN)).await.unwrap_err();
        match err {
            tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 400),
            other => panic!("{other:?}"),
        }
        let err = connect(&server, "?cwd=nope", Some(TOKEN))
            .await
            .unwrap_err();
        match err {
            tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 400),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn query_parsing() {
        let r = parse_request(None).unwrap();
        assert_eq!(r, PtyRequest::default());
        let r = parse_request(Some("cmd=echo+hi&cwd=sub&cols=132&rows=50&term=screen")).unwrap();
        assert_eq!(r.cmd, "echo hi");
        assert_eq!(r.cwd, "sub");
        assert_eq!((r.cols, r.rows), (132, 50));
        assert_eq!(r.term, "screen");
        assert_eq!(parse_request(Some("cols=")).unwrap().cols, DEFAULT_COLS);
        for bad in ["cols=0", "rows=-1", "cols=70000", "rows=abc", "term=a%20b"] {
            assert_eq!(parse_request(Some(bad)).unwrap_err().status, 400, "{bad}");
        }
        let home = std::env::temp_dir();
        assert_eq!(resolve_workdir(&home, "").unwrap(), home);
        assert_eq!(
            resolve_workdir(&home, "definitely-missing-dir-xyz")
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn control_frames() {
        let size = winsize(80, 24);
        let pty = openpty(&size, None::<&Termios>).unwrap();
        assert_eq!(
            apply_control(&pty.master, r#"{"type":"resize","cols":90,"rows":40}"#).unwrap(),
            Some((90, 40))
        );
        assert_eq!(apply_control(&pty.master, "not json").unwrap(), None);
        assert_eq!(
            apply_control(&pty.master, r#"{"type":"resize","cols":0,"rows":40}"#).unwrap(),
            None
        );
        assert_eq!(
            apply_control(&pty.master, r#"{"type":"other"}"#).unwrap(),
            None
        );
    }
}
