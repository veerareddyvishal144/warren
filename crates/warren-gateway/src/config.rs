//! Gateway configuration from `WARREN_*` environment variables.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use warren_core::{
    Error, Result,
    types::{
        ENV_AGENT_COMMAND, ENV_AGENT_DRIVER, ENV_GATEWAY_PORT, ENV_HOME, ENV_INSTANCE_ID,
        ENV_INSTANCE_TOKEN, ENV_MODEL, ENV_WORKSPACE_ID, GATEWAY_PORT, TERMINAL_PORT,
    },
};

use crate::wrap::WrapSpec;

/// Optional socket override for the HTTP listener (`host:port`).
pub const ENV_BIND: &str = "WARREN_GATEWAY_BIND";
/// Set to `0`, `false` or `off` to skip the `ttyd` supervisor.
pub const ENV_TERMINAL: &str = "WARREN_TERMINAL";
/// Set by the PID 1 supervisor on the server it spawns so `/v1/shutdown` may
/// power the machine off.
pub const ENV_SHUTDOWN_ENABLED: &str = "WARREN_SHUTDOWN_ENABLED";
/// Base URL of the OpenAI-compatible endpoint used by the `openai` driver.
pub const ENV_OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
/// Bearer token for the `openai` driver; may be empty for local servers.
pub const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
/// Default OpenAI-compatible endpoint.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Home directory when neither `WARREN_HOME` nor `HOME` is set.
pub const DEFAULT_HOME: &str = "/home/agent";

/// Everything the gateway needs to run, resolved once at startup.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// The persistent home directory (`WARREN_HOME`, else `HOME`, else `/home/agent`).
    pub home: PathBuf,
    /// Address the HTTP server binds (`0.0.0.0:<WARREN_GATEWAY_PORT>` by default).
    pub bind: SocketAddr,
    /// `WARREN_INSTANCE_ID`; also the hostname inside microVMs.
    pub instance_id: String,
    /// `WARREN_INSTANCE_TOKEN`; when non-empty `/v1/exec` and `/v1/shutdown`
    /// require it in `X-Warren-Instance-Token`.
    pub instance_token: String,
    /// `WARREN_WORKSPACE_ID`, informational.
    pub workspace_id: String,
    /// `WARREN_AGENT_DRIVER`: `echo`, `openai`, `command`, or empty / `none`.
    pub driver: String,
    /// `WARREN_AGENT_COMMAND` for the `command` driver.
    pub agent_command: String,
    /// `WARREN_MODEL`; empty means the driver's default.
    pub model: String,
    /// `OPENAI_BASE_URL` without a trailing slash.
    pub openai_base_url: String,
    /// `OPENAI_API_KEY`.
    pub openai_api_key: String,
    /// Supervise `ttyd` on `terminal_port`.
    pub terminal: bool,
    /// Port the browser terminal listens on.
    pub terminal_port: u16,
    /// `/v1/shutdown` is allowed to power the machine off.
    pub shutdown_enabled: bool,
    /// Wrap mode (`--wrap -- <argv...>`): the image's own entrypoint to run
    /// and supervise next to the server. `None` outside wrap mode; set from
    /// the command line, never from the environment.
    pub wrap: Option<WrapSpec>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            home: PathBuf::from(DEFAULT_HOME),
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), GATEWAY_PORT),
            instance_id: String::new(),
            instance_token: String::new(),
            workspace_id: String::new(),
            driver: String::new(),
            agent_command: String::new(),
            model: String::new(),
            openai_base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            openai_api_key: String::new(),
            terminal: true,
            terminal_port: TERMINAL_PORT,
            shutdown_enabled: false,
            wrap: None,
        }
    }
}

fn env_string(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

fn env_flag(key: &str, default: bool) -> bool {
    let v = env_string(key).to_ascii_lowercase();
    match v.as_str() {
        "" => default,
        "0" | "false" | "off" | "no" => false,
        _ => true,
    }
}

impl GatewayConfig {
    /// Build the configuration from the process environment.
    pub fn from_env() -> Result<Self> {
        let mut cfg = GatewayConfig::default();
        let home = env_string(ENV_HOME);
        cfg.home = if !home.is_empty() {
            PathBuf::from(home)
        } else {
            let h = env_string("HOME");
            if h.is_empty() {
                PathBuf::from(DEFAULT_HOME)
            } else {
                PathBuf::from(h)
            }
        };
        let port = env_string(ENV_GATEWAY_PORT);
        let port: u16 = if port.is_empty() {
            GATEWAY_PORT
        } else {
            port.parse().map_err(|_| {
                Error::invalid(format!(
                    "{ENV_GATEWAY_PORT} must be a port number, got {port:?}"
                ))
            })?
        };
        cfg.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let bind = env_string(ENV_BIND);
        if !bind.is_empty() {
            cfg.bind = bind.parse().map_err(|_| {
                Error::invalid(format!("{ENV_BIND} must be host:port, got {bind:?}"))
            })?;
        }
        cfg.instance_id = env_string(ENV_INSTANCE_ID);
        cfg.instance_token = env_string(ENV_INSTANCE_TOKEN);
        cfg.workspace_id = env_string(ENV_WORKSPACE_ID);
        cfg.driver = env_string(ENV_AGENT_DRIVER).to_ascii_lowercase();
        cfg.agent_command = env_string(ENV_AGENT_COMMAND);
        cfg.model = env_string(ENV_MODEL);
        let base = env_string(ENV_OPENAI_BASE_URL);
        cfg.openai_base_url = if base.is_empty() {
            DEFAULT_OPENAI_BASE_URL.to_string()
        } else {
            base.trim_end_matches('/').to_string()
        };
        cfg.openai_api_key = env_string(ENV_OPENAI_API_KEY);
        cfg.terminal = env_flag(ENV_TERMINAL, true);
        cfg.shutdown_enabled = env_flag(ENV_SHUTDOWN_ENABLED, false);
        Ok(cfg)
    }

    /// True when a driver is configured (`WARREN_AGENT_DRIVER` not empty and not `none`).
    pub fn agent_enabled(&self) -> bool {
        !self.driver.is_empty() && self.driver != "none"
    }

    /// Driver name for health reports: the configured one or `none`.
    pub fn driver_name(&self) -> &str {
        if self.agent_enabled() {
            &self.driver
        } else {
            "none"
        }
    }

    /// `WARREN_MODEL` or `default`, as listed by `/v1/models`.
    pub fn model_id(&self) -> String {
        if self.model.is_empty() {
            "default".to_string()
        } else {
            self.model.clone()
        }
    }

    /// `<home>/.warren`, where sessions and responses persist.
    pub fn state_dir(&self) -> PathBuf {
        state_dir(&self.home)
    }

    /// True in wrap mode with a program to run.
    pub fn wraps_program(&self) -> bool {
        self.wrap.as_ref().is_some_and(|w| !w.argv.is_empty())
    }
}

/// `<home>/.warren`.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".warren")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_helpers() {
        let mut c = GatewayConfig::default();
        assert_eq!(c.bind.port(), GATEWAY_PORT);
        assert!(!c.agent_enabled());
        assert_eq!(c.driver_name(), "none");
        assert_eq!(c.model_id(), "default");
        c.driver = "none".into();
        assert!(!c.agent_enabled());
        c.driver = "echo".into();
        assert!(c.agent_enabled());
        assert_eq!(c.driver_name(), "echo");
        c.model = "gpt-x".into();
        assert_eq!(c.model_id(), "gpt-x");
        assert_eq!(c.state_dir(), PathBuf::from(DEFAULT_HOME).join(".warren"));
        assert!(c.wrap.is_none());
        assert!(!c.wraps_program());
        c.wrap = Some(WrapSpec::default());
        assert!(!c.wraps_program());
        c.wrap = Some(WrapSpec {
            argv: vec!["sleep".into(), "1".into()],
            cwd: None,
        });
        assert!(c.wraps_program());
    }

    #[test]
    fn from_env_never_enables_wrap_mode() {
        // Wrap mode comes from the command line only.
        if let Ok(c) = GatewayConfig::from_env() {
            assert!(c.wrap.is_none());
        }
    }
}
