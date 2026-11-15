//! The route table and the helpers every handler shares.

use std::sync::Arc;

use axum::{
    Json, Router,
    http::{HeaderName, Method, StatusCode, Uri},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use http::header;
use serde::Serialize;
use tower_http::cors::{AllowOrigin, CorsLayer};
use warren_core::{Config, Error, HEADER_API_KEY, Instance, Result, ids, json::List};

use crate::{Deps, auth};

mod crons;
mod instances;
mod system;
mod templates;
mod workspace;

/// Assemble the `/v1` router around the shared dependencies.
pub fn build(state: Arc<Deps>) -> Router {
    let public = Router::new()
        .route("/v1/health", get(system::health))
        .route("/v1/version", get(system::version))
        .route("/v1/signup", post(system::signup));

    let private = Router::new()
        .route("/v1/workspace", get(workspace::get_workspace))
        .route(
            "/v1/api-keys",
            get(workspace::list_keys).post(workspace::create_key),
        )
        .route("/v1/api-keys/{id}", delete(workspace::delete_key))
        .route("/v1/billing", get(workspace::billing))
        .route("/v1/billing/top-up", post(workspace::top_up))
        .route("/v1/billing/ledger", get(workspace::ledger))
        .route(
            "/v1/templates",
            get(templates::list).post(templates::register),
        )
        .route(
            "/v1/templates/{name}",
            get(templates::get_one).delete(templates::delete),
        )
        .route(
            "/v1/instances",
            get(instances::list).post(instances::create),
        )
        .route(
            "/v1/instances/{id}",
            get(instances::get_one)
                .patch(instances::patch)
                .delete(instances::delete),
        )
        .route("/v1/instances/{id}/stop", post(instances::stop))
        .route("/v1/instances/{id}/start", post(instances::start))
        .route("/v1/instances/{id}/sleep", post(instances::sleep))
        .route("/v1/instances/{id}/restart", post(instances::restart))
        .route("/v1/instances/{id}/resize", post(instances::resize))
        .route("/v1/instances/{id}/update", post(instances::update))
        .route("/v1/instances/{id}/fork", post(instances::fork))
        .route("/v1/instances/{id}/exec", post(instances::exec))
        .route("/v1/instances/{id}/logs", get(instances::logs))
        .route("/v1/instances/{id}/metrics", get(instances::metrics))
        .route("/v1/instances/{id}/signed-url", get(instances::signed_url))
        .route(
            "/v1/instances/{id}/backups",
            get(instances::list_backups).post(instances::create_backup),
        )
        .route("/v1/instances/{id}/restore", post(instances::restore))
        .route(
            "/v1/instances/{id}/crons",
            get(crons::list).post(crons::create),
        )
        .route(
            "/v1/instances/{id}/crons/{cid}",
            get(crons::get_one)
                .patch(crons::patch)
                .delete(crons::delete),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_key,
        ));

    Router::new()
        .merge(public)
        .merge(private)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(cors())
        .with_state(state)
}

/// Browser clients (the dashboard on the root host) call the API host
/// cross-origin: mirror the Origin, allow the key headers and credentials.
fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static(HEADER_API_KEY),
        ])
        .allow_credentials(true)
}

async fn not_found(uri: Uri) -> Error {
    Error::not_found(&format!("route {}", uri.path()))
}

async fn method_not_allowed(method: Method, uri: Uri) -> Error {
    Error::new(
        warren_core::errors::CODE_INVALID_REQUEST,
        405,
        format!("{method} is not allowed on {}", uri.path()),
    )
}

/// `201 Created` with a JSON body.
pub(crate) fn created<T: Serialize>(value: T) -> Response {
    (StatusCode::CREATED, Json(value)).into_response()
}

/// Fill the derived `url` field.
pub(crate) fn with_url(cfg: &Config, mut inst: Instance) -> Instance {
    inst.url = cfg.instance_url(&inst.id);
    inst
}

/// Newest first by the given timestamp, keeping the incoming order for ties.
pub(crate) fn newest_first<T>(items: &mut [T], at: impl Fn(&T) -> i64) {
    items.sort_by_key(|i| std::cmp::Reverse(at(i)));
}

/// The `{"data": [...]}` envelope.
pub(crate) fn envelope<T: Serialize>(items: Vec<T>) -> Json<List<T>> {
    Json(List::new(items))
}

/// A malformed id can never match an instance; answer `not_found` without a
/// store round-trip.
pub(crate) fn check_instance_id(id: &str) -> Result<()> {
    if ids::valid_instance_id(id) {
        Ok(())
    } else {
        Err(Error::not_found("instance"))
    }
}
