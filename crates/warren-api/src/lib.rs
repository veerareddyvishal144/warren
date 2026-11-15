//! The Sandbox API under `/v1`.
//!
//! A thin layer over the core traits: authenticate the caller, validate the
//! request, call `Lifecycle` / `Biller` / `TemplateService` / `Store`, write
//! JSON. Every route of `docs/API.md` is built by [`router`]; the edge mounts
//! the returned router on the API host (paths keep their `/v1` prefix).
//!
//! Conventions implemented here:
//! - `Authorization: Bearer sk_live_...` or `x-warren-key` is resolved through
//!   `Deps::auth` by a middleware and exposed to handlers as [`Caller`];
//!   `/v1/health`, `/v1/version` and `POST /v1/signup` need no key.
//! - JSON bodies are read by [`JsonBody`], bounded by
//!   `warren_core::json::MAX_BODY_BYTES`; unknown fields and type errors are
//!   answered with `400 invalid_request` and the serde message.
//! - Lists are `{"data": [...]}` (`warren_core::json::List`), newest first.
//! - Every instance returned carries `url = config.instance_url(id)`.

use std::sync::Arc;

use axum::Router;
use warren_core::{
    Authenticator, Biller, Clock, Config, Lifecycle, Signer, Store, TemplateService,
};

mod auth;
mod extract;
mod requests;
mod routes;
mod schedule;
mod validate;

#[cfg(test)]
mod tests;

pub use auth::Caller;
pub use extract::{JsonBody, Query};
pub use requests::{
    CreateBackupRequest, CreateCronRequest, CreateKeyRequest, PatchCronRequest, RestoreRequest,
    SignupRequest, TopUpRequest, UpdateInstanceRequest,
};
pub use schedule::{MAX_PROMPT_BYTES, next_run_at, parse_schedule, parse_timezone};

/// Everything the routes need. Wrapped in an `Arc` and used as axum state.
pub struct Deps {
    pub store: Arc<dyn Store>,
    pub lifecycle: Arc<dyn Lifecycle>,
    pub biller: Arc<dyn Biller>,
    pub templates: Arc<dyn TemplateService>,
    pub auth: Arc<dyn Authenticator>,
    pub signer: Arc<dyn Signer>,
    pub config: Config,
    pub clock: Arc<dyn Clock>,
}

/// Every `/v1` route, with CORS, authentication and JSON fallbacks for unknown
/// paths and methods. The result is a `Router<()>` ready to serve or to nest.
pub fn router(deps: Deps) -> Router {
    routes::build(Arc::new(deps))
}
