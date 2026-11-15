use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use crate::{types::*, InstanceType, Shape, Template, TemplateScope};

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
    /// Docker runtime for default instances: "auto" | "runsc" | "runc".
    pub runtime: String,
    pub region: String,
    pub enable_firecracker: bool,
    pub firecracker_bin: PathBuf,
    pub kernel_image: Option<PathBuf>,
    /// Dev top-ups without a payment provider, short idle timeouts, host.docker.internal.
    pub dev_mode: bool,
    pub free_credit_micros: Money,
    pub min_idle_timeout_sec: u32,
    pub rates: Rates,
    /// HMAC secret for signed URLs.
    pub secret: String,
    pub meter_interval: Duration,
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
            dev_mode: false,
            free_credit_micros: 5 * MICROS_PER_DOLLAR,
            min_idle_timeout_sec: MIN_IDLE_TIMEOUT_SEC,
            rates: Rates::default(),
            secret: String::new(),
            meter_interval: Duration::from_secs(60),
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
    Instance { id: String, port: u16 },
}

impl Config {
    fn host_port(&self, host: &str) -> String {
        let implicit = self.public_port == 0
            || (self.public_scheme == "http" && self.public_port == 80)
            || (self.public_scheme == "https" && self.public_port == 443);
        if implicit { host.to_string() } else { format!("{host}:{}", self.public_port) }
    }

    pub fn api_host(&self) -> String {
        format!("api.{}", self.base_domain)
    }

    /// Public base URL of the Sandbox API, without a trailing slash.
    pub fn api_url(&self) -> String {
        format!("{}://{}/v1", self.public_scheme, self.host_port(&self.api_host()))
    }

    /// `scheme://{id}.<base>[:port]`
    pub fn instance_url(&self, id: &str) -> String {
        format!("{}://{}", self.public_scheme, self.host_port(&format!("{id}.{}", self.base_domain)))
    }

    /// `scheme://{id}-{port}.<base>[:port]`
    pub fn preview_url(&self, id: &str, port: u16) -> String {
        format!("{}://{}", self.public_scheme, self.host_port(&format!("{id}-{port}.{}", self.base_domain)))
    }

    /// Classify a Host header. Ports in the header are ignored.
    pub fn parse_host(&self, host_header: &str) -> HostKind {
        let mut host = host_header.trim().to_ascii_lowercase();
        if host.starts_with('[') {
            // [::1]:7337
            if let Some(end) = host.find(']') {
                host = host[..=end].to_string();
            }
        } else if let Some(i) = host.rfind(':') {
            if host[i + 1..].chars().all(|c| c.is_ascii_digit()) {
                host.truncate(i);
            }
        }
        let base = self.base_domain.to_ascii_lowercase();
        if host == base || host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1" {
            return HostKind::Root;
        }
        if host == format!("api.{base}") {
            return HostKind::Api;
        }
        let Some(label) = host.strip_suffix(&format!(".{base}")) else { return HostKind::Root };
        if label.contains('.') {
            return HostKind::Root;
        }
        if let Some(i) = label.rfind('-') {
            let (id, port) = (&label[..i], &label[i + 1..]);
            if id.len() == INSTANCE_ID_LENGTH {
                if let Ok(p) = port.parse::<u16>() {
                    if p > 0 {
                        return HostKind::Instance { id: id.to_string(), port: p };
                    }
                }
            }
        }
        if label.len() == INSTANCE_ID_LENGTH && crate::ids::valid_instance_id(label) {
            return HostKind::Instance { id: label.to_string(), port: 0 };
        }
        HostKind::Root
    }

    /// The built-in template catalog. Image refs use `image_tag`.
    pub fn system_templates(&self) -> Vec<Template> {
        let img = |n: &str| format!("warren/{n}:{}", self.image_tag);
        let all = vec![InstanceType::Default, InstanceType::Vm];
        let home = "/home/agent".to_string();
        vec![
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
                mount_path: home,
                agent: true,
                description: "OpenCode, the open-source coding agent, behind the Agent API.".into(),
                types: all,
                default_shape: Shape { cpu: 2, memory: 4, disk: 6 },
                env: BTreeMap::from([(ENV_AGENT_DRIVER.to_string(), "command".to_string())]),
                revision: 1,
                ..Default::default()
            },
        ]
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.data_dir.join("instances")
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
        assert_eq!(c.parse_host("ab12cd34ef.warren.localhost"), HostKind::Instance { id: "ab12cd34ef".into(), port: 0 });
        assert_eq!(c.parse_host("AB12cd34ef-7681.warren.localhost:7337"), HostKind::Instance { id: "ab12cd34ef".into(), port: 7681 });
        assert_eq!(c.parse_host("evil.ab12cd34ef.warren.localhost"), HostKind::Root);
        assert_eq!(c.parse_host("[::1]:7337"), HostKind::Root);
        assert_eq!(c.parse_host("other.example.com"), HostKind::Root);
    }

    #[test]
    fn urls() {
        let c = Config::default();
        assert_eq!(c.api_url(), "http://api.warren.localhost:7337/v1");
        assert_eq!(c.instance_url("ab12cd34ef"), "http://ab12cd34ef.warren.localhost:7337");
        assert_eq!(c.preview_url("ab12cd34ef", 8080), "http://ab12cd34ef-8080.warren.localhost:7337");
        let mut c = Config::default();
        c.public_scheme = "https".into();
        c.public_port = 443;
        assert_eq!(c.instance_url("ab12cd34ef"), "https://ab12cd34ef.warren.localhost");
    }
}
