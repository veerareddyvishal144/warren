//! End-to-end walk of the gateway over HTTP with the echo driver and a
//! temporary home directory. No Docker, no network beyond loopback.

use std::{path::PathBuf, time::Duration};

use futures::StreamExt;
use serde_json::{Value, json};
use warren_core::types::HEADER_INSTANCE_TOKEN;
use warren_gateway::{
    GatewayConfig,
    server::{build_state, serve_listener},
};

struct TestServer {
    base: String,
    home: PathBuf,
    /// Sent as `x-warren-instance-token` by the request helpers when non-empty.
    token: String,
    client: reqwest::Client,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

async fn start(driver: &str, token: &str) -> TestServer {
    let home = std::env::temp_dir().join(format!("warren-gw-it-{}", warren_core::ids::new_id()));
    let config = GatewayConfig {
        home: home.clone(),
        driver: driver.to_string(),
        instance_token: token.to_string(),
        terminal: false,
        ..GatewayConfig::default()
    };
    let state = build_state(config).expect("state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        serve_listener(state, listener, std::future::pending())
            .await
            .expect("serve");
    });
    TestServer {
        base: format!("http://{addr}"),
        home,
        token: token.to_string(),
        client: reqwest::Client::new(),
    }
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
    /// A request carrying the instance token (when the server has one), the
    /// way the edge and the daemon send them.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.client.request(method, self.url(path));
        if self.token.is_empty() {
            r
        } else {
            r.header(HEADER_INSTANCE_TOKEN, &self.token)
        }
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, path)
    }
    fn put(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::PUT, path)
    }
    fn patch(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::PATCH, path)
    }
    fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::DELETE, path)
    }
    async fn get_json(&self, path: &str) -> (u16, Value) {
        let r = self.get(path).send().await.expect("request");
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        (status, v)
    }
    async fn post_json(&self, path: &str, body: &Value) -> (u16, Value) {
        let r = self.post(path).json(body).send().await.expect("request");
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        (status, v)
    }
    /// A bare request without the token, as an unauthenticated caller sends it.
    async fn anonymous(&self, method: reqwest::Method, path: &str) -> (u16, Value) {
        let r = self
            .client
            .request(method, self.url(path))
            .send()
            .await
            .expect("request");
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        (status, v)
    }
}

/// Parse `event: x\ndata: {...}\n\n` blocks; comment-only keep-alives are skipped.
fn parse_sse(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(e) = line.strip_prefix("event:") {
                event = Some(e.trim().to_string());
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim());
            }
        }
        if let Some(e) = event {
            out.push((e, serde_json::from_str(&data).unwrap_or(Value::Null)));
        }
    }
    out
}

fn error_code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn walk_the_agent_api_with_the_echo_driver() {
    let s = start("echo", "secret-token").await;

    // Health and version answer without the token.
    let (status, health) = s.anonymous(reqwest::Method::GET, "/v1/health").await;
    assert_eq!(status, 200);
    assert_eq!(health["healthy"], true);
    assert_eq!(health["agent_ready"], true);
    assert_eq!(health["driver"], "echo");
    assert!(!health["version"].as_str().unwrap().is_empty());
    assert_eq!(
        health["home_owner"]["uid"],
        warren_gateway::exec::effective_uid()
    );
    assert!(!health["home_owner"]["name"].as_str().unwrap().is_empty());
    let (status, version) = s.anonymous(reqwest::Method::GET, "/v1/version").await;
    assert_eq!(status, 200);
    assert_eq!(version["version"], health["version"]);
    let (status, models) = s.get_json("/v1/models").await;
    assert_eq!(status, 200);
    assert_eq!(models["data"][0]["id"], "default");

    // Non-streaming response.
    let (status, r) = s
        .post_json("/v1/responses", &json!({ "input": "hello" }))
        .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["status"], "completed");
    assert_eq!(r["output_text"], "echo: hello");
    assert_eq!(r["id"].as_str().unwrap().len(), 32);
    let session_id = r["session_id"].as_str().unwrap().to_string();
    assert_eq!(session_id.len(), 32);
    assert!(r["created_at"].as_i64().unwrap() > 0);
    assert!(r["completed_at"].as_i64().unwrap() >= r["created_at"].as_i64().unwrap());
    let first_id = r["id"].as_str().unwrap().to_string();

    // Streaming response in the same session.
    let resp = s
        .post("/v1/responses")
        .json(&json!({ "input": "again", "session_id": session_id, "stream": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = resp.text().await.unwrap();
    let events = parse_sse(&text);
    assert_eq!(events[0].0, "response.created");
    assert_eq!(events[0].1["session_id"], session_id);
    let stream_id = events[0].1["id"].as_str().unwrap().to_string();
    let deltas: Vec<&str> = events
        .iter()
        .filter(|(e, _)| e == "response.output_text.delta")
        .map(|(_, d)| d["delta"].as_str().unwrap())
        .collect();
    assert!(deltas.len() >= 2, "{events:?}");
    assert_eq!(deltas.concat(), "echo: again");
    let (last_event, last) = events.last().unwrap();
    assert_eq!(last_event, "response.completed");
    assert_eq!(last["output_text"], "echo: again");
    assert_eq!(last["status"], "completed");
    assert_eq!(last["id"], stream_id);

    // Fetch and replay.
    let (status, got) = s.get_json(&format!("/v1/responses/{stream_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(got["status"], "completed");
    assert_eq!(got["output_text"], "echo: again");
    let replay = s
        .get(&format!("/v1/responses/{first_id}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status().as_u16(), 200);
    let replayed = parse_sse(&replay.text().await.unwrap());
    assert_eq!(replayed[0].0, "response.created");
    assert_eq!(replayed.last().unwrap().0, "response.completed");
    assert_eq!(replayed.last().unwrap().1["output_text"], "echo: hello");
    let (status, missing) = s
        .get_json("/v1/responses/00000000000000000000000000000000")
        .await;
    assert_eq!(status, 404);
    assert_eq!(error_code(&missing), "not_found");

    // Sessions.
    let (status, list) = s.get_json("/v1/sessions").await;
    assert_eq!(status, 200);
    let data = list["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], session_id);
    assert_eq!(data[0]["message_count"], 4);
    assert_eq!(data[0]["name"], "hello");
    let (status, detail) = s.get_json(&format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 200);
    let messages = detail["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hello");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "echo: hello");
    assert!(messages[0]["at"].as_i64().unwrap() > 0);
    let r = s
        .patch(&format!("/v1/sessions/{session_id}"))
        .json(&json!({ "name": "renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "renamed");
    let r = s
        .patch(&format!("/v1/sessions/{session_id}"))
        .json(&json!({ "nom": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(
        error_code(&r.json::<Value>().await.unwrap()),
        "invalid_request"
    );
    // A second session appears first in the list because it is newer.
    let (_, r2) = s
        .post_json("/v1/responses", &json!({ "input": "second session" }))
        .await;
    let (_, list) = s.get_json("/v1/sessions").await;
    assert_eq!(list["data"][0]["id"], r2["session_id"]);
    assert_eq!(list["data"][1]["id"], session_id);
    let r = s
        .delete(&format!("/v1/sessions/{session_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["deleted"], true);
    let (status, _) = s.get_json(&format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 404);
    let r = s
        .delete(&format!("/v1/sessions/{session_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);

    // Files: put, get, list, move, download, delete.
    let r = s
        .put("/v1/files/content?path=dir/a.txt")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let put: Value = r.json().await.unwrap();
    assert_eq!(put["path"], "dir/a.txt");
    assert_eq!(put["size"], 5);
    let r = s
        .get("/v1/files/content?path=dir/a.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert_eq!(r.text().await.unwrap(), "hello");
    let (status, listing) = s.get_json("/v1/files?path=dir").await;
    assert_eq!(status, 200);
    assert_eq!(listing["path"], "dir");
    assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
    assert_eq!(listing["entries"][0]["name"], "a.txt");
    assert_eq!(listing["entries"][0]["type"], "file");
    assert_eq!(listing["entries"][0]["size"], 5);
    let (_, root) = s.get_json("/v1/files").await;
    assert_eq!(root["path"], ".");
    assert!(
        root["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "dir" && e["type"] == "dir")
    );
    let (status, moved) = s
        .post_json(
            "/v1/files/move",
            &json!({ "from": "dir/a.txt", "to": "dir/sub/b.txt" }),
        )
        .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["moved"], true);
    assert_eq!(moved["to"], "dir/sub/b.txt");
    let r = s
        .get("/v1/files/content?path=dir/sub/b.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), "hello");
    let r = s.get("/v1/files/download?path=dir").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "application/gzip");
    assert!(
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("dir.tar.gz")
    );
    let bytes = r.bytes().await.unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    let names: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"dir/sub/b.txt".to_string()), "{names:?}");
    let r = s.delete("/v1/files?path=dir").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["deleted"], true);
    let (status, _) = s.get_json("/v1/files?path=dir").await;
    assert_eq!(status, 404);
    assert!(!s.home.join("dir").exists());

    // Path traversal and escapes are forbidden.
    for path in [
        "../../etc/passwd",
        "/etc/passwd",
        "/etc/definitely-missing",
        "..",
        "a/../../x",
    ] {
        let r = s
            .get(&format!(
                "/v1/files/content?path={}",
                urlencoding::encode(path)
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403, "{path}");
        assert_eq!(error_code(&r.json::<Value>().await.unwrap()), "forbidden");
    }
    let r = s
        .put("/v1/files/content?path=../escape.txt")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert!(!s.home.parent().unwrap().join("escape.txt").exists());
    let (status, _) = s.get_json("/v1/files?path=/etc").await;
    assert_eq!(status, 403);
    let r = s.delete("/v1/files?path=..").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let (status, _) = s
        .post_json("/v1/files/move", &json!({ "from": ".", "to": "x" }))
        .await;
    assert_eq!(status, 403);
    let r = s.get("/v1/files/content").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);

    // Exec requires the instance token: missing or wrong -> 401 invalid_api_key.
    let body = json!({ "command": "echo out; echo err 1>&2; exit 3" });
    let (status, e) = s.anonymous(reqwest::Method::POST, "/v1/exec").await;
    assert_eq!(status, 401);
    assert_eq!(error_code(&e), "invalid_api_key");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains(HEADER_INSTANCE_TOKEN),
        "{e}"
    );
    let r = s
        .client
        .post(s.url("/v1/exec"))
        .header(HEADER_INSTANCE_TOKEN, "wrong")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert_eq!(
        error_code(&r.json::<Value>().await.unwrap()),
        "invalid_api_key"
    );
    let r = s.post("/v1/exec").json(&body).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let result: Value = r.json().await.unwrap();
    assert_eq!(result["exit_code"], 3);
    assert_eq!(result["stdout"], "out\n");
    assert_eq!(result["stderr"], "err\n");
    assert!(result["duration_ms"].as_i64().unwrap() >= 0);
    let r = s
        .post("/v1/exec")
        .json(&json!({ "command": "pwd", "bogus": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let r = s
        .post("/v1/exec")
        .json(&json!({ "command": "pwd" }))
        .send()
        .await
        .unwrap();
    let pwd: Value = r.json().await.unwrap();
    assert_eq!(
        pwd["stdout"].as_str().unwrap().trim(),
        std::fs::canonicalize(&s.home).unwrap().to_str().unwrap()
    );

    // Shutdown is token-protected and unavailable outside init mode.
    let (status, e) = s.anonymous(reqwest::Method::POST, "/v1/shutdown").await;
    assert_eq!(status, 401);
    assert_eq!(error_code(&e), "invalid_api_key");
    let r = s.post("/v1/shutdown").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(
        error_code(&r.json::<Value>().await.unwrap()),
        "not_supported"
    );

    // Request validation and unknown routes use the error envelope.
    let (status, e) = s
        .post_json("/v1/responses", &json!({ "input": "x", "unknown": true }))
        .await;
    assert_eq!(status, 400);
    assert_eq!(error_code(&e), "invalid_request");
    let (status, _) = s
        .post_json("/v1/responses", &json!({ "input": "   " }))
        .await;
    assert_eq!(status, 400);
    let r = s
        .post("/v1/responses")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let (status, e) = s.get_json("/v1/nope").await;
    assert_eq!(status, 404);
    assert_eq!(error_code(&e), "not_found");

    // Cancel a long-running turn while following its stream.
    let started = std::time::Instant::now();
    let resp = s
        .post("/v1/responses")
        .json(&json!({ "input": "!sleep 30", "stream": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut body = resp.bytes_stream();
    let mut text = String::new();
    let mut cancel_id = None;
    while cancel_id.is_none() {
        let chunk = tokio::time::timeout(Duration::from_secs(10), body.next())
            .await
            .expect("stream chunk")
            .expect("chunk")
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
        if let Some((_, data)) = parse_sse(&text)
            .into_iter()
            .find(|(e, _)| e == "response.created")
        {
            cancel_id = data["id"].as_str().map(str::to_string);
        }
    }
    let cancel_id = cancel_id.unwrap();
    let (status, c) = s
        .post_json(&format!("/v1/responses/{cancel_id}/cancel"), &json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(c["id"], cancel_id);
    assert_eq!(c["status"], "cancelled");
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), body.next())
        .await
        .expect("stream end")
    {
        text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    let events = parse_sse(&text);
    assert!(events.iter().any(|(e, _)| e == "response.tool_call"));
    let (last_event, last) = events.last().unwrap();
    assert_eq!(last_event, "response.completed");
    assert_eq!(last["status"], "cancelled");
    assert!(started.elapsed() < Duration::from_secs(15));
    let (_, got) = s.get_json(&format!("/v1/responses/{cancel_id}")).await;
    assert_eq!(got["status"], "cancelled");
    // The on-disk record exists under <home>/.warren/responses.
    assert!(
        s.home
            .join(".warren/responses")
            .join(format!("{cancel_id}.json"))
            .is_file()
    );
    assert!(s.home.join(".warren/sessions").is_dir());
}

#[tokio::test]
async fn non_agent_template_serves_files_exec_and_health_only() {
    let s = start("", "").await;
    let (status, health) = s.get_json("/v1/health").await;
    assert_eq!(status, 200);
    assert_eq!(health["healthy"], true);
    assert_eq!(health["agent_ready"], false);
    assert_eq!(health["driver"], "none");
    assert_eq!(
        health["home_owner"]["uid"],
        warren_gateway::exec::effective_uid()
    );
    // Without a configured token every route is open (local runs).
    let (status, _) = s.anonymous(reqwest::Method::GET, "/v1/files").await;
    assert_eq!(status, 200);

    let (status, e) = s
        .post_json("/v1/responses", &json!({ "input": "hello" }))
        .await;
    assert_eq!(status, 404);
    assert_eq!(error_code(&e), "not_supported");
    let (status, sessions) = s.get_json("/v1/sessions").await;
    assert_eq!(status, 200);
    assert_eq!(sessions["data"].as_array().unwrap().len(), 0);

    let r = s
        .put("/v1/files/content?path=f.txt")
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let r = s.get("/v1/files/content?path=f.txt").send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "data");

    // Without a configured token, exec needs no header.
    let (status, result) = s
        .post_json("/v1/exec", &json!({ "command": "echo ok" }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(result["stdout"], "ok\n");
    assert_eq!(result["exit_code"], 0);
}

#[tokio::test]
async fn stats_report_guest_usage() {
    let s = start("none", "stats-token").await;
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (status, stats) = s.get_json("/v1/stats").await;
    assert_eq!(status, 200, "{stats}");
    // Exactly the contract's fields, every one a number.
    let mut keys: Vec<&str> = stats
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "at",
            "cpu_percent",
            "disk_bytes",
            "memory_bytes",
            "memory_limit_bytes"
        ]
    );
    let at = stats["at"].as_i64().unwrap();
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        at >= before && at <= after,
        "at={at} before={before} after={after}"
    );
    let cpu = stats["cpu_percent"].as_f64().unwrap();
    assert!((0.0..=100.0).contains(&cpu), "{cpu}");
    // The home lives on a real filesystem and this host has memory.
    assert!(stats["disk_bytes"].as_i64().unwrap() > 0, "{stats}");
    assert!(stats["memory_bytes"].as_i64().unwrap() > 0, "{stats}");
    assert!(stats["memory_limit_bytes"].as_i64().unwrap() > 0, "{stats}");
    // Token-protected like every other route.
    let (status, e) = s.anonymous(reqwest::Method::GET, "/v1/stats").await;
    assert_eq!(status, 401);
    assert_eq!(error_code(&e), "invalid_api_key");
}

#[tokio::test]
async fn token_guards_every_route_except_health_and_version() {
    use reqwest::Method;
    let s = start("echo", "guard-token").await;

    // The two open routes, exactly spelled.
    for path in ["/v1/health", "/v1/version"] {
        let (status, _) = s.anonymous(Method::GET, path).await;
        assert_eq!(status, 200, "{path}");
    }

    // Everything else answers 401 invalid_api_key without the token: the
    // Agent API, files, exec, shutdown, the root document, unknown routes and
    // alternative spellings of open and protected paths.
    let protected: &[(Method, &str)] = &[
        (Method::GET, "/"),
        (Method::POST, "/v1/responses"),
        (
            Method::GET,
            "/v1/responses/00000000000000000000000000000000",
        ),
        (
            Method::GET,
            "/v1/responses/00000000000000000000000000000000/stream",
        ),
        (
            Method::POST,
            "/v1/responses/00000000000000000000000000000000/cancel",
        ),
        (Method::GET, "/v1/sessions"),
        (Method::GET, "/v1/sessions/x"),
        (Method::PATCH, "/v1/sessions/x"),
        (Method::DELETE, "/v1/sessions/x"),
        (Method::GET, "/v1/models"),
        (Method::GET, "/v1/files"),
        (Method::GET, "/v1/files?path=."),
        (Method::DELETE, "/v1/files?path=x"),
        (Method::GET, "/v1/files/content?path=x"),
        (Method::PUT, "/v1/files/content?path=x"),
        (Method::POST, "/v1/files/move"),
        (Method::GET, "/v1/files/download?path=."),
        (Method::POST, "/v1/exec"),
        (Method::GET, "/v1/stats"),
        (Method::POST, "/v1/shutdown"),
        (Method::GET, "/v1/nope"),
        (Method::POST, "/v1/health"),
        (Method::GET, "/v1/health/"),
        (Method::GET, "//v1/files"),
        (Method::GET, "/v1/./files"),
        (Method::GET, "/v1/health/../files"),
        (Method::GET, "//v1/health"),
        (Method::GET, "/v1//health"),
        (Method::GET, "/V1/files"),
    ];
    for (method, path) in protected {
        let (status, e) = s.anonymous(method.clone(), path).await;
        assert_eq!(status, 401, "{method} {path}: {e}");
        assert_eq!(error_code(&e), "invalid_api_key", "{method} {path}");
        assert!(
            e["error"]["message"]
                .as_str()
                .unwrap()
                .contains(HEADER_INSTANCE_TOKEN),
            "{method} {path}: {e}"
        );
    }
    // A wrong token is as good as none, whatever its length.
    for wrong in ["", "guard-toke", "guard-tokens", "GUARD-TOKEN", "x"] {
        let r = s
            .client
            .get(s.url("/v1/files"))
            .header(HEADER_INSTANCE_TOKEN, wrong)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "{wrong:?}");
    }

    // With the token the same routes work.
    let (status, listing) = s.get_json("/v1/files").await;
    assert_eq!(status, 200, "{listing}");
    assert_eq!(listing["path"], ".");
    let (status, _) = s.get_json("/v1/sessions").await;
    assert_eq!(status, 200);
    let (status, root) = s.get_json("/").await;
    assert_eq!(status, 200);
    assert_eq!(root["name"], "warren-gateway");
    let (status, e) = s.get_json("/v1/nope").await;
    assert_eq!(status, 404);
    assert_eq!(error_code(&e), "not_found");
    let (status, r) = s
        .post_json("/v1/responses", &json!({ "input": "hi" }))
        .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["output_text"], "echo: hi");
}
