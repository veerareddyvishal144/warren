//! Wrap mode (`warren-gateway --wrap -- <argv...>`): the gateway as the
//! container's entrypoint around an image that does not ship it.
//!
//! The daemon injects the static gateway into registered workspace images
//! and replaces the entrypoint with it, passing the image's original
//! entrypoint and command after `--`. The gateway binds its port first, then
//! runs that command as a child in the working directory the container
//! started in, with the process environment minus the `WARREN_*` platform
//! variables (the instance token above all) except `WARREN_HOME`, and with
//! stdout and stderr inherited so `docker logs` shows the application.
//!
//! The child is supervised, not managed: its pid, exit status and exit time
//! are recorded and reported by `GET /v1/health` under `wrapped`, but it is
//! never restarted (the image's own process model decides), and the gateway
//! keeps serving after it exits so the instance stays manageable. When the
//! gateway is told to stop (SIGTERM or SIGINT) it forwards SIGTERM to the
//! child, waits up to [`GRACE`], sends SIGKILL, and exits with the child's
//! exit code, the way the container would have exited without the wrapper.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde::Serialize;
use tokio::{process::Command, sync::watch};
use warren_core::{Clock, types::ENV_HOME};

use crate::exec::exit_code;

/// How long the child gets after SIGTERM before SIGKILL.
pub const GRACE: Duration = Duration::from_secs(10);
/// How long to wait for the child to be reaped after SIGKILL.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// Prefix of the platform variables hidden from the wrapped program.
const PLATFORM_PREFIX: &str = "WARREN_";

/// What to wrap: the image's entrypoint and command, and where to run it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrapSpec {
    /// Program and arguments; empty means wrap mode without a program.
    pub argv: Vec<String>,
    /// Working directory for the child; `None` inherits the gateway's.
    pub cwd: Option<PathBuf>,
}

/// The supervised child as reported by `GET /v1/health` (`wrapped`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WrappedStatus {
    /// The child is alive.
    pub running: bool,
    /// Pid of the child; `0` when it could not be started.
    pub pid: u32,
    /// Exit code once it exited: the program's code, `128 + signal` when a
    /// signal killed it, `127`/`126` when it could not be started.
    pub exit_code: Option<i32>,
    /// Unix milliseconds when the child started.
    pub started_at: Option<i64>,
    /// Unix milliseconds when the child exited.
    pub exited_at: Option<i64>,
    /// Why the child could not be started or waited for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WrappedStatus {
    /// The code the gateway process exits with in wrap mode: the child's
    /// exit code, `0` when the child never ran or is somehow still around.
    pub fn process_exit_code(&self) -> i32 {
        self.exit_code.unwrap_or(0)
    }
}

/// True for the platform variables a wrapped program must not see: every
/// `WARREN_*` variable except `WARREN_HOME`.
pub fn is_hidden_var(key: &str) -> bool {
    key.starts_with(PLATFORM_PREFIX) && key != ENV_HOME
}

/// The `Command` for `spec`: the program resolved through `PATH`, the
/// gateway's environment minus the hidden platform variables, stdio
/// inherited, running in `spec.cwd`.
pub fn child_command(spec: &WrapSpec) -> Command {
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    crate::exec::hide_platform_env(&mut cmd);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

/// Runs and watches the wrapped program.
pub struct Supervisor {
    spec: WrapSpec,
    clock: Arc<dyn Clock>,
    status: watch::Sender<WrappedStatus>,
    /// Set by [`shutdown`](Self::shutdown): the exit that follows is ours.
    stopping: Arc<AtomicBool>,
}

impl Supervisor {
    /// A supervisor for `spec`; nothing runs until [`start`](Self::start).
    pub fn new(spec: WrapSpec, clock: Arc<dyn Clock>) -> Supervisor {
        let (status, _) = watch::channel(WrappedStatus::default());
        Supervisor {
            spec,
            clock,
            status,
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The program and arguments being wrapped.
    pub fn argv(&self) -> &[String] {
        &self.spec.argv
    }

    /// Current state of the child.
    pub fn status(&self) -> WrappedStatus {
        self.status.borrow().clone()
    }

    /// Start the child and the task that records its exit. A program that
    /// cannot be started is recorded as exited (`127` when not found, `126`
    /// otherwise) with the error; the gateway keeps serving either way.
    pub fn start(&self) {
        if self.spec.argv.is_empty() {
            return;
        }
        let now = self.clock.now_unix_ms();
        let mut child = match child_command(&self.spec).spawn() {
            Ok(c) => c,
            Err(e) => {
                let code = if e.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                };
                tracing::error!(
                    program = %self.spec.argv[0],
                    error = %e,
                    "cannot start the wrapped program; serving without it"
                );
                self.status.send_replace(WrappedStatus {
                    running: false,
                    pid: 0,
                    exit_code: Some(code),
                    started_at: Some(now),
                    exited_at: Some(now),
                    error: Some(format!("cannot start {}: {e}", self.spec.argv[0])),
                });
                return;
            }
        };
        let pid = child.id().unwrap_or(0);
        tracing::info!(pid, argv = ?self.spec.argv, "wrapped program started");
        self.status.send_replace(WrappedStatus {
            running: true,
            pid,
            exit_code: None,
            started_at: Some(now),
            exited_at: None,
            error: None,
        });
        let status = self.status.clone();
        let clock = self.clock.clone();
        let stopping = self.stopping.clone();
        tokio::spawn(async move {
            let waited = child.wait().await;
            let now = clock.now_unix_ms();
            status.send_modify(|s| {
                s.running = false;
                s.exited_at = Some(now);
                match &waited {
                    Ok(st) => s.exit_code = Some(exit_code(*st)),
                    Err(e) => s.error = Some(format!("cannot wait for the wrapped program: {e}")),
                }
            });
            match waited {
                Ok(st) if stopping.load(Ordering::SeqCst) => {
                    tracing::info!(pid, exit_code = exit_code(st), "wrapped program stopped")
                }
                Ok(st) => tracing::warn!(
                    pid,
                    exit_code = exit_code(st),
                    "wrapped program exited; not restarting it, the gateway keeps serving"
                ),
                Err(e) => tracing::error!(pid, error = %e, "cannot wait for the wrapped program"),
            }
        });
    }

    /// Stop the child: SIGTERM, up to `grace` to exit, then SIGKILL. Returns
    /// the final status; a child that already exited is left alone.
    pub async fn shutdown(&self, grace: Duration) -> WrappedStatus {
        let mut rx = self.status.subscribe();
        let current = rx.borrow_and_update().clone();
        if !current.running {
            return current;
        }
        self.stopping.store(true, Ordering::SeqCst);
        let pid = Pid::from_raw(current.pid as i32);
        tracing::info!(
            pid = current.pid,
            "forwarding SIGTERM to the wrapped program"
        );
        if let Err(e) = kill(pid, Signal::SIGTERM) {
            tracing::warn!(pid = current.pid, error = %e, "cannot signal the wrapped program");
        }
        match tokio::time::timeout(grace, rx.wait_for(|s| !s.running)).await {
            Ok(Ok(s)) => return s.clone(),
            Ok(Err(_)) => return self.status(),
            Err(_) => {
                tracing::warn!(
                    pid = current.pid,
                    grace_s = grace.as_secs(),
                    "wrapped program ignored SIGTERM; sending SIGKILL"
                );
                if let Err(e) = kill(pid, Signal::SIGKILL) {
                    tracing::warn!(pid = current.pid, error = %e, "cannot kill the wrapped program");
                }
            }
        }
        match tokio::time::timeout(KILL_WAIT, rx.wait_for(|s| !s.running)).await {
            Ok(Ok(s)) => s.clone(),
            _ => {
                tracing::error!(
                    pid = current.pid,
                    "wrapped program did not exit after SIGKILL"
                );
                self.status()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warren_core::RealClock;

    fn spec(argv: &[&str]) -> WrapSpec {
        WrapSpec {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
        }
    }

    fn supervisor(argv: &[&str]) -> Supervisor {
        Supervisor::new(spec(argv), Arc::new(RealClock))
    }

    async fn wait_exit(s: &Supervisor) -> WrappedStatus {
        let mut rx = s.status.subscribe();
        tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|st| !st.running))
            .await
            .expect("exit in time")
            .expect("sender alive")
            .clone()
    }

    #[test]
    fn hidden_variables() {
        assert!(is_hidden_var("WARREN_INSTANCE_TOKEN"));
        assert!(is_hidden_var("WARREN_GATEWAY_BIND"));
        assert!(is_hidden_var("WARREN_NETWORK"));
        assert!(is_hidden_var("WARREN_BROKER"));
        // The proxy variables and secret routes are for the agent's tools.
        assert!(!is_hidden_var("HTTPS_PROXY"));
        assert!(!is_hidden_var("ANTHROPIC_BASE_URL"));
        assert!(is_hidden_var("WARREN_"));
        assert!(!is_hidden_var(ENV_HOME));
        assert!(!is_hidden_var("WARREN"));
        assert!(!is_hidden_var("OPENAI_API_KEY"));
        assert!(!is_hidden_var("PATH"));
        assert!(!is_hidden_var("XWARREN_FOO"));
    }

    #[test]
    fn child_command_hides_platform_variables_and_keeps_the_rest() {
        // SAFETY: tests in this module are the only ones touching these
        // variables, and they only read them back through `std::env`.
        unsafe {
            std::env::set_var("WARREN_WRAP_TEST_TOKEN", "secret");
        }
        let s = WrapSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            cwd: Some(PathBuf::from("/")),
        };
        let cmd = child_command(&s);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "/bin/sh");
        let args: Vec<_> = std_cmd.get_args().collect();
        assert_eq!(args, ["-c", "true"]);
        assert_eq!(std_cmd.get_current_dir(), Some(std::path::Path::new("/")));
        let removed: Vec<String> = std_cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(removed.contains(&"WARREN_WRAP_TEST_TOKEN".to_string()));
        assert!(!removed.contains(&ENV_HOME.to_string()));
        assert!(!removed.contains(&"PATH".to_string()));
        assert!(removed.iter().all(|k| k.starts_with("WARREN_")));
        unsafe {
            std::env::remove_var("WARREN_WRAP_TEST_TOKEN");
        }
    }

    #[test]
    fn status_json_shape() {
        let s = WrappedStatus {
            running: true,
            pid: 42,
            exit_code: None,
            started_at: Some(1),
            exited_at: None,
            error: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 42);
        assert!(v["exit_code"].is_null());
        assert!(v.get("error").is_none());
        assert_eq!(s.process_exit_code(), 0);
        let done = WrappedStatus {
            running: false,
            exit_code: Some(3),
            error: Some("x".into()),
            ..s
        };
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["error"], "x");
        assert_eq!(done.process_exit_code(), 3);
    }

    #[tokio::test]
    async fn records_exit_code_and_time() {
        let s = supervisor(&["/bin/sh", "-c", "exit 7"]);
        assert_eq!(s.status(), WrappedStatus::default());
        assert_eq!(s.argv()[2], "exit 7");
        s.start();
        let started = s.status();
        assert!(started.pid > 0);
        assert!(started.started_at.unwrap() > 0);
        let done = wait_exit(&s).await;
        assert!(!done.running);
        assert_eq!(done.pid, started.pid);
        assert_eq!(done.exit_code, Some(7));
        assert!(done.exited_at.unwrap() >= started.started_at.unwrap());
        assert!(done.error.is_none());
        // Shutting down an exited child is a no-op.
        assert_eq!(s.shutdown(GRACE).await, done);
        assert_eq!(done.process_exit_code(), 7);
    }

    #[tokio::test]
    async fn signal_deaths_are_reported_as_128_plus_signal() {
        let s = supervisor(&["/bin/sh", "-c", "kill -9 $$"]);
        s.start();
        assert_eq!(wait_exit(&s).await.exit_code, Some(137));
    }

    #[tokio::test]
    async fn unstartable_program_is_recorded_not_fatal() {
        let s = supervisor(&["/definitely/missing/program", "x"]);
        s.start();
        let st = s.status();
        assert!(!st.running);
        assert_eq!(st.pid, 0);
        assert_eq!(st.exit_code, Some(127));
        assert!(
            st.error
                .as_deref()
                .unwrap()
                .contains("/definitely/missing/program")
        );
        assert_eq!(s.shutdown(GRACE).await, st);
        let empty = supervisor(&[]);
        empty.start();
        assert_eq!(empty.status(), WrappedStatus::default());
    }

    #[tokio::test]
    async fn shutdown_terminates_a_running_child() {
        let s = supervisor(&["/bin/sh", "-c", "exec sleep 30"]);
        s.start();
        let pid = s.status().pid;
        assert!(pid > 0);
        let started = std::time::Instant::now();
        let st = s.shutdown(GRACE).await;
        assert!(!st.running);
        assert_eq!(st.exit_code, Some(128 + Signal::SIGTERM as i32));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            kill(Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        );
    }

    #[tokio::test]
    async fn shutdown_kills_a_child_that_ignores_sigterm() {
        // The shell signals through a marker file that the trap is installed,
        // otherwise SIGTERM could arrive while it is still starting up.
        let marker = std::env::temp_dir().join(format!(
            "warren-gw-wrap-trap-{}",
            warren_core::ids::new_id()
        ));
        let script = format!("trap '' TERM; : > '{}'; sleep 30 & wait", marker.display());
        let s = supervisor(&["/bin/sh", "-c", &script]);
        s.start();
        let pid = s.status().pid;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(std::time::Instant::now() < deadline, "trap never installed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&marker);
        let st = s.shutdown(Duration::from_millis(500)).await;
        assert!(!st.running);
        assert_eq!(st.exit_code, Some(137));
        assert_eq!(
            kill(Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        );
    }
}
