//! Root helper for storage work the daemon cannot do itself.
//!
//! The gateway runs as root inside every instance, so files it writes into the
//! bind-mounted home (and files an agent shell creates as root) are root-owned on
//! the host. A `warrend` that runs as an ordinary user then gets
//! `PermissionDenied` when it removes, copies or reads them. The fallback is a
//! short-lived [`HELPER_IMAGE`] container with the instances directory
//! bind-mounted at [`HELPER_MOUNT`] that runs one fixed command as root (`rm`,
//! `find -delete`, `cp -a` or `tar`), exits, and is removed again. Every host path
//! handed to it passes [`is_instance_storage`] first, so the helper can never touch
//! anything outside the instances directory. It runs on Docker's default OCI
//! runtime: its job is host file-system work, not isolation.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

use bollard::{
    errors::Error as DockerError,
    models::{ContainerCreateBody, HostConfig},
    query_parameters::{CreateContainerOptions, WaitContainerOptions},
};
use futures::StreamExt;
use warren_core::{Error, Result, ids};

use crate::{
    DockerRuntime, HELPER_IMAGE, HELPER_MOUNT, HELPER_PREFIX, HELPER_TIMEOUT, LABEL_HELPER, error,
    storage::is_instance_storage,
};

/// Whether a failed host-side operation is worth retrying through the helper:
/// only a permission problem is, everything else (missing files, full disk,
/// I/O errors) would fail the same way as root.
pub(crate) fn needs_helper(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

/// One job for the helper container. Paths are host paths under the instances
/// directory; [`HelperJob::command`] translates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelperJob {
    /// `rm -rf <path>`: delete a tree, typically an instance's `home`.
    Remove { path: PathBuf },
    /// Empty `dir` without removing it (it may be a live bind-mount source) and
    /// hand the directory itself to `owner` (`uid`, `gid`) so the daemon can
    /// refill it.
    Clear { dir: PathBuf, owner: (u32, u32) },
    /// `cp -a <src>/. <dst>/`: copy the contents of one home into another,
    /// keeping modes, ownership and symlinks. `dst` must exist.
    Copy { src: PathBuf, dst: PathBuf },
    /// `tar -czf <archive> -C <home> .`: archive a home into a file that must
    /// also live under the instances directory.
    Archive { home: PathBuf, archive: PathBuf },
}

impl HelperJob {
    /// Short name for logs and the container name.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            HelperJob::Remove { .. } => "remove",
            HelperJob::Clear { .. } => "clear",
            HelperJob::Copy { .. } => "copy",
            HelperJob::Archive { .. } => "archive",
        }
    }

    /// The command line for the helper, every host path translated to its
    /// location under [`HELPER_MOUNT`]. Fails when a path is not an instance path
    /// under `instances_dir`. Shell jobs take their paths as positional
    /// parameters, so no path is ever interpreted by the shell.
    pub(crate) fn command(&self, instances_dir: &Path) -> Result<Vec<String>> {
        let arg = |s: &str| s.to_string();
        Ok(match self {
            HelperJob::Remove { path } => {
                vec![arg("rm"), arg("-rf"), helper_path(instances_dir, path)?]
            }
            HelperJob::Clear {
                dir,
                owner: (uid, gid),
            } => vec![
                arg("sh"),
                arg("-c"),
                arg(r#"find "$1" -mindepth 1 -delete && chown "$2" "$1""#),
                arg("clear"),
                helper_path(instances_dir, dir)?,
                format!("{uid}:{gid}"),
            ],
            HelperJob::Copy { src, dst } => vec![
                arg("cp"),
                arg("-a"),
                format!("{}/.", helper_path(instances_dir, src)?),
                format!("{}/", helper_path(instances_dir, dst)?),
            ],
            HelperJob::Archive { home, archive } => vec![
                arg("tar"),
                arg("-czf"),
                helper_path(instances_dir, archive)?,
                arg("-C"),
                helper_path(instances_dir, home)?,
                arg("."),
            ],
        })
    }
}

/// `<instances_dir>/<id>/...` as the helper sees it: `/w/<id>/...`. Anything
/// that is not an instance path under `instances_dir` is refused.
pub(crate) fn helper_path(instances_dir: &Path, path: &Path) -> Result<String> {
    if !is_instance_storage(instances_dir, path) {
        return Err(Error::internal(format!(
            "refusing to hand {} to the storage helper: not an instance path under {}",
            path.display(),
            instances_dir.display()
        )));
    }
    let rest = path
        .strip_prefix(instances_dir)
        .map_err(|e| Error::internal(format!("helper path {}: {e}", path.display())))?;
    Ok(Path::new(HELPER_MOUNT)
        .join(rest)
        .to_string_lossy()
        .into_owned())
}

impl DockerRuntime {
    /// Run `job` as root in a throwaway helper container and wait for it to exit
    /// with status 0. Reaching this means the daemon could not do the work itself,
    /// so it is logged at warn. The helper is removed afterwards in every case,
    /// including a timeout (which kills it).
    pub(crate) async fn run_helper(&self, instance_id: &str, job: &HelperJob) -> Result<()> {
        let instances_dir = self.cfg.instances_dir();
        if !instances_dir.is_absolute() {
            return Err(Error::internal(format!(
                "instances directory {} is not absolute; cannot bind-mount it into the storage helper",
                instances_dir.display()
            )));
        }
        let cmd = job.command(&instances_dir)?;
        tracing::warn!(
            instance = %instance_id,
            job = job.name(),
            command = ?cmd,
            "the daemon lacks permission on the instance's files; running the root storage helper"
        );
        self.ensure_image_impl(HELPER_IMAGE).await?;
        let name = format!("{HELPER_PREFIX}{}-{}", job.name(), ids::new_instance_id());
        let body = ContainerCreateBody {
            image: Some(HELPER_IMAGE.to_string()),
            cmd: Some(cmd),
            user: Some("0:0".to_string()),
            labels: Some(HashMap::from([(
                LABEL_HELPER.to_string(),
                instance_id.to_string(),
            )])),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{}:{HELPER_MOUNT}", instances_dir.display())]),
                network_mode: Some("none".to_string()),
                // Removed explicitly below instead of AutoRemove: an auto-removed
                // container that finishes in milliseconds can vanish before the
                // wait is registered, and its exit code with it.
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let options = CreateContainerOptions {
            name: Some(name.clone()),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(Some(options), body)
            .await
            .map_err(|e| error::map(e, "helper image"))?;
        let outcome = self.drive_helper(&created.id, instance_id, job).await;
        if let Err(e) = self.remove_container(&created.id).await {
            tracing::warn!(container = %name, err = %e, "could not remove the storage helper container");
        }
        outcome
    }

    /// Start the helper and wait for it to exit; `Ok` only for exit status 0.
    async fn drive_helper(
        &self,
        container: &str,
        instance_id: &str,
        job: &HelperJob,
    ) -> Result<()> {
        self.docker
            .start_container(container, None)
            .await
            .map_err(|e| error::map(e, "helper container"))?;
        // `not-running` is issued after the start, so it reports the exit code
        // whether the job is still running or already finished.
        let mut wait = std::pin::pin!(
            self.docker
                .wait_container(container, Some(WaitContainerOptions::default()))
        );
        let describe = || format!("storage helper {} for instance {instance_id}", job.name());
        match tokio::time::timeout(HELPER_TIMEOUT, wait.next()).await {
            Ok(Some(Ok(response))) if response.status_code == 0 => {
                tracing::info!(instance = %instance_id, job = job.name(), "storage helper finished");
                Ok(())
            }
            Ok(Some(Ok(response))) => {
                let logs = self.tail_logs(container, 20).await;
                Err(Error::internal(format!(
                    "{} exited with code {}: {logs}",
                    describe(),
                    response.status_code
                )))
            }
            Ok(Some(Err(DockerError::DockerContainerWaitError { error: cause, code }))) => {
                let logs = self.tail_logs(container, 20).await;
                Err(Error::internal(format!(
                    "{} exited with code {code}: {cause} {logs}",
                    describe()
                )))
            }
            Ok(Some(Err(e))) => Err(error::map(e, "helper container")),
            Ok(None) => Err(Error::internal(format!(
                "{}: docker closed the wait stream without a result",
                describe()
            ))),
            Err(_) => {
                tracing::warn!(instance = %instance_id, job = job.name(), "storage helper timed out; killing it");
                Err(Error::timeout(format!(
                    "{} did not finish within {} s",
                    describe(),
                    HELPER_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instances() -> PathBuf {
        PathBuf::from("/var/lib/warren/instances")
    }

    #[test]
    fn permission_denied_is_the_only_helper_trigger() {
        assert!(needs_helper(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!needs_helper(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!needs_helper(&io::Error::other("disk on fire")));
    }

    #[test]
    fn translates_paths_under_the_instances_dir_only() {
        let dir = instances();
        assert_eq!(
            helper_path(&dir, &dir.join("ab12cd34ef/home")).unwrap(),
            "/w/ab12cd34ef/home"
        );
        assert_eq!(
            helper_path(&dir, &dir.join("ab12cd34ef")).unwrap(),
            "/w/ab12cd34ef"
        );
        for outside in [
            dir.clone(),
            dir.join(".."),
            dir.join("ab12cd34ef/../../etc"),
            PathBuf::from("/var/lib/warren/instances-old/ab12cd34ef"),
            PathBuf::from("/"),
            PathBuf::from("/etc/passwd"),
            PathBuf::from("ab12cd34ef/home"),
        ] {
            let err = helper_path(&dir, &outside).unwrap_err();
            assert!(err.is_internal(), "{}: {err}", outside.display());
        }
    }

    #[test]
    fn builds_the_commands() {
        let dir = instances();
        let home = dir.join("ab12cd34ef/home");
        assert_eq!(
            HelperJob::Remove { path: home.clone() }
                .command(&dir)
                .unwrap(),
            vec!["rm", "-rf", "/w/ab12cd34ef/home"]
        );
        assert_eq!(
            HelperJob::Clear {
                dir: home.clone(),
                owner: (1000, 100)
            }
            .command(&dir)
            .unwrap(),
            vec![
                "sh",
                "-c",
                r#"find "$1" -mindepth 1 -delete && chown "$2" "$1""#,
                "clear",
                "/w/ab12cd34ef/home",
                "1000:100"
            ]
        );
        assert_eq!(
            HelperJob::Copy {
                src: home.clone(),
                dst: dir.join("zz98yy76xx/home")
            }
            .command(&dir)
            .unwrap(),
            vec!["cp", "-a", "/w/ab12cd34ef/home/.", "/w/zz98yy76xx/home/"]
        );
        assert_eq!(
            HelperJob::Archive {
                home: home.clone(),
                archive: dir.join("ab12cd34ef/.backup-1.tar.gz.partial")
            }
            .command(&dir)
            .unwrap(),
            vec![
                "tar",
                "-czf",
                "/w/ab12cd34ef/.backup-1.tar.gz.partial",
                "-C",
                "/w/ab12cd34ef/home",
                "."
            ]
        );
        assert_eq!(
            [
                HelperJob::Remove { path: home.clone() }.name(),
                HelperJob::Clear {
                    dir: home.clone(),
                    owner: (0, 0)
                }
                .name(),
                HelperJob::Copy {
                    src: home.clone(),
                    dst: home.clone()
                }
                .name(),
                HelperJob::Archive {
                    home: home.clone(),
                    archive: home.clone()
                }
                .name(),
            ],
            ["remove", "clear", "copy", "archive"]
        );
    }

    #[test]
    fn every_path_of_a_job_is_guarded() {
        let dir = instances();
        let inside = dir.join("ab12cd34ef/home");
        let outside = PathBuf::from("/home/azureuser");
        assert!(
            HelperJob::Remove {
                path: outside.clone()
            }
            .command(&dir)
            .is_err()
        );
        assert!(
            HelperJob::Clear {
                dir: dir.clone(),
                owner: (1000, 1000)
            }
            .command(&dir)
            .is_err()
        );
        assert!(
            HelperJob::Copy {
                src: outside.clone(),
                dst: inside.clone()
            }
            .command(&dir)
            .is_err()
        );
        assert!(
            HelperJob::Copy {
                src: inside.clone(),
                dst: outside.clone()
            }
            .command(&dir)
            .is_err()
        );
        assert!(
            HelperJob::Archive {
                home: inside.clone(),
                archive: PathBuf::from("/var/lib/warren/backups/x.tar.gz")
            }
            .command(&dir)
            .is_err(),
            "the archive must be staged inside the instances directory"
        );
        assert!(
            HelperJob::Archive {
                home: outside,
                archive: inside
            }
            .command(&dir)
            .is_err()
        );
    }
}
