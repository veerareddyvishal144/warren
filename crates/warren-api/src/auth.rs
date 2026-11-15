//! API-key authentication for the Sandbox API.

use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http::{HeaderMap, header::AUTHORIZATION, request::Parts};
use warren_core::{ApiKey, Error, HEADER_API_KEY, Result, Workspace};

use crate::Deps;

/// The authenticated caller. Inserted into the request extensions by
/// [`require_key`]; handlers receive it as an extractor.
#[derive(Debug, Clone)]
pub struct Caller {
    pub workspace: Workspace,
    pub key: ApiKey,
}

impl Caller {
    /// Id of the caller's workspace.
    pub fn workspace_id(&self) -> &str {
        &self.workspace.id
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self> {
        parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or_else(|| Error::internal("route is not behind the authentication middleware"))
    }
}

/// Middleware: resolve `Authorization: Bearer <key>` or `x-warren-key: <key>`
/// through `Deps::auth` and store the [`Caller`] in the request extensions.
/// Failures answer `401 invalid_api_key` in the JSON error envelope.
pub async fn require_key(State(d): State<Arc<Deps>>, mut req: Request, next: Next) -> Response {
    match authenticate(&d, req.headers()).await {
        Ok(caller) => {
            tracing::debug!(workspace = %caller.workspace.id, key = %caller.key.id, "authenticated");
            req.extensions_mut().insert(caller);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

async fn authenticate(d: &Deps, headers: &HeaderMap) -> Result<Caller> {
    let raw = raw_key(headers)?;
    let (workspace, key) = d.auth.authenticate(&raw).await?;
    Ok(Caller { workspace, key })
}

/// The first non-empty credential: `Authorization` wins over `x-warren-key`.
fn raw_key(headers: &HeaderMap) -> Result<String> {
    let mut saw_garbage = false;
    for name in [AUTHORIZATION.as_str(), HEADER_API_KEY] {
        for value in headers.get_all(name) {
            match value.to_str() {
                Ok(s) if !s.trim().is_empty() => return Ok(s.trim().to_string()),
                Ok(_) => {}
                Err(_) => saw_garbage = true,
            }
        }
    }
    if saw_garbage {
        return Err(Error::invalid_api_key("Invalid API key."));
    }
    Err(Error::invalid_api_key(format!(
        "Missing API key. Send it as a Bearer token in Authorization or in the {HEADER_API_KEY} header."
    )))
}
