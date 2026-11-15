//! Giving an exported image tree the gateway it lacks, and telling that gateway
//! what the image itself would have run.
//!
//! The built-in (system) images contain `/usr/local/bin/warren-gateway` and the
//! kernel starts it as PID 1 (`init=... -- --init`). A workspace template is
//! registered from an arbitrary image (`alpine:3.20`, an n8n image) that has no
//! gateway and often no long-running process at all. For those, the fully
//! static gateway the daemon was configured with (`--gateway-bin`) is copied to
//! the init path so the kernel command line keeps working, and [`WRAP_FILE`]
//! records the image's own entrypoint and command for the gateway to supervise
//! while it serves the Warren API. The daemon decides by template scope; this
//! module only carries the decision out.

use std::{os::unix::fs::PermissionsExt, path::Path};

use warren_core::{Error, Result};

use crate::{rootfs, vmconfig::GUEST_INIT};

/// The wrap file, relative to the root of the image tree: `/etc/warren.wrap.json`
/// in the guest. It is written into workspace-scope images only; system images
/// never carry it, which is also how a rebuild that has no template at hand
/// (restore) knows the image needed the gateway injected.
///
/// # Format
///
/// A UTF-8 JSON document, mode 0644, holding one array of strings followed by
/// a newline: the image's `Config.Entrypoint` and then its `Config.Cmd` exactly
/// as `docker image inspect` reports them, with `null` contributing nothing.
/// That is the argv Docker would have executed for a container of the image:
/// element 0 is the program (looked up on `PATH` when it has no slash; Docker
/// has already turned shell-form instructions into `["/bin/sh", "-c", "..."]`),
/// the rest are its arguments. An empty array means the image declares no
/// command of its own and the gateway has nothing to supervise. Examples:
///
/// ```json
/// ["docker-entrypoint.sh","redis-server"]
/// ```
///
/// ```json
/// ["/bin/sh"]
/// ```
///
/// The image's `Env`, `WorkingDir` and `User` are not part of this format.
pub const WRAP_FILE: &str = "etc/warren.wrap.json";

/// What [`ensure_gateway`] found at the init path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayInjection {
    /// The image ships its own `/usr/local/bin/warren-gateway`; it was kept.
    AlreadyPresent,
    /// The daemon's gateway binary was copied in.
    Injected,
}

/// Make sure the tree at `rootfs_dir` has a gateway at [`GUEST_INIT`]
/// (`/usr/local/bin/warren-gateway`), the path the kernel command line boots.
/// A regular file already there is kept (symlinks are followed as the host
/// sees them, so a relative link inside the image counts); otherwise
/// `gateway_bin` is copied in with mode 0755, creating directories and
/// replacing a dangling link. With no binary configured and none in the image
/// the VM could never serve the API, so that is `not_supported` and names
/// `--gateway-bin`.
pub fn ensure_gateway(rootfs_dir: &Path, gateway_bin: Option<&Path>) -> Result<GatewayInjection> {
    let target = rootfs::guest_to_host(rootfs_dir, GUEST_INIT);
    if std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
        return Ok(GatewayInjection::AlreadyPresent);
    }
    let Some(bin) = gateway_bin else {
        return Err(Error::not_supported(format!(
            "this image has no {GUEST_INIT} and the daemon has no gateway binary to inject; \
             start warrend with --gateway-bin <path to a static warren-gateway>"
        )));
    };
    if !bin.is_file() {
        return Err(Error::not_supported(format!(
            "the gateway binary {} (--gateway-bin) is missing or not a regular file",
            bin.display()
        )));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
    }
    if std::fs::symlink_metadata(&target).is_ok() {
        std::fs::remove_file(&target)
            .map_err(|e| Error::internal(format!("remove {}: {e}", target.display())))?;
    }
    std::fs::copy(bin, &target).map_err(|e| {
        Error::internal(format!(
            "copy {} to {}: {e}",
            bin.display(),
            target.display()
        ))
    })?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| Error::internal(format!("chmod {}: {e}", target.display())))?;
    Ok(GatewayInjection::Injected)
}

/// The text of [`WRAP_FILE`] for `command`: one compact JSON array of strings
/// and a newline.
pub fn render_wrap(command: &[String]) -> String {
    let mut text = serde_json::Value::from(command).to_string();
    text.push('\n');
    text
}

/// Write [`WRAP_FILE`] (mode 0644) into an exported root directory, replacing
/// anything the image had at that path.
pub fn write_wrap_file(rootfs_dir: &Path, command: &[String]) -> Result<()> {
    rootfs::write_file(&rootfs_dir.join(WRAP_FILE), &render_wrap(command), 0o644)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::testutil::TempDir;
    use warren_core::errors::CODE_NOT_SUPPORTED;

    /// A fake export: an image tree with `/etc` and nothing else.
    fn tree(tmp: &TempDir, name: &str) -> PathBuf {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn copies_the_gateway_in_when_the_image_lacks_it() {
        let tmp = TempDir::new("fc-inject");
        let bin = tmp.path().join("warren-gateway");
        std::fs::write(&bin, "static gateway").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o600)).unwrap();
        let dir = tree(&tmp, "alpine");
        assert_eq!(
            ensure_gateway(&dir, Some(&bin)).unwrap(),
            GatewayInjection::Injected
        );
        let target = dir.join("usr/local/bin/warren-gateway");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "static gateway");
        assert_eq!(mode(&target), 0o755);
        // Running again finds the copy and leaves it alone.
        std::fs::write(&bin, "newer build").unwrap();
        assert_eq!(
            ensure_gateway(&dir, Some(&bin)).unwrap(),
            GatewayInjection::AlreadyPresent
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "static gateway");
    }

    #[test]
    fn keeps_the_images_own_gateway() {
        let tmp = TempDir::new("fc-inject-own");
        let dir = tree(&tmp, "derived");
        std::fs::create_dir_all(dir.join("usr/local/bin")).unwrap();
        let own = dir.join("usr/local/bin/warren-gateway");
        std::fs::write(&own, "image gateway").unwrap();
        std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = tmp.path().join("warren-gateway");
        std::fs::write(&bin, "daemon gateway").unwrap();
        assert_eq!(
            ensure_gateway(&dir, Some(&bin)).unwrap(),
            GatewayInjection::AlreadyPresent
        );
        assert_eq!(std::fs::read_to_string(&own).unwrap(), "image gateway");
        assert_eq!(mode(&own), 0o700, "an existing file is not touched");
        // Nothing to inject is fine when the image has one.
        assert_eq!(
            ensure_gateway(&dir, None).unwrap(),
            GatewayInjection::AlreadyPresent
        );
        // A relative symlink to a file inside the image counts as present.
        let dir = tree(&tmp, "linked");
        std::fs::create_dir_all(dir.join("usr/local/bin")).unwrap();
        std::fs::create_dir_all(dir.join("opt/gw")).unwrap();
        std::fs::write(dir.join("opt/gw/warren-gateway"), "x").unwrap();
        std::os::unix::fs::symlink(
            "../../../opt/gw/warren-gateway",
            dir.join("usr/local/bin/warren-gateway"),
        )
        .unwrap();
        assert_eq!(
            ensure_gateway(&dir, None).unwrap(),
            GatewayInjection::AlreadyPresent
        );
    }

    #[test]
    fn refuses_a_foreign_image_without_a_configured_gateway() {
        let tmp = TempDir::new("fc-inject-none");
        let dir = tree(&tmp, "alpine");
        let err = ensure_gateway(&dir, None).unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
        assert!(err.message.contains("--gateway-bin"), "{}", err.message);
        assert!(!dir.join("usr").exists(), "nothing is created on failure");
        // A configured path that does not exist is reported the same way.
        let err = ensure_gateway(&dir, Some(&tmp.path().join("missing"))).unwrap_err();
        assert_eq!(err.code, CODE_NOT_SUPPORTED);
        assert!(err.message.contains("--gateway-bin"), "{}", err.message);
        assert!(!dir.join("usr").exists());
    }

    #[test]
    fn replaces_a_dangling_link() {
        let tmp = TempDir::new("fc-inject-dangling");
        let dir = tree(&tmp, "odd");
        std::fs::create_dir_all(dir.join("usr/local/bin")).unwrap();
        let target = dir.join("usr/local/bin/warren-gateway");
        std::os::unix::fs::symlink("/nonexistent/warren-gateway", &target).unwrap();
        let bin = tmp.path().join("warren-gateway");
        std::fs::write(&bin, "daemon gateway").unwrap();
        assert_eq!(
            ensure_gateway(&dir, Some(&bin)).unwrap(),
            GatewayInjection::Injected
        );
        assert!(std::fs::symlink_metadata(&target).unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "daemon gateway");
        assert_eq!(mode(&target), 0o755);
    }

    #[test]
    fn wrap_file_is_a_json_array_of_the_command() {
        assert_eq!(render_wrap(&[]), "[]\n");
        assert_eq!(render_wrap(&["/bin/sh".to_string()]), "[\"/bin/sh\"]\n");
        let cmd = vec![
            "docker-entrypoint.sh".to_string(),
            "redis-server".to_string(),
            "--save \"60 1\"".to_string(),
            "naïve".to_string(),
        ];
        let text = render_wrap(&cmd);
        assert_eq!(
            text,
            "[\"docker-entrypoint.sh\",\"redis-server\",\"--save \\\"60 1\\\"\",\"naïve\"]\n"
        );
        let back: Vec<String> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, cmd);

        let tmp = TempDir::new("fc-wrap");
        let dir = tree(&tmp, "redis");
        // Whatever the image had at the path, file or link, is replaced.
        std::os::unix::fs::symlink("/dev/null", dir.join(WRAP_FILE)).unwrap();
        write_wrap_file(&dir, &cmd).unwrap();
        let path = dir.join(WRAP_FILE);
        assert_eq!(path, dir.join("etc/warren.wrap.json"));
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(mode(&path), 0o644);
        // A scratch image has no /etc at all.
        let dir = tmp.path().join("scratch");
        std::fs::create_dir_all(&dir).unwrap();
        write_wrap_file(&dir, &[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(WRAP_FILE)).unwrap(),
            "[]\n"
        );
    }
}
