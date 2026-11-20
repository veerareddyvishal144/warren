//! `warren up`, `warren down` and `warren status`: the Warren daemon on this
//! machine, run as a launchd LaunchAgent (macOS), a systemd user service
//! (Linux) or, with `--no-service`, as a background process whose pid is
//! recorded in `<data>/warrend.pid`.
//!
//! `up` writes what it set up to `daemon.json` next to the CLI config file, so
//! `down`, `status` and `doctor` know the port, the data directory and how the
//! daemon is run. The service definitions are rendered from the templates in
//! `packaging/` (embedded at build time) with absolute paths, because neither
//! launchd nor systemd expands `~`.
//!
//! The helpers at the bottom (finding binaries, versions, ports, Lume and
//! Docker probes) are shared with `doctor` and `mac init`.

use std::{
    fs,
    io::{IsTerminal, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use warren_cli::{
    EXIT_OK, Failure,
    cli::{DownArgs, RuntimeChoice, UpArgs},
    config::{FileConfig, Settings, normalize_api_url},
    util::find_in_path,
};

use super::{Ctx, api::Api};

/// launchd label of the LaunchAgent (`packaging/launchd/dev.warren.daemon.plist`).
pub const LAUNCHD_LABEL: &str = "dev.warren.daemon";
/// Name of the systemd user unit.
pub const SYSTEMD_UNIT: &str = "warrend";
/// Port when `--port` is not given.
pub const DEFAULT_PORT: u16 = 7337;
/// Base domain when `--base-domain` is not given.
pub const DEFAULT_BASE_DOMAIN: &str = "warren.localhost";
/// Lume base VM when `--mac-base-vm` is not given.
pub const DEFAULT_MAC_BASE_VM: &str = "warren-macos-26";
/// Oldest Lume the macOS runtime works with.
pub const LUME_MIN_VERSION: (u32, u32, u32) = (0, 7, 2);
/// Default address of Lume's API server (`lume serve`).
pub const DEFAULT_LUME_API: &str = "http://127.0.0.1:7777";
/// How to get Lume, printed when it is missing or too old. Nothing is downloaded by the CLI.
pub const LUME_INSTALL_HINT: &str = "/bin/bash -c \"$(curl -fsSL https://cua.ai/lume/install.sh)\"    # or a release tarball from https://github.com/trycua/cua/releases (lume-v0.7.2 or newer)";
/// How to get gVisor on a Linux host.
pub const GVISOR_HINT: &str = "sudo scripts/install-gvisor.sh (from a checkout; see https://gvisor.dev/docs/user_guide/install/) registers runsc with Docker; without it default instances run under runc";
/// Time `up` waits for `GET /v1/health`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(120);
/// Name of the state file next to the CLI config.
const STATE_FILE: &str = "daemon.json";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LAUNCHD_TEMPLATE: &str =
    include_str!("../../../../packaging/launchd/dev.warren.daemon.plist");
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SYSTEMD_TEMPLATE: &str = include_str!("../../../../packaging/systemd/warrend.service");

/// How the daemon is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    /// A launchd LaunchAgent in the user's `gui/<uid>` domain.
    Launchd,
    /// A systemd user unit (`systemctl --user`).
    Systemd,
    /// A background process started by `warren up --no-service`.
    Process,
}

impl ServiceKind {
    fn label(self) -> String {
        match self {
            ServiceKind::Launchd => format!("launchd {LAUNCHD_LABEL}"),
            ServiceKind::Systemd => format!("systemd --user {SYSTEMD_UNIT}"),
            ServiceKind::Process => "background process".to_string(),
        }
    }
}

/// What `warren up` set up, stored in `daemon.json` next to the config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    pub service: ServiceKind,
    pub port: u16,
    pub listen: String,
    pub base_domain: String,
    pub api_url: String,
    pub dashboard_url: String,
    pub data_dir: PathBuf,
    pub warrend: PathBuf,
    pub runtime: String,
    #[serde(default)]
    pub mac_base_vm: Option<String>,
    #[serde(default)]
    pub dev: bool,
    /// Where the daemon's output goes (a file; `journalctl --user -u warrend` for systemd).
    pub log_path: PathBuf,
    #[serde(default)]
    pub pid_file: Option<PathBuf>,
    /// The plist or unit file written by `up`.
    #[serde(default)]
    pub unit_path: Option<PathBuf>,
    pub created_at: i64,
}

impl DaemonState {
    /// `<config dir>/daemon.json`; `None` when no config location is known.
    pub fn path(ctx: &Ctx) -> Option<PathBuf> {
        ctx.config_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.join(STATE_FILE))
    }

    /// The recorded state, `None` when `up` never ran here.
    pub fn load(ctx: &Ctx) -> Result<Option<DaemonState>, Failure> {
        let Some(path) = Self::path(ctx) else {
            return Ok(None);
        };
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Failure::local(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Failure::local(format!("{}: {e}", path.display()))),
        }
    }

    fn save(&self, ctx: &Ctx) -> Result<PathBuf, Failure> {
        let path = Self::path(ctx).ok_or_else(|| {
            Failure::local("neither XDG_CONFIG_HOME nor HOME is set; pass --config PATH")
        })?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| Failure::local(format!("{}: {e}", dir.display())))?;
        }
        let body = serde_json::to_string_pretty(self).map_err(|e| Failure::local(e.to_string()))?;
        fs::write(&path, format!("{body}\n"))
            .map_err(|e| Failure::local(format!("{}: {e}", path.display())))?;
        Ok(path)
    }

    fn remove(ctx: &Ctx) {
        if let Some(path) = Self::path(ctx) {
            let _ = fs::remove_file(path);
        }
    }

    /// The pid of the running daemon, `None` when it is not running.
    pub fn running_pid(&self) -> Option<i32> {
        match self.service {
            ServiceKind::Launchd => launchd_pid(),
            ServiceKind::Systemd => systemd_main_pid(),
            ServiceKind::Process => self
                .pid_file
                .as_deref()
                .and_then(read_pid)
                .filter(|pid| pid_alive(*pid)),
        }
    }
}

/// Everything `up` decided before touching the system.
#[derive(Debug, Clone)]
struct UpPlan {
    warrend: PathBuf,
    port: u16,
    listen: String,
    base_domain: String,
    data_dir: PathBuf,
    runtime: String,
    mac_base_vm: Option<String>,
    lume_bin: Option<PathBuf>,
    dev: bool,
    api_url: String,
    dashboard_url: String,
}

impl UpPlan {
    /// The warrend command line, without the binary.
    fn warrend_args(&self) -> Vec<String> {
        let mut a = vec![
            "--runtime".to_string(),
            self.runtime.clone(),
            "--data-dir".to_string(),
            self.data_dir.display().to_string(),
            "--listen".to_string(),
            self.listen.clone(),
            "--public-port".to_string(),
            self.port.to_string(),
            "--base-domain".to_string(),
            self.base_domain.clone(),
        ];
        if let Some(vm) = &self.mac_base_vm {
            a.push("--mac-base-vm".into());
            a.push(vm.clone());
        }
        if let Some(lume) = &self.lume_bin {
            a.push("--lume-bin".into());
            a.push(lume.display().to_string());
        }
        if self.dev {
            a.push("--dev".into());
        }
        a
    }
}

// ----- up ---------------------------------------------------------------------------

/// `warren up`
pub async fn up(ctx: &Ctx, args: UpArgs) -> Result<i32, Failure> {
    let home = home_dir()?;
    let warrend = find_warrend(args.warrend_bin.as_deref())?;
    let port = args.port.unwrap_or(DEFAULT_PORT);
    let base_domain = args
        .base_domain
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_DOMAIN.to_string());
    let data_dir = absolute(args.data_dir.unwrap_or_else(|| default_data_dir(&home)));
    let listen = args
        .listen
        .clone()
        .unwrap_or_else(|| format!("127.0.0.1:{port}"));
    let mut notes: Vec<String> = Vec::new();

    // Platform preflight.
    let docker = docker_ok();
    let runtime = resolve_runtime(args.runtime, docker)?;
    let mut lume_bin = None;
    let mut mac_base_vm = None;
    if cfg!(target_os = "macos") {
        let (bin, version) = lume_version()
            .ok_or_else(|| Failure::local(format!("lume is not installed (the macOS runtime needs Lume {}); install it with:\n  {LUME_INSTALL_HINT}", fmt_version(LUME_MIN_VERSION))))?;
        if !version_at_least(&version, LUME_MIN_VERSION) {
            return Err(Failure::local(format!(
                "lume {version} is too old; {} or newer is required. Upgrade with:\n  {LUME_INSTALL_HINT}",
                fmt_version(LUME_MIN_VERSION)
            )));
        }
        let vm = args
            .mac_base_vm
            .clone()
            .unwrap_or_else(|| DEFAULT_MAC_BASE_VM.to_string());
        if lume_vm_status(&bin, &vm).is_none() {
            notes.push(format!(
                "the base VM {vm} does not exist yet; mac instances stay disabled until `warren mac init` has built it"
            ));
        }
        lume_bin = Some(bin);
        mac_base_vm = Some(vm);
    } else if runtime != "none" && docker && docker_has_runsc() == Some(false) {
        notes.push(format!("Docker has no runsc runtime; {GVISOR_HINT}"));
    }

    // A daemon this command set up before is replaced.
    if let Some(prev) = DaemonState::load(ctx)?
        && let Some(pid) = prev.running_pid()
    {
        eprintln!(
            "stopping the daemon from the previous `warren up` ({}, pid {pid})",
            prev.service.label()
        );
        stop_service(&prev)?;
    }
    if port_in_use(port) {
        let holder = who_listens(port).unwrap_or_else(|| "another process".to_string());
        let probe = Api::new(&Settings {
            api_url: api_url_for(&base_domain, port),
            api_key: None,
        })?;
        let hint = match probe.health().await {
            Ok(h) => format!(
                "it is a Warren daemon (version {}) that `warren up` does not manage; use it with `warren login --api-url {}` or `warren signup --api-url ...`, or pick another port",
                h.get("version").and_then(Value::as_str).unwrap_or("?"),
                api_url_for(&base_domain, port)
            ),
            Err(_) => "pass --port N to use another port".to_string(),
        };
        return Err(Failure::local(format!(
            "port {port} is in use by {holder}; {hint}"
        )));
    }

    fs::create_dir_all(&data_dir)
        .map_err(|e| Failure::local(format!("creating {}: {e}", data_dir.display())))?;
    let plan = UpPlan {
        warrend,
        port,
        listen,
        base_domain: base_domain.clone(),
        data_dir,
        runtime,
        mac_base_vm,
        lume_bin,
        dev: args.dev,
        api_url: api_url_for(&base_domain, port),
        dashboard_url: dashboard_url_for(&base_domain, port),
    };

    let state = if args.no_service {
        start_process(&plan)?
    } else {
        install_service(ctx, &plan, &home, docker)?
    };
    let state_path = state.save(ctx)?;

    eprintln!(
        "waiting for warrend at {} (up to {} s)",
        plan.api_url,
        HEALTH_TIMEOUT.as_secs()
    );
    let api = Api::new(&Settings {
        api_url: plan.api_url.clone(),
        api_key: None,
    })?;
    let health = wait_for_health(&api, &state).await?;
    let key = ensure_key(ctx, &plan.api_url, &api).await;

    let runtimes: Vec<String> = health
        .get("runtimes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ctx.json {
        let (key_status, key_prefix) = match &key {
            KeyOutcome::Existing(k) => ("existing", Some(key_prefix(k))),
            KeyOutcome::Created(k) => ("created", Some(key_prefix(k))),
            KeyOutcome::Unavailable(_) => ("none", None),
        };
        println!(
            "{}",
            warren_cli::output::to_json_pretty(&json!({
                "api_url": plan.api_url,
                "dashboard_url": plan.dashboard_url,
                "data_dir": plan.data_dir,
                "service": state.service,
                "pid": state.running_pid(),
                "log": state.log_path,
                "state_file": state_path,
                "config": ctx.config_path,
                "key": key_status,
                "key_prefix": key_prefix,
                "version": health.get("version"),
                "runtimes": runtimes,
                "notes": notes,
            }))
        );
        return Ok(EXIT_OK);
    }

    println!("Warren is up.");
    println!("  dashboard  {}", plan.dashboard_url);
    println!("  API        {}", plan.api_url);
    match &key {
        KeyOutcome::Existing(k) => println!(
            "  key        {} ({}, already there)",
            display_path(ctx.config_path.as_deref()),
            key_prefix(k)
        ),
        KeyOutcome::Created(k) => println!(
            "  key        {} ({}, new workspace)",
            display_path(ctx.config_path.as_deref()),
            key_prefix(k)
        ),
        KeyOutcome::Unavailable(why) => println!("  key        none: {why}"),
    }
    println!("  data       {}", plan.data_dir.display());
    println!(
        "  service    {}{}",
        state.service.label(),
        state
            .running_pid()
            .map(|p| format!(" (pid {p})"))
            .unwrap_or_default()
    );
    println!("  log        {}", state.log_path.display());
    println!(
        "  daemon     warrend {}, runtimes: {}",
        health.get("version").and_then(Value::as_str).unwrap_or("?"),
        if runtimes.is_empty() {
            "none".to_string()
        } else {
            runtimes.join(", ")
        }
    );
    for n in &notes {
        println!("  note       {n}");
    }
    println!("Next:");
    if cfg!(target_os = "macos")
        && !runtimes.iter().any(|r| r == "lume")
        && plan.mac_base_vm.is_some()
    {
        println!("  warren mac init          # build the macOS base VM, then `warren up` again");
    }
    println!(
        "  warren connect claude    # register the MCP server with Claude Code (codex, cursor, windsurf, caret, opencode, gemini too)"
    );
    if runtimes.iter().any(|r| r == "lume") {
        println!(
            "  warren run --type mac -- claude    # Claude Code in a fresh macOS instance bound to this directory"
        );
    }
    if runtimes.iter().any(|r| r == "docker") {
        println!(
            "  warren run --template warren-claude -- claude    # Claude Code in a fresh Linux instance bound to this directory"
        );
    }
    println!("  warren doctor            # check the setup");
    Ok(EXIT_OK)
}

/// Map `--runtime` to warrend's `--runtime` value.
fn resolve_runtime(choice: RuntimeChoice, docker: bool) -> Result<String, Failure> {
    Ok(match choice {
        RuntimeChoice::None => "none".to_string(),
        RuntimeChoice::Docker if docker => "auto".to_string(),
        RuntimeChoice::Docker => {
            return Err(Failure::local(
                "--runtime docker: `docker info` fails; start Docker (or add yourself to the docker group) and retry, or use --runtime none",
            ));
        }
        RuntimeChoice::Auto if docker => "auto".to_string(),
        RuntimeChoice::Auto if cfg!(target_os = "macos") => "none".to_string(),
        RuntimeChoice::Auto => {
            return Err(Failure::local(
                "Docker is not reachable (`docker info` fails) and nothing else runs instances on Linux; install Docker (https://docs.docker.com/engine/install/), make sure your user can use it, or pass --runtime none for a daemon with no instance types",
            ));
        }
    })
}

/// Start warrend detached, output to `<data>/warrend.log`, pid in `<data>/warrend.pid`.
fn start_process(plan: &UpPlan) -> Result<DaemonState, Failure> {
    use std::os::unix::process::CommandExt as _;
    let log_path = plan.data_dir.join("warrend.log");
    let pid_file = plan.data_dir.join("warrend.pid");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| Failure::local(format!("{}: {e}", log_path.display())))?;
    let err = log
        .try_clone()
        .map_err(|e| Failure::local(format!("{}: {e}", log_path.display())))?;
    let child = Command::new(&plan.warrend)
        .args(plan.warrend_args())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| Failure::local(format!("starting {}: {e}", plan.warrend.display())))?;
    let pid = child.id();
    fs::write(&pid_file, format!("{pid}\n"))
        .map_err(|e| Failure::local(format!("{}: {e}", pid_file.display())))?;
    eprintln!(
        "started {} (pid {pid}), output in {}",
        plan.warrend.display(),
        log_path.display()
    );
    Ok(DaemonState {
        service: ServiceKind::Process,
        port: plan.port,
        listen: plan.listen.clone(),
        base_domain: plan.base_domain.clone(),
        api_url: plan.api_url.clone(),
        dashboard_url: plan.dashboard_url.clone(),
        data_dir: plan.data_dir.clone(),
        warrend: plan.warrend.clone(),
        runtime: plan.runtime.clone(),
        mac_base_vm: plan.mac_base_vm.clone(),
        dev: plan.dev,
        log_path,
        pid_file: Some(pid_file),
        unit_path: None,
        created_at: now(),
    })
}

/// Install and start the platform service.
fn install_service(
    ctx: &Ctx,
    plan: &UpPlan,
    home: &Path,
    docker: bool,
) -> Result<DaemonState, Failure> {
    #[cfg(target_os = "macos")]
    {
        let _ = (ctx, docker);
        install_launchd(plan, home)
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd(ctx, plan, home, docker)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (ctx, plan, home, docker);
        Err(Failure::local(
            "no service manager is supported on this platform; use --no-service",
        ))
    }
}

/// Values filled into the launchd template.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct LaunchdParams<'a> {
    pub warrend: &'a Path,
    pub home: &'a Path,
    pub base_vm: &'a str,
    pub args: &'a [String],
    pub working_dir: &'a Path,
    pub log_path: &'a Path,
    /// Directories prepended to the PATH launchd gives the daemon (where `lume` lives).
    pub extra_path: &'a [PathBuf],
}

/// Fill the LaunchAgent template: placeholders, the `ProgramArguments`
/// array, the working directory and the log paths.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn render_launchd_plist(template: &str, p: &LaunchdParams<'_>) -> String {
    let home = p.home.display().to_string();
    let mut out = template
        .replace("__WARREND__", &xml_escape(&p.warrend.display().to_string()))
        .replace("__BASE_VM__", &xml_escape(p.base_vm))
        .replace(
            "<string>__HOME__/.warren</string>",
            &format!(
                "<string>{}</string>",
                xml_escape(&p.working_dir.display().to_string())
            ),
        );
    // The template has a fixed argument list; replace it with ours.
    if let Some(start) = out.find("<key>ProgramArguments</key>")
        && let Some(open) = out[start..].find("<array>")
    {
        let open = start + open + "<array>".len();
        if let Some(close) = out[open..].find("</array>") {
            let close = open + close;
            let mut body = String::from("\n");
            body.push_str(&format!(
                "\t\t<string>{}</string>\n",
                xml_escape(&p.warrend.display().to_string())
            ));
            for a in p.args {
                body.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(a)));
            }
            body.push('\t');
            out.replace_range(open..close, &body);
        }
    }
    // Log paths: both streams to the same file.
    out = out.replace(
        "__HOME__/Library/Logs/Warren/warrend.log",
        &xml_escape(&p.log_path.display().to_string()),
    );
    // PATH: lume may live outside the template's directories (~/.local/bin).
    if !p.extra_path.is_empty() {
        let prefix: Vec<String> = p
            .extra_path
            .iter()
            .map(|d| xml_escape(&d.display().to_string()))
            .collect();
        out = out.replace(
            "<string>/opt/homebrew/bin:",
            &format!("<string>{}:/opt/homebrew/bin:", prefix.join(":")),
        );
    }
    out.replace("__HOME__", &xml_escape(&home))
}

#[cfg(target_os = "macos")]
fn install_launchd(plan: &UpPlan, home: &Path) -> Result<DaemonState, Failure> {
    let agents = home.join("Library/LaunchAgents");
    let logs = home.join("Library/Logs/Warren");
    for d in [&agents, &logs] {
        fs::create_dir_all(d).map_err(|e| Failure::local(format!("{}: {e}", d.display())))?;
    }
    let log_path = logs.join("warrend.log");
    let plist_path = agents.join(format!("{LAUNCHD_LABEL}.plist"));
    let extra: Vec<PathBuf> = plan
        .lume_bin
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    let args = plan.warrend_args();
    let body = render_launchd_plist(
        LAUNCHD_TEMPLATE,
        &LaunchdParams {
            warrend: &plan.warrend,
            home,
            base_vm: plan.mac_base_vm.as_deref().unwrap_or(DEFAULT_MAC_BASE_VM),
            args: &args,
            working_dir: &plan.data_dir,
            log_path: &log_path,
            extra_path: &extra,
        },
    );
    fs::write(&plist_path, body)
        .map_err(|e| Failure::local(format!("{}: {e}", plist_path.display())))?;
    if let Err(e) = run_ok(
        "plutil",
        &["-lint", "-s", &plist_path.display().to_string()],
    ) {
        return Err(Failure::local(format!(
            "the rendered {} is not a valid plist: {e}",
            plist_path.display()
        )));
    }
    if launchd_loaded() {
        launchctl_bootout()?;
    }
    let domain = format!("gui/{}", uid());
    run_ok(
        "launchctl",
        &["bootstrap", &domain, &plist_path.display().to_string()],
    )
    .map_err(|e| Failure::local(format!("launchctl bootstrap {domain}: {e}")))?;
    eprintln!("loaded {LAUNCHD_LABEL} from {}", plist_path.display());
    Ok(DaemonState {
        service: ServiceKind::Launchd,
        port: plan.port,
        listen: plan.listen.clone(),
        base_domain: plan.base_domain.clone(),
        api_url: plan.api_url.clone(),
        dashboard_url: plan.dashboard_url.clone(),
        data_dir: plan.data_dir.clone(),
        warrend: plan.warrend.clone(),
        runtime: plan.runtime.clone(),
        mac_base_vm: plan.mac_base_vm.clone(),
        dev: plan.dev,
        log_path,
        pid_file: None,
        unit_path: Some(plist_path),
        created_at: now(),
    })
}

/// Values filled into the systemd templates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct SystemdParams<'a> {
    pub warrend: &'a Path,
    pub data_dir: &'a Path,
    pub listen: &'a str,
    pub port: u16,
    pub base_domain: &'a str,
    pub runtime: &'a str,
    pub dev: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl SystemdParams<'_> {
    fn environment(&self) -> Vec<(&'static str, String)> {
        vec![
            ("WARREN_DATA_DIR", self.data_dir.display().to_string()),
            ("WARREN_LISTEN", self.listen.to_string()),
            ("WARREN_PUBLIC_PORT", self.port.to_string()),
            ("WARREN_BASE_DOMAIN", self.base_domain.to_string()),
            ("WARREN_RUNTIME", self.runtime.to_string()),
            ("WARREN_DEV", self.dev.to_string()),
            ("WARREN_LOG", "info".to_string()),
        ]
    }
}

/// Turn the system unit template into a user unit: no Docker dependency (the
/// user session has no `docker.service`), no environment file, no
/// `StateDirectory`, none of the sandboxing directives (they need root or
/// block the home directory), the settings inline and `default.target`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn render_systemd_user_unit(template: &str, p: &SystemdParams<'_>) -> String {
    const KEEP: &[&str] = &[
        "Description=",
        "Documentation=",
        "Type=",
        "Restart=",
        "RestartSec=",
        "KillSignal=",
        "TimeoutStopSec=",
    ];
    let mut unit = Vec::new();
    let mut service = Vec::new();
    let mut section = "";
    for line in template.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            section = t;
            continue;
        }
        if !KEEP.iter().any(|k| t.starts_with(k)) {
            continue;
        }
        match section {
            "[Unit]" => unit.push(t.to_string()),
            "[Service]" => service.push(t.to_string()),
            _ => {}
        }
    }
    let mut out = String::new();
    out.push_str("# Warren daemon as a systemd user service, written by `warren up` from\n");
    out.push_str(
        "# packaging/systemd/warrend.service. Edit and `systemctl --user daemon-reload`,\n",
    );
    out.push_str("# or run `warren up` again to regenerate it.\n\n[Unit]\n");
    for l in &unit {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str("\n[Service]\n");
    for l in &service {
        out.push_str(l);
        out.push('\n');
    }
    for (k, v) in p.environment() {
        out.push_str(&format!("Environment={k}={}\n", systemd_quote(&v)));
    }
    out.push_str(&format!("ExecStart={}\n", p.warrend.display()));
    out.push_str("\n[Install]\nWantedBy=default.target\n");
    out
}

/// The system unit with `ExecStart` pointing at this warrend, for the sudo path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn render_systemd_system_unit(template: &str, warrend: &Path) -> String {
    template
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("ExecStart=") {
                format!("ExecStart={}", warrend.display())
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `/etc/warren/warrend.env` for the sudo path (the system unit keeps its own data directory).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn render_systemd_env(p: &SystemdParams<'_>) -> String {
    let mut out = String::from(
        "# Warren daemon settings, written by `warren up`; every variable is listed in docs/self-hosting.md.\n",
    );
    for (k, v) in p.environment() {
        if k == "WARREN_DATA_DIR" {
            out.push_str("WARREN_DATA_DIR=/var/lib/warren\n");
        } else {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
    out
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemd_quote(v: &str) -> String {
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c))
    {
        v.to_string()
    } else {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(target_os = "linux")]
fn install_systemd(
    ctx: &Ctx,
    plan: &UpPlan,
    home: &Path,
    docker: bool,
) -> Result<DaemonState, Failure> {
    let params = SystemdParams {
        warrend: &plan.warrend,
        data_dir: &plan.data_dir,
        listen: &plan.listen,
        port: plan.port,
        base_domain: &plan.base_domain,
        runtime: &plan.runtime,
        dev: plan.dev,
    };
    if plan.runtime != "none" && !docker {
        // The user cannot talk to Docker: a user service would not either.
        // Render the system unit and the steps to install it as root.
        let dir = ctx
            .config_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config/warren"));
        fs::create_dir_all(&dir).map_err(|e| Failure::local(format!("{}: {e}", dir.display())))?;
        let unit = dir.join("warrend.service");
        let env = dir.join("warrend.env");
        fs::write(
            &unit,
            render_systemd_system_unit(SYSTEMD_TEMPLATE, &plan.warrend),
        )
        .map_err(|e| Failure::local(format!("{}: {e}", unit.display())))?;
        fs::write(&env, render_systemd_env(&params))
            .map_err(|e| Failure::local(format!("{}: {e}", env.display())))?;
        return Err(Failure::local(format!(
            "your user cannot talk to Docker (`docker info` fails), so the daemon has to run as root. Install it as a system service:\n\
             \x20 sudo install -m 0644 {unit} /etc/systemd/system/warrend.service\n\
             \x20 sudo install -d -m 0750 /etc/warren\n\
             \x20 sudo install -m 0640 {env} /etc/warren/warrend.env\n\
             \x20 sudo systemctl daemon-reload\n\
             \x20 sudo systemctl enable --now warrend\n\
             \x20 warren signup --api-url {api}\n\
             or add yourself to the docker group (`sudo usermod -aG docker $USER`, then log in again) and run `warren up` again for a user service.",
            unit = unit.display(),
            env = env.display(),
            api = plan.api_url
        )));
    }
    let unit_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user");
    fs::create_dir_all(&unit_dir)
        .map_err(|e| Failure::local(format!("{}: {e}", unit_dir.display())))?;
    let unit_path = unit_dir.join(format!("{SYSTEMD_UNIT}.service"));
    fs::write(
        &unit_path,
        render_systemd_user_unit(SYSTEMD_TEMPLATE, &params),
    )
    .map_err(|e| Failure::local(format!("{}: {e}", unit_path.display())))?;
    systemctl_user(&["daemon-reload"])?;
    systemctl_user(&["enable", "--now", SYSTEMD_UNIT])?;
    if systemd_active() {
        // `enable --now` does not restart a unit that was already active.
        systemctl_user(&["restart", SYSTEMD_UNIT])?;
    }
    eprintln!("enabled {SYSTEMD_UNIT} from {}", unit_path.display());
    Ok(DaemonState {
        service: ServiceKind::Systemd,
        port: plan.port,
        listen: plan.listen.clone(),
        base_domain: plan.base_domain.clone(),
        api_url: plan.api_url.clone(),
        dashboard_url: plan.dashboard_url.clone(),
        data_dir: plan.data_dir.clone(),
        warrend: plan.warrend.clone(),
        runtime: plan.runtime.clone(),
        mac_base_vm: None,
        dev: plan.dev,
        log_path: PathBuf::from(format!("journalctl --user -u {SYSTEMD_UNIT}")),
        pid_file: None,
        unit_path: Some(unit_path),
        created_at: now(),
    })
}

/// Poll `GET /v1/health` until it answers or the timeout passes.
async fn wait_for_health(api: &Api, state: &DaemonState) -> Result<Value, Failure> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        let last_err = match api.health().await {
            Ok(v) => return Ok(v),
            Err(e) => e.to_string(),
        };
        if state.service == ServiceKind::Process && state.running_pid().is_none() {
            return Err(Failure::local(format!(
                "warrend exited before answering; last lines of {}:\n{}",
                state.log_path.display(),
                tail_file(&state.log_path, 20)
            )));
        }
        if Instant::now() >= deadline {
            return Err(Failure::local(format!(
                "warrend did not answer at {} within {} s ({last_err}); last lines of {}:\n{}",
                state.api_url,
                HEALTH_TIMEOUT.as_secs(),
                state.log_path.display(),
                log_tail(state)
            )));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Whether a key for the daemon ended up in the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// The config already had a key for this URL.
    Existing(String),
    /// `POST /v1/signup` created a workspace and the key was saved.
    Created(String),
    /// No key, with the reason.
    Unavailable(String),
}

/// Reuse the configured key when it belongs to this URL, else sign up and save.
async fn ensure_key(ctx: &Ctx, api_url: &str, api: &Api) -> KeyOutcome {
    let Some(path) = &ctx.config_path else {
        return KeyOutcome::Unavailable(
            "neither XDG_CONFIG_HOME nor HOME is set, nowhere to save a key".into(),
        );
    };
    let file = FileConfig::load(path).unwrap_or_default();
    if let (Some(key), Some(url)) = (&file.api_key, &file.api_url)
        && normalize_api_url(url) == api_url
    {
        return KeyOutcome::Existing(key.clone());
    }
    match api.signup(None).await {
        Ok(v) => {
            let key = v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if key.is_empty() {
                return KeyOutcome::Unavailable("signup returned no key".into());
            }
            let cfg = FileConfig {
                api_url: Some(api_url.to_string()),
                api_key: Some(key.clone()),
            };
            match cfg.save(path) {
                Ok(()) => KeyOutcome::Created(key),
                Err(e) => {
                    KeyOutcome::Unavailable(format!("saving {}: {}", path.display(), e.message))
                }
            }
        }
        Err(e) => KeyOutcome::Unavailable(format!(
            "signup failed ({e}); run `warren login --api-url {api_url} --api-key KEY` with a key from the operator"
        )),
    }
}

// ----- down -------------------------------------------------------------------------

/// `warren down [--purge] [--yes]`
pub async fn down(ctx: &Ctx, args: DownArgs) -> Result<i32, Failure> {
    let Some(state) = DaemonState::load(ctx)? else {
        // No state: still stop a LaunchAgent or user unit installed by hand.
        if cfg!(target_os = "macos") && launchd_loaded() {
            launchctl_bootout()?;
            println!("stopped {LAUNCHD_LABEL} (no `warren up` state was recorded)");
            return Ok(EXIT_OK);
        }
        if cfg!(target_os = "linux") && systemd_active() {
            systemctl_user(&["disable", "--now", SYSTEMD_UNIT])?;
            println!("stopped {SYSTEMD_UNIT} (no `warren up` state was recorded)");
            return Ok(EXIT_OK);
        }
        return Err(Failure::local(format!(
            "nothing to stop: `warren up` has not run on this machine ({} is missing)",
            display_path(DaemonState::path(ctx).as_deref())
        )));
    };
    let mut lines = Vec::new();
    match state.running_pid() {
        Some(pid) => {
            stop_service(&state)?;
            lines.push(format!("stopped {} (pid {pid})", state.service.label()));
        }
        None => {
            // Make sure a loaded-but-idle service is unloaded too.
            if state.service == ServiceKind::Launchd && launchd_loaded() {
                launchctl_bootout()?;
                lines.push(format!("unloaded {LAUNCHD_LABEL}"));
            } else if state.service == ServiceKind::Systemd {
                let _ = systemctl_user(&["disable", "--now", SYSTEMD_UNIT]);
                lines.push(format!("disabled {SYSTEMD_UNIT}"));
            } else {
                lines.push(format!("{} was not running", state.service.label()));
            }
        }
    }
    if let Some(pid_file) = &state.pid_file {
        let _ = fs::remove_file(pid_file);
    }
    if port_in_use(state.port) {
        lines.push(format!(
            "warning: port {} is still in use by {}",
            state.port,
            who_listens(state.port).unwrap_or_else(|| "another process".into())
        ));
    } else {
        lines.push(format!("port {} is free", state.port));
    }
    let mut removed_data = false;
    if args.purge {
        if let Some(unit) = &state.unit_path {
            match fs::remove_file(unit) {
                Ok(()) => lines.push(format!("removed {}", unit.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => lines.push(format!("warning: {}: {e}", unit.display())),
            }
            if state.service == ServiceKind::Systemd {
                let _ = systemctl_user(&["daemon-reload"]);
            }
        }
        if state.data_dir.exists() {
            let ok = args.yes
                || confirm(&format!(
                    "Delete {} with every instance disk, backup, workspace and key in it? [y/N] ",
                    state.data_dir.display()
                ));
            if ok {
                fs::remove_dir_all(&state.data_dir)
                    .map_err(|e| Failure::local(format!("{}: {e}", state.data_dir.display())))?;
                lines.push(format!("removed {}", state.data_dir.display()));
                removed_data = true;
            } else {
                lines.push(format!(
                    "kept {} (pass --yes to remove it without a prompt)",
                    state.data_dir.display()
                ));
            }
        }
        DaemonState::remove(ctx);
    }
    if ctx.json {
        println!(
            "{}",
            warren_cli::output::to_json_pretty(&json!({
                "service": state.service,
                "port": state.port,
                "port_free": !port_in_use(state.port),
                "purged": args.purge,
                "data_dir_removed": removed_data,
                "messages": lines,
            }))
        );
    } else {
        for l in lines {
            println!("{l}");
        }
    }
    Ok(EXIT_OK)
}

/// Stop the daemon described by `state`, whichever way it runs.
fn stop_service(state: &DaemonState) -> Result<(), Failure> {
    match state.service {
        ServiceKind::Launchd => {
            if launchd_loaded() {
                launchctl_bootout()?;
            }
            Ok(())
        }
        ServiceKind::Systemd => systemctl_user(&["disable", "--now", SYSTEMD_UNIT]).map(|_| ()),
        ServiceKind::Process => {
            let Some(pid) = state.pid_file.as_deref().and_then(read_pid) else {
                return Ok(());
            };
            terminate(pid, Duration::from_secs(30))
        }
    }
}

/// Ask on the terminal; `false` without one.
fn confirm(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
}

// ----- status -----------------------------------------------------------------------

/// `warren status`
pub async fn status(ctx: &Ctx) -> Result<i32, Failure> {
    let state = DaemonState::load(ctx)?;
    let api_url = state
        .as_ref()
        .map(|s| s.api_url.clone())
        .unwrap_or_else(|| ctx.settings.api_url.clone());
    let (service, pid) = match &state {
        Some(s) => (Some(s.service), s.running_pid()),
        None => (None, None),
    };
    let anonymous = Api::new(&Settings {
        api_url: api_url.clone(),
        api_key: None,
    })?;
    let health = anonymous.health().await.ok();
    // The instance count needs the key that belongs to this URL.
    let key = key_for_url(ctx, &api_url);
    let instances = match &key {
        Some(k) => {
            let api = Api::new(&Settings {
                api_url: api_url.clone(),
                api_key: Some(k.clone()),
            })?;
            api.list_instances().await.ok().map(|v| {
                let items = warren_cli::output::items_of(&v);
                let mut by_status: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for i in &items {
                    *by_status
                        .entry(
                            i.get("status")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_string(),
                        )
                        .or_default() += 1;
                }
                (items.len(), by_status)
            })
        }
        None => None,
    };
    if ctx.json {
        println!(
            "{}",
            warren_cli::output::to_json_pretty(&json!({
                "service": service,
                "pid": pid,
                "running": pid.is_some(),
                "api_url": api_url,
                "dashboard_url": state.as_ref().map(|s| s.dashboard_url.clone()),
                "data_dir": state.as_ref().map(|s| s.data_dir.clone()),
                "log": state.as_ref().map(|s| s.log_path.clone()),
                "health": health,
                "key_configured": key.is_some(),
                "instances": instances.as_ref().map(|(n, by)| json!({"count": n, "by_status": by})),
            }))
        );
        return Ok(EXIT_OK);
    }
    match (&state, pid) {
        (Some(s), Some(pid)) => println!("service    {}: running (pid {pid})", s.service.label()),
        (Some(s), None) => println!("service    {}: not running", s.service.label()),
        (None, _) => {
            println!("service    none recorded (`warren up` has not run here); probing {api_url}")
        }
    }
    match &health {
        Some(h) => {
            let runtimes: Vec<&str> = h
                .get("runtimes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            println!(
                "health     {}, warrend {}, runtimes: {}{}",
                if h.get("healthy").and_then(Value::as_bool).unwrap_or(false) {
                    "healthy"
                } else {
                    "unhealthy"
                },
                h.get("version").and_then(Value::as_str).unwrap_or("?"),
                if runtimes.is_empty() {
                    "none".to_string()
                } else {
                    runtimes.join(", ")
                },
                if h.get("dev_mode").and_then(Value::as_bool).unwrap_or(false) {
                    ", dev mode"
                } else {
                    ""
                }
            );
        }
        None => println!("health     no answer from {api_url}/v1/health"),
    }
    println!("api        {api_url}");
    if let Some(s) = &state {
        println!("dashboard  {}", s.dashboard_url);
        println!("data       {}", s.data_dir.display());
        println!("log        {}", s.log_path.display());
    }
    match (&instances, &key) {
        (Some((n, by)), _) => {
            let detail: Vec<String> = by.iter().map(|(k, v)| format!("{v} {k}")).collect();
            println!(
                "instances  {n}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", detail.join(", "))
                }
            );
        }
        (None, Some(_)) => println!("instances  - (the API did not answer)"),
        (None, None) => {
            println!("instances  - (no API key for {api_url}; `warren signup --api-url {api_url}`)")
        }
    }
    Ok(EXIT_OK)
}

/// The configured key when it belongs to `api_url` (flags and environment win).
pub fn key_for_url(ctx: &Ctx, api_url: &str) -> Option<String> {
    if ctx.settings.api_url == api_url
        && let Some(k) = &ctx.settings.api_key
    {
        return Some(k.clone());
    }
    let path = ctx.config_path.as_ref()?;
    let file = FileConfig::load(path).ok()?;
    match (file.api_url, file.api_key) {
        (Some(u), Some(k)) if normalize_api_url(&u) == api_url => Some(k),
        _ => None,
    }
}

// ----- shared helpers -----------------------------------------------------------------

/// `$HOME`, required for every path this module writes.
pub fn home_dir() -> Result<PathBuf, Failure> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Failure::local("HOME is not set"))
}

/// `~/Library/Application Support/Warren` on macOS; `$XDG_DATA_HOME/warren` or
/// `~/.local/share/warren` elsewhere.
pub fn default_data_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Warren")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("warren")
    }
}

/// Make a path absolute against the current directory.
pub fn absolute(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
    }
}

/// A sibling binary: next to the running `warren` (symlinks resolved), then on PATH.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let exe = fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    find_in_path(name)
}

/// The warrend binary: the flag (or `WARREN_WARREND`), else [`find_tool`].
pub fn find_warrend(flag: Option<&Path>) -> Result<PathBuf, Failure> {
    if let Some(p) = flag {
        let p = absolute(p.to_path_buf());
        return if p.is_file() {
            Ok(p)
        } else {
            Err(Failure::local(format!(
                "--warrend-bin {}: no such file",
                p.display()
            )))
        };
    }
    find_tool("warrend").ok_or_else(|| {
        Failure::local(
            "warrend not found next to this binary or on PATH; install it (docs/install.md) or pass --warrend-bin PATH",
        )
    })
}

/// `<name> --version` → the version token, when the binary runs.
pub fn binary_version(path: &Path) -> Option<String> {
    let out = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    extract_version(&text)
}

/// The first `digits.digits[.digits]` run in `s`, for example `0.7.2` in `lume v0.7.2`.
pub fn extract_version(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'v';
        if !(b[i].is_ascii_digit() && boundary) {
            i += 1;
            continue;
        }
        let start = i;
        let mut dots = 0;
        while i < b.len() {
            if b[i].is_ascii_digit() {
                i += 1;
            } else if b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                dots += 1;
                i += 1;
            } else {
                break;
            }
        }
        if dots >= 1 {
            return Some(s[start..i].to_string());
        }
    }
    None
}

/// `a.b.c >= min` (missing components count as zero).
pub fn version_at_least(v: &str, min: (u32, u32, u32)) -> bool {
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let got = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    got >= min
}

fn fmt_version(v: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// `lume` on PATH and its version.
pub fn lume_version() -> Option<(PathBuf, String)> {
    let bin = find_in_path("lume")?;
    let version = binary_version(&bin)?;
    Some((bin, version))
}

/// Every VM Lume knows, from `lume ls -f json`.
pub fn lume_vms(bin: &Path) -> Option<Vec<Value>> {
    let out = Command::new(bin)
        .args(["ls", "-f", "json"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(warren_cli::output::items_of(&v))
}

/// `status` of one VM (`running`, `stopped`), `None` when it does not exist.
pub fn lume_vm_status(bin: &Path, name: &str) -> Option<String> {
    let out = Command::new(bin)
        .args(["get", name, "-f", "json"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let items = warren_cli::output::items_of(&v);
    items
        .first()
        .and_then(|vm| vm.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Lume's API server address: `WARREN_LUME_API` or the default.
pub fn lume_api_url() -> String {
    std::env::var("WARREN_LUME_API")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LUME_API.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// `GET /lume/host/status` of `lume serve`.
pub async fn lume_host_status() -> Option<Value> {
    let url = format!("{}/lume/host/status", lume_api_url());
    let resp = warren_sdk::http_client()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Whether `docker info` succeeds for this user.
pub fn docker_ok() -> bool {
    run_ok("docker", &["info"]).is_ok()
}

/// Whether Docker lists the `runsc` runtime; `None` when Docker does not answer.
pub fn docker_has_runsc() -> Option<bool> {
    let out = Command::new("docker")
        .args(["info", "--format", "{{json .Runtimes}}"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).contains("runsc"))
}

/// `docker version --format {{.Server.Version}}`.
pub fn docker_version() -> Option<String> {
    let out = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Whether `docker image inspect <image>` succeeds.
pub fn docker_image_present(image: &str) -> Option<bool> {
    if !docker_ok() {
        return None;
    }
    Some(run_ok("docker", &["image", "inspect", image]).is_ok())
}

/// True when something listens on `port` on either loopback address.
pub fn port_in_use(port: u16) -> bool {
    use std::net::TcpListener;
    let busy = |addr: &str| match TcpListener::bind((addr, port)) {
        Ok(_) => false,
        Err(e) => e.kind() == std::io::ErrorKind::AddrInUse,
    };
    busy("127.0.0.1") || busy("::1")
}

/// `command (pid N)` of the listener on `port`, from lsof (or ss on Linux).
pub fn who_listens(port: u16) -> Option<String> {
    if let Ok(out) = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .stdin(Stdio::null())
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().nth(1) {
            let mut f = line.split_whitespace();
            if let (Some(cmd), Some(pid)) = (f.next(), f.next()) {
                return Some(format!("{cmd} (pid {pid})"));
            }
        }
    }
    if let Ok(out) = Command::new("ss")
        .args(["-ltnp", &format!("sport = :{port}")])
        .stdin(Stdio::null())
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().nth(1) {
            return Some(parse_ss_line(line));
        }
    }
    None
}

/// `name (pid N)` from an `ss -ltnp` line, or the line itself.
pub fn parse_ss_line(line: &str) -> String {
    if let Some(start) = line.find("users:((\"") {
        let rest = &line[start + 9..];
        let name = rest.split('"').next().unwrap_or("?");
        let pid = rest
            .split("pid=")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .unwrap_or("?");
        return format!("{name} (pid {pid})");
    }
    line.trim().to_string()
}

/// `http://api.<base>:<port>`
pub fn api_url_for(base_domain: &str, port: u16) -> String {
    format!("http://api.{base_domain}:{port}")
}

/// `http://<base>:<port>/`
pub fn dashboard_url_for(base_domain: &str, port: u16) -> String {
    format!("http://{base_domain}:{port}/")
}

/// Escape text for a plist `<string>`.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `sk_live_abcdef...`
pub fn key_prefix(key: &str) -> String {
    let shown: String = key.chars().take(14).collect();
    if key.chars().count() > 14 {
        format!("{shown}...")
    } else {
        shown
    }
}

fn display_path(p: Option<&Path>) -> String {
    p.map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run a command, succeed on exit 0, else return its stderr.
pub fn run_ok(bin: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if err.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            err
        };
        Err(format!(
            "{bin} {}: exit {}{}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            if msg.is_empty() {
                String::new()
            } else {
                format!(": {msg}")
            }
        ))
    }
}

/// The last `n` lines of a file, or a note when it cannot be read.
pub fn tail_file(path: &Path, n: usize) -> String {
    match fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].join("\n")
        }
        Err(e) => format!("(cannot read {}: {e})", path.display()),
    }
}

fn log_tail(state: &DaemonState) -> String {
    if state.service == ServiceKind::Systemd {
        run_ok(
            "journalctl",
            &["--user", "-u", SYSTEMD_UNIT, "-n", "20", "--no-pager"],
        )
        .unwrap_or_else(|e| e)
    } else {
        tail_file(&state.log_path, 20)
    }
}

// ----- processes ----------------------------------------------------------------------

/// The pid in a pid file.
pub fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a process with this pid exists (signal 0).
pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// SIGTERM, wait up to `grace`, then SIGKILL.
pub fn terminate(pid: i32, grace: Duration) -> Result<(), Failure> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if !pid_alive(pid) {
        return Ok(());
    }
    kill(Pid::from_raw(pid), Signal::SIGTERM)
        .map_err(|e| Failure::local(format!("signalling pid {pid}: {e}")))?;
    let deadline = Instant::now() + grace;
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            std::thread::sleep(Duration::from_millis(200));
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

// ----- launchd -------------------------------------------------------------------------

/// Whether the LaunchAgent is loaded in this user's domain.
pub fn launchd_loaded() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    run_ok(
        "launchctl",
        &["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())],
    )
    .is_ok()
}

/// The pid launchd reports for the LaunchAgent, when it is running.
pub fn launchd_pid() -> Option<i32> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let text = run_ok(
        "launchctl",
        &["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())],
    )
    .ok()?;
    parse_launchctl_pid(&text)
}

/// `pid = N` from `launchctl print` output.
pub fn parse_launchctl_pid(text: &str) -> Option<i32> {
    text.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("pid = "))
        .and_then(|p| p.trim().parse().ok())
        .filter(|p: &i32| *p > 0)
}

fn launchctl_bootout() -> Result<(), Failure> {
    let target = format!("gui/{}/{LAUNCHD_LABEL}", uid());
    run_ok("launchctl", &["bootout", &target])
        .map(|_| ())
        .map_err(|e| Failure::local(format!("launchctl bootout {target}: {e}")))?;
    // bootout returns before the process is gone; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(15);
    while launchd_pid().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

// ----- systemd -------------------------------------------------------------------------

fn systemctl_user(args: &[&str]) -> Result<String, Failure> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    run_ok("systemctl", &full).map_err(Failure::local)
}

/// Whether the user unit is active.
pub fn systemd_active() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    run_ok(
        "systemctl",
        &["--user", "is-active", "--quiet", SYSTEMD_UNIT],
    )
    .is_ok()
}

/// `MainPID` of the user unit, when it runs.
pub fn systemd_main_pid() -> Option<i32> {
    if !systemd_active() {
        return None;
    }
    run_ok(
        "systemctl",
        &["--user", "show", "-p", "MainPID", "--value", SYSTEMD_UNIT],
    )
    .ok()?
    .trim()
    .parse()
    .ok()
    .filter(|p: &i32| *p > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(extract_version("lume v0.7.2").as_deref(), Some("0.7.2"));
        assert_eq!(extract_version("warrend 0.1.0").as_deref(), Some("0.1.0"));
        assert_eq!(
            extract_version("Docker version 29.0.1, build abc").as_deref(),
            Some("29.0.1")
        );
        assert_eq!(extract_version("no digits"), None);
        assert_eq!(extract_version("build 42"), None);
        assert!(version_at_least("0.7.2", LUME_MIN_VERSION));
        assert!(version_at_least("0.10.0", LUME_MIN_VERSION));
        assert!(version_at_least("1.0", LUME_MIN_VERSION));
        assert!(!version_at_least("0.7.1", LUME_MIN_VERSION));
        assert!(!version_at_least("0.1.5", LUME_MIN_VERSION));
    }

    #[test]
    fn urls_and_paths() {
        assert_eq!(
            api_url_for("warren.localhost", 7337),
            "http://api.warren.localhost:7337"
        );
        assert_eq!(
            dashboard_url_for("warren.localhost", 7357),
            "http://warren.localhost:7357/"
        );
        let d = default_data_dir(Path::new("/home/u"));
        if cfg!(target_os = "macos") {
            assert_eq!(
                d,
                PathBuf::from("/home/u/Library/Application Support/Warren")
            );
        } else {
            assert!(d.ends_with("warren"));
        }
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(key_prefix("sk_live_abcdefghijklmnop"), "sk_live_abcdef...");
        assert_eq!(key_prefix("short"), "short");
        assert_eq!(
            parse_launchctl_pid("state = running\n\tpid = 4242\n"),
            Some(4242)
        );
        assert_eq!(parse_launchctl_pid("state = not running\n"), None);
        assert_eq!(
            parse_ss_line(
                "LISTEN 0 4096 127.0.0.1:7337 0.0.0.0:* users:((\"warrend\",pid=4242,fd=9))"
            ),
            "warrend (pid 4242)"
        );
        assert_eq!(parse_ss_line("  odd line  "), "odd line");
    }

    #[test]
    fn runtime_mapping() {
        assert_eq!(resolve_runtime(RuntimeChoice::None, true).unwrap(), "none");
        assert_eq!(resolve_runtime(RuntimeChoice::None, false).unwrap(), "none");
        assert_eq!(
            resolve_runtime(RuntimeChoice::Docker, true).unwrap(),
            "auto"
        );
        assert!(resolve_runtime(RuntimeChoice::Docker, false).is_err());
        assert_eq!(resolve_runtime(RuntimeChoice::Auto, true).unwrap(), "auto");
        if cfg!(target_os = "macos") {
            assert_eq!(resolve_runtime(RuntimeChoice::Auto, false).unwrap(), "none");
        } else {
            assert!(resolve_runtime(RuntimeChoice::Auto, false).is_err());
        }
    }

    #[test]
    fn warrend_arguments() {
        let plan = UpPlan {
            warrend: PathBuf::from("/opt/warrend"),
            port: 7357,
            listen: "127.0.0.1:7357".into(),
            base_domain: "warren.localhost".into(),
            data_dir: PathBuf::from("/tmp/w"),
            runtime: "none".into(),
            mac_base_vm: Some("warren-macos-26".into()),
            lume_bin: Some(PathBuf::from("/Users/u/.local/bin/lume")),
            dev: true,
            api_url: "http://api.warren.localhost:7357".into(),
            dashboard_url: "http://warren.localhost:7357/".into(),
        };
        assert_eq!(
            plan.warrend_args(),
            vec![
                "--runtime",
                "none",
                "--data-dir",
                "/tmp/w",
                "--listen",
                "127.0.0.1:7357",
                "--public-port",
                "7357",
                "--base-domain",
                "warren.localhost",
                "--mac-base-vm",
                "warren-macos-26",
                "--lume-bin",
                "/Users/u/.local/bin/lume",
                "--dev"
            ]
        );
    }

    #[test]
    fn launchd_plist_rendering() {
        let args = vec![
            "--runtime".to_string(),
            "none".to_string(),
            "--data-dir".to_string(),
            "/Users/alice/Library/Application Support/Warren".to_string(),
            "--listen".to_string(),
            "127.0.0.1:7337".to_string(),
        ];
        let extra = vec![PathBuf::from("/Users/alice/.local/bin")];
        let out = render_launchd_plist(
            LAUNCHD_TEMPLATE,
            &LaunchdParams {
                warrend: Path::new("/usr/local/bin/warrend"),
                home: Path::new("/Users/alice"),
                base_vm: "warren-macos-26",
                args: &args,
                working_dir: Path::new("/Users/alice/Library/Application Support/Warren"),
                log_path: Path::new("/Users/alice/Library/Logs/Warren/warrend.log"),
                extra_path: &extra,
            },
        );
        assert!(
            !out.contains("__WARREND__")
                && !out.contains("__HOME__")
                && !out.contains("__BASE_VM__"),
            "{out}"
        );
        assert!(out.contains("<string>/usr/local/bin/warrend</string>"));
        assert!(out.contains("<string>--data-dir</string>\n\t\t<string>/Users/alice/Library/Application Support/Warren</string>"));
        assert!(out.contains("<string>127.0.0.1:7337</string>"));
        // The template's own arguments are gone.
        assert!(!out.contains("<string>--public-port</string>"));
        assert!(!out.contains("/.warren/data"));
        assert!(out.contains("<key>WorkingDirectory</key>\n\t<string>/Users/alice/Library/Application Support/Warren</string>"));
        assert_eq!(
            out.matches("/Users/alice/Library/Logs/Warren/warrend.log")
                .count(),
            2
        );
        assert!(out.contains("<string>/Users/alice/.local/bin:/opt/homebrew/bin:"));
        assert!(out.contains("<key>HOME</key>\n\t\t<string>/Users/alice</string>"));
        assert!(out.contains("<string>dev.warren.daemon</string>"));
        // Balanced array.
        assert_eq!(
            out.matches("<array>").count(),
            out.matches("</array>").count()
        );
    }

    #[test]
    fn systemd_rendering() {
        let p = SystemdParams {
            warrend: Path::new("/home/u/.local/bin/warrend"),
            data_dir: Path::new("/home/u/.local/share/warren"),
            listen: "127.0.0.1:7337",
            port: 7337,
            base_domain: "warren.localhost",
            runtime: "auto",
            dev: false,
        };
        let unit = render_systemd_user_unit(SYSTEMD_TEMPLATE, &p);
        assert!(unit.contains("[Unit]\nDescription=Warren daemon"));
        assert!(unit.contains("ExecStart=/home/u/.local/bin/warrend\n"));
        assert!(unit.contains("Environment=WARREN_DATA_DIR=/home/u/.local/share/warren\n"));
        assert!(unit.contains("Environment=WARREN_LISTEN=127.0.0.1:7337\n"));
        assert!(unit.contains("Environment=WARREN_RUNTIME=auto\n"));
        assert!(unit.contains("Restart=always\n"));
        assert!(unit.contains("WantedBy=default.target"));
        for gone in [
            "Requires=docker.service",
            "EnvironmentFile=",
            "StateDirectory=",
            "ProtectHome=",
            "ProtectSystem=",
            "LimitNOFILE=",
            "multi-user.target",
        ] {
            assert!(!unit.contains(gone), "{gone} should be gone:\n{unit}");
        }
        let sys = render_systemd_system_unit(SYSTEMD_TEMPLATE, Path::new("/opt/bin/warrend"));
        assert!(sys.contains("ExecStart=/opt/bin/warrend\n"));
        assert!(sys.contains("Requires=docker.service"));
        assert!(sys.contains("EnvironmentFile=/etc/warren/warrend.env"));
        let env = render_systemd_env(&p);
        assert!(env.contains("WARREN_DATA_DIR=/var/lib/warren\n"));
        assert!(env.contains("WARREN_BASE_DOMAIN=warren.localhost\n"));
        assert!(env.contains("WARREN_PUBLIC_PORT=7337\n"));
        assert_eq!(systemd_quote("a b"), "\"a b\"");
        assert_eq!(systemd_quote("/x/y:1"), "/x/y:1");
    }

    #[test]
    fn state_round_trip() {
        let s = DaemonState {
            service: ServiceKind::Process,
            port: 7357,
            listen: "127.0.0.1:7357".into(),
            base_domain: "warren.localhost".into(),
            api_url: "http://api.warren.localhost:7357".into(),
            dashboard_url: "http://warren.localhost:7357/".into(),
            data_dir: PathBuf::from("/tmp/w"),
            warrend: PathBuf::from("/opt/warrend"),
            runtime: "none".into(),
            mac_base_vm: None,
            dev: true,
            log_path: PathBuf::from("/tmp/w/warrend.log"),
            pid_file: Some(PathBuf::from("/tmp/w/warrend.pid")),
            unit_path: None,
            created_at: 1,
        };
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"service\":\"process\""));
        let back: DaemonState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id() as i32));
    }

    #[test]
    fn free_port_is_detected() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(port_in_use(port));
        drop(l);
        assert!(!port_in_use(port));
    }
}
