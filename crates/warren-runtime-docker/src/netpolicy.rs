//! Network policy for the Docker runtime: the bridge options instances are kept
//! apart with, and the host firewall rules that stop them reaching the cloud
//! metadata service, private networks and the host itself.
//!
//! - Both Warren networks are created with `com.docker.network.bridge.enable_icc=false`
//!   so containers on the same bridge cannot talk to each other (one tenant's
//!   instance could otherwise reach another's gateway on 3737 or terminal on
//!   7681). Host-to-container traffic, which the edge and the daemon use, is not
//!   affected. A pre-existing network without the option is recreated when
//!   nothing is attached to it, and otherwise marked *blocked*: `ready` still
//!   succeeds so existing instances stay manageable, but no new instance is
//!   created on it until the operator stops the instances, runs
//!   `docker network rm <name>` and restarts warrend.
//! - When the daemon runs as root, iptables rules are installed at `ready` for
//!   each Warren bridge interface: `DOCKER-USER` drops forwarded traffic to
//!   [`LINK_LOCAL`] always and, unless `Config::allow_private_egress`, to the
//!   [`PRIVATE_RANGES`]; `INPUT` drops new connections from the bridge to the
//!   host's own addresses (traffic to a local address is delivered through
//!   `INPUT`, so a `DOCKER-USER` rule cannot catch it). Rules are checked with
//!   `-C` before they are inserted, so repeated `ready` calls add nothing. They
//!   are not removed on shutdown. Without root the rules are skipped with one
//!   warning.
//! - With the egress broker enabled, an `INPUT` rule per bridge accepts new TCP
//!   connections to the bridge gateway on the broker port ([`broker_rules`]),
//!   inserted after the drops so it is evaluated first: that is the only way
//!   an `allowlist` or `deny-all` instance (on the internal bridge) reaches
//!   anything, and how an `allow-all` instance reaches its secret routes.
//!
//! The pure parts (rule composition, bridge naming, address parsing) are plain
//! functions so they can be unit-tested without Docker or root.

use std::{
    collections::HashMap,
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

use warren_core::{Error, Result};

/// Bridge driver option that turns inter-container communication off.
pub const OPT_ENABLE_ICC: &str = "com.docker.network.bridge.enable_icc";
/// Bridge driver option naming the host interface; absent for the `br-<id>` default.
pub const OPT_BRIDGE_NAME: &str = "com.docker.network.bridge.name";
/// `pids_limit` of every instance container: a fork bomb cannot take the host down.
pub const PIDS_LIMIT: i64 = 4096;
/// Link-local range, which holds the cloud metadata service (169.254.169.254).
pub const LINK_LOCAL: &str = "169.254.0.0/16";
/// RFC1918 ranges blocked unless `Config::allow_private_egress`.
pub const PRIVATE_RANGES: [&str; 3] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"];
/// Chain Docker reserves for user rules; it is consulted first for forwarded traffic.
pub const CHAIN_FORWARD: &str = "DOCKER-USER";
/// Chain for traffic addressed to the host itself.
pub const CHAIN_INPUT: &str = "INPUT";

/// One iptables rule: the chain and the match/target arguments, without the
/// operation flag (`-C`, `-I`), which [`Rule::argv`] adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    pub chain: &'static str,
    pub args: Vec<String>,
}

impl Rule {
    fn with_target(chain: &'static str, matches: &[&str], target: &str) -> Rule {
        let mut args: Vec<String> = matches.iter().map(|s| s.to_string()).collect();
        args.extend(["-j".to_string(), target.to_string()]);
        Rule { chain, args }
    }

    fn drop(chain: &'static str, matches: &[&str]) -> Rule {
        Self::with_target(chain, matches, "DROP")
    }

    fn accept(chain: &'static str, matches: &[&str]) -> Rule {
        Self::with_target(chain, matches, "ACCEPT")
    }

    /// `[op, chain, args...]`, e.g. `-C DOCKER-USER -i br-x -d 169.254.0.0/16 -j DROP`.
    pub fn argv(&self, op: &str) -> Vec<String> {
        let mut out = Vec::with_capacity(self.args.len() + 2);
        out.push(op.to_string());
        out.push(self.chain.to_string());
        out.extend(self.args.iter().cloned());
        out
    }
}

/// True when the network's options disable inter-container communication.
pub(crate) fn icc_disabled(options: Option<&HashMap<String, String>>) -> bool {
    options
        .and_then(|o| o.get(OPT_ENABLE_ICC))
        .map(|v| v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// The options both Warren networks are created with.
pub(crate) fn network_options() -> HashMap<String, String> {
    HashMap::from([(OPT_ENABLE_ICC.to_string(), "false".to_string())])
}

/// Host interface of a bridge network: the `com.docker.network.bridge.name`
/// option when set, else Docker's `br-<first 12 characters of the id>`.
pub(crate) fn bridge_name(options: Option<&HashMap<String, String>>, id: &str) -> Option<String> {
    if let Some(name) = options
        .and_then(|o| o.get(OPT_BRIDGE_NAME))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Some(name.to_string());
    }
    let id = id.trim();
    if id.len() < 12 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("br-{}", &id[..12]))
}

/// The rules protecting one bridge interface. Link-local is always dropped;
/// private ranges and the host's own addresses only unless `allow_private_egress`.
pub(crate) fn egress_rules(
    bridge: &str,
    allow_private_egress: bool,
    host_addrs: &[Ipv4Addr],
) -> Vec<Rule> {
    let mut rules = vec![Rule::drop(CHAIN_FORWARD, &["-i", bridge, "-d", LINK_LOCAL])];
    if allow_private_egress {
        return rules;
    }
    for range in PRIVATE_RANGES {
        rules.push(Rule::drop(CHAIN_FORWARD, &["-i", bridge, "-d", range]));
    }
    for addr in host_addrs {
        let dest = format!("{addr}/32");
        rules.push(Rule::drop(
            CHAIN_INPUT,
            &[
                "-i",
                bridge,
                "-d",
                &dest,
                "-m",
                "conntrack",
                "--ctstate",
                "NEW",
            ],
        ));
    }
    rules
}

/// The rule letting instances on `bridge` reach the daemon's egress broker:
/// new TCP connections to the bridge gateway (the host's address on that
/// bridge) on `port` are accepted in `INPUT`. Installed after
/// [`egress_rules`], so `-I` places it ahead of the host-address drops.
pub(crate) fn broker_rules(bridge: &str, gateway: Ipv4Addr, port: u16) -> Vec<Rule> {
    let dest = format!("{gateway}/32");
    let port = port.to_string();
    vec![Rule::accept(
        CHAIN_INPUT,
        &[
            "-i",
            bridge,
            "-d",
            &dest,
            "-p",
            "tcp",
            "--dport",
            &port,
            "-m",
            "conntrack",
            "--ctstate",
            "NEW",
        ],
    )]
}

/// An IPv4 gateway as Docker reports it in a network's IPAM config
/// (`172.18.0.1`, sometimes with a prefix length).
pub(crate) fn parse_gateway(s: &str) -> Option<Ipv4Addr> {
    s.trim().split('/').next()?.parse().ok()
}

/// Hint logged and returned when a network exists with ICC on and containers attached.
pub(crate) fn blocked_network_hint(name: &str) -> String {
    format!(
        "docker network {name} exists without {OPT_ENABLE_ICC}=false, so its instances could reach each other; \
         stop the instances on it, run `docker network rm {name}` and restart warrend"
    )
}

/// The effective uid, from the `Uid:` line of `/proc/self/status`.
pub(crate) fn effective_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            let line = text.lines().find(|l| l.starts_with("Uid:"))?;
            line.split_ascii_whitespace().nth(2)?.parse::<u32>().ok()
        })
        .unwrap_or(u32::MAX)
}

/// Effective uid 0.
pub(crate) fn is_root() -> bool {
    effective_uid() == 0
}

const TOOL_DIRS: [&str; 6] = [
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
];

/// Locate an executable on `PATH` or in the usual system directories (the sbin
/// ones hold `iptables` and `ip` and are missing from many user shells).
pub(crate) fn find_tool(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let from_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .iter()
        .map(|d| d.join(name))
        .chain(TOOL_DIRS.iter().map(|d| Path::new(d).join(name)))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Run a command to completion; a non-zero exit is an internal error carrying
/// the command line and stderr.
pub(crate) async fn run(program: &Path, args: &[String]) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| Error::internal(format!("{}: {e}", program.display())))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(Error::internal(format!(
            "{} {} failed ({}): {}",
            program.display(),
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        )))
    }
}

/// Run a command and report whether it succeeded (`iptables -C` style checks).
pub(crate) async fn succeeds(program: &Path, args: &[String]) -> Result<bool> {
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

/// Insert `rule` unless it is already present. Insertion (`-I`) rather than
/// appending keeps the rule ahead of a trailing `RETURN` older Docker versions
/// put in `DOCKER-USER`; all rules are DROPs, so their relative order is moot.
pub(crate) async fn ensure_rule(iptables: &Path, rule: &Rule) -> Result<bool> {
    if succeeds(iptables, &rule.argv("-C")).await? {
        return Ok(false);
    }
    run(iptables, &rule.argv("-I")).await?;
    Ok(true)
}

/// The host's IPv4 addresses that are not loopback, from `ip -j addr`, with
/// `/proc/net/fib_trie` as fallback when `ip` is unavailable.
pub(crate) async fn host_addresses() -> Vec<Ipv4Addr> {
    if let Some(ip) = find_tool("ip") {
        let args = ["-j", "-4", "addr"].map(String::from);
        match run(&ip, &args).await {
            Ok(out) => {
                let addrs = parse_ip_addr_json(&String::from_utf8_lossy(&out));
                if !addrs.is_empty() {
                    return addrs;
                }
            }
            Err(e) => tracing::warn!(error = %e, "ip -j addr failed; reading /proc/net/fib_trie"),
        }
    }
    std::fs::read_to_string("/proc/net/fib_trie")
        .map(|t| parse_fib_trie(&t))
        .unwrap_or_default()
}

/// Addresses from the JSON of `ip -j -4 addr`: every `inet` address whose scope
/// is not `host`, deduplicated, loopback excluded.
pub(crate) fn parse_ip_addr_json(text: &str) -> Vec<Ipv4Addr> {
    let Ok(serde_json::Value::Array(links)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for link in links {
        let Some(infos) = link.get("addr_info").and_then(|v| v.as_array()) else {
            continue;
        };
        for info in infos {
            if info.get("family").and_then(|f| f.as_str()) != Some("inet")
                || info.get("scope").and_then(|s| s.as_str()) == Some("host")
            {
                continue;
            }
            if let Some(addr) = info
                .get("local")
                .and_then(|l| l.as_str())
                .and_then(|l| l.parse::<Ipv4Addr>().ok())
            {
                push_unique(&mut out, addr);
            }
        }
    }
    out
}

/// Local addresses from `/proc/net/fib_trie`: the address line preceding a
/// `/32 host LOCAL` leaf.
pub(crate) fn parse_fib_trie(text: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut last: Option<Ipv4Addr> = None;
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '|', '+', '-']);
        if trimmed.contains("/32 host LOCAL") {
            if let Some(addr) = last {
                push_unique(&mut out, addr);
            }
        } else if let Some(addr) = trimmed
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<Ipv4Addr>().ok())
        {
            last = Some(addr);
        }
    }
    out
}

fn push_unique(out: &mut Vec<Ipv4Addr>, addr: Ipv4Addr) {
    if !addr.is_loopback() && !addr.is_unspecified() && !out.contains(&addr) {
        out.push(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn icc_option_is_recognised() {
        assert!(icc_disabled(Some(&opts(&[(OPT_ENABLE_ICC, "false")]))));
        assert!(icc_disabled(Some(&opts(&[(OPT_ENABLE_ICC, " FALSE ")]))));
        assert!(!icc_disabled(Some(&opts(&[(OPT_ENABLE_ICC, "true")]))));
        assert!(!icc_disabled(Some(&opts(&[]))));
        assert!(!icc_disabled(None));
        assert!(icc_disabled(Some(&network_options())));
    }

    #[test]
    fn bridge_names_follow_option_or_id() {
        let id = "d0821ee986f1aa55bb66cc77dd88ee99ff0011223344556677889900aabbccdd";
        assert_eq!(bridge_name(None, id).as_deref(), Some("br-d0821ee986f1"));
        assert_eq!(
            bridge_name(Some(&opts(&[(OPT_BRIDGE_NAME, "warren0")])), id).as_deref(),
            Some("warren0")
        );
        assert_eq!(
            bridge_name(Some(&opts(&[(OPT_BRIDGE_NAME, " ")])), id).as_deref(),
            Some("br-d0821ee986f1")
        );
        assert_eq!(bridge_name(None, "short"), None);
        assert_eq!(bridge_name(None, "not-hex-not-hex-not-hex"), None);
    }

    #[test]
    fn rules_cover_metadata_private_ranges_and_host() {
        let host = [Ipv4Addr::new(10, 0, 0, 13), Ipv4Addr::new(172, 22, 0, 1)];
        let rules = egress_rules("br-abc", false, &host);
        assert_eq!(rules.len(), 1 + PRIVATE_RANGES.len() + host.len());
        assert_eq!(
            rules[0].argv("-C"),
            [
                "-C",
                "DOCKER-USER",
                "-i",
                "br-abc",
                "-d",
                "169.254.0.0/16",
                "-j",
                "DROP"
            ]
        );
        for (rule, range) in rules[1..].iter().zip(PRIVATE_RANGES) {
            assert_eq!(rule.chain, CHAIN_FORWARD);
            assert_eq!(
                rule.argv("-I"),
                [
                    "-I",
                    "DOCKER-USER",
                    "-i",
                    "br-abc",
                    "-d",
                    range,
                    "-j",
                    "DROP"
                ]
            );
        }
        let host_rule = &rules[1 + PRIVATE_RANGES.len()];
        assert_eq!(host_rule.chain, CHAIN_INPUT);
        assert_eq!(
            host_rule.argv("-C"),
            [
                "-C",
                "INPUT",
                "-i",
                "br-abc",
                "-d",
                "10.0.0.13/32",
                "-m",
                "conntrack",
                "--ctstate",
                "NEW",
                "-j",
                "DROP"
            ]
        );
        // With private egress allowed only the metadata range is dropped.
        let open = egress_rules("br-abc", true, &host);
        assert_eq!(open.len(), 1);
        assert!(open[0].args.contains(&LINK_LOCAL.to_string()));
    }

    #[test]
    fn broker_rule_accepts_the_gateway_port_only() {
        let rules = broker_rules("br-abc", Ipv4Addr::new(172, 18, 0, 1), 7349);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].chain, CHAIN_INPUT);
        assert_eq!(
            rules[0].argv("-I"),
            [
                "-I",
                "INPUT",
                "-i",
                "br-abc",
                "-d",
                "172.18.0.1/32",
                "-p",
                "tcp",
                "--dport",
                "7349",
                "-m",
                "conntrack",
                "--ctstate",
                "NEW",
                "-j",
                "ACCEPT"
            ]
        );
        assert_eq!(
            parse_gateway("172.18.0.1"),
            Some(Ipv4Addr::new(172, 18, 0, 1))
        );
        assert_eq!(
            parse_gateway(" 172.18.0.1/16 "),
            Some(Ipv4Addr::new(172, 18, 0, 1))
        );
        assert_eq!(parse_gateway("fd00::1"), None);
        assert_eq!(parse_gateway(""), None);
    }

    #[test]
    fn parses_ip_addr_json() {
        let json = r#"[
          {"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","scope":"host"}]},
          {"ifname":"eth0","addr_info":[
             {"family":"inet","local":"10.0.0.13","prefixlen":24,"scope":"global"},
             {"family":"inet6","local":"fe80::1","scope":"link"}]},
          {"ifname":"br-x","addr_info":[{"family":"inet","local":"172.22.0.1","scope":"global"},
                                        {"family":"inet","local":"172.22.0.1","scope":"global"}]},
          {"ifname":"down","addr_info":[]}
        ]"#;
        assert_eq!(
            parse_ip_addr_json(json),
            vec![Ipv4Addr::new(10, 0, 0, 13), Ipv4Addr::new(172, 22, 0, 1)]
        );
        assert!(parse_ip_addr_json("not json").is_empty());
        assert!(parse_ip_addr_json("{}").is_empty());
    }

    #[test]
    fn parses_fib_trie() {
        let text = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     +-- 10.0.0.0/24 2 0 2\n        |-- 10.0.0.0\n           /32 link BROADCAST\n           /24 link UNICAST\n        |-- 10.0.0.13\n           /32 host LOCAL\n        |-- 10.0.0.255\n           /32 link BROADCAST\n     +-- 127.0.0.0/8 2 0 2\n        |-- 127.0.0.1\n           /32 host LOCAL\n        |-- 172.22.0.1\n           /32 host LOCAL\nLocal:\n        |-- 10.0.0.13\n           /32 host LOCAL\n";
        assert_eq!(
            parse_fib_trie(text),
            vec![Ipv4Addr::new(10, 0, 0, 13), Ipv4Addr::new(172, 22, 0, 1)]
        );
    }

    #[test]
    fn hint_names_the_fix() {
        let hint = blocked_network_hint("warren-net");
        assert!(hint.contains("docker network rm warren-net"));
        assert!(hint.contains("restart warrend"));
        assert!(hint.contains(OPT_ENABLE_ICC));
    }

    #[test]
    #[cfg(target_os = "linux")] // compares against /proc/self
    fn finds_tools_and_uid() {
        assert!(find_tool("sh").is_some());
        assert!(find_tool("definitely-not-a-tool-zzz").is_none());
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            effective_uid(),
            std::fs::metadata("/proc/self").unwrap().uid()
        );
    }

    #[tokio::test]
    async fn host_addresses_exclude_loopback() {
        let addrs = host_addresses().await;
        assert!(addrs.iter().all(|a| !a.is_loopback()));
        if find_tool("ip").is_some() || Path::new("/proc/net/fib_trie").exists() {
            assert!(!addrs.is_empty(), "a host with networking has an address");
        }
    }

    #[tokio::test]
    async fn run_and_succeeds_report_status() {
        let sh = find_tool("sh").unwrap();
        let out = run(&sh, &["-c".to_string(), "echo hi".to_string()])
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hi");
        let err = run(&sh, &["-c".to_string(), "echo bad >&2; exit 3".to_string()])
            .await
            .unwrap_err();
        assert!(err.is_internal());
        assert!(
            succeeds(&sh, &["-c".to_string(), "true".to_string()])
                .await
                .unwrap()
        );
        assert!(
            !succeeds(&sh, &["-c".to_string(), "false".to_string()])
                .await
                .unwrap()
        );
    }
}
