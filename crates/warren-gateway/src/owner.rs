//! The home owner: the user whose files the persistent home holds and the
//! user the agent's commands run as.
//!
//! Inside the built-in images the gateway runs as root so it can supervise
//! `ttyd` as the unprivileged user and honour the `user` field of exec
//! requests. The persistent home (`WARREN_HOME`) is bind-mounted from the
//! host, where it belongs to an ordinary user. Everything the gateway creates
//! inside the home must therefore be handed to that user, and agent commands
//! must run as that user; otherwise files end up root-owned on the host (so a
//! daemon running as an ordinary user cannot delete the instance's storage)
//! and the agent works as root inside its own machine.
//!
//! At startup, when the process is root, the home directory is stat'ed and
//! its uid/gid recorded; the user name is looked up in `/etc/passwd`, then
//! through `getpwuid` (Open Directory on macOS), and falls back to the
//! numeric uid. When the gateway is not root the owner is
//! the current user and every operation here is a no-op.

use std::{
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::exec::{Identity, effective_gid, effective_uid, lookup_uid, parse_passwd};

/// The user who owns the persistent home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeOwner {
    /// User name from the user database (`/etc/passwd`, else `getpwuid`),
    /// or the numeric uid when unknown.
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// `name` is known to the user database, so `runuser`/`su` can resolve it.
    pub named: bool,
    /// Effective uid of the gateway process.
    pub process_uid: u32,
}

impl HomeOwner {
    /// The owner of `home` when the gateway is root, else the current user.
    pub fn detect(home: &Path) -> HomeOwner {
        let process_uid = effective_uid();
        if process_uid != 0 {
            return HomeOwner::current();
        }
        match std::fs::metadata(home) {
            Ok(m) => {
                let (name, named) = owner_name(m.uid());
                HomeOwner {
                    name,
                    uid: m.uid(),
                    gid: m.gid(),
                    named,
                    process_uid,
                }
            }
            Err(e) => {
                tracing::warn!(
                    home = %home.display(),
                    error = %e,
                    "cannot stat the home directory; files stay owned by root"
                );
                HomeOwner::current()
            }
        }
    }

    /// The gateway's own user: nothing is chowned and nothing switches user.
    pub fn current() -> HomeOwner {
        let uid = effective_uid();
        let (name, named) = owner_name(uid);
        HomeOwner {
            name,
            uid,
            gid: effective_gid(),
            named,
            process_uid: uid,
        }
    }

    /// True when the gateway is root and the home belongs to someone else:
    /// created files are chowned and commands switch user.
    pub fn switches(&self) -> bool {
        self.process_uid == 0 && self.uid != 0
    }

    /// True when `user` names this owner and a switch applies (the numeric
    /// uid counts when the owner is unknown to the user database).
    pub fn is_named(&self, user: &str) -> bool {
        self.switches() && self.name == user.trim()
    }

    /// The identity child processes switch to; `None` when they run as the
    /// gateway itself.
    pub fn identity(&self) -> Option<Identity> {
        self.switches().then(|| Identity {
            name: self.name.clone(),
            uid: self.uid,
            gid: self.gid,
            named: self.named,
        })
    }

    /// Hand `path` to the owner. Symlinks are left alone (the gateway never
    /// creates any) and nothing happens when no switch applies. Returns
    /// whether ownership was changed.
    pub fn chown(&self, path: &Path) -> io::Result<bool> {
        if !self.switches() {
            return Ok(false);
        }
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Ok(false);
        }
        std::os::unix::fs::chown(path, Some(self.uid), Some(self.gid))?;
        Ok(true)
    }

    /// [`chown`](Self::chown) that logs a failure instead of returning it:
    /// the file exists and was written, so a wrong owner must not fail the
    /// request that created it.
    pub fn own(&self, path: &Path) {
        if let Err(e) = self.chown(path) {
            tracing::warn!(
                path = %path.display(),
                uid = self.uid,
                gid = self.gid,
                error = %e,
                "cannot chown to the home owner"
            );
        }
    }

    /// Create `dir` and its missing ancestors, handing every directory this
    /// call creates to the owner. Existing directories are left untouched.
    /// Returns the created directories, top-most first.
    pub fn create_dir_all_owned(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut created = Vec::new();
        for d in missing_dirs(dir)? {
            match std::fs::create_dir(&d) {
                Ok(()) => {
                    self.own(&d);
                    created.push(d);
                }
                // Created meanwhile by another request or by the agent's own
                // shell: not ours, so not ours to chown.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Ok(created)
    }
}

/// `dir` and the ancestors of it that do not exist yet, top-most first. A
/// file in the way is left for the eventual `mkdir` to report.
pub fn missing_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if p.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(p) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(p.to_path_buf());
                cur = p.parent();
            }
            Err(e) => return Err(e),
        }
    }
    missing.reverse();
    Ok(missing)
}

/// The name listed for `uid` in `passwd` (`/etc/passwd` content).
pub fn name_for_uid(passwd: &str, uid: u32) -> Option<String> {
    parse_passwd(passwd)
        .into_iter()
        .find(|e| e.uid == uid)
        .map(|e| e.name)
}

/// `(name, true)` from the user database (`/etc/passwd`, else `getpwuid`),
/// or `(uid as a string, false)` when the uid is unknown.
pub fn owner_name(uid: u32) -> (String, bool) {
    match lookup_uid(uid) {
        Some(e) => (e.name, true),
        None => (uid.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::is_root;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
                          agent:x:1000:1000::/home/agent:/bin/bash\n";

    fn temp_dir() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("warren-gw-owner-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pretend_root_owner() -> HomeOwner {
        HomeOwner {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
            process_uid: 0,
        }
    }

    #[test]
    fn passwd_lookup_by_uid() {
        assert_eq!(name_for_uid(PASSWD, 1000).as_deref(), Some("agent"));
        assert_eq!(name_for_uid(PASSWD, 0).as_deref(), Some("root"));
        assert_eq!(name_for_uid(PASSWD, 4242), None);
        assert_eq!(name_for_uid("", 0), None);
        // The real passwd file: root is always listed, an absurd uid never is.
        if std::fs::metadata("/etc/passwd").is_ok() {
            assert_eq!(owner_name(0), ("root".to_string(), true));
        }
        assert_eq!(owner_name(4_000_000_000), ("4000000000".to_string(), false));
    }

    #[test]
    fn current_owner_is_a_no_op() {
        let me = HomeOwner::current();
        assert_eq!(me.uid, effective_uid());
        assert_eq!(me.process_uid, me.uid);
        assert!(!me.name.is_empty());
        if me.named {
            assert_eq!(owner_name(me.uid).0, me.name);
        } else {
            assert_eq!(me.name, me.uid.to_string());
        }
        let dir = temp_dir();
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        if !is_root() {
            assert!(!me.switches());
            assert!(me.identity().is_none());
            assert!(!me.is_named(&me.name));
            assert_eq!(HomeOwner::detect(&dir), me);
            assert!(!me.chown(&file).unwrap());
            me.own(&file);
            assert!(!me.chown(&dir.join("missing")).unwrap());
        }
        // Whoever we are, the home's owner is reported with its real ids.
        let meta = std::fs::metadata(&dir).unwrap();
        let detected = HomeOwner::detect(&dir);
        if is_root() {
            assert_eq!((detected.uid, detected.gid), (meta.uid(), meta.gid()));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn switching_rules() {
        let agent = pretend_root_owner();
        assert!(agent.switches());
        assert!(agent.is_named("agent"));
        assert!(agent.is_named(" agent "));
        assert!(!agent.is_named("root"));
        assert_eq!(
            agent.identity(),
            Some(Identity {
                name: "agent".into(),
                uid: 1000,
                gid: 1000,
                named: true,
            })
        );
        // A root-owned home under a root gateway: nothing to switch to.
        let root_home = HomeOwner {
            name: "root".into(),
            uid: 0,
            gid: 0,
            named: true,
            process_uid: 0,
        };
        assert!(!root_home.switches());
        assert!(root_home.identity().is_none());
        assert!(!root_home.is_named("root"));
        // An unprivileged gateway never switches, whoever owns the home.
        let unprivileged = HomeOwner {
            name: "1000".into(),
            uid: 1000,
            gid: 1000,
            named: false,
            process_uid: 1000,
        };
        assert!(!unprivileged.switches());
        assert!(unprivileged.identity().is_none());
        // A numeric owner (no passwd entry) still switches, by uid.
        let numeric = HomeOwner {
            name: "1234".into(),
            uid: 1234,
            gid: 1234,
            named: false,
            process_uid: 0,
        };
        assert!(numeric.switches());
        assert!(numeric.is_named("1234"));
        assert!(!numeric.identity().unwrap().named);
        // A symlink is never chowned, even by a switching owner; a missing
        // path is an error for `chown` and only a log line for `own`.
        let dir = temp_dir();
        let link = dir.join("link");
        std::os::unix::fs::symlink("/nonexistent/target", &link).unwrap();
        assert!(!agent.chown(&link).unwrap());
        assert!(agent.chown(&dir.join("missing")).is_err());
        agent.own(&dir.join("missing"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tracked_directory_creation() {
        let dir = temp_dir();
        let me = HomeOwner::current();
        let target = dir.join("a/b/c");
        let expected = vec![dir.join("a"), dir.join("a/b"), dir.join("a/b/c")];
        assert_eq!(missing_dirs(&target).unwrap(), expected);
        assert!(missing_dirs(&dir).unwrap().is_empty());
        assert_eq!(me.create_dir_all_owned(&target).unwrap(), expected);
        assert!(target.is_dir());
        // Nothing new the second time; an intermediate directory is reused.
        assert!(me.create_dir_all_owned(&target).unwrap().is_empty());
        assert_eq!(
            me.create_dir_all_owned(&dir.join("a/x")).unwrap(),
            vec![dir.join("a/x")]
        );
        // A file in the way is an error, not a silent success.
        std::fs::write(dir.join("a/file"), b"x").unwrap();
        assert!(me.create_dir_all_owned(&dir.join("a/file/y")).is_err());
        assert!(missing_dirs(Path::new("")).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
