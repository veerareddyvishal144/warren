//! The runtime against a mock of Lume's API and a fake in-guest gateway.

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use warren_core::{
    Config, ExecRequest, Instance, InstanceType, LogsOptions, Runtime, Shape, Template,
    errors::{self, CODE_HOST_CAPACITY_REACHED, CODE_INVALID_REQUEST, CODE_NOT_SUPPORTED},
};

use crate::{MacRuntime, filter_since, line_timestamp, share};

#[derive(Clone)]
struct Vm {
    os: String,
    cpu: u32,
    memory: u64,
    disk_total: u64,
    running: bool,
    saved: bool,
}

impl Default for Vm {
    fn default() -> Self {
        Vm {
            os: "macOS".into(),
            cpu: 2,
            memory: 4 << 30,
            disk_total: 50 << 30,
            running: false,
            saved: false,
        }
    }
}

#[derive(Default)]
struct Mock {
    vms: HashMap<String, Vm>,
    max: u32,
    /// Answer `GET /lume/host/status` without the capacity fields, as a server
    /// that does not have them would.
    bare_host_status: bool,
    version: String,
    calls: Vec<String>,
    run_delay_ms: u64,
    location_path: String,
}

type Shared = Arc<Mutex<Mock>>;

fn details(name: &str, vm: &Vm) -> Value {
    json!({
        "name": name, "os": vm.os, "cpuCount": vm.cpu, "memorySize": vm.memory,
        "diskSize": { "allocated": 20u64 << 30, "total": vm.disk_total },
        "display": "1024x768",
        "status": if vm.running { "running" } else { "stopped" },
        "provisioningOperation": null,
        "vncUrl": null,
        "ipAddress": if vm.running { Some("127.0.0.1") } else { None },
        "sshAvailable": vm.running,
        "locationName": "home",
        "sharedDirectories": null,
        "networkMode": "nat",
        "downloadProgress": null,
        "machineIdentifier": format!("id-{name}"),
        "macAddress": "aa:bb:cc:dd:ee:ff",
        "savedState": vm.saved
    })
}

fn not_found(name: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "message": format!("Virtual machine not found: {name}") })),
    )
        .into_response()
}

fn running_macos(m: &Mock) -> u32 {
    m.vms
        .values()
        .filter(|v| v.running && v.os.eq_ignore_ascii_case("macos"))
        .count() as u32
}

async fn host_status(State(s): State<Shared>) -> Json<Value> {
    let m = s.lock().unwrap();
    let running = running_macos(&m);
    if m.bare_host_status {
        return Json(json!({ "status": "healthy", "version": m.version }));
    }
    Json(json!({
        "status": "healthy", "vmCount": running, "maxVMs": m.max,
        "availableSlots": m.max.saturating_sub(running), "version": m.version
    }))
}

async fn config(State(s): State<Shared>) -> Json<Value> {
    let m = s.lock().unwrap();
    Json(json!({
        "defaultLocationName": "home",
        "vmLocations": [{ "name": "home", "path": m.location_path }],
        "homeDirectory": m.location_path, "cacheDirectory": "", "cachingEnabled": false
    }))
}

async fn list(State(s): State<Shared>) -> Json<Value> {
    let m = s.lock().unwrap();
    Json(Value::Array(
        m.vms.iter().map(|(n, v)| details(n, v)).collect(),
    ))
}

async fn get_vm(State(s): State<Shared>, AxPath(name): AxPath<String>) -> axum::response::Response {
    let m = s.lock().unwrap();
    match m.vms.get(&name) {
        Some(v) => Json(details(&name, v)).into_response(),
        None => not_found(&name),
    }
}

async fn clone(State(s): State<Shared>, Json(body): Json<Value>) -> axum::response::Response {
    let mut m = s.lock().unwrap();
    let src = body["name"].as_str().unwrap_or("").to_string();
    let dst = body["newName"].as_str().unwrap_or("").to_string();
    m.calls.push(format!(
        "clone {src} -> {dst} saved={}",
        body["withSavedState"]
    ));
    let Some(v) = m.vms.get(&src).cloned() else {
        return not_found(&src);
    };
    m.vms.insert(
        dst,
        Vm {
            running: false,
            saved: false,
            ..v
        },
    );
    (StatusCode::OK, Json(json!({ "message": "cloned" }))).into_response()
}

async fn run(
    State(s): State<Shared>,
    AxPath(name): AxPath<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let delay = {
        let mut m = s.lock().unwrap();
        m.calls.push(format!(
            "run {name} noDisplay={} vnc={} shares={}",
            body["noDisplay"],
            body["vnc"],
            body["sharedDirectories"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0)
        ));
        if !m.vms.contains_key(&name) {
            return not_found(&name);
        }
        if running_macos(&m) >= m.max {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "message": format!("VM start rejected: host limit of {} running macOS guests reached", m.max) })),
            )
                .into_response();
        }
        m.run_delay_ms
    };
    let s2 = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let mut m = s2.lock().unwrap();
        if let Some(v) = m.vms.get_mut(&name) {
            v.running = true;
            v.saved = false;
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "message": "VM start initiated", "status": "pending" })),
    )
        .into_response()
}

async fn stop(
    State(s): State<Shared>,
    AxPath(name): AxPath<String>,
    body: Option<Json<Value>>,
) -> axum::response::Response {
    let mut m = s.lock().unwrap();
    let b = body.map(|Json(v)| v).unwrap_or(Value::Null);
    m.calls.push(format!(
        "stop {name} force={} timeout={}",
        b["force"], b["timeout"]
    ));
    let Some(v) = m.vms.get_mut(&name) else {
        return not_found(&name);
    };
    v.running = false;
    (
        StatusCode::OK,
        Json(json!({ "message": "VM stopped successfully" })),
    )
        .into_response()
}

async fn suspend(
    State(s): State<Shared>,
    AxPath(name): AxPath<String>,
) -> axum::response::Response {
    let mut m = s.lock().unwrap();
    m.calls.push(format!("suspend {name}"));
    let Some(v) = m.vms.get_mut(&name) else {
        return not_found(&name);
    };
    if !v.running {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": format!("Virtual machine not running: {name}") })),
        )
            .into_response();
    }
    v.running = false;
    v.saved = true;
    (StatusCode::OK, Json(json!({ "message": "VM suspended" }))).into_response()
}

async fn set(
    State(s): State<Shared>,
    AxPath(name): AxPath<String>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let mut m = s.lock().unwrap();
    m.calls.push(format!(
        "set {name} cpu={} memory={} diskSize={}",
        body["cpu"], body["memory"], body["diskSize"]
    ));
    let Some(v) = m.vms.get_mut(&name) else {
        return not_found(&name);
    };
    if let Some(c) = body["cpu"].as_u64() {
        v.cpu = c as u32;
    }
    if let Some(mem) = body["memory"].as_str() {
        v.memory = mem.trim_end_matches("GB").parse::<u64>().unwrap() << 30;
    }
    if let Some(d) = body["diskSize"].as_str() {
        v.disk_total = d.trim_end_matches("GB").parse::<u64>().unwrap() << 30;
    }
    (StatusCode::OK, Json(json!({ "message": "updated" }))).into_response()
}

async fn delete(State(s): State<Shared>, AxPath(name): AxPath<String>) -> axum::response::Response {
    let mut m = s.lock().unwrap();
    m.calls.push(format!("delete {name}"));
    if m.vms.remove(&name).is_none() {
        return not_found(&name);
    }
    (StatusCode::OK, Json(json!({ "message": "deleted" }))).into_response()
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

async fn mock_lume(state: Shared) -> String {
    let app = Router::new()
        .route("/lume/host/status", get(host_status))
        .route("/lume/config", get(config))
        .route("/lume/vms", get(list))
        .route("/lume/vms/clone", post(clone))
        .route("/lume/vms/{name}", get(get_vm).patch(set).delete(delete))
        .route("/lume/vms/{name}/run", post(run))
        .route("/lume/vms/{name}/stop", post(stop))
        .route("/lume/vms/{name}/suspend", post(suspend))
        .with_state(state);
    format!("http://{}", serve(app).await)
}

const TOKEN: &str = "tok-123";

async fn gw_health() -> Json<Value> {
    Json(json!({ "healthy": true, "agent_ready": true, "driver": "echo", "version": "t" }))
}

async fn gw_exec(headers: HeaderMap, Json(req): Json<Value>) -> axum::response::Response {
    if headers
        .get("x-warren-instance-token")
        .and_then(|v| v.to_str().ok())
        != Some(TOKEN)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "code": "invalid_api_key", "message": "bad token" } })),
        )
            .into_response();
    }
    Json(json!({
        "exit_code": 0, "stdout": format!("ran: {}", req["command"].as_str().unwrap_or("")),
        "stderr": "", "duration_ms": 1
    }))
    .into_response()
}

async fn gw_stats(headers: HeaderMap) -> axum::response::Response {
    if headers
        .get("x-warren-instance-token")
        .and_then(|v| v.to_str().ok())
        != Some(TOKEN)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({ "at": 1, "cpu_percent": 12.5, "memory_bytes": 1000, "memory_limit_bytes": 2000, "disk_bytes": 3000 }))
        .into_response()
}

async fn fake_gateway() -> u16 {
    let app = Router::new()
        .route("/v1/health", get(gw_health))
        .route("/v1/exec", post(gw_exec))
        .route("/v1/stats", get(gw_stats));
    serve(app).await.port()
}

struct Env {
    rt: MacRuntime,
    state: Shared,
    root: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn env_with(mock: Mock) -> Env {
    let root = std::env::temp_dir().join(format!(
        "warren-mac-test-{}-{}",
        std::process::id(),
        warren_core::ids::new_id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let bin = root.join("warren-gateway");
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
    let state: Shared = Arc::new(Mutex::new(Mock {
        location_path: root.join("lume").to_string_lossy().into_owned(),
        ..mock
    }));
    let url = mock_lume(state.clone()).await;
    let gw_port = fake_gateway().await;
    let cfg = Config {
        data_dir: root.join("data"),
        gateway_bin: Some(bin),
        mac_base_vm: Some("base".into()),
        lume_api_url: url,
        ..Config::default()
    };
    let rt = MacRuntime::new(cfg)
        .unwrap()
        .with_test_knobs(gw_port, Duration::from_secs(10));
    Env { rt, state, root }
}

fn base_mock() -> Mock {
    let mut vms = HashMap::new();
    vms.insert("base".to_string(), Vm::default());
    Mock {
        vms,
        max: 2,
        version: "0.7.2".into(),
        run_delay_ms: 150,
        ..Mock::default()
    }
}

fn instance(id: &str) -> Instance {
    Instance {
        id: id.into(),
        workspace_id: "ws1".into(),
        template: "warren-macos".into(),
        template_name: "warren-macos".into(),
        template_revision: 1,
        image_ref: "base".into(),
        kind: InstanceType::Mac,
        shape: Shape {
            cpu: 2,
            memory: 4,
            disk: 50,
        },
        mount_path: Config::MAC_HOME.into(),
        default_port: 3737,
        instance_token: TOKEN.into(),
        ..Default::default()
    }
}

fn template() -> Template {
    Template {
        name: "warren-macos".into(),
        image_ref: "base".into(),
        mount_path: Config::MAC_HOME.into(),
        types: vec![InstanceType::Mac],
        env: BTreeMap::from([("WARREN_AGENT_DRIVER".to_string(), "echo".to_string())]),
        ..Default::default()
    }
}

fn calls(e: &Env) -> Vec<String> {
    e.state.lock().unwrap().calls.clone()
}

fn vm(e: &Env, name: &str) -> Option<Vm> {
    e.state.lock().unwrap().vms.get(name).cloned()
}

#[tokio::test]
async fn ready_checks_lume_version_base_vm_and_gateway() {
    let e = env_with(base_mock()).await;
    e.rt.ready().await.unwrap();

    let mut old = base_mock();
    old.version = "0.1.13".into();
    let e = env_with(old).await;
    let err = e.rt.ready().await.unwrap_err();
    assert_eq!(err.code, CODE_NOT_SUPPORTED);
    assert!(err.message.contains("too old"), "{}", err.message);

    let mut nobase = base_mock();
    nobase.vms.clear();
    let e = env_with(nobase).await;
    let err = e.rt.ready().await.unwrap_err();
    assert!(err.message.contains("build-base.sh"), "{}", err.message);

    let mut linux = base_mock();
    linux.vms.get_mut("base").unwrap().os = "linux".into();
    let e = env_with(linux).await;
    let err = e.rt.ready().await.unwrap_err();
    assert!(err.message.contains("not macOS"), "{}", err.message);
}

#[tokio::test]
async fn create_clones_the_base_applies_the_shape_and_writes_the_share() {
    let e = env_with(base_mock()).await;
    let tpl = template();
    let digest = e.rt.ensure_image("base").await.unwrap();
    assert!(digest.starts_with("sha256:"));
    assert!(
        e.rt.ensure_image("missing")
            .await
            .unwrap_err()
            .is_not_found()
    );

    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &tpl).await.unwrap();
    assert_eq!(inst.runtime_ref, "warren-ab12cd34ef");
    assert_eq!(inst.runtime, "lume");
    assert_eq!(inst.shape.disk, 50);
    let share_dir = PathBuf::from(&inst.storage_path);
    assert!(share_dir.ends_with("mac/ab12cd34ef/share"));
    assert!(share_dir.join(share::GATEWAY_FILE).is_file());
    let env = std::fs::read_to_string(share_dir.join(share::ENV_FILE)).unwrap();
    assert!(
        env.contains("export WARREN_INSTANCE_TOKEN='tok-123'"),
        "{env}"
    );
    assert!(env.contains("export WARREN_HOME='/Users/lume'"), "{env}");
    assert!(env.contains("export WARREN_AGENT_DRIVER='echo'"), "{env}");
    let c = calls(&e);
    assert!(
        c.iter()
            .any(|l| l == "clone base -> warren-ab12cd34ef saved=false"),
        "{c:?}"
    );
    assert!(
        !c.iter().any(|l| l.starts_with("set ")),
        "same shape as the base: no resize {c:?}"
    );

    // A bigger shape is applied to the stopped clone; a smaller disk is kept.
    let mut big = instance("bb12cd34ef");
    big.shape = Shape {
        cpu: 4,
        memory: 8,
        disk: 60,
    };
    e.rt.create(&mut big, &tpl).await.unwrap();
    assert_eq!(big.shape.disk, 60);
    let c = calls(&e);
    assert!(
        c.iter()
            .any(|l| l == "set warren-bb12cd34ef cpu=4 memory=\"8GB\" diskSize=\"60GB\""),
        "{c:?}"
    );
    let mut small = instance("cc12cd34ef");
    small.shape.disk = 20;
    e.rt.create(&mut small, &tpl).await.unwrap();
    assert_eq!(
        small.shape.disk, 50,
        "the disk never shrinks below the base image"
    );
}

#[tokio::test]
async fn start_waits_for_the_gateway_then_exec_logs_and_stats_work() {
    let e = env_with(base_mock()).await;
    let mut inst = instance("ab12cd34ef");
    let err = e.rt.start(&mut inst).await.unwrap_err();
    assert_eq!(
        err.code,
        errors::CODE_CONFLICT,
        "no VM yet: {}",
        err.message
    );
    e.rt.create(&mut inst, &template()).await.unwrap();
    assert!(!e.rt.is_running(&inst).await.unwrap());

    let info = e.rt.start(&mut inst).await.unwrap();
    assert_eq!(info.ip, "127.0.0.1");
    assert_eq!(info.runtime_ref, "warren-ab12cd34ef");
    assert!(e.rt.is_running(&inst).await.unwrap());
    let c = calls(&e);
    assert!(
        c.iter()
            .any(|l| l == "run warren-ab12cd34ef noDisplay=true vnc=\"disabled\" shares=1"),
        "{c:?}"
    );

    let out =
        e.rt.exec(
            &inst,
            &ExecRequest {
                command: "sw_vers".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, "ran: sw_vers");

    let mut wrong = inst.clone();
    wrong.instance_token = "nope".into();
    let err =
        e.rt.exec(&wrong, &ExecRequest::default())
            .await
            .unwrap_err();
    assert_eq!(err.status, 401);

    let logs =
        e.rt.logs(&inst, LogsOptions { tail: 50, since: 0 })
            .await
            .unwrap();
    assert!(
        logs.contains("tail -n 50 '/Users/lume/Library/Logs/warren-gateway.log'"),
        "{logs}"
    );

    let stats = e.rt.stats(&inst).await.unwrap();
    assert_eq!(stats.cpu_percent, 12.5);
    assert_eq!(stats.memory_bytes, 1000);
    assert_eq!(
        stats.memory_limit_bytes,
        4 << 30,
        "the shape's memory, not the gateway's number"
    );
    assert_eq!(stats.disk_bytes, 20 << 30, "lume's allocated bytes");
    assert_eq!(e.rt.disk_usage(&inst).await.unwrap(), 20 << 30);

    // Starting again adopts the running VM without another run call.
    let before = calls(&e).len();
    e.rt.start(&mut inst).await.unwrap();
    assert_eq!(calls(&e).len(), before);
}

#[tokio::test]
async fn a_full_host_refuses_with_host_capacity_reached() {
    let mut m = base_mock();
    m.vms.insert(
        "other-a".into(),
        Vm {
            running: true,
            ..Vm::default()
        },
    );
    m.vms.insert(
        "other-b".into(),
        Vm {
            running: true,
            ..Vm::default()
        },
    );
    let e = env_with(m).await;
    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &template()).await.unwrap();
    let err = e.rt.start(&mut inst).await.unwrap_err();
    assert_eq!(err.code, CODE_HOST_CAPACITY_REACHED);
    assert_eq!(err.status, 409);
    assert_eq!(err.details.as_ref().unwrap()["max_running"], 2);
    assert_eq!(err.details.as_ref().unwrap()["running"], 2);
    assert!(
        !calls(&e).iter().any(|l| l.starts_with("run ")),
        "refused before asking lume"
    );
    // Linux guests do not take a slot.
    e.state.lock().unwrap().vms.get_mut("other-b").unwrap().os = "linux".into();
    e.rt.start(&mut inst).await.unwrap();
}

#[tokio::test]
async fn sleep_suspends_and_the_next_start_resumes() {
    let e = env_with(base_mock()).await;
    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &template()).await.unwrap();
    e.rt.start(&mut inst).await.unwrap();

    e.rt.sleep(&inst, Duration::from_secs(5)).await.unwrap();
    let v = vm(&e, "warren-ab12cd34ef").unwrap();
    assert!(!v.running && v.saved, "suspended with saved state");
    assert!(!e.rt.is_running(&inst).await.unwrap());
    assert!(calls(&e).iter().any(|l| l == "suspend warren-ab12cd34ef"));
    // Sleeping again is a no-op, not an error.
    e.rt.sleep(&inst, Duration::from_secs(5)).await.unwrap();

    e.rt.start(&mut inst).await.unwrap();
    let v = vm(&e, "warren-ab12cd34ef").unwrap();
    assert!(v.running && !v.saved);

    // A stop asks lume for a graceful stop with the grace period.
    e.rt.stop(&inst, Duration::from_secs(7)).await.unwrap();
    assert!(
        calls(&e)
            .iter()
            .any(|l| l == "stop warren-ab12cd34ef force=false timeout=7")
    );
    assert!(!vm(&e, "warren-ab12cd34ef").unwrap().running);
    e.rt.stop(&inst, Duration::from_secs(7)).await.unwrap();
}

#[tokio::test]
async fn destroy_removes_the_vm_and_the_share() {
    let e = env_with(base_mock()).await;
    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &template()).await.unwrap();
    e.rt.start(&mut inst).await.unwrap();
    e.rt.destroy(&inst, false).await.unwrap();
    assert!(vm(&e, "warren-ab12cd34ef").is_some(), "storage kept");
    assert!(!vm(&e, "warren-ab12cd34ef").unwrap().running);
    e.rt.destroy(&inst, true).await.unwrap();
    assert!(vm(&e, "warren-ab12cd34ef").is_none());
    assert!(!PathBuf::from(&inst.storage_path).exists());
    // Gone already: fine.
    e.rt.destroy(&inst, true).await.unwrap();
    assert!(
        e.rt.exec(&inst, &ExecRequest::default())
            .await
            .unwrap_err()
            .is_not_found()
    );
}

#[tokio::test]
async fn resize_needs_a_stopped_vm_and_never_shrinks() {
    let e = env_with(base_mock()).await;
    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &template()).await.unwrap();
    e.rt.start(&mut inst).await.unwrap();
    let bigger = Shape {
        cpu: 4,
        memory: 8,
        disk: 50,
    };
    let err = e.rt.resize(&mut inst, bigger).await.unwrap_err();
    assert!(errors::is_requires_restart(&err));
    e.rt.stop(&inst, Duration::from_secs(1)).await.unwrap();
    let err =
        e.rt.resize(
            &mut inst,
            Shape {
                cpu: 2,
                memory: 4,
                disk: 30,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INVALID_REQUEST);
    e.rt.resize(&mut inst, bigger).await.unwrap();
    assert_eq!(inst.shape, bigger);
    let v = vm(&e, "warren-ab12cd34ef").unwrap();
    assert_eq!((v.cpu, v.memory >> 30), (4, 8));
}

#[tokio::test]
async fn recreate_rewrites_the_environment_but_cannot_change_the_base() {
    let e = env_with(base_mock()).await;
    let mut inst = instance("ab12cd34ef");
    let tpl = template();
    e.rt.create(&mut inst, &tpl).await.unwrap();
    let mut other = tpl.clone();
    other.image_ref = "another-base".into();
    let err = e.rt.recreate(&mut inst, &other).await.unwrap_err();
    assert_eq!(err.code, CODE_NOT_SUPPORTED);
    let mut newer = tpl.clone();
    newer
        .env
        .insert("WARREN_MODEL".to_string(), "gpt-x".to_string());
    e.rt.recreate(&mut inst, &newer).await.unwrap();
    let env =
        std::fs::read_to_string(PathBuf::from(&inst.storage_path).join(share::ENV_FILE)).unwrap();
    assert!(env.contains("export WARREN_MODEL='gpt-x'"), "{env}");
    e.rt.start(&mut inst).await.unwrap();
    assert_eq!(
        e.rt.recreate(&mut inst, &newer).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );
}

#[tokio::test]
async fn clone_storage_copies_a_stopped_or_sleeping_vm() {
    let e = env_with(base_mock()).await;
    let mut src = instance("ab12cd34ef");
    e.rt.create(&mut src, &template()).await.unwrap();
    e.rt.start(&mut src).await.unwrap();
    let mut dst = instance("dd12cd34ef");
    let err = e.rt.clone_storage(&src, &mut dst).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_CONFLICT);
    e.rt.sleep(&src, Duration::from_secs(1)).await.unwrap();
    e.rt.clone_storage(&src, &mut dst).await.unwrap();
    assert_eq!(dst.runtime_ref, "warren-dd12cd34ef");
    assert!(
        calls(&e)
            .iter()
            .any(|l| l == "clone warren-ab12cd34ef -> warren-dd12cd34ef saved=false")
    );
    // create() then adopts the copy instead of cloning the base again.
    e.rt.create(&mut dst, &template()).await.unwrap();
    assert_eq!(
        calls(&e)
            .iter()
            .filter(|l| l.starts_with("clone base"))
            .count(),
        1
    );
    e.rt.start(&mut dst).await.unwrap();
}

#[tokio::test]
async fn backups_are_copy_on_write_clones_and_restore_drops_saved_memory() {
    let e = env_with(base_mock()).await;
    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &template()).await.unwrap();
    let vm_dir = e.root.join("lume").join("warren-ab12cd34ef");
    std::fs::create_dir_all(&vm_dir).unwrap();
    std::fs::write(vm_dir.join("disk.img"), b"disk-v1").unwrap();
    std::fs::write(vm_dir.join("nvram.bin"), b"nvram").unwrap();
    std::fs::write(vm_dir.join("config.json"), b"{}").unwrap();

    let dest = e.root.join("backups").join("b1");
    let size = e.rt.backup_storage(&inst, &dest).await.unwrap();
    assert_eq!(
        size,
        5 + 2,
        "only the small files count; the disk clone shares blocks"
    );
    assert_eq!(std::fs::read(dest.join("disk.img")).unwrap(), b"disk-v1");
    assert!(dest.join("nvram.bin").is_file() && dest.join("config.json").is_file());

    std::fs::write(vm_dir.join("disk.img"), b"disk-v2").unwrap();
    std::fs::write(vm_dir.join("state.vzvmsave"), b"memory").unwrap();
    e.rt.restore_storage(&inst, &dest).await.unwrap();
    assert_eq!(std::fs::read(vm_dir.join("disk.img")).unwrap(), b"disk-v1");
    assert!(
        !vm_dir.join("state.vzvmsave").exists(),
        "saved memory cannot match the restored disk"
    );

    e.rt.start(&mut inst).await.unwrap();
    assert_eq!(
        e.rt.backup_storage(&inst, &dest).await.unwrap_err().code,
        errors::CODE_CONFLICT
    );
    assert!(
        e.rt.restore_storage(&inst, &e.root.join("nothing"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn liveness_is_unknown_when_lume_is_down() {
    let e = env_with(base_mock()).await;
    let cfg = Config {
        lume_api_url: "http://127.0.0.1:1".into(),
        ..Config::default()
    };
    let down = MacRuntime::new(cfg).unwrap();
    let inst = instance("ab12cd34ef");
    let err = down.is_running(&inst).await.unwrap_err();
    assert_eq!(err.code, errors::CODE_TRY_AGAIN, "{}", err.message);
    drop(e);
}

#[test]
fn log_lines_are_filtered_by_their_timestamp() {
    assert_eq!(
        line_timestamp("2026-11-19T05:41:14.130943Z  INFO x"),
        Some(1_795_066_874)
    );
    assert_eq!(line_timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(line_timestamp("no timestamp here"), None);
    let text = "2026-11-19T05:41:14Z old\n2026-11-19T05:41:16Z new\nplain line\n";
    assert_eq!(filter_since(text, 0), text);
    assert_eq!(
        filter_since(text, 1_795_066_875),
        "2026-11-19T05:41:16Z new\nplain line\n"
    );
}

#[tokio::test]
async fn capacity_is_counted_from_the_vm_list_when_host_status_lacks_it() {
    let mut m = base_mock();
    m.bare_host_status = true;
    m.vms.insert(
        "other-a".into(),
        Vm {
            running: true,
            ..Vm::default()
        },
    );
    let e = env_with(m).await;
    e.rt.ready().await.unwrap();
    let cap = e.rt.lume().capacity().await.unwrap();
    assert_eq!((cap.max_running, cap.running), (2, 1));
    let mut inst = instance("ab12cd34ef");
    e.rt.create(&mut inst, &template()).await.unwrap();
    e.rt.start(&mut inst).await.unwrap();
    let cap = e.rt.lume().capacity().await.unwrap();
    assert_eq!((cap.max_running, cap.running), (2, 2));
    let mut second = instance("bb12cd34ef");
    e.rt.create(&mut second, &template()).await.unwrap();
    let err = e.rt.start(&mut second).await.unwrap_err();
    assert_eq!(err.code, CODE_HOST_CAPACITY_REACHED);
    assert_eq!(err.details.as_ref().unwrap()["running"], 2);
}
