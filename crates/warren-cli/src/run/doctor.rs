//! `warren doctor`: checks this machine for everything Warren needs and prints
//! one `OK`, `WARN` or `FAIL` line per check, with a one-line fix for every
//! problem. The exit code is 1 when any check fails.
//!
//! Common: platform, the `warren`, `warrend` and `warren-mcp` binaries, the
//! config file and API URL, the daemon's health and runtimes, who holds the
//! port, the data directory and its free space, the service state. macOS:
//! Lume's version, `lume serve`, the two-guest limit, the base VM, running
//! `warren-*` VMs. Linux: Docker, gVisor's `runsc`, the `warren/base` image.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use warren_cli::{EXIT_API, EXIT_OK, Failure, config::Settings, util::format_bytes};

use super::{
    Ctx,
    api::Api,
    setup::{
        self, DEFAULT_MAC_BASE_VM, DaemonState, GVISOR_HINT, LUME_INSTALL_HINT, LUME_MIN_VERSION,
    },
};

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            status: Status::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            name,
            status: Status::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            name,
            status: Status::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Render the report: `STATUS  name  detail`, then an indented `fix:` line.
pub fn render(checks: &[Check]) -> String {
    let width = checks
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for c in checks {
        out.push_str(&format!(
            "{:<5} {:<w$}  {}\n",
            c.status.tag(),
            c.name,
            c.detail,
            w = width
        ));
        if let Some(fix) = &c.fix {
            out.push_str(&format!("{:<5} {:<w$}  fix: {fix}\n", "", "", w = width));
        }
    }
    out
}

/// `warren doctor`
pub async fn doctor(ctx: &Ctx) -> Result<i32, Failure> {
    let state = DaemonState::load(ctx).ok().flatten();
    let mut checks: Vec<Check> = Vec::new();

    // Platform.
    let arch = std::env::consts::ARCH;
    let platform = platform_name();
    if cfg!(target_os = "macos") && arch != "aarch64" {
        checks.push(Check::warn(
            "platform",
            format!("{platform} ({arch})"),
            "mac instances need an Apple silicon Mac; this one can only run the CLI",
        ));
    } else {
        checks.push(Check::ok("platform", format!("{platform} ({arch})")));
    }

    // Binaries.
    let warren_version = env!("CARGO_PKG_VERSION");
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    checks.push(Check::ok("warren", format!("{warren_version} at {exe}")));
    let warrend = state
        .as_ref()
        .map(|s| s.warrend.clone())
        .filter(|p| p.is_file())
        .or_else(|| setup::find_tool("warrend"));
    match &warrend {
        Some(p) => {
            let v = setup::binary_version(p).unwrap_or_else(|| "?".into());
            if v != warren_version && v != "?" {
                checks.push(Check::warn(
                    "warrend",
                    format!("{v} at {} (warren is {warren_version})", p.display()),
                    "install matching versions of warren and warrend (docs/install.md)",
                ));
            } else {
                checks.push(Check::ok("warrend", format!("{v} at {}", p.display())));
            }
        }
        None => checks.push(Check::fail(
            "warrend",
            "not found next to warren or on PATH",
            "install the release tarball or `cargo build --release -p warrend` (docs/install.md), or pass --warrend-bin to `warren up`",
        )),
    }
    match setup::find_tool("warren-mcp") {
        Some(p) => checks.push(Check::ok("warren-mcp", p.display().to_string())),
        None => checks.push(Check::warn(
            "warren-mcp",
            "not found next to warren or on PATH",
            "`warren connect` needs it; install it from the release tarball or `cargo build --release -p warren-mcp`",
        )),
    }

    // Config and API URL.
    let api_url = state
        .as_ref()
        .map(|s| s.api_url.clone())
        .unwrap_or_else(|| ctx.settings.api_url.clone());
    let config_path = ctx
        .config_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no HOME)".into());
    let key = setup::key_for_url(ctx, &api_url);
    if key.is_some() {
        checks.push(Check::ok(
            "config",
            format!("{config_path}; API {api_url}; key configured"),
        ));
    } else {
        checks.push(Check::warn(
            "config",
            format!("{config_path}; API {api_url}; no key for this URL"),
            format!("`warren up` signs up and saves a key, or `warren signup --api-url {api_url}`"),
        ));
    }

    // Daemon health.
    let api = Api::new(&Settings {
        api_url: api_url.clone(),
        api_key: None,
    })?;
    let health = api.health().await.ok();
    let runtimes: Vec<String> = health
        .as_ref()
        .and_then(|h| h.get("runtimes"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    match &health {
        Some(h) => {
            let detail = format!(
                "warrend {} at {api_url}; runtimes: {}{}",
                h.get("version").and_then(Value::as_str).unwrap_or("?"),
                if runtimes.is_empty() {
                    "none".to_string()
                } else {
                    runtimes.join(", ")
                },
                if h.get("dev_mode").and_then(Value::as_bool).unwrap_or(false) {
                    "; dev mode"
                } else {
                    ""
                }
            );
            if runtimes.is_empty() {
                checks.push(Check::warn(
                    "daemon",
                    detail,
                    "no instance type is enabled: on macOS build the base VM with `warren mac init`, on Linux make Docker reachable, then `warren up` again",
                ));
            } else {
                checks.push(Check::ok("daemon", detail));
            }
        }
        None => {
            let fix = match &state {
                Some(s) => format!(
                    "`warren up` to start it; the log is {}",
                    s.log_path.display()
                ),
                None => "`warren up` starts a daemon on this machine".to_string(),
            };
            checks.push(Check::fail(
                "daemon",
                format!("no answer from {api_url}/v1/health"),
                fix,
            ));
        }
    }

    // Port.
    let port = state
        .as_ref()
        .map(|s| s.port)
        .or_else(|| port_of(&api_url))
        .unwrap_or(setup::DEFAULT_PORT);
    match (setup::who_listens(port), health.is_some()) {
        (Some(holder), true) => checks.push(Check::ok("port", format!("{port} held by {holder}"))),
        (Some(holder), false) => checks.push(Check::warn(
            "port",
            format!("{port} held by {holder}, which is not answering as Warren"),
            "stop it or run `warren up --port N` with another port",
        )),
        (None, true) => checks.push(Check::ok("port", format!("{port} answers"))),
        (None, false) => checks.push(Check::warn(
            "port",
            format!("nothing listens on {port}"),
            "`warren up`",
        )),
    }

    // Service.
    if let Some(s) = &state {
        match s.running_pid() {
            Some(pid) => checks.push(Check::ok(
                "service",
                format!("{} running (pid {pid})", service_label(s)),
            )),
            None => checks.push(Check::warn(
                "service",
                format!("{} not running", service_label(s)),
                format!("`warren up` (log: {})", s.log_path.display()),
            )),
        }
    }

    // Data directory and free space.
    let data_dir = state
        .as_ref()
        .map(|s| s.data_dir.clone())
        .or_else(|| setup::home_dir().ok().map(|h| setup::default_data_dir(&h)));
    if let Some(dir) = &data_dir {
        let exists = dir.is_dir();
        match free_space(dir) {
            Some(free) => {
                let detail = format!(
                    "{}{}; {} free",
                    dir.display(),
                    if exists { "" } else { " (not created yet)" },
                    format_bytes(free as i64)
                );
                if free < 10 << 30 {
                    checks.push(Check::warn(
                        "data dir",
                        detail,
                        "free disk space: instance disks and macOS VM clones live here (a base VM alone is about 25 GB)",
                    ));
                } else {
                    checks.push(Check::ok("data dir", detail));
                }
            }
            None => checks.push(Check::warn(
                "data dir",
                format!("{} (cannot stat)", dir.display()),
                "`warren up --data-dir DIR` with a writable directory",
            )),
        }
    }

    // Platform specifics.
    if cfg!(target_os = "macos") {
        mac_checks(&mut checks, state.as_ref(), &runtimes).await;
        firewall_check(&mut checks);
        smolvm_check(&mut checks);
    } else if cfg!(target_os = "linux") {
        linux_checks(&mut checks);
    }

    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    if ctx.json {
        println!(
            "{}",
            warren_cli::output::to_json_pretty(&json!({ "checks": checks, "failed": failed }))
        );
    } else {
        print!("{}", render(&checks));
        if failed > 0 {
            println!(
                "{failed} check{} failed.",
                if failed == 1 { "" } else { "s" }
            );
        }
    }
    Ok(if failed > 0 { EXIT_API } else { EXIT_OK })
}

async fn mac_checks(checks: &mut Vec<Check>, state: Option<&DaemonState>, runtimes: &[String]) {
    let min = format!(
        "{}.{}.{}",
        LUME_MIN_VERSION.0, LUME_MIN_VERSION.1, LUME_MIN_VERSION.2
    );
    let lume = match setup::lume_version() {
        Some((bin, version)) if setup::version_at_least(&version, LUME_MIN_VERSION) => {
            checks.push(Check::ok("lume", format!("{version} at {}", bin.display())));
            Some(bin)
        }
        Some((bin, version)) => {
            checks.push(Check::fail(
                "lume",
                format!(
                    "{version} at {} is too old ({min} or newer needed)",
                    bin.display()
                ),
                LUME_INSTALL_HINT,
            ));
            Some(bin)
        }
        None => {
            checks.push(Check::fail("lume", "not found on PATH", LUME_INSTALL_HINT));
            None
        }
    };
    // lume serve and the two-guest limit.
    let api = setup::lume_api_url();
    match setup::lume_host_status().await {
        Some(h) => {
            let running = h.get("vm_count").and_then(Value::as_u64).unwrap_or(0);
            let max = h.get("max_vms").and_then(Value::as_u64).unwrap_or(2);
            checks.push(Check::ok(
                "lume serve",
                format!(
                    "answering at {api} (lume {})",
                    h.get("version").and_then(Value::as_str).unwrap_or("?")
                ),
            ));
            if running >= max {
                checks.push(Check::warn(
                    "mac guests",
                    format!("{running} of {max} macOS guests running: the host is full"),
                    "sleep or delete a mac instance (`warren instances sleep <id>`) before creating another; macOS allows two guests per host",
                ));
            } else {
                checks.push(Check::ok(
                    "mac guests",
                    format!("{running} of {max} macOS guests running"),
                ));
            }
        }
        None => {
            if runtimes.iter().any(|r| r == "lume") {
                checks.push(Check::warn(
                    "lume serve",
                    format!("not answering at {api}, although the daemon reports the lume runtime"),
                    "warrend starts `lume serve` itself and restarts it when it dies; check the daemon log",
                ));
            } else {
                checks.push(Check::warn(
                    "lume serve",
                    format!("not answering at {api}"),
                    "normal when the daemon is not running: warrend starts `lume serve` on demand (`lume serve` by hand to test Lume alone)",
                ));
            }
            if let Some(bin) = &lume {
                let running = running_vms(bin, |_| true);
                checks.push(Check::ok(
                    "mac guests",
                    format!("{} running (from lume ls)", running.len()),
                ));
            }
        }
    }
    // The base VM.
    let base = state
        .and_then(|s| s.mac_base_vm.clone())
        .unwrap_or_else(|| DEFAULT_MAC_BASE_VM.to_string());
    if let Some(bin) = &lume {
        match setup::lume_vm_status(bin, &base).as_deref() {
            Some("stopped") => checks.push(Check::ok(
                "base VM",
                format!("{base} exists and is stopped"),
            )),
            Some(other) => checks.push(Check::warn(
                "base VM",
                format!("{base} is {other}"),
                format!("the daemon clones it only while it is stopped: `lume stop {base}`"),
            )),
            None => checks.push(Check::fail(
                "base VM",
                format!("{base} does not exist"),
                "`warren mac init` builds it (docs/macos.md)",
            )),
        }
        let warren_vms = running_vms(bin, |name| name.starts_with("warren-") && name != base);
        checks.push(Check::ok(
            "warren VMs",
            if warren_vms.is_empty() {
                "no warren-* VM running".to_string()
            } else {
                format!("{} running: {}", warren_vms.len(), warren_vms.join(", "))
            },
        ));
    }
}

fn linux_checks(checks: &mut Vec<Check>) {
    match setup::docker_version() {
        Some(v) => checks.push(Check::ok("docker", format!("server {v}, reachable"))),
        None => checks.push(Check::fail(
            "docker",
            "`docker version` fails",
            "install Docker (https://docs.docker.com/engine/install/) and add yourself to the docker group (`sudo usermod -aG docker $USER`, log in again), or run the daemon as root (packaging/systemd/warrend.service)",
        )),
    }
    match setup::docker_has_runsc() {
        Some(true) => checks.push(Check::ok("gvisor", "runsc registered with Docker")),
        Some(false) => checks.push(Check::warn(
            "gvisor",
            "Docker has no runsc runtime; default instances run under runc",
            GVISOR_HINT,
        )),
        None => checks.push(Check::warn(
            "gvisor",
            "cannot ask Docker about its runtimes",
            GVISOR_HINT,
        )),
    }
    let tag = std::env::var("WARREN_IMAGE_TAG")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "dev".to_string());
    let image = format!("warren/base:{tag}");
    match setup::docker_image_present(&image) {
        Some(true) => checks.push(Check::ok("image", format!("{image} present"))),
        Some(false) => checks.push(Check::warn(
            "image",
            format!("{image} missing"),
            format!(
                "WARREN_GATEWAY_BIN=$(command -v warren-gateway) WARREN_IMAGE_TAG={tag} scripts/build-images.sh from a checkout (docs/templates.md)"
            ),
        )),
        None => checks.push(Check::warn(
            "image",
            format!("cannot check {image} without Docker"),
            "see the docker check",
        )),
    }
}

/// Names of the VMs `lume ls` reports as running, filtered by name.
fn running_vms(bin: &Path, keep: impl Fn(&str) -> bool) -> Vec<String> {
    setup::lume_vms(bin)
        .unwrap_or_default()
        .iter()
        .filter(|vm| vm.get("status").and_then(Value::as_str) == Some("running"))
        .filter_map(|vm| vm.get("name").and_then(Value::as_str))
        .filter(|n| keep(n))
        .map(str::to_string)
        .collect()
}

fn service_label(s: &DaemonState) -> String {
    match s.service {
        setup::ServiceKind::Launchd => format!("launchd {}", setup::LAUNCHD_LABEL),
        setup::ServiceKind::Systemd => format!("systemd --user {}", setup::SYSTEMD_UNIT),
        setup::ServiceKind::Process => "background process".to_string(),
    }
}

/// `macOS 26.1`, the `PRETTY_NAME` of `/etc/os-release`, or the OS name.
fn platform_name() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(v) = setup::run_ok("sw_vers", &["-productVersion"]) {
            return format!("macOS {v}");
        }
        return "macOS".to_string();
    }
    if let Ok(text) = std::fs::read_to_string("/etc/os-release")
        && let Some(line) = text.lines().find(|l| l.starts_with("PRETTY_NAME="))
    {
        return line["PRETTY_NAME=".len()..].trim_matches('"').to_string();
    }
    std::env::consts::OS.to_string()
}

/// The port in an `http://host:port` URL.
pub fn port_of(url: &str) -> Option<u16> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    host_port.rsplit_once(':')?.1.parse().ok()
}

/// Free bytes on the file system holding `path` (or its nearest existing ancestor).
pub fn free_space(path: &Path) -> Option<u64> {
    let mut p: PathBuf = path.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let s = nix::sys::statvfs::statvfs(&p).ok()?;
    Some(s.blocks_available() as u64 * s.fragment_size() as u64)
}

/// The macOS Application Firewall must let `warrend` accept connections from
/// the guests' NAT network, or the egress broker is unreachable from inside
/// `allowlist` instances (loopback is exempt, so the daemon itself looks fine).
fn firewall_check(checks: &mut Vec<Check>) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Ok(bin) = setup::find_warrend(None) else {
        return;
    };
    match setup::firewall_allows(&bin) {
        None => checks.push(Check::ok(
            "firewall",
            "Application Firewall off or not queryable; guests can reach the egress broker",
        )),
        Some((true, _)) => checks.push(Check::ok(
            "firewall",
            format!("Application Firewall allows incoming connections for {}", bin.display()),
        )),
        Some((false, fix)) => checks.push(Check::warn(
            "firewall",
            format!(
                "Application Firewall blocks incoming connections for {} (macOS guests cannot reach the egress broker, so `--network allowlist` and secrets fail there)",
                bin.display()
            ),
            fix,
        )),
    }
}

/// smolvm gives a Mac Linux instances as microVMs without Docker Desktop
/// (docs/linux-on-mac.md); optional when a Docker daemon answers.
fn smolvm_check(checks: &mut Vec<Check>) {
    if !cfg!(target_os = "macos") {
        return;
    }
    match setup::find_smolvm() {
        Some(bin) => {
            let version = std::process::Command::new(&bin)
                .arg("--version")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "version unknown".to_string());
            checks.push(Check::ok(
                "smolvm",
                format!("{version} at {} (Linux instances as microVMs)", bin.display()),
            ));
        }
        None if setup::docker_ok() => checks.push(Check::ok(
            "smolvm",
            "not installed; Docker runs the Linux instances on this Mac",
        )),
        None => checks.push(Check::warn(
            "smolvm",
            "not installed and no Docker daemon answers: no Linux instances on this Mac",
            "install smolvm (https://github.com/smol-machines/smolvm/releases) or Docker Desktop, then `warren up` again; see docs/linux-on-mac.md",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_aligns_and_adds_fix_lines() {
        let checks = vec![
            Check::ok("platform", "macOS 26.1 (aarch64)"),
            Check::fail(
                "base VM",
                "warren-macos-26 does not exist",
                "warren mac init",
            ),
            Check::warn("gvisor", "no runsc", "install it"),
        ];
        let out = render(&checks);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "OK    platform  macOS 26.1 (aarch64)");
        assert_eq!(lines[1], "FAIL  base VM   warren-macos-26 does not exist");
        assert_eq!(lines[2], "                fix: warren mac init");
        assert_eq!(lines[3], "WARN  gvisor    no runsc");
        assert_eq!(lines[4], "                fix: install it");
        let j = serde_json::to_value(&checks[1]).unwrap();
        assert_eq!(j["status"], "FAIL");
        assert_eq!(j["fix"], "warren mac init");
        assert!(
            serde_json::to_value(&checks[0])
                .unwrap()
                .get("fix")
                .is_none()
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(port_of("http://api.warren.localhost:7357"), Some(7357));
        assert_eq!(port_of("https://warren.example.com"), None);
        assert_eq!(port_of("http://[::1]:7337/v1"), Some(7337));
        assert!(free_space(Path::new("/")).unwrap() > 0);
        assert!(free_space(Path::new("/definitely/not/here/warren")).is_some());
    }
}
