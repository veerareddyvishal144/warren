//! The [`DockerRuntime`] type: connection, readiness, runtime selection, the two
//! networks, and the [`Runtime`] trait implementation (which delegates to the
//! sibling modules).

use std::{
    collections::{BTreeSet, HashMap},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bollard::{Docker, models::NetworkCreateRequest, query_parameters::ListContainersOptions};
use warren_core::{
    Config, Error, ExecRequest, ExecResult, Instance, LogsOptions, Result, RunInfo, Runtime, Shape,
    Stats, Template,
};

use crate::{
    CONTAINER_PREFIX, HOME_DIR, NETWORK_INTERNAL, NETWORK_PUBLIC, container::network_for, error,
    netpolicy, observe::CpuSample,
};

/// Default-type instances as Docker containers.
///
/// Build one with [`DockerRuntime::new`] and call [`Runtime::ready`] before creating
/// instances: it chooses the OCI runtime and creates the networks. (`create` runs it
/// on demand when nobody did.) The value is `Send + Sync` and meant to live behind
/// an `Arc`.
pub struct DockerRuntime {
    pub(crate) cfg: Config,
    pub(crate) docker: Docker,
    /// OCI runtime chosen by `ready`.
    chosen: OnceLock<String>,
    /// Last CPU counters per container, so one-shot stats become a rate.
    pub(crate) cpu_samples: Mutex<HashMap<String, CpuSample>>,
    /// Networks found with inter-container communication on and containers
    /// attached; no new instance is created on them (see `netpolicy`).
    blocked_networks: Mutex<BTreeSet<String>>,
    /// The egress firewall rules were installed by the last `ready`.
    egress_installed: AtomicBool,
    /// The "not root, no egress rules" warning was logged.
    egress_warned: AtomicBool,
    /// Checkpoints are in use: requested by the config and the engine runs in
    /// experimental mode (decided by `ready`); see the `checkpoint` module.
    pub(crate) checkpoints: AtomicBool,
    /// The "requested but the engine is not experimental" warning was logged.
    pub(crate) checkpoints_warned: AtomicBool,
}

/// Outcome of choosing the OCI runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) name: String,
    /// `auto` wanted runsc but the daemon only offers runc.
    pub(crate) fell_back: bool,
}

/// Decide the OCI runtime from the configured preference and what the daemon offers.
///
/// `runsc` requires gVisor; `runc` (or an empty preference) is always accepted;
/// `auto` prefers runsc, falls back to runc, and on an engine that offers
/// neither (smolvm's Docker API, where every container is a microVM) takes
/// the runtime it does offer; any other name must be registered with the daemon.
pub(crate) fn select_runtime(preference: &str, available: &BTreeSet<String>) -> Result<Selection> {
    let has_runsc = available.contains("runsc");
    let pick = |name: &str, fell_back: bool| Selection {
        name: name.to_string(),
        fell_back,
    };
    match preference.trim() {
        "runsc" if has_runsc => Ok(pick("runsc", false)),
        "runsc" => Err(Error::not_supported(
            "the Docker daemon has no runsc (gVisor) runtime; install gVisor or start warrend with --runtime runc",
        )),
        "" | "runc" => Ok(pick("runc", false)),
        "auto" if has_runsc => Ok(pick("runsc", false)),
        "auto" if available.contains("runc") || available.is_empty() => Ok(pick("runc", true)),
        // An engine that offers neither (smolvm's Docker API offers only
        // `smolvm`, a microVM per container): take what it has.
        "auto" => Ok(pick(available.iter().next().expect("non-empty"), false)),
        other if available.contains(other) => Ok(pick(other, false)),
        other => {
            let offered = available.iter().cloned().collect::<Vec<_>>().join(", ");
            Err(Error::invalid(format!(
                "unknown docker runtime {other:?}; the daemon offers: {offered}"
            )))
        }
    }
}

/// `warren-<id>`: the container name of an instance.
pub(crate) fn container_name(instance_id: &str) -> String {
    format!("{CONTAINER_PREFIX}{instance_id}")
}

/// What Docker calls the instance's container: the recorded container id, else its name.
pub(crate) fn target(inst: &Instance) -> String {
    if inst.runtime_ref.is_empty() {
        container_name(&inst.id)
    } else {
        inst.runtime_ref.clone()
    }
}

impl DockerRuntime {
    /// Build a client for the local Docker daemon (`DOCKER_HOST` or the default
    /// socket). Nothing is sent until [`Runtime::ready`].
    pub async fn new(cfg: Config) -> Result<DockerRuntime> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| Error::internal(format!("docker: cannot build a client: {e}")))?;
        tracing::debug!(preference = %cfg.runtime, instances_dir = %cfg.instances_dir().display(), "docker client built");
        Ok(DockerRuntime {
            cfg,
            docker,
            chosen: OnceLock::new(),
            cpu_samples: Mutex::new(HashMap::new()),
            blocked_networks: Mutex::new(BTreeSet::new()),
            egress_installed: AtomicBool::new(false),
            egress_warned: AtomicBool::new(false),
            checkpoints: AtomicBool::new(false),
            checkpoints_warned: AtomicBool::new(false),
        })
    }

    /// True when the last [`Runtime::ready`] installed the host firewall rules
    /// that keep instances away from the metadata service, private networks and
    /// the host (root only; see the `netpolicy` module).
    pub fn egress_rules_installed(&self) -> bool {
        self.egress_installed.load(Ordering::Relaxed)
    }

    /// Networks that exist with inter-container communication enabled and
    /// containers attached; `create` refuses instances on them until the
    /// operator recreates the network.
    pub fn blocked_networks(&self) -> Vec<String> {
        self.blocked_networks
            .lock()
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn network_blocked(&self, name: &str) -> bool {
        self.blocked_networks
            .lock()
            .map(|b| b.contains(name))
            .unwrap_or(false)
    }

    fn set_network_blocked(&self, name: &str, blocked: bool) {
        if let Ok(mut set) = self.blocked_networks.lock() {
            if blocked {
                set.insert(name.to_string());
            } else {
                set.remove(name);
            }
        }
    }

    /// The OCI runtime new containers are created with (`runsc` or `runc`). Empty
    /// until [`Runtime::ready`] has run once.
    pub fn runtime_name(&self) -> &str {
        self.chosen.get().map(String::as_str).unwrap_or("")
    }

    /// The underlying bollard client.
    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    /// The configuration this runtime was built with.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The chosen runtime, running `ready` first when nobody did.
    pub(crate) async fn chosen_runtime(&self) -> Result<String> {
        if let Some(name) = self.chosen.get() {
            return Ok(name.clone());
        }
        self.ready_impl().await?;
        Ok(self.runtime_name().to_string())
    }

    /// `inst.storage_path` when set, else `<instances_dir>/<id>`.
    pub(crate) fn storage_path(&self, inst: &Instance) -> PathBuf {
        if inst.storage_path.is_empty() {
            self.cfg.instances_dir().join(&inst.id)
        } else {
            PathBuf::from(&inst.storage_path)
        }
    }

    /// The directory bind-mounted into the container.
    pub(crate) fn home_path(&self, inst: &Instance) -> PathBuf {
        self.storage_path(inst).join(HOME_DIR)
    }

    /// Ping, choose the runtime, ensure both networks.
    pub(crate) async fn ready_impl(&self) -> Result<()> {
        self.docker
            .ping()
            .await
            .map_err(|e| Error::try_again(format!("docker daemon unreachable: {e}")))?;
        let info = self
            .docker
            .info()
            .await
            .map_err(|e| error::map(e, "docker info"))?;
        let available: BTreeSet<String> = info.runtimes.unwrap_or_default().into_keys().collect();
        let selection = select_runtime(&self.cfg.runtime, &available)?;
        match self.chosen.get() {
            None => {
                if selection.fell_back {
                    tracing::warn!(
                        "gVisor (runsc) is not registered with Docker; falling back to runc, which isolates less"
                    );
                }
                let _ = self.chosen.set(selection.name.clone());
            }
            Some(previous) if previous != &selection.name => {
                tracing::warn!(%previous, now = %selection.name, "docker runtime changed since startup; keeping the first choice");
            }
            Some(_) => {}
        }
        self.ensure_network(NETWORK_PUBLIC, false).await?;
        self.ensure_network(NETWORK_INTERNAL, true).await?;
        self.install_egress_rules().await?;
        self.note_checkpoint_support(
            info.experimental_build.unwrap_or(false),
            self.runtime_name(),
        );
        tracing::info!(
            runtime = %self.runtime_name(),
            server = info.server_version.as_deref().unwrap_or("?"),
            offered = ?available,
            egress_rules = self.egress_rules_installed(),
            checkpoints = self.checkpoints_enabled(),
            "docker runtime ready"
        );
        Ok(())
    }

    /// Containers (running or not) attached to the network `name`.
    async fn attached_containers(&self, name: &str) -> Result<Vec<String>> {
        let options = ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "network".to_string(),
                vec![name.to_string()],
            )])),
            ..Default::default()
        };
        let list = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(|e| error::map(e, "container"))?;
        Ok(list
            .into_iter()
            .map(|c| {
                c.names
                    .and_then(|n| n.first().cloned())
                    .or(c.id)
                    .unwrap_or_default()
            })
            .collect())
    }

    /// Create the bridge network `name` with inter-container communication off,
    /// unless it already exists that way. An existing network with ICC on is
    /// recreated when nothing is attached to it and otherwise blocked for new
    /// instances with an error naming the fix.
    async fn ensure_network(&self, name: &str, internal: bool) -> Result<()> {
        match self.docker.inspect_network(name, None).await {
            Ok(net) => {
                if net.internal.unwrap_or(false) != internal {
                    tracing::warn!(
                        network = name,
                        want_internal = internal,
                        "existing network has the wrong internal flag"
                    );
                }
                if netpolicy::icc_disabled(net.options.as_ref()) {
                    self.set_network_blocked(name, false);
                    tracing::debug!(network = name, "network present with ICC off");
                    return Ok(());
                }
                let attached = self.attached_containers(name).await?;
                if !attached.is_empty() {
                    self.set_network_blocked(name, true);
                    tracing::error!(
                        network = name,
                        containers = ?attached,
                        "{}; refusing to create new instances on it until then",
                        netpolicy::blocked_network_hint(name)
                    );
                    return Ok(());
                }
                tracing::warn!(
                    network = name,
                    "network exists with inter-container communication on and nothing attached; recreating it with ICC off"
                );
                match self.docker.remove_network(name).await {
                    Ok(()) => {}
                    Err(e) if error::is_not_found(&e) => {}
                    Err(e) => return Err(error::map(e, "network")),
                }
            }
            Err(e) if error::is_not_found(&e) => {}
            Err(e) => return Err(error::map(e, "network")),
        }
        let request = NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            internal: Some(internal),
            options: Some(netpolicy::network_options()),
            labels: Some(HashMap::from([(
                "warren.managed".to_string(),
                "true".to_string(),
            )])),
            ..Default::default()
        };
        self.set_network_blocked(name, false);
        match self.docker.create_network(request).await {
            Ok(created) => {
                if !created.warning.is_empty() {
                    tracing::warn!(network = name, warning = %created.warning, "docker network warning");
                }
                tracing::info!(network = name, id = %created.id, internal, "created docker network");
                Ok(())
            }
            Err(e) if error::is_conflict(&e) => {
                tracing::debug!(network = name, "network appeared concurrently");
                Ok(())
            }
            Err(e) => Err(error::map(e, "network")),
        }
    }

    /// Host interface of the Docker network `name`, from its options or id.
    pub(crate) async fn bridge_interface(&self, name: &str) -> Result<String> {
        let net = self
            .docker
            .inspect_network(name, None)
            .await
            .map_err(|e| error::map(e, "network"))?;
        netpolicy::bridge_name(net.options.as_ref(), net.id.as_deref().unwrap_or("")).ok_or_else(
            || {
                Error::internal(format!(
                    "cannot determine the bridge interface of docker network {name}"
                ))
            },
        )
    }

    /// IPv4 gateway of the Docker network `name`: the host's address on that
    /// bridge, where instances reach the daemon's broker.
    pub(crate) async fn network_gateway(&self, name: &str) -> Result<Option<Ipv4Addr>> {
        let net = self
            .docker
            .inspect_network(name, None)
            .await
            .map_err(|e| error::map(e, "network"))?;
        Ok(net
            .ipam
            .and_then(|i| i.config)
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c.gateway.as_deref())
            .find_map(netpolicy::parse_gateway))
    }

    /// Install the egress firewall rules for both Warren bridges when running as
    /// root; otherwise warn once and leave them out. Idempotent. As root a
    /// failure is an error: the daemon must not silently run without them.
    /// With a broker, each bridge also gets the accept for the broker port on
    /// its gateway, inserted last so it is evaluated first.
    async fn install_egress_rules(&self) -> Result<()> {
        if !netpolicy::is_root() {
            if !self.egress_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    uid = netpolicy::effective_uid(),
                    "not root: egress rules (metadata service, private ranges, host) are not installed; instances can reach them"
                );
            }
            return Ok(());
        }
        let iptables = netpolicy::find_tool("iptables").ok_or_else(|| {
            Error::not_supported(
                "iptables is not installed; it is needed for the instance egress rules",
            )
        })?;
        let host_addrs = netpolicy::host_addresses().await;
        if host_addrs.is_empty() && !self.cfg.allow_private_egress {
            tracing::warn!(
                "no host addresses found; instances are not kept from reaching the host"
            );
        }
        let broker_port = self.cfg.broker_enabled().then(|| self.cfg.broker_port());
        let mut added = 0usize;
        for name in [NETWORK_PUBLIC, NETWORK_INTERNAL] {
            let bridge = self.bridge_interface(name).await?;
            for rule in netpolicy::egress_rules(&bridge, self.cfg.allow_private_egress, &host_addrs)
            {
                if netpolicy::ensure_rule(&iptables, &rule).await? {
                    added += 1;
                }
            }
            if let Some(port) = broker_port {
                match self.network_gateway(name).await? {
                    Some(gateway) => {
                        for rule in netpolicy::broker_rules(&bridge, gateway, port) {
                            if netpolicy::ensure_rule(&iptables, &rule).await? {
                                added += 1;
                            }
                        }
                    }
                    None => tracing::warn!(
                        network = name,
                        "network has no IPv4 gateway; its instances cannot reach the broker"
                    ),
                }
            }
        }
        self.egress_installed.store(true, Ordering::Relaxed);
        tracing::info!(
            added,
            allow_private_egress = self.cfg.allow_private_egress,
            broker_port,
            host_addrs = ?host_addrs,
            "egress rules in place for the warren bridges"
        );
        Ok(())
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn ready(&self) -> Result<()> {
        self.ready_impl().await
    }

    async fn ensure_image(&self, image_ref: &str) -> Result<String> {
        self.ensure_image_impl(image_ref).await
    }

    async fn broker_host(&self, inst: &Instance) -> Result<Option<String>> {
        Ok(self
            .network_gateway(network_for(inst.network))
            .await?
            .map(|ip| ip.to_string()))
    }

    async fn create(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        self.create_with(inst, tpl, None).await
    }

    async fn start(&self, inst: &mut Instance) -> Result<RunInfo> {
        self.start_impl(inst).await
    }

    async fn stop(&self, inst: &Instance, grace: Duration) -> Result<()> {
        self.stop_impl(inst, grace).await
    }

    async fn sleep(&self, inst: &Instance, grace: Duration) -> Result<()> {
        self.sleep_impl(inst, grace).await
    }

    /// From the container's state, not its stats: the engine answers one-shot
    /// stats (zeros) for an exited container too, which would read as alive.
    async fn is_running(&self, inst: &Instance) -> Result<bool> {
        match self.docker.inspect_container(&target(inst), None).await {
            Ok(inspect) => Ok(crate::container::is_running(&inspect)),
            Err(e) if error::is_not_found(&e) => Ok(false),
            Err(e) => Err(error::map(e, "container")),
        }
    }

    async fn destroy(&self, inst: &Instance, remove_storage: bool) -> Result<()> {
        self.destroy_impl(inst, remove_storage).await
    }

    async fn exec(&self, inst: &Instance, req: &ExecRequest) -> Result<ExecResult> {
        self.exec_impl(inst, req).await
    }

    async fn logs(&self, inst: &Instance, opts: LogsOptions) -> Result<String> {
        self.logs_impl(inst, opts).await
    }

    async fn stats(&self, inst: &Instance) -> Result<Stats> {
        self.stats_impl(inst).await
    }

    async fn resize(&self, inst: &mut Instance, shape: Shape) -> Result<()> {
        self.resize_impl(inst, shape).await
    }

    async fn recreate(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        self.recreate_impl(inst, tpl).await
    }

    async fn clone_storage(&self, src: &Instance, dst: &mut Instance) -> Result<()> {
        self.clone_storage_impl(src, dst).await
    }

    async fn backup_storage(&self, inst: &Instance, dest_path: &Path) -> Result<i64> {
        self.backup_storage_impl(inst, dest_path).await
    }

    async fn restore_storage(&self, inst: &Instance, src_path: &Path) -> Result<()> {
        self.restore_storage_impl(inst, src_path).await
    }

    async fn disk_usage(&self, inst: &Instance) -> Result<i64> {
        self.disk_usage_impl(inst).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn selects_runtime_by_preference() {
        let both = offered(&["io.containerd.runc.v2", "runc", "runsc"]);
        let runc_only = offered(&["io.containerd.runc.v2", "runc"]);
        assert_eq!(
            select_runtime("auto", &both).unwrap(),
            Selection {
                name: "runsc".into(),
                fell_back: false
            }
        );
        assert_eq!(
            select_runtime("auto", &runc_only).unwrap(),
            Selection {
                name: "runc".into(),
                fell_back: true
            }
        );
        // smolvm's Docker API offers only its own runtime: auto takes it.
        assert_eq!(
            select_runtime("auto", &offered(&["smolvm"])).unwrap(),
            Selection {
                name: "smolvm".into(),
                fell_back: false
            }
        );
        assert_eq!(select_runtime("runsc", &both).unwrap().name, "runsc");
        assert_eq!(
            select_runtime("runsc", &runc_only).unwrap_err().code,
            warren_core::errors::CODE_NOT_SUPPORTED
        );
        assert_eq!(select_runtime("runc", &runc_only).unwrap().name, "runc");
        assert_eq!(select_runtime("", &BTreeSet::new()).unwrap().name, "runc");
        assert_eq!(
            select_runtime("io.containerd.runc.v2", &both).unwrap().name,
            "io.containerd.runc.v2"
        );
        assert_eq!(
            select_runtime("kata", &both).unwrap_err().code,
            warren_core::errors::CODE_INVALID_REQUEST
        );
    }

    #[test]
    fn names_and_targets() {
        assert_eq!(container_name("ab12cd34ef"), "warren-ab12cd34ef");
        let mut inst = Instance {
            id: "ab12cd34ef".into(),
            ..Default::default()
        };
        assert_eq!(target(&inst), "warren-ab12cd34ef");
        inst.runtime_ref = "deadbeef".into();
        assert_eq!(target(&inst), "deadbeef");
    }

    #[tokio::test]
    async fn storage_paths_follow_config_until_recorded() {
        let cfg = Config {
            data_dir: PathBuf::from("/var/lib/warren-test"),
            ..Config::default()
        };
        let rt = DockerRuntime::new(cfg)
            .await
            .expect("building a client needs no daemon");
        assert_eq!(rt.runtime_name(), "");
        let mut inst = Instance {
            id: "ab12cd34ef".into(),
            ..Default::default()
        };
        assert_eq!(
            rt.storage_path(&inst),
            PathBuf::from("/var/lib/warren-test/instances/ab12cd34ef")
        );
        assert_eq!(
            rt.home_path(&inst),
            PathBuf::from("/var/lib/warren-test/instances/ab12cd34ef/home")
        );
        inst.storage_path = "/elsewhere/ab12cd34ef".into();
        assert_eq!(
            rt.home_path(&inst),
            PathBuf::from("/elsewhere/ab12cd34ef/home")
        );
        assert_eq!(rt.name(), "docker");
    }
}
