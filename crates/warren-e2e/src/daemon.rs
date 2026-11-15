//! Starts one real `warrend` for a test, waits for its health endpoint, hands
//! out HTTP clients whose DNS is pinned to it, and kills it on drop.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs::File,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use serde_json::Value;
use warren_core::Result;

use crate::util::{TempRoot, fail, free_port, tail_lines, transport};

/// Base domain the daemon is started with unless `WARREN_E2E_BASE_DOMAIN` says otherwise.
pub const DEFAULT_BASE_DOMAIN: &str = "warren.localhost";
/// Kernel used for vm-type instances unless `WARREN_E2E_KERNEL` is set.
pub const DEFAULT_KERNEL: &str = "/data/sandbox-probe/vmlinux-6.1.155";
/// Firecracker binary used unless `WARREN_E2E_FIRECRACKER_BIN` is set (only
/// passed when it exists; otherwise the daemon's own default applies).
pub const DEFAULT_FIRECRACKER_BIN: &str = "/data/sandbox-probe/firecracker";
/// Meter interval the daemon runs with; tests on billing wait a few of these.
pub const METER_INTERVAL: Duration = Duration::from_secs(2);
/// Smallest idle timeout the daemon accepts in tests.
pub const MIN_IDLE_SECONDS: u32 = 5;
/// How long the daemon may take to answer its health endpoint after spawn.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

/// Sandbox API base URL (`http://api.<base>:<port>/v1`).
pub fn api_url_for(base_domain: &str, port: u16) -> String {
    format!("http://api.{base_domain}:{port}/v1")
}

/// Host of an instance URL: `<id>.<base>` or `<id>-<preview_port>.<base>`.
pub fn instance_host_for(base_domain: &str, id: &str, preview_port: u16) -> String {
    if preview_port == 0 {
        format!("{id}.{base_domain}")
    } else {
        format!("{id}-{preview_port}.{base_domain}")
    }
}

/// Instance URL without a trailing slash; `preview_port` 0 means the default port.
pub fn instance_url_for(
    base_domain: &str,
    public_port: u16,
    id: &str,
    preview_port: u16,
) -> String {
    format!(
        "http://{}:{public_port}",
        instance_host_for(base_domain, id, preview_port)
    )
}

/// Resolve the `warrend` binary: `WARREN_BIN` wins; otherwise
/// `<repo>/target/release/warrend`, then `$CARGO_TARGET_DIR/release/warrend`.
pub fn binary_path_from(
    override_path: Option<OsString>,
    cargo_target_dir: Option<OsString>,
) -> Result<PathBuf> {
    let mut candidates: Vec<(PathBuf, &str)> = Vec::new();
    if let Some(p) = override_path {
        candidates.push((PathBuf::from(p), "WARREN_BIN"));
    } else {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/warrend");
        candidates.push((repo, "<repo>/target/release/warrend"));
        if let Some(t) = cargo_target_dir {
            candidates.push((
                PathBuf::from(t).join("release/warrend"),
                "$CARGO_TARGET_DIR/release/warrend",
            ));
        }
    }
    if let Some((p, _)) = candidates.iter().find(|(p, _)| p.is_file()) {
        return Ok(p.clone());
    }
    let tried: Vec<String> = candidates
        .iter()
        .map(|(p, why)| format!("{} ({why})", p.display()))
        .collect();
    Err(fail(format!(
        "warrend binary not found; tried {}. Build it with `cargo build --release -p warrend` (or `make build`, or \
         `scripts/e2e.sh` which builds everything) or point WARREN_BIN at a warrend binary.",
        tried.join(", ")
    )))
}

/// A running `warrend` process owned by one test.
pub struct Daemon {
    port: u16,
    base_domain: String,
    data_dir: PathBuf,
    log_path: PathBuf,
    child: Mutex<Option<Child>>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    raw_clients: Mutex<HashMap<String, reqwest::Client>>,
    vm: bool,
    health: Value,
    // Declared last so the directory is removed after the process has exited.
    root: TempRoot,
}

impl Daemon {
    /// `WARREN_E2E_VM=1` asks for `--firecracker --kernel ...` on every daemon.
    pub fn vm_requested() -> bool {
        std::env::var("WARREN_E2E_VM")
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    /// Where the daemon binary is (see [`binary_path_from`]).
    pub fn binary_path() -> Result<PathBuf> {
        binary_path_from(
            std::env::var_os("WARREN_BIN"),
            std::env::var_os("CARGO_TARGET_DIR"),
        )
    }

    /// Spawn `warrend --dev` on a free port with a fresh data directory, capture
    /// its output to `warrend.log` in the temp directory and wait for
    /// `GET /v1/health`. On failure the log tail is printed and the error says why.
    pub async fn start() -> Result<Daemon> {
        let bin = Self::binary_path()?;
        let vm = Self::vm_requested();
        let port = free_port()?;
        let root = TempRoot::new("warren-e2e")?;
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&data_dir).map_err(|e| transport("create data dir", e))?;
        let log_path = root.path().join("warrend.log");
        let log = File::create(&log_path).map_err(|e| transport("create daemon log", e))?;
        let log_err = log
            .try_clone()
            .map_err(|e| transport("clone daemon log handle", e))?;
        let base_domain = std::env::var("WARREN_E2E_BASE_DOMAIN")
            .unwrap_or_else(|_| DEFAULT_BASE_DOMAIN.to_string());
        let log_level = std::env::var("WARREN_E2E_LOG").unwrap_or_else(|_| "info".to_string());

        let mut cmd = Command::new(&bin);
        cmd.arg("--dev")
            .args(["--listen", &format!("[::]:{port}")])
            .args(["--public-port", &port.to_string()])
            .arg("--data-dir")
            .arg(&data_dir)
            .args(["--base-domain", &base_domain])
            .args([
                "--meter-interval",
                &format!("{}s", METER_INTERVAL.as_secs()),
            ])
            .args(["--min-idle-seconds", &MIN_IDLE_SECONDS.to_string()])
            .args(["--log", &log_level]);
        if let Some(tag) = std::env::var("WARREN_E2E_IMAGE_TAG")
            .ok()
            .filter(|t| !t.is_empty())
        {
            cmd.args(["--image-tag", &tag]);
        }
        if vm {
            let kernel =
                std::env::var("WARREN_E2E_KERNEL").unwrap_or_else(|_| DEFAULT_KERNEL.to_string());
            cmd.arg("--firecracker").args(["--kernel", &kernel]);
            let fc_bin = std::env::var("WARREN_E2E_FIRECRACKER_BIN")
                .ok()
                .or_else(|| {
                    Path::new(DEFAULT_FIRECRACKER_BIN)
                        .is_file()
                        .then(|| DEFAULT_FIRECRACKER_BIN.to_string())
                });
            if let Some(fc) = fc_bin {
                cmd.args(["--firecracker-bin", &fc]);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        let child = cmd
            .spawn()
            .map_err(|e| transport(&format!("spawn {}", bin.display()), e))?;
        eprintln!(
            "[e2e] warrend pid {} listening on {port}; log {}",
            child.id(),
            log_path.display()
        );

        let mut daemon = Daemon {
            port,
            base_domain,
            data_dir,
            log_path,
            child: Mutex::new(Some(child)),
            clients: Mutex::new(HashMap::new()),
            raw_clients: Mutex::new(HashMap::new()),
            vm,
            health: Value::Null,
            root,
        };
        match daemon.wait_healthy(STARTUP_TIMEOUT).await {
            Ok(health) => {
                daemon.health = health;
                Ok(daemon)
            }
            Err(e) => {
                daemon.dump_log_tail(60);
                Err(e)
            }
        }
    }

    async fn wait_healthy(&self, timeout: Duration) -> Result<Value> {
        let start = Instant::now();
        let url = format!("{}/health", self.api_url());
        let client = self.client(&self.api_host())?;
        let mut last;
        loop {
            if let Some(status) = self.try_wait() {
                return Err(fail(format!(
                    "warrend exited during startup ({status}); see the log tail above"
                )));
            }
            match client
                .get(&url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    return resp
                        .json::<Value>()
                        .await
                        .map_err(|e| transport("decode /v1/health", e));
                }
                Ok(resp) => last = format!("HTTP {}", resp.status()),
                Err(e) => last = e.to_string(),
            }
            if start.elapsed() >= timeout {
                return Err(fail(format!(
                    "warrend did not become healthy within {timeout:?} (last: {last})"
                )));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    fn try_wait(&self) -> Option<ExitStatus> {
        let mut guard = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        guard.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    /// The port the daemon listens on and advertises in URLs.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_domain(&self) -> &str {
        &self.base_domain
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The `GET /v1/health` body observed at startup.
    pub fn health(&self) -> &Value {
        &self.health
    }

    /// Runtimes the daemon reported at startup, e.g. `["docker"]`.
    pub fn runtimes(&self) -> Vec<String> {
        self.health["runtimes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `WARREN_E2E_VM=1` was set and the daemon reports the firecracker runtime.
    pub fn vm_enabled(&self) -> bool {
        self.vm && self.runtimes().iter().any(|r| r == "firecracker")
    }

    pub fn api_host(&self) -> String {
        format!("api.{}", self.base_domain)
    }

    /// `http://api.<base>:<port>/v1`
    pub fn api_url(&self) -> String {
        api_url_for(&self.base_domain, self.port)
    }

    /// `<id>.<base>` or `<id>-<preview_port>.<base>`.
    pub fn instance_host(&self, id: &str, preview_port: u16) -> String {
        instance_host_for(&self.base_domain, id, preview_port)
    }

    /// `http://<id>[-<preview_port>].<base>:<port>` without a trailing slash.
    pub fn instance_url(&self, id: &str, preview_port: u16) -> String {
        instance_url_for(&self.base_domain, self.port, id, preview_port)
    }

    /// The SDK's client (`warren_sdk::client_with_resolve`) with DNS for `host`
    /// pinned to this daemon, built on first use and cached per host.
    pub fn client(&self, host: &str) -> Result<reqwest::Client> {
        let host = host.to_ascii_lowercase();
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(c) = clients.get(&host) {
            return Ok(c.clone());
        }
        let client = crate::client_with_resolve(&host, self.addr())?;
        clients.insert(host, client.clone());
        Ok(client)
    }

    /// Like [`Daemon::client`] but never follows redirects, for inspecting the
    /// signed-URL `302` and raw edge answers.
    pub fn raw_client(&self, host: &str) -> Result<reqwest::Client> {
        let host = host.to_ascii_lowercase();
        let mut clients = self
            .raw_clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(c) = clients.get(&host) {
            return Ok(c.clone());
        }
        let client = crate::raw_client_with_resolve(&host, self.addr())?;
        clients.insert(host, client.clone());
        Ok(client)
    }

    /// [`Daemon::client`] for the host of an absolute URL.
    pub fn client_for_url(&self, url: &str) -> Result<reqwest::Client> {
        self.client(&host_of_url(url)?)
    }

    /// [`Daemon::raw_client`] for the host of an absolute URL.
    pub fn raw_client_for_url(&self, url: &str) -> Result<reqwest::Client> {
        self.raw_client(&host_of_url(url)?)
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port)
    }

    /// Last `lines` lines of the daemon log.
    pub fn log_tail(&self, lines: usize) -> String {
        tail_lines(&self.log_path, lines)
    }

    /// Print the log tail to stderr (shown by `cargo test` on failure or with `--nocapture`).
    pub fn dump_log_tail(&self, lines: usize) {
        eprintln!(
            "---- warrend log tail ({}) ----\n{}\n---- end of log ----",
            self.log_path.display(),
            self.log_tail(lines)
        );
    }

    /// Keep the temp directory (data dir and log) after the daemon is dropped.
    pub fn keep_data(&mut self) {
        self.root.keep();
    }

    /// Ask the daemon to shut down (SIGTERM), then kill it after 15 s.
    pub fn stop(&self) {
        let mut guard = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(mut child) = guard.take() else {
            return;
        };
        let pid = child.id().to_string();
        let _ = Command::new("kill")
            .args(["-TERM", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

fn host_of_url(url: &str) -> Result<String> {
    crate::util::host_of(url).ok_or_else(|| fail(format!("no host in url {url}")))
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        if std::thread::panicking() {
            self.dump_log_tail(80);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_urls() {
        assert_eq!(
            api_url_for("warren.localhost", 7337),
            "http://api.warren.localhost:7337/v1"
        );
        assert_eq!(
            instance_host_for("warren.localhost", "ab12cd34ef", 0),
            "ab12cd34ef.warren.localhost"
        );
        assert_eq!(
            instance_host_for("warren.localhost", "ab12cd34ef", 7681),
            "ab12cd34ef-7681.warren.localhost"
        );
        assert_eq!(
            instance_url_for("warren.localhost", 7337, "ab12cd34ef", 0),
            "http://ab12cd34ef.warren.localhost:7337"
        );
        assert_eq!(
            instance_url_for("warren.localhost", 7337, "ab12cd34ef", 7681),
            "http://ab12cd34ef-7681.warren.localhost:7337"
        );
    }

    #[test]
    fn missing_binary_is_a_clear_error() {
        let err = binary_path_from(Some("/nonexistent/warrend".into()), None).unwrap_err();
        assert!(
            err.message.contains("/nonexistent/warrend (WARREN_BIN)"),
            "{err}"
        );
        assert!(
            err.message.contains("cargo build --release -p warrend"),
            "{err}"
        );
        let err = binary_path_from(None, Some("/nonexistent-target".into())).unwrap_err();
        assert!(
            err.message
                .contains("target/release/warrend (<repo>/target/release/warrend)"),
            "{err}"
        );
        assert!(
            err.message.contains(
                "/nonexistent-target/release/warrend ($CARGO_TARGET_DIR/release/warrend)"
            ),
            "{err}"
        );
    }

    #[test]
    fn existing_binary_is_found() {
        let root = TempRoot::new("warren-e2e-bin").unwrap();
        let fake = root.path().join("warrend");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();
        assert_eq!(
            binary_path_from(Some(fake.clone().into_os_string()), None).unwrap(),
            fake
        );
        std::fs::create_dir_all(root.path().join("release")).unwrap();
        std::fs::write(root.path().join("release/warrend"), b"x").unwrap();
        let found = binary_path_from(None, Some(root.path().as_os_str().to_owned()));
        // Either the repo's release binary exists (preferred) or the target-dir fallback is used.
        let found = found.unwrap();
        assert!(found.ends_with("release/warrend"), "{}", found.display());
    }
}
