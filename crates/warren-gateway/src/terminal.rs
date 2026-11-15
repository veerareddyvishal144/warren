//! Supervises the browser terminal: `ttyd -p 7681 -W -t disableLeaveAlert=true bash -l`
//! with `HOME` set to the persistent home, running as the home owner (the
//! agent user in the built-in images) when the gateway is root. It is
//! restarted with exponential backoff when it exits; when `ttyd` is not
//! installed a warning is logged and nothing else happens.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::{process::Command, task::JoinHandle};

use crate::{
    exec::{Identity, hide_platform_env, is_root, which},
    owner::HomeOwner,
};

/// First restart delay.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// Restart delays never exceed this.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run at least this long resets the backoff.
pub const STABLE_RUN: Duration = Duration::from_secs(60);

/// Start the supervisor as a background task.
pub fn spawn_supervisor(home: PathBuf, port: u16, owner: HomeOwner) -> JoinHandle<()> {
    tokio::spawn(supervise(home, port, owner))
}

/// The supervisor loop; returns only when `ttyd` cannot be started at all.
pub async fn supervise(home: PathBuf, port: u16, owner: HomeOwner) {
    let Some(ttyd) = which("ttyd") else {
        tracing::warn!("ttyd not found; the browser terminal is disabled");
        return;
    };
    let identity = terminal_identity(&owner);
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        match spawn_ttyd(&ttyd, &home, port, identity.as_ref()) {
            Ok(mut child) => {
                tracing::info!(
                    port,
                    pid = child.id().unwrap_or(0),
                    user = identity
                        .as_ref()
                        .map(|i| i.name.as_str())
                        .unwrap_or("current"),
                    "terminal started"
                );
                match child.wait().await {
                    Ok(status) => tracing::warn!(%status, "ttyd exited; restarting"),
                    Err(e) => tracing::warn!(error = %e, "waiting for ttyd failed; restarting"),
                }
                if started.elapsed() >= STABLE_RUN {
                    backoff = MIN_BACKOFF;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(error = %e, "ttyd disappeared; the browser terminal is disabled");
                return;
            }
            Err(e) => tracing::warn!(error = %e, "cannot start ttyd; retrying"),
        }
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

fn spawn_ttyd(
    ttyd: &Path,
    home: &Path,
    port: u16,
    identity: Option<&Identity>,
) -> std::io::Result<tokio::process::Child> {
    ttyd_command(ttyd, home, port, identity).spawn()
}

/// The `ttyd` command line: the gateway's environment minus the platform
/// variables (the shell it serves is the user's), `HOME` the persistent
/// home, dropped to `identity` when one applies.
pub fn ttyd_command(ttyd: &Path, home: &Path, port: u16, identity: Option<&Identity>) -> Command {
    let mut cmd = Command::new(ttyd);
    cmd.args([
        "-p",
        &port.to_string(),
        "-W",
        "-t",
        "disableLeaveAlert=true",
        "bash",
        "-l",
    ])
    .current_dir(home)
    .env("HOME", home)
    .env("SHELL", "/bin/bash")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    hide_platform_env(&mut cmd);
    if let Some(id) = identity {
        cmd.uid(id.uid)
            .gid(id.gid)
            .env("USER", &id.name)
            .env("LOGNAME", &id.name);
    }
    cmd
}

/// Doubling backoff capped at [`MAX_BACKOFF`].
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// The identity the terminal drops to: the home owner when the gateway is
/// root and the home belongs to someone else; otherwise `None` (run as
/// ourselves).
pub fn terminal_identity(owner: &HomeOwner) -> Option<Identity> {
    let identity = owner.identity();
    if identity.is_none() && is_root() {
        tracing::warn!("the home is owned by root; the terminal runs as root");
    }
    identity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = MIN_BACKOFF;
        let mut seen = vec![b];
        for _ in 0..8 {
            b = next_backoff(b);
            seen.push(b);
        }
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[4], Duration::from_secs(16));
        assert_eq!(*seen.last().unwrap(), MAX_BACKOFF);
        assert!(seen.windows(2).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn identity_follows_the_home_owner() {
        if !is_root() {
            assert_eq!(terminal_identity(&HomeOwner::current()), None);
        }
        let owner = HomeOwner {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
            process_uid: 0,
        };
        assert_eq!(
            terminal_identity(&owner),
            Some(Identity {
                name: "agent".into(),
                uid: 1000,
                gid: 1000,
                named: true,
            })
        );
        let root_home = HomeOwner {
            name: "root".into(),
            uid: 0,
            gid: 0,
            named: true,
            process_uid: 0,
        };
        assert_eq!(terminal_identity(&root_home), None);
    }

    #[test]
    fn ttyd_command_hides_platform_variables() {
        let agent = Identity {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
        };
        // Other tests in this binary set and unset WARREN_* variables, so
        // only variables present both before and after building the command
        // are required to have been removed.
        let before = crate::exec::hidden_vars_now();
        let cmd = ttyd_command(
            Path::new("/usr/bin/ttyd"),
            Path::new("/home/agent"),
            7681,
            Some(&agent),
        );
        let after = crate::exec::hidden_vars_now();
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "/usr/bin/ttyd");
        assert_eq!(std_cmd.get_current_dir(), Some(Path::new("/home/agent")));
        let mut removed = Vec::new();
        let mut set = Vec::new();
        for (k, v) in std_cmd.get_envs() {
            let k = k.to_string_lossy().into_owned();
            match v {
                None => removed.push(k),
                Some(v) => set.push((k, v.to_string_lossy().into_owned())),
            }
        }
        assert!(
            removed.iter().all(|k| crate::wrap::is_hidden_var(k)),
            "{removed:?}"
        );
        for k in before.iter().filter(|k| after.contains(k)) {
            assert!(removed.contains(k), "{k} not hidden: {removed:?}");
        }
        assert!(set.contains(&("HOME".to_string(), "/home/agent".to_string())));
        assert!(set.contains(&("USER".to_string(), "agent".to_string())));
        assert!(set.contains(&("LOGNAME".to_string(), "agent".to_string())));
        assert!(!removed.contains(&"HOME".to_string()));
    }

    #[tokio::test]
    async fn missing_ttyd_returns_quickly() {
        // With an empty PATH and no system ttyd the supervisor must give up at once.
        if which("ttyd").is_none() {
            let home = std::env::temp_dir();
            tokio::time::timeout(
                Duration::from_secs(5),
                supervise(home, 7681, HomeOwner::current()),
            )
            .await
            .unwrap();
        }
    }
}
