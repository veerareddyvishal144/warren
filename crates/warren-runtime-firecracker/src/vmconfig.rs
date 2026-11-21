//! The Firecracker `--config-file` document and the guest kernel command line.

use std::{net::Ipv4Addr, path::Path};

use serde::{Deserialize, Serialize};
use warren_core::{Error, Result, Shape};

use crate::{
    limits::RateLimits,
    net::{self, HOST_IP, NETMASK},
};

/// The gateway binary inside the image; it runs as PID 1 with `--init`.
pub const GUEST_INIT: &str = "/usr/local/bin/warren-gateway";
/// Drive id of the root filesystem.
pub const ROOT_DRIVE_ID: &str = "rootfs";
/// Interface id of the single NIC.
pub const IFACE_ID: &str = "eth0";

/// Firecracker VM configuration as accepted by `firecracker --config-file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    /// Kernel and command line.
    #[serde(rename = "boot-source")]
    pub boot_source: BootSource,
    /// Block devices; the first is the root filesystem.
    pub drives: Vec<Drive>,
    /// The guest's NICs; one tap on the bridge.
    #[serde(rename = "network-interfaces")]
    pub network_interfaces: Vec<NetworkInterface>,
    /// vCPUs and memory.
    #[serde(rename = "machine-config")]
    pub machine_config: MachineConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    pub boot_args: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
    /// Bandwidth and ops limits; none means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RateLimiter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
    /// Limit on traffic from the host into the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_rate_limiter: Option<RateLimiter>,
    /// Limit on traffic from the guest out to the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_rate_limiter: Option<RateLimiter>,
}

/// Firecracker's `RateLimiter`: a bandwidth bucket (bytes) and an ops bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimiter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth: Option<TokenBucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ops: Option<TokenBucket>,
}

/// Firecracker's `TokenBucket`: `size` tokens are refilled every
/// `refill_time` milliseconds; `one_time_burst` extra tokens are available
/// once at the start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBucket {
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_time_burst: Option<u64>,
    pub refill_time: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
}

/// Kernel command line: serial console, reboot-on-panic so a crashed guest exits
/// Firecracker, the root drive, a static `ip=` so no DHCP client is needed, and
/// the gateway as init.
pub fn boot_args(guest_ip: Ipv4Addr) -> String {
    boot_args_with_init(guest_ip, GUEST_INIT)
}

/// [`boot_args`] with another program as PID 1.
pub fn boot_args_with_init(guest_ip: Ipv4Addr, init: &str) -> String {
    format!(
        "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw \
         ip={guest_ip}::{HOST_IP}:{NETMASK}::{IFACE_ID}:off init={init} -- --init"
    )
}

/// Build the configuration for one VM. Paths are made absolute because
/// Firecracker resolves them relative to its own working directory (inside
/// the jailer they are jail-relative and already absolute). `limits` puts
/// token-bucket rate limiters on the root drive and both directions of the
/// NIC; `None` leaves them unlimited.
pub fn render(
    kernel: &Path,
    rootfs: &Path,
    tap: &str,
    guest_ip: Ipv4Addr,
    shape: Shape,
    limits: Option<&RateLimits>,
) -> VmConfig {
    VmConfig {
        boot_source: BootSource {
            kernel_image_path: absolute(kernel),
            boot_args: boot_args(guest_ip),
        },
        drives: vec![Drive {
            drive_id: ROOT_DRIVE_ID.into(),
            path_on_host: absolute(rootfs),
            is_root_device: true,
            is_read_only: false,
            rate_limiter: limits.map(RateLimits::drive),
        }],
        network_interfaces: vec![NetworkInterface {
            iface_id: IFACE_ID.into(),
            host_dev_name: tap.to_string(),
            guest_mac: Some(net::guest_mac(guest_ip)),
            rx_rate_limiter: limits.map(RateLimits::nic),
            tx_rate_limiter: limits.map(RateLimits::nic),
        }],
        machine_config: MachineConfig {
            vcpu_count: shape.cpu,
            mem_size_mib: shape.memory.saturating_mul(1024),
        },
    }
}

fn absolute(p: &Path) -> String {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    abs.to_string_lossy().into_owned()
}

/// The config as pretty JSON.
pub fn to_json(cfg: &VmConfig) -> Result<String> {
    Ok(serde_json::to_string_pretty(cfg)?)
}

/// Write the config as pretty JSON.
pub fn write(path: &Path, cfg: &VmConfig) -> Result<()> {
    let text = to_json(cfg)?;
    std::fs::write(path, text)
        .map_err(|e| Error::internal(format!("write {}: {e}", path.display())))
}

/// Read a config written by [`write`].
#[cfg(test)]
pub fn read(path: &Path) -> Result<VmConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::internal(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::internal(format!("parse {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_args_match_the_contract() {
        let args = boot_args(Ipv4Addr::new(172, 30, 0, 5));
        assert_eq!(
            args,
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw \
             ip=172.30.0.5::172.30.0.1:255.255.255.0::eth0:off init=/usr/local/bin/warren-gateway -- --init"
        );
    }

    #[test]
    fn renders_the_firecracker_document() {
        let cfg = render(
            Path::new("/var/lib/warren/firecracker/vmlinux"),
            Path::new("/var/lib/warren/instances/ab12cd34ef/vm/rootfs.ext4"),
            "tap-ab12cd34",
            Ipv4Addr::new(172, 30, 0, 2),
            Shape {
                cpu: 2,
                memory: 4,
                disk: 4,
            },
            None,
        );
        let v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            v["boot-source"]["kernel_image_path"],
            "/var/lib/warren/firecracker/vmlinux"
        );
        assert!(
            v["boot-source"]["boot_args"]
                .as_str()
                .unwrap()
                .contains("ip=172.30.0.2::172.30.0.1:")
        );
        assert_eq!(v["drives"][0]["drive_id"], "rootfs");
        assert_eq!(
            v["drives"][0]["path_on_host"],
            "/var/lib/warren/instances/ab12cd34ef/vm/rootfs.ext4"
        );
        assert_eq!(v["drives"][0]["is_root_device"], true);
        assert_eq!(v["drives"][0]["is_read_only"], false);
        assert_eq!(v["network-interfaces"][0]["iface_id"], "eth0");
        assert_eq!(v["network-interfaces"][0]["host_dev_name"], "tap-ab12cd34");
        assert_eq!(v["network-interfaces"][0]["guest_mac"], "06:00:ac:1e:00:02");
        assert_eq!(v["machine-config"]["vcpu_count"], 2);
        assert_eq!(v["machine-config"]["mem_size_mib"], 4096);
        // Only the four top-level sections Firecracker expects, and no rate
        // limiter keys at all when limits are off.
        assert_eq!(v.as_object().unwrap().len(), 4);
        assert!(v["drives"][0].get("rate_limiter").is_none());
        assert!(v["network-interfaces"][0].get("rx_rate_limiter").is_none());
        assert!(v["network-interfaces"][0].get("tx_rate_limiter").is_none());
    }

    #[test]
    fn renders_rate_limiters_in_firecracker_form() {
        let shape = Shape {
            cpu: 4,
            memory: 8,
            disk: 6,
        };
        let limits = crate::limits::for_shape(shape);
        let cfg = render(
            Path::new("/vmlinux"),
            Path::new("/rootfs.ext4"),
            "tap-ab12cd34",
            Ipv4Addr::new(172, 30, 0, 2),
            shape,
            Some(&limits),
        );
        let v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        // Jail-relative paths stay as they are.
        assert_eq!(v["boot-source"]["kernel_image_path"], "/vmlinux");
        assert_eq!(v["drives"][0]["path_on_host"], "/rootfs.ext4");
        let drive = &v["drives"][0]["rate_limiter"];
        assert_eq!(drive["bandwidth"]["size"], 20_000_000);
        assert_eq!(drive["bandwidth"]["refill_time"], 100);
        assert_eq!(drive["bandwidth"]["one_time_burst"], 200_000_000);
        assert_eq!(drive["ops"]["size"], 800);
        let rx = &v["network-interfaces"][0]["rx_rate_limiter"];
        let tx = &v["network-interfaces"][0]["tx_rate_limiter"];
        assert_eq!(rx, tx);
        assert_eq!(rx["bandwidth"]["size"], 2_500_000, "200 Mbit/s over 100 ms");
        assert!(rx.get("ops").is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = crate::testutil::TempDir::new("fc-vmconfig");
        let path = tmp.path().join("config.json");
        let shape = Shape {
            cpu: 4,
            memory: 8,
            disk: 6,
        };
        let cfg = render(
            Path::new("/k/vmlinux"),
            Path::new("/i/vm/rootfs.ext4"),
            "tap-x",
            Ipv4Addr::new(172, 30, 0, 9),
            shape,
            Some(&crate::limits::for_shape(shape)),
        );
        write(&path, &cfg).unwrap();
        assert_eq!(read(&path).unwrap(), cfg);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            to_json(&cfg).unwrap()
        );
        assert!(read(&tmp.path().join("missing.json")).is_err());
    }
}
