//! Host-side storage of an instance: the `home` directory that is bind-mounted
//! into the container, its size, copies for forks, and tar.gz backups and
//! restores. File-system work runs on tokio's blocking pool.
//!
//! The gateway runs as root inside the instance, so the home can hold files a
//! non-root daemon may not delete, copy or read. Every operation here first
//! tries on the host and, on `PermissionDenied`, falls back to the root helper
//! container of the `helper` module before retrying or finishing the work.

use std::{
    fs,
    io::{self, BufReader, BufWriter, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use walkdir::WalkDir;
use warren_core::{Error, Instance, Result, ids};

use crate::{
    AGENT_GID, AGENT_UID, DockerRuntime, HOME_DIR,
    helper::{HelperJob, needs_helper},
};

/// Run file-system work off the async threads.
async fn blocking<T, F>(what: &'static str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::internal(format!("{what}: blocking task failed: {e}")))?
}

fn io_err(what: &str, e: io::Error) -> Error {
    Error::internal(format!("{what}: {e}"))
}

/// Give `path` to the agent user. Returns whether it worked. A failure is only
/// worth a warning when the path is not already owned by uid 1000, which is the
/// normal case when warrend runs as the same unprivileged user as the containers.
pub(crate) fn chown_agent(path: &Path) -> bool {
    match std::os::unix::fs::chown(path, Some(AGENT_UID), Some(AGENT_GID)) {
        Ok(()) => true,
        Err(e) => {
            let owner = fs::metadata(path).map(|m| m.uid()).ok();
            if owner == Some(AGENT_UID) {
                tracing::debug!(path = %path.display(), err = %e, "chown skipped; already owned by uid 1000");
            } else {
                tracing::warn!(path = %path.display(), ?owner, err = %e, "could not chown to 1000:1000; the container user may not be able to write");
            }
            false
        }
    }
}

/// `lchown` everything under `root` to the agent user, best effort.
pub(crate) fn chown_tree(root: &Path) {
    for entry in WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if let Err(e) = std::os::unix::fs::lchown(entry.path(), Some(AGENT_UID), Some(AGENT_GID)) {
            tracing::debug!(path = %entry.path().display(), err = %e, "chown failed");
        }
    }
}

/// Create the home directory (and parents) and try to hand it to the agent user.
pub(crate) fn prepare_home_blocking(home: &Path) -> Result<bool> {
    fs::create_dir_all(home).map_err(|e| io_err("create home", e))?;
    Ok(chown_agent(home))
}

/// `(uid, gid)` owning `path`, or `None` when it cannot be read.
pub(crate) fn owner_blocking(path: &Path) -> Option<(u32, u32)> {
    fs::metadata(path).ok().map(|m| (m.uid(), m.gid()))
}

/// Sum of the sizes of the regular files under `root`; 0 when it does not exist.
pub(crate) fn dir_size_blocking(root: &Path) -> i64 {
    let mut total: u64 = 0;
    for entry in WalkDir::new(root).follow_links(false) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(root = %root.display(), err = %e, "disk usage: skipping entry")
            }
        }
    }
    i64::try_from(total).unwrap_or(i64::MAX)
}

/// Copy `src` into `dst` (created when missing): directories with their modes,
/// regular files with their permissions, symlinks as symlinks. Other file types
/// are skipped. Returns the number of files copied.
pub(crate) fn copy_tree_blocking(src: &Path, dst: &Path) -> io::Result<u64> {
    fs::create_dir_all(dst)?;
    let mut dir_modes: Vec<(PathBuf, fs::Permissions)> = Vec::new();
    let mut files = 0u64;
    for entry in WalkDir::new(src).follow_links(false).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(src).map_err(io::Error::other)?;
        let dest = dst.join(relative);
        let kind = entry.file_type();
        if kind.is_dir() {
            fs::create_dir_all(&dest)?;
            dir_modes.push((dest, entry.metadata()?.permissions()));
        } else if kind.is_symlink() {
            let link = fs::read_link(entry.path())?;
            if dest.symlink_metadata().is_ok() {
                fs::remove_file(&dest)?;
            }
            std::os::unix::fs::symlink(link, &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &dest)?;
            files += 1;
        } else {
            tracing::debug!(path = %entry.path().display(), "copy: skipping special file");
        }
    }
    // Deepest first, so a read-only directory never blocks its own children.
    for (dir, perms) in dir_modes.into_iter().rev() {
        if let Err(e) = fs::set_permissions(&dir, perms) {
            tracing::debug!(path = %dir.display(), err = %e, "copy: could not set directory mode");
        }
    }
    Ok(files)
}

fn write_archive(home: &Path, partial: &Path) -> io::Result<i64> {
    let file = fs::File::create(partial)?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    archive.append_dir_all("", home)?;
    let encoder = archive.into_inner()?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    let file = writer.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(i64::try_from(file.metadata()?.len()).unwrap_or(i64::MAX))
}

/// Write a gzip-compressed tar of `home`'s contents to `dest`, through a
/// `.partial` file renamed on success. Returns the archive size in bytes.
pub(crate) fn backup_blocking(home: &Path, dest: &Path) -> io::Result<i64> {
    if !home.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a directory", home.display()),
        ));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = PathBuf::from(format!("{}.partial", dest.display()));
    match write_archive(home, &partial) {
        Ok(size) => {
            fs::rename(&partial, dest)?;
            Ok(size)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Move the finished file `src` to `dest` (parents created): a rename, or a
/// copy through `<dest>.partial` and a delete when the two are on different
/// file systems. Returns the size of `dest`.
pub(crate) fn move_file_blocking(src: &Path, dest: &Path) -> io::Result<i64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Err(e) = fs::rename(src, dest) {
        tracing::debug!(src = %src.display(), dest = %dest.display(), err = %e, "rename failed; copying instead");
        let partial = PathBuf::from(format!("{}.partial", dest.display()));
        if let Err(e) = fs::copy(src, &partial).and_then(|_| fs::rename(&partial, dest)) {
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
        fs::remove_file(src)?;
    }
    let file = fs::File::open(dest)?;
    file.sync_all()?;
    Ok(i64::try_from(file.metadata()?.len()).unwrap_or(i64::MAX))
}

/// Remove everything inside `dir`, creating it when missing, without touching
/// `dir` itself (it may be a live bind-mount source).
pub(crate) fn clear_dir(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        return fs::create_dir_all(dir);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?
        } else {
            fs::remove_file(&path)?
        }
    }
    Ok(())
}

/// Replace `home`'s contents with the tar.gz archive at `src`. Modes are
/// restored, ownership is not (the caller re-owns the tree when it can).
pub(crate) fn restore_blocking(home: &Path, src: &Path) -> io::Result<()> {
    let file = fs::File::open(src)?;
    clear_dir(home)?;
    let mut archive = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(false);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);
    archive.unpack(home)
}

/// `path` is `<instances_dir>/<something>` with only plain components after the prefix.
pub(crate) fn is_instance_storage(instances_dir: &Path, path: &Path) -> bool {
    match path.strip_prefix(instances_dir) {
        Ok(rest) => {
            let components: Vec<Component<'_>> = rest.components().collect();
            !components.is_empty() && components.iter().all(|c| matches!(c, Component::Normal(_)))
        }
        Err(_) => false,
    }
}

/// Delete an instance's storage directory. Refuses paths that are not instance
/// directories under `instances_dir` (`InvalidInput`); a missing directory is fine.
pub(crate) fn remove_storage_blocking(instances_dir: &Path, path: &Path) -> io::Result<()> {
    if !is_instance_storage(instances_dir, path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to remove {} because it is not an instance directory under {}",
                path.display(),
                instances_dir.display()
            ),
        ));
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Create the home directory and try to hand it to the agent user.
pub(crate) async fn prepare_home(home: PathBuf) -> Result<bool> {
    blocking("prepare home", move || prepare_home_blocking(&home)).await
}

/// `(uid, gid)` owning `path` (see [`owner_blocking`]).
pub(crate) async fn owner(path: PathBuf) -> Option<(u32, u32)> {
    tokio::task::spawn_blocking(move || owner_blocking(&path))
        .await
        .unwrap_or(None)
}

/// One attempt at deleting an instance directory on the host.
async fn remove_attempt(instances_dir: PathBuf, path: PathBuf) -> Result<io::Result<()>> {
    blocking("remove storage", move || {
        Ok(remove_storage_blocking(&instances_dir, &path))
    })
    .await
}

/// One attempt at restoring a home on the host.
async fn restore_attempt(home: PathBuf, src: PathBuf) -> Result<io::Result<()>> {
    blocking("restore storage", move || {
        if !src.is_file() {
            return Err(Error::not_found("backup file"));
        }
        Ok(restore_blocking(&home, &src))
    })
    .await
}

impl DockerRuntime {
    /// Bytes used under the instance's storage path.
    pub(crate) async fn disk_usage_impl(&self, inst: &Instance) -> Result<i64> {
        let root = self.storage_path(inst);
        blocking("disk usage", move || Ok(dir_size_blocking(&root))).await
    }

    /// Delete the instance's storage directory. When the daemon may not remove
    /// files the instance created as root, the helper deletes the home and the
    /// removal is retried.
    pub(crate) async fn remove_storage_impl(&self, inst: &Instance) -> Result<()> {
        let instances_dir = self.cfg.instances_dir();
        let storage = self.storage_path(inst);
        match remove_attempt(instances_dir.clone(), storage.clone()).await? {
            Ok(()) => return Ok(()),
            Err(e) if needs_helper(&e) => {
                tracing::warn!(instance = %inst.id, path = %storage.display(), err = %e, "removing the storage was denied; the root helper deletes the home");
            }
            Err(e) => return Err(io_err("remove storage", e)),
        }
        let job = HelperJob::Remove {
            path: storage.join(HOME_DIR),
        };
        self.run_helper(&inst.id, &job).await?;
        remove_attempt(instances_dir, storage)
            .await?
            .map_err(|e| io_err("remove storage after the helper", e))
    }

    /// Copy `src`'s home into a fresh storage directory for `dst`.
    pub(crate) async fn clone_storage_impl(
        &self,
        src: &Instance,
        dst: &mut Instance,
    ) -> Result<()> {
        if dst.id.is_empty() || dst.id == src.id {
            return Err(Error::invalid("clone target must be a different instance"));
        }
        let src_home = self.home_path(src);
        let dst_storage = self.cfg.instances_dir().join(&dst.id);
        let dst_home = dst_storage.join(HOME_DIR);
        let copied = {
            let (src_home, dst_home) = (src_home.clone(), dst_home.clone());
            let (src_id, dst_id) = (src.id.clone(), dst.id.clone());
            blocking("clone storage", move || {
                let owned = prepare_home_blocking(&dst_home)?;
                if !src_home.is_dir() {
                    tracing::warn!(src = %src_id, dst = %dst_id, path = %src_home.display(), "source home missing; clone starts empty");
                    return Ok(Ok(0));
                }
                Ok(copy_tree_blocking(&src_home, &dst_home).inspect(|_| {
                    if owned {
                        chown_tree(&dst_home);
                    }
                }))
            })
            .await?
        };
        let storage = dst_storage.to_string_lossy().into_owned();
        match copied {
            Ok(files) => {
                tracing::info!(src = %src.id, dst = %dst.id, files, %storage, "storage cloned");
            }
            Err(e) if needs_helper(&e) => {
                tracing::warn!(src = %src.id, dst = %dst.id, err = %e, "copying the home was denied; the root helper copies it");
                // Drop whatever the host managed to copy so the helper starts clean.
                let partial = dst_home.clone();
                blocking("clear clone target", move || {
                    clear_dir(&partial).map_err(|e| io_err("clear clone target", e))
                })
                .await?;
                let job = HelperJob::Copy {
                    src: src_home,
                    dst: dst_home,
                };
                self.run_helper(&dst.id, &job).await?;
                tracing::info!(src = %src.id, dst = %dst.id, %storage, "storage cloned by the root helper");
            }
            Err(e) => return Err(io_err("copy home", e)),
        }
        dst.storage_path = storage;
        Ok(())
    }

    /// Write a tar.gz of the instance's home to `dest_path`; returns its size.
    pub(crate) async fn backup_storage_impl(
        &self,
        inst: &Instance,
        dest_path: &Path,
    ) -> Result<i64> {
        let home = self.home_path(inst);
        let dest = dest_path.to_path_buf();
        let archived = {
            let (home, dest) = (home.clone(), dest.clone());
            blocking("backup storage", move || {
                if !home.is_dir() {
                    return Err(Error::not_found("instance storage"));
                }
                Ok(backup_blocking(&home, &dest))
            })
            .await?
        };
        let bytes = match archived {
            Ok(bytes) => bytes,
            Err(e) if needs_helper(&e) => {
                tracing::warn!(instance = %inst.id, err = %e, "reading the home for a backup was denied; the root helper archives it");
                self.backup_via_helper(inst, &home, &dest).await?
            }
            Err(e) => return Err(io_err("backup", e)),
        };
        tracing::info!(instance = %inst.id, bytes, dest = %dest_path.display(), "storage backed up");
        Ok(bytes)
    }

    /// Archive through the helper: the host creates an empty staging file inside
    /// the instance directory (root writing into an existing file leaves its
    /// ownership alone, so the daemon keeps owning it), the helper fills it with
    /// `tar`, and the host moves it to `dest`.
    async fn backup_via_helper(&self, inst: &Instance, home: &Path, dest: &Path) -> Result<i64> {
        let staging = self
            .storage_path(inst)
            .join(format!(".backup-{}.tar.gz.partial", ids::new_id()));
        {
            let staging = staging.clone();
            blocking("stage backup", move || {
                fs::File::create(&staging)
                    .map(drop)
                    .map_err(|e| io_err("stage backup", e))
            })
            .await?;
        }
        let job = HelperJob::Archive {
            home: home.to_path_buf(),
            archive: staging.clone(),
        };
        if let Err(e) = self.run_helper(&inst.id, &job).await {
            let leftover = staging.clone();
            let _ = blocking("discard staged backup", move || {
                Ok(fs::remove_file(&leftover))
            })
            .await;
            return Err(e);
        }
        let dest = dest.to_path_buf();
        blocking("move backup", move || {
            move_file_blocking(&staging, &dest).map_err(|e| io_err("move backup", e))
        })
        .await
    }

    /// Replace the instance's home with the tar.gz at `src_path`.
    pub(crate) async fn restore_storage_impl(
        &self,
        inst: &Instance,
        src_path: &Path,
    ) -> Result<()> {
        let home = self.home_path(inst);
        let src = src_path.to_path_buf();
        match restore_attempt(home.clone(), src.clone()).await? {
            Ok(()) => {}
            Err(e) if needs_helper(&e) => {
                tracing::warn!(instance = %inst.id, err = %e, "clearing the home for a restore was denied; the root helper empties it");
                // The helper hands the emptied home back to whoever owns the
                // storage directory (the daemon created it), so the unpack can write.
                let storage = self.storage_path(inst);
                let owner = owner(storage).await.unwrap_or((AGENT_UID, AGENT_GID));
                let job = HelperJob::Clear {
                    dir: home.clone(),
                    owner,
                };
                self.run_helper(&inst.id, &job).await?;
                restore_attempt(home.clone(), src)
                    .await?
                    .map_err(|e| io_err("restore after the helper", e))?;
            }
            Err(e) => return Err(io_err("restore", e)),
        }
        blocking("re-own restored home", move || {
            if chown_agent(&home) {
                chown_tree(&home);
            }
            Ok(())
        })
        .await?;
        tracing::info!(instance = %inst.id, src = %src_path.display(), "storage restored");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::tests::scratch_dir;

    fn populate(home: &Path) {
        fs::create_dir_all(home.join("project/src")).unwrap();
        fs::write(home.join("project/src/main.rs"), b"fn main() {}").unwrap();
        fs::write(home.join("notes.txt"), vec![b'n'; 3000]).unwrap();
        fs::write(home.join("project/run.sh"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(
            home.join("project/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("notes.txt", home.join("link")).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn disk_usage_sums_regular_files() {
        let root = scratch_dir("du");
        assert_eq!(dir_size_blocking(&root.join("missing")), 0);
        populate(&root);
        assert_eq!(dir_size_blocking(&root), 12 + 3000 + 10);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copies_trees_with_modes_and_symlinks() {
        let root = scratch_dir("copy");
        let (src, dst) = (root.join("src"), root.join("dst"));
        populate(&src);
        fs::create_dir_all(src.join("locked")).unwrap();
        fs::write(src.join("locked/f"), b"x").unwrap();
        fs::set_permissions(src.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        let files = copy_tree_blocking(&src, &dst).unwrap();
        assert_eq!(files, 4);
        assert_eq!(
            fs::read(dst.join("project/src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert_eq!(mode(&dst.join("project/run.sh")), 0o755);
        assert_eq!(mode(&dst.join("locked")), 0o555);
        assert_eq!(
            fs::read_link(dst.join("link")).unwrap(),
            PathBuf::from("notes.txt")
        );
        assert_eq!(dir_size_blocking(&dst), 12 + 3000 + 10 + 1);
        fs::set_permissions(src.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dst.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn backup_and_restore_round_trip() {
        let root = scratch_dir("backup");
        let home = root.join("home");
        populate(&home);
        let archive = root.join("backups/one.tar.gz");
        let size = backup_blocking(&home, &archive).unwrap();
        assert!(size > 0);
        assert_eq!(fs::metadata(&archive).unwrap().len(), size as u64);
        assert!(!root.join("backups/one.tar.gz.partial").exists());

        // Change the tree, then restore: additions vanish, modes and links come back.
        fs::write(home.join("project/src/main.rs"), b"changed").unwrap();
        fs::write(home.join("extra.bin"), b"gone after restore").unwrap();
        fs::create_dir_all(home.join("extra-dir")).unwrap();
        restore_blocking(&home, &archive).unwrap();
        assert_eq!(
            fs::read(home.join("project/src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert!(!home.join("extra.bin").exists());
        assert!(!home.join("extra-dir").exists());
        assert_eq!(mode(&home.join("project/run.sh")), 0o755);
        assert_eq!(
            fs::read_link(home.join("link")).unwrap(),
            PathBuf::from("notes.txt")
        );
        assert_eq!(dir_size_blocking(&home), 12 + 3000 + 10);

        assert!(backup_blocking(&root.join("nope"), &archive).is_err());
        assert!(restore_blocking(&home, &root.join("missing.tar.gz")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_into_missing_home_creates_it() {
        let root = scratch_dir("restore-new");
        let home = root.join("home");
        populate(&home);
        let archive = root.join("b.tar.gz");
        backup_blocking(&home, &archive).unwrap();
        let fresh = root.join("fresh/home");
        restore_blocking(&fresh, &archive).unwrap();
        assert!(fresh.join("project/src/main.rs").is_file());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn storage_removal_is_guarded() {
        let root = scratch_dir("remove");
        let instances = root.join("instances");
        let inst = instances.join("ab12cd34ef");
        fs::create_dir_all(inst.join("home")).unwrap();
        assert!(is_instance_storage(&instances, &inst));
        assert!(!is_instance_storage(&instances, &instances));
        assert!(!is_instance_storage(&instances, &root));
        assert!(!is_instance_storage(
            &instances,
            &instances.join("../instances/x")
        ));
        assert!(!is_instance_storage(&instances, Path::new("/")));
        let refused = remove_storage_blocking(&instances, &instances).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert!(
            !needs_helper(&refused),
            "a refused path is never retried as root"
        );
        assert!(remove_storage_blocking(&instances, &root).is_err());
        remove_storage_blocking(&instances, &inst).unwrap();
        assert!(!inst.exists());
        remove_storage_blocking(&instances, &inst).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prepare_home_creates_directories() {
        let root = scratch_dir("home");
        let home = root.join("a/b/home");
        prepare_home_blocking(&home).unwrap();
        assert!(home.is_dir());
        prepare_home_blocking(&home).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn owner_is_read_from_the_directory() {
        let root = scratch_dir("owner");
        let meta = fs::metadata(&root).unwrap();
        assert_eq!(owner_blocking(&root), Some((meta.uid(), meta.gid())));
        assert_eq!(owner_blocking(&root.join("missing")), None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// A directory the daemon may not enter is what a root-owned tree in a home
    /// looks like to a non-root daemon: removal, clear, copy and backup all fail
    /// with `PermissionDenied`, the one error the helper fallback reacts to, and
    /// succeed again once the tree is accessible.
    #[test]
    fn permission_denied_is_what_triggers_the_helper() {
        let root = scratch_dir("denied");
        if owner_blocking(&root).map(|(uid, _)| uid) == Some(0) {
            eprintln!("skipping: running as root, nothing is denied");
            fs::remove_dir_all(&root).unwrap();
            return;
        }
        let instances = root.join("instances");
        let inst = instances.join("ab12cd34ef");
        let home = inst.join("home");
        let locked = home.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("f"), b"x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let err = remove_storage_blocking(&instances, &inst).unwrap_err();
        assert!(needs_helper(&err), "remove: {err}");
        let err = clear_dir(&home).unwrap_err();
        assert!(needs_helper(&err), "clear: {err}");
        let err = copy_tree_blocking(&home, &root.join("copy")).unwrap_err();
        assert!(needs_helper(&err), "copy: {err}");
        let err = backup_blocking(&home, &root.join("b.tar.gz")).unwrap_err();
        assert!(needs_helper(&err), "backup: {err}");
        assert!(!root.join("b.tar.gz.partial").exists());

        // Once the "helper" has made the tree accessible the retry succeeds.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(copy_tree_blocking(&home, &root.join("copy")).unwrap(), 1);
        remove_storage_blocking(&instances, &inst).unwrap();
        assert!(!inst.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn moves_files_into_new_directories() {
        let root = scratch_dir("move");
        let src = root.join("staged.tar.gz.partial");
        fs::write(&src, b"archive bytes").unwrap();
        let dest = root.join("backups/ab12cd34ef/b1.tar.gz");
        assert_eq!(move_file_blocking(&src, &dest).unwrap(), 13);
        assert!(!src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"archive bytes");
        assert!(
            move_file_blocking(&src, &dest).is_err(),
            "nothing left to move"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
