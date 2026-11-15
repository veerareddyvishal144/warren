//! Unauthenticated routes: health, version, signup.

use std::sync::Arc;

use axum::{Extension, Json, extract::State, response::Response};
use serde::Serialize;
use serde_json::{Value, json};
use warren_core::{ApiKey, Error, Result, Workspace, ids};

use crate::{
    ClientIp, Deps, JsonBody, SIGNUPS_PER_HOUR, SignupRequest, ratelimit::RateLimiter,
    routes::created, validate,
};

/// `GET /v1/health`
pub async fn health(State(d): State<Arc<Deps>>) -> Json<Value> {
    let mut runtimes = vec!["docker"];
    if d.config.enable_firecracker {
        runtimes.push("firecracker");
    }
    Json(json!({
        "healthy": true,
        "version": d.config.version,
        "runtimes": runtimes,
        "dev_mode": d.config.dev_mode,
        "time": d.clock.now_unix(),
    }))
}

/// `GET /v1/version`
pub async fn version(State(d): State<Arc<Deps>>) -> Json<Value> {
    Json(json!({ "version": d.config.version }))
}

/// `POST /v1/signup`
#[derive(Debug, Clone, Serialize)]
pub struct SignupResponse {
    pub workspace: Workspace,
    pub api_key: String,
    pub key_id: String,
    pub wallet_micros: i64,
}

/// `POST /v1/signup`: a workspace, its free credit (once) and a first key.
/// `404` when signups are disabled; `429 try_again` past [`SIGNUPS_PER_HOUR`]
/// from one client IP (unknown peers share one bucket; loopback is exempt in
/// dev mode).
pub async fn signup(
    State(d): State<Arc<Deps>>,
    Extension(limiter): Extension<Arc<RateLimiter>>,
    ClientIp(ip): ClientIp,
    JsonBody(req): JsonBody<SignupRequest>,
) -> Result<Response> {
    if !d.config.signup_enabled {
        return Err(Error::not_found("route /v1/signup"));
    }
    let exempt = d.config.dev_mode && ip.is_some_and(|ip| ip.is_loopback());
    if !exempt && !limiter.try_acquire(ip, d.clock.now_unix()) {
        tracing::info!(ip = ?ip, "signup rate limit hit");
        return Err(Error::new(
            warren_core::errors::CODE_TRY_AGAIN,
            429,
            format!("Too many signups from this address; at most {SIGNUPS_PER_HOUR} per hour."),
        ));
    }
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("workspace")
        .to_string();
    validate::name("name", &name)?;
    let now = d.clock.now_unix();
    let mut ws = Workspace {
        id: ids::new_workspace_id(),
        name,
        created_at: now,
        wallet_micros: 0,
        top_up_total_micros: 0,
        free_credit_granted: false,
    };
    d.store.create_workspace(&ws).await?;
    if d.config.free_credit_micros > 0 {
        ws = d
            .biller
            .grant_credit(&ws.id, d.config.free_credit_micros, "welcome credit")
            .await?;
        ws.free_credit_granted = true;
        d.store.update_workspace(&ws).await?;
    }
    let (plain, hash, prefix) = ids::new_api_key();
    let key = ApiKey {
        id: ids::new_id(),
        workspace_id: ws.id.clone(),
        name: "default".into(),
        prefix,
        hash,
        created_at: now,
        expires_at: 0,
        last_used_at: 0,
    };
    d.store.create_api_key(&key).await?;
    tracing::info!(workspace = %ws.id, wallet_micros = ws.wallet_micros, "workspace created");
    Ok(created(SignupResponse {
        wallet_micros: ws.wallet_micros,
        workspace: ws,
        api_key: plain,
        key_id: key.id,
    }))
}
