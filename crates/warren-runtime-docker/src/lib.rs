//! Default-type Warren instances as Docker containers, preferring gVisor (`runsc`).
//!
//! [`DockerRuntime`] implements [`warren_core::Runtime`] on top of `bollard`:
//!
//! - `ready` pings the daemon, picks the OCI runtime (`runsc` when the daemon offers
//!   it, else `runc`, as configured by `Config::runtime`) and makes sure the two
//!   bridge networks exist: [`NETWORK_PUBLIC`] with egress and [`NETWORK_INTERNAL`]
//!   (`internal: true`) for deny-all instances. Both have inter-container
//!   communication off, and as root `ready` installs the iptables egress rules
//!   (metadata service, private ranges, the host); see the `netpolicy` module.
//! - Every instance is one container named `warren-<id>`. Its persistent disk is the
//!   host directory `<data_dir>/instances/<id>/home`, bind-mounted at the template's
//!   `mount_path`. The container carries the labels `warren.instance` and
//!   `warren.workspace`.
//! - Images that do not ship the gateway get it injected: for templates with
//!   [`warren_core::TemplateScope::Workspace`] (registered from arbitrary images)
//!   the static gateway named by `Config::gateway_bin` is bind-mounted read-only at
//!   [`warren_core::Config::INJECTED_GATEWAY_PATH`] and becomes the entrypoint in
//!   wrap mode, `--serve --wrap -- <image entrypoint> <image cmd>`, serving the
//!   Warren API while supervising the image's own command; the image's user and
//!   working directory are kept and the container carries [`LABEL_INJECTED`]. System
//!   templates already contain the gateway and are untouched. Without a configured
//!   binary a workspace image starts unchanged (with a warning) and a failed `start`
//!   names `--gateway-bin` as the fix.
//! - `exec` uses Docker exec. Without an explicit user the command runs as the
//!   `uid:gid` that owns the instance's home on the host (uid 1000 for the
//!   built-in images) with `HOME` set to the mount path; `root` stays allowed.
//! - `logs`, `stats` and `resize` use the Docker API; `clone_storage`,
//!   `backup_storage` and `restore_storage` are file-system operations on the
//!   storage directory (tar.gz for backups).
//! - With `Config::docker_checkpoints` and an engine in experimental mode, `sleep`
//!   checkpoints the container through the engine's checkpoint API (gVisor's
//!   `runsc checkpoint`) and `start` restores it, so processes keep running
//!   with their pids across a sleep; see the `checkpoint` module. Otherwise
//!   sleep stops the container and keeps the disk.
//! - The gateway inside an instance runs as root, so the home can hold files a
//!   non-root daemon may not touch. Storage operations that hit
//!   `PermissionDenied` fall back to a short-lived root helper container
//!   ([`HELPER_IMAGE`], instances directory mounted at [`HELPER_MOUNT`]) that
//!   only ever works on validated paths under the instances directory.
//!
//! Module map: `runtime` (connection, readiness, the trait impl), `container`
//! (create, start, stop, destroy, resize, recreate), `checkpoint` (sleep with
//! memory through the engine's checkpoint API), `inject` (gateway injection:
//! the decision, the wrapped entrypoint, the start-failure hint), `exec`,
//! `observe` (logs and stats), `image` (pulls), `storage` (disk usage, copies,
//! backups), `helper` (the root helper container) and `error` (Docker error
//! mapping).

use std::time::Duration;

mod checkpoint;
mod container;
mod error;
mod exec;
mod helper;
mod image;
mod inject;
mod netpolicy;
mod observe;
mod runtime;
mod storage;

#[cfg(test)]
mod tests;

pub use checkpoint::CHECKPOINT_ID;
pub use netpolicy::{LINK_LOCAL, OPT_ENABLE_ICC, PIDS_LIMIT, PRIVATE_RANGES};
pub use runtime::DockerRuntime;

/// Bridge network with egress, used for [`warren_core::NetworkMode::AllowAll`].
pub const NETWORK_PUBLIC: &str = "warren-net";
/// Internal bridge without egress, used for [`warren_core::NetworkMode::DenyAll`].
pub const NETWORK_INTERNAL: &str = "warren-internal";
/// Every container is named `warren-<instance id>`.
pub const CONTAINER_PREFIX: &str = "warren-";
/// Directory inside the storage path that is bind-mounted into the container.
pub const HOME_DIR: &str = "home";
/// Container label carrying the instance id.
pub const LABEL_INSTANCE: &str = "warren.instance";
/// Container label carrying the workspace id.
pub const LABEL_WORKSPACE: &str = "warren.workspace";
/// Container label carrying the scope of the template the container was created
/// from (`system` or `workspace`).
pub const LABEL_SCOPE: &str = "warren.scope";
/// Container label set to `true` when the gateway was injected into the image.
pub const LABEL_INJECTED: &str = "warren.injected";
/// uid of the `agent` user inside the built-in images; the home directory is handed to it.
pub const AGENT_UID: u32 = 1000;
/// gid of the `agent` user inside the built-in images.
pub const AGENT_GID: u32 = 1000;
/// How long `start` waits for the gateway port to accept connections.
pub const START_TIMEOUT: Duration = Duration::from_secs(90);
/// Interval between connection attempts while waiting for the gateway port.
pub const START_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Upper bound on the lines `logs` returns; `tail: 0` means this many.
pub const MAX_LOG_LINES: u32 = 2000;
/// Upper bound on the bytes `logs` returns.
pub const MAX_LOG_BYTES: usize = 8 << 20;
/// Image of the root storage helper (see the `helper` module).
pub const HELPER_IMAGE: &str = "alpine:3.20";
/// Where the helper container sees the instances directory.
pub const HELPER_MOUNT: &str = "/w";
/// Helper containers are named `warren-helper-<job>-<random>`.
pub const HELPER_PREFIX: &str = "warren-helper-";
/// Container label on helper containers carrying the instance id they work for.
pub const LABEL_HELPER: &str = "warren.helper";
/// Upper bound on one helper job; deleting or copying a large home takes a while.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(600);
