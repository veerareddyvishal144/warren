use std::{collections::BTreeMap, path::PathBuf};

use crate::{InstanceType, Shape, Template, TemplateScope, types::*};

/// Default uid and gid the Firecracker jailer drops to: a dedicated id above
/// the ranges distributions hand to system and login users, below `nobody`.
pub const DEFAULT_FIRECRACKER_JAIL_ID: u32 = 61000;
/// Default `firecracker_rootfs_cache_gb`.
pub const DEFAULT_FIRECRACKER_ROOTFS_CACHE_GB: u32 = 20;

/// Daemon configuration. Built by `warrend` from flags and `WARREN_*` variables;
/// defaults are safe for a single developer machine.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    /// "warren.localhost": api.<base>, <id>.<base>, <id>-<port>.<base>.
    pub base_domain: String,
    pub public_scheme: String,
    /// Port clients use in URLs; 0 omits it.
    pub public_port: u16,
    pub data_dir: PathBuf,
    /// Docker runtime for default instances: "auto" | "runsc" | "runc", or
    /// "none" to run without Docker (a macOS host serving only `mac` instances).
    pub runtime: String,
    pub region: String,
    pub enable_firecracker: bool,
    pub firecracker_bin: PathBuf,
    pub kernel_image: Option<PathBuf>,
    /// The Firecracker `jailer` binary. When set, every microVM is launched
    /// through it: a chroot per instance under `firecracker_jail_dir()`, the
    /// process dropped to `firecracker_jail_uid`/`gid`, seccomp on. None
    /// launches `firecracker_bin` directly as root (the default).
    pub firecracker_jailer: Option<PathBuf>,
    /// Unprivileged uid and gid the jailer switches to; they need not exist
    /// in `/etc/passwd`.
    pub firecracker_jail_uid: u32,
    pub firecracker_jail_gid: u32,
    /// Apply the per-shape token-bucket limits to each microVM's block device
    /// and NIC. On by default; `--firecracker-no-rate-limits` turns it off.
    pub firecracker_rate_limits: bool,
    /// Disk budget in GiB for packed root filesystems kept under
    /// `firecracker_rootfs_cache_dir()` and shared by `vm` instances of the
    /// same image; 0 builds every root filesystem from its image.
    pub firecracker_rootfs_cache_gb: u32,
    /// Base URL of the Lume API server (`lume serve`) that hosts `mac` instances.
    pub lume_api_url: String,
    /// The `lume` executable, started as `lume serve` when the API is down.
    pub lume_bin: PathBuf,
    /// Name of the stopped Lume VM every `mac` instance is cloned from. None
    /// leaves the `mac` type disabled.
    pub mac_base_vm: Option<String>,
    /// Start `mac` instances with Lume's VNC server on (a random password per
    /// run, reachable from the host only). Off by default.
    pub mac_vnc: bool,
    /// Names of the runtimes that came up, as reported by `GET /v1/health`
    /// ("docker", "firecracker", "lume"). Filled by the daemon after startup.
    pub runtime_names: Vec<String>,
    /// The static `warren-gateway` binary the runtimes inject into images that do
    /// not ship it (registered workspace templates). None disables injection.
    pub gateway_bin: Option<PathBuf>,
    /// Dev top-ups without a payment provider, short idle timeouts, host.docker.internal.
    pub dev_mode: bool,
    /// Whether POST /v1/signup is served. Hosted deployments usually gate signup
    /// behind their own flow and turn this off.
    pub signup_enabled: bool,
    /// Let instances reach RFC 1918 ranges and the host's own addresses. Off by
    /// default; the cloud metadata range 169.254.0.0/16 is always blocked when the
    /// daemon can install firewall rules.
    pub allow_private_egress: bool,
    /// Publish each Docker instance's gateway and terminal ports on 127.0.0.1
    /// and reach it there instead of its container address; needed on macOS
    /// hosts (Docker Desktop, the smolvm engine), where the host has no route
    /// to the container networks. Defaults to on for macOS hosts.
    pub docker_publish_ports: bool,
    /// Sleep Docker instances with their memory: checkpoint the container
    /// through the engine's checkpoint API (gVisor's `runsc checkpoint`) and
    /// restore it on wake, so processes keep running with their pids. Needs
    /// an engine in experimental mode; otherwise sleep keeps the disk only.
    /// Off by default.
    pub docker_checkpoints: bool,
    /// Where the egress broker listens (`host:port`); it must be reachable from
    /// the instances' networks (the Docker bridge, `warren-fc0`, the Lume NAT).
    /// Empty or `none` runs the daemon without a broker, which refuses
    /// `allowlist` instances and secrets.
    pub broker_listen: String,
    /// `host:port` instances use to reach the broker. None derives it per
    /// runtime (the bridge gateway for Docker, 172.30.0.1 for Firecracker,
    /// 192.168.64.1 for Lume) with the port of `broker_listen`.
    pub broker_advertise_addr: Option<String>,
    pub min_idle_timeout_sec: u32,
    /// Instances a workspace may keep awake at once; None means unlimited.
    pub max_awake_instances: Option<u32>,
    /// HMAC secret for signed URLs.
    pub secret: String,
    /// Tag of the built-in template images, e.g. "dev".
    pub image_tag: String,
    pub version: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: ":7337".into(),
            base_domain: "warren.localhost".into(),
            public_scheme: "http".into(),
            public_port: 7337,
            data_dir: PathBuf::from("/var/lib/warren"),
            runtime: "auto".into(),
            region: "local".into(),
            enable_firecracker: false,
            firecracker_bin: PathBuf::from("/usr/local/bin/firecracker"),
            kernel_image: None,
            firecracker_jailer: None,
            firecracker_jail_uid: DEFAULT_FIRECRACKER_JAIL_ID,
            firecracker_jail_gid: DEFAULT_FIRECRACKER_JAIL_ID,
            firecracker_rate_limits: true,
            firecracker_rootfs_cache_gb: DEFAULT_FIRECRACKER_ROOTFS_CACHE_GB,
            lume_api_url: "http://127.0.0.1:7777".into(),
            lume_bin: PathBuf::from("lume"),
            mac_base_vm: None,
            mac_vnc: false,
            runtime_names: Vec::new(),
            gateway_bin: None,
            dev_mode: false,
            signup_enabled: true,
            allow_private_egress: false,
            docker_publish_ports: cfg!(target_os = "macos"),
            docker_checkpoints: false,
            broker_listen: crate::broker::DEFAULT_BROKER_LISTEN.into(),
            broker_advertise_addr: None,
            min_idle_timeout_sec: MIN_IDLE_TIMEOUT_SEC,
            max_awake_instances: None,
            secret: String::new(),
            image_tag: "dev".into(),
            version: "dev".into(),
        }
    }
}

/// Classification of a request Host header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKind {
    Root,
    Api,
    /// `port` 0 means the template's default port.
    Instance {
        id: String,
        port: u16,
    },
}

impl Config {
    fn host_port(&self, host: &str) -> String {
        let implicit = self.public_port == 0
            || (self.public_scheme == "http" && self.public_port == 80)
            || (self.public_scheme == "https" && self.public_port == 443);
        if implicit {
            host.to_string()
        } else {
            format!("{host}:{}", self.public_port)
        }
    }

    pub fn api_host(&self) -> String {
        format!("api.{}", self.base_domain)
    }

    /// Public base URL of the Sandbox API, without a trailing slash.
    pub fn api_url(&self) -> String {
        format!(
            "{}://{}/v1",
            self.public_scheme,
            self.host_port(&self.api_host())
        )
    }

    /// `scheme://{id}.<base>[:port]`
    pub fn instance_url(&self, id: &str) -> String {
        format!(
            "{}://{}",
            self.public_scheme,
            self.host_port(&format!("{id}.{}", self.base_domain))
        )
    }

    /// `scheme://{id}-{port}.<base>[:port]`
    pub fn preview_url(&self, id: &str, port: u16) -> String {
        format!(
            "{}://{}",
            self.public_scheme,
            self.host_port(&format!("{id}-{port}.{}", self.base_domain))
        )
    }

    /// Classify a Host header. Ports in the header are ignored.
    pub fn parse_host(&self, host_header: &str) -> HostKind {
        let mut host = host_header.trim().to_ascii_lowercase();
        if host.starts_with('[') {
            // [::1]:7337
            if let Some(end) = host.find(']') {
                host = host[..=end].to_string();
            }
        } else if let Some(i) = host.rfind(':')
            && host[i + 1..].chars().all(|c| c.is_ascii_digit())
        {
            host.truncate(i);
        }
        let base = self.base_domain.to_ascii_lowercase();
        if host == base
            || host == "localhost"
            || host == "127.0.0.1"
            || host == "[::1]"
            || host == "::1"
        {
            return HostKind::Root;
        }
        if host == format!("api.{base}") {
            return HostKind::Api;
        }
        let Some(label) = host.strip_suffix(&format!(".{base}")) else {
            return HostKind::Root;
        };
        if label.contains('.') {
            return HostKind::Root;
        }
        if let Some(i) = label.rfind('-') {
            let (id, port) = (&label[..i], &label[i + 1..]);
            if id.len() == INSTANCE_ID_LENGTH
                && let Ok(p) = port.parse::<u16>()
                && p > 0
            {
                return HostKind::Instance {
                    id: id.to_string(),
                    port: p,
                };
            }
        }
        if label.len() == INSTANCE_ID_LENGTH && crate::ids::valid_instance_id(label) {
            return HostKind::Instance {
                id: label.to_string(),
                port: 0,
            };
        }
        HostKind::Root
    }

    /// Home directory of the user inside a `mac` instance: the account Lume's
    /// unattended macOS install creates.
    pub const MAC_HOME: &'static str = "/Users/lume";

    /// The built-in template catalog. Image refs use `image_tag`. The `mac`
    /// template exists only when a base VM is configured; its image is that
    /// VM's name.
    pub fn system_templates(&self) -> Vec<Template> {
        let img = |n: &str| format!("warren/{n}:{}", self.image_tag);
        let all = vec![InstanceType::Default, InstanceType::Vm];
        let home = "/home/agent".to_string();
        let mut out = vec![
            Template {
                name: "warren-base".into(),
                scope: TemplateScope::System,
                image_ref: img("base"),
                default_port: GATEWAY_PORT,
                mount_path: home.clone(),
                agent: false,
                description: "A plain Linux computer: Node, Python, git, a browser terminal and the files API. No agent.".into(),
                types: all.clone(),
                default_shape: Shape { cpu: 2, memory: 4, disk: 4 },
                revision: 1,
                ..Default::default()
            },
            Template {
                name: "warren-agent".into(),
                scope: TemplateScope::System,
                image_ref: img("agent"),
                default_port: GATEWAY_PORT,
                mount_path: home.clone(),
                agent: true,
                description: "The built-in agent: chat, shell and file tools on any OpenAI-compatible model.".into(),
                types: all.clone(),
                default_shape: Shape { cpu: 2, memory: 4, disk: 4 },
                env: BTreeMap::from([(ENV_AGENT_DRIVER.to_string(), "openai".to_string())]),
                revision: 1,
                ..Default::default()
            },
            Template {
                name: "warren-opencode".into(),
                scope: TemplateScope::System,
                image_ref: img("opencode"),
                default_port: GATEWAY_PORT,
                mount_path: home.clone(),
                agent: true,
                description: "OpenCode, the open-source coding agent, behind the Agent API.".into(),
                types: all.clone(),
                default_shape: Shape { cpu: 2, memory: 4, disk: 6 },
                env: BTreeMap::from([(ENV_AGENT_DRIVER.to_string(), "command".to_string())]),
                revision: 1,
                ..Default::default()
            },
            Template {
                name: "warren-claude".into(),
                scope: TemplateScope::System,
                image_ref: img("claude"),
                default_port: GATEWAY_PORT,
                mount_path: home,
                agent: false,
                description: "The plain Linux computer with the Claude Code, Codex and OpenCode CLIs installed; drive them over exec or the terminal. No Agent API.".into(),
                types: all,
                default_shape: Shape { cpu: 2, memory: 4, disk: 6 },
                revision: 1,
                ..Default::default()
            },
        ];
        if let Some(base) = &self.mac_base_vm {
            out.push(Template {
                name: "warren-macos".into(),
                scope: TemplateScope::System,
                image_ref: base.clone(),
                default_port: GATEWAY_PORT,
                mount_path: Self::MAC_HOME.to_string(),
                agent: false,
                description: "A macOS computer (Apple silicon host): the base VM's apps, a shell over exec and the files API. Two run per host; sleep keeps memory.".into(),
                types: vec![InstanceType::Mac],
                default_shape: Shape { cpu: 2, memory: 4, disk: 50 },
                revision: 1,
                ..Default::default()
            });
        }
        out
    }

    /// Where injected gateways are mounted inside a container or placed in a VM
    /// rootfs. The path is outside any home so it can never collide with user files.
    pub const INJECTED_GATEWAY_PATH: &'static str = "/warren/warren-gateway";

    /// Resolve the gateway binary: an explicit setting wins; otherwise a
    /// `warren-gateway` next to the running executable, then
    /// `/usr/local/bin/warren-gateway`. Returns None when nothing exists.
    pub fn resolve_gateway_bin(explicit: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p);
        }
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join("warren-gateway"));
            // Developer checkouts: target/release/warrend next to the musl target dir.
            if let Some(target) = dir.parent() {
                candidates.push(target.join("x86_64-unknown-linux-musl/release/warren-gateway"));
            }
        }
        candidates.push(PathBuf::from("/usr/local/bin/warren-gateway"));
        candidates.into_iter().find(|p| p.is_file())
    }

    /// Whether the daemon runs the egress broker (`broker_listen` names an address).
    pub fn broker_enabled(&self) -> bool {
        let l = self.broker_listen.trim();
        !l.is_empty() && !l.eq_ignore_ascii_case("none")
    }

    /// The port of `broker_listen`, or the default broker port when it names none.
    pub fn broker_port(&self) -> u16 {
        crate::broker::listen_port(&self.broker_listen)
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.data_dir.join("instances")
    }

    /// Where the Firecracker jailer builds its chroots (`--chroot-base-dir`):
    /// under the data directory so the instance files can be hard-linked in.
    pub fn firecracker_jail_dir(&self) -> PathBuf {
        self.data_dir.join("firecracker").join("jail")
    }
    /// Packed root filesystems shared by `vm` instances of one image.
    pub fn firecracker_rootfs_cache_dir(&self) -> PathBuf {
        self.data_dir.join("firecracker").join("rootfs-cache")
    }
    /// Per-instance state of `mac` instances (the shared folder each VM mounts).
    pub fn mac_dir(&self) -> PathBuf {
        self.data_dir.join("mac")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts() {
        let c = Config::default();
        assert_eq!(c.parse_host("warren.localhost:7337"), HostKind::Root);
        assert_eq!(c.parse_host("api.warren.localhost:7337"), HostKind::Api);
        assert_eq!(
            c.parse_host("ab12cd34ef.warren.localhost"),
            HostKind::Instance {
                id: "ab12cd34ef".into(),
                port: 0
            }
        );
        assert_eq!(
            c.parse_host("AB12cd34ef-7681.warren.localhost:7337"),
            HostKind::Instance {
                id: "ab12cd34ef".into(),
                port: 7681
            }
        );
        assert_eq!(
            c.parse_host("evil.ab12cd34ef.warren.localhost"),
            HostKind::Root
        );
        assert_eq!(c.parse_host("[::1]:7337"), HostKind::Root);
        assert_eq!(c.parse_host("other.example.com"), HostKind::Root);
    }

    #[test]
    fn urls() {
        let c = Config::default();
        assert_eq!(c.api_url(), "http://api.warren.localhost:7337/v1");
        assert_eq!(
            c.instance_url("ab12cd34ef"),
            "http://ab12cd34ef.warren.localhost:7337"
        );
        assert_eq!(
            c.preview_url("ab12cd34ef", 8080),
            "http://ab12cd34ef-8080.warren.localhost:7337"
        );
        let c = Config {
            public_scheme: "https".into(),
            public_port: 443,
            ..Config::default()
        };
        assert_eq!(
            c.instance_url("ab12cd34ef"),
            "https://ab12cd34ef.warren.localhost"
        );
    }
}
