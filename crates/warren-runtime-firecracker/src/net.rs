//! Guest addressing on the `warren-fc0` bridge: deterministic tap names and MAC
//! addresses, and the lowest-free IP allocation recorded in `<instance>/vm/ip`.

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

use warren_core::{Error, Result};

use crate::paths;

/// Linux bridge every microVM tap is attached to.
pub const BRIDGE: &str = "warren-fc0";
/// Address of the host on the bridge; the guests' default gateway.
pub const HOST_IP: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 1);
/// Netmask of the guest network as the kernel `ip=` parameter wants it.
pub const NETMASK: &str = "255.255.255.0";
/// Prefix length of the guest network.
pub const PREFIX_LEN: u8 = 24;
/// The guest network in CIDR notation, for NAT rules.
pub const SUBNET: &str = "172.30.0.0/24";
/// First and last host octets handed to guests (`.1` is the host).
pub const FIRST_HOST: u8 = 2;
pub const LAST_HOST: u8 = 254;

/// `tap-<first 8 characters of the id>`: at most 12 characters, well inside the
/// 15-character interface name limit.
pub fn tap_name(id: &str) -> String {
    format!("tap-{}", id.chars().take(8).collect::<String>())
}

/// A locally administered MAC derived from the guest IP so ARP caches stay
/// valid across restarts: `06:00:ac:1e:00:<host>`.
pub fn guest_mac(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("06:00:{:02x}:{:02x}:{:02x}:{:02x}", o[0], o[1], o[2], o[3])
}

/// True when `ip` is an address this runtime hands to guests.
pub fn is_guest_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 172 && o[1] == 30 && o[2] == 0 && (FIRST_HOST..=LAST_HOST).contains(&o[3])
}

/// Parse the contents of a `vm/ip` file; anything outside the guest range is
/// ignored.
pub fn parse_ip_file(contents: &str) -> Option<Ipv4Addr> {
    contents.trim().parse().ok().filter(|ip| is_guest_ip(*ip))
}

/// Every guest address recorded under `<instances_dir>/*/vm/ip`.
pub fn used_ips(instances_dir: &Path) -> Vec<Ipv4Addr> {
    let Ok(entries) = std::fs::read_dir(instances_dir) else {
        return Vec::new();
    };
    let mut used = Vec::new();
    for entry in entries.flatten() {
        let ip_file: PathBuf = entry.path().join(paths::VM_DIR).join(paths::IP_FILE);
        if let Ok(text) = std::fs::read_to_string(&ip_file) {
            match parse_ip_file(&text) {
                Some(ip) => used.push(ip),
                None => {
                    tracing::warn!(path = %ip_file.display(), "ignoring unreadable guest ip file")
                }
            }
        }
    }
    used
}

/// The lowest guest address not in `used`.
pub fn lowest_free(used: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    (FIRST_HOST..=LAST_HOST)
        .map(|h| Ipv4Addr::new(172, 30, 0, h))
        .find(|ip| !used.contains(ip))
}

/// Pick the lowest unused guest address by scanning the instances directory.
/// The caller records it in the instance's `vm/ip` right away (under the
/// runtime's allocation lock) so concurrent creates never collide.
pub fn allocate(instances_dir: &Path) -> Result<Ipv4Addr> {
    let used = used_ips(instances_dir);
    lowest_free(&used).ok_or_else(|| {
        Error::try_again(format!(
            "all {} guest addresses in {SUBNET} are in use",
            LAST_HOST - FIRST_HOST + 1
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn tap_names_are_short_and_stable() {
        assert_eq!(tap_name("ab12cd34ef"), "tap-ab12cd34");
        assert_eq!(tap_name("abc"), "tap-abc");
        assert!(tap_name("ab12cd34ef").len() <= 15);
    }

    #[test]
    fn mac_follows_ip() {
        assert_eq!(guest_mac(Ipv4Addr::new(172, 30, 0, 2)), "06:00:ac:1e:00:02");
        assert_eq!(
            guest_mac(Ipv4Addr::new(172, 30, 0, 254)),
            "06:00:ac:1e:00:fe"
        );
    }

    #[test]
    fn ip_file_parsing() {
        assert_eq!(
            parse_ip_file("172.30.0.7\n"),
            Some(Ipv4Addr::new(172, 30, 0, 7))
        );
        assert_eq!(
            parse_ip_file("172.30.0.1"),
            None,
            "the host address is never a guest"
        );
        assert_eq!(parse_ip_file("172.30.0.255"), None);
        assert_eq!(parse_ip_file("10.0.0.2"), None);
        assert_eq!(parse_ip_file("garbage"), None);
    }

    #[test]
    fn lowest_free_skips_used_and_exhausts() {
        assert_eq!(lowest_free(&[]), Some(Ipv4Addr::new(172, 30, 0, 2)));
        let used = [Ipv4Addr::new(172, 30, 0, 2), Ipv4Addr::new(172, 30, 0, 4)];
        assert_eq!(lowest_free(&used), Some(Ipv4Addr::new(172, 30, 0, 3)));
        let all: Vec<Ipv4Addr> = (FIRST_HOST..=LAST_HOST)
            .map(|h| Ipv4Addr::new(172, 30, 0, h))
            .collect();
        assert_eq!(lowest_free(&all), None);
    }

    #[test]
    fn allocation_scans_instance_dirs() {
        let tmp = TempDir::new("fc-net");
        let dir = tmp.path();
        for (id, ip) in [
            ("aaaaaaaaaa", "172.30.0.2"),
            ("bbbbbbbbbb", "172.30.0.3"),
            ("cccccccccc", "bogus"),
        ] {
            let vm = dir.join(id).join(paths::VM_DIR);
            std::fs::create_dir_all(&vm).unwrap();
            std::fs::write(vm.join(paths::IP_FILE), format!("{ip}\n")).unwrap();
        }
        // An instance without a vm dir and a stray file are ignored.
        std::fs::create_dir_all(dir.join("dddddddddd")).unwrap();
        std::fs::write(dir.join("stray"), "x").unwrap();
        assert_eq!(allocate(dir).unwrap(), Ipv4Addr::new(172, 30, 0, 4));
        assert_eq!(
            allocate(&dir.join("does-not-exist")).unwrap(),
            Ipv4Addr::new(172, 30, 0, 2)
        );
    }

    #[test]
    fn allocation_reports_exhaustion() {
        let tmp = TempDir::new("fc-net-full");
        for h in FIRST_HOST..=LAST_HOST {
            let vm = tmp.path().join(format!("i{h:09}")).join(paths::VM_DIR);
            std::fs::create_dir_all(&vm).unwrap();
            std::fs::write(vm.join(paths::IP_FILE), format!("172.30.0.{h}")).unwrap();
        }
        let err = allocate(tmp.path()).unwrap_err();
        assert_eq!(err.code, warren_core::errors::CODE_TRY_AGAIN);
    }
}
