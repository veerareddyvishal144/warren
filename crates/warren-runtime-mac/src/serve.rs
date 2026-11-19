//! Keeping `lume serve` alive. The API server hosts every macOS VM Warren
//! starts, so it is started when the API does not answer and watched while
//! the daemon runs. It is spawned in its own process group with its output
//! in a log file, and it is never stopped by the daemon: a daemon restart
//! must not take the VMs down with it.

use std::{path::Path, process::Stdio, time::Duration};

use tokio_util::sync::CancellationToken;
use warren_core::{Config, Error, Result};

use crate::lume::LumeClient;

/// How long a freshly started server may take to answer.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the supervisor checks the server while the daemon runs.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(15);

pub fn log_path(cfg: &Config) -> std::path::PathBuf {
    cfg.mac_dir().join("lume-serve.log")
}

/// The port in `lume_api_url`, and whether the URL points at this machine.
fn local_port(url: &str) -> Result<Option<u16>> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| Error::invalid(format!("--lume-api {url:?} is not a URL: {e}")))?;
    let local = matches!(
        parsed.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
    );
    Ok(local.then(|| parsed.port().unwrap_or(7777)))
}

/// Make sure the Lume API answers: return at once when it does, otherwise
/// start `lume serve --port <port>` (when the URL is local) and wait for it.
pub async fn ensure_running(cfg: &Config, client: &LumeClient) -> Result<()> {
    if client.host_status().await.is_ok() {
        return Ok(());
    }
    let Some(port) = local_port(&cfg.lume_api_url)? else {
        return Err(Error::try_again(format!(
            "the Lume API at {} is unreachable and not local; start `lume serve` there",
            cfg.lume_api_url
        )));
    };
    spawn(&cfg.lume_bin, port, &log_path(cfg))?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        match client.host_status().await {
            Ok(status) => {
                tracing::info!(port, version = %status.version, "lume serve is up");
                return Ok(());
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                return Err(Error::try_again(format!(
                    "`lume serve --port {port}` did not answer within {}s (log: {}): {e}",
                    START_TIMEOUT.as_secs(),
                    log_path(cfg).display()
                )));
            }
        }
    }
}

fn spawn(lume_bin: &Path, port: u16, log: &Path) -> Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", dir.display())))?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| Error::internal(format!("open {}: {e}", log.display())))?;
    let err = out
        .try_clone()
        .map_err(|e| Error::internal(format!("dup log handle: {e}")))?;
    let mut cmd = std::process::Command::new(lume_bin);
    cmd.arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: a Ctrl-C to the daemon must not reach it.
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| {
        Error::not_supported(format!(
            "cannot start `{} serve`: {e}; install Lume 0.7.2 or newer (https://github.com/trycua/cua) or point --lume-api at a running server",
            lume_bin.display()
        ))
    })?;
    tracing::info!(pid = child.id(), port, log = %log.display(), "started lume serve");
    // The child is intentionally not waited on; it outlives the daemon.
    std::mem::forget(child);
    Ok(())
}

/// Watch the API while the daemon runs and restart the server when it is
/// gone. VMs it hosted are gone with it; the lifecycle's repair pass marks
/// their instances stopped and the next request boots them again.
pub async fn supervise(cfg: Config, client: LumeClient, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(CHECK_INTERVAL) => {}
        }
        if client.host_status().await.is_ok() {
            continue;
        }
        tracing::warn!(url = %client.base_url(), "lume serve is not answering; starting it again");
        if let Err(e) = ensure_running(&cfg, &client).await {
            tracing::error!(error = %e, "could not bring lume serve back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls_and_ports() {
        assert_eq!(local_port("http://127.0.0.1:7777").unwrap(), Some(7777));
        assert_eq!(local_port("http://localhost").unwrap(), Some(7777));
        assert_eq!(local_port("http://127.0.0.1:7778/").unwrap(), Some(7778));
        assert_eq!(local_port("http://mac-mini.local:7777").unwrap(), None);
        assert!(local_port("not a url").is_err());
    }

    #[tokio::test]
    async fn remote_api_down_is_reported_not_spawned() {
        let cfg = Config {
            lume_api_url: "http://192.0.2.1:7777".into(),
            ..Config::default()
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let client = LumeClient::new(http, &cfg.lume_api_url);
        let err = ensure_running(&cfg, &client).await.unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_TRY_AGAIN);
        assert!(err.message.contains("not local"), "{}", err.message);
    }

    #[tokio::test]
    async fn missing_lume_binary_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("warren-serve-{}", std::process::id()));
        let cfg = Config {
            lume_api_url: "http://127.0.0.1:1".into(),
            lume_bin: "/definitely/missing/lume".into(),
            data_dir: dir.clone(),
            ..Config::default()
        };
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let client = LumeClient::new(http, &cfg.lume_api_url);
        let err = ensure_running(&cfg, &client).await.unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_NOT_SUPPORTED);
        assert!(err.message.contains("install Lume"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
