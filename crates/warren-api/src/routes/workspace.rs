//! Workspace, API keys and billing.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use serde::Serialize;
use serde_json::{Value, json};
use warren_core::{
    ApiKey, Error, LedgerEntry, LedgerKind, Rates, Result, Workspace, ids, json::List,
};

use crate::{
    Caller, CreateKeyRequest, Deps, JsonBody, Query, TopUpRequest,
    routes::{created, envelope, newest_first},
    validate,
};

/// `GET /v1/workspace`: the workspace object plus its awake-instance figures.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceView {
    #[serde(flatten)]
    pub workspace: Workspace,
    pub awake_limit: u32,
    pub awake_count: u32,
}

/// `GET /v1/workspace`
pub async fn get_workspace(
    State(d): State<Arc<Deps>>,
    caller: Caller,
) -> Result<Json<WorkspaceView>> {
    let awake_count = d.store.count_awake_instances(caller.workspace_id()).await?;
    let awake_limit = d.biller.awake_limit(&caller.workspace);
    Ok(Json(WorkspaceView {
        workspace: caller.workspace,
        awake_limit,
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

/// `GET /v1/billing`
#[derive(Debug, Clone, Serialize)]
pub struct BillingView {
    pub wallet_micros: i64,
    pub top_up_total_micros: i64,
    pub awake_limit: u32,
    pub awake_count: u32,
    pub rates: Rates,
    pub free_credit_granted: bool,
}

/// `GET /v1/billing`
pub async fn billing(State(d): State<Arc<Deps>>, caller: Caller) -> Result<Json<BillingView>> {
    let ws = &caller.workspace;
    let awake_count = d.store.count_awake_instances(&ws.id).await?;
    Ok(Json(BillingView {
        wallet_micros: ws.wallet_micros,
        top_up_total_micros: ws.top_up_total_micros,
        awake_limit: d.biller.awake_limit(ws),
        awake_count,
        rates: d.biller.rates(),
        free_credit_granted: ws.free_credit_granted,
    }))
}

/// `POST /v1/billing/top-up`: provider "dev" only, and only with `--dev`.
pub async fn top_up(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    JsonBody(req): JsonBody<TopUpRequest>,
) -> Result<Json<Value>> {
    let provider = req
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("dev");
    if provider != "dev" {
        return Err(Error::not_supported(format!(
            "payment provider {provider:?} is not available; only \"dev\" exists in this build"
        )));
    }
    if !d.config.dev_mode {
        return Err(Error::not_supported(
            "dev top-ups are only available when the daemon runs with --dev",
        ));
    }
    let ws = d
        .biller
        .top_up(caller.workspace_id(), req.amount_micros, "dev top-up")
        .await?;
    let entry = topup_entry(&d, &ws, req.amount_micros).await;
    tracing::info!(workspace = %ws.id, micros = req.amount_micros, "dev top-up");
    Ok(Json(json!({ "workspace": ws, "entry": entry })))
}

/// The ledger entry the biller just wrote. The `Biller` trait returns only the
/// workspace, so look the entry up; fall back to a description of the movement
/// if the store did not surface it.
async fn topup_entry(d: &Deps, ws: &Workspace, micros: i64) -> LedgerEntry {
    if let Ok(entries) = d.store.list_ledger(&ws.id, 10).await
        && let Some(e) = entries
            .into_iter()
            .find(|e| e.kind == LedgerKind::Topup && e.micros == micros)
    {
        return e;
    }
    LedgerEntry {
        id: String::new(),
        workspace_id: ws.id.clone(),
        instance_id: String::new(),
        kind: LedgerKind::Topup,
        micros,
        minutes: 0.0,
        at: d.clock.now_unix(),
        note: "dev top-up".into(),
    }
}

/// `GET /v1/billing/ledger?limit=100`
pub async fn ledger(
    State(d): State<Arc<Deps>>,
    caller: Caller,
    q: Query,
) -> Result<Json<List<LedgerEntry>>> {
    let limit: u32 = q.int("limit", 100, 1, 1000)?;
    let mut entries = d.store.list_ledger(caller.workspace_id(), limit).await?;
    newest_first(&mut entries, |e| e.at);
    Ok(envelope(entries))
}
