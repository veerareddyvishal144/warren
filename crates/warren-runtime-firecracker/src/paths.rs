//! Layout of one instance's storage: `<instances_dir>/<id>/vm/` holds the
//! rootfs image, the Firecracker config, the guest ip, the pid and the console log.

use std::path::{Path, PathBuf};

use warren_core::Instance;

/// Sub-directory of the instance storage path that holds VM state.
pub const VM_DIR: &str = "vm";
/// The ext4 root filesystem image booted by the VM.
pub const ROOTFS: &str = "rootfs.ext4";
/// A rootfs being built; renamed over [`ROOTFS`] when complete.
pub const ROOTFS_TMP: &str = "rootfs.ext4.tmp";
/// Where the template image is exported before `mkfs.ext4 -d` packs it.
pub const ROOTFS_DIR: &str = "rootfs-dir";
/// Firecracker `--config-file`.
pub const CONFIG: &str = "config.json";
/// The guest's address, one line.
pub const IP_FILE: &str = "ip";
/// Pid of the running firecracker process.
pub const PID_FILE: &str = "pid";
/// Serial console output of every boot.
pub const CONSOLE_LOG: &str = "console.log";
/// Scratch directories used while rebuilding or backing up.
pub const KEEP_DIR: &str = "home-keep";
pub const RESTORE_DIR: &str = "restore-tmp";
pub const BACKUP_DIR: &str = "backup-tmp";

/// Resolved paths of one instance.
#[derive(Debug, Clone)]
pub struct VmPaths {
    /// `Instance::storage_path`.
    pub root: PathBuf,
    /// `<root>/vm`.
    pub vm: PathBuf,
}

impl VmPaths {
    /// Use the instance's recorded storage path, or derive it from the
    /// instances directory when the instance has not been created yet.
    pub fn for_instance(instances_dir: &Path, inst: &Instance) -> VmPaths {
        let root = if inst.storage_path.is_empty() {
            instances_dir.join(&inst.id)
        } else {
            PathBuf::from(&inst.storage_path)
        };
        VmPaths::new(root)
    }

    /// Paths rooted at an explicit storage directory.
    pub fn new(root: PathBuf) -> VmPaths {
        let vm = root.join(VM_DIR);
        VmPaths { root, vm }
    }

    /// `vm/rootfs.ext4`.
    pub fn rootfs(&self) -> PathBuf {
        self.vm.join(ROOTFS)
    }
    /// `vm/rootfs.ext4.tmp`, a rootfs under construction.
    pub fn rootfs_tmp(&self) -> PathBuf {
        self.vm.join(ROOTFS_TMP)
    }
    /// `vm/rootfs-dir`, the exported image tree before packing.
    pub fn rootfs_dir(&self) -> PathBuf {
        self.vm.join(ROOTFS_DIR)
    }
    /// `vm/config.json`.
    pub fn config(&self) -> PathBuf {
        self.vm.join(CONFIG)
    }
    /// `vm/ip`.
    pub fn ip(&self) -> PathBuf {
        self.vm.join(IP_FILE)
    }
    /// `vm/pid`.
    pub fn pid(&self) -> PathBuf {
        self.vm.join(PID_FILE)
    }
    /// `vm/console.log`.
    pub fn console(&self) -> PathBuf {
        self.vm.join(CONSOLE_LOG)
    }
    /// Scratch directory holding the preserved home during a rebuild.
    pub fn keep_dir(&self) -> PathBuf {
        self.vm.join(KEEP_DIR)
    }
    /// Scratch directory where a backup is unpacked before a rebuild.
    pub fn restore_dir(&self) -> PathBuf {
        self.vm.join(RESTORE_DIR)
    }
    /// Scratch directory where the home is dumped while backing up.
    pub fn backup_dir(&self) -> PathBuf {
        self.vm.join(BACKUP_DIR)
    }
}
