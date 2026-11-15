//! Privileged host operations: root and tool checks, the `warren-fc0` bridge with
//! forwarding and NAT, tap devices, and per-guest egress rules. Everything shells
//! out to `ip` and `iptables`, which is what the kernel netlink API looks like
//! from a daemon that must also run unprivileged for development.
//!
//! Guest isolation: every tap is a *isolated* bridge port, so frames are only
//! forwarded between a tap and the bridge itself, never tap to tap, and guests
//! cannot reach each other. Forwarded traffic from the guest subnet to the
//! link-local range (the cloud metadata service) is dropped always, and to the
//! RFC1918 ranges unless `Config::allow_private_egress`; new connections from
//! the guest subnet to the host's own addresses are dropped in `INPUT` under
//! the same flag (traffic to a local address never traverses `FORWARD`). The
//! rules are checked with `-C` before they are inserted and are left in place
//! on shutdown.

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

/// Link-local range, which holds the cloud metadata service (169.254.169.254).
pub const LINK_LOCAL: &str = "169.254.0.0/16";
/// RFC1918 ranges guests may not reach unless `Config::allow_private_egress`.
pub const PRIVATE_RANGES: [&str; 3] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"];

/// `ip link set dev <tap> type bridge_slave isolated on`: the arguments that make
/// a tap an isolated bridge port (no tap-to-tap forwarding).
pub fn tap_isolate_args(tap: &str) -> [&str; 8] {
    [
        "link",
        "set",
        "dev",
        tap,
        "type",
        "bridge_slave",
        "isolated",
        "on",
    ]
}

/// The egress policy rules for the guest subnet (chain first, no operation
/// flag). Link-local is always dropped in `FORWARD`; with private egress
/// disallowed the RFC1918 ranges join it and new connections to each host
/// address are dropped in `INPUT`.
pub fn egress_policy_rules(
    allow_private_egress: bool,
    host_addrs: &[Ipv4Addr],
) -> Vec<Vec<String>> {
    let forward_drop = |dest: &str| -> Vec<String> {
        [
            "FORWARD", "-i", BRIDGE, "-s", SUBNET, "-d", dest, "-j", "DROP",
        ]
        .map(String::from)
        .to_vec()
    };
    let mut rules = vec![forward_drop(LINK_LOCAL)];
    if allow_private_egress {
        return rules;
    }
    rules.extend(PRIVATE_RANGES.iter().map(|r| forward_drop(r)));
    for addr in host_addrs {
        let dest = format!("{addr}/32");
        rules.push(
            [
                "INPUT",
                "-i",
                BRIDGE,
                "-s",
                SUBNET,
                "-d",
                &dest,
                "-m",
                "conntrack",
                "--ctstate",
                "NEW",
                "-j",
                "DROP",
            ]
            .map(String::from)
            .to_vec(),
        );
    }
    rules
}

/// Addresses from the JSON of `ip -j -4 addr`: every `inet` address whose scope
/// is not `host`, deduplicated, loopback excluded.
pub fn parse_ip_addr_json(text: &str) -> Vec<Ipv4Addr> {
    let Ok(serde_json::Value::Array(links)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Vec::new();
    };
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for info in links
        .iter()
        .filter_map(|l| l.get("addr_info")?.as_array())
        .flatten()
    {
        if info.get("family").and_then(|f| f.as_str()) != Some("inet")
            || info.get("scope").and_then(|s| s.as_str()) == Some("host")
        {
            continue;
        }
        if let Some(addr) = info
            .get("local")
            .and_then(|l| l.as_str())
            .and_then(|l| l.parse::<Ipv4Addr>().ok())
            && !addr.is_loopback()
            && !out.contains(&addr)
        {
            out.push(addr);
        }
    }
    out
}

/// The host's non-loopback IPv4 addresses, via `ip -j -4 addr`.
pub async fn host_addresses(ip: &Path) -> Result<Vec<Ipv4Addr>> {
    let out = run(ip, &["-j", "-4", "addr"]).await?;
    Ok(parse_ip_addr_json(&String::from_utf8_lossy(&out.stdout)))
}

/// Bring the guest network up: the bridge with the host address, IPv4
/// forwarding, MASQUERADE for the subnet, FORWARD accepts for the bridge
/// (Docker sets the FORWARD policy to DROP) and, above them, the egress policy
/// drops of [`egress_policy_rules`]. Idempotent; needs root.
pub async fn ensure_network(allow_private_egress: bool) -> Result<()> {
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
    // Inserted after the accepts, so `-I` places them ahead.
    let host_addrs = host_addresses(&ip).await?;
    if host_addrs.is_empty() && !allow_private_egress {
        tracing::warn!("no host addresses found; guests are not kept from reaching the host");
    }
    for rule in egress_policy_rules(allow_private_egress, &host_addrs) {
        let parts: Vec<&str> = rule.iter().map(String::as_str).collect();
        ensure_rule(&iptables, &parts).await?;
    }
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
    // Isolated port: the kernel never forwards frames between two isolated
    // ports, so guests cannot reach each other even on the same bridge.
    run(&ip, &tap_isolate_args(name)).await?;
    if !tap_is_isolated(name).await? {
        let _ = run(&ip, &["link", "del", name]).await;
        return Err(Error::internal(format!(
            "tap {name} did not become an isolated bridge port; guests could reach each other"
        )));
    }
    Ok(())
}

/// True when the bridge port `name` is isolated (from `ip -d -j link show`).
pub async fn tap_is_isolated(name: &str) -> Result<bool> {
    let ip = tool("ip")?;
    let out = run(&ip, &["-d", "-j", "link", "show", "dev", name]).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::internal(format!("ip -d -j link show {name}: {e}")))?;
    Ok(parsed
        .as_array()
        .and_then(|a| a.first())
        .and_then(|l| l.get("linkinfo")?.get("info_slave_data")?.get("isolated"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
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
    fn tap_isolation_command_is_composed() {
        assert_eq!(
            tap_isolate_args("tap-ab12cd34").join(" "),
            "link set dev tap-ab12cd34 type bridge_slave isolated on"
        );
    }

    #[test]
    fn egress_policy_covers_metadata_private_ranges_and_host() {
        let host = [Ipv4Addr::new(10, 0, 0, 13), Ipv4Addr::new(172, 30, 0, 1)];
        let rules = egress_policy_rules(false, &host);
        assert_eq!(rules.len(), 1 + PRIVATE_RANGES.len() + host.len());
        assert_eq!(
            rules[0].join(" "),
            "FORWARD -i warren-fc0 -s 172.30.0.0/24 -d 169.254.0.0/16 -j DROP"
        );
        for (rule, range) in rules[1..].iter().zip(PRIVATE_RANGES) {
            assert_eq!(
                rule.join(" "),
                format!("FORWARD -i warren-fc0 -s 172.30.0.0/24 -d {range} -j DROP")
            );
        }
        assert_eq!(
            rules[1 + PRIVATE_RANGES.len()].join(" "),
            "INPUT -i warren-fc0 -s 172.30.0.0/24 -d 10.0.0.13/32 -m conntrack --ctstate NEW -j DROP"
        );
        // `-C`/`-I` go in front of the chain name.
        let parts: Vec<&str> = rules[0].iter().map(String::as_str).collect();
        assert_eq!(with_op(&parts, "-C")[..2], ["-C", "FORWARD"]);
        // Private egress allowed: only the metadata range is dropped.
        let open = egress_policy_rules(true, &host);
        assert_eq!(open.len(), 1);
        assert!(open[0].contains(&LINK_LOCAL.to_string()));
    }

    #[test]
    fn parses_ip_addr_json() {
        let json = r#"[
          {"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","scope":"host"}]},
          {"ifname":"eth0","addr_info":[
             {"family":"inet","local":"10.0.0.13","scope":"global"},
             {"family":"inet6","local":"fe80::1","scope":"link"}]},
          {"ifname":"warren-fc0","addr_info":[{"family":"inet","local":"172.30.0.1","scope":"global"},
                                              {"family":"inet","local":"172.30.0.1","scope":"global"}]}
        ]"#;
        assert_eq!(
            parse_ip_addr_json(json),
            vec![Ipv4Addr::new(10, 0, 0, 13), Ipv4Addr::new(172, 30, 0, 1)]
        );
        assert!(parse_ip_addr_json("nope").is_empty());
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
            let err = ensure_network(false).await.unwrap_err();
            assert!(err.message.contains("needs root"));
            assert!(tap_create("tap-test0").await.is_err());
            // Deleting a tap that does not exist needs no privilege.
            tap_delete("tap-does-not-exist").await.unwrap();
            eprintln!("skipping the live bridge, tap isolation and iptables checks: needs root");
            return;
        }
        ensure_network(false).await.unwrap();
        ensure_network(false).await.unwrap();
        assert!(link_exists(BRIDGE));
        let iptables = tool("iptables").unwrap();
        let ip = tool("ip").unwrap();
        let host_addrs = host_addresses(&ip).await.unwrap();
        assert!(!host_addrs.is_empty());
        for rule in egress_policy_rules(false, &host_addrs) {
            let parts: Vec<&str> = rule.iter().map(String::as_str).collect();
            assert!(
                succeeds(&iptables, &with_op(&parts, "-C")).await.unwrap(),
                "rule present: {}",
                rule.join(" ")
            );
        }
        tap_create("tap-wtest0").await.unwrap();
        tap_create("tap-wtest0").await.unwrap();
        assert!(link_exists("tap-wtest0"));
        assert!(
            tap_is_isolated("tap-wtest0").await.unwrap(),
            "the tap is an isolated bridge port"
        );
        tap_delete("tap-wtest0").await.unwrap();
        assert!(!link_exists("tap-wtest0"));
        let guest = Ipv4Addr::new(172, 30, 0, 253);
        set_egress(guest, false).await.unwrap();
        set_egress(guest, false).await.unwrap();
        set_egress(guest, true).await.unwrap();
        set_egress(guest, true).await.unwrap();
    }
}
