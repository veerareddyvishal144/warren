//! The Firecracker API over its Unix socket: the few calls sleep and wake need
//! (`GET /`, `PATCH /vm`, `PUT /snapshot/create`, `PUT /snapshot/load`),
//! spoken with hyper over `tokio::net::UnixStream`. A VM booted with
//! `--config-file` keeps the API available unless `--no-api` is given, which
//! is why the runtime never passes `--no-api` any more.
//!
//! Faults Firecracker reports (`{"fault_message": ...}` with a 4xx or 5xx)
//! and transport failures (socket missing, process gone) are both
//! `Error::internal`; [`is_fault`] tells them apart, because a fault on a
//! snapshot load means the snapshot is unusable while a transport failure may
//! be transient.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, body::Bytes};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use warren_core::{Error, Result};

/// Key in `Error::details` carrying Firecracker's `fault_message`.
pub const FAULT_KEY: &str = "firecracker_fault";
/// Time allowed for the API socket to answer `GET /` after the process starts.
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
/// Request timeout for quick calls (`GET /`, `PATCH /vm`).
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);
/// Request timeout for snapshot creation and loading, which write or map the
/// whole guest memory.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(900);

/// `GET /`: what the process says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub app_name: String,
    pub id: String,
    /// `Not started`, `Running` or `Paused`.
    pub state: String,
    /// The build version, e.g. `1.18.0`.
    pub vmm_version: String,
}

/// Body of `PATCH /vm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmState {
    pub state: String,
}

/// Body of `PUT /snapshot/create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCreateParams {
    pub snapshot_type: String,
    pub snapshot_path: String,
    pub mem_file_path: String,
}

/// `mem_backend` of `PUT /snapshot/load`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBackend {
    pub backend_type: String,
    pub backend_path: String,
}

/// Body of `PUT /snapshot/load`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotLoadParams {
    pub snapshot_path: String,
    pub mem_backend: MemoryBackend,
    pub enable_diff_snapshots: bool,
    pub resume_vm: bool,
}

/// Firecracker's error body.
#[derive(Debug, Deserialize)]
struct Fault {
    fault_message: String,
}

/// One VM's API socket.
#[derive(Debug, Clone)]
pub struct Api {
    sock: PathBuf,
}

/// True when the error is a fault Firecracker answered (as opposed to a
/// transport failure).
pub fn is_fault(e: &Error) -> bool {
    e.details.as_ref().and_then(|d| d.get(FAULT_KEY)).is_some()
}

/// The fault message of a Firecracker error, if it is one.
pub fn fault_message(e: &Error) -> Option<String> {
    e.details
        .as_ref()
        .and_then(|d| d.get(FAULT_KEY))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Longest socket path handed to `connect(2)` as is. `sun_path` holds 108
/// bytes on Linux (104 on macOS) including the terminator; anything longer
/// goes through [`connect_unix`]'s descriptor route.
pub const MAX_DIRECT_SOCKET_PATH: usize = 100;

/// Connect to the Unix socket at `path` whatever the length of the path.
/// `connect(2)` takes at most `sun_path` bytes, and the jailer's socket sits
/// deep under the chroot base (`<data>/firecracker/jail/firecracker/<id>/
/// root/run/firecracker.socket`), so a long path is reached through its
/// directory's descriptor as `/proc/self/fd/<n>/<name>`, which is short.
pub async fn connect_unix(path: &Path) -> std::io::Result<tokio::net::UnixStream> {
    if path.as_os_str().len() <= MAX_DIRECT_SOCKET_PATH {
        return tokio::net::UnixStream::connect(path).await;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .ok_or_else(|| std::io::Error::other("socket path has no directory"))?;
        let name = path
            .file_name()
            .ok_or_else(|| std::io::Error::other("socket path has no file name"))?;
        let dirfd = std::fs::File::open(dir)?;
        let short = PathBuf::from(format!("/proc/self/fd/{}", dirfd.as_raw_fd())).join(name);
        let stream = tokio::net::UnixStream::connect(&short).await;
        drop(dirfd);
        stream
    }
    #[cfg(not(target_os = "linux"))]
    Err(std::io::Error::other(format!(
        "socket path {} is longer than this platform allows ({} bytes)",
        path.display(),
        path.as_os_str().len()
    )))
}

impl Api {
    /// An API client for the socket at `sock` (a host path).
    pub fn new(sock: impl Into<PathBuf>) -> Api {
        Api { sock: sock.into() }
    }

    /// `GET /`.
    pub async fn info(&self) -> Result<InstanceInfo> {
        let body = self.request(Method::GET, "/", None, QUICK_TIMEOUT).await?;
        serde_json::from_slice(&body)
            .map_err(|e| Error::internal(format!("firecracker GET /: unreadable answer: {e}")))
    }

    /// Poll `GET /` until the process answers, `timeout` passes or
    /// `still_alive` says the process is gone.
    pub async fn wait_ready(
        &self,
        timeout: Duration,
        mut still_alive: impl FnMut() -> bool,
    ) -> Result<InstanceInfo> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        loop {
            if self.sock.exists() {
                match self.info().await {
                    Ok(info) => return Ok(info),
                    Err(e) => last = e.to_string(),
                }
            }
            if !still_alive() {
                return Err(Error::internal(format!(
                    "firecracker exited before its API socket {} answered",
                    self.sock.display()
                )));
            }
            if Instant::now() >= deadline {
                return Err(Error::timeout(format!(
                    "firecracker API socket {} did not answer within {}s ({last})",
                    self.sock.display(),
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `PATCH /vm {"state": "Paused"}`.
    pub async fn pause(&self) -> Result<()> {
        self.set_state("Paused").await
    }

    /// `PATCH /vm {"state": "Resumed"}`.
    pub async fn resume(&self) -> Result<()> {
        self.set_state("Resumed").await
    }

    async fn set_state(&self, state: &str) -> Result<()> {
        let body = serde_json::to_vec(&VmState {
            state: state.to_string(),
        })?;
        self.request(Method::PATCH, "/vm", Some(body), QUICK_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// `PUT /snapshot/create` of a full snapshot: the VM state to `vmstate`
    /// and the guest memory to `mem`, both paths as Firecracker sees them.
    /// The VM must be paused.
    pub async fn create_snapshot(&self, vmstate: &Path, mem: &Path) -> Result<()> {
        let body = serde_json::to_vec(&SnapshotCreateParams {
            snapshot_type: "Full".into(),
            snapshot_path: vmstate.to_string_lossy().into_owned(),
            mem_file_path: mem.to_string_lossy().into_owned(),
        })?;
        self.request(
            Method::PUT,
            "/snapshot/create",
            Some(body),
            SNAPSHOT_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    /// `PUT /snapshot/load` from `vmstate` and the memory file `mem` (paths
    /// as Firecracker sees them), resuming the guest at once. Only a fresh
    /// process that has configured nothing accepts it.
    pub async fn load_snapshot(&self, vmstate: &Path, mem: &Path) -> Result<()> {
        let body = serde_json::to_vec(&SnapshotLoadParams {
            snapshot_path: vmstate.to_string_lossy().into_owned(),
            mem_backend: MemoryBackend {
                backend_type: "File".into(),
                backend_path: mem.to_string_lossy().into_owned(),
            },
            enable_diff_snapshots: false,
            resume_vm: true,
        })?;
        self.request(Method::PUT, "/snapshot/load", Some(body), SNAPSHOT_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// One HTTP/1.1 exchange over a fresh connection. 2xx returns the body;
    /// anything else is an error carrying Firecracker's `fault_message`.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Bytes> {
        let what = format!("firecracker {method} {path}");
        let fut = async {
            let stream = connect_unix(&self.sock).await.map_err(|e| {
                Error::internal(format!("{what}: connect {}: {e}", self.sock.display()))
            })?;
            let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|e| Error::internal(format!("{what}: handshake: {e}")))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let req = Request::builder()
                .method(method.clone())
                .uri(path)
                .header("Host", "localhost")
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(body.unwrap_or_default())))
                .map_err(|e| Error::internal(format!("{what}: build request: {e}")))?;
            let resp = sender
                .send_request(req)
                .await
                .map_err(|e| Error::internal(format!("{what}: {e}")))?;
            let status = resp.status();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| Error::internal(format!("{what}: read body: {e}")))?
                .to_bytes();
            if status.is_success() {
                return Ok(bytes);
            }
            let message = serde_json::from_slice::<Fault>(&bytes)
                .map(|f| f.fault_message)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).chars().take(300).collect());
            let mut e = Error::internal(format!("{what}: HTTP {}: {message}", status.as_u16()));
            let mut details = e.details.take().unwrap_or_else(|| serde_json::json!({}));
            details[FAULT_KEY] = serde_json::Value::String(message);
            details["status"] = serde_json::Value::from(status.as_u16());
            e.details = Some(details);
            Err(e)
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(Error::timeout(format!(
                "{what} did not answer within {}s",
                timeout.as_secs()
            ))),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::{Arc, Mutex};

    use hyper::{Response, service::service_fn};
    use serde_json::{Value, json};

    use super::*;

    /// A request the fake server saw.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Seen {
        pub method: String,
        pub path: String,
        pub body: Value,
    }

    /// A fake Firecracker API on a Unix socket: records every request and
    /// answers from a script keyed by `METHOD path`, or `204` by default.
    pub struct FakeApi {
        pub sock: PathBuf,
        pub seen: Arc<Mutex<Vec<Seen>>>,
        answers: Arc<Mutex<Vec<(String, u16, Value)>>>,
    }

    static SOCK_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    impl FakeApi {
        /// Bind a short socket path under `/tmp` (the sun_path limit is about
        /// 100 bytes) and start serving.
        pub fn start() -> FakeApi {
            let n = SOCK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let sock = PathBuf::from(format!("/tmp/warren-fcapi-{}-{n}.sock", std::process::id()));
            let _ = std::fs::remove_file(&sock);
            let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake api socket");
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
            let answers: Arc<Mutex<Vec<(String, u16, Value)>>> = Arc::new(Mutex::new(Vec::new()));
            let (seen2, answers2) = (seen.clone(), answers.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let (seen, answers) = (seen2.clone(), answers2.clone());
                    tokio::spawn(async move {
                        let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let (seen, answers) = (seen.clone(), answers.clone());
                            async move {
                                let method = req.method().to_string();
                                let path = req.uri().path().to_string();
                                let bytes = req.into_body().collect().await.unwrap().to_bytes();
                                let body: Value = if bytes.is_empty() {
                                    Value::Null
                                } else {
                                    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
                                };
                                seen.lock().unwrap().push(Seen {
                                    method: method.clone(),
                                    path: path.clone(),
                                    body,
                                });
                                let key = format!("{method} {path}");
                                let (status, answer) = answers
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .find(|(k, _, _)| *k == key)
                                    .map(|(_, s, v)| (*s, v.clone()))
                                    .unwrap_or((204, Value::Null));
                                let text = if answer.is_null() {
                                    String::new()
                                } else {
                                    answer.to_string()
                                };
                                Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(status)
                                        .header("Content-Type", "application/json")
                                        .body(Full::new(Bytes::from(text)))
                                        .unwrap(),
                                )
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
            });
            FakeApi {
                sock,
                seen,
                answers,
            }
        }

        /// Answer `METHOD path` with `status` and `body`.
        pub fn answer(&self, method_path: &str, status: u16, body: Value) {
            self.answers
                .lock()
                .unwrap()
                .push((method_path.to_string(), status, body));
        }

        pub fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for FakeApi {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.sock);
        }
    }

    pub fn running_info() -> Value {
        json!({"app_name": "Firecracker", "id": "anonymous-instance", "state": "Running", "vmm_version": "1.18.0"})
    }

    /// The jailed socket path can exceed `sun_path`; the client still
    /// reaches it (through the directory's descriptor on Linux).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reaches_a_socket_whose_path_is_too_long_for_connect() {
        let fake = FakeApi::start();
        fake.answer("GET /", 200, running_info());
        let tmp = crate::testutil::TempDir::new("fcapi-long");
        let mut deep = tmp.path().to_path_buf();
        while deep.as_os_str().len() < 120 {
            deep.push("a-directory-with-a-long-name-for-this-test");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let long = deep.join("firecracker.socket");
        std::os::unix::fs::symlink(&fake.sock, &long).unwrap();
        assert!(long.as_os_str().len() > 108);
        assert!(
            tokio::net::UnixStream::connect(&long).await.is_err(),
            "a direct connect to the long path fails, which is what the route works around"
        );
        let info = Api::new(&long).info().await.unwrap();
        assert_eq!(info.vmm_version, "1.18.0");
    }

    #[tokio::test]
    async fn speaks_the_snapshot_protocol() {
        let fake = FakeApi::start();
        fake.answer("GET /", 200, running_info());
        let api = Api::new(&fake.sock);
        let info = api
            .wait_ready(Duration::from_secs(5), || true)
            .await
            .unwrap();
        assert_eq!(info.vmm_version, "1.18.0");
        assert_eq!(info.state, "Running");
        api.pause().await.unwrap();
        api.create_snapshot(Path::new("/snapshot/vmstate"), Path::new("/snapshot/mem"))
            .await
            .unwrap();
        api.load_snapshot(Path::new("/s/vmstate"), Path::new("/s/mem"))
            .await
            .unwrap();
        api.resume().await.unwrap();
        let seen = fake.seen();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, "/");
        assert_eq!(
            (
                seen[1].method.as_str(),
                seen[1].path.as_str(),
                &seen[1].body
            ),
            ("PATCH", "/vm", &json!({"state": "Paused"}))
        );
        assert_eq!(
            (
                seen[2].method.as_str(),
                seen[2].path.as_str(),
                &seen[2].body
            ),
            (
                "PUT",
                "/snapshot/create",
                &json!({"snapshot_type": "Full", "snapshot_path": "/snapshot/vmstate", "mem_file_path": "/snapshot/mem"})
            )
        );
        assert_eq!(
            (
                seen[3].method.as_str(),
                seen[3].path.as_str(),
                &seen[3].body
            ),
            (
                "PUT",
                "/snapshot/load",
                &json!({
                    "snapshot_path": "/s/vmstate",
                    "mem_backend": {"backend_type": "File", "backend_path": "/s/mem"},
                    "enable_diff_snapshots": false,
                    "resume_vm": true
                })
            )
        );
        assert_eq!(seen[4].body, json!({"state": "Resumed"}));
    }

    #[tokio::test]
    async fn faults_are_distinguished_from_transport_failures() {
        let fake = FakeApi::start();
        fake.answer(
            "PUT /snapshot/load",
            400,
            json!({"fault_message": "Load snapshot error: incompatible snapshot version"}),
        );
        let api = Api::new(&fake.sock);
        let err = api
            .load_snapshot(Path::new("/s/vmstate"), Path::new("/s/mem"))
            .await
            .unwrap_err();
        assert!(err.is_internal());
        assert!(is_fault(&err));
        assert_eq!(
            fault_message(&err).as_deref(),
            Some("Load snapshot error: incompatible snapshot version")
        );
        assert!(
            err.details.as_ref().unwrap()["cause"]
                .as_str()
                .unwrap()
                .contains("HTTP 400")
        );

        let gone = Api::new("/tmp/warren-fcapi-does-not-exist.sock");
        let err = gone.info().await.unwrap_err();
        assert!(err.is_internal() && !is_fault(&err));
        assert_eq!(fault_message(&err), None);
        let err = gone
            .wait_ready(Duration::from_millis(300), || true)
            .await
            .unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_TIMEOUT);
        let err = gone
            .wait_ready(Duration::from_secs(5), || false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exited") || err.is_internal());
    }
}
