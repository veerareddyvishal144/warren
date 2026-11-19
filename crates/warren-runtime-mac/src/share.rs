//! The per-instance folder a macOS VM mounts read-only: the gateway binary
//! and its environment. Inside the guest it appears at [`GUEST_SHARE_PATH`];
//! a LaunchDaemon installed by `scripts/mac/build-base.sh` waits for it,
//! copies the binary to the local disk and starts it with the environment.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use warren_core::{Error, Result};

/// Last path component of the shared host folder; macOS names the mount
/// after it, so every instance sees the same guest path.
pub const SHARE_DIR_NAME: &str = "share";
pub const GATEWAY_FILE: &str = "warren-gateway";
pub const ENV_FILE: &str = "warren.env";
/// Where macOS guests automount the folder.
pub const GUEST_SHARE_PATH: &str = "/Volumes/My Shared Files/share";

/// `<mac_dir>/<instance id>`: the instance's host-side state.
pub fn instance_dir(mac_dir: &Path, instance_id: &str) -> PathBuf {
    mac_dir.join(instance_id)
}

pub fn share_dir(mac_dir: &Path, instance_id: &str) -> PathBuf {
    instance_dir(mac_dir, instance_id).join(SHARE_DIR_NAME)
}

/// Single-quote `v` for a POSIX shell: `'` becomes `'\''`.
pub fn shell_quote(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('\'');
    for c in v.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `export KEY='value'` lines, sorted by key, safe to source with `.` in
/// `sh`, `bash` or `zsh`. Keys that are not shell identifiers are skipped.
pub fn render_env(env: &BTreeMap<String, String>) -> String {
    let mut out = String::from("# written by warrend; sourced by warren-gateway-launcher.sh\n");
    for (k, v) in env {
        if !valid_key(k) {
            tracing::warn!(key = %k, "skipping an environment key that is not a shell identifier");
            continue;
        }
        out.push_str("export ");
        out.push_str(k);
        out.push('=');
        out.push_str(&shell_quote(v));
        out.push('\n');
    }
    out
}

fn io(what: &str, p: &Path, e: std::io::Error) -> Error {
    Error::internal(format!("{what} {}: {e}", p.display()))
}

/// Create the share folder, place the gateway binary (copied only when the
/// bytes differ, so a daemon upgrade propagates on the next start) and write
/// the environment file atomically.
pub fn write_share(dir: &Path, gateway_bin: &Path, env: &BTreeMap<String, String>) -> Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| io("mkdir", parent, e))?;
        restrict(parent)?;
    }
    fs::create_dir_all(dir).map_err(|e| io("mkdir", dir, e))?;
    let target = dir.join(GATEWAY_FILE);
    let fresh = fs::read(gateway_bin).map_err(|e| io("read gateway binary", gateway_bin, e))?;
    let current = fs::read(&target).ok();
    if current.as_deref() != Some(fresh.as_slice()) {
        let tmp = dir.join(".warren-gateway.tmp");
        fs::write(&tmp, &fresh).map_err(|e| io("write", &tmp, e))?;
        set_mode(&tmp, 0o755)?;
        fs::rename(&tmp, &target).map_err(|e| io("rename", &target, e))?;
    }
    let env_path = dir.join(ENV_FILE);
    let tmp = dir.join(".warren.env.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| io("create", &tmp, e))?;
        f.write_all(render_env(env).as_bytes())
            .map_err(|e| io("write", &tmp, e))?;
        f.sync_all().map_err(|e| io("sync", &tmp, e))?;
    }
    set_mode(&tmp, 0o644)?;
    fs::rename(&tmp, &env_path).map_err(|e| io("rename", &env_path, e))?;
    Ok(())
}

/// Remove the instance's host-side folder; absent is fine.
pub fn remove_instance_dir(mac_dir: &Path, instance_id: &str) -> Result<()> {
    let dir = instance_dir(mac_dir, instance_id);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("remove", &dir, e)),
    }
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(|e| io("chmod", p, e))
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// The instance directory holds the gateway token: owner-only.
pub fn restrict(p: &Path) -> Result<()> {
    set_mode(p, 0o700)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_rendering() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        let env = BTreeMap::from([
            ("WARREN_INSTANCE_ID".to_string(), "ab12".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-x'y\n$HOME".to_string()),
            ("bad-key".to_string(), "x".to_string()),
        ]);
        let text = render_env(&env);
        assert!(text.contains("export WARREN_INSTANCE_ID='ab12'\n"));
        assert!(text.contains("export OPENAI_API_KEY='sk-x'\\''y\n$HOME'\n"));
        assert!(!text.contains("bad-key"));
        // Sourced by a real shell, the values come back intact.
        let dir = std::env::temp_dir().join(format!("warren-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("e.sh");
        std::fs::write(&f, &text).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                ". {} && printf '%s' \"$OPENAI_API_KEY\"",
                f.display()
            ))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "sk-x'y\n$HOME");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn share_is_written_and_binary_refreshed_only_on_change() {
        let root = std::env::temp_dir().join(format!("warren-mac-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("gw");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&bin, b"#!/bin/sh\necho one\n").unwrap();
        let dir = share_dir(&root.join("mac"), "ab12cd34ef");
        let env = BTreeMap::from([("A".to_string(), "1".to_string())]);
        write_share(&dir, &bin, &env).unwrap();
        assert_eq!(
            std::fs::read(dir.join(GATEWAY_FILE)).unwrap(),
            b"#!/bin/sh\necho one\n"
        );
        assert!(
            std::fs::read_to_string(dir.join(ENV_FILE))
                .unwrap()
                .contains("export A='1'")
        );
        let first = std::fs::metadata(dir.join(GATEWAY_FILE))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_share(&dir, &bin, &env).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join(GATEWAY_FILE))
                .unwrap()
                .modified()
                .unwrap(),
            first,
            "unchanged binary is not rewritten"
        );
        std::fs::write(&bin, b"#!/bin/sh\necho two\n").unwrap();
        write_share(&dir, &bin, &env).unwrap();
        assert_eq!(
            std::fs::read(dir.join(GATEWAY_FILE)).unwrap(),
            b"#!/bin/sh\necho two\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.join(GATEWAY_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::metadata(dir.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        remove_instance_dir(&root.join("mac"), "ab12cd34ef").unwrap();
        assert!(!dir.exists());
        remove_instance_dir(&root.join("mac"), "ab12cd34ef").unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
