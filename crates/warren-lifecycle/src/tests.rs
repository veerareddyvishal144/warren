//! Unit tests over in-memory fakes.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use warren_core::{
    Backup, Config, CreateInstanceRequest, Cron, ExecRequest, ForkRequest, GATEWAY_PORT, Instance,
    InstanceStatus, InstanceType, Lifecycle, LogsOptions, MAX_BACKUP_BYTES_PER_WORKSPACE,
    MAX_BACKUPS_PER_INSTANCE, MetricPoint, NetworkMode, PatchInstanceRequest, Runtime, Shape,
    ShapeRequest, Store, Template, Workspace, errors, ids,
};

use crate::{
    Deps, Manager,
    fakes::{FakeClock, FakeRuntime, FakeStore, FakeTemplates},
};

const NOW: i64 = 1_800_000_000;
const WS: &str = "ws_testworkspace00001";
const BASE_SHAPE: Shape = Shape {
    cpu: 2,
    memory: 4,
    disk: 4,
};

struct Env {
    store: Arc<FakeStore>,
    runtime: Arc<FakeRuntime>,
    templates: Arc<FakeTemplates>,
    clock: Arc<FakeClock>,
    mgr: Arc<Manager>,
    ws: Workspace,
    data_dir: PathBuf,
}

impl Env {
    fn new() -> Env {
        Self::with_config(|_| {})
    }

    fn with_config(tweak: impl FnOnce(&mut Config)) -> Env {
        let store = FakeStore::new();
        let runtime = FakeRuntime::new();
        let clock = FakeClock::new(NOW);
        let mut cfg = Config {
            data_dir: std::env::temp_dir().join(format!("warren-lifecycle-test-{}", ids::new_id())),
            ..Default::default()
        };
        tweak(&mut cfg);
        let templates = FakeTemplates::new(cfg.system_templates());
        let mut runtimes: HashMap<InstanceType, Arc<dyn Runtime>> = HashMap::new();
        runtimes.insert(InstanceType::Default, runtime.clone());
        let ws = Workspace {
            id: WS.into(),
            name: "test".into(),
            created_at: NOW,
        };
        store.put_workspace(ws.clone());
        let mgr = Manager::new(Deps {
            store: store.clone(),
            runtimes: Arc::new(runtimes),
            templates: templates.clone(),
            config: cfg.clone(),
            clock: clock.clone(),
        })
        .expect("manager");
        Env {
            store,
            runtime,
            templates,
            clock,
            mgr,
            ws,
            data_dir: cfg.data_dir,
        }
    }

    async fn create(&self, req: CreateInstanceRequest) -> Instance {
        self.mgr.create(&self.ws, &req).await.expect("create")
    }

    async fn create_running(&self) -> Instance {
        let inst = self.create(CreateInstanceRequest::default()).await;
        self.wait_for(&inst.id, InstanceStatus::Running).await
    }

    async fn wait_for(&self, id: &str, status: InstanceStatus) -> Instance {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let inst = self.store.instance(id);
            if inst.status == status {
                return inst;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "instance {id} did not reach {status}; it is {} ({})",
                inst.status,
                inst.status_reason
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Put an instance straight into the store, as if a previous daemon left it.
    fn seed(&self, status: InstanceStatus) -> Instance {
        let id = ids::new_instance_id();
        let inst = Instance {
            id: id.clone(),
            workspace_id: WS.into(),
            template: "warren-base".into(),
            template_name: "warren-base".into(),
            template_revision: 1,
            image_ref: "warren/base:dev".into(),
            status,
            shape: BASE_SHAPE,
            region: "local".into(),
            idle_timeout_seconds: 900,
            default_port: GATEWAY_PORT,
            mount_path: "/home/agent".into(),
            created_at: NOW,
            updated_at: NOW,
            runtime: "fake".into(),
            runtime_ref: format!("ctr-{id}"),
            storage_path: format!("/fake/{id}"),
            instance_token: "token".into(),
            ..Default::default()
        };
        self.store.put_instance(inst.clone());
        inst
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn long(n: usize) -> String {
    "x".repeat(n)
}

fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ------------------------------------------------------------------ create

#[tokio::test]
async fn create_applies_defaults_and_reaches_running() {
    let env = Env::new();
    let inst = env.create(CreateInstanceRequest::default()).await;
    assert_eq!(inst.status, InstanceStatus::Provisioning);
    assert_eq!(inst.template, "warren-base");
    assert_eq!(inst.template_name, "warren-base");
    assert_eq!(inst.template_revision, 1);
    assert_eq!(inst.image_ref, "warren/base:dev");
    assert_eq!(inst.kind, InstanceType::Default);
    assert!(!inst.auto_sleep);
    assert_eq!(inst.idle_timeout_seconds, 900);
    assert_eq!(inst.network, NetworkMode::AllowAll);
    assert_eq!(inst.region, "local");
    assert_eq!(inst.shape, BASE_SHAPE);
    assert_eq!(inst.runtime, "fake");
    assert_eq!(inst.default_port, GATEWAY_PORT);
    assert_eq!(inst.mount_path, "/home/agent");
    assert!(!inst.agent_api);
    assert_eq!(inst.instance_token.len(), 40);
    assert!(ids::valid_instance_id(&inst.id));
    assert_eq!(
        inst.url,
        format!("http://{}.warren.localhost:7337", inst.id)
    );
    assert_eq!((inst.created_at, inst.updated_at), (NOW, NOW));

    let running = env.wait_for(&inst.id, InstanceStatus::Running).await;
    assert_eq!(running.ip, "10.0.0.1");
    assert!(
        running.runtime_ref.starts_with("ctr-"),
        "{}",
        running.runtime_ref
    );
    assert!(!running.storage_path.is_empty());
    assert_eq!(running.started_at, NOW);
    assert_eq!(running.last_activity_at, NOW);
    assert_eq!(running.status_reason, "");
    assert_eq!(env.runtime.ops_for(&inst.id), ["create", "start"]);
    assert!(
        env.runtime
            .calls
            .lock()
            .contains(&"ensure_image:warren/base:dev".to_string())
    );

    let got = env.mgr.get(WS, &inst.id).await.unwrap();
    assert_eq!(got.status, InstanceStatus::Running);
    assert!(
        env.mgr
            .get("ws_other", &inst.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        env.mgr
            .get(WS, "nope123456")
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn create_honours_every_option() {
    let env = Env::new();
    let req = CreateInstanceRequest {
        template: Some("warren-agent".into()),
        shape: Some(ShapeRequest {
            cpu: Some(4),
            memory: Some(8),
            disk: None,
        }),
        kind: Some(InstanceType::Default),
        region: Some("local".into()),
        name: Some("Production agent".into()),
        metadata: Some(env_of(&[("plan", "pro")])),
        env: Some(env_of(&[("APP_ENV", "production")])),
        auto_sleep: Some(true),
        idle_timeout_seconds: Some(600),
        network: Some(NetworkMode::DenyAll),
    };
    let inst = env.create(req).await;
    assert_eq!(inst.template_name, "warren-agent");
    assert_eq!(inst.image_ref, "warren/agent:dev");
    assert!(inst.agent_api);
    assert_eq!(
        inst.shape,
        Shape {
            cpu: 4,
            memory: 8,
            disk: 6
        }
    );
    assert_eq!(inst.name, "Production agent");
    assert_eq!(inst.metadata.get("plan").map(String::as_str), Some("pro"));
    assert_eq!(
        inst.env.get("APP_ENV").map(String::as_str),
        Some("production")
    );
    assert!(inst.auto_sleep);
    assert_eq!(inst.idle_timeout_seconds, 600);
    assert_eq!(inst.network, NetworkMode::DenyAll);
    env.wait_for(&inst.id, InstanceStatus::Running).await;
}

#[tokio::test]
async fn provisioning_failure_marks_failed_with_reason() {
    let env = Env::new();
    env.runtime.fail_start.store(true, Ordering::SeqCst);
    let inst = env.create(CreateInstanceRequest::default()).await;
    let failed = env.wait_for(&inst.id, InstanceStatus::Failed).await;
    assert!(
        failed.status_reason.contains("start failed"),
        "{}",
        failed.status_reason
    );
    assert!(!env.runtime.is_running(&inst.id));

    env.runtime.fail_start.store(false, Ordering::SeqCst);
    env.runtime.fail_ensure_image.store(true, Ordering::SeqCst);
    let inst = env.create(CreateInstanceRequest::default()).await;
    let failed = env.wait_for(&inst.id, InstanceStatus::Failed).await;
    assert!(
        failed.status_reason.contains("image pull failed"),
        "{}",
        failed.status_reason
    );
    assert_eq!(env.runtime.ops_for(&inst.id), Vec::<String>::new());

    // A failed instance can be started once the cause is gone.
    env.runtime.fail_ensure_image.store(false, Ordering::SeqCst);
    let started = env.mgr.start(WS, &inst.id).await.unwrap();
    assert_eq!(started.status, InstanceStatus::Running);
    assert_eq!(started.status_reason, "");
}

#[tokio::test]
async fn create_validation_errors() {
    let env = Env::new();
    let cases: Vec<(&str, CreateInstanceRequest, &str)> = vec![
        (
            "long name",
            CreateInstanceRequest {
                name: Some(long(121)),
                ..Default::default()
            },
            errors::CODE_INVALID_REQUEST,
        ),
        (
            "too much metadata",
            CreateInstanceRequest {
                metadata: Some(
                    (0..33)
                        .map(|i| (format!("k{i}"), "v".to_string()))
                        .collect(),
                ),
                ..Default::default()
            },
            errors::CODE_INVALID_REQUEST,
        ),
        (
            "idle too short",
            CreateInstanceRequest {
                idle_timeout_seconds: Some(299),
                ..Default::default()
            },
            errors::CODE_INVALID_REQUEST,
        ),
        (
            "idle too long",
            CreateInstanceRequest {
                idle_timeout_seconds: Some(86_401),
                ..Default::default()
            },
            errors::CODE_INVALID_REQUEST,
        ),
        (
            "performance",
            CreateInstanceRequest {
                kind: Some(InstanceType::Performance),
                ..Default::default()
            },
            errors::CODE_NOT_SUPPORTED,
        ),
        (
            "no vm runtime",
            CreateInstanceRequest {
                kind: Some(InstanceType::Vm),
                ..Default::default()
            },
            errors::CODE_NOT_SUPPORTED,
        ),
        (
            "unknown template",
            CreateInstanceRequest {
                template: Some("nope".into()),
                ..Default::default()
            },
            errors::CODE_NOT_FOUND,
        ),
        (
            "bad shape",
            CreateInstanceRequest {
                shape: Some(ShapeRequest {
                    cpu: Some(2),
                    memory: Some(8),
                    disk: None,
                }),
                ..Default::default()
            },
            errors::CODE_INVALID_REQUEST,
        ),
        (
            "bad region",
            CreateInstanceRequest {
                region: Some("mars".into()),
                ..Default::default()
            },
            errors::CODE_INVALID_REQUEST,
        ),
    ];
    for (what, req, code) in cases {
        let err = env.mgr.create(&env.ws, &req).await.expect_err(what);
        assert_eq!(err.code, code, "{what}: {err}");
    }

    // A template that only allows vm instances rejects the default type.
    env.templates.put(Template {
        name: "vm-only".into(),
        image_ref: "x/y:z".into(),
        types: vec![InstanceType::Vm],
        default_shape: BASE_SHAPE,
        revision: 1,
        ..Default::default()
    });
    let err = env
        .mgr
        .create(
            &env.ws,
            &CreateInstanceRequest {
                template: Some("vm-only".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INVALID_REQUEST);

    assert!(
        env.mgr.list(WS).await.unwrap().is_empty(),
        "no instance may be created by a rejected request"
    );
    assert!(env.runtime.calls.lock().is_empty());
}

#[tokio::test]
async fn env_key_rules() {
    let env = Env::new();
    let bad: Vec<(&str, BTreeMap<String, String>)> = vec![
        ("lowercase", env_of(&[("lower", "x")])),
        ("digit first", env_of(&[("1BAD", "x")])),
        ("dash", env_of(&[("A-B", "x")])),
        ("reserved", env_of(&[("WARREN_INSTANCE_ID", "x")])),
        ("platform key", env_of(&[("WARREN_INSTANCE_ID", "x")])),
        (
            "too many",
            (0..65)
                .map(|i| (format!("K{i}"), "v".to_string()))
                .collect(),
        ),
        ("value too long", env_of(&[("OK", &long(4097))])),
    ];
    for (what, e) in bad {
        let err = env
            .mgr
            .create(
                &env.ws,
                &CreateInstanceRequest {
                    env: Some(e),
                    ..Default::default()
                },
            )
            .await
            .expect_err(what);
        assert_eq!(err.code, errors::CODE_INVALID_REQUEST, "{what}");
    }
    let good = env_of(&[
        ("OPENAI_API_KEY", "sk"),
        ("_PRIVATE", ""),
        ("A1_B2", &long(4096)),
    ]);
    let inst = env
        .create(CreateInstanceRequest {
            env: Some(good.clone()),
            ..Default::default()
        })
        .await;
    assert_eq!(inst.env, good);
    env.wait_for(&inst.id, InstanceStatus::Running).await;

    let err = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                env: Some(env_of(&[("WARREN_HOME", "/x")])),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INVALID_REQUEST);
    assert_eq!(
        env.store.instance(&inst.id).env,
        good,
        "a rejected patch changes nothing"
    );
}

// ------------------------------------------------------------------ transitions

#[tokio::test]
async fn stop_start_sleep_wake_cycle() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();

    env.clock.advance(100);
    let stopped = env.mgr.stop(WS, &id).await.unwrap();
    assert_eq!(stopped.status, InstanceStatus::Stopped);
    assert_eq!(stopped.stopped_at, NOW + 100);
    assert_eq!(stopped.ip, "");
    assert!(!env.runtime.is_running(&id));
    assert_eq!(
        env.mgr.stop(WS, &id).await.unwrap().status,
        InstanceStatus::Stopped,
        "stop is idempotent"
    );
    assert_eq!(
        env.mgr.ensure_awake(&id).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );

    env.clock.advance(100);
    let started = env.mgr.start(WS, &id).await.unwrap();
    assert_eq!(started.status, InstanceStatus::Running);
    assert_eq!(started.started_at, NOW + 200);
    assert_eq!(started.ip, "10.0.0.2");
    assert!(env.runtime.is_running(&id));
    assert_eq!(
        env.mgr.start(WS, &id).await.unwrap().status,
        InstanceStatus::Running,
        "start is idempotent"
    );

    env.clock.advance(100);
    let sleeping = env.mgr.sleep(WS, &id).await.unwrap();
    assert_eq!(sleeping.status, InstanceStatus::Sleeping);
    assert_eq!(sleeping.slept_at, NOW + 300);
    assert_eq!(sleeping.status_reason, "");
    assert!(!env.runtime.is_running(&id));
    assert_eq!(
        env.mgr.sleep(WS, &id).await.unwrap().status,
        InstanceStatus::Sleeping,
        "sleep is idempotent"
    );

    let woken = env.mgr.ensure_awake(&id).await.unwrap();
    assert_eq!(woken.status, InstanceStatus::Running);
    assert_eq!(woken.ip, "10.0.0.3");
    assert_eq!(
        env.runtime.ops_for(&id),
        ["create", "start", "stop", "start", "stop", "start"]
    );

    // Sleeping instances can also be stopped and started explicitly.
    env.mgr.sleep(WS, &id).await.unwrap();
    assert_eq!(
        env.mgr.start(WS, &id).await.unwrap().status,
        InstanceStatus::Running
    );
    env.mgr.sleep(WS, &id).await.unwrap();
    assert_eq!(
        env.mgr.stop(WS, &id).await.unwrap().status,
        InstanceStatus::Stopped
    );
    // Sleep is only reachable from running.
    assert_eq!(
        env.mgr.sleep(WS, &id).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );
}

#[tokio::test]
async fn start_respects_awake_limit() {
    let env = Env::with_config(|c| c.max_awake_instances = Some(3));
    let inst = env.create_running().await;
    env.mgr.stop(WS, &inst.id).await.unwrap();
    // Three other instances fill the quota; the stopped one cannot come back.
    let fillers: Vec<Instance> = (0..3).map(|_| env.seed(InstanceStatus::Running)).collect();
    let err = env.mgr.start(WS, &inst.id).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_INSTANCE_LIMIT_REACHED);
    assert_eq!(
        err.details,
        Some(serde_json::json!({ "limit": 3, "awake": 3 }))
    );
    assert_eq!(env.store.instance(&inst.id).status, InstanceStatus::Stopped);
    assert_eq!(
        env.runtime.ops_for(&inst.id),
        ["create", "start", "stop"],
        "a refused start touches no runtime"
    );

    // One slot frees up: the start goes through.
    env.store.put_instance(Instance {
        status: InstanceStatus::Stopped,
        ..fillers[0].clone()
    });
    env.mgr.start(WS, &inst.id).await.unwrap();
    env.mgr.sleep(WS, &inst.id).await.unwrap();

    // Full again: neither a wake on request nor an exec may wake it.
    env.store.put_instance(fillers[0].clone());
    let err = env.mgr.ensure_awake(&inst.id).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_INSTANCE_LIMIT_REACHED);
    let err = env
        .mgr
        .exec(
            WS,
            &inst.id,
            &ExecRequest {
                command: "true".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INSTANCE_LIMIT_REACHED);
    assert_eq!(
        env.store.instance(&inst.id).status,
        InstanceStatus::Sleeping,
        "a refused wake leaves the instance sleeping"
    );
    assert!(!env.runtime.is_running(&inst.id));

    // Failed instances do not count, and a restart of a running instance
    // needs no new slot.
    env.store.put_instance(Instance {
        status: InstanceStatus::Failed,
        ..fillers[1].clone()
    });
    let woken = env.mgr.ensure_awake(&inst.id).await.unwrap();
    assert_eq!(woken.status, InstanceStatus::Running);
    assert_eq!(env.store.count_awake_instances(WS).await.unwrap(), 3);
    assert_eq!(
        env.mgr.restart(WS, &inst.id).await.unwrap().status,
        InstanceStatus::Running
    );
}

#[tokio::test]
async fn start_failure_marks_failed() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.mgr.stop(WS, &inst.id).await.unwrap();
    env.runtime.fail_start.store(true, Ordering::SeqCst);
    let err = env.mgr.start(WS, &inst.id).await.unwrap_err();
    assert!(err.is_internal());
    let failed = env.store.instance(&inst.id);
    assert_eq!(failed.status, InstanceStatus::Failed);
    assert!(failed.status_reason.contains("start failed"));
}

#[tokio::test]
async fn ensure_awake_conflicts_and_not_found() {
    let env = Env::new();
    assert!(
        env.mgr
            .ensure_awake("unknown123")
            .await
            .unwrap_err()
            .is_not_found()
    );
    let inst = env.create_running().await;
    assert_eq!(
        env.mgr.ensure_awake(&inst.id).await.unwrap().status,
        InstanceStatus::Running
    );
    env.mgr.stop(WS, &inst.id).await.unwrap();
    let err = env.mgr.ensure_awake(&inst.id).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_CONFLICT);
    assert!(err.message.contains("stopped"), "{}", err.message);

    let failed = env.seed(InstanceStatus::Failed);
    let err = env.mgr.ensure_awake(&failed.id).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_CONFLICT);
    let deleting = env.seed(InstanceStatus::Deleting);
    assert_eq!(
        env.mgr.ensure_awake(&deleting.id).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );
}

#[tokio::test]
async fn ensure_awake_waits_for_a_transitional_instance() {
    let env = Env::new();
    let inst = env.seed(InstanceStatus::Starting);
    let store = env.store.clone();
    let id = inst.id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut i = store.instance(&id);
        i.status = InstanceStatus::Running;
        i.ip = "10.9.9.9".into();
        store.put_instance(i);
    });
    let started = tokio::time::Instant::now();
    let awake = env.mgr.ensure_awake(&inst.id).await.unwrap();
    assert_eq!(awake.status, InstanceStatus::Running);
    assert_eq!(awake.ip, "10.9.9.9");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "polled until the flip"
    );
}

#[tokio::test]
async fn ensure_awake_times_out_on_a_stuck_instance() {
    let env = Env::new();
    assert_eq!(env.mgr.wake_deadline(), Duration::from_secs(90));
    env.mgr.set_wake_deadline(Duration::from_millis(600));
    let inst = env.seed(InstanceStatus::Waking);
    let started = tokio::time::Instant::now();
    let err = env.mgr.ensure_awake(&inst.id).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_TIMEOUT);
    assert!(started.elapsed() >= Duration::from_millis(600));
    assert!(started.elapsed() < Duration::from_secs(5));
    // The same applies to exec, which waits through ensure_awake.
    let err = env
        .mgr
        .exec(
            WS,
            &inst.id,
            &ExecRequest {
                command: "ls".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_TIMEOUT);
}

#[tokio::test]
async fn restart_cycles_the_runtime() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.clock.advance(10);
    let restarted = env.mgr.restart(WS, &inst.id).await.unwrap();
    assert_eq!(restarted.status, InstanceStatus::Running);
    assert_eq!(restarted.started_at, NOW + 10);
    assert_eq!(
        env.runtime.ops_for(&inst.id),
        ["create", "start", "stop", "start"]
    );
    env.mgr.stop(WS, &inst.id).await.unwrap();
    assert_eq!(
        env.mgr.restart(WS, &inst.id).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );
}

#[tokio::test]
async fn transitional_states_reject_control_operations() {
    let env = Env::new();
    let inst = env.seed(InstanceStatus::Starting);
    for (what, res) in [
        ("start", env.mgr.start(WS, &inst.id).await.map(|_| ())),
        ("stop", env.mgr.stop(WS, &inst.id).await.map(|_| ())),
        ("sleep", env.mgr.sleep(WS, &inst.id).await.map(|_| ())),
        ("restart", env.mgr.restart(WS, &inst.id).await.map(|_| ())),
        (
            "resize",
            env.mgr
                .resize(
                    WS,
                    &inst.id,
                    &ShapeRequest {
                        disk: Some(8),
                        ..Default::default()
                    },
                )
                .await
                .map(|_| ()),
        ),
        (
            "update",
            env.mgr.update(WS, &inst.id, None).await.map(|_| ()),
        ),
        (
            "fork",
            env.mgr
                .fork(&env.ws, &inst.id, &ForkRequest::default())
                .await
                .map(|_| ()),
        ),
        ("backup", env.mgr.backup(WS, &inst.id, "").await.map(|_| ())),
    ] {
        assert_eq!(res.unwrap_err().code, errors::CODE_CONFLICT, "{what}");
    }
    assert!(env.runtime.calls.lock().is_empty());
}

// ------------------------------------------------------------------ patch

#[tokio::test]
async fn patch_fields_and_network_change() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.clock.advance(5);
    let patched = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                name: Some("renamed".into()),
                metadata: Some(env_of(&[("a", "b")])),
                env: Some(env_of(&[("NEW", "1")])),
                auto_sleep: Some(true),
                idle_timeout_seconds: Some(1200),
                network: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(patched.name, "renamed");
    assert_eq!(patched.metadata, env_of(&[("a", "b")]));
    assert_eq!(patched.env, env_of(&[("NEW", "1")]));
    assert!(patched.auto_sleep);
    assert_eq!(patched.idle_timeout_seconds, 1200);
    assert_eq!(patched.updated_at, NOW + 5);
    assert_eq!(patched.status, InstanceStatus::Running);
    assert_eq!(
        env.runtime.ops_for(&inst.id),
        ["create", "start"],
        "field patches touch no runtime"
    );

    let same = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                network: Some(NetworkMode::AllowAll),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(same.network, NetworkMode::AllowAll);
    assert_eq!(
        env.runtime.ops_for(&inst.id),
        ["create", "start"],
        "unchanged network is a no-op"
    );

    let denied = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                network: Some(NetworkMode::DenyAll),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(denied.network, NetworkMode::DenyAll);
    assert_eq!(denied.status, InstanceStatus::Running);
    assert_eq!(
        env.runtime.ops_for(&inst.id),
        ["create", "start", "stop", "recreate", "start"]
    );
    assert!(env.runtime.is_running(&inst.id));

    env.mgr.stop(WS, &inst.id).await.unwrap();
    let back = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                network: Some(NetworkMode::AllowAll),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(back.network, NetworkMode::AllowAll);
    assert_eq!(back.status, InstanceStatus::Stopped);
    assert!(
        env.runtime
            .ops_for(&inst.id)
            .ends_with(&["stop".to_string(), "recreate".to_string()])
    );

    let err = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                idle_timeout_seconds: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INVALID_REQUEST);

    // A transitional instance accepts field patches but not a network change.
    let starting = env.seed(InstanceStatus::Starting);
    env.mgr
        .patch(
            WS,
            &starting.id,
            &PatchInstanceRequest {
                name: Some("x".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let err = env
        .mgr
        .patch(
            WS,
            &starting.id,
            &PatchInstanceRequest {
                network: Some(NetworkMode::DenyAll),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_CONFLICT);
}

#[tokio::test]
async fn network_change_failure_marks_failed_and_keeps_old_network() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.runtime.fail_recreate.store(true, Ordering::SeqCst);
    let err = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                network: Some(NetworkMode::DenyAll),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.is_internal());
    let after = env.store.instance(&inst.id);
    assert_eq!(after.status, InstanceStatus::Failed);
    assert_eq!(after.network, NetworkMode::AllowAll);
    assert!(after.status_reason.contains("recreate failed"));
}

// ------------------------------------------------------------------ resize

#[tokio::test]
async fn resize_live_and_with_restart() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();

    let live = env
        .mgr
        .resize(
            WS,
            &id,
            &ShapeRequest {
                cpu: Some(4),
                memory: Some(8),
                disk: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        live.shape,
        Shape {
            cpu: 4,
            memory: 8,
            disk: 6
        }
    );
    assert_eq!(live.status, InstanceStatus::Running);
    assert_eq!(env.runtime.resize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(env.runtime.ops_for(&id), ["create", "start", "resize"]);

    env.runtime
        .resize_requires_restart
        .store(true, Ordering::SeqCst);
    let grown = env
        .mgr
        .resize(
            WS,
            &id,
            &ShapeRequest {
                disk: Some(10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        grown.shape,
        Shape {
            cpu: 4,
            memory: 8,
            disk: 10
        }
    );
    assert_eq!(grown.status, InstanceStatus::Running);
    assert_eq!(env.runtime.resize_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        env.runtime.ops_for(&id),
        [
            "create", "start", "resize", "resize", "stop", "resize", "start"
        ]
    );
    assert!(env.runtime.is_running(&id));
    assert_eq!(env.store.instance(&id).shape.disk, 10);

    let err = env
        .mgr
        .resize(
            WS,
            &id,
            &ShapeRequest {
                disk: Some(8),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        errors::CODE_INVALID_REQUEST,
        "disk may not shrink"
    );
    let err = env
        .mgr
        .resize(
            WS,
            &id,
            &ShapeRequest {
                cpu: Some(3),
                memory: Some(5),
                disk: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INVALID_REQUEST, "unknown shape");

    let same = env
        .mgr
        .resize(WS, &id, &ShapeRequest::default())
        .await
        .unwrap();
    assert_eq!(same.shape, grown.shape);
    assert_eq!(
        env.runtime.resize_calls.load(Ordering::SeqCst),
        3,
        "an unchanged shape calls no runtime"
    );

    // Shrinking cpu/memory keeps the larger current disk instead of the shape default.
    let smaller = env
        .mgr
        .resize(
            WS,
            &id,
            &ShapeRequest {
                cpu: Some(2),
                memory: Some(4),
                disk: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        smaller.shape,
        Shape {
            cpu: 2,
            memory: 4,
            disk: 10
        }
    );

    // A stopped instance resizes in place even when live resizes need a restart.
    env.mgr.stop(WS, &id).await.unwrap();
    let before = env.runtime.resize_calls.load(Ordering::SeqCst);
    let stopped = env
        .mgr
        .resize(
            WS,
            &id,
            &ShapeRequest {
                disk: Some(12),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stopped.shape.disk, 12);
    assert_eq!(stopped.status, InstanceStatus::Stopped);
    assert_eq!(env.runtime.resize_calls.load(Ordering::SeqCst), before + 1);
}

// ------------------------------------------------------------------ update

#[tokio::test]
async fn update_swaps_the_template() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();
    let mut v2 = env.templates.resolve_sync("warren-base");
    v2.revision = 2;
    v2.image_ref = "warren/base:v2".into();
    env.templates.put(v2);

    let updated = env.mgr.update(WS, &id, None).await.unwrap();
    assert_eq!(updated.status, InstanceStatus::Running);
    assert_eq!(updated.image_ref, "warren/base:v2");
    assert_eq!(updated.template_revision, 2);
    assert_eq!(updated.template, "warren-base");
    assert_eq!(
        env.runtime.ops_for(&id),
        ["create", "start", "stop", "recreate", "start"]
    );
    assert!(
        env.runtime
            .calls
            .lock()
            .contains(&"ensure_image:warren/base:v2".to_string())
    );
    assert!(env.runtime.is_running(&id));

    let agent = env.mgr.update(WS, &id, Some("warren-agent")).await.unwrap();
    assert_eq!(agent.template_name, "warren-agent");
    assert_eq!(agent.template, "warren-agent");
    assert!(agent.agent_api);
    assert_eq!(agent.image_ref, "warren/agent:dev");

    let err = env.mgr.update(WS, &id, Some("missing")).await.unwrap_err();
    assert!(err.is_not_found());
    assert_eq!(env.store.instance(&id).template_name, "warren-agent");

    env.mgr.stop(WS, &id).await.unwrap();
    let stopped = env
        .mgr
        .update(WS, &id, Some("warren-base@2"))
        .await
        .unwrap();
    assert_eq!(stopped.status, InstanceStatus::Stopped);
    assert_eq!(stopped.template, "warren-base@2");
    assert_eq!(stopped.image_ref, "warren/base:v2");
    assert!(
        env.runtime
            .ops_for(&id)
            .ends_with(&["recreate".to_string()])
    );
    assert!(!env.runtime.is_running(&id));
}

// ------------------------------------------------------------------ fork

#[tokio::test]
async fn fork_copies_storage_and_wakes_the_source() {
    let env = Env::with_config(|c| c.max_awake_instances = Some(3));
    let src = env
        .create(CreateInstanceRequest {
            env: Some(env_of(&[("KEY", "v")])),
            network: Some(NetworkMode::DenyAll),
            metadata: Some(env_of(&[("team", "a")])),
            ..Default::default()
        })
        .await;
    let src = env.wait_for(&src.id, InstanceStatus::Running).await;
    env.clock.advance(10);

    let dst = env
        .mgr
        .fork(
            &env.ws,
            &src.id,
            &ForkRequest {
                name: Some("copy".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(dst.status, InstanceStatus::Running);
    assert_ne!(dst.id, src.id);
    assert_eq!(dst.parent_id, src.id);
    assert_eq!(dst.name, "copy");
    assert_eq!(dst.env, src.env);
    assert_eq!(dst.network, NetworkMode::DenyAll);
    assert_eq!(dst.metadata, src.metadata);
    assert_eq!(dst.shape, src.shape);
    assert_eq!(dst.kind, src.kind);
    assert_eq!(dst.template_name, src.template_name);
    assert_eq!(dst.image_ref, src.image_ref);
    assert_eq!(dst.instance_token.len(), 40);
    assert_ne!(dst.instance_token, src.instance_token);
    assert!(
        dst.storage_path.contains(&format!("-clone-of-{}", src.id)),
        "{}",
        dst.storage_path
    );
    assert_eq!(dst.created_at, NOW + 10);
    assert_eq!(
        env.runtime.ops_for(&dst.id),
        ["clone_storage", "create", "start"]
    );
    assert_eq!(
        env.runtime.ops_for(&src.id),
        ["create", "start", "stop", "start"],
        "source slept and woke"
    );
    let src_after = env.store.instance(&src.id);
    assert_eq!(src_after.status, InstanceStatus::Running);
    assert_eq!(src_after.status_reason, "");
    assert!(env.runtime.is_running(&dst.id) && env.runtime.is_running(&src.id));

    // A stopped source stays stopped; the fork can take its own shape.
    env.mgr.stop(WS, &src.id).await.unwrap();
    let dst2 = env
        .mgr
        .fork(
            &env.ws,
            &src.id,
            &ForkRequest {
                shape: Some(ShapeRequest {
                    disk: Some(8),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(dst2.status, InstanceStatus::Running);
    assert_eq!(
        dst2.shape,
        Shape {
            cpu: 2,
            memory: 4,
            disk: 8
        }
    );
    assert_eq!(dst2.name, src.name);
    assert_eq!(env.store.instance(&src.id).status, InstanceStatus::Stopped);

    // A full quota refuses the fork before anything is touched.
    env.seed(InstanceStatus::Running);
    assert_eq!(env.store.count_awake_instances(WS).await.unwrap(), 3);
    let err = env
        .mgr
        .fork(&env.ws, &src.id, &ForkRequest::default())
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INSTANCE_LIMIT_REACHED);
    assert_eq!(env.mgr.list(WS).await.unwrap().len(), 4);
    assert_eq!(env.store.instance(&src.id).status, InstanceStatus::Stopped);
    assert_eq!(
        env.runtime.ops_for(&src.id),
        ["create", "start", "stop", "start", "stop"],
        "a refused fork touches no runtime"
    );
}

#[tokio::test]
async fn fork_failure_returns_a_failed_instance() {
    let env = Env::new();
    let src = env.create_running().await;
    env.runtime.fail_create.store(true, Ordering::SeqCst);
    let dst = env
        .mgr
        .fork(&env.ws, &src.id, &ForkRequest::default())
        .await
        .unwrap();
    assert_eq!(dst.status, InstanceStatus::Failed);
    assert!(dst.status_reason.contains("create failed"));
    assert_eq!(env.store.instance(&dst.id).status, InstanceStatus::Failed);
    assert_eq!(
        env.store.instance(&src.id).status,
        InstanceStatus::Running,
        "the source is woken again"
    );
}

// ------------------------------------------------------------------ delete

#[tokio::test]
async fn delete_removes_runtime_backups_and_crons() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();
    let backup = env.mgr.backup(WS, &id, "before delete").await.unwrap();
    assert!(PathBuf::from(&backup.path).exists());
    env.store.put_cron(Cron {
        id: ids::new_id(),
        instance_id: id.clone(),
        workspace_id: WS.into(),
        enabled: true,
        ..Default::default()
    });
    env.store.put_cron(Cron {
        id: ids::new_id(),
        instance_id: id.clone(),
        workspace_id: WS.into(),
        enabled: true,
        ..Default::default()
    });

    env.mgr.delete(WS, &id).await.unwrap();
    assert!(!env.store.has_instance(&id));
    assert!(env.store.backups_for(&id).is_empty());
    assert!(env.store.crons_for(&id).is_empty());
    assert!(!PathBuf::from(&backup.path).exists());
    assert!(!env.data_dir.join("backups").join(&id).exists());
    assert!(
        env.runtime
            .ops_for(&id)
            .ends_with(&["destroy+storage".to_string()])
    );
    assert!(!env.runtime.is_running(&id));
    assert!(env.mgr.delete(WS, &id).await.unwrap_err().is_not_found());
    assert!(env.mgr.get(WS, &id).await.unwrap_err().is_not_found());

    // Any state may be deleted, including failed.
    let failed = env.seed(InstanceStatus::Failed);
    env.mgr.delete(WS, &failed.id).await.unwrap();
    assert!(!env.store.has_instance(&failed.id));
}

#[tokio::test]
async fn delete_failure_keeps_the_record() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.runtime.fail_destroy.store(true, Ordering::SeqCst);
    let err = env.mgr.delete(WS, &inst.id).await.unwrap_err();
    assert!(err.is_internal());
    let after = env.store.instance(&inst.id);
    assert_eq!(after.status, InstanceStatus::Failed);
    assert!(after.status_reason.contains("destroy failed"));
    env.runtime.fail_destroy.store(false, Ordering::SeqCst);
    env.mgr.delete(WS, &inst.id).await.unwrap();
    assert!(!env.store.has_instance(&inst.id));
}

// ------------------------------------------------------------------ exec, logs, metrics

#[tokio::test]
async fn exec_wakes_sleeping_and_conflicts_when_stopped() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.clock.advance(7);
    let out = env
        .mgr
        .exec(
            WS,
            &inst.id,
            &ExecRequest {
                command: "ls".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, "ran: ls");
    assert_eq!(out.exit_code, 0);
    assert_eq!(
        env.mgr.get(WS, &inst.id).await.unwrap().last_activity_at,
        NOW + 7,
        "exec records activity"
    );

    env.mgr.sleep(WS, &inst.id).await.unwrap();
    let out = env
        .mgr
        .exec(
            WS,
            &inst.id,
            &ExecRequest {
                command: "pwd".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, "ran: pwd");
    assert_eq!(env.store.instance(&inst.id).status, InstanceStatus::Running);

    env.mgr.stop(WS, &inst.id).await.unwrap();
    let err = env
        .mgr
        .exec(
            WS,
            &inst.id,
            &ExecRequest {
                command: "ls".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_CONFLICT);
    let err = env
        .mgr
        .exec(
            WS,
            &inst.id,
            &ExecRequest {
                command: "  ".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, errors::CODE_INVALID_REQUEST);
    assert!(
        env.mgr
            .exec(
                "ws_other",
                &inst.id,
                &ExecRequest {
                    command: "ls".into(),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn logs_and_metrics() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();
    let logs = env
        .mgr
        .logs(WS, &id, LogsOptions { tail: 10, since: 0 })
        .await
        .unwrap();
    assert_eq!(logs, format!("logs of {id}"));
    let never = env.seed(InstanceStatus::Failed);
    let mut never_created = env.store.instance(&never.id);
    never_created.runtime_ref.clear();
    env.store.put_instance(never_created);
    assert_eq!(
        env.mgr
            .logs(WS, &never.id, LogsOptions::default())
            .await
            .unwrap(),
        ""
    );

    env.mgr.sample_metrics().await;
    let points = env.mgr.metrics(WS, &id, 1).await.unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].at, NOW);
    assert_eq!(points[0].cpu_percent, 12.5);
    assert_eq!(points[0].memory_bytes, 1024);
    assert_eq!(points[0].disk_bytes, 2048);
    assert!(
        env.store.metrics_for(&never.id).is_empty(),
        "only running instances are sampled"
    );

    env.store.put_metric(
        &id,
        MetricPoint {
            at: NOW - 2 * 86_400,
            ..Default::default()
        },
    );
    env.store.put_metric(
        &id,
        MetricPoint {
            at: NOW - 8 * 86_400,
            ..Default::default()
        },
    );
    assert_eq!(
        env.mgr.metrics(WS, &id, 0).await.unwrap().len(),
        1,
        "hours clamps up to 1"
    );
    let week = env.mgr.metrics(WS, &id, 1000).await.unwrap();
    assert_eq!(week.len(), 2, "hours clamps down to 168");
    assert!(week[0].at > week[1].at, "newest first");
    env.mgr.prune_metrics().await;
    assert_eq!(env.store.metrics_for(&id).len(), 2);
}

// ------------------------------------------------------------------ backups

#[tokio::test]
async fn backup_list_and_restore() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();
    env.clock.advance(3);
    let b = env.mgr.backup(WS, &id, "first").await.unwrap();
    assert_eq!(b.instance_id, id);
    assert_eq!(b.workspace_id, WS);
    assert_eq!(b.note, "first");
    assert_eq!(b.created_at, NOW + 3);
    assert_eq!(b.id.len(), 32);
    let expected = env
        .data_dir
        .join("backups")
        .join(&id)
        .join(format!("{}.tar.gz", b.id));
    assert_eq!(PathBuf::from(&b.path), expected);
    assert!(expected.exists());
    assert_eq!(
        b.size_bytes,
        std::fs::metadata(&expected).unwrap().len() as i64
    );
    assert_eq!(
        env.store.instance(&id).status,
        InstanceStatus::Running,
        "backups do not stop the instance"
    );
    assert_eq!(
        env.runtime.ops_for(&id),
        ["create", "start", "backup_storage"]
    );

    env.clock.advance(1);
    let b2 = env.mgr.backup(WS, &id, "").await.unwrap();
    let list = env.mgr.list_backups(WS, &id).await.unwrap();
    assert_eq!(
        list.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        [b2.id.as_str(), b.id.as_str()],
        "newest first"
    );
    assert_eq!(
        env.mgr.backup(WS, &id, &long(1025)).await.unwrap_err().code,
        errors::CODE_INVALID_REQUEST
    );

    let restored = env.mgr.restore(WS, &id, &b.id).await.unwrap();
    assert_eq!(restored.status, InstanceStatus::Running);
    assert!(env.runtime.ops_for(&id).ends_with(&[
        "stop".to_string(),
        "restore_storage".to_string(),
        "start".to_string()
    ]));
    assert!(
        env.mgr
            .restore(WS, &id, "missing")
            .await
            .unwrap_err()
            .is_not_found()
    );

    env.mgr.stop(WS, &id).await.unwrap();
    let n = env.runtime.calls.lock().len();
    let restored = env.mgr.restore(WS, &id, &b2.id).await.unwrap();
    assert_eq!(restored.status, InstanceStatus::Stopped);
    assert_eq!(
        env.runtime.ops_for(&id).last().map(String::as_str),
        Some("restore_storage")
    );
    assert_eq!(env.runtime.calls.lock().len(), n + 1);

    std::fs::remove_file(&expected).unwrap();
    assert!(
        env.mgr
            .restore(WS, &id, &b.id)
            .await
            .unwrap_err()
            .is_not_found()
    );

    let provisioning = env.seed(InstanceStatus::Provisioning);
    assert_eq!(
        env.mgr
            .backup(WS, &provisioning.id, "")
            .await
            .unwrap_err()
            .code,
        errors::CODE_CONFLICT
    );
}

// ------------------------------------------------------------------ loops

#[tokio::test]
async fn idle_sleeper_triggers_after_the_timeout() {
    let env = Env::new();
    let idle = env
        .create(CreateInstanceRequest {
            auto_sleep: Some(true),
            idle_timeout_seconds: Some(300),
            ..Default::default()
        })
        .await;
    let active = env
        .create(CreateInstanceRequest {
            auto_sleep: Some(true),
            idle_timeout_seconds: Some(300),
            ..Default::default()
        })
        .await;
    let manual = env.create(CreateInstanceRequest::default()).await;
    for i in [&idle, &active, &manual] {
        env.wait_for(&i.id, InstanceStatus::Running).await;
    }

    env.mgr.idle_sweep().await;
    assert_eq!(
        env.store.instance(&idle.id).status,
        InstanceStatus::Running,
        "not idle yet"
    );

    env.clock.advance(300);
    env.mgr.idle_sweep().await;
    assert_eq!(
        env.store.instance(&idle.id).status,
        InstanceStatus::Running,
        "exactly the timeout is not over it"
    );

    env.clock.advance(1);
    env.mgr.record_activity(&active.id);
    env.mgr.idle_sweep().await;
    let slept = env.store.instance(&idle.id);
    assert_eq!(slept.status, InstanceStatus::Sleeping);
    assert_eq!(slept.status_reason, "idle");
    assert_eq!(slept.slept_at, NOW + 301);
    assert!(!env.runtime.is_running(&idle.id));
    assert_eq!(
        env.store.instance(&active.id).status,
        InstanceStatus::Running,
        "recent activity keeps it awake"
    );
    assert_eq!(
        env.store.instance(&manual.id).status,
        InstanceStatus::Running,
        "auto_sleep off"
    );

    // Waking resets the idle timer.
    env.mgr.ensure_awake(&idle.id).await.unwrap();
    env.mgr.idle_sweep().await;
    assert_eq!(env.store.instance(&idle.id).status, InstanceStatus::Running);
    env.clock.advance(301);
    env.mgr.idle_sweep().await;
    assert_eq!(
        env.store.instance(&idle.id).status,
        InstanceStatus::Sleeping
    );
    assert_eq!(
        env.store.instance(&active.id).status,
        InstanceStatus::Sleeping
    );
}

#[tokio::test]
async fn activity_is_overlaid_and_flushed() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.clock.advance(50);
    env.mgr.record_activity(&inst.id);
    assert_eq!(
        env.store.instance(&inst.id).last_activity_at,
        NOW,
        "not stored yet"
    );
    assert_eq!(
        env.mgr.get(WS, &inst.id).await.unwrap().last_activity_at,
        NOW + 50,
        "but visible"
    );
    assert_eq!(
        env.mgr.list(WS).await.unwrap()[0].last_activity_at,
        NOW + 50
    );
    env.mgr.flush_activity().await;
    assert_eq!(env.store.instance(&inst.id).last_activity_at, NOW + 50);
    let updates = env.store.updates.load(Ordering::SeqCst);
    env.mgr.flush_activity().await;
    assert_eq!(
        env.store.updates.load(Ordering::SeqCst),
        updates,
        "nothing pending writes nothing"
    );
    env.mgr.record_activity("gone000000");
    env.mgr.flush_activity().await;
}

#[tokio::test]
async fn disk_usage_refresh_stores_the_runtime_answer() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.runtime.disk_usage.store(4242, Ordering::SeqCst);
    env.mgr.refresh_disk_usage().await;
    assert_eq!(env.store.instance(&inst.id).disk_used_bytes, 4242);
    let provisioning = env.seed(InstanceStatus::Provisioning);
    env.mgr.refresh_disk_usage().await;
    assert_eq!(env.store.instance(&provisioning.id).disk_used_bytes, 0);
}

#[tokio::test]
async fn reconcile_fixes_states_left_by_a_crash() {
    let env = Env::new();
    let transitional = [
        InstanceStatus::Provisioning,
        InstanceStatus::Starting,
        InstanceStatus::Waking,
        InstanceStatus::Restarting,
        InstanceStatus::Updating,
    ]
    .map(|s| env.seed(s));
    let stopping = env.seed(InstanceStatus::Stopping);
    env.runtime.set_running(&stopping.id, true);
    let deleting = env.seed(InstanceStatus::Deleting);
    let running_gone = env.seed(InstanceStatus::Running);
    let running_ok = env.seed(InstanceStatus::Running);
    env.runtime.set_running(&running_ok.id, true);
    let sleeping_leftover = env.seed(InstanceStatus::Sleeping);
    env.runtime.set_running(&sleeping_leftover.id, true);
    let stopped = env.seed(InstanceStatus::Stopped);
    env.clock.advance(60);

    env.mgr.reconcile().await.unwrap();

    for inst in &transitional {
        let after = env.store.instance(&inst.id);
        assert_eq!(after.status, InstanceStatus::Failed, "{}", inst.status);
        assert_eq!(
            after.status_reason,
            format!("daemon restarted during {}", inst.status)
        );
    }
    let after = env.store.instance(&stopping.id);
    assert_eq!(after.status, InstanceStatus::Stopped);
    assert_eq!(after.stopped_at, NOW + 60);
    assert!(
        !env.runtime.is_running(&stopping.id),
        "an interrupted stop is finished"
    );
    assert!(!env.store.has_instance(&deleting.id));
    assert!(
        env.runtime
            .ops_for(&deleting.id)
            .contains(&"destroy+storage".to_string())
    );
    let after = env.store.instance(&running_gone.id);
    assert_eq!(after.status, InstanceStatus::Stopped);
    assert_eq!(after.status_reason, "runtime not running after restart");
    assert_eq!(
        env.store.instance(&running_ok.id).status,
        InstanceStatus::Running
    );
    assert!(env.runtime.is_running(&running_ok.id));
    assert_eq!(
        env.store.instance(&sleeping_leftover.id).status,
        InstanceStatus::Sleeping
    );
    assert!(
        !env.runtime.is_running(&sleeping_leftover.id),
        "a leftover runtime is stopped"
    );
    assert_eq!(
        env.store.instance(&stopped.id).status,
        InstanceStatus::Stopped
    );

    // An adopted running instance is not put to sleep right away by the idle sweeper.
    let mut adopted = env.store.instance(&running_ok.id);
    adopted.auto_sleep = true;
    adopted.idle_timeout_seconds = 300;
    env.store.put_instance(adopted);
    env.mgr.idle_sweep().await;
    assert_eq!(
        env.store.instance(&running_ok.id).status,
        InstanceStatus::Running
    );
}

#[tokio::test]
async fn run_reconciles_then_loops_until_cancelled() {
    let env = Env::new();
    let inst = env
        .create(CreateInstanceRequest {
            auto_sleep: Some(true),
            idle_timeout_seconds: Some(300),
            ..Default::default()
        })
        .await;
    env.wait_for(&inst.id, InstanceStatus::Running).await;
    let stuck = env.seed(InstanceStatus::Starting);
    env.runtime.disk_usage.store(99, Ordering::SeqCst);
    env.clock.advance(301);
    env.mgr.record_activity(&inst.id);

    let token = CancellationToken::new();
    let handle = tokio::spawn(env.mgr.clone().run(token.clone()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run ends on cancel")
        .unwrap()
        .unwrap();

    assert_eq!(
        env.store.instance(&stuck.id).status,
        InstanceStatus::Failed,
        "reconciled at startup"
    );
    let after = env.store.instance(&inst.id);
    assert_eq!(
        after.status,
        InstanceStatus::Running,
        "an adopted instance gets a fresh idle timer"
    );
    assert_eq!(after.last_activity_at, NOW + 301, "activity flush ran");
    assert_eq!(after.disk_used_bytes, 99, "disk refresh ran");
    assert_eq!(
        env.store.metrics_for(&inst.id).len(),
        1,
        "metrics sampler ran once"
    );

    // The adopted instance sleeps once a full idle timeout passes without traffic.
    env.clock.advance(302);
    env.mgr.idle_sweep().await;
    assert_eq!(
        env.store.instance(&inst.id).status,
        InstanceStatus::Sleeping
    );
}

// ------------------------------------------------------------------ misc

#[tokio::test]
async fn endpoint_and_list_order() {
    let env = Env::new();
    let a = env.create_running().await;
    env.clock.advance(1);
    let b = env.create_running().await;
    let list = env.mgr.list(WS).await.unwrap();
    assert_eq!(
        list.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        [b.id.as_str(), a.id.as_str()]
    );
    assert!(list.iter().all(|i| !i.url.is_empty()));
    assert!(env.mgr.list("ws_other").await.unwrap().is_empty());

    assert_eq!(env.mgr.endpoint(&a.id).await.unwrap(), a.ip);
    env.mgr.stop(WS, &a.id).await.unwrap();
    assert_eq!(
        env.mgr.endpoint(&a.id).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );
    assert!(
        env.mgr
            .endpoint("nothere000")
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn operations_on_one_instance_serialize_behind_provisioning() {
    let env = Env::new();
    env.runtime.start_delay_ms.store(60, Ordering::SeqCst);
    let inst = env.create(CreateInstanceRequest::default()).await;
    // Provisioning holds the lock; the patch waits for it and sees the running instance.
    let patched = env
        .mgr
        .patch(
            WS,
            &inst.id,
            &PatchInstanceRequest {
                name: Some("later".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(patched.status, InstanceStatus::Running);
    assert_eq!(patched.name, "later");
    assert_eq!(env.store.instance(&inst.id).name, "later");
    assert_eq!(env.store.instance(&inst.id).status, InstanceStatus::Running);
}

#[tokio::test]
async fn manager_new_validates_deps() {
    let env = Env::new();
    let err = Manager::new(Deps {
        store: env.store.clone(),
        runtimes: Arc::new(HashMap::new()),
        templates: env.templates.clone(),
        config: Config::default(),
        clock: env.clock.clone(),
    })
    .err()
    .expect("no runtimes is an error");
    assert_eq!(err.code, errors::CODE_INVALID_REQUEST);
}

// ------------------------------------------------------------------ detached operations

#[tokio::test]
async fn cancelled_callers_do_not_strand_transitional_states() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();

    // The client goes away while the stop is in flight: the row has already
    // moved to Stopping, and the detached task still finishes the stop.
    env.runtime.stop_delay_ms.store(80, Ordering::SeqCst);
    let dropped = tokio::time::timeout(Duration::from_millis(10), env.mgr.stop(WS, &id)).await;
    assert!(dropped.is_err(), "the caller gave up");
    let stopped = env.wait_for(&id, InstanceStatus::Stopped).await;
    assert!(stopped.ip.is_empty());
    assert!(!env.runtime.is_running(&id));
    env.runtime.stop_delay_ms.store(0, Ordering::SeqCst);

    env.runtime.start_delay_ms.store(80, Ordering::SeqCst);
    let dropped = tokio::time::timeout(Duration::from_millis(10), env.mgr.start(WS, &id)).await;
    assert!(dropped.is_err());
    let running = env.wait_for(&id, InstanceStatus::Running).await;
    assert!(env.runtime.is_running(&id));
    assert!(!running.ip.is_empty());
    env.runtime.start_delay_ms.store(0, Ordering::SeqCst);

    // The same for a wake-on-request the edge stopped waiting for.
    env.mgr.sleep(WS, &id).await.unwrap();
    env.runtime.start_delay_ms.store(80, Ordering::SeqCst);
    let dropped = tokio::time::timeout(Duration::from_millis(10), env.mgr.ensure_awake(&id)).await;
    assert!(dropped.is_err());
    env.wait_for(&id, InstanceStatus::Running).await;
    env.runtime.start_delay_ms.store(0, Ordering::SeqCst);

    // And for a restart.
    env.runtime.stop_delay_ms.store(80, Ordering::SeqCst);
    let dropped = tokio::time::timeout(Duration::from_millis(10), env.mgr.restart(WS, &id)).await;
    assert!(dropped.is_err());
    let running = env.wait_for(&id, InstanceStatus::Running).await;
    assert!(env.runtime.is_running(&id));
    assert!(!running.ip.is_empty());
    env.runtime.stop_delay_ms.store(0, Ordering::SeqCst);
}

// ------------------------------------------------------------------ awake quota

#[tokio::test]
async fn concurrent_creates_and_starts_respect_the_awake_limit() {
    let env = Env::with_config(|c| c.max_awake_instances = Some(3));
    // A pause inside the count widens the window between check and insert.
    env.store.count_delay_ms.store(15, Ordering::SeqCst);
    let req = CreateInstanceRequest::default();
    let results = futures::future::join_all((0..8).map(|_| env.mgr.create(&env.ws, &req))).await;
    let ok: Vec<&Instance> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(ok.len(), 3, "exactly the limit is admitted");
    for err in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(err.code, errors::CODE_INSTANCE_LIMIT_REACHED);
        assert_eq!(
            err.details,
            Some(serde_json::json!({ "limit": 3, "awake": 3 }))
        );
    }
    for inst in &ok {
        env.wait_for(&inst.id, InstanceStatus::Running).await;
    }
    assert_eq!(env.store.count_awake_instances(WS).await.unwrap(), 3);

    // Starts of stopped instances race in the same way.
    for inst in &ok {
        env.mgr.stop(WS, &inst.id).await.unwrap();
    }
    let mut ids: Vec<String> = ok.iter().map(|i| i.id.clone()).collect();
    ids.push(env.seed(InstanceStatus::Stopped).id);
    ids.push(env.seed(InstanceStatus::Stopped).id);
    let results = futures::future::join_all(ids.iter().map(|id| env.mgr.start(WS, id))).await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    for err in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(err.code, errors::CODE_INSTANCE_LIMIT_REACHED);
    }
    assert_eq!(env.store.count_awake_instances(WS).await.unwrap(), 3);
    for id in &ids {
        let status = env.store.instance(id).status;
        assert!(
            matches!(status, InstanceStatus::Running | InstanceStatus::Stopped),
            "{id} is {status}"
        );
    }

    // No configured limit means any number may be awake.
    let unlimited = Env::new();
    let results =
        futures::future::join_all((0..8).map(|_| unlimited.mgr.create(&unlimited.ws, &req))).await;
    assert!(results.iter().all(|r| r.is_ok()));
    for inst in results.iter().flatten() {
        unlimited.wait_for(&inst.id, InstanceStatus::Running).await;
    }
    assert_eq!(unlimited.store.count_awake_instances(WS).await.unwrap(), 8);
}

// ------------------------------------------------------------------ reconcile and repair

#[tokio::test]
async fn reconcile_leaves_rows_alone_when_liveness_is_unknown() {
    let env = Env::new();
    let running = env.seed(InstanceStatus::Running);
    let starting = env.seed(InstanceStatus::Starting);
    env.runtime.liveness_unknown.store(true, Ordering::SeqCst);

    env.mgr.reconcile().await.unwrap();
    env.mgr.repair().await;
    assert_eq!(
        env.store.instance(&running.id).status,
        InstanceStatus::Running
    );
    assert_eq!(
        env.store.instance(&starting.id).status,
        InstanceStatus::Starting
    );

    env.runtime.liveness_unknown.store(false, Ordering::SeqCst);
    env.mgr.reconcile().await.unwrap();
    let after = env.store.instance(&running.id);
    assert_eq!(after.status, InstanceStatus::Stopped);
    assert_eq!(after.status_reason, "runtime not running after restart");
    assert_eq!(
        env.store.instance(&starting.id).status,
        InstanceStatus::Failed
    );
}

#[tokio::test]
async fn repair_pass_fixes_stranded_rows() {
    let env = Env::new();
    let old = NOW - 601;
    let mut stuck = env.seed(InstanceStatus::Starting);
    stuck.updated_at = old;
    env.store.put_instance(stuck.clone());
    env.runtime.set_running(&stuck.id, true);
    let mut stuck_stop = env.seed(InstanceStatus::Stopping);
    stuck_stop.updated_at = old;
    stuck_stop.status_reason = "idle".into();
    env.store.put_instance(stuck_stop.clone());
    env.runtime.set_running(&stuck_stop.id, true);
    let fresh = env.seed(InstanceStatus::Waking);
    let mut failed_live = env.seed(InstanceStatus::Failed);
    failed_live.status_reason = "start failed".into();
    env.store.put_instance(failed_live.clone());
    env.runtime.set_running(&failed_live.id, true);
    let running_gone = env.seed(InstanceStatus::Running);
    let running_ok = env.seed(InstanceStatus::Running);
    env.runtime.set_running(&running_ok.id, true);
    let mut busy = env.seed(InstanceStatus::Starting);
    busy.updated_at = old;
    env.store.put_instance(busy.clone());
    let held = env.mgr.lock(&busy.id).await;
    let mut stale_delete = env.seed(InstanceStatus::Deleting);
    stale_delete.updated_at = old;
    env.store.put_instance(stale_delete.clone());

    env.mgr.repair().await;

    let after = env.store.instance(&stuck.id);
    assert_eq!(after.status, InstanceStatus::Failed);
    assert_eq!(
        after.status_reason,
        "stuck in starting for 601 seconds; runtime stopped"
    );
    assert!(!env.runtime.is_running(&stuck.id));
    let after = env.store.instance(&stuck_stop.id);
    assert_eq!(after.status, InstanceStatus::Stopped);
    assert_eq!(
        after.status_reason,
        "idle; stop interrupted; runtime stopped"
    );
    assert_eq!(after.stopped_at, NOW);
    assert!(!env.runtime.is_running(&stuck_stop.id));
    assert_eq!(
        env.store.instance(&fresh.id).status,
        InstanceStatus::Waking,
        "a recent transition is left to its operation"
    );
    let after = env.store.instance(&failed_live.id);
    assert_eq!(after.status, InstanceStatus::Failed);
    assert_eq!(after.status_reason, "start failed; runtime stopped");
    assert!(!env.runtime.is_running(&failed_live.id));
    let after = env.store.instance(&running_gone.id);
    assert_eq!(after.status, InstanceStatus::Stopped);
    assert_eq!(after.status_reason, "runtime not running");
    assert_eq!(after.stopped_at, NOW);
    assert_eq!(
        env.store.instance(&running_ok.id).status,
        InstanceStatus::Running
    );
    assert_eq!(
        env.store.instance(&busy.id).status,
        InstanceStatus::Starting,
        "a busy instance is skipped"
    );
    assert!(!env.store.has_instance(&stale_delete.id));
    drop(held);

    // A failed instance whose runtime refuses to stop is destroyed instead.
    let mut stubborn = env.seed(InstanceStatus::Failed);
    stubborn.status_reason = "stop failed".into();
    env.store.put_instance(stubborn.clone());
    env.runtime.set_running(&stubborn.id, true);
    env.runtime.fail_stop.store(true, Ordering::SeqCst);
    env.mgr.repair().await;
    let after = env.store.instance(&stubborn.id);
    assert_eq!(after.status_reason, "stop failed; runtime destroyed");
    assert!(!env.runtime.is_running(&stubborn.id));
    assert!(
        env.runtime
            .ops_for(&stubborn.id)
            .ends_with(&["stop".to_string(), "destroy".to_string()])
    );
}

// ------------------------------------------------------------------ forced destroy

#[tokio::test]
async fn failed_stop_force_destroys_the_runtime() {
    let env = Env::new();
    let inst = env.create_running().await;
    env.runtime.fail_stop.store(true, Ordering::SeqCst);
    let err = env.mgr.stop(WS, &inst.id).await.unwrap_err();
    assert!(err.is_internal());
    let after = env.store.instance(&inst.id);
    assert_eq!(after.status, InstanceStatus::Failed);
    assert_eq!(after.status_reason, "stop failed; runtime destroyed");
    assert!(after.ip.is_empty());
    assert!(
        !env.runtime.is_running(&inst.id),
        "nothing keeps running behind a failed row"
    );
    assert!(
        env.runtime
            .ops_for(&inst.id)
            .ends_with(&["stop".to_string(), "destroy".to_string()])
    );

    // When even the forced destroy fails the reason says so.
    let other = env.create_running().await;
    env.runtime.fail_destroy.store(true, Ordering::SeqCst);
    env.mgr.sleep(WS, &other.id).await.unwrap_err();
    let after = env.store.instance(&other.id);
    assert_eq!(after.status, InstanceStatus::Failed);
    assert_eq!(
        after.status_reason,
        "stop failed; forced destroy failed: destroy failed"
    );
    env.runtime.fail_stop.store(false, Ordering::SeqCst);
    env.runtime.fail_destroy.store(false, Ordering::SeqCst);
}

// ------------------------------------------------------------------ resize and the quota

#[tokio::test]
async fn resize_up_needs_no_free_slot() {
    let env = Env::with_config(|c| c.max_awake_instances = Some(1));
    let inst = env.create_running().await;
    assert_eq!(env.store.count_awake_instances(WS).await.unwrap(), 1);

    // The same shape is a no-op.
    env.mgr
        .resize(
            WS,
            &inst.id,
            &ShapeRequest {
                cpu: Some(2),
                memory: Some(4),
                disk: Some(4),
            },
        )
        .await
        .unwrap();
    assert_eq!(env.runtime.resize_calls.load(Ordering::SeqCst), 0);

    // Growing an awake instance at the quota is allowed: it holds its slot.
    let bigger = ShapeRequest {
        cpu: Some(4),
        memory: Some(8),
        disk: None,
    };
    let grown = env.mgr.resize(WS, &inst.id, &bigger).await.unwrap();
    assert_eq!(
        grown.shape,
        Shape {
            cpu: 4,
            memory: 8,
            disk: 6
        }
    );
    assert_eq!(grown.status, InstanceStatus::Running);
    assert_eq!(env.runtime.resize_calls.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------------------ backup caps

#[tokio::test]
async fn backups_are_capped_per_instance_and_per_workspace() {
    let env = Env::new();
    let inst = env.create_running().await;
    let id = inst.id.clone();
    let dir = env.data_dir.join("backups").join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut seeded = Vec::new();
    for i in 0..MAX_BACKUPS_PER_INSTANCE {
        let bid = ids::new_id();
        let path = dir.join(format!("{bid}.tar.gz"));
        std::fs::write(&path, b"old").unwrap();
        env.store.put_backup(Backup {
            id: bid.clone(),
            instance_id: id.clone(),
            workspace_id: WS.into(),
            path: path.to_string_lossy().into_owned(),
            size_bytes: 3,
            created_at: NOW - 1000 + i as i64,
            note: String::new(),
        });
        seeded.push((bid, path));
    }
    env.clock.advance(5);
    let b = env.mgr.backup(WS, &id, "newest").await.unwrap();
    let list = env.mgr.list_backups(WS, &id).await.unwrap();
    assert_eq!(list.len(), MAX_BACKUPS_PER_INSTANCE);
    assert_eq!(list[0].id, b.id, "newest first");
    let (oldest_id, oldest_path) = &seeded[0];
    assert!(
        list.iter().all(|x| &x.id != oldest_id),
        "the oldest row is gone"
    );
    assert!(!oldest_path.exists(), "and its file");
    assert!(seeded[1].1.exists(), "the next one stays");
    assert!(PathBuf::from(&b.path).exists());

    // Workspace quota: another instance's backups nearly fill it, so the next
    // archive would exceed it and is removed again.
    let other = env.create_running().await;
    env.store.put_backup(Backup {
        id: ids::new_id(),
        instance_id: other.id.clone(),
        workspace_id: WS.into(),
        path: String::new(),
        size_bytes: MAX_BACKUP_BYTES_PER_WORKSPACE as i64 - 10,
        created_at: NOW,
        note: String::new(),
    });
    let files_before = std::fs::read_dir(&dir).unwrap().count();
    let err = env.mgr.backup(WS, &id, "").await.unwrap_err();
    assert_eq!(err.code, errors::CODE_CONFLICT);
    assert_eq!(err.message, "backup storage quota reached");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        files_before,
        "the over-quota archive is removed"
    );
    assert_eq!(
        env.mgr.list_backups(WS, &id).await.unwrap().len(),
        MAX_BACKUPS_PER_INSTANCE
    );
    assert_eq!(
        env.mgr.backup(WS, &other.id, "").await.unwrap_err().code,
        errors::CODE_CONFLICT,
        "every instance of the workspace shares the quota"
    );
}

// ------------------------------------------------------------------ credentials

#[tokio::test]
async fn gateway_credentials_need_a_running_instance() {
    let env = Env::new();
    let inst = env.create_running().await;
    let (ip, token) = env.mgr.gateway_credentials(&inst.id).await.unwrap();
    assert_eq!(ip, "10.0.0.1");
    assert_eq!(token, inst.instance_token);
    assert!(
        env.mgr
            .gateway_credentials("unknown999")
            .await
            .unwrap_err()
            .is_not_found()
    );

    env.mgr.sleep(WS, &inst.id).await.unwrap();
    assert_eq!(
        env.mgr
            .gateway_credentials(&inst.id)
            .await
            .unwrap_err()
            .code,
        errors::CODE_CONFLICT
    );

    let woken = env.mgr.ensure_awake(&inst.id).await.unwrap();
    assert_eq!(woken.status, InstanceStatus::Running);
    let (ip, token) = env.mgr.gateway_credentials(&inst.id).await.unwrap();
    assert_eq!(ip, woken.ip);
    assert_eq!(token, inst.instance_token);
}

/// A full host is not a failure of the instance: the row goes back to
/// `stopped` (or `sleeping` for a wake) with the refusal as its reason, and
/// the next attempt simply tries again.
#[tokio::test]
async fn host_capacity_refusals_do_not_fail_the_instance() {
    let env = Env::new();
    env.runtime
        .refuse_start_capacity
        .store(true, Ordering::SeqCst);
    let created = env.create(CreateInstanceRequest::default()).await;
    let inst = env.wait_for(&created.id, InstanceStatus::Stopped).await;
    assert!(
        inst.status_reason.contains("at most 2"),
        "reason carries the refusal: {}",
        inst.status_reason
    );
    assert!(inst.ip.is_empty());
    env.runtime
        .refuse_start_capacity
        .store(false, Ordering::SeqCst);
    let inst = env.mgr.start(WS, &inst.id).await.unwrap();
    assert_eq!(inst.status, InstanceStatus::Running);

    env.mgr.sleep(WS, &inst.id).await.unwrap();
    env.runtime
        .refuse_start_capacity
        .store(true, Ordering::SeqCst);
    let err = env.mgr.start(WS, &inst.id).await.unwrap_err();
    assert_eq!(err.code, warren_core::errors::CODE_HOST_CAPACITY_REACHED);
    assert_eq!(err.status, 409);
    let inst = env.store.instance(&inst.id);
    assert_eq!(
        inst.status,
        InstanceStatus::Sleeping,
        "{}",
        inst.status_reason
    );
    env.runtime
        .refuse_start_capacity
        .store(false, Ordering::SeqCst);
    assert_eq!(
        env.mgr.start(WS, &inst.id).await.unwrap().status,
        InstanceStatus::Running
    );
}
