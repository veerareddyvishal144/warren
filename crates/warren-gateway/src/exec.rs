//! `POST /v1/exec` and the shared process helpers used by the drivers.
//!
//! Commands run through `/bin/sh -lc` in a fresh process group so a timeout
//! can kill the whole tree. Output is capped per stream at
//! [`MAX_EXEC_OUTPUT_BYTES`] and `truncated` is reported.
//!
//! User switching: when the gateway is root, commands run as the
//! [`HomeOwner`] unless the request names another user. The switch goes
//! through `runuser` (util-linux; `--session-command` keeps the command in
//! our process group so a timeout still kills the whole tree), else `su`,
//! else the child changes its ids itself (when the owner is known to no user
//! database or neither helper is installed).
//!
//! Users are looked up in `/etc/passwd` first and then through the system's
//! `getpwnam_r`/`getpwuid_r` (NSS on Linux, Open Directory on macOS, where
//! ordinary accounts never appear in `/etc/passwd`).

use std::{
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use axum::{Json, extract::State};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::{Pid, Uid, User},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};
use warren_core::{
    Error, Result,
    types::{DEFAULT_EXEC_TIMEOUT_SEC, ExecRequest, ExecResult, MAX_EXEC_OUTPUT_BYTES},
};

use crate::{
    extract::{JsonBody, parse_json},
    owner::HomeOwner,
    server::SharedState,
    wrap::is_hidden_var,
};

/// Fields accepted in an `ExecRequest` body.
const EXEC_FIELDS: &[&str] = &["command", "user", "timeout_seconds", "workdir", "env"];
/// Longest exec a client may ask for (24 hours).
const MAX_EXEC_TIMEOUT_SEC: u32 = 86_400;

/// `POST /v1/exec`: run a shell command inside the instance. The instance
/// token is enforced by the router's middleware (see `server::require_token`).
pub async fn exec_handler(
    State(state): State<SharedState>,
    JsonBody(body): JsonBody,
) -> Result<Json<ExecResult>> {
    let req: ExecRequest = parse_json(&body, EXEC_FIELDS)?;
    let home = state.home.clone();
    Ok(Json(run_exec(&home, &state.owner, &req).await?))
}

/// Run an [`ExecRequest`] with `home` as the default working directory.
///
/// An empty `user` means the owner of the persistent home: the agent user in
/// the built-in images, where the gateway is root; the gateway's own user
/// otherwise. `HOME` is then the persistent home. Any user the system knows
/// (see [`lookup_passwd`]), `"root"` included, may be asked for explicitly
/// when the gateway is root; such a user gets its passwd home as `HOME`.
pub async fn run_exec(home: &Path, owner: &HomeOwner, req: &ExecRequest) -> Result<ExecResult> {
    if req.command.trim().is_empty() {
        return Err(Error::invalid("command must not be empty"));
    }
    if req.timeout_seconds > MAX_EXEC_TIMEOUT_SEC {
        return Err(Error::invalid(format!(
            "timeout_seconds must be at most {MAX_EXEC_TIMEOUT_SEC}"
        )));
    }
    for (k, v) in &req.env {
        if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
            return Err(Error::invalid(format!(
                "invalid environment variable name {k:?}"
            )));
        }
    }
    let workdir = if req.workdir.is_empty() {
        home.to_path_buf()
    } else {
        let w = home.join(&req.workdir);
        if !w.is_dir() {
            return Err(Error::invalid(format!(
                "workdir {} does not exist",
                req.workdir
            )));
        }
        w
    };
    let timeout = Duration::from_secs(if req.timeout_seconds == 0 {
        DEFAULT_EXEC_TIMEOUT_SEC
    } else {
        req.timeout_seconds
    } as u64);

    let user = req.user.trim();
    let (mut cmd, home_env) = if user.is_empty() || owner.is_named(user) {
        (owner_shell(owner, &req.command), home.to_path_buf())
    } else {
        (
            shell_command(user, &req.command)?,
            user_home(user).unwrap_or_else(|| home.to_path_buf()),
        )
    };
    cmd.current_dir(&workdir);
    cmd.env("HOME", home_env);
    cmd.envs(&req.env);
    let started = Instant::now();
    let out = run_capped(cmd, timeout, MAX_EXEC_OUTPUT_BYTES).await?;
    Ok(ExecResult {
        exit_code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
        duration_ms: started.elapsed().as_millis() as i64,
        truncated: out.truncated,
    })
}

/// Captured output of a finished process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// Spawn `cmd` with piped stdio in its own process group, wait for it within
/// `timeout` and capture at most `cap` bytes per stream. On timeout the whole
/// process group is killed and `Error::timeout` is returned.
pub async fn run_capped(mut cmd: Command, timeout: Duration, cap: usize) -> Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().map_err(spawn_error)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let body = async {
        let (out, err) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
        let status = child.wait().await;
        (out, err, status)
    };
    match tokio::time::timeout(timeout, body).await {
        Ok(((out, out_trunc), (err, err_trunc), status)) => {
            let status = status?;
            Ok(Captured {
                exit_code: exit_code(status),
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                truncated: out_trunc || err_trunc,
            })
        }
        Err(_) => {
            kill_tree(&mut child).await;
            Err(Error::timeout(format!(
                "command did not finish within {} seconds",
                timeout.as_secs()
            )))
        }
    }
}

/// Run `command` with `/bin/sh -lc` in `cwd` as the home owner, `HOME` set to
/// `cwd`, stderr merged into stdout.
pub async fn run_shell_combined(
    cwd: &Path,
    owner: &HomeOwner,
    command: &str,
    timeout: Duration,
    cap: usize,
) -> Result<Captured> {
    let script = format!("exec 2>&1\n{command}");
    let mut cmd = owner_shell(owner, &script);
    cmd.current_dir(cwd).env("HOME", cwd);
    run_capped(cmd, timeout, cap).await
}

/// Kill the child's process group, then the child itself, and reap it.
pub async fn kill_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    let _ = child.kill().await;
}

/// Exit code of a finished process; `128 + signal` when it was killed.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Read a stream to EOF, keeping at most `cap` bytes. Returns the bytes and
/// whether anything was dropped.
pub async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let Some(mut reader) = reader else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                let take = n.min(room);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    truncated = true;
                }
            }
        }
    }
    (buf, truncated)
}

fn spawn_error(e: std::io::Error) -> Error {
    match e.kind() {
        std::io::ErrorKind::NotFound => Error::invalid(format!("cannot start command: {e}")),
        std::io::ErrorKind::PermissionDenied => {
            Error::forbidden(format!("cannot start command: {e}"))
        }
        _ => Error::internal(format!("cannot start command: {e}")),
    }
}

// ----- environment -----

/// Remove the platform variables (every `WARREN_*` except `WARREN_HOME`, see
/// [`is_hidden_var`]) from the environment `cmd` will start with, so no
/// process run on behalf of a user sees the instance token or the gateway's
/// own settings. Applies to the gateway's current environment; variables the
/// caller sets on `cmd` afterwards are kept.
pub fn hide_platform_env(cmd: &mut Command) {
    hide_platform_env_from(cmd, std::env::vars_os().map(|(k, _)| k));
}

/// Names of the hidden platform variables currently in the process
/// environment (for tests and diagnostics).
pub fn hidden_vars_now() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| is_hidden_var(k))
        .collect()
}

/// [`hide_platform_env`] for an explicit list of variable names.
pub fn hide_platform_env_from<I>(cmd: &mut Command, keys: I)
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    for key in keys {
        if key.to_str().is_some_and(is_hidden_var) {
            cmd.env_remove(key);
        }
    }
}

// ----- user switching -----

/// A user a child process is switched to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// `name` is known to the user database (`/etc/passwd` or the system's
    /// `getpwnam`), so `runuser`/`su` can resolve it.
    pub named: bool,
}

impl Identity {
    /// The identity of a passwd entry.
    pub fn from_passwd(entry: &PasswdEntry) -> Identity {
        Identity {
            name: entry.name.clone(),
            uid: entry.uid,
            gid: entry.gid,
            named: true,
        }
    }
}

/// `/bin/sh -lc <script>` running as the home owner; a plain shell when no
/// switch applies. The caller sets `HOME` and the working directory.
pub fn owner_shell(owner: &HomeOwner, script: &str) -> Command {
    match owner.identity() {
        Some(id) => switched(&id, shell_argv(script)),
        None => plain(shell_argv(script)),
    }
}

/// `argv` running as the home owner; the plain program when no switch applies.
pub fn owner_command(owner: &HomeOwner, argv: &[String]) -> Result<Command> {
    if argv.is_empty() {
        return Err(Error::invalid("command must not be empty"));
    }
    Ok(match owner.identity() {
        Some(id) => switched(&id, argv.to_vec()),
        None => plain(argv.to_vec()),
    })
}

/// `/bin/sh -lc <script>` as the named `user`: a plain shell for the current
/// user (or an empty name); otherwise the gateway must be root and the user
/// must exist.
fn shell_command(user: &str, script: &str) -> Result<Command> {
    let user = user.trim();
    if user.is_empty() || is_current_user(user) {
        return Ok(plain(shell_argv(script)));
    }
    if !is_root() {
        return Err(Error::not_supported(format!(
            "cannot run as {user}: the gateway is not running as root"
        )));
    }
    let entry =
        lookup_passwd(user).ok_or_else(|| Error::invalid(format!("unknown user {user:?}")))?;
    Ok(switched(&Identity::from_passwd(&entry), shell_argv(script)))
}

fn shell_argv(script: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-lc".to_string(), script.to_string()]
}

/// `argv` as the gateway's own user, without the platform variables.
fn plain(argv: Vec<String>) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    hide_platform_env(&mut c);
    c
}

/// `argv` running as `id`. `runuser --session-command` keeps the command in
/// our process group so a timeout kills the whole tree; `su -c` (images
/// without util-linux) starts a new session, so only its direct child is
/// reached. Without a passwd name, or without either helper, the child
/// changes its ids itself; supplementary groups are then dropped instead of
/// initialised from `/etc/group`.
///
/// Both helpers run with `-m`, which hands the user exactly the environment
/// we give `runuser`/`su` itself, so the platform variables are removed from
/// that environment and nothing reintroduces them on the other side of the
/// switch.
fn switched(id: &Identity, argv: Vec<String>) -> Command {
    let mut cmd = match (id.named, which("runuser"), which("su")) {
        (true, Some(runuser), _) => {
            let mut c = Command::new(runuser);
            c.args([
                "-m",
                "-s",
                "/bin/sh",
                "--session-command",
                &shell_join(&argv),
                &id.name,
            ]);
            c
        }
        (true, None, Some(su)) => {
            let mut c = Command::new(su);
            c.args(["-m", "-s", "/bin/sh", &id.name, "-c", &shell_join(&argv)]);
            c
        }
        _ => {
            tracing::debug!(user = %id.name, uid = id.uid, "switching ids directly in the child");
            let mut c = plain(argv);
            c.uid(id.uid).gid(id.gid);
            c
        }
    };
    hide_platform_env(&mut cmd);
    cmd.env("USER", &id.name).env("LOGNAME", &id.name);
    cmd
}

/// `exec` of `argv` as one POSIX shell command line, every word single-quoted.
pub fn shell_join(argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|w| format!("'{}'", w.replace('\'', "'\\''")))
        .collect();
    format!("exec {}", words.join(" "))
}

fn user_home(user: &str) -> Option<PathBuf> {
    let user = user.trim();
    if user.is_empty() || is_current_user(user) {
        return None;
    }
    lookup_passwd(user).map(|p| p.home)
}

/// Effective uid of this process.
pub fn effective_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// Effective gid of this process.
pub fn effective_gid() -> u32 {
    nix::unistd::getegid().as_raw()
}

/// True when the effective uid is 0.
pub fn is_root() -> bool {
    effective_uid() == 0
}

/// One user record: an `/etc/passwd` line or the equivalent answer of the
/// system's user database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: String,
}

/// Parse `/etc/passwd` content.
pub fn parse_passwd(content: &str) -> Vec<PasswdEntry> {
    content
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 || f[0].is_empty() {
                return None;
            }
            Some(PasswdEntry {
                name: f[0].to_string(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().ok()?,
                home: PathBuf::from(f[5]),
                shell: f[6].to_string(),
            })
        })
        .collect()
}

impl From<User> for PasswdEntry {
    fn from(u: User) -> Self {
        PasswdEntry {
            name: u.name,
            uid: u.uid.as_raw(),
            gid: u.gid.as_raw(),
            home: u.dir,
            shell: u.shell.to_string_lossy().into_owned(),
        }
    }
}

/// Look a user up by name: in `/etc/passwd` first, then in the system's user
/// database (see [`system_user_by_name`]).
pub fn lookup_passwd(name: &str) -> Option<PasswdEntry> {
    passwd_file()
        .and_then(|c| parse_passwd(&c).into_iter().find(|e| e.name == name))
        .or_else(|| system_user_by_name(name))
}

/// Look a user up by uid: in `/etc/passwd` first, then in the system's user
/// database (see [`system_user_by_uid`]).
pub fn lookup_uid(uid: u32) -> Option<PasswdEntry> {
    passwd_file()
        .and_then(|c| parse_passwd(&c).into_iter().find(|e| e.uid == uid))
        .or_else(|| system_user_by_uid(uid))
}

fn passwd_file() -> Option<String> {
    std::fs::read_to_string("/etc/passwd").ok()
}

/// `getpwuid_r`: the user database behind NSS on Linux and Open Directory on
/// macOS, where ordinary accounts (uid 501 and up) are not in `/etc/passwd`.
/// `None` when the uid is unknown or the lookup fails.
pub fn system_user_by_uid(uid: u32) -> Option<PasswdEntry> {
    User::from_uid(Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(PasswdEntry::from)
}

/// `getpwnam_r`; see [`system_user_by_uid`]. An empty name is never found.
pub fn system_user_by_name(name: &str) -> Option<PasswdEntry> {
    if name.is_empty() {
        return None;
    }
    User::from_name(name).ok().flatten().map(PasswdEntry::from)
}

/// Name of the effective user, if the user database knows it.
pub fn current_user_name() -> Option<String> {
    lookup_uid(effective_uid()).map(|e| e.name)
}

fn is_current_user(user: &str) -> bool {
    if user == "root" && is_root() {
        return true;
    }
    match lookup_passwd(user) {
        Some(e) => e.uid == effective_uid(),
        None => false,
    }
}

/// Find an executable on `PATH` (falling back to the usual system directories).
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs = path
        .split(':')
        .filter(|d| !d.is_empty())
        .map(String::from)
        .chain(
            [
                "/usr/local/sbin",
                "/usr/local/bin",
                "/usr/sbin",
                "/usr/bin",
                "/sbin",
                "/bin",
            ]
            .into_iter()
            .map(String::from),
        );
    for d in dirs {
        let candidate = Path::new(&d).join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let d = std::env::temp_dir().join(format!("warren-gw-exec-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn me() -> HomeOwner {
        HomeOwner::current()
    }

    fn program(cmd: &Command) -> String {
        cmd.as_std().get_program().to_string_lossy().into_owned()
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.as_std().get_envs().find_map(|(k, v)| {
            (k == key).then(|| {
                v.map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
        })
    }

    #[tokio::test]
    async fn runs_and_reports_streams_and_exit_code() {
        let h = home();
        let req = ExecRequest {
            command: "echo out; echo err 1>&2; exit 3".into(),
            ..Default::default()
        };
        let r = run_exec(&h, &me(), &req).await.unwrap();
        assert_eq!(r.exit_code, 3);
        assert_eq!(r.stdout, "out\n");
        assert_eq!(r.stderr, "err\n");
        assert!(!r.truncated);
        assert!(r.duration_ms >= 0);
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[tokio::test]
    async fn workdir_env_and_validation() {
        let h = home();
        std::fs::create_dir_all(h.join("sub")).unwrap();
        let req = ExecRequest {
            command: "pwd; echo $FOO; echo $HOME".into(),
            workdir: "sub".into(),
            env: [("FOO".to_string(), "bar".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let r = run_exec(&h, &me(), &req).await.unwrap();
        assert_eq!(
            r.stdout,
            format!("{}/sub\nbar\n{}\n", h.display(), h.display()),
            "{}",
            r.stdout
        );
        let bad = ExecRequest {
            command: "true".into(),
            workdir: "nope".into(),
            ..Default::default()
        };
        assert_eq!(run_exec(&h, &me(), &bad).await.unwrap_err().status, 400);
        let empty = ExecRequest {
            command: "  ".into(),
            ..Default::default()
        };
        assert_eq!(run_exec(&h, &me(), &empty).await.unwrap_err().status, 400);
        let bad_env = ExecRequest {
            command: "true".into(),
            env: [("A=B".to_string(), "x".to_string())].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(run_exec(&h, &me(), &bad_env).await.unwrap_err().status, 400);
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[tokio::test]
    async fn caps_output() {
        let h = home();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 5000 /dev/zero | tr '\\0' x"])
            .current_dir(&h);
        let out = run_capped(cmd, Duration::from_secs(30), 100).await.unwrap();
        assert_eq!(out.stdout.len(), 100);
        assert!(out.truncated);
        assert_eq!(out.exit_code, 0);
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[tokio::test]
    async fn times_out_and_kills_the_tree() {
        let h = home();
        let started = Instant::now();
        let req = ExecRequest {
            command: "sleep 30; echo late".into(),
            timeout_seconds: 1,
            ..Default::default()
        };
        let e = run_exec(&h, &me(), &req).await.unwrap_err();
        assert_eq!(e.code, "timeout");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[tokio::test]
    async fn combined_output_merges_stderr() {
        let h = home();
        let out = run_shell_combined(
            &h,
            &me(),
            "echo a; echo b 1>&2; echo $HOME",
            Duration::from_secs(30),
            1024,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, format!("a\nb\n{}\n", h.display()));
        assert_eq!(out.stderr, "");
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[tokio::test]
    async fn switching_user_requires_root_or_same_user() {
        let h = home();
        let my_name = current_user_name().unwrap_or_default();
        if !my_name.is_empty() {
            let req = ExecRequest {
                command: "echo same".into(),
                user: my_name,
                ..Default::default()
            };
            assert_eq!(run_exec(&h, &me(), &req).await.unwrap().stdout, "same\n");
        }
        if !is_root() {
            let req = ExecRequest {
                command: "id".into(),
                user: "nobody-such-user".into(),
                ..Default::default()
            };
            let e = run_exec(&h, &me(), &req).await.unwrap_err();
            assert_eq!(e.code, "not_supported");
        } else {
            let req = ExecRequest {
                command: "id".into(),
                user: "nobody-such-user".into(),
                ..Default::default()
            };
            assert_eq!(run_exec(&h, &me(), &req).await.unwrap_err().status, 400);
        }
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn passwd_parsing_and_which() {
        let entries = parse_passwd(
            "root:x:0:0:root:/root:/bin/bash\nbad line\nagent:x:1000:1000::/home/agent:/bin/bash\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name, "agent");
        assert_eq!(entries[1].uid, 1000);
        assert_eq!(entries[1].home, PathBuf::from("/home/agent"));
        let id = Identity::from_passwd(&entries[1]);
        assert_eq!(
            (id.name.as_str(), id.uid, id.gid, id.named),
            ("agent", 1000, 1000, true)
        );
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-binary-xyz").is_none());
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(9)), 137);
        assert_eq!(effective_uid(), effective_uid());
        assert_ne!(effective_gid(), u32::MAX);
        if std::fs::metadata("/etc/passwd").is_ok() {
            assert_eq!(lookup_uid(0).unwrap().name, "root");
        }
    }

    #[test]
    fn uid_lookup_falls_back_to_the_system_user_database() {
        let me = effective_uid();
        let entry = lookup_uid(me).expect("the effective user is known to the system");
        assert!(
            entry.name.parse::<u32>().is_err(),
            "numeric name {:?} for uid {me}",
            entry.name
        );
        assert_eq!(entry.uid, me);
        // `getpwuid_r` agrees with whatever source answered.
        let sys = User::from_uid(Uid::from_raw(me))
            .expect("getpwuid_r")
            .expect("the effective user has a system record");
        assert_eq!(entry.name, sys.name);
        assert_eq!(entry.gid, sys.gid.as_raw());
        assert_eq!(entry.home, sys.dir);
        // The fallbacks alone find the same user, by uid and by name.
        let by_uid = system_user_by_uid(me).unwrap();
        assert_eq!(
            (by_uid.name.as_str(), by_uid.uid),
            (entry.name.as_str(), me)
        );
        let by_name = system_user_by_name(&entry.name).unwrap();
        assert_eq!(by_name, by_uid);
        assert_eq!(lookup_passwd(&entry.name).unwrap().uid, me);
        assert!(Identity::from_passwd(&by_uid).named);
        // Unknown users stay unknown through every path.
        assert!(system_user_by_uid(4_000_000_000).is_none());
        assert!(lookup_uid(4_000_000_000).is_none());
        assert!(system_user_by_name("nobody-such-user-xyz").is_none());
        assert!(system_user_by_name("").is_none());
        assert!(system_user_by_name("bad\0name").is_none());
        assert!(lookup_passwd("nobody-such-user-xyz").is_none());
    }

    fn removed_envs(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn platform_variables_are_hidden_from_user_processes() {
        let mut cmd = Command::new("/bin/sh");
        hide_platform_env_from(
            &mut cmd,
            [
                "WARREN_INSTANCE_TOKEN",
                "WARREN_HOME",
                "PATH",
                "WARREN_GATEWAY_BIND",
                "OPENAI_API_KEY",
                "WARREN_",
            ]
            .map(std::ffi::OsString::from),
        );
        // `get_envs` is sorted by key.
        assert_eq!(
            removed_envs(&cmd),
            ["WARREN_", "WARREN_GATEWAY_BIND", "WARREN_INSTANCE_TOKEN"]
        );
        // Every path a user command takes scrubs the gateway's real
        // environment: plain, through runuser/su, and with direct id changes.
        let named = Identity {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
        };
        let numeric = Identity {
            name: "1234".into(),
            uid: 1234,
            gid: 1234,
            named: false,
        };
        // Other tests in this binary set and unset WARREN_* variables, so
        // only variables present both before and after building a command
        // are required to have been removed.
        let before = hidden_vars_now();
        let cmds = [
            plain(shell_argv("env")),
            switched(&named, shell_argv("env")),
            switched(&numeric, shell_argv("env")),
            owner_shell(&HomeOwner::current(), "env"),
        ];
        let after = hidden_vars_now();
        for cmd in cmds {
            let removed = removed_envs(&cmd);
            assert!(removed.iter().all(|k| is_hidden_var(k)), "{removed:?}");
            for k in before.iter().filter(|k| after.contains(k)) {
                assert!(removed.contains(k), "{k} not hidden: {removed:?}");
            }
            // Explicit settings made afterwards survive (USER/LOGNAME here).
            assert!(removed.iter().all(|k| k != "USER" && k != "LOGNAME"));
        }
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(
            shell_join(&["a b".to_string(), "it's".to_string(), String::new()]),
            "exec 'a b' 'it'\\''s' ''"
        );
        assert_eq!(
            shell_join(&shell_argv("echo $X")),
            "exec '/bin/sh' '-lc' 'echo $X'"
        );
    }

    #[test]
    fn owner_commands_without_a_switch_are_plain() {
        let owner = me();
        if owner.switches() {
            return; // running as root with a foreign home: covered below with a fake owner
        }
        let cmd = owner_shell(&owner, "echo hi");
        assert_eq!(program(&cmd), "/bin/sh");
        assert_eq!(args(&cmd), ["-lc", "echo hi"]);
        assert_eq!(env_of(&cmd, "USER"), None);
        let cmd = owner_command(&owner, &["/bin/echo".to_string(), "x".to_string()]).unwrap();
        assert_eq!(program(&cmd), "/bin/echo");
        assert_eq!(args(&cmd), ["x"]);
        assert!(owner_command(&owner, &[]).is_err());
    }

    #[test]
    fn switched_commands_use_runuser_su_or_direct_ids() {
        let agent = Identity {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
        };
        let cmd = switched(&agent, shell_argv("echo hi"));
        let a = args(&cmd);
        if let Some(runuser) = which("runuser") {
            assert_eq!(program(&cmd), runuser.to_string_lossy());
            assert_eq!(a[..4], ["-m", "-s", "/bin/sh", "--session-command"]);
            assert_eq!(a[4], "exec '/bin/sh' '-lc' 'echo hi'");
            assert_eq!(a[5], "agent");
        } else if let Some(su) = which("su") {
            assert_eq!(program(&cmd), su.to_string_lossy());
            assert_eq!(
                a,
                [
                    "-m",
                    "-s",
                    "/bin/sh",
                    "agent",
                    "-c",
                    "exec '/bin/sh' '-lc' 'echo hi'"
                ]
            );
        } else {
            assert_eq!(program(&cmd), "/bin/sh");
            assert_eq!(a, ["-lc", "echo hi"]);
        }
        assert_eq!(env_of(&cmd, "USER").as_deref(), Some("agent"));
        assert_eq!(env_of(&cmd, "LOGNAME").as_deref(), Some("agent"));

        // An owner without a passwd entry cannot go through runuser/su.
        let numeric = Identity {
            name: "1234".into(),
            uid: 1234,
            gid: 1234,
            named: false,
        };
        let cmd = switched(
            &numeric,
            vec!["/usr/bin/agent".into(), "run".into(), "a b".into()],
        );
        assert_eq!(program(&cmd), "/usr/bin/agent");
        assert_eq!(args(&cmd), ["run", "a b"]);
        assert_eq!(env_of(&cmd, "USER").as_deref(), Some("1234"));

        // The same decisions through a (pretend) root gateway's home owner.
        let owner = HomeOwner {
            name: "agent".into(),
            uid: 1000,
            gid: 1000,
            named: true,
            process_uid: 0,
        };
        let cmd = owner_command(&owner, &["/bin/true".to_string()]).unwrap();
        if which("runuser").is_some() || which("su").is_some() {
            assert_ne!(program(&cmd), "/bin/true");
            assert!(args(&cmd).iter().any(|a| a == "exec '/bin/true'"));
        } else {
            assert_eq!(program(&cmd), "/bin/true");
        }
        assert_eq!(
            env_of(&owner_shell(&owner, "id"), "LOGNAME").as_deref(),
            Some("agent")
        );
    }
}
