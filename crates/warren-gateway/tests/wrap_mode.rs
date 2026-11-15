//! Wrap mode through the real binary: `warren-gateway --wrap -- <argv...>`
//! around small shell programs, driven over HTTP on loopback. Runs as
//! whatever user runs the tests (uid 1000 on the dev host, like the agent
//! user in the images), so it also covers the unprivileged, no-switching path.

use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use warren_core::types::HEADER_INSTANCE_TOKEN;

const BIN: &str = env!("CARGO_BIN_EXE_warren-gateway");
const TOKEN: &str = "wrap-test-token";

struct Gateway {
    child: Child,
    base: String,
    dir: PathBuf,
    home: PathBuf,
    client: reqwest::Client,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Start the binary with `flags` (`--serve --wrap` or just `--wrap`) and the
/// wrapped `argv` after `--`, in a fresh temp directory that is also the
/// working directory, with a home inside it.
async fn start(flags: &[&str], argv: &[&str]) -> Gateway {
    let dir = std::env::temp_dir().join(format!("warren-gw-wrap-{}", warren_core::ids::new_id()));
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("mkdir");
    let port = free_port();
    let mut cmd = Command::new(BIN);
    cmd.args(flags);
    if !argv.is_empty() {
        cmd.arg("--").args(argv);
    }
    cmd.current_dir(&dir)
        .env("WARREN_HOME", &home)
        .env("WARREN_GATEWAY_BIND", format!("127.0.0.1:{port}"))
        .env("WARREN_INSTANCE_ID", "ab12cd34ef")
        .env("WARREN_INSTANCE_TOKEN", TOKEN)
        .env("WARREN_WORKSPACE_ID", "ws_test")
        .env("WARREN_TERMINAL", "off")
        .env("APP_ENV", "production")
        .env("RUST_LOG", "warn")
        .env_remove("WARREN_AGENT_DRIVER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn gateway");
    Gateway {
        child,
        base: format!("http://127.0.0.1:{port}"),
        dir,
        home,
        client: reqwest::Client::new(),
    }
}

impl Gateway {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id().expect("gateway pid") as i32)
    }

    async fn try_get_json(&self, path: &str) -> Option<(u16, Value)> {
        let r = self.client.get(self.url(path)).send().await.ok()?;
        let status = r.status().as_u16();
        let v = r.json().await.unwrap_or(Value::Null);
        Some((status, v))
    }

    async fn get_json(&self, path: &str) -> (u16, Value) {
        self.try_get_json(path).await.expect("request")
    }

    /// Poll `/v1/health` until `ready` accepts it (the server may still be
    /// binding when we start asking).
    async fn health_when(&self, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some((200, h)) = self.try_get_json("/v1/health").await
                && ready(&h)
            {
                return h;
            }
            assert!(
                Instant::now() < deadline,
                "gateway health did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Read the gateway's stdout (which the wrapped program inherits) until a
    /// line equals `wanted`. Never reads to EOF: the program may keep running.
    async fn stdout_line(&mut self, wanted: &str) {
        let out = self.child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(out).lines();
        let found = tokio::time::timeout(Duration::from_secs(15), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim() == wanted {
                    return true;
                }
            }
            false
        })
        .await
        .expect("stdout line in time");
        assert!(found, "did not see {wanted:?} on the gateway's stdout");
    }

    /// SIGTERM the gateway and return its exit code.
    async fn terminate(&mut self) -> i32 {
        kill(self.pid(), Signal::SIGTERM).expect("signal gateway");
        let status = tokio::time::timeout(Duration::from_secs(30), self.child.wait())
            .await
            .expect("gateway exits in time")
            .expect("wait");
        status.code().unwrap_or(-1)
    }
}

fn pid_is_gone(pid: u32) -> bool {
    matches!(kill(Pid::from_raw(pid as i32), None), Err(Errno::ESRCH))
}

#[tokio::test]
async fn wrapped_program_runs_with_a_filtered_environment() {
    let mut g = start(
        &["--serve", "--wrap"],
        &[
            "sh",
            "-c",
            "pwd -P > cwd.txt; env > env.txt; echo started; sleep 30",
        ],
    )
    .await;
    let health = g.health_when(|h| h["wrapped"]["running"] == true).await;
    assert_eq!(health["healthy"], true);
    assert_eq!(health["agent_ready"], false);
    assert_eq!(health["driver"], "none");
    assert!(!health["version"].as_str().unwrap().is_empty());
    let pid = health["wrapped"]["pid"].as_u64().expect("pid") as u32;
    assert!(pid > 0);
    assert!(!pid_is_gone(pid));
    assert!(health["wrapped"]["exit_code"].is_null());
    assert!(health["wrapped"]["started_at"].as_i64().unwrap() > 0);
    assert!(health["wrapped"]["exited_at"].is_null());

    // Its stdout is the gateway's stdout, so Docker logs show the app.
    g.stdout_line("started").await;

    // Same working directory the gateway started in.
    let cwd = std::fs::read_to_string(g.dir.join("cwd.txt")).expect("cwd.txt");
    assert_eq!(
        PathBuf::from(cwd.trim()),
        std::fs::canonicalize(&g.dir).unwrap()
    );
    // The environment is inherited minus WARREN_* except WARREN_HOME.
    let env = std::fs::read_to_string(g.dir.join("env.txt")).expect("env.txt");
    let vars: Vec<&str> = env.lines().collect();
    assert!(
        vars.contains(&format!("WARREN_HOME={}", g.home.display()).as_str()),
        "{env}"
    );
    assert!(vars.contains(&"APP_ENV=production"), "{env}");
    for hidden in [
        "WARREN_INSTANCE_TOKEN=",
        "WARREN_GATEWAY_BIND=",
        "WARREN_INSTANCE_ID=",
        "WARREN_WORKSPACE_ID=",
        "WARREN_TERMINAL=",
    ] {
        assert!(
            !vars.iter().any(|v| v.starts_with(hidden)),
            "{hidden} leaked into the wrapped program: {env}"
        );
    }
    assert!(!env.contains(TOKEN));

    // The root document reports the same thing.
    let (_, root) = g.get_json("/").await;
    assert_eq!(root["wrapped"]["pid"], pid);

    // SIGTERM to the gateway terminates the program and the gateway exits
    // with the program's status (killed by SIGTERM: 143).
    let started = Instant::now();
    let code = g.terminate().await;
    assert_eq!(code, 128 + Signal::SIGTERM as i32);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(pid_is_gone(pid), "wrapped pid {pid} still exists");
}

#[tokio::test]
async fn exited_program_leaves_the_gateway_serving() {
    let mut g = start(&["--wrap"], &["sh", "-c", "exit 3"]).await;
    let health = g.health_when(|h| h["wrapped"]["running"] == false).await;
    assert_eq!(health["wrapped"]["exit_code"], 3);
    assert!(health["wrapped"]["pid"].as_u64().unwrap() > 0);
    assert!(health["wrapped"]["exited_at"].as_i64().unwrap() > 0);
    assert!(health["wrapped"].get("error").is_none());

    // Files still work.
    let r = g
        .client
        .put(g.url("/v1/files/content?path=notes/a.txt"))
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["size"], 5);
    let r = g
        .client
        .get(g.url("/v1/files/content?path=notes/a.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.text().await.unwrap(), "hello");
    let (status, listing) = g.get_json("/v1/files?path=notes").await;
    assert_eq!(status, 200);
    assert_eq!(listing["entries"][0]["name"], "a.txt");
    assert!(g.home.join("notes/a.txt").is_file());

    // Exec too, as the gateway's own user: no switching without root.
    let r = g
        .client
        .post(g.url("/v1/exec"))
        .header(HEADER_INSTANCE_TOKEN, TOKEN)
        .json(&json!({ "command": "id -u; echo $HOME" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let result: Value = r.json().await.unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(
        result["stdout"],
        format!(
            "{}\n{}\n",
            warren_gateway::exec::effective_uid(),
            std::fs::canonicalize(&g.home).unwrap().display()
        )
    );
    // The program is not restarted.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, again) = g.get_json("/v1/health").await;
    assert_eq!(again["wrapped"], health["wrapped"]);

    // Stopping the gateway reports the program's exit code.
    assert_eq!(g.terminate().await, 3);
}

#[tokio::test]
async fn sigterm_kills_a_program_that_ignores_it() {
    let mut g = start(
        &["--wrap"],
        &["sh", "-c", "trap '' TERM; echo trapped; sleep 30 & wait"],
    )
    .await;
    let health = g.health_when(|h| h["wrapped"]["running"] == true).await;
    let pid = health["wrapped"]["pid"].as_u64().unwrap() as u32;
    // Only signal once the trap is in place.
    g.stdout_line("trapped").await;
    let started = Instant::now();
    let code = g.terminate().await;
    // SIGTERM was ignored, so SIGKILL followed after the 10 s grace period.
    assert_eq!(code, 128 + Signal::SIGKILL as i32);
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(25));
    assert!(pid_is_gone(pid));
}

#[tokio::test]
async fn wrap_without_a_program_just_serves() {
    let mut g = start(&["--wrap"], &[]).await;
    let health = g.health_when(|_| true).await;
    assert_eq!(health["healthy"], true);
    assert!(health.get("wrapped").is_none(), "{health}");
    assert_eq!(g.terminate().await, 0);
}

#[tokio::test]
async fn unstartable_program_is_reported() {
    let mut g = start(&["--wrap"], &["/definitely/missing/entrypoint"]).await;
    let health = g.health_when(|h| h["wrapped"]["running"] == false).await;
    assert_eq!(health["wrapped"]["exit_code"], 127);
    assert_eq!(health["wrapped"]["pid"], 0);
    assert!(
        health["wrapped"]["error"]
            .as_str()
            .unwrap()
            .contains("/definitely/missing/entrypoint")
    );
    assert_eq!(g.terminate().await, 127);
}

#[tokio::test]
async fn help_documents_wrap_and_usage_errors_are_loud() {
    let out = Command::new(BIN).arg("--help").output().await.unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--wrap -- <command>"), "{text}");
    assert!(text.contains("WARREN_HOME"), "{text}");

    let out = Command::new(BIN).args(["--", "sh"]).output().await.unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--wrap"));

    let out = Command::new(BIN)
        .args(["--init", "--wrap", "--", "sh"])
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
