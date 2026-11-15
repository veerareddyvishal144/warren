//! Docker-backed tests. They skip with a note on stderr when no Docker socket is
//! reachable and otherwise drive throwaway `alpine:3.20` containers through the
//! whole `Runtime` surface. The plain lifecycle templates have `default_port: 0`,
//! so `start` does not wait for a gateway; the gateway-injection tests use
//! workspace-scope templates with the gateway port and a mount at `/home`, once
//! with the static gateway from `make gateway` (skipped while it is missing or
//! has no wrap mode yet) and once with a shell-script stand-in that checks the
//! wiring on its own.
//!
//! Some steps create files as root inside the bind-mounted home. When the tests
//! run as an ordinary user against a root Docker daemon (the usual developer
//! setup) those files exercise the root helper fallback of the storage
//! operations; as root they are simply removed on the host.

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use warren_core::{
    Config, ExecRequest, ExecResult, GATEWAY_PORT, Instance, LogsOptions, MAX_EXEC_OUTPUT_BYTES,
    NetworkMode, Runtime, Shape, Template, TemplateScope,
    errors::{CODE_BAD_GATEWAY, CODE_TIMEOUT, is_requires_restart},
    ids,
};

use crate::{
    DockerRuntime, HOME_DIR, LABEL_INJECTED, LABEL_SCOPE, NETWORK_INTERNAL, NETWORK_PUBLIC,
};

const IMAGE: &str = "alpine:3.20";
/// The fully static gateway `make gateway` builds.
const STATIC_GATEWAY: &str =
    "/data/warren/target-musl/x86_64-unknown-linux-musl/release/warren-gateway";
/// Points the static-gateway test at another binary (a private musl build).
const ENV_TEST_GATEWAY: &str = "WARREN_TEST_GATEWAY_BIN";

/// A fresh empty directory under `TMPDIR` for one test.
pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "warren-rtdocker-{tag}-{}-{n}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn docker_available() -> bool {
    Path::new("/var/run/docker.sock").exists() || std::env::var_os("DOCKER_HOST").is_some()
}

macro_rules! require_docker {
    () => {
        if !docker_available() {
            eprintln!("skipping: no Docker socket available");
            return;
        }
    };
}

macro_rules! ensure {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err(format!($($arg)+));
        }
    };
}

struct Fixture {
    rt: DockerRuntime,
    data_dir: PathBuf,
}

impl Fixture {
    async fn new() -> Fixture {
        Self::with_gateway(None).await
    }

    /// A runtime that injects `gateway_bin` into workspace-template containers.
    async fn with_gateway(gateway_bin: Option<PathBuf>) -> Fixture {
        let data_dir = scratch_dir("fixture");
        let cfg = Config {
            data_dir: data_dir.clone(),
            runtime: "auto".into(),
            dev_mode: true,
            gateway_bin,
            ..Config::default()
        };
        let rt = DockerRuntime::new(cfg).await.expect("docker client");
        rt.ready().await.expect("docker runtime ready");
        Fixture { rt, data_dir }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn template() -> Template {
    Template {
        name: "alpine-test".into(),
        image_ref: IMAGE.into(),
        default_port: 0,
        mount_path: "/home/agent".into(),
        default_shape: Shape {
            cpu: 1,
            memory: 1,
            disk: 2,
        },
        revision: 1,
        ..Template::default()
    }
}

fn instance(tpl: &Template) -> Instance {
    Instance {
        id: ids::new_instance_id(),
        workspace_id: "ws_testworkspace0000000".into(),
        template: tpl.name.clone(),
        template_name: tpl.name.clone(),
        template_revision: tpl.revision,
        image_ref: tpl.image_ref.clone(),
        shape: tpl.default_shape,
        region: "local".into(),
        network: NetworkMode::AllowAll,
        default_port: tpl.default_port,
        mount_path: tpl.mount_path.clone(),
        instance_token: "test-token".into(),
        ..Instance::default()
    }
}

/// A template registered from a plain image, the way a workspace does it: no
/// gateway of its own, the gateway port as its default port, the disk at `/home`.
fn workspace_template() -> Template {
    Template {
        name: "alpine-workspace".into(),
        scope: TemplateScope::Workspace,
        workspace_id: "ws_testworkspace0000000".into(),
        default_port: GATEWAY_PORT,
        mount_path: "/home".into(),
        ..template()
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

fn cmd(parts: &[&str]) -> Option<Vec<String>> {
    Some(strings(parts))
}

/// A command that logs one line and then stays alive until stopped.
fn keepalive() -> Option<Vec<String>> {
    cmd(&["/bin/sh", "-c", "echo booting; exec sleep 100000"])
}

fn exec(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.into(),
        ..Default::default()
    }
}

/// Exec as uid 1000 so files written into the bind mount stay removable by the test user.
fn exec_as_agent(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.into(),
        user: "1000:1000".into(),
        ..Default::default()
    }
}

/// Exec as root, the way the gateway inside an instance works.
fn exec_as_root(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.into(),
        user: "root".into(),
        ..Default::default()
    }
}

async fn run(rt: &DockerRuntime, inst: &Instance, req: ExecRequest) -> Result<ExecResult, String> {
    rt.exec(inst, &req)
        .await
        .map_err(|e| format!("exec {:?}: {e}", req.command))
}

async fn destroy_quietly(rt: &DockerRuntime, inst: &Instance) {
    if let Err(e) = rt.destroy(inst, true).await {
        eprintln!("cleanup of {} failed: {e}", inst.id);
    }
}

#[tokio::test]
async fn ready_selects_runtime_and_creates_networks() {
    require_docker!();
    let f = Fixture::new().await;
    assert!(
        matches!(f.rt.runtime_name(), "runsc" | "runc"),
        "runtime {:?}",
        f.rt.runtime_name()
    );
    let public =
        f.rt.docker()
            .inspect_network(NETWORK_PUBLIC, None)
            .await
            .expect("warren-net exists");
    assert_ne!(public.internal, Some(true));
    assert_eq!(public.driver.as_deref(), Some("bridge"));
    let internal =
        f.rt.docker()
            .inspect_network(NETWORK_INTERNAL, None)
            .await
            .expect("warren-internal exists");
    assert_eq!(internal.internal, Some(true));
    f.rt.ready().await.expect("ready is idempotent");
    assert_eq!(f.rt.name(), "docker");
}

#[tokio::test]
async fn ensure_image_returns_a_digest() {
    require_docker!();
    let f = Fixture::new().await;
    let digest =
        f.rt.ensure_image(IMAGE)
            .await
            .expect("alpine:3.20 is pulled");
    assert!(digest.contains("sha256:"), "digest {digest}");
    let err = f.rt.ensure_image("").await.unwrap_err();
    assert_eq!(err.code, warren_core::errors::CODE_INVALID_REQUEST);
}

#[tokio::test]
async fn container_lifecycle_roundtrip() {
    require_docker!();
    let f = Fixture::new().await;
    let tpl = template();
    let mut inst = instance(&tpl);
    let outcome = lifecycle(&f, &tpl, &mut inst).await;
    destroy_quietly(&f.rt, &inst).await;
    outcome.unwrap();
}

async fn lifecycle(f: &Fixture, tpl: &Template, inst: &mut Instance) -> Result<(), String> {
    let rt = &f.rt;
    rt.create_with(inst, tpl, keepalive())
        .await
        .map_err(|e| format!("create: {e}"))?;
    ensure!(
        inst.runtime == "docker",
        "runtime recorded: {:?}",
        inst.runtime
    );
    ensure!(!inst.runtime_ref.is_empty(), "container id recorded");
    ensure!(
        inst.storage_path
            == f.data_dir
                .join("instances")
                .join(&inst.id)
                .to_string_lossy(),
        "storage path {:?}",
        inst.storage_path
    );
    ensure!(
        Path::new(&inst.storage_path).join(HOME_DIR).is_dir(),
        "home directory created"
    );

    let inspect = rt
        .inspect(&inst.runtime_ref)
        .await
        .map_err(|e| format!("inspect: {e}"))?;
    let host = inspect.host_config.clone().unwrap_or_default();
    ensure!(
        host.runtime.as_deref() == Some(rt.runtime_name()),
        "runtime {:?}",
        host.runtime
    );
    ensure!(host.memory == Some(1 << 30), "memory {:?}", host.memory);
    ensure!(
        host.nano_cpus == Some(1_000_000_000),
        "nano cpus {:?}",
        host.nano_cpus
    );
    ensure!(
        host.network_mode.as_deref() == Some(NETWORK_PUBLIC),
        "network {:?}",
        host.network_mode
    );
    let labels = inspect.config.and_then(|c| c.labels).unwrap_or_default();
    ensure!(
        labels.get("warren.instance") == Some(&inst.id),
        "labels {labels:?}"
    );

    let info = rt.start(inst).await.map_err(|e| format!("start: {e}"))?;
    ensure!(
        !info.ip.is_empty() && inst.ip == info.ip,
        "ip {:?}",
        info.ip
    );
    ensure!(info.runtime_ref == inst.runtime_ref, "run info ref");

    // stdout and stderr are separate; the exit code is the command's.
    let res = run(rt, inst, exec("echo out; echo err 1>&2; exit 3")).await?;
    ensure!(res.exit_code == 3, "exit code {}", res.exit_code);
    ensure!(res.stdout == "out\n", "stdout {:?}", res.stdout);
    ensure!(res.stderr == "err\n", "stderr {:?}", res.stderr);
    ensure!(!res.truncated && res.duration_ms >= 0, "flags");

    // Without a user the command runs as the owner of the home directory on the
    // host (alpine has no such account, hence the numeric ids) with HOME at the
    // mount path; an explicit root stays root.
    let home_dir = Path::new(&inst.storage_path).join(HOME_DIR);
    let owner = fs::metadata(&home_dir).map_err(|e| format!("stat home: {e}"))?;
    let res = run(rt, inst, exec("id -u; id -g; echo $HOME")).await?;
    ensure!(
        res.stdout == format!("{}\n{}\n/home/agent\n", owner.uid(), owner.gid()),
        "default identity {:?} (home owned by {}:{})",
        res.stdout,
        owner.uid(),
        owner.gid()
    );
    let res = run(rt, inst, exec_as_root("id -u; echo $HOME")).await?;
    ensure!(res.stdout == "0\n/root\n", "root exec {:?}", res.stdout);
    let res = run(rt, inst, exec("touch /home/agent/by-default-user")).await?;
    ensure!(
        res.exit_code == 0,
        "default user writes the home: {}",
        res.stderr
    );
    let created =
        fs::metadata(home_dir.join("by-default-user")).map_err(|e| format!("stat: {e}"))?;
    ensure!(
        (created.uid(), created.gid()) == (owner.uid(), owner.gid()),
        "file owned by {}:{} on the host, home by {}:{}",
        created.uid(),
        created.gid(),
        owner.uid(),
        owner.gid()
    );

    // user, workdir, env, and the platform env injected at create.
    let req = ExecRequest {
        command: "echo $FOO; pwd; id -u; echo $WARREN_INSTANCE_ID".into(),
        user: "1000:1000".into(),
        workdir: "/tmp".into(),
        env: [("FOO".to_string(), "bar".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let res = run(rt, inst, req).await?;
    ensure!(
        res.stdout == format!("bar\n/tmp\n1000\n{}\n", inst.id),
        "stdout {:?}",
        res.stdout
    );

    // Output is capped per stream and flagged.
    let res = run(
        rt,
        inst,
        exec("head -c 1200000 /dev/zero | tr '\\0' x; echo done 1>&2"),
    )
    .await?;
    ensure!(res.truncated, "truncated flag");
    ensure!(
        res.stdout.len() == MAX_EXEC_OUTPUT_BYTES,
        "stdout len {}",
        res.stdout.len()
    );
    ensure!(
        res.stderr == "done\n",
        "stderr survives truncation: {:?}",
        res.stderr
    );

    // Timeouts are enforced.
    let slow = ExecRequest {
        command: "sleep 30".into(),
        timeout_seconds: 1,
        ..Default::default()
    };
    let started = Instant::now();
    let err = rt.exec(inst, &slow).await.err().ok_or("timeout expected")?;
    ensure!(err.code == CODE_TIMEOUT, "code {}", err.code);
    ensure!(
        started.elapsed() < Duration::from_secs(10),
        "timed out late: {:?}",
        started.elapsed()
    );

    let logs = rt
        .logs(inst, LogsOptions { tail: 10, since: 0 })
        .await
        .map_err(|e| format!("logs: {e}"))?;
    ensure!(logs.contains("booting"), "logs {logs:?}");
    let none = rt
        .logs(
            inst,
            LogsOptions {
                tail: 0,
                since: i64::from(i32::MAX),
            },
        )
        .await
        .map_err(|e| format!("logs: {e}"))?;
    ensure!(none.is_empty(), "logs since the far future {none:?}");

    let res = run(
        rt,
        inst,
        exec_as_agent("head -c 4096 /dev/zero > /home/agent/blob && ls -l /home/agent/blob"),
    )
    .await?;
    ensure!(
        res.exit_code == 0,
        "write blob: {} {}",
        res.stdout,
        res.stderr
    );
    let disk = rt
        .disk_usage(inst)
        .await
        .map_err(|e| format!("disk usage: {e}"))?;
    ensure!(disk >= 4096, "disk usage {disk}");

    let stats = rt.stats(inst).await.map_err(|e| format!("stats: {e}"))?;
    ensure!(
        stats.at > 0 && stats.memory_limit_bytes > 0 && stats.disk_bytes == disk,
        "stats {stats:?}"
    );
    let stats = rt.stats(inst).await.map_err(|e| format!("stats: {e}"))?;
    ensure!(
        stats.cpu_percent.is_finite() && stats.cpu_percent.is_sign_positive(),
        "cpu percent {}",
        stats.cpu_percent
    );

    match rt
        .resize(
            inst,
            Shape {
                cpu: 2,
                memory: 2,
                disk: 4,
            },
        )
        .await
    {
        Ok(()) => {
            ensure!(
                inst.shape
                    == Shape {
                        cpu: 2,
                        memory: 2,
                        disk: 4
                    },
                "shape recorded {}",
                inst.shape
            );
            let host = rt
                .inspect(&inst.runtime_ref)
                .await
                .map_err(|e| format!("inspect: {e}"))?
                .host_config
                .unwrap_or_default();
            ensure!(
                host.memory == Some(2 << 30) && host.nano_cpus == Some(2_000_000_000),
                "limits after resize {:?} {:?}",
                host.memory,
                host.nano_cpus
            );
        }
        Err(e) if is_requires_restart(&e) => eprintln!(
            "live resize unsupported under {}; restart requested",
            rt.runtime_name()
        ),
        Err(e) => return Err(format!("resize: {e}")),
    }

    rt.stop(inst, Duration::from_secs(2))
        .await
        .map_err(|e| format!("stop: {e}"))?;
    let inspect = rt
        .inspect(&inst.runtime_ref)
        .await
        .map_err(|e| format!("inspect: {e}"))?;
    ensure!(!crate::container::is_running(&inspect), "stopped");
    rt.stop(inst, Duration::from_secs(2))
        .await
        .map_err(|e| format!("second stop: {e}"))?;
    let err = rt
        .exec(inst, &exec("true"))
        .await
        .err()
        .ok_or("exec on a stopped container must fail")?;
    ensure!(
        err.code == warren_core::errors::CODE_CONFLICT,
        "exec while stopped: {}",
        err.code
    );

    let again = rt
        .start(inst)
        .await
        .map_err(|e| format!("second start: {e}"))?;
    ensure!(!again.ip.is_empty(), "ip after restart");
    let res = run(rt, inst, exec("cat /home/agent/blob | wc -c")).await?;
    ensure!(
        res.stdout.trim() == "4096",
        "data survives a restart: {:?}",
        res.stdout
    );

    // Files the instance creates as root (the gateway does) must not survive
    // destroy: a root-owned directory with content is exactly what a non-root
    // daemon cannot delete itself, so remove_storage falls back to the helper.
    let res = run(
        rt,
        inst,
        exec_as_root(
            "touch /home/agent/rootfile && mkdir -p /home/agent/rootdir && touch /home/agent/rootdir/inner && chmod 700 /home/agent/rootdir",
        ),
    )
    .await?;
    ensure!(res.exit_code == 0, "root writes: {}", res.stderr);
    ensure!(
        home_dir.join("rootfile").is_file() && home_dir.join("rootdir").is_dir(),
        "root-created entries visible on the host"
    );

    rt.destroy(inst, true)
        .await
        .map_err(|e| format!("destroy: {e}"))?;
    ensure!(!Path::new(&inst.storage_path).exists(), "storage removed");
    ensure!(
        rt.inspect(&inst.runtime_ref).await.is_err(),
        "container removed"
    );
    rt.destroy(inst, true)
        .await
        .map_err(|e| format!("second destroy: {e}"))?;
    let err = rt
        .start(inst)
        .await
        .err()
        .ok_or("start after destroy must fail")?;
    ensure!(err.is_not_found(), "start after destroy: {}", err.code);
    Ok(())
}

#[tokio::test]
async fn start_fails_fast_when_the_container_exits() {
    require_docker!();
    let f = Fixture::new().await;
    let mut tpl = template();
    tpl.default_port = GATEWAY_PORT;
    let mut inst = instance(&tpl);
    let outcome = async {
        f.rt.create_with(&mut inst, &tpl, cmd(&["/bin/sh", "-c", "echo bye; exit 7"]))
            .await
            .map_err(|e| format!("create: {e}"))?;
        let started = Instant::now();
        let err = f.rt.start(&mut inst).await.err().ok_or("start must fail")?;
        ensure!(err.code == CODE_BAD_GATEWAY, "code {}", err.code);
        ensure!(
            err.message.contains("code 7") && err.message.contains("bye"),
            "message {}",
            err.message
        );
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "took {:?}",
            started.elapsed()
        );
        Ok::<(), String>(())
    }
    .await;
    destroy_quietly(&f.rt, &inst).await;
    outcome.unwrap();
}

#[tokio::test]
async fn recreate_keeps_the_storage() {
    require_docker!();
    let f = Fixture::new().await;
    let tpl = template();
    let mut inst = instance(&tpl);
    let outcome = async {
        let rt = &f.rt;
        rt.create_with(&mut inst, &tpl, keepalive())
            .await
            .map_err(|e| format!("create: {e}"))?;
        rt.start(&mut inst)
            .await
            .map_err(|e| format!("start: {e}"))?;
        run(rt, &inst, exec_as_agent("echo marker > /home/agent/marker")).await?;
        rt.stop(&inst, Duration::from_secs(2))
            .await
            .map_err(|e| format!("stop: {e}"))?;
        let before = (inst.runtime_ref.clone(), inst.storage_path.clone());
        // recreate() uses the image's own command; alpine's `/bin/sh` exits at once,
        // which is fine for checking the container and the mount.
        rt.recreate(&mut inst, &tpl)
            .await
            .map_err(|e| format!("recreate: {e}"))?;
        ensure!(
            inst.runtime_ref != before.0 && !inst.runtime_ref.is_empty(),
            "new container id"
        );
        ensure!(inst.storage_path == before.1, "storage path kept");
        ensure!(
            rt.inspect(&before.0).await.is_err(),
            "old container removed"
        );
        let inspect = rt
            .inspect(&inst.runtime_ref)
            .await
            .map_err(|e| format!("inspect: {e}"))?;
        let binds = inspect
            .host_config
            .and_then(|h| h.binds)
            .unwrap_or_default();
        ensure!(
            binds == vec![format!("{}/{HOME_DIR}:/home/agent", before.1)],
            "binds {binds:?}"
        );
        ensure!(
            Path::new(&before.1).join(HOME_DIR).join("marker").is_file(),
            "data kept on disk"
        );
        Ok::<(), String>(())
    }
    .await;
    destroy_quietly(&f.rt, &inst).await;
    outcome.unwrap();
}

#[tokio::test]
async fn clone_backup_and_restore_through_containers() {
    require_docker!();
    let f = Fixture::new().await;
    let tpl = template();
    let mut src = instance(&tpl);
    let mut dst = instance(&tpl);
    let outcome = async {
        let rt = &f.rt;
        rt.create_with(&mut src, &tpl, keepalive())
            .await
            .map_err(|e| format!("create src: {e}"))?;
        rt.start(&mut src)
            .await
            .map_err(|e| format!("start src: {e}"))?;
        run(
            rt,
            &src,
            exec_as_agent("mkdir -p /home/agent/work && echo v1 > /home/agent/work/file"),
        )
        .await?;
        // A root-only directory, as the gateway leaves behind: an unprivileged
        // daemon cannot copy, archive or clear it on the host and must use the helper.
        let res = run(
            rt,
            &src,
            exec_as_root("mkdir -p /home/agent/secret && echo s1 > /home/agent/secret/file && chmod 700 /home/agent/secret"),
        )
        .await?;
        ensure!(res.exit_code == 0, "root writes: {}", res.stderr);
        rt.stop(&src, Duration::from_secs(2))
            .await
            .map_err(|e| format!("stop src: {e}"))?;

        // Fork: copy the disk, then create and start the copy.
        rt.clone_storage(&src, &mut dst)
            .await
            .map_err(|e| format!("clone: {e}"))?;
        ensure!(
            dst.storage_path == f.data_dir.join("instances").join(&dst.id).to_string_lossy(),
            "dst storage {:?}",
            dst.storage_path
        );
        rt.create_with(&mut dst, &tpl, keepalive())
            .await
            .map_err(|e| format!("create dst: {e}"))?;
        rt.start(&mut dst)
            .await
            .map_err(|e| format!("start dst: {e}"))?;
        let res = run(rt, &dst, exec("cat /home/agent/work/file")).await?;
        ensure!(res.stdout == "v1\n", "cloned content {:?}", res.stdout);
        let res = run(rt, &dst, exec_as_root("cat /home/agent/secret/file")).await?;
        ensure!(res.stdout == "s1\n", "cloned root-only content {:?}", res.stdout);

        // Backup the source, change it, restore, and read the old content back.
        let backup = f.data_dir.join("backups").join(&src.id).join("b1.tar.gz");
        let size = rt
            .backup_storage(&src, &backup)
            .await
            .map_err(|e| format!("backup: {e}"))?;
        ensure!(size > 0 && backup.is_file(), "backup written");
        rt.start(&mut src)
            .await
            .map_err(|e| format!("restart src: {e}"))?;
        run(
            rt,
            &src,
            exec_as_agent("echo v2 > /home/agent/work/file && echo extra > /home/agent/extra"),
        )
        .await?;
        rt.stop(&src, Duration::from_secs(2))
            .await
            .map_err(|e| format!("stop src again: {e}"))?;
        rt.restore_storage(&src, &backup)
            .await
            .map_err(|e| format!("restore: {e}"))?;
        rt.start(&mut src)
            .await
            .map_err(|e| format!("start restored: {e}"))?;
        let res = run(
            rt,
            &src,
            exec("cat /home/agent/work/file; test -e /home/agent/extra && echo still-there"),
        )
        .await?;
        ensure!(res.stdout == "v1\n", "restored content {:?}", res.stdout);
        let res = run(rt, &src, exec_as_root("cat /home/agent/secret/file")).await?;
        ensure!(res.stdout == "s1\n", "restored root-only content {:?}", res.stdout);
        let err = rt
            .restore_storage(&src, &f.data_dir.join("missing.tar.gz"))
            .await
            .err()
            .ok_or("missing backup must fail")?;
        ensure!(err.is_not_found(), "missing backup: {}", err.code);
        Ok::<(), String>(())
    }
    .await;
    destroy_quietly(&f.rt, &src).await;
    destroy_quietly(&f.rt, &dst).await;
    outcome.unwrap();
}

/// Exercises the registry pull path. Opt in with
/// `cargo test -p warren-runtime-docker -- --ignored`: it needs network access to
/// Docker Hub and downloads the tiny `hello-world` image.
#[tokio::test]
#[ignore = "pulls hello-world:latest from Docker Hub"]
async fn ensure_image_pulls_from_the_registry() {
    require_docker!();
    let f = Fixture::new().await;
    let image = "hello-world:latest";
    if f.rt.docker().inspect_image(image).await.is_ok() {
        f.rt.docker()
            .remove_image(
                image,
                None::<bollard::query_parameters::RemoveImageOptions>,
                None,
            )
            .await
            .expect("remove the cached hello-world image");
    }
    let digest =
        f.rt.ensure_image("hello-world")
            .await
            .expect("pull hello-world");
    assert!(digest.contains("sha256:"), "digest {digest}");
    assert!(
        f.rt.docker().inspect_image(image).await.is_ok(),
        "image present after the pull"
    );
    let err =
        f.rt.ensure_image("warren/definitely-missing-image:1")
            .await
            .unwrap_err();
    assert_eq!(err.code, warren_core::errors::CODE_INVALID_REQUEST, "{err}");
}

/// The static gateway (`STATIC_GATEWAY`, or `WARREN_TEST_GATEWAY_BIN`), when it
/// exists and knows the wrap mode; `None` skips with a note.
fn static_gateway() -> Option<PathBuf> {
    let path = std::env::var_os(ENV_TEST_GATEWAY)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(STATIC_GATEWAY));
    if !path.is_file() {
        eprintln!(
            "skipping: no static gateway at {} (run `make gateway` or set {ENV_TEST_GATEWAY})",
            path.display()
        );
        return None;
    }
    let help = std::process::Command::new(&path)
        .arg("--help")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
        })
        .unwrap_or_default();
    if !help.contains("--wrap") {
        eprintln!(
            "skipping: the static gateway at {} has no --wrap mode yet",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// A stand-in for the gateway that checks the injection wiring without the real
/// binary: it records its arguments in the home, answers `200 {"healthy":true}`
/// on the gateway port with busybox `nc`, runs the wrapped command once and then
/// stays alive, which is what the real wrap mode does for a command that exits.
const FAKE_GATEWAY: &str = r#"#!/bin/sh
echo "$@" > "$WARREN_HOME/gateway-args"
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
[ "$1" = "--" ] && shift
(while true; do printf 'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{"healthy":true}' | nc -l -p 3737; done) &
if [ $# -gt 0 ]; then "$@"; echo "child exited with $?" >&2; fi
exec sleep 100000
"#;

fn write_fake_gateway(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("warren-gateway");
    fs::write(&path, FAKE_GATEWAY).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// `GET path` over a raw HTTP/1.1 connection (reqwest is not a dependency of
/// this crate); returns the status code and the body. Retries for a few seconds
/// because the stand-in gateway re-arms its listener between connections.
async fn http_get(ip: &str, port: u16, path: &str) -> Result<(u16, String), String> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: {ip}:{port}\r\nConnection: close\r\n\r\n");
    let mut last = String::new();
    for _ in 0..20 {
        let attempt = tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = TcpStream::connect((ip, port)).await?;
            stream.write_all(request.as_bytes()).await?;
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await?;
            Ok::<_, std::io::Error>(raw)
        })
        .await;
        match attempt {
            Ok(Ok(raw)) if !raw.is_empty() => {
                let text = String::from_utf8_lossy(&raw).into_owned();
                let status = text
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u16>().ok())
                    .ok_or_else(|| format!("no status line in {text:?}"))?;
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                return Ok((status, body));
            }
            Ok(Ok(_)) => last = "empty response".into(),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "timed out".into(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!("GET {path} on {ip}:{port} failed: {last}"))
}

/// Create, inspect, start, exec, probe and recreate an injected workspace instance.
/// `records_args` is true for the stand-in, which writes its argv into the home.
async fn injected_lifecycle(
    f: &Fixture,
    tpl: &Template,
    inst: &mut Instance,
    gateway_bin: &Path,
    records_args: bool,
) -> Result<(), String> {
    let rt = &f.rt;
    rt.create(inst, tpl)
        .await
        .map_err(|e| format!("create: {e}"))?;
    let inspect = rt
        .inspect(&inst.runtime_ref)
        .await
        .map_err(|e| format!("inspect: {e}"))?;
    let config = inspect.config.clone().unwrap_or_default();
    ensure!(
        config.entrypoint
            == Some(strings(&[
                Config::INJECTED_GATEWAY_PATH,
                "--serve",
                "--wrap",
                "--",
                "/bin/sh"
            ])),
        "entrypoint {:?}",
        config.entrypoint
    );
    ensure!(
        config.cmd.as_deref().unwrap_or_default().is_empty(),
        "cmd {:?}",
        config.cmd
    );
    ensure!(
        config.user.as_deref().unwrap_or_default().is_empty(),
        "alpine runs as root and stays so: {:?}",
        config.user
    );
    let labels = config.labels.unwrap_or_default();
    ensure!(
        labels.get(LABEL_INJECTED).map(String::as_str) == Some("true")
            && labels.get(LABEL_SCOPE).map(String::as_str) == Some("workspace"),
        "labels {labels:?}"
    );
    let mounts = inspect.mounts.clone().unwrap_or_default();
    let gateway_mount = mounts
        .iter()
        .find(|m| m.destination.as_deref() == Some(Config::INJECTED_GATEWAY_PATH))
        .ok_or_else(|| format!("no gateway mount in {mounts:?}"))?;
    ensure!(
        gateway_mount.rw == Some(false),
        "gateway mount is read-only: {gateway_mount:?}"
    );
    let canonical = gateway_bin
        .canonicalize()
        .map_err(|e| format!("canonicalize: {e}"))?;
    ensure!(
        gateway_mount.source.as_deref() == canonical.to_str(),
        "gateway mount source {:?}",
        gateway_mount.source
    );
    ensure!(
        mounts
            .iter()
            .any(|m| m.destination.as_deref() == Some("/home") && m.rw == Some(true)),
        "home mounted read-write at /home: {mounts:?}"
    );

    // start waits for the gateway port; the container stays up although
    // alpine's own command (/bin/sh without a terminal) exits at once.
    let info = rt.start(inst).await.map_err(|e| format!("start: {e}"))?;
    ensure!(!info.ip.is_empty(), "ip");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let inspect = rt
        .inspect(&inst.runtime_ref)
        .await
        .map_err(|e| format!("inspect: {e}"))?;
    ensure!(
        crate::container::is_running(&inspect),
        "container stays up; logs: {}",
        rt.tail_logs(&inst.runtime_ref, 20).await
    );

    let res = run(rt, inst, exec("cat /etc/alpine-release")).await?;
    ensure!(
        res.exit_code == 0 && res.stdout.starts_with("3.20."),
        "alpine release {:?} {:?}",
        res.stdout,
        res.stderr
    );

    let (status, body) = http_get(&info.ip, GATEWAY_PORT, "/v1/health").await?;
    ensure!(status == 200, "health status {status}: {body}");
    ensure!(body.contains("\"healthy\":true"), "health body {body:?}");

    if records_args {
        let args = fs::read_to_string(
            Path::new(&inst.storage_path)
                .join(HOME_DIR)
                .join("gateway-args"),
        )
        .map_err(|e| format!("gateway-args: {e}"))?;
        ensure!(
            args.trim() == "--serve --wrap -- /bin/sh",
            "wrapped argv {args:?}"
        );
    }

    // The instance cannot tamper with the injected binary.
    let res = run(
        rt,
        inst,
        exec_as_root(&format!("echo x >> {}", Config::INJECTED_GATEWAY_PATH)),
    )
    .await?;
    ensure!(
        res.exit_code != 0,
        "the gateway binary must be read-only inside the container"
    );

    // recreate goes through create and injects again.
    rt.stop(inst, Duration::from_secs(2))
        .await
        .map_err(|e| format!("stop: {e}"))?;
    rt.recreate(inst, tpl)
        .await
        .map_err(|e| format!("recreate: {e}"))?;
    let config = rt
        .inspect(&inst.runtime_ref)
        .await
        .map_err(|e| format!("inspect: {e}"))?
        .config
        .unwrap_or_default();
    ensure!(
        config
            .labels
            .unwrap_or_default()
            .get(LABEL_INJECTED)
            .map(String::as_str)
            == Some("true"),
        "recreated container is injected"
    );
    ensure!(
        config
            .entrypoint
            .as_ref()
            .and_then(|e| e.first())
            .map(String::as_str)
            == Some(Config::INJECTED_GATEWAY_PATH),
        "recreated entrypoint {:?}",
        config.entrypoint
    );
    let info = rt
        .start(inst)
        .await
        .map_err(|e| format!("start after recreate: {e}"))?;
    let (status, _) = http_get(&info.ip, GATEWAY_PORT, "/v1/health").await?;
    ensure!(status == 200, "health after recreate: {status}");

    rt.destroy(inst, true)
        .await
        .map_err(|e| format!("destroy: {e}"))?;
    ensure!(!Path::new(&inst.storage_path).exists(), "storage removed");
    Ok(())
}

#[tokio::test]
async fn injected_static_gateway_serves_a_workspace_image() {
    require_docker!();
    let Some(gateway) = static_gateway() else {
        return;
    };
    let f = Fixture::with_gateway(Some(gateway.clone())).await;
    let tpl = workspace_template();
    let mut inst = instance(&tpl);
    let outcome = injected_lifecycle(&f, &tpl, &mut inst, &gateway, false).await;
    destroy_quietly(&f.rt, &inst).await;
    outcome.unwrap();
}

#[tokio::test]
async fn injection_wires_a_stand_in_gateway_into_a_workspace_image() {
    require_docker!();
    let fake_dir = scratch_dir("fake-gateway");
    let fake = write_fake_gateway(&fake_dir);
    let f = Fixture::with_gateway(Some(fake.clone())).await;
    let tpl = workspace_template();
    let mut inst = instance(&tpl);
    let outcome = injected_lifecycle(&f, &tpl, &mut inst, &fake, true).await;
    destroy_quietly(&f.rt, &inst).await;
    let _ = fs::remove_dir_all(&fake_dir);
    outcome.unwrap();
}

#[tokio::test]
async fn start_names_gateway_bin_when_a_workspace_image_has_no_gateway() {
    require_docker!();
    let f = Fixture::new().await;
    let tpl = workspace_template();
    let mut inst = instance(&tpl);
    let outcome = async {
        // No gateway binary configured: the image runs unchanged (a warning is
        // logged) and alpine's /bin/sh exits at once.
        f.rt.create(&mut inst, &tpl)
            .await
            .map_err(|e| format!("create: {e}"))?;
        let inspect =
            f.rt.inspect(&inst.runtime_ref)
                .await
                .map_err(|e| format!("inspect: {e}"))?;
        let config = inspect.config.unwrap_or_default();
        ensure!(
            config.entrypoint.unwrap_or_default().is_empty(),
            "image entrypoint untouched"
        );
        ensure!(
            config.cmd == Some(strings(&["/bin/sh"])),
            "image command kept: {:?}",
            config.cmd
        );
        let labels = config.labels.unwrap_or_default();
        ensure!(
            !labels.contains_key(LABEL_INJECTED)
                && labels.get(LABEL_SCOPE).map(String::as_str) == Some("workspace"),
            "labels {labels:?}"
        );
        ensure!(
            inspect.mounts.unwrap_or_default().len() == 1,
            "only the home is mounted"
        );
        let started = Instant::now();
        let err = f.rt.start(&mut inst).await.err().ok_or("start must fail")?;
        ensure!(err.code == CODE_BAD_GATEWAY, "code {}", err.code);
        ensure!(
            err.message.contains("--gateway-bin"),
            "message names the fix: {}",
            err.message
        );
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "took {:?}",
            started.elapsed()
        );
        Ok::<(), String>(())
    }
    .await;
    destroy_quietly(&f.rt, &inst).await;
    outcome.unwrap();
}
