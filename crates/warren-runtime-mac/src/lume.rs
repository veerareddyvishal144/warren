//! A client for the Lume API server (`lume serve`, `http://127.0.0.1:7777`).
//!
//! Only the calls the runtime needs are wrapped. Lume answers errors as
//! `{ "message": "..." }` with a 4xx status; a VM that does not exist is a
//! 400 whose message starts with "Virtual machine not found", which this
//! client turns into `None`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use warren_core::{Error, Result};

/// `GET /lume/host/status`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    #[serde(default)]
    pub status: String,
    /// Running VMs, macOS and Linux.
    #[serde(default)]
    pub vm_count: u32,
    /// The most macOS guests the host runs at once (2). Lume spells the key
    /// `maxVMs`.
    #[serde(default, rename = "maxVMs")]
    pub max_vms: u32,
    /// macOS guests that can still start.
    #[serde(default)]
    pub available_slots: u32,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct DiskSize {
    #[serde(default)]
    pub allocated: u64,
    #[serde(default)]
    pub total: u64,
}

/// One VM as `GET /lume/vms/:name` describes it. Fields Lume may omit are
/// optional; unknown fields are ignored so newer Lume versions keep working.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmDetails {
    pub name: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub cpu_count: u32,
    /// Bytes.
    #[serde(default)]
    pub memory_size: u64,
    #[serde(default)]
    pub disk_size: DiskSize,
    /// "running", "stopped", "provisioning", "pulling".
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub ip_address: Option<String>,
    #[serde(default)]
    pub ssh_available: Option<bool>,
    #[serde(default)]
    pub saved_state: Option<bool>,
    #[serde(default)]
    pub vnc_url: Option<String>,
    #[serde(default)]
    pub mac_address: Option<String>,
    #[serde(default)]
    pub machine_identifier: Option<String>,
    #[serde(default)]
    pub location_name: Option<String>,
}

impl VmDetails {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
    pub fn is_macos(&self) -> bool {
        self.os.eq_ignore_ascii_case("macos")
    }
    pub fn has_saved_state(&self) -> bool {
        self.saved_state.unwrap_or(false)
    }
    /// Memory in whole GiB, rounded to the nearest.
    pub fn memory_gb(&self) -> u32 {
        ((self.memory_size + GIB / 2) / GIB) as u32
    }
    /// Disk in whole GiB, rounded up.
    pub fn disk_gb(&self) -> u32 {
        self.disk_size.total.div_ceil(GIB) as u32
    }
}

pub const GIB: u64 = 1 << 30;

/// `POST /lume/vms/:name/run`.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub no_display: bool,
    /// "enabled" or "disabled".
    pub vnc: String,
    pub shared_directories: Vec<SharedDirectory>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedDirectory {
    pub host_path: String,
    pub read_only: bool,
}

/// `PATCH /lume/vms/:name`; absent fields are left alone. Sizes are strings
/// with a unit ("8GB") as Lume expects.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_size: Option<String>,
}

/// A VM storage location from `GET /lume/config`.
#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub default_location_name: String,
    #[serde(default)]
    pub vm_locations: Vec<Location>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone)]
pub struct LumeClient {
    http: reqwest::Client,
    base: String,
}

/// Lume waits for a graceful shutdown and then five more seconds for locks,
/// and a suspend writes the guest's memory to disk; both need long timeouts.
const SLOW_CALL_TIMEOUT: Duration = Duration::from_secs(240);
const FAST_CALL_TIMEOUT: Duration = Duration::from_secs(20);

impl LumeClient {
    pub fn new(http: reqwest::Client, base_url: &str) -> LumeClient {
        LumeClient {
            http,
            base: base_url.trim_end_matches('/').to_string(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn transport(&self, e: reqwest::Error) -> Error {
        if e.is_timeout() {
            Error::timeout(format!(
                "the Lume API at {} did not answer in time",
                self.base
            ))
        } else if e.is_connect() {
            Error::try_again(format!("the Lume API at {} is unreachable: {e}", self.base))
        } else {
            Error::internal(format!("Lume API request failed: {e}"))
        }
    }

    /// Send a request and return the body; errors carry Lume's message.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, bytes::Bytes)> {
        let resp = req.send().await.map_err(|e| self.transport(e))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| self.transport(e))?;
        Ok((status, body))
    }

    fn message(body: &[u8]) -> String {
        serde_json::from_slice::<ApiError>(body)
            .map(|e| e.message)
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(200).collect())
    }

    fn failure(what: &str, status: u16, body: &[u8]) -> Error {
        Error::internal(format!(
            "lume {what} failed (HTTP {status}): {}",
            Self::message(body)
        ))
    }

    fn is_not_found(status: u16, body: &[u8]) -> bool {
        status == 404
            || (status == 400 && Self::message(body).contains("Virtual machine not found"))
    }

    pub async fn host_status(&self) -> Result<HostStatus> {
        let (status, body) = self
            .send(
                self.http
                    .get(self.url("/lume/host/status"))
                    .timeout(FAST_CALL_TIMEOUT),
            )
            .await?;
        if status != 200 {
            return Err(Self::failure("host status", status, &body));
        }
        serde_json::from_slice(&body)
            .map_err(|e| Error::internal(format!("lume host status is unreadable: {e}")))
    }

    pub async fn settings(&self) -> Result<Settings> {
        let (status, body) = self
            .send(
                self.http
                    .get(self.url("/lume/config"))
                    .timeout(FAST_CALL_TIMEOUT),
            )
            .await?;
        if status != 200 {
            return Err(Self::failure("config", status, &body));
        }
        serde_json::from_slice(&body)
            .map_err(|e| Error::internal(format!("lume config is unreadable: {e}")))
    }

    pub async fn list(&self) -> Result<Vec<VmDetails>> {
        let (status, body) = self
            .send(
                self.http
                    .get(self.url("/lume/vms"))
                    .timeout(FAST_CALL_TIMEOUT),
            )
            .await?;
        if status != 200 {
            return Err(Self::failure("list", status, &body));
        }
        serde_json::from_slice(&body)
            .map_err(|e| Error::internal(format!("lume vm list is unreadable: {e}")))
    }

    /// The VM, or `None` when Lume does not know it.
    pub async fn get(&self, name: &str) -> Result<Option<VmDetails>> {
        let (status, body) = self
            .send(
                self.http
                    .get(self.url(&format!("/lume/vms/{name}")))
                    .timeout(FAST_CALL_TIMEOUT),
            )
            .await?;
        if Self::is_not_found(status, &body) {
            return Ok(None);
        }
        if status != 200 {
            return Err(Self::failure("get", status, &body));
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| Error::internal(format!("lume vm details are unreadable: {e}")))
    }

    /// APFS clone of `src` into a new VM `dst`; the saved state is never
    /// carried over, so the clone boots from disk with its own identity.
    pub async fn clone_vm(&self, src: &str, dst: &str) -> Result<()> {
        let (status, body) = self
            .send(
                self.http
                    .post(self.url("/lume/vms/clone"))
                    .json(&serde_json::json!({ "name": src, "newName": dst, "withSavedState": false }))
                    .timeout(SLOW_CALL_TIMEOUT),
            )
            .await?;
        if !(200..300).contains(&status) {
            if Self::is_not_found(status, &body) {
                return Err(Error::not_found(&format!("lume vm {src}")));
            }
            return Err(Self::failure("clone", status, &body));
        }
        Ok(())
    }

    /// Start the VM. Lume accepts the request (202) and boots in the
    /// background; poll [`Self::get`] for `running` and an address. A full
    /// host answers 409, mapped to `host_capacity_reached`.
    pub async fn run(&self, name: &str, req: &RunRequest) -> Result<()> {
        let (status, body) = self
            .send(
                self.http
                    .post(self.url(&format!("/lume/vms/{name}/run")))
                    .json(req)
                    .timeout(FAST_CALL_TIMEOUT),
            )
            .await?;
        match status {
            200..=299 => Ok(()),
            409 => Err(Error::host_capacity(MAX_MACOS_GUESTS, MAX_MACOS_GUESTS)),
            _ if Self::is_not_found(status, &body) => {
                Err(Error::not_found(&format!("lume vm {name}")))
            }
            _ => Err(Self::failure("run", status, &body)),
        }
    }

    /// Stop the VM: graceful, then forced after `timeout_secs` (`force` skips
    /// the graceful phase). Lume waits five extra seconds for locks.
    pub async fn stop(&self, name: &str, force: bool, timeout_secs: u64) -> Result<()> {
        let (status, body) = self
            .send(
                self.http
                    .post(self.url(&format!("/lume/vms/{name}/stop")))
                    .json(&serde_json::json!({ "force": force, "timeout": timeout_secs }))
                    .timeout(SLOW_CALL_TIMEOUT),
            )
            .await?;
        if (200..300).contains(&status) || Self::is_not_found(status, &body) {
            return Ok(());
        }
        // Lume reports a VM that is already down as an error; that is success here.
        let msg = Self::message(&body);
        if msg.contains("not running") {
            return Ok(());
        }
        Err(Self::failure("stop", status, &body))
    }

    /// Save the running VM's memory next to its disk and stop it; the next
    /// run resumes from that state. Only VMs this API server started can be
    /// suspended.
    pub async fn suspend(&self, name: &str) -> Result<()> {
        let (status, body) = self
            .send(
                self.http
                    .post(self.url(&format!("/lume/vms/{name}/suspend")))
                    .timeout(SLOW_CALL_TIMEOUT),
            )
            .await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(Self::failure("suspend", status, &body))
    }

    pub async fn set(&self, name: &str, req: &SetRequest) -> Result<()> {
        let (status, body) = self
            .send(
                self.http
                    .patch(self.url(&format!("/lume/vms/{name}")))
                    .json(req)
                    .timeout(SLOW_CALL_TIMEOUT),
            )
            .await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        if Self::is_not_found(status, &body) {
            return Err(Error::not_found(&format!("lume vm {name}")));
        }
        Err(Self::failure("set", status, &body))
    }

    /// Remove the VM directory. A VM Lume does not know is already gone.
    pub async fn delete(&self, name: &str) -> Result<()> {
        let (status, body) = self
            .send(
                self.http
                    .delete(self.url(&format!("/lume/vms/{name}")))
                    .timeout(SLOW_CALL_TIMEOUT),
            )
            .await?;
        if (200..300).contains(&status) || Self::is_not_found(status, &body) {
            return Ok(());
        }
        Err(Self::failure("delete", status, &body))
    }
}

/// Virtualization.framework runs at most this many macOS guests per host.
pub const MAX_MACOS_GUESTS: u32 = 2;

/// Parse "0.7.2" style versions into comparable tuples; unknown parts are 0.
pub fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut it = v
        .trim()
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// The oldest Lume whose API this runtime relies on (async run, suspend,
/// host status).
pub const MIN_LUME_VERSION: (u32, u32, u32) = (0, 7, 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_parse_and_round() {
        let v: VmDetails = serde_json::from_str(
            r#"{"name":"warren-ab12cd34ef","os":"macOS","cpuCount":4,"memorySize":4294967296,
                "diskSize":{"allocated":25000000000,"total":53687091200},"display":"1024x768",
                "status":"running","provisioningOperation":null,"vncUrl":null,
                "ipAddress":"192.168.64.3","sshAvailable":true,"locationName":"home",
                "sharedDirectories":null,"networkMode":"nat","downloadProgress":null,
                "machineIdentifier":"YnBs","macAddress":"aa:bb","savedState":false}"#,
        )
        .unwrap();
        assert!(v.is_running() && v.is_macos() && !v.has_saved_state());
        assert_eq!(v.memory_gb(), 4);
        assert_eq!(v.disk_gb(), 50);
        assert_eq!(v.ip_address.as_deref(), Some("192.168.64.3"));
        // Older or sparser answers still parse.
        let v: VmDetails = serde_json::from_str(r#"{"name":"x","status":"stopped"}"#).unwrap();
        assert!(!v.is_running() && v.disk_gb() == 0);
    }

    #[test]
    fn host_status_uses_lumes_spelling() {
        let h: HostStatus = serde_json::from_str(
            r#"{"status":"healthy","vmCount":1,"maxVMs":2,"availableSlots":1,"version":"0.7.2"}"#,
        )
        .unwrap();
        assert_eq!((h.vm_count, h.max_vms, h.available_slots), (1, 2, 1));
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.7.2"), (0, 7, 2));
        assert_eq!(parse_version("v0.10.0-beta"), (0, 10, 0));
        assert!(parse_version("0.7.2") >= MIN_LUME_VERSION);
        assert!(parse_version("0.1.13") < MIN_LUME_VERSION);
    }

    #[test]
    fn requests_serialize_as_lume_expects() {
        let r = RunRequest {
            no_display: true,
            vnc: "disabled".into(),
            shared_directories: vec![SharedDirectory {
                host_path: "/x/share".into(),
                read_only: true,
            }],
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["noDisplay"], true);
        assert_eq!(j["sharedDirectories"][0]["hostPath"], "/x/share");
        assert_eq!(j["sharedDirectories"][0]["readOnly"], true);
        let s = SetRequest {
            cpu: Some(4),
            memory: Some("8GB".into()),
            disk_size: None,
        };
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["cpu"], 4);
        assert_eq!(j["memory"], "8GB");
        assert!(j.get("diskSize").is_none());
    }
}
