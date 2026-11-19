//! warrend: Sandbox API, edge proxy, lifecycle and cron in one process.
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use warren_core::{
    Config, HmacSigner, InstanceType, KeyAuthenticator, MIN_IDLE_TIMEOUT_SEC, RealClock, Runtime,
    Runtimes, TemplateService,
};

/// Warren daemon.
#[derive(Parser, Debug)]
#[command(name = "warrend", version, about)]
struct Args {
    /// Listen address of the single HTTP listener.
    #[arg(long, env = "WARREN_LISTEN", default_value = ":7337")]
    listen: String,
    /// Base domain: api.<base>, <id>.<base>, <id>-<port>.<base>.
    #[arg(long, env = "WARREN_BASE_DOMAIN", default_value = "warren.localhost")]
    base_domain: String,
    /// Scheme clients use in URLs.
    #[arg(long, env = "WARREN_PUBLIC_SCHEME", default_value = "http")]
    public_scheme: String,
    /// Port clients use in URLs; 0 omits it.
    #[arg(long, env = "WARREN_PUBLIC_PORT", default_value_t = 7337)]
    public_port: u16,
    /// Data directory (sqlite, instances, backups, secret).
    #[arg(long, env = "WARREN_DATA_DIR", default_value = "/var/lib/warren")]
    data_dir: PathBuf,
    /// Docker runtime for default instances: auto | runsc | runc, or none to run
    /// without Docker (a macOS host serving only mac instances).
    #[arg(long, env = "WARREN_RUNTIME", default_value = "auto")]
    runtime: String,
    /// Region label reported on instances.
    #[arg(long, env = "WARREN_REGION", default_value = "local")]
    region: String,
    /// Development mode: 5 s minimum idle timeout, host.docker.internal, no signup
    /// rate limit for loopback clients.
    #[arg(long, env = "WARREN_DEV", default_value_t = false)]
    dev: bool,
    /// Whether `POST /v1/signup` is open; when off it answers 404 and workspaces
    /// are created some other way.
    #[arg(long, env = "WARREN_SIGNUP_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    signup_enabled: bool,
    /// Let instances reach private networks (RFC 1918, link-local, the host);
    /// off for a hosted service.
    #[arg(long, env = "WARREN_ALLOW_PRIVATE_EGRESS", default_value_t = false)]
    allow_private_egress: bool,
    /// Instances one workspace may keep awake at once; unlimited when unset.
    #[arg(long, env = "WARREN_MAX_AWAKE_INSTANCES", value_name = "N")]
    max_awake_instances: Option<u32>,
    /// Minimum idle timeout in seconds (overrides the dev default when set).
    #[arg(long, env = "WARREN_MIN_IDLE_SECONDS")]
    min_idle_seconds: Option<u32>,
    /// Enable vm-type instances (needs root, /dev/kvm, firecracker and a kernel).
    #[arg(long, env = "WARREN_ENABLE_FIRECRACKER", default_value_t = false)]
    firecracker: bool,
    #[arg(
        long,
        env = "WARREN_FIRECRACKER_BIN",
        default_value = "/usr/local/bin/firecracker"
    )]
    firecracker_bin: PathBuf,
    #[arg(long, env = "WARREN_KERNEL_IMAGE")]
    kernel: Option<PathBuf>,
    /// Static warren-gateway binary injected into registered images; found next to
    /// warrend or in /usr/local/bin when unset.
    #[arg(long, env = "WARREN_GATEWAY_BIN")]
    gateway_bin: Option<PathBuf>,
    /// Enable mac-type instances (macOS VMs through Lume on an Apple silicon
    /// host): the name of the stopped Lume VM to clone them from.
    #[arg(long, env = "WARREN_MAC_BASE_VM", value_name = "VM")]
    mac_base_vm: Option<String>,
    /// Lume API server that hosts mac instances; started as `lume serve` when
    /// it is local and not running.
    #[arg(long, env = "WARREN_LUME_API", default_value = "http://127.0.0.1:7777")]
    lume_api: String,
    /// The lume executable.
    #[arg(long, env = "WARREN_LUME_BIN", default_value = "lume")]
    lume_bin: PathBuf,
    /// Start mac instances with Lume's VNC server on (host-only, random password).
    #[arg(long, env = "WARREN_MAC_VNC", default_value_t = false)]
    mac_vnc: bool,
    /// HMAC secret for signed URLs; generated into the data dir when unset.
    #[arg(long, env = "WARREN_SECRET")]
    secret: Option<String>,
    /// Tag of the built-in template images.
    #[arg(long, env = "WARREN_IMAGE_TAG", default_value = "dev")]
    image_tag: String,
    /// Log filter (tracing EnvFilter syntax).
    #[arg(long, env = "WARREN_LOG", default_value = "info")]
    log: String,
}

impl Args {
    fn into_config(self) -> Config {
        let dev = self.dev;
        Config {
            listen: self.listen,
            base_domain: self.base_domain,
            public_scheme: self.public_scheme,
            public_port: self.public_port,
            data_dir: self.data_dir,
            runtime: self.runtime,
            region: self.region,
            dev_mode: dev,
            signup_enabled: self.signup_enabled,
            allow_private_egress: self.allow_private_egress,
            max_awake_instances: self.max_awake_instances,
            min_idle_timeout_sec: self.min_idle_seconds.unwrap_or(if dev {
                5
            } else {
                MIN_IDLE_TIMEOUT_SEC
            }),
            enable_firecracker: self.firecracker,
            firecracker_bin: self.firecracker_bin,
            kernel_image: self.kernel,
            gateway_bin: Config::resolve_gateway_bin(self.gateway_bin),
            mac_base_vm: self.mac_base_vm.filter(|v| !v.trim().is_empty()),
            lume_api_url: self.lume_api,
            lume_bin: self.lume_bin,
            mac_vnc: self.mac_vnc,
            runtime_names: Vec::new(),
            secret: self.secret.unwrap_or_default(),
            image_tag: self.image_tag,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&args.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();
    let mut cfg = args.into_config();

    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("create data dir {}", cfg.data_dir.display()))?;
    if cfg.secret.is_empty() {
        cfg.secret = load_or_create_secret(&cfg.data_dir.join("secret"))?;
    }

    let store =
        warren_store::SqliteStore::open(&cfg.data_dir.join("warren.db")).context("open store")?;
    let clock = Arc::new(RealClock);

    let mut runtimes: HashMap<InstanceType, Arc<dyn Runtime>> = HashMap::new();
    if cfg.runtime == "none" {
        tracing::info!("docker runtime disabled (--runtime none); default instances are off");
    } else {
        let docker = warren_runtime_docker::DockerRuntime::new(cfg.clone())
            .await
            .context("docker runtime")?;
        docker.ready().await.context("docker runtime not ready")?;
        runtimes.insert(InstanceType::Default, Arc::new(docker));
    }
    if cfg.enable_firecracker {
        match warren_runtime_firecracker::FirecrackerRuntime::new(cfg.clone()).await {
            Ok(fc) => match fc.ready().await {
                Ok(()) => {
                    tracing::info!("firecracker runtime enabled");
                    runtimes.insert(InstanceType::Vm, Arc::new(fc));
                }
                Err(e) => {
                    tracing::warn!(err = %e, "firecracker runtime not ready; vm instances disabled")
                }
            },
            Err(e) => tracing::warn!(err = %e, "firecracker runtime disabled"),
        }
    }
    let shutdown = CancellationToken::new();
    let mut tasks = tokio::task::JoinSet::new();
    if cfg.mac_base_vm.is_some() {
        match warren_runtime_mac::MacRuntime::new(cfg.clone()) {
            Ok(mac) => match mac.ready().await {
                Ok(()) => {
                    tracing::info!("lume runtime enabled; mac instances available");
                    tasks.spawn({
                        let cfg = cfg.clone();
                        let client = mac.lume().clone();
                        let s = shutdown.clone();
                        async move {
                            warren_runtime_mac::serve::supervise(cfg, client, s).await;
                            Ok(())
                        }
                    });
                    runtimes.insert(InstanceType::Mac, Arc::new(mac));
                }
                Err(e) => {
                    tracing::warn!(err = %e, "lume runtime not ready; mac instances disabled")
                }
            },
            Err(e) => tracing::warn!(err = %e, "lume runtime disabled"),
        }
    }
    if runtimes.is_empty() {
        anyhow::bail!(
            "no runtime came up: enable Docker (--runtime auto) or configure --mac-base-vm on a Mac"
        );
    }
    cfg.runtime_names = {
        let mut names: Vec<String> = runtimes.values().map(|r| r.name().to_string()).collect();
        names.sort();
        names.dedup();
        names
    };
    let runtimes: Runtimes = Arc::new(runtimes);

    let templates = warren_templates::Templates::new(store.clone(), runtimes.clone(), cfg.clone());
    templates
        .ensure_system_templates()
        .await
        .context("system templates")?;
    let lifecycle = warren_lifecycle::Manager::new(warren_lifecycle::Deps {
        store: store.clone(),
        runtimes: runtimes.clone(),
        templates: templates.clone(),
        config: cfg.clone(),
        clock: clock.clone(),
    })
    .context("lifecycle")?;
    let scheduler =
        warren_cron::Scheduler::new(store.clone(), lifecycle.clone(), cfg.clone(), clock.clone());
    let auth = Arc::new(KeyAuthenticator::new(store.clone(), clock.clone()));
    let signer = Arc::new(HmacSigner::new(&cfg.secret, clock.clone()));

    let api = warren_api::router(warren_api::Deps {
        store: store.clone(),
        lifecycle: lifecycle.clone(),
        templates: templates.clone(),
        auth: auth.clone(),
        signer: signer.clone(),
        config: cfg.clone(),
        clock: clock.clone(),
    });
    let dashboard = warren_dashboard::router(&cfg);
    let root = warren_edge::router(warren_edge::Deps {
        config: cfg.clone(),
        api,
        dashboard,
        lifecycle: lifecycle.clone(),
        auth,
        signer,
    });

    tasks.spawn({
        let lc = lifecycle.clone();
        let s = shutdown.clone();
        async move {
            lc.run(s)
                .await
                .map_err(|e| anyhow::anyhow!("lifecycle: {e}"))
        }
    });
    tasks.spawn({
        let s = shutdown.clone();
        async move {
            scheduler
                .run(s)
                .await
                .map_err(|e| anyhow::anyhow!("cron: {e}"))
        }
    });

    let addr = normalize_listen(&cfg.listen);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    match &cfg.gateway_bin {
        Some(p) => {
            tracing::info!(path = %p.display(), "gateway injection enabled for registered images")
        }
        None => tracing::warn!(
            "no warren-gateway binary found; registered images that do not ship the gateway will not start (set --gateway-bin)"
        ),
    }
    tracing::info!(addr = %addr, api = %cfg.api_url(), base_domain = %cfg.base_domain, dev_mode = cfg.dev_mode, version = %cfg.version, "warrend listening");
    let server_shutdown = shutdown.clone();
    tasks.spawn(async move {
        axum::serve(
            listener,
            root.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
        .await
        .map_err(|e| anyhow::anyhow!("http: {e}"))
    });

    tokio::select! {
        _ = wait_for_signal() => tracing::info!("shutting down"),
        Some(res) = tasks.join_next() => match res {
            Ok(Ok(())) => tracing::warn!("a background task finished early"),
            Ok(Err(e)) => tracing::error!(err = %e, "background task failed"),
            Err(e) => tracing::error!(err = %e, "background task panicked"),
        },
    }
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("sigterm handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}

/// Accept ":7337" as well as "0.0.0.0:7337"; bind dual-stack when no host is given.
fn normalize_listen(listen: &str) -> String {
    if listen.starts_with(':') {
        format!("[::]{listen}")
    } else {
        listen.to_string()
    }
}

fn load_or_create_secret(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(s) = std::fs::read_to_string(path) {
        let s = s.trim().to_string();
        if s.len() >= 32 {
            return Ok(s);
        }
    }
    let s = warren_core::ids::new_secret();
    std::fs::write(path, format!("{s}\n"))
        .with_context(|| format!("write secret {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(s)
}
