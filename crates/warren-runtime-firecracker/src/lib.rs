//! VM-type instances as Firecracker microVMs built from the template image.
//!
//! The template image is exported with the Docker CLI into a directory, the
//! instance environment is written to `/etc/warren.env`, and `mkfs.ext4 -d`
//! packs the tree into a sparse `rootfs.ext4` sized to the shape's disk. The VM
//! boots the configured kernel with the gateway as PID 1 (`--init`); its single
//! NIC is a tap on the `warren-fc0` bridge (172.30.0.0/24, NAT to the host's
//! uplink) and the guest address is fixed through the kernel `ip=` parameter, so
//! no DHCP runs anywhere. The daemon talks to the gateway by IP for exec and
//! shutdown. Nothing is ever mounted on the host: backups, rebuilds and clones
//! read and edit images with `debugfs`.
//!
//! System templates ship the gateway in their image. Workspace templates are
//! registered from arbitrary images that have none (and often no long-running
//! process), so before packing, the fully static gateway the daemon was given
//! (`--gateway-bin`) is copied to `/usr/local/bin/warren-gateway` and the
//! image's own `Entrypoint + Cmd` is recorded in `/etc/warren.wrap.json`
//! ([`WRAP_FILE`]) for the gateway to supervise while it serves the API.
//!
//! Sleep keeps the memory: the guest is paused and Firecracker writes a full
//! snapshot (`vm/snapshot/{vmstate,mem}`, the memory file as large as the
//! guest's RAM) before the process is killed and the tap removed; the next
//! `start` recreates the tap, starts a fresh process and loads the snapshot,
//! so every process in the guest continues with its pid. A snapshot is used
//! once; `stop` discards it; a missing, corrupt, resized or
//! other-Firecracker-version snapshot falls back to a cold boot with a
//! warning ([`snapshot`]). Firecracker is driven over its API socket
//! ([`api`]); with `--firecracker-jailer` it runs inside the jailer's chroot
//! as an unprivileged uid ([`jail`]); each shape gets token-bucket limits on
//! its disk and NIC unless `--firecracker-no-rate-limits` ([`limits`]).
//!
//! Layout under `<instances_dir>/<id>/vm/`: `rootfs.ext4`, `config.json`, `ip`,
//! `pid`, `console.log`, `firecracker.sock` while running outside the jailer,
//! `snapshot/` while asleep with memory.
//!
//! Needs root and `/dev/kvm`; [`Runtime::ready`] says exactly what is missing.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod api;
mod host;
mod image;
mod inject;
mod jail;
mod limits;
mod net;
mod paths;
mod process;
mod rootfs;
mod snapshot;
#[cfg(test)]
mod testutil;
mod vmconfig;

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use warren_core::{
    Config, Error, ExecRequest, ExecResult, GATEWAY_PORT, Instance, LogsOptions, NetworkMode,
    Result, RunInfo, Runtime, STOP_GRACE_SECS, Shape, Stats, Template, TemplateScope, errors,
};

pub use inject::WRAP_FILE;
pub use jail::{API_SOCK as JAIL_API_SOCK, JAIL_SUBDIR, JailConfig};
pub use limits::{RateLimits, for_shape as rate_limits_for_shape};
pub use net::{BRIDGE, HOST_IP, SUBNET, tap_name};
pub use paths::VmPaths;
pub use snapshot::{Assessment as SnapshotAssessment, Meta as SnapshotMeta};
pub use vmconfig::{GUEST_INIT, VmConfig, boot_args, boot_args_with_init};

use paths::VmPaths as Paths;

/// Value of `Instance::runtime` for VMs created here.
pub const RUNTIME_NAME: &str = "firecracker";
/// How long `start` waits for the gateway port after launching the VM.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
/// How much of the console log `logs` reads from the end.
const CONSOLE_TAIL_BYTES: u64 = 4 << 20;
/// Time allowed for a SIGKILLed VM to disappear.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// Time allowed for `sync` inside the guest before a snapshot.
const GUEST_SYNC_TIMEOUT_SECS: u32 = 30;

/// How a root filesystem gets the gateway that serves the Warren API. The
/// daemon decides through the template's scope; see [`inject`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum GatewayPlan {
    /// A system image: it ships `/usr/local/bin/warren-gateway` as its entrypoint.
    ImageProvides,
    /// A workspace image: copy the daemon's static gateway in unless the image
    /// has one, and record the image's own command in the wrap file.
    Inject {
        /// `Config.Entrypoint` followed by `Config.Cmd` of the image.
        command: Vec<String>,
    },
}

/// What one firecracker process is told and where the host finds the same
/// things: directly in the instance's `vm` directory, or through the
/// jailer's chroot where every path is jail-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    jail: Option<jail::Jail>,
    /// Host path of the API socket.
    api_sock: PathBuf,
    /// `--api-sock` as firecracker is told.
    fc_api_sock: PathBuf,
    /// `--config-file` as firecracker is told.
    fc_config: PathBuf,
    /// Snapshot files as firecracker is told.
    fc_vmstate: PathBuf,
    fc_mem: PathBuf,
    /// Where on the host the files firecracker writes a snapshot to appear.
    host_snapshot_dir: PathBuf,
}

/// Firecracker-backed [`Runtime`].
pub struct FirecrackerRuntime {
    cfg: Config,
    http: reqwest::Client,
    guest_init: String,
    /// Serialises guest ip allocation.
    alloc: tokio::sync::Mutex<()>,
    /// Last CPU sample per instance id, for `stats`.
    cpu: Mutex<HashMap<String, process::CpuSample>>,
}

impl FirecrackerRuntime {
    /// Build the runtime. Nothing privileged happens here; `ready` checks the
    /// host and brings the bridge network up.
    pub async fn new(cfg: Config) -> Result<FirecrackerRuntime> {
        // No keep-alive reuse: a connection opened before a sleep points at a
        // guest that has since been snapshotted, killed and restored from its
        // memory, and the first request on it after the wake fails. A fresh
        // connection per request costs one handshake on the local bridge.
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| Error::internal(format!("http client: {e}")))?;
        Ok(FirecrackerRuntime {
            cfg,
            http,
            guest_init: GUEST_INIT.to_string(),
            alloc: tokio::sync::Mutex::new(()),
            cpu: Mutex::new(HashMap::new()),
        })
    }

    /// Boot a different program as PID 1 instead of the gateway. Meant for
    /// images that ship their own init and for tests; the daemon never sets it.
    pub fn with_guest_init(mut self, init: impl Into<String>) -> FirecrackerRuntime {
        self.guest_init = init.into();
        self
    }

    /// The configuration this runtime was built with.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Storage layout of an instance.
    pub fn paths(&self, inst: &Instance) -> VmPaths {
        Paths::for_instance(&self.cfg.instances_dir(), inst)
    }

    fn kernel(&self) -> Result<PathBuf> {
        self.cfg.kernel_image.clone().ok_or_else(|| {
            Error::not_supported("the firecracker runtime has no kernel image configured")
        })
    }

    /// The jailer configuration, when the daemon launches microVMs through it.
    pub fn jail_config(&self) -> Option<JailConfig> {
        self.cfg
            .firecracker_jailer
            .as_ref()
            .map(|jailer| JailConfig {
                jailer: jailer.clone(),
                firecracker: self.cfg.firecracker_bin.clone(),
                base: self.cfg.firecracker_jail_dir(),
                uid: self.cfg.firecracker_jail_uid,
                gid: self.cfg.firecracker_jail_gid,
            })
    }

    fn jail_for(&self, inst: &Instance) -> Option<jail::Jail> {
        self.jail_config().map(|c| jail::Jail::new(c, &inst.id))
    }

    /// Refuse a jailer setup that cannot work: a missing binary, uid or gid
    /// 0 (the jail would be pointless), or a base directory on another
    /// filesystem than the instances (the jail hard-links their files).
    /// Creates the base directory.
    fn check_jail_config(&self) -> Result<()> {
        let Some(jc) = self.jail_config() else {
            return Ok(());
        };
        if !host::is_executable(&jc.jailer) {
            return Err(Error::not_supported(format!(
                "jailer binary {} is missing or not executable",
                jc.jailer.display()
            )));
        }
        if jc.uid == 0 || jc.gid == 0 {
            return Err(Error::not_supported(
                "--firecracker-jail-uid and --firecracker-jail-gid must not be 0",
            ));
        }
        let instances = self.cfg.instances_dir();
        for dir in [&instances, &jc.base] {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::internal(format!("mkdir {}: {e}", dir.display())))?;
        }
        if !jail::same_filesystem(&jc.base, &instances) {
            return Err(Error::not_supported(format!(
                "the jail directory {} and the instances directory {} are on different filesystems; the jailer hard-links instance files into the chroot",
                jc.base.display(),
                instances.display()
            )));
        }
        Ok(())
    }

    /// The string that identifies a VM's firecracker process on its command
    /// line: the instance's `vm` directory (part of `--config-file` and
    /// `--api-sock`), or the instance id the jailer passes as `--id`.
    fn marker(&self, inst: &Instance, p: &Paths) -> String {
        match self.jail_for(inst) {
            Some(j) => j.marker(),
            None => p.vm.to_string_lossy().into_owned(),
        }
    }

    /// Pid of the live firecracker process for this VM, if any.
    fn running_pid(&self, inst: &Instance, p: &Paths) -> Result<Option<i32>> {
        match process::read_pid(&p.pid())? {
            Some(pid) if process::is_alive(pid, &self.marker(inst, p)) => Ok(Some(pid)),
            _ => Ok(None),
        }
    }

    fn recorded_ip(p: &Paths) -> Option<Ipv4Addr> {
        std::fs::read_to_string(p.ip())
            .ok()
            .and_then(|t| net::parse_ip_file(&t))
    }

    /// The guest address from `vm/ip`, allocating and recording one when missing.
    async fn ensure_ip(&self, p: &Paths) -> Result<Ipv4Addr> {
        if let Some(ip) = Self::recorded_ip(p) {
            return Ok(ip);
        }
        let _guard = self.alloc.lock().await;
        if let Some(ip) = Self::recorded_ip(p) {
            return Ok(ip);
        }
        let ip = net::allocate(&self.cfg.instances_dir())?;
        std::fs::create_dir_all(&p.vm)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", p.vm.display())))?;
        std::fs::write(p.ip(), format!("{ip}\n"))
            .map_err(|e| Error::internal(format!("write {}: {e}", p.ip().display())))?;
        tracing::debug!(ip = %ip, dir = %p.root.display(), "allocated guest ip");
        Ok(ip)
    }

    /// The address to talk to: the instance record first, then `vm/ip`.
    fn guest_ip(inst: &Instance, p: &Paths) -> Option<Ipv4Addr> {
        inst.ip
            .parse::<Ipv4Addr>()
            .ok()
            .or_else(|| Self::recorded_ip(p))
    }

    /// The rate limits for a shape, unless the daemon turned them off.
    fn rate_limits(&self, shape: Shape) -> Option<RateLimits> {
        self.cfg
            .firecracker_rate_limits
            .then(|| limits::for_shape(shape))
    }

    /// Render the VM configuration with `kernel` and `rootfs` as firecracker
    /// should name them (host paths, or jail-relative ones).
    fn render_config(
        &self,
        inst: &Instance,
        kernel: &Path,
        rootfs: &Path,
        ip: Ipv4Addr,
    ) -> VmConfig {
        let limits = self.rate_limits(inst.shape);
        let mut cfg = vmconfig::render(
            kernel,
            rootfs,
            &net::tap_name(&inst.id),
            ip,
            inst.shape,
            limits.as_ref(),
        );
        cfg.boot_source.boot_args = boot_args_with_init(ip, &self.guest_init);
        cfg
    }

    /// Render and write `config.json` (host paths) from the instance's current shape.
    fn write_config(&self, inst: &Instance, p: &Paths, ip: Ipv4Addr) -> Result<VmConfig> {
        let kernel = self.kernel()?;
        let cfg = self.render_config(inst, &kernel, &p.rootfs(), ip);
        vmconfig::write(&p.config(), &cfg)?;
        Ok(cfg)
    }

    /// The persistent directory of an instance, or `None` when the template
    /// mounts nothing (`/` or empty), in which case nothing is preserved.
    fn home_dir(inst: &Instance) -> Option<&str> {
        let m = inst.mount_path.trim();
        (!m.trim_matches('/').is_empty()).then_some(m)
    }

    /// Workspace templates are registered from arbitrary images and get the
    /// daemon's gateway injected; system templates are built with it.
    fn needs_injection(tpl: &Template) -> bool {
        tpl.scope == TemplateScope::Workspace
    }

    /// Turn the daemon's decision into a plan. Injection needs the image's own
    /// command, read with `docker image inspect`.
    async fn plan_gateway(image: &str, inject: bool) -> Result<GatewayPlan> {
        if !inject {
            return Ok(GatewayPlan::ImageProvides);
        }
        let command = image::inspect_command(image).await?;
        Ok(GatewayPlan::Inject { command })
    }

    /// Everything the exported tree at `dir` needs before it is packed: the
    /// gateway and wrap file as `plan` says, `preserved_home` moved to the
    /// mount path, then the identity files and the env.
    fn stage_rootfs_dir(
        &self,
        dir: &Path,
        inst: &Instance,
        env: &BTreeMap<String, String>,
        preserved_home: Option<PathBuf>,
        plan: &GatewayPlan,
    ) -> Result<()> {
        match plan {
            GatewayPlan::ImageProvides => {
                if self.guest_init == GUEST_INIT
                    && !rootfs::guest_to_host(dir, GUEST_INIT).is_file()
                {
                    tracing::warn!(instance = %inst.id, init = GUEST_INIT, "system image has no gateway at the init path; the VM cannot serve the API");
                }
            }
            GatewayPlan::Inject { command } => {
                let outcome = inject::ensure_gateway(dir, self.cfg.gateway_bin.as_deref())?;
                tracing::info!(instance = %inst.id, gateway = ?outcome, command = ?command, "workspace image prepared for the gateway");
                inject::write_wrap_file(dir, command)?;
            }
        }
        if let (Some(home), Some(mount)) = (preserved_home, Self::home_dir(inst)) {
            let target = rootfs::guest_to_host(dir, mount);
            rootfs::remove_tree(&target)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
            }
            std::fs::rename(&home, &target).map_err(|e| {
                Error::internal(format!(
                    "move {} to {}: {e}",
                    home.display(),
                    target.display()
                ))
            })?;
        }
        rootfs::write_identity_files(dir, &inst.id, &inst.mount_path)?;
        rootfs::write_env_file(dir, env)?;
        Ok(())
    }

    /// Export `image`, stage the tree (gateway, preserved home, identity files,
    /// env) and pack everything into a fresh `rootfs.ext4`. The old image stays
    /// in place until the new one is complete. `inject_gateway` is the daemon's
    /// decision for this image: true for workspace templates.
    async fn build_rootfs(
        &self,
        inst: &Instance,
        p: &Paths,
        image: &str,
        env: &BTreeMap<String, String>,
        preserved_home: Option<PathBuf>,
        inject_gateway: bool,
    ) -> Result<()> {
        if image.is_empty() {
            return Err(Error::invalid(
                "the instance has no image to build a root filesystem from",
            ));
        }
        std::fs::create_dir_all(&p.vm)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", p.vm.display())))?;
        let dir = p.rootfs_dir();
        let tmp = p.rootfs_tmp();
        rootfs::remove_tree(&dir)?;
        let _ = std::fs::remove_file(&tmp);
        let result = async {
            let plan = Self::plan_gateway(image, inject_gateway).await?;
            tracing::info!(instance = %inst.id, image, "exporting image");
            image::export_rootfs(image, &dir).await?;
            self.stage_rootfs_dir(&dir, inst, env, preserved_home, &plan)?;
            tracing::info!(instance = %inst.id, disk_gb = inst.shape.disk, "packing root filesystem");
            rootfs::mkfs(&dir, &tmp, inst.shape.disk).await?;
            std::fs::rename(&tmp, p.rootfs()).map_err(|e| Error::internal(format!("install rootfs: {e}")))?;
            Ok(())
        }
        .await;
        let _ = rootfs::remove_tree(&dir);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// SIGKILL the process and wait for it to vanish.
    async fn terminate(&self, pid: i32, marker: &str) {
        if let Err(e) = process::signal(pid, nix::sys::signal::Signal::SIGKILL) {
            tracing::warn!(pid, error = %e.describe(), "cannot kill firecracker");
        }
        if !process::wait_exit(pid, marker, KILL_WAIT).await {
            tracing::error!(pid, "firecracker survived SIGKILL");
        }
    }

    /// Kill a VM that did not stop or start as it should.
    async fn kill_vm(&self, p: &Paths, pid: i32, marker: &str) {
        tracing::warn!(pid, dir = %p.root.display(), "killing firecracker");
        self.terminate(pid, marker).await;
    }

    /// Delete the tap and any per-guest egress rule. Unprivileged daemons never
    /// created either, so there is nothing to do for them.
    async fn teardown_network(&self, inst: &Instance, ip: Option<Ipv4Addr>) -> Result<()> {
        host::tap_delete(&net::tap_name(&inst.id)).await?;
        if host::is_root()
            && let Some(ip) = ip
        {
            host::set_egress(ip, true).await?;
        }
        Ok(())
    }

    /// Everything a stopped VM leaves on the host: the network, the jail and
    /// the API socket file.
    async fn teardown(&self, inst: &Instance, ip: Option<Ipv4Addr>) -> Result<()> {
        let net = self.teardown_network(inst, ip).await;
        if let Some(j) = self.jail_for(inst)
            && let Err(e) = j.remove()
        {
            tracing::warn!(instance = %inst.id, error = %e.describe(), "cannot remove the jail directory");
        }
        let _ = std::fs::remove_file(self.paths(inst).api_sock());
        net
    }

    /// Bridge, tap (owned by the jail user when jailed) and the egress rule
    /// for a boot or a restore. Returns the tap name.
    async fn bring_up_network(&self, inst: &Instance, ip: Ipv4Addr) -> Result<String> {
        host::ensure_network(self.cfg.allow_private_egress, self.broker_port()).await?;
        let tap = net::tap_name(&inst.id);
        let owner = self.jail_config().map(|c| (c.uid, c.gid));
        host::tap_create_owned(&tap, owner).await?;
        if let Err(e) = host::set_egress(ip, inst.network == NetworkMode::AllowAll).await {
            let _ = host::tap_delete(&tap).await;
            return Err(e);
        }
        Ok(tap)
    }

    /// Where this VM's process finds its files and socket.
    fn launch(&self, inst: &Instance, p: &Paths) -> Launch {
        match self.jail_for(inst) {
            Some(j) => Launch {
                api_sock: j.api_sock(),
                fc_api_sock: PathBuf::from(jail::API_SOCK),
                fc_config: PathBuf::from(jail::CONFIG),
                fc_vmstate: Path::new(jail::SNAPSHOT_DIR).join(snapshot::VMSTATE),
                fc_mem: Path::new(jail::SNAPSHOT_DIR).join(snapshot::MEM),
                host_snapshot_dir: j.snapshot_dir(),
                jail: Some(j),
            },
            None => Launch {
                jail: None,
                api_sock: p.api_sock(),
                fc_api_sock: p.api_sock(),
                fc_config: p.config(),
                fc_vmstate: p.snapshot_vmstate(),
                fc_mem: p.snapshot_mem(),
                host_snapshot_dir: p.snapshot_dir(),
            },
        }
    }

    /// The program to spawn and its arguments: `firecracker --api-sock ...`
    /// with `--config-file` for a boot (a restore configures itself through
    /// `PUT /snapshot/load`), wrapped in the jailer's command line when
    /// jailed.
    fn command(&self, l: &Launch, boot: bool) -> (PathBuf, Vec<OsString>) {
        let mut fc_args: Vec<OsString> =
            vec!["--api-sock".into(), l.fc_api_sock.clone().into_os_string()];
        if boot {
            fc_args.push("--config-file".into());
            fc_args.push(l.fc_config.clone().into_os_string());
        }
        match &l.jail {
            Some(j) => j.command(&fc_args),
            None => (self.cfg.firecracker_bin.clone(), fc_args),
        }
    }

    /// Put in place what the process needs: outside the jailer only a clean
    /// socket path; inside it the chroot with the kernel, the root
    /// filesystem, a jail-relative config and, for a restore, the snapshot
    /// files.
    fn prepare_launch(
        &self,
        inst: &Instance,
        p: &Paths,
        l: &Launch,
        ip: Ipv4Addr,
        restore: bool,
    ) -> Result<()> {
        if let Err(e) = std::fs::remove_file(&l.api_sock)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Error::internal(format!(
                "remove stale socket {}: {e}",
                l.api_sock.display()
            )));
        }
        if l.jail.is_none() && l.fc_api_sock.as_os_str().len() > api::MAX_DIRECT_SOCKET_PATH {
            // Firecracker binds this path itself and cannot take a long one;
            // the jailer binds a short jail-relative path instead.
            return Err(Error::invalid(format!(
                "the API socket path {} is {} bytes, more than a Unix socket allows; use a shorter --data-dir or launch through the jailer (--firecracker-jailer)",
                l.fc_api_sock.display(),
                l.fc_api_sock.as_os_str().len()
            )));
        }
        if let Some(j) = &l.jail {
            let cfg =
                self.render_config(inst, Path::new(jail::KERNEL), Path::new(jail::ROOTFS), ip);
            let json = vmconfig::to_json(&cfg)?;
            let (vmstate, mem) = (p.snapshot_vmstate(), p.snapshot_mem());
            let snap = restore.then_some((vmstate.as_path(), mem.as_path()));
            j.prepare(&self.kernel()?, &p.rootfs(), &json, snap)?;
        }
        Ok(())
    }

    /// Spawn the process with the console redirected to the log.
    async fn spawn_vm(&self, l: &Launch, p: &Paths, boot: bool) -> Result<i32> {
        let (program, args) = self.command(l, boot);
        process::spawn(&program, &args, &p.console()).await
    }

    /// A start that did not reach the gateway: kill the process, drop the
    /// network and the jail, and attach the console tail to the error.
    async fn abandon_start(
        &self,
        inst: &Instance,
        p: &Paths,
        pid: i32,
        marker: &str,
        ip: Ipv4Addr,
        mut e: Error,
    ) -> Error {
        let console = process::tail(&p.console(), 40, 64 << 10).unwrap_or_default();
        self.kill_vm(p, pid, marker).await;
        process::clear_pid(&p.pid());
        let _ = self.teardown(inst, Some(ip)).await;
        let mut details = e.details.take().unwrap_or_else(|| serde_json::json!({}));
        details["console_tail"] = serde_json::Value::String(console);
        e.details = Some(details);
        e
    }

    /// Cold boot: network, launch files, the process with its config, then
    /// the gateway port.
    async fn boot_vm(&self, inst: &Instance, p: &Paths, ip: Ipv4Addr) -> Result<()> {
        let l = self.launch(inst, p);
        let marker = self.marker(inst, p);
        let tap = self.bring_up_network(inst, ip).await?;
        if let Err(e) = self.prepare_launch(inst, p, &l, ip, false) {
            let _ = self.teardown(inst, Some(ip)).await;
            return Err(e);
        }
        Self::append_console_marker(p, "boot");
        let pid = match self.spawn_vm(&l, p, true).await {
            Ok(pid) => pid,
            Err(e) => {
                let _ = self.teardown(inst, Some(ip)).await;
                return Err(e);
            }
        };
        process::write_pid(&p.pid(), pid)?;
        tracing::info!(instance = %inst.id, pid, ip = %ip, tap, jailed = l.jail.is_some(), "firecracker started; waiting for the gateway");
        if let Err(e) = warren_gateway_client::wait_for_port(ip, GATEWAY_PORT, BOOT_TIMEOUT, || {
            process::is_alive(pid, &marker)
        })
        .await
        {
            return Err(self.abandon_start(inst, p, pid, &marker, ip, e).await);
        }
        self.forget_cpu(&inst.id);
        Ok(())
    }

    /// Wake from the saved memory: the same tap as before, a fresh process
    /// without a config, `PUT /snapshot/load` with `resume_vm`, then the
    /// gateway port. On success the snapshot files are removed: a snapshot is
    /// single-use (the restored process keeps the memory file mapped until it
    /// exits, so the space is released then). On failure the files stay for a
    /// retry unless Firecracker itself rejected them, which the caller reads
    /// from [`api::is_fault`].
    async fn restore_vm(
        &self,
        inst: &Instance,
        p: &Paths,
        ip: Ipv4Addr,
        meta: &snapshot::Meta,
    ) -> Result<()> {
        let started = Instant::now();
        let l = self.launch(inst, p);
        let marker = self.marker(inst, p);
        let tap = self.bring_up_network(inst, ip).await?;
        if let Err(e) = self.prepare_launch(inst, p, &l, ip, true) {
            let _ = self.teardown(inst, Some(ip)).await;
            return Err(e);
        }
        Self::append_console_marker(p, "resume");
        let pid = match self.spawn_vm(&l, p, false).await {
            Ok(pid) => pid,
            Err(e) => {
                let _ = self.teardown(inst, Some(ip)).await;
                return Err(e);
            }
        };
        process::write_pid(&p.pid(), pid)?;
        tracing::info!(
            instance = %inst.id, pid, ip = %ip, tap, jailed = l.jail.is_some(),
            firecracker = %meta.firecracker_version, slept_at = meta.created_at, mem_mib = meta.mem_size_mib,
            "firecracker started; loading the snapshot"
        );
        let fc = api::Api::new(&l.api_sock);
        let result = async {
            fc.wait_ready(api::SOCKET_TIMEOUT, || process::is_alive(pid, &marker))
                .await?;
            fc.load_snapshot(&l.fc_vmstate, &l.fc_mem).await?;
            warren_gateway_client::wait_for_port(ip, GATEWAY_PORT, BOOT_TIMEOUT, || {
                process::is_alive(pid, &marker)
            })
            .await
        }
        .await;
        if let Err(e) = result {
            return Err(self.abandon_start(inst, p, pid, &marker, ip, e).await);
        }
        if let Err(e) = snapshot::discard(&p.snapshot_dir()) {
            tracing::warn!(instance = %inst.id, error = %e.describe(), "cannot remove the used snapshot");
        }
        if l.jail.is_some() {
            for f in [snapshot::VMSTATE, snapshot::MEM] {
                let _ = std::fs::remove_file(l.host_snapshot_dir.join(f));
            }
        }
        self.forget_cpu(&inst.id);
        tracing::info!(instance = %inst.id, pid, elapsed_ms = started.elapsed().as_millis() as u64, "VM resumed from its snapshot");
        Ok(())
    }

    /// Best-effort `sync` inside the guest, so the disk is consistent with
    /// the memory about to be saved (for forks and backups taken while
    /// asleep, and for a cold boot should the snapshot become unusable).
    async fn sync_guest(&self, inst: &Instance, ip: Ipv4Addr) {
        let req = ExecRequest {
            command: "sync".into(),
            timeout_seconds: GUEST_SYNC_TIMEOUT_SECS,
            ..Default::default()
        };
        match warren_gateway_client::exec(&self.http, ip, &inst.instance_token, &req).await {
            Ok(r) if r.exit_code == 0 => {
                tracing::debug!(instance = %inst.id, "guest disk synced before the snapshot")
            }
            Ok(r) => {
                tracing::warn!(instance = %inst.id, exit_code = r.exit_code, stderr = %r.stderr.trim(), "sync in the guest failed; its disk may lag the saved memory")
            }
            Err(e) => {
                tracing::warn!(instance = %inst.id, error = %e.describe(), "cannot sync the guest disk before the snapshot")
            }
        }
    }

    /// Pause the running VM and write its memory and state into the
    /// instance's snapshot directory with a meta file; the VM is left paused.
    /// On error it is resumed so the caller can stop it the normal way.
    async fn snapshot_vm(
        &self,
        inst: &Instance,
        p: &Paths,
        ip: Option<Ipv4Addr>,
    ) -> Result<snapshot::Meta> {
        let ip = ip
            .ok_or_else(|| Error::internal(format!("instance {} has no guest address", inst.id)))?;
        let l = self.launch(inst, p);
        let fc = api::Api::new(&l.api_sock);
        let info = fc.info().await?;
        let version = snapshot::parse_version(&info.vmm_version).unwrap_or(info.vmm_version);
        self.sync_guest(inst, ip).await;
        snapshot::discard(&p.snapshot_dir())?;
        std::fs::create_dir_all(&l.host_snapshot_dir).map_err(|e| {
            Error::internal(format!("mkdir {}: {e}", l.host_snapshot_dir.display()))
        })?;
        fc.pause().await?;
        let result = async {
            fc.create_snapshot(&l.fc_vmstate, &l.fc_mem).await?;
            if let Some(j) = &l.jail {
                j.take_snapshot_files(&p.snapshot_dir())?;
            }
            let meta = snapshot::Meta {
                firecracker_version: version,
                created_at: Self::now_unix(),
                shape: inst.shape,
                ip,
                tap: net::tap_name(&inst.id),
                mem_size_mib: inst.shape.memory.saturating_mul(1024),
            };
            snapshot::write_meta(&p.snapshot_dir(), &meta)?;
            Ok(meta)
        }
        .await;
        if result.is_err()
            && let Err(e) = fc.resume().await
        {
            tracing::warn!(instance = %inst.id, error = %e.describe(), "cannot resume the VM after a failed snapshot");
        }
        result
    }

    /// What `start` should do about a saved snapshot, given the host's
    /// Firecracker version (`None` when unknown).
    fn assess_snapshot(
        &self,
        inst: &Instance,
        p: &Paths,
        ip: Ipv4Addr,
        firecracker_version: Option<&str>,
    ) -> snapshot::Assessment {
        let tap = net::tap_name(&inst.id);
        snapshot::assess(
            &p.snapshot_dir(),
            &snapshot::Expect {
                shape: inst.shape,
                ip,
                tap: &tap,
                firecracker_version,
            },
        )
    }

    /// Drop a snapshot that can no longer be used, saying why.
    fn discard_snapshot(&self, inst: &Instance, p: &Paths, why: &str) -> Result<()> {
        if snapshot::exists(&p.snapshot_dir()) {
            tracing::info!(instance = %inst.id, why, "discarding the saved memory; the next start boots cold");
            snapshot::discard(&p.snapshot_dir())?;
        }
        Ok(())
    }

    /// CPU percentage since the previous `stats` call for this instance.
    fn sample_cpu(&self, id: &str, pid: i32) -> f64 {
        let Some(ticks) = process::cpu_ticks(pid) else {
            return 0.0;
        };
        let cur = process::CpuSample {
            at: Instant::now(),
            ticks,
        };
        let mut cache = self.cpu.lock().unwrap_or_else(|e| e.into_inner());
        let pct = cache
            .get(id)
            .map(|prev| process::cpu_percent(*prev, cur))
            .unwrap_or(0.0);
        cache.insert(id.to_string(), cur);
        pct
    }

    fn forget_cpu(&self, id: &str) {
        self.cpu
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn append_console_marker(p: &Paths, text: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p.console())
        {
            let _ = writeln!(f, "--- warren: {text} at {} ---", Self::now_unix());
        }
    }

    /// Bytes on disk: the root filesystem plus any saved memory.
    fn storage_bytes(p: &Paths) -> Result<i64> {
        Ok(rootfs::allocated_bytes(&p.rootfs())?
            .saturating_add(snapshot::allocated_bytes(&p.snapshot_dir())?))
    }
}

impl FirecrackerRuntime {
    /// The broker port guests may reach on the bridge address, when the daemon runs one.
    fn broker_port(&self) -> Option<u16> {
        self.cfg.broker_enabled().then(|| self.cfg.broker_port())
    }
}

#[async_trait]
impl Runtime for FirecrackerRuntime {
    fn name(&self) -> &'static str {
        RUNTIME_NAME
    }

    async fn broker_host(&self, _inst: &Instance) -> Result<Option<String>> {
        Ok(Some(net::HOST_IP.to_string()))
    }

    async fn ready(&self) -> Result<()> {
        host::require_root("run microVMs: tap devices, NAT and the firecracker process")?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(|e| {
                Error::not_supported(format!("/dev/kvm is not readable and writable: {e}"))
            })?;
        let bin = &self.cfg.firecracker_bin;
        if !host::is_executable(bin) {
            return Err(Error::not_supported(format!(
                "firecracker binary {} is missing or not executable",
                bin.display()
            )));
        }
        let kernel = self.kernel()?;
        if !kernel.is_file() {
            return Err(Error::not_supported(format!(
                "kernel image {} does not exist",
                kernel.display()
            )));
        }
        self.check_jail_config()?;
        // Not a hard failure: system templates work without it.
        match &self.cfg.gateway_bin {
            Some(bin) if !bin.is_file() => {
                tracing::warn!(path = %bin.display(), "gateway binary (--gateway-bin) does not exist; workspace templates cannot be created")
            }
            Some(bin) => {
                tracing::debug!(path = %bin.display(), "gateway injection enabled for workspace templates")
            }
            None => {
                tracing::info!(
                    init = GUEST_INIT,
                    "no gateway binary configured (--gateway-bin); workspace templates whose image lacks the gateway cannot be created"
                )
            }
        }
        for t in [
            "mkfs.ext4",
            "debugfs",
            "resize2fs",
            "e2fsck",
            "docker",
            "ip",
            "iptables",
        ] {
            host::tool(t)?;
        }
        let dir = self.cfg.instances_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", dir.display())))?;
        host::ensure_network(self.cfg.allow_private_egress, self.broker_port()).await?;
        match self.jail_config() {
            Some(jc) => tracing::info!(
                firecracker = %bin.display(), kernel = %kernel.display(), bridge = BRIDGE,
                jailer = %jc.jailer.display(), jail_dir = %jc.base.display(), uid = jc.uid, gid = jc.gid,
                rate_limits = self.cfg.firecracker_rate_limits,
                "firecracker runtime ready (jailed)"
            ),
            None => tracing::info!(
                firecracker = %bin.display(), kernel = %kernel.display(), bridge = BRIDGE,
                rate_limits = self.cfg.firecracker_rate_limits,
                "firecracker runtime ready"
            ),
        }
        Ok(())
    }

    async fn ensure_image(&self, image_ref: &str) -> Result<String> {
        image::ensure(image_ref).await
    }

    async fn create(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        let p = self.paths(inst);
        std::fs::create_dir_all(&p.vm)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", p.vm.display())))?;
        if !host::is_root() {
            tracing::warn!(instance = %inst.id, "not root: file ownership inside the root filesystem is not preserved");
        }
        let env = inst.effective_env(&tpl.env);
        if p.rootfs().is_file() {
            // A disk copied by clone_storage (or left by a destroy that kept
            // storage): give it this instance's identity and size.
            tracing::info!(instance = %inst.id, "adopting existing root filesystem");
            let image = p.rootfs();
            rootfs::put_file(
                &image,
                &format!("/{}", rootfs::ENV_FILE),
                &rootfs::render_env(&env),
                0o600,
            )
            .await?;
            rootfs::put_file(&image, "/etc/hostname", &format!("{}\n", inst.id), 0o644).await?;
            rootfs::grow(&image, inst.shape.disk).await?;
        } else {
            self.build_rootfs(
                inst,
                &p,
                &tpl.image_ref,
                &env,
                None,
                Self::needs_injection(tpl),
            )
            .await?;
        }
        let ip = self.ensure_ip(&p).await?;
        self.write_config(inst, &p, ip)?;
        inst.storage_path = p.root.to_string_lossy().into_owned();
        inst.runtime_ref = format!("fc-{}", inst.id);
        inst.runtime = RUNTIME_NAME.to_string();
        inst.ip = ip.to_string();
        Ok(())
    }

    async fn start(&self, inst: &mut Instance) -> Result<RunInfo> {
        host::require_root("start a microVM")?;
        let p = self.paths(inst);
        if !p.rootfs().is_file() {
            return Err(Error::conflict(format!(
                "instance {} has no root filesystem; create it first",
                inst.id
            )));
        }
        let ip = self.ensure_ip(&p).await?;
        let runtime_ref = format!("fc-{}", inst.id);
        let marker = self.marker(inst, &p);

        if let Some(pid) = self.running_pid(inst, &p)? {
            tracing::info!(instance = %inst.id, pid, "VM already running; adopting it");
            warren_gateway_client::wait_for_port(ip, GATEWAY_PORT, BOOT_TIMEOUT, || {
                process::is_alive(pid, &marker)
            })
            .await?;
            inst.ip = ip.to_string();
            inst.runtime_ref = runtime_ref.clone();
            return Ok(RunInfo {
                ip: inst.ip.clone(),
                runtime_ref,
            });
        }

        self.write_config(inst, &p, ip)?;
        if snapshot::exists(&p.snapshot_dir()) {
            let version = process::version(&self.cfg.firecracker_bin).await;
            match self.assess_snapshot(inst, &p, ip, version.as_deref()) {
                snapshot::Assessment::Restore(meta) => {
                    match self.restore_vm(inst, &p, ip, &meta).await {
                        Ok(()) => {
                            inst.ip = ip.to_string();
                            inst.runtime_ref = runtime_ref.clone();
                            inst.runtime = RUNTIME_NAME.to_string();
                            return Ok(RunInfo {
                                ip: inst.ip.clone(),
                                runtime_ref,
                            });
                        }
                        Err(e) if api::is_fault(&e) => {
                            tracing::error!(
                                instance = %inst.id, error = %e.describe(), fault = api::fault_message(&e).unwrap_or_default(),
                                "firecracker rejected the snapshot; discarding it and booting cold (processes from before the sleep are lost)"
                            );
                            snapshot::discard(&p.snapshot_dir())?;
                        }
                        Err(mut e) => {
                            tracing::error!(
                                instance = %inst.id, error = %e.describe(), dir = %p.snapshot_dir().display(),
                                "restore failed; the snapshot is kept for the next start (stop the instance to discard it)"
                            );
                            let mut details =
                                e.details.take().unwrap_or_else(|| serde_json::json!({}));
                            details["snapshot_kept"] = serde_json::Value::String(
                                p.snapshot_dir().to_string_lossy().into_owned(),
                            );
                            e.details = Some(details);
                            return Err(e);
                        }
                    }
                }
                snapshot::Assessment::Unusable(reason) => {
                    tracing::warn!(instance = %inst.id, reason, "the saved memory cannot be restored; booting cold (processes from before the sleep are lost)");
                    snapshot::discard(&p.snapshot_dir())?;
                }
                snapshot::Assessment::None => {}
            }
        }

        self.boot_vm(inst, &p, ip).await?;
        inst.ip = ip.to_string();
        inst.runtime_ref = runtime_ref.clone();
        inst.runtime = RUNTIME_NAME.to_string();
        Ok(RunInfo {
            ip: inst.ip.clone(),
            runtime_ref,
        })
    }

    async fn stop(&self, inst: &Instance, grace: Duration) -> Result<()> {
        let p = self.paths(inst);
        let ip = Self::guest_ip(inst, &p);
        if let Some(pid) = self.running_pid(inst, &p)? {
            host::require_root("stop a microVM")?;
            let marker = self.marker(inst, &p);
            if let Some(ip) = ip {
                match warren_gateway_client::shutdown(&self.http, ip, &inst.instance_token).await {
                    Ok(()) => tracing::debug!(instance = %inst.id, "shutdown requested"),
                    Err(e) => {
                        tracing::debug!(instance = %inst.id, error = %e.describe(), "shutdown request failed; waiting then signalling")
                    }
                }
            }
            if !process::wait_exit(pid, &marker, grace).await {
                tracing::warn!(instance = %inst.id, pid, "VM did not power off within {}s", grace.as_secs());
                self.kill_vm(&p, pid, &marker).await;
            }
            Self::append_console_marker(&p, "stopped");
        }
        process::clear_pid(&p.pid());
        self.forget_cpu(&inst.id);
        // A stop, unlike a sleep, forgets the memory: the next start boots cold.
        self.discard_snapshot(inst, &p, "stop")?;
        self.teardown(inst, ip).await
    }

    /// Sleep with memory: `sync` in the guest, pause, full snapshot into
    /// `vm/snapshot/`, then the process is killed and the tap removed. When
    /// the snapshot cannot be taken the VM is stopped instead (disk kept, as
    /// before) with a warning. A VM that is not running is only torn down;
    /// an existing snapshot stays (the instance is already asleep).
    async fn sleep(&self, inst: &Instance, grace: Duration) -> Result<()> {
        let p = self.paths(inst);
        let ip = Self::guest_ip(inst, &p);
        let Some(pid) = self.running_pid(inst, &p)? else {
            process::clear_pid(&p.pid());
            self.forget_cpu(&inst.id);
            return self.teardown(inst, ip).await;
        };
        host::require_root("sleep a microVM")?;
        let marker = self.marker(inst, &p);
        let started = Instant::now();
        match self.snapshot_vm(inst, &p, ip).await {
            Ok(meta) => {
                tracing::info!(
                    instance = %inst.id, pid, mem_mib = meta.mem_size_mib, firecracker = %meta.firecracker_version,
                    dir = %p.snapshot_dir().display(), elapsed_ms = started.elapsed().as_millis() as u64,
                    "memory saved; stopping the paused VM"
                );
                Self::append_console_marker(&p, "slept");
                self.terminate(pid, &marker).await;
                process::clear_pid(&p.pid());
                self.forget_cpu(&inst.id);
                self.teardown(inst, ip).await
            }
            Err(e) => {
                tracing::warn!(instance = %inst.id, error = %e.describe(), "snapshot failed; stopping the VM instead (this sleep keeps the disk only)");
                let _ = snapshot::discard(&p.snapshot_dir());
                self.stop(inst, grace).await
            }
        }
    }

    async fn destroy(&self, inst: &Instance, remove_storage: bool) -> Result<()> {
        let p = self.paths(inst);
        self.stop(inst, Duration::from_secs(STOP_GRACE_SECS))
            .await?;
        if remove_storage {
            rootfs::remove_tree(&p.root)?;
        } else {
            for scratch in [
                p.rootfs_dir(),
                p.keep_dir(),
                p.restore_dir(),
                p.backup_dir(),
                p.snapshot_dir(),
            ] {
                rootfs::remove_tree(&scratch)?;
            }
            for f in [p.config(), p.rootfs_tmp(), p.api_sock()] {
                if let Err(e) = std::fs::remove_file(&f)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(Error::internal(format!("remove {}: {e}", f.display())));
                }
            }
        }
        Ok(())
    }

    async fn exec(&self, inst: &Instance, req: &ExecRequest) -> Result<ExecResult> {
        let p = self.paths(inst);
        if self.running_pid(inst, &p)?.is_none() {
            return Err(Error::conflict(format!(
                "instance {} is not running",
                inst.id
            )));
        }
        let ip = Self::guest_ip(inst, &p)
            .ok_or_else(|| Error::internal(format!("instance {} has no guest address", inst.id)))?;
        warren_gateway_client::exec(&self.http, ip, &inst.instance_token, req).await
    }

    async fn logs(&self, inst: &Instance, opts: LogsOptions) -> Result<String> {
        let console = self.paths(inst).console();
        if opts.since > 0 {
            let modified = std::fs::metadata(&console)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            // Console lines carry no timestamps; a log untouched since `since` has nothing newer.
            if modified.is_some_and(|m| m < opts.since) {
                return Ok(String::new());
            }
        }
        process::tail(&console, opts.tail as usize, CONSOLE_TAIL_BYTES)
    }

    async fn stats(&self, inst: &Instance) -> Result<Stats> {
        let p = self.paths(inst);
        let (cpu_percent, memory_bytes) = match self.running_pid(inst, &p)? {
            Some(pid) => (
                self.sample_cpu(&inst.id, pid),
                process::rss_bytes(pid).unwrap_or(0),
            ),
            None => {
                self.forget_cpu(&inst.id);
                (0.0, 0)
            }
        };
        Ok(Stats {
            at: Self::now_unix(),
            cpu_percent,
            memory_bytes,
            memory_limit_bytes: i64::from(inst.shape.memory).saturating_mul(rootfs::GIB as i64),
            disk_bytes: Self::storage_bytes(&p)?,
        })
    }

    /// Whether the firecracker process is alive. A sleeping VM has none (its
    /// memory is on disk), so this is false while asleep.
    async fn is_running(&self, inst: &Instance) -> Result<bool> {
        let p = self.paths(inst);
        Ok(self.running_pid(inst, &p)?.is_some())
    }

    /// Grow the disk and rewrite the config. A saved snapshot is discarded
    /// when the shape changes: it holds the old vCPU and memory layout, and
    /// resizing the filesystem under a suspended guest would corrupt it.
    async fn resize(&self, inst: &mut Instance, shape: Shape) -> Result<()> {
        let p = self.paths(inst);
        if self.running_pid(inst, &p)?.is_some() {
            return Err(errors::requires_restart());
        }
        if shape != inst.shape {
            self.discard_snapshot(inst, &p, "resize")?;
        }
        if p.rootfs().is_file() {
            let current = rootfs::size_gb(&p.rootfs())?;
            if shape.disk < current {
                return Err(Error::invalid(format!(
                    "disk may only grow: it is {current} GB, {} GB requested",
                    shape.disk
                )));
            }
            if rootfs::grow(&p.rootfs(), shape.disk).await? {
                tracing::info!(instance = %inst.id, disk_gb = shape.disk, "root filesystem grown");
            }
        }
        inst.shape = shape;
        if p.config().is_file() {
            let ip = self.ensure_ip(&p).await?;
            self.write_config(inst, &p, ip)?;
        }
        Ok(())
    }

    async fn recreate(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        let p = self.paths(inst);
        if self.running_pid(inst, &p)?.is_some() {
            return Err(Error::conflict(format!(
                "instance {} must be stopped before it is recreated",
                inst.id
            )));
        }
        // A new root filesystem cannot continue the old memory.
        self.discard_snapshot(inst, &p, "recreate")?;
        let env = inst.effective_env(&tpl.env);
        let keep = p.keep_dir();
        rootfs::remove_tree(&keep)?;
        let preserved = match (p.rootfs().is_file(), Self::home_dir(inst)) {
            (true, Some(home)) => rootfs::rdump(&p.rootfs(), home, &keep).await?,
            _ => None,
        };
        let result = self
            .build_rootfs(
                inst,
                &p,
                &tpl.image_ref,
                &env,
                preserved,
                Self::needs_injection(tpl),
            )
            .await;
        let _ = rootfs::remove_tree(&keep);
        result?;
        let ip = self.ensure_ip(&p).await?;
        self.write_config(inst, &p, ip)?;
        inst.storage_path = p.root.to_string_lossy().into_owned();
        inst.runtime_ref = format!("fc-{}", inst.id);
        inst.runtime = RUNTIME_NAME.to_string();
        Ok(())
    }

    /// Copy the source's disk only. A saved snapshot is never copied: the
    /// memory holds the source's address, hostname, instance token and open
    /// connections, so a restored clone would be the source under another
    /// id. The clone boots fresh from the copied disk (which `sleep` synced
    /// before saving the memory); `create` then gives it its own identity.
    async fn clone_storage(&self, src: &Instance, dst: &mut Instance) -> Result<()> {
        let sp = self.paths(src);
        if !sp.rootfs().is_file() {
            return Err(Error::conflict(format!(
                "instance {} has no root filesystem to copy",
                src.id
            )));
        }
        if self.running_pid(src, &sp)?.is_some() {
            return Err(Error::conflict(format!(
                "instance {} must be stopped or sleeping before it is forked",
                src.id
            )));
        }
        if snapshot::exists(&sp.snapshot_dir()) {
            tracing::info!(src = %src.id, dst = %dst.id, "source sleeps with its memory saved; the copy takes the disk only and boots fresh");
        }
        let dp = Paths::new(self.cfg.instances_dir().join(&dst.id));
        std::fs::create_dir_all(&dp.vm)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", dp.vm.display())))?;
        let (from, to) = (sp.rootfs(), dp.rootfs_tmp());
        tracing::info!(src = %src.id, dst = %dst.id, "copying root filesystem");
        tokio::task::spawn_blocking(move || rootfs::sparse_copy(&from, &to))
            .await
            .map_err(|e| Error::internal(format!("copy task failed: {e}")))??;
        std::fs::rename(dp.rootfs_tmp(), dp.rootfs())
            .map_err(|e| Error::internal(format!("install copied rootfs: {e}")))?;
        let ip = self.ensure_ip(&dp).await?;
        dst.storage_path = dp.root.to_string_lossy().into_owned();
        dst.ip = ip.to_string();
        dst.runtime = RUNTIME_NAME.to_string();
        Ok(())
    }

    async fn backup_storage(&self, inst: &Instance, dest_path: &Path) -> Result<i64> {
        let p = self.paths(inst);
        if !p.rootfs().is_file() {
            return Err(Error::conflict(format!(
                "instance {} has no root filesystem to back up",
                inst.id
            )));
        }
        let scratch = p.backup_dir();
        rootfs::remove_tree(&scratch)?;
        let result = async {
            let dumped = match Self::home_dir(inst) {
                Some(home) => rootfs::rdump(&p.rootfs(), home, &scratch).await?,
                None => None,
            };
            let dir = match dumped {
                Some(d) => d,
                None => {
                    let d = scratch.join("empty");
                    std::fs::create_dir_all(&d)
                        .map_err(|e| Error::internal(format!("mkdir {}: {e}", d.display())))?;
                    d
                }
            };
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
            }
            let dest = dest_path.to_path_buf();
            tokio::task::spawn_blocking(move || rootfs::pack_dir_gz(&dir, &dest))
                .await
                .map_err(|e| Error::internal(format!("backup task failed: {e}")))?
        }
        .await;
        let _ = rootfs::remove_tree(&scratch);
        result
    }

    async fn restore_storage(&self, inst: &Instance, src_path: &Path) -> Result<()> {
        let p = self.paths(inst);
        if self.running_pid(inst, &p)?.is_some() {
            return Err(Error::conflict(format!(
                "instance {} must be stopped before it is restored",
                inst.id
            )));
        }
        if !src_path.is_file() {
            return Err(Error::not_found("backup archive"));
        }
        let Some(home) = Self::home_dir(inst) else {
            return Err(Error::not_supported(
                "this template has no mount path to restore into",
            ));
        };
        // A rebuilt root filesystem cannot continue the old memory.
        self.discard_snapshot(inst, &p, "restore")?;
        // The current image remembers what the instance record alone does not:
        // its env file carries the template defaults, and its wrap file marks a
        // workspace image whose gateway the daemon injected.
        let (base, inject_gateway) = if p.rootfs().is_file() {
            let base = rootfs::cat(&p.rootfs(), &format!("/{}", rootfs::ENV_FILE))
                .await?
                .map(|t| rootfs::parse_env(&t));
            let wrapped = rootfs::exists(&p.rootfs(), &format!("/{WRAP_FILE}")).await?;
            (base, wrapped)
        } else {
            (None, false)
        };
        let env = inst.effective_env(&base.unwrap_or_default());
        let scratch = p.restore_dir();
        rootfs::remove_tree(&scratch)?;
        let target = scratch.join(rootfs::guest_basename(home)?);
        let (src, dest) = (src_path.to_path_buf(), target.clone());
        let result = match tokio::task::spawn_blocking(move || rootfs::unpack_gz(&src, &dest))
            .await
            .map_err(|e| Error::internal(format!("restore task failed: {e}")))?
        {
            Ok(()) => {
                self.build_rootfs(
                    inst,
                    &p,
                    &inst.image_ref,
                    &env,
                    Some(target),
                    inject_gateway,
                )
                .await
            }
            Err(e) => Err(e),
        };
        let _ = rootfs::remove_tree(&scratch);
        result?;
        let ip = self.ensure_ip(&p).await?;
        self.write_config(inst, &p, ip)?;
        Ok(())
    }

    /// The root filesystem plus the saved memory of a sleeping VM (as large
    /// as the guest's RAM while it sleeps).
    async fn disk_usage(&self, inst: &Instance) -> Result<i64> {
        Self::storage_bytes(&self.paths(inst))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testutil::{TempDir, docker_test_image, firecracker_assets, have_e2fsprogs};
    use warren_core::{InstanceType, errors::CODE_NOT_SUPPORTED};

    fn test_config(data_dir: &Path) -> Config {
        Config {
            data_dir: data_dir.to_path_buf(),
            enable_firecracker: true,
            firecracker_bin: PathBuf::from("/usr/local/bin/firecracker"),
            kernel_image: Some(PathBuf::from("/var/lib/warren/firecracker/vmlinux")),
            ..Config::default()
        }
    }

    fn template(image: &str) -> Template {
        Template {
            name: "warren-base".into(),
            image_ref: image.into(),
            default_port: GATEWAY_PORT,
            mount_path: "/home/agent".into(),
            default_shape: Shape {
                cpu: 2,
                memory: 4,
                disk: 1,
            },
            env: BTreeMap::from([("TPL_VAR".to_string(), "from-template".to_string())]),
            revision: 1,
            ..Default::default()
        }
    }

    fn instance(id: &str, tpl: &Template) -> Instance {
        Instance {
            id: id.into(),
            workspace_id: "ws_test".into(),
            template: tpl.name.clone(),
            template_name: tpl.name.clone(),
            template_revision: tpl.revision,
            image_ref: tpl.image_ref.clone(),
            kind: InstanceType::Vm,
            shape: Shape {
                cpu: 2,
                memory: 4,
                disk: 1,
            },
            region: "local".into(),
            env: BTreeMap::from([("INST_VAR".to_string(), "from-instance".to_string())]),
            default_port: tpl.default_port,
            mount_path: tpl.mount_path.clone(),
            instance_token: format!("token-{id}"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn constructs_and_names_itself() {
        let tmp = TempDir::new("fc-new");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        assert_eq!(rt.name(), "firecracker");
        assert_eq!(rt.config().data_dir, tmp.path());
        let inst = instance("ab12cd34ef", &template("alpine:3.20"));
        assert_eq!(
            rt.paths(&inst).root,
            tmp.path().join("instances/ab12cd34ef")
        );
        let mut located = inst.clone();
        located.storage_path = "/elsewhere/x".into();
        assert_eq!(rt.paths(&located).vm, PathBuf::from("/elsewhere/x/vm"));
    }

    #[tokio::test]
    async fn ready_explains_what_is_missing() {
        let tmp = TempDir::new("fc-ready");
        let mut cfg = test_config(tmp.path());
        cfg.firecracker_bin = tmp.path().join("no-such-firecracker");
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        let err = rt.ready().await.unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
        if host::is_root() {
            assert!(
                err.message.contains("firecracker binary"),
                "{}",
                err.message
            );
        } else {
            assert!(err.message.contains("needs root"), "{}", err.message);
        }
        let mut cfg = test_config(tmp.path());
        cfg.kernel_image = None;
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        let err = rt.ready().await.unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn ready_succeeds_as_root_with_assets() {
        if !host::is_root() {
            eprintln!("skipping: needs root");
            return;
        }
        let Some((bin, kernel)) = firecracker_assets() else {
            eprintln!("skipping: firecracker assets not found");
            return;
        };
        let tmp = TempDir::new("fc-ready-root");
        let mut cfg = test_config(tmp.path());
        cfg.firecracker_bin = bin;
        cfg.kernel_image = Some(kernel);
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        rt.ready().await.unwrap();
        assert!(host::link_exists(BRIDGE));
        assert!(tmp.path().join("instances").is_dir());
    }

    #[tokio::test]
    async fn unprivileged_operations_on_a_missing_vm() {
        let tmp = TempDir::new("fc-missing");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let tpl = template("alpine:3.20");
        let mut inst = instance("ab12cd34ef", &tpl);

        if !host::is_root() {
            let err = rt.start(&mut inst).await.unwrap_err();
            assert_eq!(err.code, CODE_NOT_SUPPORTED);
            assert!(err.message.contains("needs root to start a microVM"));
        }
        // Nothing to stop, nothing to tear down.
        rt.stop(&inst, Duration::from_secs(1)).await.unwrap();
        rt.destroy(&inst, false).await.unwrap();
        let err = rt
            .exec(
                &inst,
                &ExecRequest {
                    command: "true".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_CONFLICT);
        assert_eq!(
            rt.logs(&inst, LogsOptions { tail: 10, since: 0 })
                .await
                .unwrap(),
            ""
        );
        let stats = rt.stats(&inst).await.unwrap();
        assert_eq!(
            (stats.cpu_percent, stats.memory_bytes, stats.disk_bytes),
            (0.0, 0, 0)
        );
        assert_eq!(stats.memory_limit_bytes, 4 << 30);
        assert_eq!(rt.disk_usage(&inst).await.unwrap(), 0);
        let bigger = Shape {
            cpu: 4,
            memory: 8,
            disk: 6,
        };
        rt.resize(&mut inst, bigger).await.unwrap();
        assert_eq!(inst.shape, bigger);
        assert!(
            rt.backup_storage(&inst, &tmp.path().join("b.tar.gz"))
                .await
                .is_err()
        );
        let err = rt
            .restore_storage(&inst, &tmp.path().join("nope.tar.gz"))
            .await
            .unwrap_err();
        assert!(err.is_not_found());
        let mut dst = instance("zz12cd34ef", &tpl);
        assert!(rt.clone_storage(&inst, &mut dst).await.is_err());
        rt.destroy(&inst, true).await.unwrap();
    }

    #[tokio::test]
    async fn logs_respect_since_and_tail() {
        let tmp = TempDir::new("fc-logs");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let inst = instance("ab12cd34ef", &template("alpine:3.20"));
        let p = rt.paths(&inst);
        std::fs::create_dir_all(&p.vm).unwrap();
        std::fs::write(p.console(), "boot\nline two\nline three\n").unwrap();
        assert_eq!(
            rt.logs(&inst, LogsOptions { tail: 2, since: 0 })
                .await
                .unwrap(),
            "line two\nline three\n"
        );
        assert_eq!(
            rt.logs(&inst, LogsOptions { tail: 0, since: 0 })
                .await
                .unwrap(),
            "boot\nline two\nline three\n"
        );
        let future = FirecrackerRuntime::now_unix() + 3600;
        assert_eq!(
            rt.logs(
                &inst,
                LogsOptions {
                    tail: 0,
                    since: future
                }
            )
            .await
            .unwrap(),
            ""
        );
        let past = FirecrackerRuntime::now_unix() - 3600;
        assert!(
            !rt.logs(
                &inst,
                LogsOptions {
                    tail: 0,
                    since: past
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    /// Everything that does not need KVM or root: build, inspect, resize, back
    /// up, recreate, restore, fork and destroy an instance's root filesystem.
    #[tokio::test]
    async fn storage_lifecycle_with_docker() {
        let Some(image) = docker_test_image().await else {
            eprintln!("skipping: docker unavailable");
            return;
        };
        if !have_e2fsprogs() {
            eprintln!("skipping: e2fsprogs not installed");
            return;
        }
        let tmp = TempDir::new("fc-lifecycle");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let tpl = template(&image);
        let mut inst = instance("ab12cd34ef", &tpl);

        assert!(
            rt.ensure_image(&image)
                .await
                .unwrap()
                .starts_with("sha256:")
        );
        rt.create(&mut inst, &tpl).await.unwrap();
        let p = rt.paths(&inst);
        assert_eq!(
            inst.storage_path,
            tmp.path().join("instances/ab12cd34ef").to_string_lossy()
        );
        assert_eq!(inst.runtime_ref, "fc-ab12cd34ef");
        assert_eq!(inst.runtime, "firecracker");
        assert_eq!(inst.ip, "172.30.0.2");
        assert!(p.rootfs().is_file());
        assert!(
            !p.rootfs_dir().exists(),
            "export directory is removed after mkfs"
        );
        assert_eq!(
            std::fs::read_to_string(p.ip()).unwrap().trim(),
            "172.30.0.2"
        );
        assert_eq!(std::fs::metadata(p.rootfs()).unwrap().len(), rootfs::GIB);

        let cfg = vmconfig::read(&p.config()).unwrap();
        assert_eq!(cfg.machine_config.vcpu_count, 2);
        assert_eq!(cfg.machine_config.mem_size_mib, 4096);
        assert_eq!(cfg.drives[0].path_on_host, p.rootfs().to_string_lossy());
        assert_eq!(cfg.network_interfaces[0].host_dev_name, "tap-ab12cd34");
        assert!(
            cfg.boot_source
                .boot_args
                .contains("ip=172.30.0.2::172.30.0.1:255.255.255.0::eth0:off")
        );
        assert!(
            cfg.boot_source
                .boot_args
                .ends_with("init=/usr/local/bin/warren-gateway -- --init")
        );

        let env = rootfs::cat(&p.rootfs(), "/etc/warren.env")
            .await
            .unwrap()
            .unwrap();
        let env = rootfs::parse_env(&env);
        assert_eq!(
            env.get("TPL_VAR").map(String::as_str),
            Some("from-template")
        );
        assert_eq!(
            env.get("INST_VAR").map(String::as_str),
            Some("from-instance")
        );
        assert_eq!(
            env.get("WARREN_INSTANCE_ID").map(String::as_str),
            Some("ab12cd34ef")
        );
        assert_eq!(
            env.get("WARREN_INSTANCE_TOKEN").map(String::as_str),
            Some("token-ab12cd34ef")
        );
        assert_eq!(
            env.get("WARREN_HOME").map(String::as_str),
            Some("/home/agent")
        );
        assert!(rootfs::exists(&p.rootfs(), "/home/agent").await.unwrap());
        assert!(rootfs::exists(&p.rootfs(), "/etc/hostname").await.unwrap());
        assert!(rt.disk_usage(&inst).await.unwrap() > 0);
        assert!(rt.stats(&inst).await.unwrap().disk_bytes > 0);

        // Resize while stopped grows the disk and rewrites the config; shrinking is refused.
        let bigger = Shape {
            cpu: 4,
            memory: 8,
            disk: 2,
        };
        rt.resize(&mut inst, bigger).await.unwrap();
        assert_eq!(
            std::fs::metadata(p.rootfs()).unwrap().len(),
            2 * rootfs::GIB
        );
        let cfg = vmconfig::read(&p.config()).unwrap();
        assert_eq!(
            (
                cfg.machine_config.vcpu_count,
                cfg.machine_config.mem_size_mib
            ),
            (4, 8192)
        );
        let err = rt
            .resize(
                &mut inst,
                Shape {
                    cpu: 2,
                    memory: 4,
                    disk: 1,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_INVALID_REQUEST);

        // Data in the home directory survives backup, recreate and restore.
        rootfs::put_file(&p.rootfs(), "/home/agent/hello.txt", "v1", 0o644)
            .await
            .unwrap();
        let backup = tmp.path().join("backups/ab12cd34ef/b1.tar.gz");
        let size = rt.backup_storage(&inst, &backup).await.unwrap();
        assert!(size > 0 && backup.is_file());
        let check = tmp.path().join("check");
        rootfs::unpack_gz(&backup, &check).unwrap();
        assert_eq!(
            std::fs::read_to_string(check.join("hello.txt")).unwrap(),
            "v1"
        );

        let mut tpl2 = tpl.clone();
        tpl2.env.insert("TPL_VAR".into(), "revision-2".into());
        rt.recreate(&mut inst, &tpl2).await.unwrap();
        assert_eq!(
            rootfs::cat(&p.rootfs(), "/home/agent/hello.txt")
                .await
                .unwrap()
                .as_deref(),
            Some("v1")
        );
        let env = rootfs::parse_env(
            &rootfs::cat(&p.rootfs(), "/etc/warren.env")
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(env.get("TPL_VAR").map(String::as_str), Some("revision-2"));
        assert_eq!(
            std::fs::metadata(p.rootfs()).unwrap().len(),
            2 * rootfs::GIB,
            "recreate keeps the shape"
        );

        rootfs::put_file(&p.rootfs(), "/home/agent/hello.txt", "v2", 0o644)
            .await
            .unwrap();
        rt.restore_storage(&inst, &backup).await.unwrap();
        assert_eq!(
            rootfs::cat(&p.rootfs(), "/home/agent/hello.txt")
                .await
                .unwrap()
                .as_deref(),
            Some("v1")
        );
        let env = rootfs::parse_env(
            &rootfs::cat(&p.rootfs(), "/etc/warren.env")
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            env.get("TPL_VAR").map(String::as_str),
            Some("revision-2"),
            "template env survives a restore"
        );
        assert_eq!(
            env.get("WARREN_INSTANCE_ID").map(String::as_str),
            Some("ab12cd34ef")
        );

        // Fork: copy the disk, then create gives the copy its own identity and ip.
        let mut dst = instance("zz98zz98zz", &tpl);
        dst.shape.disk = 3;
        rt.clone_storage(&inst, &mut dst).await.unwrap();
        assert_eq!(
            dst.storage_path,
            tmp.path().join("instances/zz98zz98zz").to_string_lossy()
        );
        rt.create(&mut dst, &tpl).await.unwrap();
        let dp = rt.paths(&dst);
        assert_eq!(dst.ip, "172.30.0.3");
        assert_eq!(
            std::fs::metadata(dp.rootfs()).unwrap().len(),
            3 * rootfs::GIB,
            "fork may grow the disk"
        );
        assert_eq!(
            rootfs::cat(&dp.rootfs(), "/home/agent/hello.txt")
                .await
                .unwrap()
                .as_deref(),
            Some("v1")
        );
        let env = rootfs::parse_env(
            &rootfs::cat(&dp.rootfs(), "/etc/warren.env")
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            env.get("WARREN_INSTANCE_ID").map(String::as_str),
            Some("zz98zz98zz")
        );
        assert_eq!(
            env.get("WARREN_INSTANCE_TOKEN").map(String::as_str),
            Some("token-zz98zz98zz")
        );
        assert_eq!(
            rootfs::cat(&dp.rootfs(), "/etc/hostname")
                .await
                .unwrap()
                .as_deref(),
            Some("zz98zz98zz\n")
        );
        let dcfg = vmconfig::read(&dp.config()).unwrap();
        assert!(dcfg.boot_source.boot_args.contains("ip=172.30.0.3::"));
        assert_eq!(dcfg.network_interfaces[0].host_dev_name, "tap-zz98zz98");

        // Destroy keeping storage removes the VM object only; with storage, everything.
        rt.destroy(&dst, false).await.unwrap();
        assert!(dp.rootfs().is_file() && !dp.config().exists());
        rt.destroy(&dst, true).await.unwrap();
        assert!(!dp.root.exists());
        rt.destroy(&inst, true).await.unwrap();
        assert!(!p.root.exists());
    }

    /// Boots a real microVM when root, KVM, Docker and the Firecracker assets
    /// are all available, with a busybox listener standing in for the gateway.
    #[tokio::test]
    async fn boots_a_microvm_as_root() {
        if !host::is_root() {
            eprintln!("skipping: needs root");
            return;
        }
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_err()
        {
            eprintln!("skipping: /dev/kvm unavailable");
            return;
        }
        let Some((bin, kernel)) = firecracker_assets() else {
            eprintln!("skipping: firecracker assets not found");
            return;
        };
        let Some(image) = docker_test_image().await else {
            eprintln!("skipping: docker unavailable");
            return;
        };
        if !have_e2fsprogs() {
            eprintln!("skipping: e2fsprogs not installed");
            return;
        }
        let tmp = TempDir::new("fc-boot");
        let mut cfg = test_config(tmp.path());
        cfg.firecracker_bin = bin;
        cfg.kernel_image = Some(kernel);
        let rt = FirecrackerRuntime::new(cfg)
            .await
            .unwrap()
            .with_guest_init("/warren-smoke-init.sh");
        rt.ready().await.unwrap();
        let tpl = template(&image);
        let mut inst = instance("fcsmoke001", &tpl);
        inst.shape = Shape {
            cpu: 1,
            memory: 1,
            disk: 1,
        };
        rt.create(&mut inst, &tpl).await.unwrap();
        let p = rt.paths(&inst);
        let init = "#!/bin/sh\necho WARREN_SMOKE_UP\nwhile true; do nc -l -p 3737 </dev/null >/dev/null 2>&1; done\n";
        rootfs::put_file(&p.rootfs(), "/warren-smoke-init.sh", init, 0o755)
            .await
            .unwrap();

        let run = rt.start(&mut inst).await.unwrap();
        assert_eq!(run.ip, "172.30.0.2");
        assert_eq!(run.runtime_ref, "fc-fcsmoke001");
        assert!(host::link_exists(&tap_name(&inst.id)));
        let pid = rt
            .running_pid(&inst, &p)
            .unwrap()
            .expect("firecracker is running");
        let stats = rt.stats(&inst).await.unwrap();
        assert!(stats.memory_bytes > 0);
        let logs = rt
            .logs(&inst, LogsOptions { tail: 0, since: 0 })
            .await
            .unwrap();
        assert!(logs.contains("WARREN_SMOKE_UP"), "{logs}");
        assert!(logs.contains("IP-Config: Complete"), "{logs}");
        // Starting again adopts the running VM.
        rt.start(&mut inst).await.unwrap();
        assert_eq!(rt.running_pid(&inst, &p).unwrap(), Some(pid));
        let err = rt
            .resize(
                &mut inst,
                Shape {
                    cpu: 2,
                    memory: 2,
                    disk: 1,
                },
            )
            .await
            .unwrap_err();
        assert!(errors::is_requires_restart(&err));

        rt.stop(&inst, Duration::from_secs(2)).await.unwrap();
        assert_eq!(rt.running_pid(&inst, &p).unwrap(), None);
        assert!(!host::link_exists(&tap_name(&inst.id)));
        rt.destroy(&inst, true).await.unwrap();
        assert!(!p.root.exists());
    }

    /// A fake `docker export`: just enough of an Alpine layout.
    fn fake_export(dir: &Path) {
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/sh"), "#!/bin/sh\n").unwrap();
    }

    #[test]
    fn scope_decides_injection() {
        let mut tpl = template("alpine:3.20");
        assert_eq!(tpl.scope, TemplateScope::System);
        assert!(!FirecrackerRuntime::needs_injection(&tpl));
        tpl.scope = TemplateScope::Workspace;
        assert!(FirecrackerRuntime::needs_injection(&tpl));
    }

    /// The staging step on a fake export: workspace images get the daemon's
    /// gateway (0755) and the wrap file, system images are left alone, and
    /// without `--gateway-bin` a foreign image is refused. No Docker, no root.
    #[tokio::test]
    async fn staging_injects_the_gateway_for_workspace_images_only() {
        let tmp = TempDir::new("fc-stage");
        let fake_gateway = tmp.path().join("warren-gateway-musl");
        std::fs::write(&fake_gateway, "ELF static gateway").unwrap();
        let mut cfg = test_config(tmp.path());
        cfg.gateway_bin = Some(fake_gateway);
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        let tpl = template("alpine:3.20");
        let inst = instance("ab12cd34ef", &tpl);
        let env = inst.effective_env(&tpl.env);
        let gateway = Path::new("usr/local/bin/warren-gateway");

        // System image: nothing but the identity files and the env.
        let dir = tmp.path().join("system");
        fake_export(&dir);
        rt.stage_rootfs_dir(&dir, &inst, &env, None, &GatewayPlan::ImageProvides)
            .unwrap();
        assert!(!dir.join(gateway).exists());
        assert!(!dir.join(WRAP_FILE).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("etc/hostname")).unwrap(),
            "ab12cd34ef\n"
        );
        assert!(dir.join(rootfs::ENV_FILE).is_file());

        // Workspace image: the gateway is copied in, the command recorded, and
        // a preserved home lands at the mount path.
        let dir = tmp.path().join("workspace");
        fake_export(&dir);
        let keep = tmp.path().join("keep/agent");
        std::fs::create_dir_all(keep.join("work")).unwrap();
        std::fs::write(keep.join("work/notes.txt"), "keep me").unwrap();
        let plan = GatewayPlan::Inject {
            command: vec!["/bin/sh".to_string()],
        };
        rt.stage_rootfs_dir(&dir, &inst, &env, Some(keep.clone()), &plan)
            .unwrap();
        let injected = dir.join(gateway);
        assert_eq!(
            std::fs::read_to_string(&injected).unwrap(),
            "ELF static gateway"
        );
        assert_eq!(
            std::fs::metadata(&injected).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(WRAP_FILE)).unwrap(),
            "[\"/bin/sh\"]\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("home/agent/work/notes.txt")).unwrap(),
            "keep me"
        );
        assert!(!keep.exists(), "the preserved home is moved, not copied");
        let staged =
            rootfs::parse_env(&std::fs::read_to_string(dir.join(rootfs::ENV_FILE)).unwrap());
        assert_eq!(
            staged.get("WARREN_INSTANCE_ID").map(String::as_str),
            Some("ab12cd34ef")
        );

        // A workspace image built on warren/base keeps its own gateway; the
        // wrap file still says what the image would have run.
        let dir = tmp.path().join("derived");
        fake_export(&dir);
        std::fs::create_dir_all(dir.join("usr/local/bin")).unwrap();
        std::fs::write(dir.join(gateway), "image gateway").unwrap();
        let own = GatewayPlan::Inject {
            command: vec![GUEST_INIT.to_string(), "--serve".to_string()],
        };
        rt.stage_rootfs_dir(&dir, &inst, &env, None, &own).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(gateway)).unwrap(),
            "image gateway"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(WRAP_FILE)).unwrap(),
            "[\"/usr/local/bin/warren-gateway\",\"--serve\"]\n"
        );

        // No --gateway-bin: a foreign image is refused before anything is
        // staged; a system image, or a workspace image with its own gateway,
        // still works.
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        assert_eq!(rt.config().gateway_bin, None);
        let dir = tmp.path().join("bare");
        fake_export(&dir);
        let err = rt
            .stage_rootfs_dir(&dir, &inst, &env, None, &plan)
            .unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
        assert!(err.message.contains("--gateway-bin"), "{}", err.message);
        assert!(!dir.join(WRAP_FILE).exists() && !dir.join("etc/hostname").exists());
        rt.stage_rootfs_dir(&dir, &inst, &env, None, &GatewayPlan::ImageProvides)
            .unwrap();
        assert!(dir.join("etc/hostname").is_file());
        std::fs::create_dir_all(dir.join("usr/local/bin")).unwrap();
        std::fs::write(dir.join(gateway), "image gateway").unwrap();
        rt.stage_rootfs_dir(&dir, &inst, &env, None, &plan).unwrap();
        assert!(dir.join(WRAP_FILE).is_file());
    }

    /// Workspace templates come from images without a gateway: the daemon's
    /// static binary is injected and the image's command recorded, across
    /// create, recreate, restore and fork; system templates are left alone.
    /// Needs Docker and e2fsprogs, not root.
    #[tokio::test]
    async fn workspace_images_get_the_gateway_with_docker() {
        let Some(image) = docker_test_image().await else {
            eprintln!("skipping: docker unavailable");
            return;
        };
        if !have_e2fsprogs() {
            eprintln!("skipping: e2fsprogs not installed");
            return;
        }
        let tmp = TempDir::new("fc-inject");
        let fake_gateway = tmp.path().join("warren-gateway");
        std::fs::write(&fake_gateway, "#!/bin/sh\necho fake static gateway\n").unwrap();
        let mut cfg = test_config(tmp.path());
        cfg.gateway_bin = Some(fake_gateway);
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        let mut tpl = template(&image);
        tpl.name = "my-image".into();
        tpl.scope = TemplateScope::Workspace;
        let command = image::inspect_command(&image).await.unwrap();
        if image == "alpine:3.20" {
            assert_eq!(command, vec!["/bin/sh"]);
        }
        let wrap = inject::render_wrap(&command);
        let wrap_path = format!("/{WRAP_FILE}");

        let mut inst = instance("ws12cd34ef", &tpl);
        rt.create(&mut inst, &tpl).await.unwrap();
        let p = rt.paths(&inst);
        assert_eq!(
            rootfs::stat_mode(&p.rootfs(), GUEST_INIT).await.unwrap(),
            Some(0o755)
        );
        assert_eq!(
            rootfs::cat(&p.rootfs(), &wrap_path)
                .await
                .unwrap()
                .as_deref(),
            Some(wrap.as_str())
        );
        assert!(
            rootfs::exists(&p.rootfs(), "/etc/warren.env")
                .await
                .unwrap()
        );

        // A new revision injects again; a restore has no template at hand and
        // takes the decision from the image it replaces.
        rt.recreate(&mut inst, &tpl).await.unwrap();
        assert!(rootfs::exists(&p.rootfs(), GUEST_INIT).await.unwrap());
        let backup = tmp.path().join("b.tar.gz");
        rt.backup_storage(&inst, &backup).await.unwrap();
        rt.restore_storage(&inst, &backup).await.unwrap();
        assert_eq!(
            rootfs::stat_mode(&p.rootfs(), GUEST_INIT).await.unwrap(),
            Some(0o755)
        );
        assert_eq!(
            rootfs::cat(&p.rootfs(), &wrap_path)
                .await
                .unwrap()
                .as_deref(),
            Some(wrap.as_str())
        );

        // A fork copies the disk, gateway included, and create adopts it as is.
        let mut dst = instance("ws98zz98zz", &tpl);
        rt.clone_storage(&inst, &mut dst).await.unwrap();
        rt.create(&mut dst, &tpl).await.unwrap();
        let dp = rt.paths(&dst);
        assert!(rootfs::exists(&dp.rootfs(), GUEST_INIT).await.unwrap());
        assert_eq!(
            rootfs::cat(&dp.rootfs(), &wrap_path)
                .await
                .unwrap()
                .as_deref(),
            Some(wrap.as_str())
        );

        // The same image under a system template: the daemon says it ships the
        // gateway, so nothing is injected and no wrap file is written.
        let sys_tpl = template(&image);
        let mut sys = instance("sy12cd34ef", &sys_tpl);
        rt.create(&mut sys, &sys_tpl).await.unwrap();
        let sp = rt.paths(&sys);
        assert!(!rootfs::exists(&sp.rootfs(), GUEST_INIT).await.unwrap());
        assert!(!rootfs::exists(&sp.rootfs(), &wrap_path).await.unwrap());

        // Without --gateway-bin a workspace image cannot become a VM, and the
        // failed build leaves nothing behind.
        let bare_rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let mut bare = instance("no12cd34ef", &tpl);
        let err = bare_rt.create(&mut bare, &tpl).await.unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
        assert!(err.message.contains("--gateway-bin"), "{}", err.message);
        let bp = bare_rt.paths(&bare);
        assert!(!bp.rootfs().exists() && !bp.rootfs_tmp().exists() && !bp.rootfs_dir().exists());

        for (r, i) in [(&rt, &inst), (&rt, &dst), (&rt, &sys), (&bare_rt, &bare)] {
            r.destroy(i, true).await.unwrap();
        }
        assert!(!p.root.exists() && !dp.root.exists() && !sp.root.exists() && !bp.root.exists());
    }

    /// A runtime configured for the jailer, dropping to the test's own ids so
    /// `chown` works without root.
    fn jailed_config(data_dir: &Path) -> Config {
        let mut cfg = test_config(data_dir);
        cfg.firecracker_jailer = Some(PathBuf::from("/usr/local/bin/jailer"));
        cfg.firecracker_jail_uid = nix::unistd::geteuid().as_raw();
        cfg.firecracker_jail_gid = nix::unistd::getegid().as_raw();
        cfg
    }

    /// A fake snapshot in the instance's vm dir: meta for `inst`, a small
    /// vmstate and a memory file as large as the guest's RAM (sparse).
    fn fake_snapshot(rt: &FirecrackerRuntime, inst: &Instance) -> PathBuf {
        let p = rt.paths(inst);
        let dir = p.snapshot_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(p.snapshot_vmstate(), b"state").unwrap();
        let f = std::fs::File::create(p.snapshot_mem()).unwrap();
        f.set_len(u64::from(inst.shape.memory) << 30).unwrap();
        snapshot::write_meta(
            &dir,
            &SnapshotMeta {
                firecracker_version: "1.18.0".into(),
                created_at: 1,
                shape: inst.shape,
                ip: Ipv4Addr::new(172, 30, 0, 2),
                tap: tap_name(&inst.id),
                mem_size_mib: inst.shape.memory * 1024,
            },
        )
        .unwrap();
        dir
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn launch_paths_and_command_lines() {
        let tmp = TempDir::new("fc-launch");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let inst = instance("ab12cd34ef", &template("alpine:3.20"));
        let p = rt.paths(&inst);

        // Plain: host paths everywhere, the vm directory as the marker.
        let l = rt.launch(&inst, &p);
        assert_eq!(l.jail, None);
        assert_eq!(l.api_sock, p.vm.join("firecracker.sock"));
        assert_eq!(l.fc_api_sock, l.api_sock);
        assert_eq!(l.fc_config, p.config());
        assert_eq!(l.fc_vmstate, p.vm.join("snapshot/vmstate"));
        assert_eq!(l.fc_mem, p.vm.join("snapshot/mem"));
        assert_eq!(l.host_snapshot_dir, p.snapshot_dir());
        assert_eq!(rt.marker(&inst, &p), p.vm.to_string_lossy().to_string());
        let (program, args) = rt.command(&l, true);
        assert_eq!(program, PathBuf::from("/usr/local/bin/firecracker"));
        assert_eq!(
            strings(&args),
            [
                "--api-sock",
                &l.api_sock.to_string_lossy(),
                "--config-file",
                &p.config().to_string_lossy()
            ]
        );
        assert!(
            strings(&args).iter().all(|a| !a.contains("--no-api")),
            "the API stays on for sleep"
        );
        let (_, args) = rt.command(&l, false);
        assert_eq!(
            strings(&args),
            ["--api-sock", &l.api_sock.to_string_lossy()]
        );
        for a in strings(&args) {
            if a.starts_with('/') {
                assert!(a.contains(&rt.marker(&inst, &p)), "{a}");
            }
        }

        // Jailed: jail-relative paths, the id as the marker, the jailer in front.
        let jrt = FirecrackerRuntime::new(jailed_config(tmp.path()))
            .await
            .unwrap();
        let jl = jrt.launch(&inst, &p);
        let jail = jl.jail.clone().expect("jailed");
        assert_eq!(
            jail.root(),
            tmp.path()
                .join("firecracker/jail/firecracker/ab12cd34ef/root")
        );
        assert_eq!(jl.api_sock, jail.root().join("run/firecracker.socket"));
        assert_eq!(jl.fc_api_sock, PathBuf::from("/run/firecracker.socket"));
        assert_eq!(jl.fc_config, PathBuf::from("/config.json"));
        assert_eq!(jl.fc_vmstate, PathBuf::from("/snapshot/vmstate"));
        assert_eq!(jl.fc_mem, PathBuf::from("/snapshot/mem"));
        assert_eq!(jl.host_snapshot_dir, jail.root().join("snapshot"));
        assert_eq!(jrt.marker(&inst, &p), "ab12cd34ef");
        let (program, args) = jrt.command(&jl, true);
        assert_eq!(program, PathBuf::from("/usr/local/bin/jailer"));
        let args = strings(&args);
        assert_eq!(args[..2], ["--id", "ab12cd34ef"]);
        assert_eq!(
            args[args.len() - 5..],
            [
                "--",
                "--api-sock",
                "/run/firecracker.socket",
                "--config-file",
                "/config.json"
            ]
        );
        let base = args.iter().position(|a| a == "--chroot-base-dir").unwrap();
        assert_eq!(
            args[base + 1],
            tmp.path().join("firecracker/jail").to_string_lossy()
        );
        let uid = args.iter().position(|a| a == "--uid").unwrap();
        assert_eq!(args[uid + 1], nix::unistd::geteuid().as_raw().to_string());
        let (_, args) = jrt.command(&jl, false);
        assert_eq!(
            strings(&args)[args.len() - 3..],
            ["--", "--api-sock", "/run/firecracker.socket"]
        );
    }

    #[tokio::test]
    async fn jailed_launch_prepares_the_chroot_with_jail_relative_paths() {
        let tmp = TempDir::new("fc-jailed");
        let kernel = tmp.path().join("vmlinux");
        std::fs::write(&kernel, "kernel").unwrap();
        let mut cfg = jailed_config(tmp.path());
        cfg.kernel_image = Some(kernel.clone());
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        let inst = instance("ab12cd34ef", &template("alpine:3.20"));
        let p = rt.paths(&inst);
        std::fs::create_dir_all(&p.vm).unwrap();
        std::fs::write(p.rootfs(), "rootfs").unwrap();
        let ip = Ipv4Addr::new(172, 30, 0, 2);

        let l = rt.launch(&inst, &p);
        rt.prepare_launch(&inst, &p, &l, ip, false).unwrap();
        let jail = l.jail.as_ref().unwrap();
        assert!(jail.root().join("vmlinux").is_file());
        assert!(jail.root().join("rootfs.ext4").is_file());
        let jcfg = vmconfig::read(&jail.root().join("config.json")).unwrap();
        assert_eq!(jcfg.boot_source.kernel_image_path, "/vmlinux");
        assert_eq!(jcfg.drives[0].path_on_host, "/rootfs.ext4");
        assert!(
            jcfg.drives[0].rate_limiter.is_some(),
            "limits on by default"
        );
        assert!(jcfg.network_interfaces[0].rx_rate_limiter.is_some());
        assert!(jcfg.network_interfaces[0].tx_rate_limiter.is_some());
        assert_eq!(jcfg.network_interfaces[0].host_dev_name, "tap-ab12cd34");
        assert!(jcfg.boot_source.boot_args.contains("ip=172.30.0.2::"));
        assert_eq!(jcfg.machine_config.mem_size_mib, 4096);
        assert!(jail.snapshot_dir().is_dir());
        assert!(!jail.snapshot_dir().join("mem").exists());

        // The host-side config keeps host paths and the same limits.
        let host_cfg = rt.write_config(&inst, &p, ip).unwrap();
        assert_eq!(
            host_cfg.drives[0].path_on_host,
            p.rootfs().to_string_lossy()
        );
        assert_eq!(host_cfg.drives[0].rate_limiter, jcfg.drives[0].rate_limiter);
        assert_eq!(
            host_cfg.drives[0].rate_limiter,
            Some(limits::for_shape(inst.shape).drive())
        );

        // A restore links the snapshot files in.
        fake_snapshot(&rt, &inst);
        rt.prepare_launch(&inst, &p, &l, ip, true).unwrap();
        assert!(jail.snapshot_dir().join("mem").is_file());
        assert!(jail.snapshot_dir().join("vmstate").is_file());

        // Teardown (no tap to delete when unprivileged) removes the jail.
        rt.teardown(&inst, None).await.unwrap();
        assert!(!jail.dir().exists());
        assert!(p.rootfs().is_file() && p.snapshot_mem().is_file());

        // Rate limits off: no limiter keys at all.
        let mut cfg = jailed_config(tmp.path());
        cfg.firecracker_rate_limits = false;
        cfg.kernel_image = Some(kernel);
        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        assert_eq!(rt.rate_limits(inst.shape), None);
        let l = rt.launch(&inst, &p);
        rt.prepare_launch(&inst, &p, &l, ip, false).unwrap();
        let jcfg = vmconfig::read(&l.jail.as_ref().unwrap().root().join("config.json")).unwrap();
        assert_eq!(jcfg.drives[0].rate_limiter, None);
        assert_eq!(jcfg.network_interfaces[0].rx_rate_limiter, None);
        assert_eq!(jcfg.network_interfaces[0].tx_rate_limiter, None);
    }

    #[tokio::test]
    async fn jail_config_is_validated() {
        let tmp = TempDir::new("fc-jail-cfg");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        assert!(rt.jail_config().is_none());
        rt.check_jail_config().unwrap();

        let mut cfg = jailed_config(tmp.path());
        let rt = FirecrackerRuntime::new(cfg.clone()).await.unwrap();
        let err = rt.check_jail_config().unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
        assert!(err.message.contains("jailer binary"), "{}", err.message);

        let fake_jailer = tmp.path().join("jailer");
        std::fs::write(&fake_jailer, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake_jailer, std::fs::Permissions::from_mode(0o755)).unwrap();
        cfg.firecracker_jailer = Some(fake_jailer);
        let mut zero = cfg.clone();
        zero.firecracker_jail_gid = 0;
        let err = FirecrackerRuntime::new(zero)
            .await
            .unwrap()
            .check_jail_config()
            .unwrap_err();
        assert!(err.message.contains("must not be 0"), "{}", err.message);

        let rt = FirecrackerRuntime::new(cfg).await.unwrap();
        rt.check_jail_config().unwrap();
        assert!(tmp.path().join("firecracker/jail").is_dir());
        let jc = rt.jail_config().unwrap();
        assert_eq!(jc.base, tmp.path().join("firecracker/jail"));
        assert_eq!(jc.firecracker, PathBuf::from("/usr/local/bin/firecracker"));
    }

    /// The decisions `start` takes about a saved snapshot, on this instance's
    /// files, without root: restore when everything matches, cold boot (with
    /// the reason) otherwise.
    #[tokio::test]
    async fn start_assesses_the_snapshot_against_the_instance() {
        let tmp = TempDir::new("fc-assess");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let mut inst = instance("ab12cd34ef", &template("alpine:3.20"));
        let p = rt.paths(&inst);
        let ip = Ipv4Addr::new(172, 30, 0, 2);
        assert_eq!(
            rt.assess_snapshot(&inst, &p, ip, Some("1.18.0")),
            SnapshotAssessment::None
        );
        fake_snapshot(&rt, &inst);
        assert!(matches!(
            rt.assess_snapshot(&inst, &p, ip, Some("Firecracker v1.18.0")),
            SnapshotAssessment::Restore(_)
        ));
        assert!(matches!(
            rt.assess_snapshot(&inst, &p, ip, None),
            SnapshotAssessment::Restore(_)
        ));
        assert!(matches!(
            rt.assess_snapshot(&inst, &p, ip, Some("Firecracker v1.19.0")),
            SnapshotAssessment::Unusable(_)
        ));
        assert!(matches!(
            rt.assess_snapshot(&inst, &p, Ipv4Addr::new(172, 30, 0, 3), Some("1.18.0")),
            SnapshotAssessment::Unusable(_)
        ));
        inst.shape = Shape {
            cpu: 4,
            memory: 8,
            disk: 1,
        };
        let SnapshotAssessment::Unusable(why) = rt.assess_snapshot(&inst, &p, ip, Some("1.18.0"))
        else {
            panic!("a resized instance cannot load its old memory");
        };
        assert!(why.contains("4 vCPU / 8 GB"), "{why}");
    }

    /// Without a running VM and without root: sleep leaves an existing
    /// snapshot alone, is_running is false, disk usage counts the snapshot,
    /// a resize to another shape, a stop and a destroy discard it.
    #[tokio::test]
    async fn snapshot_lifecycle_without_a_vm() {
        let tmp = TempDir::new("fc-snap-life");
        let rt = FirecrackerRuntime::new(test_config(tmp.path()))
            .await
            .unwrap();
        let mut inst = instance("ab12cd34ef", &template("alpine:3.20"));
        let p = rt.paths(&inst);
        let dir = fake_snapshot(&rt, &inst);
        std::fs::write(p.snapshot_vmstate(), vec![1u8; 8192]).unwrap();
        assert!(!rt.is_running(&inst).await.unwrap());
        let usage = rt.disk_usage(&inst).await.unwrap();
        assert!(usage >= 8192, "{usage}");
        assert_eq!(rt.stats(&inst).await.unwrap().disk_bytes, usage);

        rt.sleep(&inst, Duration::from_secs(1)).await.unwrap();
        assert!(
            snapshot::exists(&dir),
            "sleeping an instance that is already asleep keeps its memory"
        );
        let same = inst.shape;
        rt.resize(&mut inst, same).await.unwrap();
        assert!(snapshot::exists(&dir), "the same shape keeps it");
        rt.resize(
            &mut inst,
            Shape {
                cpu: 4,
                memory: 8,
                disk: 1,
            },
        )
        .await
        .unwrap();
        assert!(!snapshot::exists(&dir), "another shape discards it");

        fake_snapshot(&rt, &inst);
        rt.stop(&inst, Duration::from_secs(1)).await.unwrap();
        assert!(!snapshot::exists(&dir), "stop discards the saved memory");
        assert_eq!(rt.disk_usage(&inst).await.unwrap(), 0);

        fake_snapshot(&rt, &inst);
        rt.destroy(&inst, false).await.unwrap();
        assert!(!snapshot::exists(&dir) && p.vm.is_dir());
        fake_snapshot(&rt, &inst);
        rt.destroy(&inst, true).await.unwrap();
        assert!(!p.root.exists());
    }
}
