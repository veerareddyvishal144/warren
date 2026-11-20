//! The lifecycle's side of the egress broker: the address an instance
//! reaches it at, the variables the runtime injects, the registry entry a
//! running instance gets and the stored secrets behind both.
//!
//! The broker itself (`warren-broker`) identifies an instance by the source
//! address of its connections, so the entry is registered once the runtime
//! reports an address and removed whenever the instance stops running:
//! stop, sleep, delete, a failure, a stranded transition found by the repair
//! pass. Re-registering replaces the entry, which is how a patched
//! allowlist or a replaced secret value applies to a running instance.

use std::net::IpAddr;

use warren_core::{
    BrokerEntry, BrokerRoute, Error, Instance, InstanceSecret, NetworkMode, Result, Runtime,
    SecretSpec, broker,
};

use crate::manager::Manager;

/// `host:port`, bracketing a bare IPv6 host.
fn join_host_port(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// A secret's variables may not also be set through `env`: the broker
/// values must win and silently shadowing a user value would hide a mistake.
pub(crate) fn check_secret_env_collisions(
    env: &std::collections::BTreeMap<String, String>,
    specs: &[SecretSpec],
) -> Result<()> {
    for s in specs {
        for var in [&s.name, &s.env] {
            if env.contains_key(var) {
                return Err(Error::invalid(format!(
                    "secret variable {var:?} is also set in env; a secret's name and env belong to the broker"
                )));
            }
        }
    }
    Ok(())
}

impl Manager {
    /// Whether this daemon can serve `allowlist` instances and secrets: a
    /// broker registry was wired in and `Config::broker_listen` names an address.
    pub(crate) fn broker_available(&self) -> bool {
        self.deps.broker.is_some() && self.deps.config.broker_enabled()
    }

    /// Refuse a policy the daemon cannot enforce: `allowlist`, or secrets in
    /// any mode, without a broker.
    pub(crate) fn check_broker_policy(&self, mode: NetworkMode, secrets: usize) -> Result<()> {
        if broker::uses_broker(mode, secrets > 0) && !self.broker_available() {
            return Err(Error::not_supported(
                "this daemon runs without an egress broker (--broker-listen none); network allowlist and secrets are unavailable",
            ));
        }
        Ok(())
    }

    /// `host:port` at which `inst` reaches the broker: the configured
    /// advertise address, else the runtime's host for the instance with the
    /// listen port. `None` without a broker or when the runtime cannot tell.
    pub(crate) async fn broker_addr(&self, rt: &dyn Runtime, inst: &Instance) -> Option<String> {
        if !self.broker_available() {
            return None;
        }
        if let Some(addr) = &self.deps.config.broker_advertise_addr {
            return Some(addr.clone());
        }
        match rt.broker_host(inst).await {
            Ok(Some(host)) => Some(join_host_port(&host, self.deps.config.broker_port())),
            Ok(None) => {
                tracing::warn!(
                    instance = %inst.id,
                    runtime = rt.name(),
                    "runtime cannot name the broker address for its instances; set --broker-advertise"
                );
                None
            }
            Err(e) => {
                tracing::warn!(instance = %inst.id, error = %e, "could not determine the broker address");
                None
            }
        }
    }

    /// Fill `inst.broker_env` for the next runtime call (create, recreate,
    /// start) from the stored secrets. An instance whose policy needs the
    /// broker is refused when no address is known for it, rather than started
    /// without a route to its proxy.
    pub(crate) async fn prepare_broker_env(
        &self,
        rt: &dyn Runtime,
        inst: &mut Instance,
    ) -> Result<()> {
        let secrets = self.deps.store.list_instance_secrets(&inst.id).await?;
        let addr = self.broker_addr(rt, inst).await;
        if addr.is_none() && broker::uses_broker(inst.network, !secrets.is_empty()) {
            self.check_broker_policy(inst.network, secrets.len())?;
            return Err(Error::not_supported(format!(
                "no broker address is known for {} instances; set --broker-advertise",
                rt.name()
            )));
        }
        inst.broker_env = broker::instance_env(inst.network, addr.as_deref(), &secrets);
        Ok(())
    }

    /// Register a running instance with the broker: its address, mode,
    /// allowlist and one route per secret. Instances that do not use the
    /// broker are removed instead. Problems are logged, never surfaced: the
    /// instance is running either way.
    pub(crate) async fn broker_register(&self, inst: &Instance) {
        let Some(registry) = &self.deps.broker else {
            return;
        };
        let secrets = match self.deps.store.list_instance_secrets(&inst.id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(instance = %inst.id, error = %e, "could not load the secrets for the broker; routes are unavailable");
                Vec::new()
            }
        };
        if !broker::uses_broker(inst.network, !secrets.is_empty()) {
            registry.unregister(&inst.id);
            return;
        }
        let ip: IpAddr = match inst.ip.parse() {
            Ok(ip) => ip,
            Err(_) => {
                tracing::warn!(instance = %inst.id, ip = %inst.ip, "instance has no usable address; not registered with the broker");
                return;
            }
        };
        registry.register(BrokerEntry {
            instance_id: inst.id.clone(),
            ip,
            mode: inst.network,
            allowed_domains: inst.allowed_domains.clone(),
            routes: secrets
                .iter()
                .map(|s| BrokerRoute {
                    token: s.route_token.clone(),
                    upstream: s.upstream.clone(),
                    header: s.header.clone(),
                    value: s.value.clone(),
                })
                .collect(),
        });
    }

    /// Remove an instance from the broker; a no-op without one.
    pub(crate) fn broker_unregister(&self, id: &str) {
        if let Some(registry) = &self.deps.broker {
            registry.unregister(id);
        }
    }

    /// Stored secrets from request specs. A secret that keeps its `name`
    /// keeps its route token (and creation time), so the route URL already
    /// inside a running instance stays valid while the value changes; new
    /// names get fresh tokens.
    pub(crate) fn secrets_from_specs(
        &self,
        instance_id: &str,
        specs: &[SecretSpec],
        existing: &[InstanceSecret],
    ) -> Vec<InstanceSecret> {
        let now = self.now();
        specs
            .iter()
            .map(|s| {
                let previous = existing.iter().find(|e| e.name == s.name);
                InstanceSecret {
                    instance_id: instance_id.to_string(),
                    name: s.name.clone(),
                    upstream: s.upstream.clone(),
                    header: s.header.clone(),
                    value: s.value.clone(),
                    env: s.env.clone(),
                    route_token: previous
                        .map(|e| e.route_token.clone())
                        .unwrap_or_else(broker::new_route_token),
                    created_at: previous.map(|e| e.created_at).unwrap_or(now),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_joins() {
        assert_eq!(join_host_port("172.18.0.1", 7349), "172.18.0.1:7349");
        assert_eq!(join_host_port(" 192.168.64.1 ", 1), "192.168.64.1:1");
        assert_eq!(join_host_port("fd00::1", 7349), "[fd00::1]:7349");
        assert_eq!(join_host_port("[fd00::1]", 7349), "[fd00::1]:7349");
    }

    #[test]
    fn env_collisions() {
        let env = std::collections::BTreeMap::from([("A_KEY".to_string(), "x".to_string())]);
        let spec = |name: &str, var: &str| SecretSpec {
            name: name.into(),
            env: var.into(),
            ..Default::default()
        };
        assert!(check_secret_env_collisions(&env, &[spec("B_KEY", "B_URL")]).is_ok());
        assert!(check_secret_env_collisions(&env, &[spec("A_KEY", "B_URL")]).is_err());
        assert!(check_secret_env_collisions(&env, &[spec("B_KEY", "A_KEY")]).is_err());
    }
}
