//! End-to-end smoke tests: the built binary against a fake daemon that answers
//! canned JSON (and one SSE stream) over HTTP/1.1. This covers what the unit
//! tests cannot: the SDK calls, rendering of real answers, exit codes and the
//! config file as a whole.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const KEY: &str = "sk_live_test";
const ID: &str = "ab12cd34ef";

struct Fake {
    addr: SocketAddr,
}

impl Fake {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

async fn start_fake() -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(handle(sock, addr));
        }
    });
    Fake { addr }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn handle(mut sock: TcpStream, addr: SocketAddr) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let mut request = lines.next().unwrap_or("").split_whitespace();
    let method = request.next().unwrap_or("").to_string();
    let target = request.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut authed = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.parse().unwrap_or(0),
                "authorization" => authed = value == format!("Bearer {KEY}"),
                _ => {}
            }
        }
    }
    while buf.len() < header_end + content_length {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string();
    let path = target.split('?').next().unwrap_or("").to_string();
    let (status, ctype, out) = route(&method, &path, &body, authed, addr);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        409 => "Conflict",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        out.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(out.as_bytes()).await;
    let _ = sock.shutdown().await;
}

fn workspace() -> Value {
    json!({ "id": "ws_abc", "name": "acme", "created_at": 1763000000 })
}

fn instance(addr: SocketAddr) -> Value {
    json!({
        "id": ID, "workspace_id": "ws_abc", "name": "Production agent", "template": "warren-agent", "template_name": "warren-agent",
        "template_revision": 1, "image_ref": "warren/agent:dev", "type": "default", "status": "running", "status_reason": "",
        "resources": { "cpu": 2, "memory": 4, "disk": 4 }, "region": "local", "metadata": {}, "env": {}, "network": "allow-all",
        "auto_sleep": true, "idle_timeout_seconds": 900,
        "url": format!("http://{addr}"), "default_port": 3737, "mount_path": "/home/agent", "agent_api": true, "parent_id": "",
        "created_at": 1763000000, "updated_at": 1763000100, "started_at": 1763000050, "stopped_at": 0, "slept_at": 0,
        "last_activity_at": 1763000100, "runtime": "docker", "disk_used_bytes": 123456
    })
}

fn route(
    method: &str,
    path: &str,
    body: &str,
    authed: bool,
    addr: SocketAddr,
) -> (u16, &'static str, String) {
    const JSON: &str = "application/json";
    let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let exec_path = format!("/v1/instances/{ID}/exec");
    let stop_path = format!("/v1/instances/{ID}/stop");
    let get_path = format!("/v1/instances/{ID}");
    match (method, path) {
        ("GET", "/v1/health") => (200, JSON, json!({ "healthy": true, "version": "test", "runtimes": ["docker"], "time": 1763000000 }).to_string()),
        ("POST", "/v1/signup") => (
            201,
            JSON,
            json!({ "workspace": workspace(), "api_key": "sk_live_new", "key_id": "k1" }).to_string(),
        ),
        _ if !authed => (401, JSON, json!({ "error": { "code": "invalid_api_key", "message": "Missing or invalid API key." } }).to_string()),
        ("GET", "/v1/workspace") => {
            let mut ws = workspace();
            ws["awake_limit"] = json!(3);
            ws["awake_count"] = json!(1);
            (200, JSON, ws.to_string())
        }
        ("GET", "/v1/instances") => (200, JSON, json!({ "data": [instance(addr)] }).to_string()),
        ("GET", p) if p == get_path => (200, JSON, instance(addr).to_string()),
        ("POST", p) if p == exec_path => {
            let command = body["command"].as_str().unwrap_or("");
            (200, JSON, json!({ "exit_code": 3, "stdout": format!("ran: {command}\n"), "stderr": "warn\n", "duration_ms": 5 }).to_string())
        }
        ("POST", p) if p == stop_path => (200, JSON, json!({ "id": ID, "status": "stopping" }).to_string()),
        ("POST", "/v1/responses") => {
            let input = body["input"].as_str().unwrap_or("");
            if body["stream"].as_bool() == Some(true) {
                let events = [
                    ("response.created", json!({ "id": "r1", "session_id": "s1" })),
                    ("response.output_text.delta", json!({ "delta": "echo: " })),
                    ("response.output_text.delta", json!({ "delta": input })),
                    ("response.completed", json!({ "id": "r1", "session_id": "s1", "output_text": format!("echo: {input}"), "status": "completed" })),
                ];
                let text: String = events.iter().map(|(e, d)| format!("event: {e}\ndata: {d}\n\n")).collect();
                (200, "text/event-stream", text)
            } else {
                (
                    200,
                    JSON,
                    json!({ "id": "r1", "session_id": "s1", "status": "completed", "output_text": format!("echo: {input}"), "created_at": 1763000000000i64 })
                        .to_string(),
                )
            }
        }
        ("GET", "/v1/files") => (
            200,
            JSON,
            json!({ "path": ".", "entries": [{ "name": "notes.txt", "type": "file", "size": 10, "modified_at": 1763000000000i64 }] }).to_string(),
        ),
        _ => (409, JSON, json!({ "error": { "code": "conflict", "message": "instance is stopped", "details": { "status": "stopped" } } }).to_string()),
    }
}

/// A config path no other test in this process can share: pid, a process-wide
/// counter and the clock (the clock alone can repeat across threads on this VM).
fn temp_config() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir()
        .join(format!(
            "warren-cli-smoke-{}-{n}-{nanos}",
            std::process::id()
        ))
        .join("config.json")
}

/// Run the binary; returns (exit code, stdout, stderr).
async fn warren(fake: &Fake, config: &Path, args: &[&str]) -> (i32, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_warren"))
        .arg("--config")
        .arg(config)
        .arg("--api-url")
        .arg(fake.url())
        .args(args)
        .env_remove("WARREN_API_KEY")
        .env_remove("WARREN_API_URL")
        .env_remove("WARREN_CONFIG")
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run binary");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn cleanup(config: &Path) {
    if let Some(dir) = config.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_login_and_whoami() {
    let fake = start_fake().await;
    let config = temp_config();

    let (code, out, _) = warren(&fake, &config, &["health"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("healthy:   true"), "{out}");

    let (code, _, err) = warren(&fake, &config, &["whoami"]).await;
    assert_eq!(code, 2);
    assert!(err.contains("no API key configured"), "{err}");

    let (code, out, _) = warren(&fake, &config, &["login", "--api-key", KEY]).await;
    assert_eq!(code, 0, "{out}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["api_key"], KEY);
    assert_eq!(saved["api_url"], fake.url());

    let (code, out, _) = warren(&fake, &config, &["whoami"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("id:           ws_abc"), "{out}");
    assert!(out.contains("awake_limit:  3"), "{out}");
    assert!(out.contains("awake_count:  1"), "{out}");

    let (code, out, _) = warren(&fake, &config, &["whoami", "--json"]).await;
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["id"], "ws_abc");
    assert_eq!(v["awake_limit"], 3);

    let (code, _, err) = warren(&fake, &config, &["whoami", "--api-key", "sk_live_wrong"]).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("invalid_api_key: Missing or invalid API key."),
        "{err}"
    );
    cleanup(&config);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instances_exec_and_error_codes() {
    let fake = start_fake().await;
    let config = temp_config();
    warren(&fake, &config, &["login", "--api-key", KEY]).await;

    let (code, out, _) = warren(&fake, &config, &["instances", "list"]).await;
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("ID          NAME              STATUS   TEMPLATE"),
        "{out}"
    );
    assert!(lines[1].starts_with("ab12cd34ef  Production agent  running  warren-agent  default  2    4    4     2025-11-13"), "{out}");

    let (code, out, _) = warren(&fake, &config, &["instances", "get", ID]).await;
    assert_eq!(code, 0);
    assert!(out.contains("status:                running"), "{out}");
    assert!(
        out.contains("disk_used_bytes:       123456 (120.6 KB)"),
        "{out}"
    );

    let (code, out, err) = warren(
        &fake,
        &config,
        &["instances", "exec", ID, "--", "ls", "-la"],
    )
    .await;
    assert_eq!(code, 3);
    assert_eq!(out, "ran: ls -la\n");
    assert_eq!(err, "warn\n");

    let (code, out, _) = warren(
        &fake,
        &config,
        &["instances", "exec", ID, "--json", "--", "echo hi | wc -c"],
    )
    .await;
    assert_eq!(code, 3);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["stdout"], "ran: echo hi | wc -c\n");

    let (code, out, _) = warren(&fake, &config, &["instances", "stop", ID]).await;
    assert_eq!(code, 0);
    assert_eq!(out, "ab12cd34ef  stopping\n");

    let (code, out, err) = warren(&fake, &config, &["instances", "start", "zzzzzzzzzz"]).await;
    assert_eq!(code, 1);
    assert_eq!(out, "");
    assert!(
        err.starts_with("warren: conflict: instance is stopped"),
        "{err}"
    );

    let (code, _, err) = warren(
        &fake,
        &config,
        &["instances", "start", "zzzzzzzzzz", "--json"],
    )
    .await;
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(err.trim()).unwrap();
    assert_eq!(v["error"]["code"], "conflict");

    let (code, _, err) = warren(&fake, &config, &["instances", "create", "--idle", "10"]).await;
    assert_eq!(code, 2);
    assert!(
        err.contains("--idle must be between 300 and 86400"),
        "{err}"
    );

    let (code, _, err) = warren(&fake, &config, &["instances", "bogus"]).await;
    assert_eq!(code, 2);
    assert!(err.contains("Usage: warren instances"), "{err}");
    cleanup(&config);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_streams_and_files_list() {
    let fake = start_fake().await;
    let config = temp_config();
    warren(&fake, &config, &["login", "--api-key", KEY]).await;

    let (code, out, err) = warren(&fake, &config, &["chat", ID, "hello", "world"]).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "echo: hello world\n");
    assert!(err.contains("session: s1"), "{err}");

    let (code, out, err) = warren(&fake, &config, &["chat", ID, "--no-stream", "hi"]).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "echo: hi\n");

    let (code, out, _) = warren(&fake, &config, &["chat", ID, "--json", "hi"]).await;
    assert_eq!(code, 0);
    let events: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["event"], "response.created");
    assert_eq!(events[3]["data"]["output_text"], "echo: hi");

    let (code, out, _) = warren(&fake, &config, &["files", "ls", ID]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("file  10 B  2025-11-13 02:13:20  notes.txt"),
        "{out}"
    );
    cleanup(&config);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signup_prints_and_saves_the_key() {
    let fake = start_fake().await;
    let config = temp_config();
    let (code, out, err) = warren(&fake, &config, &["signup", "--name", "acme"]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("api_key:    sk_live_new"), "{out}");
    assert!(err.contains("Saved the key to"), "{err}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["api_key"], "sk_live_new");

    let (code, out, _) = warren(&fake, &config, &["signup", "--json"]).await;
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["workspace"]["id"], "ws_abc");
    cleanup(&config);
}
