//! Workspace and API keys.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use serde::Serialize;
use serde_json::{Value, json};
use warren_core::{ApiKey, Error, Result, Workspace, ids, json::List};

use crate::{
    Caller, CreateKeyRequest, Deps, JsonBody,
    routes::{created, envelope, newest_first},
    validate,
};

/// `GET /v1/workspace`: the workspace object plus its awake-instance figures.
/// `awake_limit` is `Config::max_awake_instances`; `null` means unlimited.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceView {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub awake_limit: Option<u32>,
    pub awake_count: u32,
}

/// `GET /v1/workspace`
pub async fn get_workspace(
    State(d): State<Arc<Deps>>,
    caller: Caller,
) -> Result<Json<WorkspaceView>> {
    let awake_count = d.store.count_awake_instances(caller.workspace_id()).await?;
    Ok(Json(WorkspaceView {
        workspace: caller.workspace,
        awake_limit: d.config.max_awake_instances,
        awake_count,
    }))
}

/// `GET /v1/api-keys`
pub async fn list_keys(State(d): State<Arc<Deps>>, caller: Caller) -> Result<Json<List<ApiKey>>> {
    let mut keys = d.store.list_api_keys(caller.workspace_id()).await?;
    newest_first(&mut keys, |k| k.created_at);
    Ok(envelope(keys))
}

/// `POST /v1/api-keys`: the key record and its plaintext, shown once.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedKey {
    pub key: ApiKey,
    pub api_key: String,
}

/// `POST /v1/api-keys`: the plaintext is shown once.
pub async fn create_key(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    JsonBody(req): JsonBody<CreateKeyRequest>,
) -> Result<Response> {
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("key")
        .to_string();
    validate::name("name", &name)?;
    let now = d.clock.now_unix();
    let expires_at = req.expires_at.unwrap_or(0);
    if expires_at < 0 || (expires_at != 0 && expires_at <= now) {
        return Err(Error::invalid(
            "expires_at must be a unix timestamp in the future, or 0 for never",
        ));
    }
    let (plain, hash, prefix) = ids::new_api_key();
    let key = ApiKey {
        id: ids::new_id(),
        workspace_id: caller.workspace.id.clone(),
        name,
        prefix,
        hash,
        created_at: now,
        expires_at,
        last_used_at: 0,
    };
    d.store.create_api_key(&key).await?;
    tracing::info!(workspace = %caller.workspace.id, key = %key.id, "api key created");
    Ok(created(CreatedKey {
        key,
        api_key: plain,
    }))
}

/// `DELETE /v1/api-keys/{id}`; deleting the key in use is allowed.
pub async fn delete_key(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    d.store.delete_api_key(caller.workspace_id(), &id).await?;
    tracing::info!(workspace = %caller.workspace.id, key = %id, "api key deleted");
    Ok(Json(json!({ "id": id, "deleted": true })))
}
