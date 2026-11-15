//! Template catalog routes.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use serde_json::{Value, json};
use warren_core::{Error, RegisterTemplateRequest, Result, Template, json::List};

use crate::{
    Caller, Deps, JsonBody,
    routes::{created, envelope},
    validate,
};

/// `GET /v1/templates`: the system catalog first, then workspace templates.
pub async fn list(State(d): State<Arc<Deps>>, caller: Caller) -> Result<Json<List<Template>>> {
    let templates = d.templates.list(caller.workspace_id()).await?;
    Ok(envelope(templates))
}

/// `GET /v1/templates/{name}`; `name@revision` pins a workspace revision.
pub async fn get_one(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Json<Template>> {
    if validate::template_ref(&name).is_err() {
        return Err(Error::not_found("template"));
    }
    let tpl = if name.contains('@') {
        d.templates.resolve(caller.workspace_id(), &name).await?
    } else {
        d.templates.get(caller.workspace_id(), &name).await?
    };
    Ok(Json(tpl))
}

/// `POST /v1/templates`: register or publish a new revision.
pub async fn register(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    JsonBody(req): JsonBody<RegisterTemplateRequest>,
) -> Result<Response> {
    validate::register_template(&req)?;
    let tpl = d.templates.register(caller.workspace_id(), &req).await?;
    tracing::info!(workspace = %caller.workspace.id, template = %tpl.name, revision = tpl.revision, "template registered");
    Ok(created(tpl))
}

/// `DELETE /v1/templates/{name}`; `409 conflict` while instances use it.
pub async fn delete(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    if validate::template_ref(&name).is_err() || name.contains('@') {
        return Err(Error::not_found("template"));
    }
    d.templates.delete(caller.workspace_id(), &name).await?;
    tracing::info!(workspace = %caller.workspace.id, template = %name, "template deleted");
    Ok(Json(json!({ "name": name, "deleted": true })))
}
