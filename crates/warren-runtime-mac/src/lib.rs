//! The `mac` instance type: a macOS virtual machine per instance on an Apple
//! silicon host, managed through Lume's API server (`lume serve`).
//!
//! Every instance is an APFS clone of a base VM (`--mac-base-vm`) named
//! `warren-<instance id>`. It boots with a read-only shared folder holding
//! the `warren-gateway` binary and its environment ([`share`]); a
//! LaunchDaemon in the base image starts the gateway from there, and from
//! then on the instance is reached exactly like a Firecracker guest: the
//! edge proxies to `<vm ip>:3737`, exec goes through the gateway with the
//! instance token.
//!
//! Sleep is a suspend: Lume saves the VM's memory next to its disk and the
//! next start resumes it. A Mac runs at most two macOS guests at a time
//! ([`lume::MAX_MACOS_GUESTS`]); a start on a full host is refused with
//! `host_capacity_reached`.

pub mod lume;
pub mod serve;
pub mod share;
#[cfg(test)]
mod tests;

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use warren_core::{
    Config, Error, ExecRequest, ExecResult, GATEWAY_PORT, Instance, LogsOptions, Result, RunInfo,
    Runtime, STOP_GRACE_SECS, Shape, Stats, Template, errors,
};

use lume::{GIB, LumeClient, MIN_LUME_VERSION, RunRequest, SetRequest, SharedDirectory, VmDetails};

pub const RUNTIME_NAME: &str = "lume";
/// Prefix of every VM this runtime owns.
pub const VM_PREFIX: &str = "warren-";
/// Clone-boot to a healthy gateway: 11 s boot plus the launcher's copy on a
/// loaded laptop fits comfortably.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
/// Lume keeps the VM's saved memory in these files next to the disk.
const SAVED_STATE_FILES: [&str; 2] = ["state.vzvmsave", "state.vsock"];
const DISK_FILE: &str = "disk.img";
const NVRAM_FILE: &str = "nvram.bin";
const CONFIG_FILE: &str = "config.json";

pub struct MacRuntime {
    cfg: Config,
    http: reqwest::Client,
    lume: LumeClient,
    gateway_port: u16,
    boot_timeout: Duration,
}

impl MacRuntime {
    pub fn new(cfg: Config) -> Result<MacRuntime> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| Error::internal(format!("http client: {e}")))?;
        let lume = LumeClient::new(http.clone(), &cfg.lume_api_url);
        Ok(MacRuntime {
            cfg,
            http,
            lume,
            gateway_port: GATEWAY_PORT,
            boot_timeout: BOOT_TIMEOUT,
        })
    }

    /// Tests point the runtime at a gateway on another port and shorten waits.
    #[cfg(test)]
    pub(crate) fn with_test_knobs(mut self, gateway_port: u16, boot_timeout: Duration) -> Self {
        self.gateway_port = gateway_port;
        self.boot_timeout = boot_timeout;
        self
    }

    pub fn lume(&self) -> &LumeClient {
        &self.lume
    }

    pub fn vm_name(inst: &Instance) -> String {
        format!("{VM_PREFIX}{}", inst.id)
    }

    fn share_dir(&self, inst: &Instance) -> PathBuf {
        share::share_dir(&self.cfg.mac_dir(), &inst.id)
    }

    fn gateway_bin(&self) -> Result<&Path> {
        self.cfg.gateway_bin.as_deref().ok_or_else(|| {
            Error::not_supported(
                "mac instances need the warren-gateway binary (--gateway-bin) to place in the VM",
            )
        })
    }

    fn write_share(&self, inst: &Instance, tpl: &Template) -> Result<()> {
        let env = inst.effective_env(&tpl.env);
        share::write_share(&self.share_dir(inst), self.gateway_bin()?, &env)
    }

    fn run_request(&self, inst: &Instance) -> RunRequest {
        RunRequest {
            no_display: true,
            vnc: if self.cfg.mac_vnc {
                "enabled".into()
            } else {
                "disabled".into()
            },
            shared_directories: vec![SharedDirectory {
                host_path: self.share_dir(inst).to_string_lossy().into_owned(),
                read_only: true,
            }],
        }
    }

    async fn details(&self, inst: &Instance) -> Result<VmDetails> {
        self.lume
            .get(&Self::vm_name(inst))
            .await?
            .ok_or_else(|| Error::not_found(&format!("VM of instance {}", inst.id)))
    }

    fn guest_ip(d: &VmDetails) -> Option<Ipv4Addr> {
        d.ip_address.as_deref().and_then(|s| s.parse().ok())
    }

    fn running_ip(&self, inst: &Instance, d: &VmDetails) -> Result<Ipv4Addr> {
        if !d.is_running() {
            return Err(Error::conflict(format!(
                "instance {} is not running",
                inst.id
            )));
        }
        Self::guest_ip(d).ok_or_else(|| {
            Error::internal(format!("instance {} has no guest address yet", inst.id))
        })
    }

    /// The VM's directory on disk: its storage location's path plus its name.
    async fn vm_dir(&self, d: &VmDetails) -> Result<PathBuf> {
        let settings = self.lume.settings().await?;
        let wanted = d
            .location_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| settings.default_location_name.clone());
        let loc = settings
            .vm_locations
            .iter()
            .find(|l| l.name == wanted)
            .or_else(|| settings.vm_locations.first())
            .ok_or_else(|| Error::internal("lume reports no VM storage locations"))?;
        Ok(expand_home(&loc.path).join(&d.name))
    }

    /// Wait for Lume to report the VM running with an address, then for the
    /// gateway to listen and report healthy.
    async fn wait_ready(&self, inst: &Instance, vm: &str, deadline: Instant) -> Result<Ipv4Addr> {
        let ip = loop {
            let d =
                self.lume.get(vm).await?.ok_or_else(|| {
                    Error::internal(format!("VM {vm} disappeared while starting"))
                })?;
            if d.is_running()
                && let Some(ip) = Self::guest_ip(&d)
            {
                break ip;
            }
            if Instant::now() >= deadline {
                return Err(Error::timeout(format!(
                    "VM {vm} did not report running with an address within {}s (status {:?})",
                    self.boot_timeout.as_secs(),
                    d.status
                )));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(5));
        let hint = "the gateway inside the VM did not come up; the base VM must be prepared with scripts/mac/build-base.sh so its LaunchDaemon starts the gateway from the shared folder";
        if let Err(mut e) =
            warren_gateway_client::wait_for_port(ip, self.gateway_port, remaining, || true).await
        {
            e.message = format!("{}; {hint}", e.message);
            return Err(e);
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(5));
        warren_gateway_client::wait_for_health(
            &self.http,
            ip,
            self.gateway_port,
            remaining,
            || true,
        )
        .await
        .map_err(|mut e| {
            e.message = format!("{}; {hint}", e.message);
            e
        })?;
        tracing::info!(instance = %inst.id, vm, ip = %ip, "gateway healthy");
        Ok(ip)
    }

    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Apply CPU, memory and (growth-only) disk to a stopped VM.
    async fn apply_shape(&self, vm: &str, current: &VmDetails, shape: Shape) -> Result<u32> {
        let mut req = SetRequest::default();
        if current.cpu_count != shape.cpu {
            req.cpu = Some(shape.cpu);
        }
        if current.memory_gb() != shape.memory {
            req.memory = Some(format!("{}GB", shape.memory));
        }
        let current_disk = current.disk_gb();
        let mut disk = current_disk;
        if shape.disk > current_disk {
            req.disk_size = Some(format!("{}GB", shape.disk));
            disk = shape.disk;
        }
        if req.cpu.is_some() || req.memory.is_some() || req.disk_size.is_some() {
            tracing::info!(vm, cpu = ?req.cpu, memory = ?req.memory, disk = ?req.disk_size, "resizing VM");
            self.lume.set(vm, &req).await?;
        }
        Ok(disk)
    }
}

/// `~` and `~/...` relative to the daemon user's home.
fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest.trim_start_matches('/'));
    }
    PathBuf::from(p)
}

/// Copy a file with copy-on-write when the filesystem offers it (APFS
/// `cp -c`, Linux `--reflink=auto`); a plain copy otherwise.
async fn cow_copy(src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = tokio::process::Command::new("cp");
    #[cfg(target_os = "macos")]
    cmd.arg("-c");
    #[cfg(not(target_os = "macos"))]
    cmd.arg("--reflink=auto");
    let out = cmd
        .arg(src)
        .arg(dst)
        .output()
        .await
        .map_err(|e| Error::internal(format!("cp: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    // Fall back to a plain copy (an old cp, or a filesystem without clones).
    tokio::fs::copy(src, dst).await.map(|_| ()).map_err(|e| {
        Error::internal(format!(
            "copy {} to {}: {e} (cp said: {})",
            src.display(),
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    })
}

fn file_len(p: &Path) -> i64 {
    std::fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)
}

#[async_trait]
impl Runtime for MacRuntime {
    fn name(&self) -> &'static str {
        RUNTIME_NAME
    }

    async fn ready(&self) -> Result<()> {
        let base = self.cfg.mac_base_vm.clone().ok_or_else(|| {
            Error::not_supported(
                "no base VM configured (--mac-base-vm); mac instances are disabled",
            )
        })?;
        let bin = self.gateway_bin()?;
        if !bin.is_file() {
            return Err(Error::not_supported(format!(
                "gateway binary {} does not exist",
                bin.display()
            )));
        }
        std::fs::create_dir_all(self.cfg.mac_dir())
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", self.cfg.mac_dir().display())))?;
        share::restrict(&self.cfg.mac_dir())?;
        serve::ensure_running(&self.cfg, &self.lume).await?;
        let host = self.lume.host_status().await?;
        let capacity = self.lume.capacity().await?;
        if lume::parse_version(&host.version) < MIN_LUME_VERSION {
            return Err(Error::not_supported(format!(
                "lume {} is too old; 0.7.2 or newer is required",
                host.version
            )));
        }
        let vm = self.lume.get(&base).await?.ok_or_else(|| {
            Error::not_supported(format!(
                "base VM {base:?} does not exist; create it with scripts/mac/build-base.sh"
            ))
        })?;
        if !vm.is_macos() {
            return Err(Error::not_supported(format!(
                "base VM {base:?} is a {} guest, not macOS",
                vm.os
            )));
        }
        if vm.is_running() {
            tracing::warn!(
                base,
                "the base VM is running; stop it so clones start from a quiescent disk"
            );
        }
        tracing::info!(
            lume = %host.version,
            api = %self.cfg.lume_api_url,
            base,
            disk_gb = vm.disk_gb(),
            running = capacity.running,
            max_running = capacity.max_running,
            "lume runtime ready"
        );
        Ok(())
    }

    async fn ensure_image(&self, image_ref: &str) -> Result<String> {
        let vm = self.lume.get(image_ref).await?.ok_or_else(|| {
            Error::not_found(&format!("lume VM {image_ref} (the template's base VM)"))
        })?;
        if !vm.is_macos() {
            return Err(Error::invalid(format!(
                "VM {image_ref} is a {} guest; mac templates need a macOS VM",
                vm.os
            )));
        }
        let mut h = Sha256::new();
        h.update(vm.name.as_bytes());
        h.update(b"\n");
        h.update(vm.machine_identifier.as_deref().unwrap_or("").as_bytes());
        h.update(b"\n");
        h.update(vm.disk_size.total.to_le_bytes());
        Ok(format!("sha256:{}", hex::encode(h.finalize())))
    }

    async fn create(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        let vm = Self::vm_name(inst);
        let existing = self.lume.get(&vm).await?;
        let details = match existing {
            Some(d) => {
                tracing::info!(instance = %inst.id, vm, "adopting existing VM");
                d
            }
            None => {
                tracing::info!(instance = %inst.id, vm, base = %tpl.image_ref, "cloning base VM");
                self.lume.clone_vm(&tpl.image_ref, &vm).await?;
                self.details(inst).await?
            }
        };
        if details.is_running() {
            return Err(Error::conflict(format!(
                "VM {vm} is running; stop it before recreating the instance"
            )));
        }
        let disk = self.apply_shape(&vm, &details, inst.shape).await?;
        inst.shape.disk = disk;
        self.write_share(inst, tpl)?;
        inst.storage_path = self.share_dir(inst).to_string_lossy().into_owned();
        inst.runtime_ref = vm;
        inst.runtime = RUNTIME_NAME.to_string();
        Ok(())
    }

    async fn start(&self, inst: &mut Instance) -> Result<RunInfo> {
        let vm = Self::vm_name(inst);
        let details = self.lume.get(&vm).await?.ok_or_else(|| {
            Error::conflict(format!("instance {} has no VM; create it first", inst.id))
        })?;
        let deadline = Instant::now() + self.boot_timeout;
        if details.is_running() {
            tracing::info!(instance = %inst.id, vm, "VM already running; adopting it");
        } else {
            let capacity = self.lume.capacity().await?;
            if !capacity.has_room() {
                return Err(Error::host_capacity(capacity.max_running, capacity.running));
            }
            if !self.share_dir(inst).join(share::ENV_FILE).is_file() {
                return Err(Error::conflict(format!(
                    "instance {} has no shared folder; create it first",
                    inst.id
                )));
            }
            self.lume.run(&vm, &self.run_request(inst)).await?;
            tracing::info!(
                instance = %inst.id,
                vm,
                resuming = details.has_saved_state(),
                "lume accepted the run; waiting for the VM"
            );
        }
        let ip = match self.wait_ready(inst, &vm, deadline).await {
            Ok(ip) => ip,
            Err(e) => {
                // Do not leave a half-started guest holding one of the two slots.
                if let Err(stop) = self.lume.stop(&vm, true, 0).await {
                    tracing::warn!(instance = %inst.id, error = %stop, "could not stop the VM after a failed start");
                }
                return Err(e);
            }
        };
        inst.ip = ip.to_string();
        inst.runtime_ref = vm.clone();
        inst.runtime = RUNTIME_NAME.to_string();
        Ok(RunInfo {
            ip: inst.ip.clone(),
            runtime_ref: vm,
        })
    }

    async fn stop(&self, inst: &Instance, grace: Duration) -> Result<()> {
        let vm = Self::vm_name(inst);
        match self.lume.get(&vm).await? {
            Some(d) if d.is_running() => {
                tracing::info!(instance = %inst.id, vm, grace = grace.as_secs(), "stopping VM");
                self.lume.stop(&vm, false, grace.as_secs()).await
            }
            _ => Ok(()),
        }
    }

    async fn sleep(&self, inst: &Instance, grace: Duration) -> Result<()> {
        let vm = Self::vm_name(inst);
        match self.lume.get(&vm).await? {
            Some(d) if d.is_running() => {
                tracing::info!(instance = %inst.id, vm, "suspending VM");
                match self.lume.suspend(&vm).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        tracing::warn!(instance = %inst.id, error = %e, "suspend failed; stopping instead");
                        self.lume.stop(&vm, false, grace.as_secs()).await
                    }
                }
            }
            _ => Ok(()),
        }
    }

    async fn destroy(&self, inst: &Instance, remove_storage: bool) -> Result<()> {
        let vm = Self::vm_name(inst);
        if let Some(d) = self.lume.get(&vm).await?
            && d.is_running()
        {
            self.lume.stop(&vm, false, STOP_GRACE_SECS).await?;
        }
        if remove_storage {
            self.lume.delete(&vm).await?;
            share::remove_instance_dir(&self.cfg.mac_dir(), &inst.id)?;
        }
        Ok(())
    }

    async fn exec(&self, inst: &Instance, req: &ExecRequest) -> Result<ExecResult> {
        let d = self.details(inst).await?;
        let ip = self.running_ip(inst, &d)?;
        warren_gateway_client::exec_at(&self.http, ip, self.gateway_port, &inst.instance_token, req)
            .await
    }

    async fn logs(&self, inst: &Instance, opts: LogsOptions) -> Result<String> {
        let d = self.details(inst).await?;
        if !d.is_running() {
            return Ok(String::new());
        }
        let ip = self.running_ip(inst, &d)?;
        let tail = if opts.tail == 0 {
            200
        } else {
            opts.tail.min(10_000)
        };
        let log = format!(
            "{}/Library/Logs/warren-gateway.log",
            inst.mount_path.trim_end_matches('/')
        );
        let req = ExecRequest {
            command: format!(
                "tail -n {tail} {} 2>/dev/null || true",
                share::shell_quote(&log)
            ),
            timeout_seconds: 20,
            ..Default::default()
        };
        let out = warren_gateway_client::exec_at(
            &self.http,
            ip,
            self.gateway_port,
            &inst.instance_token,
            &req,
        )
        .await?;
        Ok(filter_since(&out.stdout, opts.since))
    }

    async fn stats(&self, inst: &Instance) -> Result<Stats> {
        let d = self.details(inst).await?;
        let disk_bytes = d.disk_size.allocated as i64;
        let limit = i64::from(inst.shape.memory).saturating_mul(GIB as i64);
        if !d.is_running() {
            return Ok(Stats {
                at: Self::now_unix(),
                cpu_percent: 0.0,
                memory_bytes: 0,
                memory_limit_bytes: limit,
                disk_bytes,
            });
        }
        let ip = self.running_ip(inst, &d)?;
        match warren_gateway_client::stats_at(
            &self.http,
            ip,
            self.gateway_port,
            &inst.instance_token,
        )
        .await
        {
            Ok(mut s) => {
                s.at = Self::now_unix();
                s.memory_limit_bytes = limit;
                s.disk_bytes = disk_bytes;
                Ok(s)
            }
            Err(e) => {
                tracing::debug!(instance = %inst.id, error = %e, "gateway stats unavailable");
                Ok(Stats {
                    at: Self::now_unix(),
                    cpu_percent: 0.0,
                    memory_bytes: 0,
                    memory_limit_bytes: limit,
                    disk_bytes,
                })
            }
        }
    }

    async fn is_running(&self, inst: &Instance) -> Result<bool> {
        Ok(self
            .lume
            .get(&Self::vm_name(inst))
            .await?
            .is_some_and(|d| d.is_running()))
    }

    async fn resize(&self, inst: &mut Instance, shape: Shape) -> Result<()> {
        let vm = Self::vm_name(inst);
        let d = self.details(inst).await?;
        if d.is_running() {
            return Err(errors::requires_restart());
        }
        if shape.disk < d.disk_gb() {
            return Err(Error::invalid(format!(
                "disk can only grow on mac instances; the VM already has {} GB",
                d.disk_gb()
            )));
        }
        let disk = self.apply_shape(&vm, &d, shape).await?;
        inst.shape = Shape { disk, ..shape };
        Ok(())
    }

    async fn recreate(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        if tpl.image_ref != inst.image_ref {
            return Err(Error::not_supported(
                "a mac instance cannot move to a template with a different base VM: its disk is its home",
            ));
        }
        let d = self.details(inst).await?;
        if d.is_running() {
            return Err(Error::conflict(format!(
                "VM {} is running; stop it before updating",
                d.name
            )));
        }
        self.write_share(inst, tpl)?;
        inst.storage_path = self.share_dir(inst).to_string_lossy().into_owned();
        inst.runtime = RUNTIME_NAME.to_string();
        Ok(())
    }

    async fn clone_storage(&self, src: &Instance, dst: &mut Instance) -> Result<()> {
        let from = Self::vm_name(src);
        let to = Self::vm_name(dst);
        let d = self
            .lume
            .get(&from)
            .await?
            .ok_or_else(|| Error::conflict(format!("instance {} has no VM to copy", src.id)))?;
        if d.is_running() {
            return Err(Error::conflict(format!(
                "instance {} must be stopped or sleeping before it is copied",
                src.id
            )));
        }
        if d.has_saved_state() {
            tracing::info!(instance = %src.id, "source is suspended; the copy takes its disk as of the suspend and boots fresh");
        }
        self.lume.clone_vm(&from, &to).await?;
        dst.storage_path = self.share_dir(dst).to_string_lossy().into_owned();
        dst.runtime_ref = to;
        dst.runtime = RUNTIME_NAME.to_string();
        Ok(())
    }

    async fn backup_storage(&self, inst: &Instance, dest_path: &Path) -> Result<i64> {
        let d = self.details(inst).await?;
        if d.is_running() {
            return Err(Error::conflict(format!(
                "instance {} must be stopped or sleeping to back up",
                inst.id
            )));
        }
        let dir = self.vm_dir(&d).await?;
        tokio::fs::create_dir_all(dest_path).await?;
        for f in [DISK_FILE, NVRAM_FILE, CONFIG_FILE] {
            let src = dir.join(f);
            if !src.is_file() {
                return Err(Error::internal(format!(
                    "VM {} has no {f} in {}",
                    d.name,
                    dir.display()
                )));
            }
            cow_copy(&src, &dest_path.join(f)).await?;
        }
        // The disk is a copy-on-write clone that shares its blocks with the
        // VM until either side writes; only the small files cost space now.
        Ok(file_len(&dest_path.join(NVRAM_FILE)) + file_len(&dest_path.join(CONFIG_FILE)))
    }

    async fn restore_storage(&self, inst: &Instance, src_path: &Path) -> Result<()> {
        let d = self.details(inst).await?;
        if d.is_running() {
            return Err(Error::conflict(format!(
                "instance {} must be stopped to restore",
                inst.id
            )));
        }
        if !src_path.join(DISK_FILE).is_file() {
            return Err(Error::not_found("backup disk image"));
        }
        let dir = self.vm_dir(&d).await?;
        // Saved memory would not match the restored disk.
        for f in SAVED_STATE_FILES {
            match tokio::fs::remove_file(dir.join(f)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::internal(format!("remove {f}: {e}"))),
            }
        }
        for f in [DISK_FILE, NVRAM_FILE] {
            let src = src_path.join(f);
            if src.is_file() {
                let tmp = dir.join(format!(".{f}.restore"));
                cow_copy(&src, &tmp).await?;
                tokio::fs::rename(&tmp, dir.join(f)).await?;
            }
        }
        Ok(())
    }

    async fn disk_usage(&self, inst: &Instance) -> Result<i64> {
        Ok(self.details(inst).await?.disk_size.allocated as i64)
    }
}

/// Keep the lines whose leading RFC 3339 timestamp is at or after `since`
/// (unix seconds); lines without a timestamp stay. `since == 0` keeps all.
pub fn filter_since(text: &str, since: i64) -> String {
    if since <= 0 {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        if let Some(ts) = line_timestamp(line)
            && ts < since
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Unix seconds of a `2026-11-19T05:41:14.130943Z` prefix, if present.
fn line_timestamp(line: &str) -> Option<i64> {
    let b = line.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (y, mo, d) = (num(&line[0..4])?, num(&line[5..7])?, num(&line[8..10])?);
    let (h, mi, s) = (
        num(&line[11..13])?,
        num(&line[14..16])?,
        num(&line[17..19])?,
    );
    // Days since the epoch (proleptic Gregorian), then seconds.
    let (y2, m2) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}
