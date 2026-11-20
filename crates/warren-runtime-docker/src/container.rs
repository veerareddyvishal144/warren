//! Container lifecycle: create, start, stop, destroy, resize and recreate.
//!
//! `create` (and `recreate`, which goes through it) injects the gateway into
//! images that do not ship it; see the `inject` module. `start` waits for the
//! gateway port as always, so injected images pass that wait; when a
//! workspace-template container that was created without an injected gateway
//! fails to listen, the error names `--gateway-bin` as the fix.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    time::{Duration, Instant},
};

use bollard::{
    models::{
        ContainerCreateBody, ContainerInspectResponse, ContainerUpdateBody, HostConfig,
        RestartPolicy, RestartPolicyNameEnum,
    },
    query_parameters::{CreateContainerOptions, RemoveContainerOptions, StopContainerOptions},
};
use tokio::net::TcpStream;
use warren_core::{
    Error, GATEWAY_PORT, Instance, NetworkMode, Result, RunInfo, Shape, Template,
    errors::requires_restart,
};

use crate::{
    DockerRuntime, HOME_DIR, LABEL_INJECTED, LABEL_INSTANCE, LABEL_SCOPE, LABEL_WORKSPACE,
    NETWORK_INTERNAL, NETWORK_PUBLIC, START_POLL_INTERVAL, START_TIMEOUT, error,
    inject::{Injection, explain_missing_gateway, gateway_bind, wrap_entrypoint},
    netpolicy::{self, PIDS_LIMIT},
    runtime::{container_name, target},
    storage,
};

/// Host-side inputs of a create-container request that are not on the instance.
pub(crate) struct ContainerSpec<'a> {
    /// OCI runtime name (`runsc` or `runc`).
    pub(crate) runtime: &'a str,
    /// Host directory bind-mounted at the template's mount path.
    pub(crate) home: &'a Path,
    /// Image reference to run.
    pub(crate) image: &'a str,
    /// Add `host.docker.internal` pointing at the host gateway.
    pub(crate) dev_mode: bool,
    /// Override the image's command (tests use it to keep a shell image alive).
    /// With an injection it replaces the image's `Cmd` inside the wrapped command.
    pub(crate) cmd: Option<Vec<String>>,
    /// Inject the gateway (see the `inject` module); `None` runs the image as is.
    pub(crate) inject: Option<Injection>,
}

/// Memory limit in bytes for a shape's GB.
pub(crate) fn memory_bytes(gb: u32) -> i64 {
    i64::from(gb) << 30
}

/// Docker `NanoCpus` for a vCPU count.
pub(crate) fn nano_cpus(cpu: u32) -> i64 {
    i64::from(cpu) * 1_000_000_000
}

/// `KEY=VALUE` pairs in key order.
pub(crate) fn env_pairs(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// The Docker network implementing a network policy: `allowlist` instances
/// sit on the internal bridge like `deny-all` ones and reach the host-side
/// broker only (see `netpolicy::broker_rules`).
pub(crate) fn network_for(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::AllowAll => NETWORK_PUBLIC,
        NetworkMode::DenyAll | NetworkMode::Allowlist => NETWORK_INTERNAL,
    }
}

/// The port `start` waits on: the gateway port, unless the template has no default
/// port (then nothing inside is expected to listen).
pub(crate) fn readiness_port(inst: &Instance) -> Option<u16> {
    (inst.default_port != 0).then_some(GATEWAY_PORT)
}

/// The container's IP on `preferred`, else on the first other network (by name)
/// that has one, so repeated inspects of the same container agree.
pub(crate) fn pick_ip(inspect: &ContainerInspectResponse, preferred: &str) -> Option<String> {
    let networks = inspect.network_settings.as_ref()?.networks.as_ref()?;
    let address = |ip: &Option<String>| {
        ip.as_deref()
            .filter(|ip| !ip.is_empty())
            .map(str::to_string)
    };
    if let Some(ip) = networks.get(preferred).and_then(|e| address(&e.ip_address)) {
        return Some(ip);
    }
    let mut others: Vec<_> = networks.iter().collect();
    others.sort_by(|a, b| a.0.cmp(b.0));
    others.into_iter().find_map(|(_, e)| address(&e.ip_address))
}

/// Whether Docker reports the container as running.
pub(crate) fn is_running(inspect: &ContainerInspectResponse) -> bool {
    inspect
        .state
        .as_ref()
        .and_then(|s| s.running)
        .unwrap_or(false)
}

fn exit_code(inspect: &ContainerInspectResponse) -> i64 {
    inspect
        .state
        .as_ref()
        .and_then(|s| s.exit_code)
        .unwrap_or(-1)
}

/// Build the create-container body for `inst` from `tpl` and `spec`.
pub(crate) fn create_body(
    inst: &Instance,
    tpl: &Template,
    spec: ContainerSpec<'_>,
) -> Result<ContainerCreateBody> {
    let mount_path = if tpl.mount_path.is_empty() {
        inst.mount_path.as_str()
    } else {
        tpl.mount_path.as_str()
    };
    if !mount_path.starts_with('/') {
        return Err(Error::invalid(format!(
            "mount_path {mount_path:?} must be an absolute path"
        )));
    }
    let shape = inst.shape;
    let mut binds = vec![format!("{}:{mount_path}", spec.home.display())];
    if let Some(inject) = &spec.inject {
        binds.push(gateway_bind(&inject.gateway_bin));
    }
    let host_config = HostConfig {
        runtime: Some(spec.runtime.to_string()),
        memory: (shape.memory > 0).then(|| memory_bytes(shape.memory)),
        memory_swap: (shape.memory > 0).then(|| memory_bytes(shape.memory)),
        nano_cpus: (shape.cpu > 0).then(|| nano_cpus(shape.cpu)),
        binds: Some(binds),
        network_mode: Some(network_for(inst.network).to_string()),
        pids_limit: Some(PIDS_LIMIT),
        init: Some(true),
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::NO),
            maximum_retry_count: None,
        }),
        extra_hosts: spec
            .dev_mode
            .then(|| vec!["host.docker.internal:host-gateway".to_string()]),
        ..Default::default()
    };
    let mut labels = HashMap::from([
        (LABEL_INSTANCE.to_string(), inst.id.clone()),
        (LABEL_WORKSPACE.to_string(), inst.workspace_id.clone()),
        (LABEL_SCOPE.to_string(), tpl.scope.as_str().to_string()),
    ]);
    // With an injection the gateway becomes the entrypoint and wraps the image's
    // own entrypoint and command (or the override); the image's user and
    // working directory stay as they are. Cmd is emptied so Docker appends nothing.
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let (entrypoint, cmd, user, working_dir) = match &spec.inject {
        Some(inject) => {
            labels.insert(LABEL_INJECTED.to_string(), "true".to_string());
            let cmd = spec.cmd.as_deref().unwrap_or(&inject.image.cmd);
            (
                Some(wrap_entrypoint(&inject.image.entrypoint, cmd)),
                Some(Vec::new()),
                non_empty(&inject.image.user),
                non_empty(&inject.image.working_dir),
            )
        }
        None => (None, spec.cmd, None, None),
    };
    Ok(ContainerCreateBody {
        hostname: Some(inst.id.clone()),
        image: Some(spec.image.to_string()),
        env: Some(env_pairs(&inst.effective_env(&tpl.env))),
        labels: Some(labels),
        entrypoint,
        cmd,
        user,
        working_dir,
        host_config: Some(host_config),
        ..Default::default()
    })
}

impl DockerRuntime {
    /// `Runtime::create` with an optional command override.
    pub(crate) async fn create_with(
        &self,
        inst: &mut Instance,
        tpl: &Template,
        cmd: Option<Vec<String>>,
    ) -> Result<()> {
        if inst.id.is_empty() {
            return Err(Error::invalid("instance id must not be empty"));
        }
        let storage_path = self.storage_path(inst);
        let home = storage_path.join(HOME_DIR);
        storage::prepare_home(home.clone()).await?;
        let runtime = self.chosen_runtime().await?;
        let network = network_for(inst.network);
        if self.network_blocked(network) {
            return Err(Error::not_supported(netpolicy::blocked_network_hint(
                network,
            )));
        }
        let image = if inst.image_ref.is_empty() {
            tpl.image_ref.clone()
        } else {
            inst.image_ref.clone()
        };
        if image.is_empty() {
            return Err(Error::invalid("instance has no image reference"));
        }
        self.ensure_image_impl(&image).await?;
        let inject = self.injection_for(tpl, &image).await?;
        let injected = inject.is_some();
        let spec = ContainerSpec {
            runtime: &runtime,
            home: &home,
            image: &image,
            dev_mode: self.cfg.dev_mode,
            cmd,
            inject,
        };
        let body = create_body(inst, tpl, spec)?;
        let name = container_name(&inst.id);
        let options = CreateContainerOptions {
            name: Some(name.clone()),
            ..Default::default()
        };
        let created = match self
            .docker
            .create_container(Some(options.clone()), body.clone())
            .await
        {
            Ok(created) => created,
            Err(e) if error::is_conflict(&e) => {
                tracing::warn!(instance = %inst.id, container = %name, "a container with this name exists; replacing it");
                self.remove_container(&name).await?;
                self.docker
                    .create_container(Some(options), body)
                    .await
                    .map_err(|e| error::map(e, "container"))?
            }
            Err(e) => return Err(error::map(e, "container")),
        };
        for warning in &created.warnings {
            tracing::warn!(instance = %inst.id, %warning, "docker create warning");
        }
        inst.runtime_ref = created.id;
        inst.runtime = "docker".to_string();
        inst.image_ref = image.clone();
        inst.storage_path = storage_path.to_string_lossy().into_owned();
        tracing::info!(
            instance = %inst.id,
            container = %inst.runtime_ref,
            %image,
            %runtime,
            network = network_for(inst.network),
            shape = %inst.shape,
            scope = tpl.scope.as_str(),
            injected,
            "container created"
        );
        Ok(())
    }

    /// Start the container, find its IP and wait for the gateway port.
    pub(crate) async fn start_impl(&self, inst: &mut Instance) -> Result<RunInfo> {
        let reference = target(inst);
        match self.docker.start_container(&reference, None).await {
            Ok(()) => {}
            Err(e) if error::is_not_found(&e) => return Err(Error::not_found("container")),
            Err(e) => return Err(error::map(e, "container")),
        }
        let inspect = self.inspect(&reference).await?;
        if inst.runtime_ref.is_empty()
            && let Some(id) = inspect.id.as_deref().filter(|id| !id.is_empty())
        {
            inst.runtime_ref = id.to_string();
        }
        let network = network_for(inst.network);
        let Some(ip) = pick_ip(&inspect, network) else {
            let logs = self.tail_logs(&reference, 30).await;
            if !is_running(&inspect) {
                // Gone before it was inspected: the same template problem
                // `wait_for_port` reports when the exit happens a moment later.
                return Err(explain_missing_gateway(
                    Error::bad_gateway(format!(
                        "container {reference} exited with code {} right after starting; last logs: {logs}",
                        exit_code(&inspect)
                    )),
                    &inspect,
                ));
            }
            return Err(Error::internal(format!(
                "container {reference} has no IP address on {network}; last logs: {logs}"
            )));
        };
        match readiness_port(inst) {
            Some(port) => self
                .wait_for_port(inst, &reference, &ip, port)
                .await
                .map_err(|e| explain_missing_gateway(e, &inspect))?,
            None => {
                tracing::debug!(instance = %inst.id, "template has no default port; not waiting for the gateway")
            }
        }
        inst.ip = ip.clone();
        tracing::info!(instance = %inst.id, container = %reference, %ip, "instance started");
        Ok(RunInfo {
            ip,
            runtime_ref: inst.runtime_ref.clone(),
        })
    }

    /// Poll `ip:port` until it accepts a TCP connection. A container that exits
    /// first yields `bad_gateway` with its last log lines (a template problem the
    /// caller can show the user); one that never listens yields `timeout` after
    /// [`START_TIMEOUT`].
    async fn wait_for_port(
        &self,
        inst: &Instance,
        reference: &str,
        ip: &str,
        port: u16,
    ) -> Result<()> {
        let address = format!("{ip}:{port}");
        let started = Instant::now();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(&address)).await {
                Ok(Ok(_)) => {
                    let waited_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    tracing::debug!(instance = %inst.id, %address, attempts, waited_ms, "gateway port open");
                    return Ok(());
                }
                Ok(Err(e)) => {
                    tracing::trace!(instance = %inst.id, %address, err = %e, "gateway port not open yet")
                }
                Err(_) => {
                    tracing::trace!(instance = %inst.id, %address, "connect attempt timed out")
                }
            }
            if attempts.is_multiple_of(8)
                && let Ok(inspect) = self.inspect(reference).await
                && !is_running(&inspect)
            {
                let logs = self.tail_logs(reference, 30).await;
                return Err(Error::bad_gateway(format!(
                    "container {reference} exited with code {} before listening on port {port}; last logs: {logs}",
                    exit_code(&inspect)
                )));
            }
            if started.elapsed() >= START_TIMEOUT {
                let logs = self.tail_logs(reference, 30).await;
                tracing::warn!(instance = %inst.id, %address, "gateway did not start listening in time");
                return Err(Error::timeout(format!(
                    "instance {} did not listen on port {port} within {} s",
                    inst.id,
                    START_TIMEOUT.as_secs()
                ))
                .with_details(serde_json::json!({ "logs": logs, "port": port })));
            }
            tokio::time::sleep(START_POLL_INTERVAL).await;
        }
    }

    /// Stop the container with `grace` before SIGKILL. A missing or already
    /// stopped container is not an error.
    pub(crate) async fn stop_impl(&self, inst: &Instance, grace: Duration) -> Result<()> {
        let reference = target(inst);
        let t = i32::try_from(grace.as_secs()).unwrap_or(i32::MAX);
        match self
            .docker
            .stop_container(
                &reference,
                Some(StopContainerOptions {
                    signal: None,
                    t: Some(t),
                }),
            )
            .await
        {
            Ok(()) => {
                tracing::info!(instance = %inst.id, container = %reference, grace_s = t, "container stopped");
                Ok(())
            }
            Err(e) if error::is_not_found(&e) => {
                tracing::warn!(instance = %inst.id, container = %reference, "stop: container does not exist");
                Ok(())
            }
            Err(e) if error::is_conflict(&e) && error::message(&e).contains("not running") => {
                tracing::debug!(instance = %inst.id, container = %reference, "stop: container was not running");
                Ok(())
            }
            Err(e) => Err(error::map(e, "container")),
        }
    }

    /// Force-remove the container (by id and by name) and optionally the storage directory.
    pub(crate) async fn destroy_impl(&self, inst: &Instance, remove_storage: bool) -> Result<()> {
        let name = container_name(&inst.id);
        let reference = target(inst);
        let mut removed = self.remove_container(&reference).await?;
        if reference != name {
            removed |= self.remove_container(&name).await?;
        }
        self.forget_cpu_samples(&[reference.as_str(), name.as_str()]);
        if remove_storage {
            self.remove_storage_impl(inst).await?;
        }
        tracing::info!(instance = %inst.id, container_removed = removed, storage_removed = remove_storage, "instance destroyed");
        Ok(())
    }

    /// Apply CPU and memory limits live; disk is only recorded on the shape.
    /// When the live update fails on a running container (some OCI runtimes do not
    /// support it) the caller is asked to stop, resize again and start.
    pub(crate) async fn resize_impl(&self, inst: &mut Instance, shape: Shape) -> Result<()> {
        if shape.cpu == 0 || shape.memory == 0 {
            return Err(Error::invalid("cpu and memory must be positive"));
        }
        let reference = target(inst);
        let body = ContainerUpdateBody {
            memory: Some(memory_bytes(shape.memory)),
            memory_swap: Some(memory_bytes(shape.memory)),
            nano_cpus: Some(nano_cpus(shape.cpu)),
            ..Default::default()
        };
        match self.docker.update_container(&reference, body).await {
            Ok(()) => {
                tracing::info!(instance = %inst.id, container = %reference, from = %inst.shape, to = %shape, "container resized");
                inst.shape = shape;
                Ok(())
            }
            Err(e) if error::is_not_found(&e) => Err(Error::not_found("container")),
            Err(e) => {
                let running = self
                    .inspect(&reference)
                    .await
                    .map(|i| is_running(&i))
                    .unwrap_or(false);
                if running {
                    tracing::warn!(instance = %inst.id, err = %e, "live resize failed; a restart is required");
                    Err(requires_restart())
                } else {
                    Err(error::map(e, "container"))
                }
            }
        }
    }

    /// Replace the container with one built from `tpl`, keeping the storage directory.
    pub(crate) async fn recreate_impl(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        let name = container_name(&inst.id);
        let reference = target(inst);
        self.remove_container(&reference).await?;
        if reference != name {
            self.remove_container(&name).await?;
        }
        self.forget_cpu_samples(&[reference.as_str(), name.as_str()]);
        inst.runtime_ref.clear();
        inst.ip.clear();
        if !tpl.image_ref.is_empty() {
            inst.image_ref = tpl.image_ref.clone();
        }
        tracing::info!(instance = %inst.id, image = %inst.image_ref, "recreating container");
        self.create_with(inst, tpl, None).await
    }

    /// Force-remove a container. Returns whether it existed.
    pub(crate) async fn remove_container(&self, reference: &str) -> Result<bool> {
        match self
            .docker
            .remove_container(
                reference,
                Some(RemoveContainerOptions {
                    v: true,
                    force: true,
                    link: false,
                }),
            )
            .await
        {
            Ok(()) => {
                tracing::debug!(container = reference, "container removed");
                Ok(true)
            }
            Err(e) if error::is_not_found(&e) => Ok(false),
            Err(e) => Err(error::map(e, "container")),
        }
    }

    /// Inspect a container by id or name.
    pub(crate) async fn inspect(&self, reference: &str) -> Result<ContainerInspectResponse> {
        self.docker
            .inspect_container(reference, None)
            .await
            .map_err(|e| error::map(e, "container"))
    }

    /// The last `lines` log lines, for error messages; never fails.
    pub(crate) async fn tail_logs(&self, reference: &str, lines: u32) -> String {
        match self.logs_raw(reference, lines, 0).await {
            Ok(text) => text.trim_end().to_string(),
            Err(e) => format!("<logs unavailable: {}>", e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bollard::models::{EndpointSettings, NetworkSettings};
    use warren_core::{ENV_INSTANCE_ID, ENV_INSTANCE_TOKEN, TemplateScope};

    use super::*;
    use crate::inject::ImageCommand;

    fn fixture() -> (Instance, Template) {
        let tpl = Template {
            name: "warren-base".into(),
            image_ref: "warren/base:dev".into(),
            default_port: GATEWAY_PORT,
            mount_path: "/home/agent".into(),
            env: BTreeMap::from([
                ("FROM_TEMPLATE".to_string(), "1".to_string()),
                ("SHARED".to_string(), "tpl".to_string()),
            ]),
            ..Default::default()
        };
        let inst = Instance {
            id: "ab12cd34ef".into(),
            workspace_id: "ws_abcdefghij0123456789".into(),
            image_ref: "warren/base:dev".into(),
            shape: Shape {
                cpu: 2,
                memory: 4,
                disk: 4,
            },
            network: NetworkMode::AllowAll,
            default_port: GATEWAY_PORT,
            mount_path: "/home/agent".into(),
            instance_token: "secret-token".into(),
            env: BTreeMap::from([("SHARED".to_string(), "inst".to_string())]),
            ..Default::default()
        };
        (inst, tpl)
    }

    fn spec<'a>(home: &'a Path, dev_mode: bool) -> ContainerSpec<'a> {
        ContainerSpec {
            runtime: "runsc",
            home,
            image: "warren/base:dev",
            dev_mode,
            cmd: None,
            inject: None,
        }
    }

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    /// An n8n-like image: entrypoint and command, non-root user, working directory.
    fn injection() -> Injection {
        Injection {
            gateway_bin: PathBuf::from("/opt/warren/warren-gateway"),
            image: ImageCommand {
                entrypoint: strings(&["tini", "--", "/docker-entrypoint.sh"]),
                cmd: strings(&["n8n", "start"]),
                user: "node".into(),
                working_dir: "/home/node".into(),
            },
        }
    }

    #[test]
    fn units() {
        assert_eq!(memory_bytes(4), 4 * 1024 * 1024 * 1024);
        assert_eq!(nano_cpus(2), 2_000_000_000);
        assert_eq!(network_for(NetworkMode::AllowAll), NETWORK_PUBLIC);
        assert_eq!(network_for(NetworkMode::DenyAll), NETWORK_INTERNAL);
        assert_eq!(network_for(NetworkMode::Allowlist), NETWORK_INTERNAL);
        let env = BTreeMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "x=y".to_string()),
        ]);
        assert_eq!(
            env_pairs(&env),
            vec!["A=x=y".to_string(), "B=2".to_string()]
        );
    }

    #[test]
    fn readiness_port_follows_default_port() {
        let (mut inst, _) = fixture();
        assert_eq!(readiness_port(&inst), Some(GATEWAY_PORT));
        inst.default_port = 0;
        assert_eq!(readiness_port(&inst), None);
    }

    #[test]
    fn builds_the_create_body() {
        let (inst, tpl) = fixture();
        let home = PathBuf::from("/var/lib/warren/instances/ab12cd34ef/home");
        let body = create_body(&inst, &tpl, spec(&home, true)).unwrap();
        assert_eq!(body.hostname.as_deref(), Some("ab12cd34ef"));
        assert_eq!(body.image.as_deref(), Some("warren/base:dev"));
        assert!(body.cmd.is_none());
        let labels = body.labels.unwrap();
        assert_eq!(labels[LABEL_INSTANCE], "ab12cd34ef");
        assert_eq!(labels[LABEL_WORKSPACE], "ws_abcdefghij0123456789");
        assert_eq!(labels[LABEL_SCOPE], "system");
        assert!(
            !labels.contains_key(LABEL_INJECTED),
            "system images are not injected"
        );
        assert!(body.entrypoint.is_none() && body.user.is_none() && body.working_dir.is_none());
        let env = body.env.unwrap();
        assert!(env.contains(&format!("{ENV_INSTANCE_ID}=ab12cd34ef")));
        assert!(env.contains(&format!("{ENV_INSTANCE_TOKEN}=secret-token")));
        assert!(env.contains(&"FROM_TEMPLATE=1".to_string()));
        assert!(
            env.contains(&"SHARED=inst".to_string()),
            "instance env wins over template env"
        );
        let host = body.host_config.unwrap();
        assert_eq!(host.runtime.as_deref(), Some("runsc"));
        assert_eq!(host.memory, Some(4 << 30));
        assert_eq!(host.memory_swap, Some(4 << 30));
        assert_eq!(host.nano_cpus, Some(2_000_000_000));
        assert_eq!(
            host.binds,
            Some(vec![
                "/var/lib/warren/instances/ab12cd34ef/home:/home/agent".to_string()
            ])
        );
        assert_eq!(host.network_mode.as_deref(), Some(NETWORK_PUBLIC));
        assert_eq!(host.pids_limit, Some(PIDS_LIMIT));
        assert_eq!(host.init, Some(true));
        assert_eq!(
            host.restart_policy.unwrap().name,
            Some(RestartPolicyNameEnum::NO)
        );
        assert_eq!(
            host.extra_hosts,
            Some(vec!["host.docker.internal:host-gateway".to_string()])
        );
    }

    #[test]
    fn create_body_variants() {
        let (mut inst, mut tpl) = fixture();
        inst.network = NetworkMode::DenyAll;
        let home = PathBuf::from("/data/instances/ab12cd34ef/home");
        let body = create_body(&inst, &tpl, spec(&home, false)).unwrap();
        let host = body.host_config.unwrap();
        assert_eq!(host.network_mode.as_deref(), Some(NETWORK_INTERNAL));
        assert!(
            host.extra_hosts.is_none(),
            "no host.docker.internal outside dev mode"
        );

        tpl.mount_path.clear();
        inst.mount_path = "/workspace".into();
        let body = create_body(&inst, &tpl, spec(&home, false)).unwrap();
        assert_eq!(
            body.host_config.unwrap().binds,
            Some(vec![
                "/data/instances/ab12cd34ef/home:/workspace".to_string()
            ])
        );

        inst.mount_path = "relative".into();
        assert_eq!(
            create_body(&inst, &tpl, spec(&home, false))
                .unwrap_err()
                .code,
            warren_core::errors::CODE_INVALID_REQUEST
        );

        inst.mount_path = "/home/agent".into();
        inst.shape = Shape {
            cpu: 0,
            memory: 0,
            disk: 0,
        };
        let cmd = Some(vec!["sleep".to_string(), "1".to_string()]);
        let body = create_body(
            &inst,
            &tpl,
            ContainerSpec {
                cmd,
                ..spec(&home, false)
            },
        )
        .unwrap();
        assert_eq!(body.cmd, Some(vec!["sleep".to_string(), "1".to_string()]));
        let host = body.host_config.unwrap();
        assert!(host.memory.is_none() && host.memory_swap.is_none() && host.nano_cpus.is_none());
    }

    #[test]
    fn injected_body_wraps_the_image_and_keeps_everything_else() {
        let (inst, mut tpl) = fixture();
        tpl.scope = TemplateScope::Workspace;
        tpl.mount_path = "/home".into();
        let home = PathBuf::from("/var/lib/warren/instances/ab12cd34ef/home");
        let plain = create_body(&inst, &tpl, spec(&home, true)).unwrap();
        let body = create_body(
            &inst,
            &tpl,
            ContainerSpec {
                inject: Some(injection()),
                ..spec(&home, true)
            },
        )
        .unwrap();

        assert_eq!(
            body.entrypoint,
            Some(strings(&[
                "/warren/warren-gateway",
                "--serve",
                "--wrap",
                "--",
                "tini",
                "--",
                "/docker-entrypoint.sh",
                "n8n",
                "start"
            ]))
        );
        assert_eq!(body.cmd, Some(Vec::new()), "Cmd is emptied");
        assert_eq!(body.user.as_deref(), Some("node"), "the image user is kept");
        assert_eq!(
            body.working_dir.as_deref(),
            Some("/home/node"),
            "the image working directory is kept"
        );
        let labels = body.labels.clone().unwrap();
        assert_eq!(labels[LABEL_INJECTED], "true");
        assert_eq!(labels[LABEL_SCOPE], "workspace");
        assert_eq!(labels[LABEL_INSTANCE], "ab12cd34ef");
        assert_eq!(labels[LABEL_WORKSPACE], "ws_abcdefghij0123456789");
        let host = body.host_config.clone().unwrap();
        assert_eq!(
            host.binds,
            Some(strings(&[
                "/var/lib/warren/instances/ab12cd34ef/home:/home",
                "/opt/warren/warren-gateway:/warren/warren-gateway:ro",
            ]))
        );

        // Everything that is not the entrypoint, the gateway mount or the labels
        // is identical to the uninjected body.
        assert_eq!(body.env, plain.env);
        assert_eq!(body.hostname, plain.hostname);
        assert_eq!(body.image, plain.image);
        let plain_host = plain.host_config.unwrap();
        assert_eq!(host.runtime, plain_host.runtime);
        assert_eq!(host.memory, plain_host.memory);
        assert_eq!(host.memory_swap, plain_host.memory_swap);
        assert_eq!(host.nano_cpus, plain_host.nano_cpus);
        assert_eq!(host.network_mode, plain_host.network_mode);
        assert_eq!(host.init, plain_host.init);
        assert_eq!(host.restart_policy, plain_host.restart_policy);
        assert_eq!(host.extra_hosts, plain_host.extra_hosts);
    }

    #[test]
    fn injected_body_variants() {
        let (inst, mut tpl) = fixture();
        tpl.scope = TemplateScope::Workspace;
        let home = PathBuf::from("/var/lib/warren/instances/ab12cd34ef/home");

        // A command override replaces the image's Cmd inside the wrapped command.
        let body = create_body(
            &inst,
            &tpl,
            ContainerSpec {
                cmd: Some(strings(&["sleep", "1"])),
                inject: Some(injection()),
                ..spec(&home, false)
            },
        )
        .unwrap();
        assert_eq!(
            body.entrypoint,
            Some(strings(&[
                "/warren/warren-gateway",
                "--serve",
                "--wrap",
                "--",
                "tini",
                "--",
                "/docker-entrypoint.sh",
                "sleep",
                "1"
            ]))
        );
        assert_eq!(body.cmd, Some(Vec::new()));

        // alpine: no entrypoint, Cmd /bin/sh, root, workdir "/".
        let alpine = Injection {
            gateway_bin: PathBuf::from("/opt/warren/warren-gateway"),
            image: ImageCommand {
                cmd: strings(&["/bin/sh"]),
                working_dir: "/".into(),
                ..Default::default()
            },
        };
        let body = create_body(
            &inst,
            &tpl,
            ContainerSpec {
                inject: Some(alpine),
                ..spec(&home, false)
            },
        )
        .unwrap();
        assert_eq!(
            body.entrypoint,
            Some(strings(&[
                "/warren/warren-gateway",
                "--serve",
                "--wrap",
                "--",
                "/bin/sh"
            ]))
        );
        assert!(body.user.is_none(), "no user forced on a root image");
        assert_eq!(body.working_dir.as_deref(), Some("/"));

        // Nothing to wrap: the gateway alone, in serve mode.
        let bare = Injection {
            gateway_bin: PathBuf::from("/opt/warren/warren-gateway"),
            image: ImageCommand::default(),
        };
        let body = create_body(
            &inst,
            &tpl,
            ContainerSpec {
                inject: Some(bare),
                ..spec(&home, false)
            },
        )
        .unwrap();
        assert_eq!(
            body.entrypoint,
            Some(strings(&["/warren/warren-gateway", "--serve"]))
        );
        assert_eq!(body.cmd, Some(Vec::new()));
        assert!(body.user.is_none() && body.working_dir.is_none());
        assert_eq!(body.labels.unwrap()[LABEL_INJECTED], "true");
    }

    #[test]
    fn picks_the_ip() {
        let endpoint = |ip: &str| EndpointSettings {
            ip_address: Some(ip.to_string()),
            ..Default::default()
        };
        let inspect = ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                networks: Some(HashMap::from([
                    ("bridge".to_string(), endpoint("172.17.0.9")),
                    (NETWORK_PUBLIC.to_string(), endpoint("172.19.0.5")),
                ])),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            pick_ip(&inspect, NETWORK_PUBLIC).as_deref(),
            Some("172.19.0.5")
        );
        assert_eq!(
            pick_ip(&inspect, NETWORK_INTERNAL).as_deref(),
            Some("172.17.0.9"),
            "falls back to any network"
        );
        let empty = ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                networks: Some(HashMap::from([(NETWORK_PUBLIC.to_string(), endpoint(""))])),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(pick_ip(&empty, NETWORK_PUBLIC), None);
        assert_eq!(
            pick_ip(&ContainerInspectResponse::default(), NETWORK_PUBLIC),
            None
        );
        assert!(!is_running(&ContainerInspectResponse::default()));
    }
}
