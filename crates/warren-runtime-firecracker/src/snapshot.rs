//! A sleeping VM's memory: the snapshot directory `<instance>/vm/snapshot/`
//! holds Firecracker's `vmstate` and `mem` files and a `meta.json` the runtime
//! writes next to them (Firecracker version, shape, guest address, time).
//! [`assess`] decides at `start` whether the files can be loaded or the VM
//! has to boot cold; a snapshot is used once and removed after a successful
//! restore.

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use warren_core::{Error, Result, Shape};

use crate::rootfs;

/// Directory under `vm/` holding the snapshot.
pub const DIR: &str = "snapshot";
/// Firecracker's microVM state file.
pub const VMSTATE: &str = "vmstate";
/// The guest memory, as large as the guest's RAM.
pub const MEM: &str = "mem";
/// What the runtime records about the snapshot.
pub const META: &str = "meta.json";

/// Everything the runtime needs to know to judge a snapshot later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The `vmm_version` of the process that wrote the snapshot, e.g. `1.18.0`.
    pub firecracker_version: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Shape of the VM when it slept; cpu and memory must match at restore.
    pub shape: Shape,
    /// Guest address baked into the saved memory (`ip=` on the command line).
    pub ip: Ipv4Addr,
    /// Tap the saved NIC was attached to; the restore recreates it by name.
    pub tap: String,
    /// Guest RAM in MiB, which is also the expected size of `mem`.
    pub mem_size_mib: u32,
}

/// What the current host expects of a snapshot it is about to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect<'a> {
    pub shape: Shape,
    pub ip: Ipv4Addr,
    pub tap: &'a str,
    /// The version of the Firecracker binary that would load it; `None` when
    /// it could not be determined, in which case Firecracker's own check is
    /// the only one.
    pub firecracker_version: Option<&'a str>,
}

/// Outcome of [`assess`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// No snapshot at all: a normal boot.
    None,
    /// A usable snapshot.
    Restore(Meta),
    /// Files exist but cannot be loaded; the reason is for the log. The caller
    /// discards them and boots cold.
    Unusable(String),
}

/// Paths of the snapshot files under `dir`.
pub fn vmstate(dir: &Path) -> PathBuf {
    dir.join(VMSTATE)
}
pub fn mem(dir: &Path) -> PathBuf {
    dir.join(MEM)
}
pub fn meta(dir: &Path) -> PathBuf {
    dir.join(META)
}

/// True when anything of a snapshot is present under `dir`.
pub fn exists(dir: &Path) -> bool {
    [vmstate(dir), mem(dir), meta(dir)]
        .iter()
        .any(|p| p.exists())
}

/// Write `meta.json`.
pub fn write_meta(dir: &Path, m: &Meta) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::internal(format!("mkdir {}: {e}", dir.display())))?;
    let text = serde_json::to_string_pretty(m)?;
    std::fs::write(meta(dir), text)
        .map_err(|e| Error::internal(format!("write {}: {e}", meta(dir).display())))
}

/// Read `meta.json`; `Ok(None)` when it does not exist.
pub fn read_meta(dir: &Path) -> Result<Option<Meta>> {
    match std::fs::read_to_string(meta(dir)) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::internal(format!("parse {}: {e}", meta(dir).display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::internal(format!(
            "read {}: {e}",
            meta(dir).display()
        ))),
    }
}

/// Remove the snapshot directory and everything in it; nothing there is fine.
pub fn discard(dir: &Path) -> Result<()> {
    rootfs::remove_tree(dir)
}

/// Bytes the snapshot files occupy on disk (allocated, not apparent).
pub fn allocated_bytes(dir: &Path) -> Result<i64> {
    let mut total = 0i64;
    for p in [vmstate(dir), mem(dir), meta(dir)] {
        total = total.saturating_add(rootfs::allocated_bytes(&p)?);
    }
    Ok(total)
}

/// `x.y.z` from `Firecracker v1.18.0`, `v1.18.0`, `1.18.0` or a line with
/// more words; `None` when no version-looking token is present.
pub fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|tok| tok.trim_start_matches('v'))
        .find(|tok| {
            let mut parts = tok.split('.');
            matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(a), Some(b), Some(c))
                    if !a.is_empty() && a.chars().all(|c| c.is_ascii_digit())
                        && !b.is_empty() && b.chars().all(|c| c.is_ascii_digit())
                        && c.chars().next().is_some_and(|ch| ch.is_ascii_digit())
            )
        })
        .map(str::to_string)
}

/// Decide what to do with the snapshot under `dir` for a VM the host expects
/// to look like `want`.
pub fn assess(dir: &Path, want: &Expect<'_>) -> Assessment {
    if !exists(dir) {
        return Assessment::None;
    }
    let m = match read_meta(dir) {
        Ok(Some(m)) => m,
        Ok(None) => return Assessment::Unusable("meta.json is missing".into()),
        Err(e) => return Assessment::Unusable(format!("meta.json is unreadable: {e}")),
    };
    for (name, path) in [("vmstate", vmstate(dir)), ("mem", mem(dir))] {
        match std::fs::metadata(&path) {
            Ok(md) if md.is_file() && md.len() > 0 => {}
            Ok(_) => return Assessment::Unusable(format!("{name} is empty")),
            Err(_) => return Assessment::Unusable(format!("{name} is missing")),
        }
    }
    let mem_len = std::fs::metadata(mem(dir)).map(|md| md.len()).unwrap_or(0);
    let expected_mem = u64::from(m.mem_size_mib) << 20;
    if mem_len < expected_mem {
        return Assessment::Unusable(format!(
            "mem is {mem_len} bytes, the guest had {expected_mem}"
        ));
    }
    if let Some(cur) = want.firecracker_version
        && parse_version(cur).as_deref() != Some(m.firecracker_version.as_str())
    {
        return Assessment::Unusable(format!(
            "taken by Firecracker {}, the host runs {}",
            m.firecracker_version,
            parse_version(cur).unwrap_or_else(|| cur.to_string())
        ));
    }
    if (m.shape.cpu, m.shape.memory) != (want.shape.cpu, want.shape.memory) {
        return Assessment::Unusable(format!(
            "taken at {} vCPU / {} GB, the instance is now {} vCPU / {} GB",
            m.shape.cpu, m.shape.memory, want.shape.cpu, want.shape.memory
        ));
    }
    if m.ip != want.ip {
        return Assessment::Unusable(format!("guest address was {}, now {}", m.ip, want.ip));
    }
    if m.tap != want.tap {
        return Assessment::Unusable(format!("tap was {}, now {}", m.tap, want.tap));
    }
    Assessment::Restore(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn shape(cpu: u32, memory: u32) -> Shape {
        Shape {
            cpu,
            memory,
            disk: 4,
        }
    }

    fn meta_for(shape: Shape) -> Meta {
        Meta {
            firecracker_version: "1.18.0".into(),
            created_at: 1_700_000_000,
            shape,
            ip: Ipv4Addr::new(172, 30, 0, 2),
            tap: "tap-ab12cd34".into(),
            mem_size_mib: shape.memory * 1024,
        }
    }

    /// A complete fake snapshot: `mem` sized like the guest's RAM (sparse).
    fn write_snapshot(dir: &Path, m: &Meta) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(vmstate(dir), b"vmstate").unwrap();
        let f = std::fs::File::create(mem(dir)).unwrap();
        f.set_len(u64::from(m.mem_size_mib) << 20).unwrap();
        write_meta(dir, m).unwrap();
    }

    fn expect(shape: Shape) -> Expect<'static> {
        Expect {
            shape,
            ip: Ipv4Addr::new(172, 30, 0, 2),
            tap: "tap-ab12cd34",
            firecracker_version: Some("Firecracker v1.18.0"),
        }
    }

    #[test]
    fn versions_are_normalised() {
        assert_eq!(
            parse_version("Firecracker v1.18.0").as_deref(),
            Some("1.18.0")
        );
        assert_eq!(
            parse_version("Firecracker v1.18.0\nfeatures: gdb\n").as_deref(),
            Some("1.18.0")
        );
        assert_eq!(parse_version("1.18.0").as_deref(), Some("1.18.0"));
        assert_eq!(parse_version("v1.19.0-dev").as_deref(), Some("1.19.0-dev"));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn meta_round_trips_and_missing_is_none() {
        let tmp = TempDir::new("fc-snap-meta");
        let dir = tmp.path().join("snapshot");
        assert_eq!(read_meta(&dir).unwrap(), None);
        assert!(!exists(&dir));
        let m = meta_for(shape(2, 4));
        write_meta(&dir, &m).unwrap();
        assert_eq!(read_meta(&dir).unwrap(), Some(m));
        assert!(exists(&dir));
        std::fs::write(meta(&dir), "{ not json").unwrap();
        assert!(read_meta(&dir).is_err());
        discard(&dir).unwrap();
        discard(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn layout_under_the_vm_dir() {
        let dir = Path::new("/var/lib/warren/instances/ab12cd34ef/vm/snapshot");
        assert_eq!(
            vmstate(dir),
            PathBuf::from("/var/lib/warren/instances/ab12cd34ef/vm/snapshot/vmstate")
        );
        assert_eq!(
            mem(dir),
            PathBuf::from("/var/lib/warren/instances/ab12cd34ef/vm/snapshot/mem")
        );
        assert_eq!(
            meta(dir),
            PathBuf::from("/var/lib/warren/instances/ab12cd34ef/vm/snapshot/meta.json")
        );
    }

    #[test]
    fn a_complete_snapshot_is_restored() {
        let tmp = TempDir::new("fc-snap-ok");
        let dir = tmp.path().join("snapshot");
        let m = meta_for(shape(2, 4));
        write_snapshot(&dir, &m);
        assert_eq!(
            assess(&dir, &expect(shape(2, 4))),
            Assessment::Restore(m.clone())
        );
        // The version check is skipped when the host's version is unknown,
        // and accepts the bare form too.
        let mut e = expect(shape(2, 4));
        e.firecracker_version = None;
        assert_eq!(assess(&dir, &e), Assessment::Restore(m.clone()));
        e.firecracker_version = Some("1.18.0");
        assert_eq!(assess(&dir, &e), Assessment::Restore(m));
        // Allocated bytes count the sparse memory file as allocated, not apparent.
        assert!(allocated_bytes(&dir).unwrap() >= 0);
        assert!(allocated_bytes(&dir).unwrap() < (4u64 << 30) as i64);
    }

    #[test]
    fn nothing_there_means_a_cold_boot_without_complaint() {
        let tmp = TempDir::new("fc-snap-none");
        let dir = tmp.path().join("snapshot");
        assert_eq!(assess(&dir, &expect(shape(2, 4))), Assessment::None);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(assess(&dir, &expect(shape(2, 4))), Assessment::None);
    }

    #[test]
    fn unusable_snapshots_say_why() {
        let tmp = TempDir::new("fc-snap-bad");
        let dir = tmp.path().join("snapshot");
        let m = meta_for(shape(2, 4));

        // Files without meta.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(vmstate(&dir), b"x").unwrap();
        let Assessment::Unusable(why) = assess(&dir, &expect(shape(2, 4))) else {
            panic!("expected unusable");
        };
        assert!(why.contains("meta.json is missing"), "{why}");

        // Meta without the memory file.
        write_meta(&dir, &m).unwrap();
        let Assessment::Unusable(why) = assess(&dir, &expect(shape(2, 4))) else {
            panic!("expected unusable");
        };
        assert!(why.contains("mem is missing"), "{why}");

        // Empty vmstate.
        write_snapshot(&dir, &m);
        std::fs::write(vmstate(&dir), b"").unwrap();
        let Assessment::Unusable(why) = assess(&dir, &expect(shape(2, 4))) else {
            panic!("expected unusable");
        };
        assert!(why.contains("vmstate is empty"), "{why}");

        // Truncated memory file.
        write_snapshot(&dir, &m);
        std::fs::File::create(mem(&dir))
            .unwrap()
            .set_len(1 << 20)
            .unwrap();
        let Assessment::Unusable(why) = assess(&dir, &expect(shape(2, 4))) else {
            panic!("expected unusable");
        };
        assert!(why.contains("mem is 1048576 bytes"), "{why}");

        // Another Firecracker version.
        write_snapshot(&dir, &m);
        let mut e = expect(shape(2, 4));
        e.firecracker_version = Some("Firecracker v1.19.0");
        let Assessment::Unusable(why) = assess(&dir, &e) else {
            panic!("expected unusable");
        };
        assert_eq!(why, "taken by Firecracker 1.18.0, the host runs 1.19.0");

        // Resized while asleep.
        let Assessment::Unusable(why) = assess(&dir, &expect(shape(4, 8))) else {
            panic!("expected unusable");
        };
        assert!(why.contains("4 vCPU / 8 GB"), "{why}");

        // Another address or tap.
        let mut e = expect(shape(2, 4));
        e.ip = Ipv4Addr::new(172, 30, 0, 9);
        let Assessment::Unusable(why) = assess(&dir, &e) else {
            panic!("expected unusable");
        };
        assert!(why.contains("172.30.0.9"), "{why}");
        let mut e = expect(shape(2, 4));
        e.tap = "tap-other";
        let Assessment::Unusable(why) = assess(&dir, &e) else {
            panic!("expected unusable");
        };
        assert!(why.contains("tap-other"), "{why}");

        // Corrupt meta.
        std::fs::write(meta(&dir), "garbage").unwrap();
        let Assessment::Unusable(why) = assess(&dir, &expect(shape(2, 4))) else {
            panic!("expected unusable");
        };
        assert!(why.contains("unreadable"), "{why}");
    }
}
