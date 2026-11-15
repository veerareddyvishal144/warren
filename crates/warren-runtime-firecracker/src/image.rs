//! Template images through the Docker CLI: inspect, pull and export a
//! container's filesystem into a directory for `mkfs.ext4 -d`.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use warren_core::{Error, Result};

use crate::host::{self, lossy};

/// Pull the image when it is missing locally and return its digest.
pub async fn ensure(image: &str) -> Result<String> {
    if let Some(d) = inspect_digest(image).await? {
        return Ok(d);
    }
    tracing::info!(image, "pulling image");
    let docker = host::tool("docker")?;
    host::run(&docker, &["pull", image]).await?;
    inspect_digest(image).await?.ok_or_else(|| {
        Error::internal(format!(
            "docker pull {image} succeeded but the image is still missing"
        ))
    })
}

/// `docker image inspect` → digest, or `None` when the image is not present.
pub async fn inspect_digest(image: &str) -> Result<Option<String>> {
    let docker = host::tool("docker")?;
    let out = tokio::process::Command::new(&docker)
        .args(["image", "inspect", image])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| Error::internal(format!("docker: {e}")))?;
    if !out.status.success() {
        let err = lossy(&out.stderr);
        if err.to_ascii_lowercase().contains("no such image") {
            return Ok(None);
        }
        return Err(Error::internal(format!(
            "docker image inspect {image} failed: {err}"
        )));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        Error::internal(format!(
            "docker image inspect {image}: unreadable output: {e}"
        ))
    })?;
    digest_from_inspect(&v).map(Some).ok_or_else(|| {
        Error::internal(format!(
            "docker image inspect {image}: no digest or id in output"
        ))
    })
}

/// The content digest from an inspect document: the first `RepoDigests` entry
/// (`repo@sha256:...`) when the image came from a registry, else its `Id`.
pub fn digest_from_inspect(v: &serde_json::Value) -> Option<String> {
    let img = v.as_array().and_then(|a| a.first()).unwrap_or(v);
    let repo_digest = img
        .get("RepoDigests")
        .and_then(|d| d.as_array())
        .and_then(|d| d.iter().find_map(|x| x.as_str()))
        .map(|s| {
            s.rsplit_once('@')
                .map(|(_, d)| d.to_string())
                .unwrap_or_else(|| s.to_string())
        });
    repo_digest
        .or_else(|| {
            img.get("Id")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
}

/// Export the image's filesystem into `dest` (created if needed). The container
/// is created with a harmless entrypoint so images without a command export too,
/// and always removed afterwards.
pub async fn export_rootfs(image: &str, dest: &Path) -> Result<()> {
    let docker = host::tool("docker")?;
    let created = host::run(&docker, &["create", "--entrypoint", "/bin/true", image]).await?;
    let cid = lossy(&created.stdout)
        .lines()
        .last()
        .unwrap_or_default()
        .trim()
        .to_string();
    if cid.is_empty() {
        return Err(Error::internal(format!(
            "docker create {image} printed no container id"
        )));
    }
    let result = {
        let (docker, cid, dest) = (docker.clone(), cid.clone(), dest.to_path_buf());
        tokio::task::spawn_blocking(move || extract_export(&docker, &cid, &dest))
            .await
            .map_err(|e| Error::internal(format!("export task failed: {e}")))?
    };
    if let Err(e) = host::run(&docker, &["rm", "-f", &cid]).await {
        tracing::warn!(container = %cid, error = %e, "cannot remove export container");
    }
    result
}

/// Stream `docker export <cid>` straight into the tar unpacker.
fn extract_export(docker: &PathBuf, cid: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)
        .map_err(|e| Error::internal(format!("mkdir {}: {e}", dest.display())))?;
    let mut child = std::process::Command::new(docker)
        .args(["export", cid])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::internal(format!("docker export: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::internal("docker export: no stdout"))?;
    let unpacked = {
        let mut archive = tar::Archive::new(std::io::BufReader::with_capacity(1 << 20, stdout));
        configure_unpack(&mut archive);
        archive.unpack(dest)
    };
    if unpacked.is_err() {
        // The exporter would otherwise block forever on a full pipe.
        let _ = child.kill();
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::internal(format!("docker export: {e}")))?;
    if let Err(e) = unpacked {
        return Err(Error::internal(format!(
            "extracting docker export of {cid}: {e}"
        )));
    }
    if !out.status.success() {
        return Err(Error::internal(format!(
            "docker export {cid} failed ({}): {}",
            out.status,
            lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Keep modes including setuid bits, and ownership and xattrs (capabilities)
/// when running as root, which is what the guest kernel will see.
pub fn configure_unpack<R: std::io::Read>(archive: &mut tar::Archive<R>) {
    let root = host::is_root();
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(root);
    archive.set_unpack_xattrs(root);
    archive.set_overwrite(true);
    archive.set_preserve_mtime(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_prefers_repo_digest_then_id() {
        let v = serde_json::json!([{
            "Id": "sha256:aaaa",
            "RepoDigests": ["alpine@sha256:bbbb"]
        }]);
        assert_eq!(digest_from_inspect(&v).as_deref(), Some("sha256:bbbb"));
        let v = serde_json::json!([{ "Id": "sha256:aaaa", "RepoDigests": [] }]);
        assert_eq!(digest_from_inspect(&v).as_deref(), Some("sha256:aaaa"));
        let v = serde_json::json!([{ "Id": "sha256:aaaa" }]);
        assert_eq!(digest_from_inspect(&v).as_deref(), Some("sha256:aaaa"));
        assert_eq!(digest_from_inspect(&serde_json::json!([])), None);
        assert_eq!(digest_from_inspect(&serde_json::json!({"Id": ""})), None);
    }

    #[tokio::test]
    async fn inspect_and_export_with_docker() {
        let Some(image) = crate::testutil::docker_test_image().await else {
            eprintln!("skipping: docker or the test image is unavailable");
            return;
        };
        assert_eq!(
            inspect_digest("warren-test/does-not-exist:zzz")
                .await
                .unwrap(),
            None
        );
        let digest = ensure(&image).await.unwrap();
        assert!(digest.starts_with("sha256:"), "{digest}");
        let tmp = crate::testutil::TempDir::new("fc-export");
        let dest = tmp.path().join("rootfs-dir");
        export_rootfs(&image, &dest).await.unwrap();
        assert!(dest.join("etc").is_dir());
        assert!(dest.join("bin").exists());
        assert!(
            export_rootfs("warren-test/does-not-exist:zzz", &tmp.path().join("x"))
                .await
                .is_err()
        );
    }
}
