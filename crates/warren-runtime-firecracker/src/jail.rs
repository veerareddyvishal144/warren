//! Launching Firecracker through its jailer. The jailer builds a chroot at
//! `<base>/<firecracker binary name>/<instance id>/root`, copies the binary
//! in, creates `/dev/kvm`, `/dev/net/tun` and `/run` inside it owned by the
//! jail uid/gid, drops privileges and execs Firecracker in place (the pid the
//! daemon spawned stays Firecracker's pid; neither `--daemonize` nor
//! `--new-pid-ns` is used). Seccomp stays at Firecracker's default: on.
//!
//! Everything the VM reads must already be inside the chroot, so before each
//! launch the runtime wipes the instance's jail and hard-links the kernel,
//! the root filesystem and, on a restore, the snapshot files into it (same
//! inodes, no copies; the base directory must therefore be on the same
//! filesystem as the instances directory, which `ready` checks). Paths in the
//! VM configuration and in snapshot calls are jail-relative (`/vmlinux`,
//! `/rootfs.ext4`, `/snapshot/mem`); the API socket is
//! `/run/firecracker.socket`, which is `<root>/run/firecracker.socket` on the
//! host. The jail is removed when the VM stops.

use std::{
    ffi::OsString,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use warren_core::{Error, Result};

/// Where the jail root of an instance is, relative to the data directory.
pub const JAIL_SUBDIR: &str = "firecracker/jail";
/// The API socket as Firecracker names it inside the chroot.
pub const API_SOCK: &str = "/run/firecracker.socket";
/// The kernel inside the chroot.
pub const KERNEL: &str = "/vmlinux";
/// The root filesystem inside the chroot.
pub const ROOTFS: &str = "/rootfs.ext4";
/// The VM configuration inside the chroot.
pub const CONFIG: &str = "/config.json";
/// The snapshot directory inside the chroot.
pub const SNAPSHOT_DIR: &str = "/snapshot";
/// The jailer's own cache of the kernel when it cannot be hard-linked (on
/// another filesystem): `<base>/.kernel/<name>`.
const KERNEL_CACHE: &str = ".kernel";

/// How the jailer is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailConfig {
    /// The `jailer` binary.
    pub jailer: PathBuf,
    /// The `firecracker` binary it execs (`--exec-file`).
    pub firecracker: PathBuf,
    /// `--chroot-base-dir`.
    pub base: PathBuf,
    /// `--uid`: the unprivileged user Firecracker runs as.
    pub uid: u32,
    /// `--gid`.
    pub gid: u32,
}

/// One instance's jail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jail {
    cfg: JailConfig,
    id: String,
}

impl Jail {
    pub fn new(cfg: JailConfig, id: &str) -> Jail {
        Jail {
            cfg,
            id: id.to_string(),
        }
    }

    /// The file name of the Firecracker binary, which names the chroot level.
    pub fn exec_name(&self) -> String {
        self.cfg
            .firecracker
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "firecracker".to_string())
    }

    /// `<base>/<exec name>/<id>`.
    pub fn dir(&self) -> PathBuf {
        self.cfg.base.join(self.exec_name()).join(&self.id)
    }

    /// `<base>/<exec name>/<id>/root`, the chroot.
    pub fn root(&self) -> PathBuf {
        self.dir().join("root")
    }

    /// The host path of a jail-relative path.
    pub fn host_path(&self, jail_path: &str) -> PathBuf {
        self.root().join(jail_path.trim_start_matches('/'))
    }

    /// Host path of the API socket.
    pub fn api_sock(&self) -> PathBuf {
        self.host_path(API_SOCK)
    }

    /// Host path of the snapshot directory Firecracker writes into.
    pub fn snapshot_dir(&self) -> PathBuf {
        self.host_path(SNAPSHOT_DIR)
    }

    /// The string that identifies this VM's process on its command line: the
    /// jailer passes `--id <id>` to Firecracker.
    pub fn marker(&self) -> String {
        self.id.clone()
    }

    /// The jailer command line for `firecracker_args` (what follows `--`).
    pub fn command(&self, firecracker_args: &[OsString]) -> (PathBuf, Vec<OsString>) {
        (
            self.cfg.jailer.clone(),
            jailer_args(&self.cfg, &self.id, firecracker_args),
        )
    }

    /// Build the chroot from scratch: wipe what a previous run left, then
    /// hard-link the kernel and the root filesystem, write `config` and make
    /// the snapshot directory writable by the jail user; with `snapshot`
    /// (`vmstate`, `mem` host paths) link those in for a restore. The root
    /// filesystem is chowned to the jail uid/gid (Firecracker writes it); the
    /// kernel and snapshot files only need to be readable.
    pub fn prepare(
        &self,
        kernel: &Path,
        rootfs: &Path,
        config_json: &str,
        snapshot: Option<(&Path, &Path)>,
    ) -> Result<()> {
        self.remove()?;
        let root = self.root();
        std::fs::create_dir_all(&root)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", root.display())))?;
        let kernel_src = self.kernel_source(kernel)?;
        link(&kernel_src, &self.host_path(KERNEL))?;
        ensure_readable(&self.host_path(KERNEL))?;
        link(rootfs, &self.host_path(ROOTFS))?;
        chown(&self.host_path(ROOTFS), self.cfg.uid, self.cfg.gid)?;
        let cfg_path = self.host_path(CONFIG);
        std::fs::write(&cfg_path, config_json)
            .map_err(|e| Error::internal(format!("write {}: {e}", cfg_path.display())))?;
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o644))
            .map_err(|e| Error::internal(format!("chmod {}: {e}", cfg_path.display())))?;
        let snap = self.snapshot_dir();
        std::fs::create_dir_all(&snap)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", snap.display())))?;
        chown(&snap, self.cfg.uid, self.cfg.gid)?;
        if let Some((vmstate, mem)) = snapshot {
            link(vmstate, &snap.join(crate::snapshot::VMSTATE))?;
            link(mem, &snap.join(crate::snapshot::MEM))?;
            ensure_readable(&snap.join(crate::snapshot::VMSTATE))?;
            ensure_readable(&snap.join(crate::snapshot::MEM))?;
        }
        Ok(())
    }

    /// The kernel to link: the configured file when it is on the jail's
    /// filesystem, else a copy cached under the base directory.
    fn kernel_source(&self, kernel: &Path) -> Result<PathBuf> {
        if same_filesystem(kernel, &self.cfg.base) {
            return Ok(kernel.to_path_buf());
        }
        let name = kernel.file_name().ok_or_else(|| {
            Error::internal(format!("kernel path {} has no name", kernel.display()))
        })?;
        let cache_dir = self.cfg.base.join(KERNEL_CACHE);
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", cache_dir.display())))?;
        let cached = cache_dir.join(name);
        let src_md = std::fs::metadata(kernel)
            .map_err(|e| Error::internal(format!("stat {}: {e}", kernel.display())))?;
        let fresh = std::fs::metadata(&cached)
            .map(|md| md.len() == src_md.len() && md.modified().ok() == src_md.modified().ok())
            .unwrap_or(false);
        if !fresh {
            tracing::info!(kernel = %kernel.display(), cache = %cached.display(), "copying the kernel next to the jail (another filesystem)");
            let tmp = cache_dir.join(format!(".{}.tmp", name.to_string_lossy()));
            std::fs::copy(kernel, &tmp)
                .map_err(|e| Error::internal(format!("copy {}: {e}", kernel.display())))?;
            if let Ok(modified) = src_md.modified() {
                let _ = std::fs::File::options()
                    .write(true)
                    .open(&tmp)
                    .and_then(|f| f.set_modified(modified));
            }
            std::fs::rename(&tmp, &cached)
                .map_err(|e| Error::internal(format!("install {}: {e}", cached.display())))?;
        }
        Ok(cached)
    }

    /// Remove the instance's jail directory; nothing there is fine. Hard links
    /// go, the instance's files stay.
    pub fn remove(&self) -> Result<()> {
        crate::rootfs::remove_tree(&self.dir())
    }

    /// Move the snapshot files Firecracker wrote in the jail to `dest_dir`
    /// (a rename on the shared filesystem). The jail's snapshot directory is
    /// left in place.
    pub fn take_snapshot_files(&self, dest_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dest_dir)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", dest_dir.display())))?;
        for name in [crate::snapshot::VMSTATE, crate::snapshot::MEM] {
            let from = self.snapshot_dir().join(name);
            let to = dest_dir.join(name);
            move_file(&from, &to)?;
        }
        Ok(())
    }
}

/// `jailer --id <id> --exec-file <firecracker> --uid <uid> --gid <gid>
/// --chroot-base-dir <base> -- <firecracker args>`. No `--netns` (the tap
/// lives in the host's namespace), no `--daemonize`, no `--new-pid-ns`, no
/// `--no-seccomp`.
pub fn jailer_args(cfg: &JailConfig, id: &str, firecracker_args: &[OsString]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--id".into(),
        id.into(),
        "--exec-file".into(),
        cfg.firecracker.clone().into_os_string(),
        "--uid".into(),
        cfg.uid.to_string().into(),
        "--gid".into(),
        cfg.gid.to_string().into(),
        "--chroot-base-dir".into(),
        cfg.base.clone().into_os_string(),
        "--".into(),
    ];
    args.extend(firecracker_args.iter().cloned());
    args
}

/// Whether two paths are on the same filesystem (same `st_dev`); false when
/// either cannot be stat'ed.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev(),
        _ => false,
    }
}

/// Hard-link `src` at `dst`, replacing an existing `dst`. A different
/// filesystem is a clear error: the jail cannot share the instance's files.
pub fn link(src: &Path, dst: &Path) -> Result<()> {
    if let Err(e) = std::fs::metadata(src) {
        return Err(Error::internal(format!("{}: {e}", src.display())));
    }
    if let Err(e) = std::fs::remove_file(dst)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(Error::internal(format!("remove {}: {e}", dst.display())));
    }
    std::fs::hard_link(src, dst).map_err(|e| {
        let hint = if e.raw_os_error() == Some(nix::libc::EXDEV) {
            "; the jail base directory must be on the same filesystem as the instances directory"
        } else {
            ""
        };
        Error::internal(format!(
            "hard link {} -> {}: {e}{hint}",
            dst.display(),
            src.display()
        ))
    })
}

/// Rename `from` to `to`, copying and removing when they are on different
/// filesystems.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(nix::libc::EXDEV) => {
            std::fs::copy(from, to).map_err(|e| {
                Error::internal(format!("copy {} to {}: {e}", from.display(), to.display()))
            })?;
            std::fs::remove_file(from)
                .map_err(|e| Error::internal(format!("remove {}: {e}", from.display())))
        }
        Err(e) => Err(Error::internal(format!(
            "move {} to {}: {e}",
            from.display(),
            to.display()
        ))),
    }
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
        .map_err(|e| Error::internal(format!("chown {}:{} {}: {e}", uid, gid, path.display())))
}

/// Make sure the jail user can read a file it does not own.
fn ensure_readable(path: &Path) -> Result<()> {
    let md = std::fs::metadata(path)
        .map_err(|e| Error::internal(format!("stat {}: {e}", path.display())))?;
    let mode = md.permissions().mode();
    if mode & 0o004 == 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o444))
            .map_err(|e| Error::internal(format!("chmod {}: {e}", path.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn cfg(base: &Path) -> JailConfig {
        JailConfig {
            jailer: PathBuf::from("/usr/local/bin/jailer"),
            firecracker: PathBuf::from("/usr/local/bin/firecracker"),
            base: base.to_path_buf(),
            uid: 61000,
            gid: 61000,
        }
    }

    #[test]
    fn layout_follows_the_jailer_convention() {
        let j = Jail::new(
            cfg(Path::new("/var/lib/warren/firecracker/jail")),
            "ab12cd34ef",
        );
        assert_eq!(j.exec_name(), "firecracker");
        assert_eq!(
            j.dir(),
            PathBuf::from("/var/lib/warren/firecracker/jail/firecracker/ab12cd34ef")
        );
        assert_eq!(
            j.root(),
            PathBuf::from("/var/lib/warren/firecracker/jail/firecracker/ab12cd34ef/root")
        );
        assert_eq!(
            j.api_sock(),
            PathBuf::from(
                "/var/lib/warren/firecracker/jail/firecracker/ab12cd34ef/root/run/firecracker.socket"
            )
        );
        assert_eq!(j.host_path(ROOTFS), j.root().join("rootfs.ext4"));
        assert_eq!(j.host_path("relative"), j.root().join("relative"));
        assert_eq!(j.snapshot_dir(), j.root().join("snapshot"));
        assert_eq!(j.marker(), "ab12cd34ef");
    }

    #[test]
    fn jailer_command_line() {
        let j = Jail::new(cfg(Path::new("/data/firecracker/jail")), "ab12cd34ef");
        let fc_args: Vec<OsString> = vec![
            "--api-sock".into(),
            API_SOCK.into(),
            "--config-file".into(),
            CONFIG.into(),
        ];
        let (program, args) = j.command(&fc_args);
        assert_eq!(program, PathBuf::from("/usr/local/bin/jailer"));
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--id",
                "ab12cd34ef",
                "--exec-file",
                "/usr/local/bin/firecracker",
                "--uid",
                "61000",
                "--gid",
                "61000",
                "--chroot-base-dir",
                "/data/firecracker/jail",
                "--",
                "--api-sock",
                "/run/firecracker.socket",
                "--config-file",
                "/config.json",
            ]
        );
        assert!(
            !args.iter().any(|a| a.contains("seccomp")
                || a == "--daemonize"
                || a == "--new-pid-ns"
                || a == "--netns"),
            "seccomp stays on, no daemonize, no pid namespace, host network namespace"
        );
    }

    /// Prepare on a scratch tree (no root): the kernel and rootfs are
    /// hard-linked (same inode), the config written, the snapshot directory
    /// created, and snapshot files linked for a restore; the jail is rebuilt
    /// from scratch each time and removed cleanly. chown to a foreign uid
    /// needs root, so the test uses its own ids.
    #[test]
    fn prepare_links_the_vm_files_into_the_chroot() {
        let tmp = TempDir::new("fc-jail");
        let data = tmp.path();
        let kernel = data.join("vmlinux");
        std::fs::write(&kernel, "kernel").unwrap();
        let rootfs = data.join("instances/ab12cd34ef/vm/rootfs.ext4");
        std::fs::create_dir_all(rootfs.parent().unwrap()).unwrap();
        std::fs::write(&rootfs, "rootfs").unwrap();
        let mut c = cfg(&data.join("firecracker/jail"));
        c.uid = crate::host::effective_uid();
        c.gid = std::fs::metadata(data).unwrap().gid();
        let j = Jail::new(c, "ab12cd34ef");

        // A stale jail from a previous run is wiped.
        std::fs::create_dir_all(j.root().join("dev/net")).unwrap();
        std::fs::write(j.root().join("stale"), "x").unwrap();

        j.prepare(&kernel, &rootfs, "{\"cfg\":true}", None).unwrap();
        assert!(!j.root().join("stale").exists());
        let same_inode = |a: &Path, b: &Path| {
            std::fs::metadata(a).unwrap().ino() == std::fs::metadata(b).unwrap().ino()
        };
        assert!(same_inode(&kernel, &j.host_path(KERNEL)));
        assert!(same_inode(&rootfs, &j.host_path(ROOTFS)));
        assert_eq!(
            std::fs::read_to_string(j.host_path(CONFIG)).unwrap(),
            "{\"cfg\":true}"
        );
        assert!(j.snapshot_dir().is_dir());
        assert!(!j.snapshot_dir().join("mem").exists());

        // A restore links the snapshot files in.
        let snap = data.join("instances/ab12cd34ef/vm/snapshot");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("vmstate"), "state").unwrap();
        std::fs::write(snap.join("mem"), "memory").unwrap();
        j.prepare(
            &kernel,
            &rootfs,
            "{}",
            Some((&snap.join("vmstate"), &snap.join("mem"))),
        )
        .unwrap();
        assert!(same_inode(&snap.join("mem"), &j.snapshot_dir().join("mem")));
        assert!(same_inode(
            &snap.join("vmstate"),
            &j.snapshot_dir().join("vmstate")
        ));

        // Snapshot files Firecracker wrote in the jail move out to the vm dir.
        std::fs::write(j.snapshot_dir().join("vmstate"), "new state").unwrap();
        std::fs::write(j.snapshot_dir().join("mem"), "new memory").unwrap();
        let out = data.join("instances/ab12cd34ef/vm/snapshot-out");
        j.take_snapshot_files(&out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("mem")).unwrap(),
            "new memory"
        );
        assert!(!j.snapshot_dir().join("mem").exists());

        // Removing the jail leaves the instance's files alone.
        j.remove().unwrap();
        assert!(!j.dir().exists());
        assert!(rootfs.is_file() && kernel.is_file() && snap.join("mem").is_file());
        j.remove().unwrap();
    }

    #[test]
    fn same_filesystem_and_links() {
        let tmp = TempDir::new("fc-jail-fs");
        let a = tmp.path().join("a");
        std::fs::write(&a, "a").unwrap();
        assert!(same_filesystem(&a, tmp.path()));
        assert!(!same_filesystem(&a, &tmp.path().join("missing")));
        let b = tmp.path().join("b");
        link(&a, &b).unwrap();
        link(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "a");
        let err = link(&tmp.path().join("missing"), &b).unwrap_err();
        assert!(err.is_internal());
        let c = tmp.path().join("c");
        move_file(&b, &c).unwrap();
        assert!(!b.exists() && c.is_file());
        assert!(move_file(&b, &c).is_err());
    }
}
