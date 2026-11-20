//! The table of running instances keyed by source address.

use std::{collections::HashMap, net::IpAddr, sync::Arc};

use parking_lot::Mutex;
use warren_core::{BrokerEntry, BrokerRegistry};

#[derive(Default)]
struct Inner {
    by_ip: HashMap<IpAddr, Arc<BrokerEntry>>,
    by_id: HashMap<String, IpAddr>,
}

/// In-memory [`BrokerRegistry`]: one entry per instance, looked up by the
/// address its connections arrive from. Registering an instance again (a new
/// address after a restart, a changed allowlist, replaced secrets) swaps the
/// entry; an address can only belong to one instance at a time, so a stale
/// entry left on an address that a new instance receives is replaced too.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    pub fn new() -> Arc<Registry> {
        Arc::new(Registry::default())
    }

    /// The instance registered for `ip`, if any.
    pub fn lookup(&self, ip: IpAddr) -> Option<Arc<BrokerEntry>> {
        self.inner.lock().by_ip.get(&ip).cloned()
    }

    /// The entry of an instance by id, if registered.
    pub fn get(&self, instance_id: &str) -> Option<Arc<BrokerEntry>> {
        let inner = self.inner.lock();
        let ip = inner.by_id.get(instance_id)?;
        inner.by_ip.get(ip).cloned()
    }

    /// Number of registered instances.
    pub fn len(&self) -> usize {
        self.inner.lock().by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BrokerRegistry for Registry {
    fn register(&self, entry: BrokerEntry) {
        let mut inner = self.inner.lock();
        if let Some(old_ip) = inner.by_id.remove(&entry.instance_id) {
            inner.by_ip.remove(&old_ip);
        }
        if let Some(previous) = inner.by_ip.remove(&entry.ip) {
            tracing::warn!(
                ip = %entry.ip,
                previous = %previous.instance_id,
                instance = %entry.instance_id,
                "address was still registered to another instance; replacing"
            );
            inner.by_id.remove(&previous.instance_id);
        }
        tracing::debug!(
            instance = %entry.instance_id,
            ip = %entry.ip,
            mode = %entry.mode,
            domains = entry.allowed_domains.len(),
            routes = entry.routes.len(),
            "broker entry registered"
        );
        inner.by_id.insert(entry.instance_id.clone(), entry.ip);
        inner.by_ip.insert(entry.ip, Arc::new(entry));
    }

    fn unregister(&self, instance_id: &str) {
        let mut inner = self.inner.lock();
        if let Some(ip) = inner.by_id.remove(instance_id) {
            inner.by_ip.remove(&ip);
            tracing::debug!(instance = instance_id, %ip, "broker entry removed");
        }
    }
}
