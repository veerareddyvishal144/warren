//! Sleep with memory for Docker instances through the engine's checkpoint
//! API (`POST /containers/{id}/checkpoints`, then `start?checkpoint=`), which
//! gVisor serves with `runsc checkpoint` and `runsc restore` (runc needs
//! CRIU). The engine must run in experimental mode; `ready` checks that and
//! otherwise sleep keeps the disk only, as it always did.
//!
//! One checkpoint per container, named [`CHECKPOINT_ID`], single use: a
//! successful restore removes it, and so do stop, resize, recreate and
//! destroy. bollard creates, lists and deletes checkpoints but cannot start a
//! container from one, so that request goes over a plain HTTP/1.1 connection
//! to the same endpoint bollard uses (`DOCKER_HOST`, else the default socket).

use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use bollard::container::{
    CreateCheckpointOptions, DeleteCheckpointOptions, ListCheckpointsOptions,
};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, body::Bytes};
use hyper_util::rt::TokioIo;
use warren_core::{Error, Instance, Result};

use crate::{DockerRuntime, container::is_running, error, runtime::target};

/// Name of the one checkpoint a sleeping container keeps.
pub const CHECKPOINT_ID: &str = "sleep";

/// The default engine socket when `DOCKER_HOST` is unset.
pub const DEFAULT_SOCKET: &str = "/var/run/docker.sock";

/// Time allowed for the engine to write a checkpoint (the container's whole
/// memory goes to disk).
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(600);
/// Time allowed for `start?checkpoint=` to answer.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(300);

/// Where the engine listens, as bollard's local defaults resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Unix(PathBuf),
    /// `host:port`.
    Tcp(String),
}

/// The engine endpoint for `docker_host` (the `DOCKER_HOST` value): `unix://`
/// paths, `tcp://` and `http://` addresses (port 2375 when none is given), a
/// bare absolute path, or the default socket when unset.
pub(crate) fn endpoint_from(docker_host: Option<&str>) -> Result<Endpoint> {
    let raw = docker_host
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unix:///var/run/docker.sock");
    if let Some(path) = raw.strip_prefix("unix://") {
        let path = if path.is_empty() {
            DEFAULT_SOCKET
        } else {
            path
        };
        return Ok(Endpoint::Unix(PathBuf::from(path)));
    }
    if let Some(rest) = raw
        .strip_prefix("tcp://")
        .or_else(|| raw.strip_prefix("http://"))
    {
        let hostport = rest.trim_end_matches('/');
        if hostport.is_empty() {
            return Err(Error::not_supported(format!(
                "DOCKER_HOST {raw:?} names no host"
            )));
        }
        let hostport = if hostport
            .rsplit(':')
            .next()
            .is_some_and(|p| p.parse::<u16>().is_ok())
            && !hostport.ends_with(']')
        {
            hostport.to_string()
        } else {
            format!("{hostport}:2375")
        };
        return Ok(Endpoint::Tcp(hostport));
    }
    if raw.starts_with('/') {
        return Ok(Endpoint::Unix(PathBuf::from(raw)));
    }
    Err(Error::not_supported(format!(
        "DOCKER_HOST {raw:?} is not a unix:// or tcp:// endpoint; checkpoints need one of those"
    )))
}

/// The engine's `{"message": ...}` from an error body, else the body itself.
fn message_of(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(300).collect())
}

/// One HTTP/1.1 exchange with the engine over a fresh connection.
async fn raw_request(endpoint: &Endpoint, method: Method, path: &str) -> Result<(u16, Bytes)> {
    let what = format!("docker {method} {path}");
    let req = Request::builder()
        .method(method.clone())
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::new()))
        .map_err(|e| Error::internal(format!("{what}: build request: {e}")))?;
    match endpoint {
        Endpoint::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| Error::internal(format!("{what}: connect {}: {e}", path.display())))?;
            send(TokioIo::new(stream), req, &what).await
        }
        Endpoint::Tcp(hostport) => {
            let stream = tokio::net::TcpStream::connect(hostport)
                .await
                .map_err(|e| Error::internal(format!("{what}: connect {hostport}: {e}")))?;
            send(TokioIo::new(stream), req, &what).await
        }
    }
}

async fn send<T>(io: TokioIo<T>, req: Request<Full<Bytes>>, what: &str) -> Result<(u16, Bytes)>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| Error::internal(format!("{what}: handshake: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| Error::internal(format!("{what}: {e}")))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| Error::internal(format!("{what}: read body: {e}")))?
        .to_bytes();
    Ok((status, bytes))
}

impl DockerRuntime {
    /// Checkpoints are in use: `--docker-checkpoints` was given and the engine
    /// reported experimental mode to `ready`.
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoints.load(Ordering::SeqCst)
    }

    /// Record what `ready` learned about the engine. Requested checkpoints on
    /// a non-experimental engine are refused with a warning and sleep keeps
    /// the disk only.
    pub(crate) fn note_checkpoint_support(&self, experimental: bool, runtime: &str) {
        if !self.cfg.docker_checkpoints {
            self.checkpoints.store(false, Ordering::SeqCst);
            return;
        }
        if experimental {
            tracing::info!(
                runtime,
                "docker checkpoints on: sleep saves each container's memory and wake restores it"
            );
            if runtime != "runsc" {
                tracing::warn!(
                    runtime,
                    "checkpoints with this runtime need CRIU on the host; gVisor (runsc) needs nothing extra"
                );
            }
        } else if !self.checkpoints_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                "--docker-checkpoints was given but the Docker engine is not in experimental mode; sleep keeps the disk only. Set \"experimental\": true in /etc/docker/daemon.json and restart Docker"
            );
        }
        self.checkpoints.store(experimental, Ordering::SeqCst);
    }

    /// Sleep: checkpoint the running container (its memory to disk, the
    /// container exited) so `start` can restore it. Without checkpoints, or
    /// when the checkpoint fails, the container is stopped as before.
    pub(crate) async fn sleep_impl(&self, inst: &Instance, grace: Duration) -> Result<()> {
        if !self.checkpoints_enabled() {
            return self.stop_impl(inst, grace).await;
        }
        let reference = target(inst);
        let inspect = match self.inspect(&reference).await {
            Ok(i) => i,
            Err(e) if e.is_not_found() => {
                tracing::warn!(instance = %inst.id, container = %reference, "sleep: container does not exist");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if !is_running(&inspect) {
            tracing::debug!(instance = %inst.id, container = %reference, "sleep: container was not running; nothing to checkpoint");
            self.discard_checkpoint(&reference).await;
            return Ok(());
        }
        // A leftover from an interrupted earlier sleep would make the create fail.
        self.discard_checkpoint(&reference).await;
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            CHECKPOINT_TIMEOUT,
            self.docker.create_checkpoint(
                &reference,
                CreateCheckpointOptions {
                    checkpoint_id: CHECKPOINT_ID.to_string(),
                    checkpoint_dir: None,
                    exit: true,
                },
            ),
        )
        .await;
        match outcome {
            Ok(Ok(())) => {
                tracing::info!(
                    instance = %inst.id, container = %reference,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "memory saved; container checkpointed and exited"
                );
                Ok(())
            }
            Ok(Err(e)) if error::is_not_found(&e) => Err(Error::not_found("container")),
            Ok(Err(e)) => {
                tracing::warn!(
                    instance = %inst.id, container = %reference, error = %error::message(&e),
                    "checkpoint failed; stopping the container instead (this sleep keeps the disk only)"
                );
                self.discard_checkpoint(&reference).await;
                self.stop_impl(inst, grace).await
            }
            Err(_) => {
                tracing::warn!(
                    instance = %inst.id, container = %reference, timeout_s = CHECKPOINT_TIMEOUT.as_secs(),
                    "checkpoint did not finish in time; stopping the container instead (this sleep keeps the disk only)"
                );
                self.discard_checkpoint(&reference).await;
                self.stop_impl(inst, grace).await
            }
        }
    }

    /// Whether the container holds a sleep checkpoint. Errors (including an
    /// engine without the checkpoint endpoints) read as "no".
    pub(crate) async fn has_checkpoint(&self, reference: &str) -> bool {
        match self
            .docker
            .list_checkpoints(reference, None::<ListCheckpointsOptions>)
            .await
        {
            Ok(list) => list.iter().any(|c| c.name == CHECKPOINT_ID),
            Err(e) => {
                tracing::debug!(container = %reference, error = %error::message(&e), "cannot list checkpoints");
                false
            }
        }
    }

    /// Remove the sleep checkpoint; a missing one (or container) is fine.
    /// Returns whether one was removed.
    pub(crate) async fn discard_checkpoint(&self, reference: &str) -> bool {
        match self
            .docker
            .delete_checkpoint(reference, CHECKPOINT_ID, None::<DeleteCheckpointOptions>)
            .await
        {
            Ok(()) => {
                tracing::debug!(container = %reference, "checkpoint removed");
                true
            }
            Err(e) if error::is_not_found(&e) => false,
            Err(e) => {
                tracing::debug!(container = %reference, error = %error::message(&e), "cannot remove the checkpoint");
                false
            }
        }
    }

    /// `POST /containers/{reference}/start?checkpoint=sleep`: restore the
    /// container from its checkpoint. 204 and 304 (already running) are
    /// success; anything else is the engine's message.
    pub(crate) async fn start_from_checkpoint(&self, reference: &str) -> Result<()> {
        let endpoint = endpoint_from(std::env::var("DOCKER_HOST").ok().as_deref())?;
        let path = format!("/containers/{reference}/start?checkpoint={CHECKPOINT_ID}");
        let (status, body) =
            tokio::time::timeout(RESTORE_TIMEOUT, raw_request(&endpoint, Method::POST, &path))
                .await
                .map_err(|_| {
                    Error::timeout(format!(
                        "docker did not restore {reference} within {}s",
                        RESTORE_TIMEOUT.as_secs()
                    ))
                })??;
        match status {
            204 | 304 => Ok(()),
            404 => Err(Error::not_found("container")),
            s => Err(Error::internal(format!(
                "docker POST {path}: HTTP {s}: {}",
                message_of(&body)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_engine_endpoints() {
        assert_eq!(
            endpoint_from(None).unwrap(),
            Endpoint::Unix(PathBuf::from("/var/run/docker.sock"))
        );
        assert_eq!(
            endpoint_from(Some("unix:///tmp/x/docker.sock")).unwrap(),
            Endpoint::Unix(PathBuf::from("/tmp/x/docker.sock"))
        );
        assert_eq!(
            endpoint_from(Some("/var/run/docker-exp.sock")).unwrap(),
            Endpoint::Unix(PathBuf::from("/var/run/docker-exp.sock"))
        );
        assert_eq!(
            endpoint_from(Some("tcp://127.0.0.1:2376")).unwrap(),
            Endpoint::Tcp("127.0.0.1:2376".into())
        );
        assert_eq!(
            endpoint_from(Some("http://docker.local/")).unwrap(),
            Endpoint::Tcp("docker.local:2375".into())
        );
        assert!(endpoint_from(Some("ssh://user@host")).is_err());
        assert!(endpoint_from(Some("tcp://")).is_err());
    }

    #[test]
    fn reads_engine_error_messages() {
        assert_eq!(
            message_of(&Bytes::from_static(
                br#"{"message":"checkpoint sleep does not exist for container x"}"#
            )),
            "checkpoint sleep does not exist for container x"
        );
        assert_eq!(
            message_of(&Bytes::from_static(b"page not found")),
            "page not found"
        );
    }
}
