//! Instance routes: CRUD, lifecycle actions, exec, logs, metrics, signed URLs
//! and backups. The lifecycle owns every state transition; this layer
//! validates, calls it and shapes the answer.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use serde_json::{Value, json};
use warren_core::{
    Backup, CreateInstanceRequest, Error, ExecRequest, ExecResult, ForkRequest, Instance,
    InstanceStatus, LogsOptions, MetricPoint, PatchInstanceRequest, QUERY_TOKEN, Result,
    ShapeRequest, json::List, signer::DEFAULT_TTL_SECS,
};

use crate::{
    Caller, CreateBackupRequest, Deps, JsonBody, Query, RestoreRequest, UpdateInstanceRequest,
    routes::{check_instance_id, created, envelope, newest_first, with_url},
    validate,
};

/// Shortest and longest signed-URL lifetimes in seconds.
pub const MIN_SIGNED_URL_TTL: i64 = 60;
pub const MAX_SIGNED_URL_TTL: i64 = 86_400;
const MAX_SIGNED_PATH_CHARS: usize = 2048;

/// `POST /v1/instances` -> `201` with status `provisioning`.
pub async fn create(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    JsonBody(req): JsonBody<CreateInstanceRequest>,
) -> Result<Response> {
    validate::create_instance(&d.config, &req)?;
    let inst = d.lifecycle.create(&caller.workspace, &req).await?;
    tracing::info!(workspace = %caller.workspace.id, instance = %inst.id, template = %inst.template_name, "instance created");
    Ok(created(with_url(&d.config, inst)))
}

/// `GET /v1/instances`
pub async fn list(State(d): State<Arc<Deps>>, caller: Caller) -> Result<Json<List<Instance>>> {
    let mut items: Vec<Instance> = d
        .lifecycle
        .list(caller.workspace_id())
        .await?
        .into_iter()
        .map(|i| with_url(&d.config, i))
        .collect();
    newest_first(&mut items, |i| i.created_at);
    Ok(envelope(items))
}

/// `GET /v1/instances/{id}`
pub async fn get_one(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Instance>> {
    check_instance_id(&id)?;
    let inst = d.lifecycle.get(caller.workspace_id(), &id).await?;
    Ok(Json(with_url(&d.config, inst)))
}

/// `PATCH /v1/instances/{id}`
pub async fn patch(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<PatchInstanceRequest>,
) -> Result<Json<Instance>> {
    check_instance_id(&id)?;
    validate::patch_instance(&d.config, &req)?;
    let inst = d.lifecycle.patch(caller.workspace_id(), &id, &req).await?;
    Ok(Json(with_url(&d.config, inst)))
}

/// `DELETE /v1/instances/{id}` -> `{ id, status: "deleting" }`
pub async fn delete(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    d.lifecycle.delete(caller.workspace_id(), &id).await?;
    tracing::info!(workspace = %caller.workspace.id, instance = %id, "instance deleted");
    Ok(Json(
        json!({ "id": id, "status": InstanceStatus::Deleting }),
    ))
}

fn status_of(inst: &Instance) -> Json<Value> {
    Json(json!({ "id": inst.id, "status": inst.status }))
}

/// `POST /v1/instances/{id}/stop`
pub async fn stop(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    Ok(status_of(
        &d.lifecycle.stop(caller.workspace_id(), &id).await?,
    ))
}

/// `POST /v1/instances/{id}/start`
pub async fn start(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    Ok(status_of(
        &d.lifecycle.start(caller.workspace_id(), &id).await?,
    ))
}

/// `POST /v1/instances/{id}/sleep`
pub async fn sleep(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    Ok(status_of(
        &d.lifecycle.sleep(caller.workspace_id(), &id).await?,
    ))
}

/// `POST /v1/instances/{id}/restart`
pub async fn restart(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    Ok(status_of(
        &d.lifecycle.restart(caller.workspace_id(), &id).await?,
    ))
}

/// `POST /v1/instances/{id}/resize` -> `{ id, status, resources }`
pub async fn resize(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<ShapeRequest>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    validate::resize(&req)?;
    let inst = d.lifecycle.resize(caller.workspace_id(), &id, &req).await?;
    Ok(Json(
        json!({ "id": inst.id, "status": inst.status, "resources": inst.shape }),
    ))
}

/// `POST /v1/instances/{id}/update` -> `{ id, status, image_ref, template_revision }`
pub async fn update(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<UpdateInstanceRequest>,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    let template = req
        .template
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if let Some(t) = template {
        validate::template_ref(t)?;
    }
    let inst = d
        .lifecycle
        .update(caller.workspace_id(), &id, template)
        .await?;
    Ok(Json(json!({
        "id": inst.id,
        "status": inst.status,
        "image_ref": inst.image_ref,
        "template_revision": inst.template_revision,
    })))
}

/// `POST /v1/instances/{id}/fork` -> `201` new instance
pub async fn fork(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<ForkRequest>,
) -> Result<Response> {
    check_instance_id(&id)?;
    validate::fork(&d.config, &req)?;
    let inst = d.lifecycle.fork(&caller.workspace, &id, &req).await?;
    tracing::info!(workspace = %caller.workspace.id, source = %id, instance = %inst.id, "instance forked");
    Ok(created(with_url(&d.config, inst)))
}

/// `POST /v1/instances/{id}/exec`
pub async fn exec(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<ExecRequest>,
) -> Result<Json<ExecResult>> {
    check_instance_id(&id)?;
    validate::exec(&req)?;
    Ok(Json(
        d.lifecycle.exec(caller.workspace_id(), &id, &req).await?,
    ))
}

/// `GET /v1/instances/{id}/logs?tail=200&since=<unix>`
pub async fn logs(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    q: Query,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    let opts = LogsOptions {
        tail: q.int("tail", 200, 0, 10_000)?,
        since: q.int("since", 0, 0, i64::MAX)?,
    };
    let logs = d.lifecycle.logs(caller.workspace_id(), &id, opts).await?;
    Ok(Json(json!({ "logs": logs })))
}

/// `GET /v1/instances/{id}/metrics?hours=1`
pub async fn metrics(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    q: Query,
) -> Result<Json<List<MetricPoint>>> {
    check_instance_id(&id)?;
    let hours: u32 = q.int("hours", 1, 1, 24 * 30)?;
    Ok(envelope(
        d.lifecycle
            .metrics(caller.workspace_id(), &id, hours)
            .await?,
    ))
}

/// `GET /v1/instances/{id}/signed-url?port=&ttl=&path=`
///
/// `port` 0 (default) means the template's default port and yields the bare
/// instance URL; any other port that is not the default yields the
/// `<id>-<port>` preview URL. The token is signed for port 0 whenever the URL
/// is the bare one so the edge's host check matches.
pub async fn signed_url(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    q: Query,
) -> Result<Json<Value>> {
    check_instance_id(&id)?;
    let inst = d.lifecycle.get(caller.workspace_id(), &id).await?;
    let port: u16 = q.int("port", 0, 0, u16::MAX)?;
    let ttl: i64 = q.int(
        "ttl",
        DEFAULT_TTL_SECS,
        MIN_SIGNED_URL_TTL,
        MAX_SIGNED_URL_TTL,
    )?;
    let path = signed_path(q.get("path"))?;
    let expires_at = d.clock.now_unix() + ttl;
    let explicit_port = port != 0 && port != inst.default_port;
    let token_port = if explicit_port { port } else { 0 };
    let base = if explicit_port {
        d.config.preview_url(&inst.id, port)
    } else {
        d.config.instance_url(&inst.id)
    };
    let token = d.signer.sign(&inst.id, token_port, expires_at);
    let sep = if path.contains('?') { '&' } else { '?' };
    Ok(Json(
        json!({ "url": format!("{base}{path}{sep}{QUERY_TOKEN}={token}"), "expires_at": expires_at }),
    ))
}

/// Normalize the `path` query parameter: default "/", always absolute, no
/// whitespace, control characters or fragments.
fn signed_path(raw: Option<&str>) -> Result<String> {
    let raw = raw.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok("/".to_string());
    }
    if raw.chars().count() > MAX_SIGNED_PATH_CHARS {
        return Err(Error::invalid(format!(
            "path must be at most {MAX_SIGNED_PATH_CHARS} characters"
        )));
    }
    if raw
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '#')
    {
        return Err(Error::invalid(
            "path must not contain whitespace, control characters or '#'",
        ));
    }
    if raw.starts_with('/') {
        Ok(raw.to_string())
    } else {
        Ok(format!("/{raw}"))
    }
}

/// `GET /v1/instances/{id}/backups`
pub async fn list_backups(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<List<Backup>>> {
    check_instance_id(&id)?;
    let mut backups = d.lifecycle.list_backups(caller.workspace_id(), &id).await?;
    newest_first(&mut backups, |b| b.created_at);
    Ok(envelope(backups))
}

/// `POST /v1/instances/{id}/backups` -> `201` backup
pub async fn create_backup(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<CreateBackupRequest>,
) -> Result<Response> {
    check_instance_id(&id)?;
    let note = req.note.unwrap_or_default();
    validate::name("note", &note)?;
    let backup = d
        .lifecycle
        .backup(caller.workspace_id(), &id, &note)
        .await?;
    tracing::info!(workspace = %caller.workspace.id, instance = %id, backup = %backup.id, "backup created");
    Ok(created(backup))
}

/// `POST /v1/instances/{id}/restore` body `{ backup_id }` -> instance
pub async fn restore(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<RestoreRequest>,
) -> Result<Json<Instance>> {
    check_instance_id(&id)?;
    let backup_id = req.backup_id.trim();
    if backup_id.is_empty() {
        return Err(Error::invalid("backup_id is required"));
    }
    let inst = d
        .lifecycle
        .restore(caller.workspace_id(), &id, backup_id)
        .await?;
    tracing::info!(workspace = %caller.workspace.id, instance = %id, backup = %backup_id, "backup restored");
    Ok(Json(with_url(&d.config, inst)))
}
