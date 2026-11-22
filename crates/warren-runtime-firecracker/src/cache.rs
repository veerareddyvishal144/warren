//! A cache of packed root filesystems, so that creating a `vm` instance from
//! an image this host has already packed costs a sparse copy and a few
//! debugfs writes instead of a `docker export` and an `mkfs.ext4 -d`.
//!
//! Entries live under `<data>/firecracker/rootfs-cache/` (the daemon's
//! `firecracker_rootfs_cache_dir`) as `<key>.ext4`
//! (the packed image, with placeholder identity files) next to `<key>.json`
//! ([`Meta`]). The key hashes everything that shapes the packed image: the
//! cache format, the image's content id, how the gateway gets in, the guest
//! init path, the mount path and the disk size. Files that differ per
//! instance (hostname, hosts, the env file) are written into each copy with
//! debugfs afterwards, so one entry serves every instance of that image and
//! disk size. Entries are evicted least recently used beyond a byte budget,
//! and when their image has left the engine.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use warren_core::{Error, Result};

use crate::rootfs;

/// Bumped whenever the packed layout changes (placeholder files, modes).
pub const FORMAT_VERSION: u32 = 1;
/// Hostname written into a cached image; every copy gets its own.
pub const PLACEHOLDER_ID: &str = "warren";
/// Sizes of the placeholder identity files in a cached image. A copy gets
/// its own contents written straight into those files' blocks, padded with
/// newlines to the same size, so the inode never changes; contents longer
/// than the placeholder fall back to a debugfs write.
pub const PLACEHOLDER_HOSTNAME_BYTES: usize = 64;
pub const PLACEHOLDER_HOSTS_BYTES: usize = 256;
pub const PLACEHOLDER_ENV_BYTES: usize = 16 * 1024;
/// Scratch files older than this are leftovers of a crashed build.
const STALE_SCRATCH_SECS: u64 = 3600;

/// What one entry was built from and when it last served a create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub format: u32,
    pub image_ref: String,
    pub image_id: String,
    pub disk_gb: u32,
    pub created_at: i64,
    pub last_used: i64,
    /// Where the placeholder files sit in the image, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<Patch>,
}

/// The data blocks of the placeholder files, so a copy can be given its
/// identity with plain writes into the image file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    pub block_size: u32,
    /// Guest path (`/etc/hostname`) to its blocks and byte size.
    pub files: BTreeMap<String, FilePlace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePlace {
    pub blocks: Vec<u64>,
    pub size: u32,
}

/// Everything a key is derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput<'a> {
    /// The image's content id (`sha256:...`), not its tag.
    pub image_id: &'a str,
    /// How the gateway gets into the image: `image` when it ships one,
    /// otherwise `inject:<hash of the daemon's gateway binary>:<wrap json>`.
    pub gateway: &'a str,
    pub guest_init: &'a str,
    pub mount_path: &'a str,
    pub disk_gb: u32,
}

/// The cache key of an input: 32 hex characters of a SHA-256 over the
/// length-prefixed parts, so no two inputs collide by concatenation.
pub fn key(input: &KeyInput<'_>) -> String {
    let mut h = Sha256::new();
    let format = FORMAT_VERSION.to_string();
    let disk = input.disk_gb.to_string();
    for part in [
        format.as_str(),
        input.image_id,
        input.gateway,
        input.guest_init,
        input.mount_path,
        disk.as_str(),
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())[..32].to_string()
}

/// SHA-256 of a file's contents as hex, or `None` when it cannot be read.
pub fn file_hash(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(hex::encode(h.finalize()))
}

/// The cache directory and its budget.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
    budget_bytes: u64,
}

impl Cache {
    /// `None` when `budget_gb` is 0: every root filesystem is then built
    /// from its image.
    pub fn new(dir: PathBuf, budget_gb: u32) -> Option<Cache> {
        (budget_gb > 0).then(|| Cache::with_budget_bytes(dir, u64::from(budget_gb) * rootfs::GIB))
    }

    /// A cache with an exact byte budget (tests use small ones).
    pub fn with_budget_bytes(dir: PathBuf, budget_bytes: u64) -> Cache {
        Cache { dir, budget_bytes }
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// `<dir>/<key>.ext4`.
    pub fn image(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.ext4"))
    }

    /// `<dir>/<key>.json`.
    pub fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// The packed image for `key` and its metadata, when the image is present
    /// with readable metadata of the current format.
    pub fn lookup(&self, key: &str) -> Option<(PathBuf, Meta)> {
        let image = self.image(key);
        let meta = self.read_meta(key)?;
        (meta.format == FORMAT_VERSION && image.is_file()).then_some((image, meta))
    }

    pub fn read_meta(&self, key: &str) -> Option<Meta> {
        let text = std::fs::read_to_string(self.meta_path(key)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_meta(&self, key: &str, meta: &Meta) -> Result<()> {
        let path = self.meta_path(key);
        let tmp = self
            .dir
            .join(format!("{key}.json.{}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(meta)?;
        std::fs::write(&tmp, text)
            .map_err(|e| Error::internal(format!("write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::internal(format!("rename {}: {e}", path.display())))
    }

    /// Record that `key` served a create now. Best effort: a failure here
    /// only skews eviction order.
    pub fn touch(&self, key: &str, now: i64) {
        if let Some(mut meta) = self.read_meta(key) {
            meta.last_used = now;
            if let Err(e) = self.write_meta(key, &meta) {
                tracing::debug!(key, error = %e.describe(), "cannot update the cache entry's last use");
            }
        }
    }

    /// A unique path in the cache directory to pack a new image into, so a
    /// half-built file is never mistaken for an entry. Creates the directory.
    pub fn scratch(&self, key: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", self.dir.display())))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Ok(self
            .dir
            .join(format!("{key}.{}.{nanos}.tmp", std::process::id())))
    }

    /// Publish the image packed at `built` as the entry for `key`. When
    /// another build published the same key first, that one is kept and
    /// `built` discarded, so concurrent misses converge on one entry.
    /// Returns the path of the entry to copy from.
    pub fn insert(&self, key: &str, built: &Path, meta: Meta) -> Result<PathBuf> {
        let image = self.image(key);
        match std::fs::hard_link(built, &image) {
            Ok(()) => {
                let _ = std::fs::remove_file(built);
                self.write_meta(key, &meta)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(built);
                if self.read_meta(key).is_none() {
                    self.write_meta(key, &meta)?;
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(built);
                return Err(Error::internal(format!(
                    "publish {} as {}: {e}",
                    built.display(),
                    image.display()
                )));
            }
        }
        Ok(image)
    }

    /// Remove an entry (both files); a missing one is fine.
    pub fn remove(&self, key: &str) -> Result<()> {
        for path in [self.image(key), self.meta_path(key)] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::internal(format!("remove {}: {e}", path.display())));
                }
            }
        }
        Ok(())
    }

    /// Every entry with readable metadata, in no particular order.
    pub fn entries(&self) -> Vec<(String, Meta)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                let key = name.strip_suffix(".json")?;
                let meta = self.read_meta(key)?;
                Some((key.to_string(), meta))
            })
            .collect()
    }

    /// Bytes the packed images occupy (allocated, not apparent).
    pub fn total_bytes(&self) -> u64 {
        self.entries()
            .iter()
            .map(|(key, _)| {
                rootfs::allocated_bytes(&self.image(key))
                    .ok()
                    .and_then(|b| u64::try_from(b).ok())
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Drop least recently used entries until the budget holds, never the
    /// entry `keep` (the one a create just used), and sweep scratch files
    /// left by a crashed build. Returns the keys removed.
    pub fn evict(&self, keep: &str, now: i64) -> Result<Vec<String>> {
        self.sweep_scratch();
        let mut entries: Vec<(String, Meta, u64)> = self
            .entries()
            .into_iter()
            .map(|(key, meta)| {
                let bytes = rootfs::allocated_bytes(&self.image(&key))
                    .ok()
                    .and_then(|b| u64::try_from(b).ok())
                    .unwrap_or(0);
                (key, meta, bytes)
            })
            .collect();
        // Oldest use first; ties by creation.
        entries.sort_by_key(|(_, m, _)| (m.last_used, m.created_at));
        let mut total: u64 = entries.iter().map(|(_, _, b)| *b).sum();
        let mut removed = Vec::new();
        for (key, meta, bytes) in entries {
            if total <= self.budget_bytes {
                break;
            }
            if key == keep {
                continue;
            }
            self.remove(&key)?;
            tracing::info!(
                key, image = %meta.image_ref, disk_gb = meta.disk_gb, idle_secs = now - meta.last_used,
                "evicted a cached root filesystem"
            );
            total = total.saturating_sub(bytes);
            removed.push(key);
        }
        Ok(removed)
    }

    fn sweep_scratch(&self) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("tmp") {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age.as_secs() > STALE_SCRATCH_SECS);
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn input<'a>(image_id: &'a str, disk_gb: u32) -> KeyInput<'a> {
        KeyInput {
            image_id,
            gateway: "image",
            guest_init: "/usr/local/bin/warren-gateway",
            mount_path: "/home/agent",
            disk_gb,
        }
    }

    fn meta(image_ref: &str, last_used: i64) -> Meta {
        Meta {
            format: FORMAT_VERSION,
            image_ref: image_ref.into(),
            image_id: format!("sha256:{image_ref}"),
            disk_gb: 4,
            created_at: last_used - 10,
            last_used,
            patch: None,
        }
    }

    /// A fake packed image: `mib` MiB of non-zero data, so allocation counts.
    fn fake_image(path: &Path, mib: usize) {
        std::fs::write(path, vec![0xabu8; mib << 20]).unwrap();
    }

    #[test]
    fn key_is_stable_and_changes_with_every_input() {
        let base = key(&input("sha256:aaa", 4));
        assert_eq!(base, key(&input("sha256:aaa", 4)));
        assert_eq!(base.len(), 32);
        assert_ne!(base, key(&input("sha256:bbb", 4)));
        assert_ne!(base, key(&input("sha256:aaa", 6)));
        let mut other = input("sha256:aaa", 4);
        other.gateway = "inject:deadbeef:[\"/bin/sh\"]";
        assert_ne!(base, key(&other));
        let mut other = input("sha256:aaa", 4);
        other.guest_init = "/sbin/init";
        assert_ne!(base, key(&other));
        let mut other = input("sha256:aaa", 4);
        other.mount_path = "/work";
        assert_ne!(base, key(&other));
        // Length prefixes: moving a character across a boundary is a new key.
        let a = key(&KeyInput {
            image_id: "ab",
            gateway: "c",
            ..input("", 4)
        });
        let b = key(&KeyInput {
            image_id: "a",
            gateway: "bc",
            ..input("", 4)
        });
        assert_ne!(a, b);
    }

    #[test]
    fn a_zero_budget_turns_the_cache_off() {
        assert!(Cache::new(PathBuf::from("/nowhere"), 0).is_none());
        let c = Cache::new(PathBuf::from("/nowhere"), 20).unwrap();
        assert_eq!(c.budget_bytes(), 20 * rootfs::GIB);
    }

    #[test]
    fn insert_publishes_once_and_lookup_needs_both_files() {
        let tmp = TempDir::new("fc-cache");
        let c = Cache::with_budget_bytes(tmp.path().join("cache"), 10 << 20);
        let k = key(&input("sha256:aaa", 4));
        assert_eq!(c.lookup(&k), None);

        let first = c.scratch(&k).unwrap();
        fake_image(&first, 1);
        let published = c.insert(&k, &first, meta("first", 100)).unwrap();
        assert_eq!(published, c.image(&k));
        assert!(!first.exists(), "the scratch file is consumed");
        let (path, found) = c.lookup(&k).unwrap();
        assert_eq!(path, c.image(&k));
        assert_eq!(found.image_ref, "first");

        // A concurrent build that finished second keeps the first entry.
        let second = c.scratch(&k).unwrap();
        std::fs::write(&second, b"other").unwrap();
        let published = c.insert(&k, &second, meta("second", 200)).unwrap();
        assert_eq!(published, c.image(&k));
        assert!(!second.exists());
        assert_eq!(std::fs::metadata(c.image(&k)).unwrap().len(), 1 << 20);
        assert_eq!(c.read_meta(&k).unwrap().image_ref, "first");

        c.touch(&k, 300);
        assert_eq!(c.read_meta(&k).unwrap().last_used, 300);

        // Metadata of another format, or a missing image, is a miss.
        let mut m = c.read_meta(&k).unwrap();
        m.format = FORMAT_VERSION + 1;
        c.write_meta(&k, &m).unwrap();
        assert!(c.lookup(&k).is_none());
        c.write_meta(&k, &meta("first", 300)).unwrap();
        std::fs::remove_file(c.image(&k)).unwrap();
        assert!(c.lookup(&k).is_none());
        c.remove(&k).unwrap();
        assert!(c.entries().is_empty());
        c.remove(&k).unwrap();
    }

    #[test]
    fn evicts_the_least_recently_used_beyond_the_budget_and_keeps_the_current() {
        let tmp = TempDir::new("fc-cache-evict");
        // Room for two 1 MiB images and a little slack.
        let c = Cache::with_budget_bytes(tmp.path().join("cache"), (2 << 20) + (256 << 10));
        let keys: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| key(&input(id, 4)))
            .collect();
        // Usage order: b oldest, then d, a, c newest.
        for (k, (name, used)) in keys
            .iter()
            .zip([("a", 300), ("b", 100), ("c", 400), ("d", 200)])
        {
            let s = c.scratch(k).unwrap();
            fake_image(&s, 1);
            c.insert(k, &s, meta(name, used)).unwrap();
        }
        assert_eq!(c.entries().len(), 4);
        assert!(c.total_bytes() >= 4 << 20);

        // Keep "d" although it is the second oldest: it is the entry in use.
        let removed = c.evict(&keys[3], 500).unwrap();
        assert_eq!(removed, vec![keys[1].clone(), keys[0].clone()]);
        let left: Vec<String> = c.entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&keys[2]) && left.contains(&keys[3]));
        assert!(c.total_bytes() <= c.budget_bytes());

        // Within budget nothing moves.
        assert!(c.evict(&keys[2], 600).unwrap().is_empty());

        // A stale scratch file is swept, a fresh one is left alone.
        let stale = c.scratch(&keys[0]).unwrap();
        std::fs::write(&stale, b"x").unwrap();
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * STALE_SCRATCH_SECS);
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let fresh = c.scratch(&keys[1]).unwrap();
        std::fs::write(&fresh, b"y").unwrap();
        c.evict(&keys[2], 700).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn file_hash_is_sha256_of_the_contents() {
        let tmp = TempDir::new("fc-cache-hash");
        let f = tmp.path().join("gateway");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            file_hash(&f).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(file_hash(&tmp.path().join("missing")), None);
    }
}
