//! Helpers shared by the unit tests: scratch directories under `TMPDIR` and
//! environment probes that let tests skip cleanly where Docker, root, KVM or the
//! Firecracker assets are unavailable.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("warren-{tag}-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small image that is present locally or can be pulled; `None` when the
/// Docker CLI or daemon is unavailable.
pub async fn docker_test_image() -> Option<String> {
    let docker = crate::host::find_tool("docker")?;
    if !crate::host::succeeds(&docker, &["info"]).await.ok()? {
        return None;
    }
    let image = std::env::var("WARREN_TEST_IMAGE").unwrap_or_else(|_| "alpine:3.20".to_string());
    crate::image::ensure(&image).await.ok()?;
    Some(image)
}

/// All e2fsprogs tools the runtime shells out to are installed.
pub fn have_e2fsprogs() -> bool {
    ["mkfs.ext4", "debugfs", "resize2fs", "e2fsck"]
        .iter()
        .all(|t| crate::host::find_tool(t).is_some())
}

/// Where the Firecracker binary and kernel live on this machine, if anywhere:
/// `WARREN_FIRECRACKER_BIN` / `WARREN_KERNEL_IMAGE`, the default asset
/// directory, or the probe directory described in `docs/DEVELOPMENT.md`.
pub fn firecracker_assets() -> Option<(PathBuf, PathBuf)> {
    let bin_candidates = [
        std::env::var("WARREN_FIRECRACKER_BIN")
            .ok()
            .map(PathBuf::from),
        Some(PathBuf::from("/var/lib/warren/firecracker/firecracker")),
        Some(PathBuf::from("/usr/local/bin/firecracker")),
        Some(PathBuf::from("/data/sandbox-probe/firecracker")),
    ];
    let kernel_candidates = [
        std::env::var("WARREN_KERNEL_IMAGE").ok().map(PathBuf::from),
        Some(PathBuf::from("/var/lib/warren/firecracker/vmlinux")),
        Some(PathBuf::from("/data/sandbox-probe/vmlinux-6.1.155")),
    ];
    let bin = bin_candidates
        .into_iter()
        .flatten()
        .find(|p| crate::host::is_executable(p))?;
    let kernel = kernel_candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_file())?;
    Some((bin, kernel))
}
