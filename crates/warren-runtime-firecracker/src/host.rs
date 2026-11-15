//! Privileged host operations: root and tool checks, the `warren-fc0` bridge with
//! forwarding and NAT, tap devices, and per-guest egress rules. Everything shells
//! out to `ip` and `iptables`, which is what the kernel netlink API looks like
//! from a daemon that must also run unprivileged for development.

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Output,
};

use warren_core::{Error, Result};

use crate::net::{BRIDGE, HOST_IP, PREFIX_LEN, SUBNET};

/// The effective uid, from the `Uid:` line of `/proc/self/status` (real,
/// effective, saved, filesystem) with the owner of `/proc/self` as fallback.
pub fn effective_uid() -> u32 {
    let from_status = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            let line = text.lines().find(|l| l.starts_with("Uid:"))?;
            line.split_ascii_whitespace().nth(2)?.parse::<u32>().ok()
        });
    from_status.unwrap_or_else(|| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(u32::MAX)
    })
}

/// Effective uid 0.
pub fn is_root() -> bool {
    effective_uid() == 0
}

/// A clear error when a privileged step is attempted without root.
pub fn require_root(action: &str) -> Result<()> {
    if is_root() {
        Ok(())
    } else {
        Err(Error::not_supported(format!(
            "the firecracker runtime needs root to {action} (running as uid {})",
            effective_uid()
        )))
    }
}

/// Directories searched after `PATH`; the sbin ones are missing from many user
/// shells but hold `mkfs.ext4`, `debugfs`, `ip` and `iptables`.
const EXTRA_DIRS: [&str; 6] = [
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
];

/// Locate an executable on `PATH` or in the usual system directories.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .iter()
        .map(|d| d.join(name))
        .chain(EXTRA_DIRS.iter().map(|d| Path::new(d).join(name)))
        .find(|candidate| is_executable(candidate))
}

/// Locate a tool or fail with a message naming it.
pub fn tool(name: &str) -> Result<PathBuf> {
    find_tool(name)
        .ok_or_else(|| Error::not_supported(format!("{name} is not installed or not on PATH")))
}

/// A regular file with any execute bit set.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// `String::from_utf8_lossy` without the trailing newline.
pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_string()
}

/// Run a command to completion and capture its output; a non-zero exit is an
/// internal error carrying the command line and stderr.
pub async fn run(program: &Path, args: &[&str]) -> Result<Output> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| Error::internal(format!("{}: {e}", program.display())))?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(Error::internal(format!(
            "{} {} failed ({}): {}",
            program.display(),
            args.join(" "),
            out.status,
            lossy(&out.stderr)
        )))
    }
}

/// Run a command and report whether it succeeded; only a failure to launch it
/// is an error. Used for `iptables -C` style existence checks.
pub async fn succeeds(program: &Path, args: &[&str]) -> Result<bool> {
    let status = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| Error::internal(format!("{}: {e}", program.display())))?;
    Ok(status.success())
}

/// True when a network interface of that name exists.
pub fn link_exists(name: &str) -> bool {
    Path::new("/sys/class/net").join(name).exists()
}

/// Bring the guest network up: the bridge with the host address, IPv4
/// forwarding, MASQUERADE for the subnet, and FORWARD accepts for the bridge
/// (Docker sets the FORWARD policy to DROP). Idempotent; needs root.
pub async fn ensure_network() -> Result<()> {
    require_root("configure the guest network")?;
    let ip = tool("ip")?;
    let iptables = tool("iptables")?;

    if !link_exists(BRIDGE) {
        tracing::info!(bridge = BRIDGE, "creating bridge");
        run(&ip, &["link", "add", BRIDGE, "type", "bridge"]).await?;
    }
    let cidr = format!("{HOST_IP}/{PREFIX_LEN}");
    let addrs = run(&ip, &["-4", "-o", "addr", "show", "dev", BRIDGE]).await?;
    if !lossy(&addrs.stdout).contains(&cidr) {
        run(&ip, &["addr", "add", &cidr, "dev", BRIDGE]).await?;
    }
    run(&ip, &["link", "set", BRIDGE, "up"]).await?;

    let forward = Path::new("/proc/sys/net/ipv4/ip_forward");
    if std::fs::read_to_string(forward)
        .map(|v| v.trim() != "1")
        .unwrap_or(true)
    {
        std::fs::write(forward, "1\n")
            .map_err(|e| Error::internal(format!("enable ip_forward: {e}")))?;
    }

    let nat = ["-t", "nat", "POSTROUTING", "-s", SUBNET, "-j", "MASQUERADE"];
    ensure_rule(&iptables, &nat).await?;
    ensure_rule(&iptables, &["FORWARD", "-i", BRIDGE, "-j", "ACCEPT"]).await?;
    ensure_rule(&iptables, &["FORWARD", "-o", BRIDGE, "-j", "ACCEPT"]).await?;
    Ok(())
}

/// `iptables -C <rule>` then `-I` when missing. `rule` starts with optional
/// `-t <table>` followed by the chain and the match.
async fn ensure_rule(iptables: &Path, rule: &[&str]) -> Result<()> {
    if !succeeds(iptables, &with_op(rule, "-C")).await? {
        run(iptables, &with_op(rule, "-I")).await?;
    }
    Ok(())
}

/// Delete a rule if present.
async fn remove_rule(iptables: &Path, rule: &[&str]) -> Result<()> {
    if succeeds(iptables, &with_op(rule, "-C")).await? {
        run(iptables, &with_op(rule, "-D")).await?;
    }
    Ok(())
}

/// Insert the operation flag before the chain name (after any `-t <table>`).
fn with_op<'a>(rule: &[&'a str], op: &'a str) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(rule.len() + 1);
    let mut i = 0;
    if rule.first() == Some(&"-t") && rule.len() >= 2 {
        out.extend_from_slice(&rule[..2]);
        i = 2;
    }
    out.push(op);
    out.extend_from_slice(&rule[i..]);
    out
}

/// Create a tap device attached to the bridge; an existing one is recreated so
/// a stale device from a crashed VM never lingers. Needs root.
pub async fn tap_create(name: &str) -> Result<()> {
    require_root("create tap devices")?;
    let ip = tool("ip")?;
    if link_exists(name) {
        run(&ip, &["link", "del", name]).await?;
    }
    run(&ip, &["tuntap", "add", name, "mode", "tap"]).await?;
    run(&ip, &["link", "set", name, "master", BRIDGE, "up"]).await?;
    Ok(())
}

/// Delete a tap device if it exists. Needs root only when there is one.
pub async fn tap_delete(name: &str) -> Result<()> {
    if !link_exists(name) {
        return Ok(());
    }
    require_root("delete tap devices")?;
    let ip = tool("ip")?;
    run(&ip, &["link", "del", name]).await?;
    Ok(())
}

fn egress_block_rule(guest: &str) -> [&str; 7] {
    ["FORWARD", "-i", BRIDGE, "-s", guest, "-j", "DROP"]
}

/// Block or allow forwarding from one guest; `deny-all` instances get a DROP
/// rule ahead of the bridge accept. Needs root.
pub async fn set_egress(guest: Ipv4Addr, allow: bool) -> Result<()> {
    require_root("change guest egress rules")?;
    let iptables = tool("iptables")?;
    let guest = guest.to_string();
    let rule = egress_block_rule(&guest);
    if allow {
        remove_rule(&iptables, &rule).await
    } else {
        ensure_rule(&iptables, &rule).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_goes_after_table_selector() {
        assert_eq!(
            with_op(&["FORWARD", "-i", "x", "-j", "ACCEPT"], "-C"),
            vec!["-C", "FORWARD", "-i", "x", "-j", "ACCEPT"]
        );
        assert_eq!(
            with_op(
                &[
                    "-t",
                    "nat",
                    "POSTROUTING",
                    "-s",
                    "10.0.0.0/8",
                    "-j",
                    "MASQUERADE"
                ],
                "-I"
            ),
            vec![
                "-t",
                "nat",
                "-I",
                "POSTROUTING",
                "-s",
                "10.0.0.0/8",
                "-j",
                "MASQUERADE"
            ]
        );
    }

    #[test]
    fn finds_common_tools() {
        assert!(find_tool("sh").is_some());
        assert!(find_tool("definitely-not-a-tool-zzz").is_none());
        assert!(!is_executable(Path::new("/etc/hostname")));
        let err = tool("definitely-not-a-tool-zzz").unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn run_reports_failures_with_stderr() {
        let sh = find_tool("sh").unwrap();
        let out = run(&sh, &["-c", "echo hi"]).await.unwrap();
        assert_eq!(lossy(&out.stdout), "hi");
        let err = run(&sh, &["-c", "echo bad >&2; exit 3"]).await.unwrap_err();
        assert!(err.is_internal());
        let cause = err.details.unwrap()["cause"].as_str().unwrap().to_string();
        assert!(
            cause.contains("bad") && cause.contains("exit status: 3"),
            "{cause}"
        );
        assert!(succeeds(&sh, &["-c", "true"]).await.unwrap());
        assert!(!succeeds(&sh, &["-c", "false"]).await.unwrap());
        assert!(run(Path::new("/nonexistent/bin"), &[]).await.is_err());
    }

    #[test]
    fn effective_uid_matches_the_process_owner() {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            effective_uid(),
            std::fs::metadata("/proc/self").unwrap().uid()
        );
    }

    #[test]
    fn root_requirement_is_clear_when_unprivileged() {
        if is_root() {
            require_root("test").unwrap();
        } else {
            let err = require_root("create tap devices").unwrap_err();
            assert_eq!(err.code, warren_core::errors::CODE_NOT_SUPPORTED);
            assert!(err.message.contains("needs root to create tap devices"));
        }
    }

    #[tokio::test]
    async fn network_setup_requires_root_or_works() {
        if !is_root() {
            let err = ensure_network().await.unwrap_err();
            assert!(err.message.contains("needs root"));
            assert!(tap_create("tap-test0").await.is_err());
            // Deleting a tap that does not exist needs no privilege.
            tap_delete("tap-does-not-exist").await.unwrap();
            return;
        }
        ensure_network().await.unwrap();
        ensure_network().await.unwrap();
        assert!(link_exists(BRIDGE));
        tap_create("tap-wtest0").await.unwrap();
        tap_create("tap-wtest0").await.unwrap();
        assert!(link_exists("tap-wtest0"));
        tap_delete("tap-wtest0").await.unwrap();
        assert!(!link_exists("tap-wtest0"));
        let guest = Ipv4Addr::new(172, 30, 0, 253);
        set_egress(guest, false).await.unwrap();
        set_egress(guest, false).await.unwrap();
        set_egress(guest, true).await.unwrap();
        set_egress(guest, true).await.unwrap();
    }
}
