//! PID 1 mode for Firecracker microVMs (`warren-gateway --init`).
//!
//! The init process mounts the pseudo filesystems, loads `/etc/warren.env`,
//! sets the hostname, brings networking up, writes `/etc/resolv.conf`, then
//! runs the normal server as a child process (`--serve`) and spends the rest
//! of its life reaping zombies and restarting the server if it dies. Keeping
//! the server out of PID 1 means the blocking `waitpid(-1)` reaper can never
//! steal exit statuses from the server's own child processes.
//!
//! Workspace images injected by the Firecracker runtime carry
//! [`WRAP_FILE`] (`/etc/warren.wrap.json`): a JSON array with the image's own
//! entrypoint and command. When it names a program, the server is started in
//! wrap mode (`--serve --wrap -- <argv...>`) so it supervises that program
//! while serving the API (see `wrap.rs`).
//!
//! `POST /v1/shutdown` on the server stops the wrapped program, if any, then
//! calls [`power_off`]: `sync()` followed by `reboot(RB_POWER_OFF)`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    mount::{MsFlags, mount},
    sys::{
        reboot::{RebootMode, reboot},
        signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction},
        wait::waitpid,
    },
    unistd::{getpid, sync},
};
use warren_core::{Error, Result, types::ENV_INSTANCE_ID};

use crate::{config::ENV_SHUTDOWN_ENABLED, exec::which};

/// Environment file written into the rootfs by the runtime.
pub const ENV_FILE: &str = "/etc/warren.env";
/// The image's own entrypoint and command, written into injected rootfs
/// images by the runtime as one JSON array of strings; absent in the
/// built-in images. An empty array means nothing to supervise.
pub const WRAP_FILE: &str = "/etc/warren.wrap.json";
/// Host side of the Firecracker bridge, used as the default route.
pub const DEFAULT_GATEWAY_IP: &str = "172.30.0.1";
/// Resolver written to `/etc/resolv.conf`.
pub const NAMESERVER: &str = "1.1.1.1";

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

struct MountSpec {
    source: &'static str,
    target: &'static str,
    fstype: &'static str,
    flags: MsFlags,
    data: Option<&'static str>,
}

fn mounts() -> Vec<MountSpec> {
    let std_flags = MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    vec![
        MountSpec {
            source: "proc",
            target: "/proc",
            fstype: "proc",
            flags: std_flags,
            data: None,
        },
        MountSpec {
            source: "sysfs",
            target: "/sys",
            fstype: "sysfs",
            flags: std_flags,
            data: None,
        },
        MountSpec {
            source: "devtmpfs",
            target: "/dev",
            fstype: "devtmpfs",
            flags: MsFlags::MS_NOSUID,
            data: Some("mode=0755"),
        },
        MountSpec {
            source: "devpts",
            target: "/dev/pts",
            fstype: "devpts",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            data: Some("gid=5,mode=620,ptmxmode=666"),
        },
        MountSpec {
            source: "tmpfs",
            target: "/dev/shm",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=1777"),
        },
        MountSpec {
            source: "tmpfs",
            target: "/tmp",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=1777"),
        },
        MountSpec {
            source: "tmpfs",
            target: "/run",
            fstype: "tmpfs",
            flags: MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=0755"),
        },
    ]
}

/// Run as PID 1: prepare the machine, start the server, reap forever.
/// Returns only when the server cannot be started at all.
pub fn run_init() -> Result<()> {
    if getpid().as_raw() != 1 {
        tracing::warn!(
            pid = getpid().as_raw(),
            "--init but not PID 1; running the init steps best-effort"
        );
    }
    mount_filesystems();
    let env = load_env_file(Path::new(ENV_FILE));
    let instance_id = env
        .get(ENV_INSTANCE_ID)
        .cloned()
        .or_else(|| std::env::var(ENV_INSTANCE_ID).ok())
        .unwrap_or_default();
    set_hostname(&instance_id);
    configure_network();
    write_resolv_conf();
    install_signal_handlers();
    let wrap = load_wrap_file(Path::new(WRAP_FILE));
    supervise_server(&env, &wrap)
}

fn mount_filesystems() {
    for m in mounts() {
        if let Err(e) = std::fs::create_dir_all(m.target) {
            tracing::debug!(target = m.target, error = %e, "cannot create mount point");
        }
        match mount(Some(m.source), m.target, Some(m.fstype), m.flags, m.data) {
            Ok(()) => tracing::info!(target = m.target, fstype = m.fstype, "mounted"),
            Err(Errno::EBUSY) => tracing::debug!(target = m.target, "already mounted"),
            Err(e) => {
                tracing::warn!(target = m.target, fstype = m.fstype, error = %e, "mount failed")
            }
        }
    }
}

/// Parse `KEY=VALUE` lines (`#` comments, optional `export`, optional quotes).
pub fn parse_env_file(content: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c == '\0') {
            continue;
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        out.insert(key.to_string(), value.to_string());
    }
    out
}

/// Read and parse the environment file; missing or unreadable means empty.
pub fn load_env_file(path: &Path) -> BTreeMap<String, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let env = parse_env_file(&content);
            tracing::info!(path = %path.display(), entries = env.len(), "loaded environment file");
            env
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "environment file not found");
            BTreeMap::new()
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read environment file");
            BTreeMap::new()
        }
    }
}

/// Parse the wrap file: a JSON array of strings.
pub fn parse_wrap_file(content: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| Error::invalid(format!("wrap file is not valid JSON: {e}")))?;
    let Some(items) = value.as_array() else {
        return Err(Error::invalid("wrap file must hold a JSON array"));
    };
    items
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| Error::invalid("wrap file array must contain strings only"))
        })
        .collect()
}

/// Read and parse the wrap file; missing, unreadable or invalid means
/// nothing to wrap (invalid content is logged).
pub fn load_wrap_file(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => match parse_wrap_file(&content) {
            Ok(argv) => {
                tracing::info!(path = %path.display(), ?argv, "loaded wrap file");
                argv
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "ignoring invalid wrap file");
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read wrap file");
            Vec::new()
        }
    }
}

fn set_hostname(name: &str) {
    if name.is_empty() {
        tracing::warn!("{ENV_INSTANCE_ID} not set; leaving the hostname alone");
        return;
    }
    match std::fs::write("/proc/sys/kernel/hostname", name) {
        Ok(()) => tracing::info!(hostname = name, "hostname set"),
        Err(e) => tracing::warn!(error = %e, "cannot set hostname"),
    }
    if let Err(e) = std::fs::write("/etc/hostname", format!("{name}\n")) {
        tracing::debug!(error = %e, "cannot write /etc/hostname");
    }
}

/// True when `/proc/net/route` content lists a default (0.0.0.0/0) route.
pub fn has_default_route(route_table: &str) -> bool {
    route_table.lines().skip(1).any(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        f.len() >= 8 && f[1] == "00000000" && f[7] == "00000000"
    })
}

fn run_quiet(bin: &Path, args: &[&str]) -> bool {
    match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(cmd = %bin.display(), ?args, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "command failed");
            false
        }
        Err(e) => {
            tracing::warn!(cmd = %bin.display(), ?args, error = %e, "cannot run command");
            false
        }
    }
}

fn configure_network() {
    let Some(ip) = which("ip") else {
        tracing::warn!("ip command not found; skipping network setup");
        return;
    };
    run_quiet(&ip, &["link", "set", "lo", "up"]);
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "lo" {
                run_quiet(&ip, &["link", "set", &name, "up"]);
            }
        }
    }
    let table = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    if has_default_route(&table) {
        tracing::info!("default route present");
    } else if run_quiet(&ip, &["route", "add", "default", "via", DEFAULT_GATEWAY_IP]) {
        tracing::info!(via = DEFAULT_GATEWAY_IP, "default route added");
    }
}

fn write_resolv_conf() {
    let path = Path::new("/etc/resolv.conf");
    // It may be a dangling symlink into /run; replace it with a plain file.
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(path);
    }
    match std::fs::write(path, format!("nameserver {NAMESERVER}\n")) {
        Ok(()) => tracing::info!(nameserver = NAMESERVER, "resolv.conf written"),
        Err(e) => tracing::warn!(error = %e, "cannot write /etc/resolv.conf"),
    }
}

extern "C" fn on_signal(_signal: nix::libc::c_int) {
    // Only an atomic store: async-signal-safe.
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    let action = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        // SAFETY: the handler only performs an atomic store, which is
        // async-signal-safe, and no other code installs handlers for these
        // signals in init mode.
        if let Err(e) = unsafe { sigaction(sig, &action) } {
            tracing::warn!(?sig, error = %e, "cannot install signal handler");
        }
    }
}

fn server_binary() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        return Ok(exe);
    }
    std::env::args()
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| Error::internal("cannot determine the gateway binary path"))
}

/// Arguments of the server process: `--serve`, plus `--wrap -- <argv...>`
/// when the wrap file names a program.
pub fn server_args(wrap: &[String]) -> Vec<String> {
    let mut args = vec!["--serve".to_string()];
    if !wrap.is_empty() {
        args.push("--wrap".to_string());
        args.push("--".to_string());
        args.extend(wrap.iter().cloned());
    }
    args
}

fn spawn_server(
    exe: &Path,
    env: &BTreeMap<String, String>,
    wrap: &[String],
) -> std::io::Result<std::process::Child> {
    Command::new(exe)
        .args(server_args(wrap))
        .envs(env)
        .env(ENV_SHUTDOWN_ENABLED, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}

fn supervise_server(env: &BTreeMap<String, String>, wrap: &[String]) -> Result<()> {
    let exe = server_binary()?;
    let mut child = spawn_server(&exe, env, wrap)
        .map_err(|e| Error::internal(format!("cannot start the gateway server: {e}")))?;
    let mut server_pid = child.id() as i32;
    let mut started = Instant::now();
    let mut backoff = crate::terminal::MIN_BACKOFF;
    tracing::info!(pid = server_pid, wrapping = ?wrap, "gateway server started");
    loop {
        if SHUTDOWN_REQUESTED.swap(false, Ordering::SeqCst) {
            tracing::warn!("termination signal received by init; powering off");
            let _ = child.kill();
            power_off()?;
        }
        match waitpid(None, None) {
            Ok(status) => {
                let Some(pid) = status.pid() else { continue };
                if pid.as_raw() != server_pid {
                    tracing::debug!(pid = pid.as_raw(), ?status, "reaped orphan");
                    continue;
                }
                tracing::error!(?status, "gateway server exited; restarting");
                if started.elapsed() >= crate::terminal::STABLE_RUN {
                    backoff = crate::terminal::MIN_BACKOFF;
                }
                std::thread::sleep(backoff);
                backoff = crate::terminal::next_backoff(backoff);
                match spawn_server(&exe, env, wrap) {
                    Ok(c) => {
                        child = c;
                        server_pid = child.id() as i32;
                        started = Instant::now();
                        tracing::info!(pid = server_pid, "gateway server restarted");
                    }
                    Err(e) => {
                        return Err(Error::internal(format!(
                            "cannot restart the gateway server: {e}"
                        )));
                    }
                }
            }
            Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) => {
                tracing::error!("no child processes left; restarting the gateway server");
                std::thread::sleep(backoff);
                backoff = crate::terminal::next_backoff(backoff);
                child = spawn_server(&exe, env, wrap).map_err(|e| {
                    Error::internal(format!("cannot restart the gateway server: {e}"))
                })?;
                server_pid = child.id() as i32;
                started = Instant::now();
            }
            Err(e) => {
                tracing::warn!(error = %e, "waitpid failed");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Flush filesystems and power the machine off. Returns only on failure.
pub fn power_off() -> Result<()> {
    sync();
    match reboot(RebootMode::RB_POWER_OFF) {
        Ok(never) => match never {},
        Err(e) => Err(Error::internal(format!("reboot(RB_POWER_OFF) failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let env = parse_env_file(
            "# comment\n\nWARREN_INSTANCE_ID=ab12cd34ef\nexport OPENAI_API_KEY=\"sk-x\"\nWARREN_MODEL='m'\nBAD LINE\n=novalue\nEMPTY=\n",
        );
        assert_eq!(env.get("WARREN_INSTANCE_ID").unwrap(), "ab12cd34ef");
        assert_eq!(env.get("OPENAI_API_KEY").unwrap(), "sk-x");
        assert_eq!(env.get("WARREN_MODEL").unwrap(), "m");
        assert_eq!(env.get("EMPTY").unwrap(), "");
        assert_eq!(env.len(), 4);
        assert!(load_env_file(Path::new("/definitely/missing/warren.env")).is_empty());
    }

    #[test]
    fn wrap_file_parsing_and_server_arguments() {
        assert_eq!(
            parse_wrap_file("[\"docker-entrypoint.sh\",\"redis-server\"]\n").unwrap(),
            vec![
                "docker-entrypoint.sh".to_string(),
                "redis-server".to_string()
            ]
        );
        assert!(parse_wrap_file("[]\n").unwrap().is_empty());
        assert_eq!(parse_wrap_file("not json").unwrap_err().status, 400);
        assert_eq!(parse_wrap_file("{\"a\":1}").unwrap_err().status, 400);
        assert_eq!(parse_wrap_file("[\"sh\", 1]").unwrap_err().status, 400);
        assert!(load_wrap_file(Path::new("/definitely/missing/warren.wrap.json")).is_empty());
        let dir =
            std::env::temp_dir().join(format!("warren-gw-init-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("warren.wrap.json");
        std::fs::write(&file, "[\"/bin/sh\",\"-c\",\"exit 0\"]\n").unwrap();
        assert_eq!(load_wrap_file(&file), ["/bin/sh", "-c", "exit 0"]);
        std::fs::write(&file, "[1]").unwrap();
        assert!(load_wrap_file(&file).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(server_args(&[]), ["--serve"]);
        assert_eq!(
            server_args(&["/bin/sh".to_string(), "--init".to_string()]),
            ["--serve", "--wrap", "--", "/bin/sh", "--init"]
        );
    }

    #[test]
    fn default_route_detection() {
        let with = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\neth0\t00000000\t01001EAC\t0003\t0\t0\t0\t00000000\t0\t0\t0\neth0\t00001EAC\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert!(has_default_route(with));
        let without = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\neth0\t00001EAC\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert!(!has_default_route(without));
        assert!(!has_default_route(""));
    }

    #[test]
    fn power_off_fails_without_privileges() {
        if !crate::exec::is_root() {
            assert!(power_off().is_err());
        }
    }
}
