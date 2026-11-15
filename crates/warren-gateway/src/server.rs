//! The axum application: shared state, routes and the serve loop.

use std::{future::Future, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as UrlPath, Request, State},
    http::Method,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use warren_core::{
    Clock, Error, RealClock, Result,
    agentapi::{ModelInfo, SessionDetail, SessionSummary},
    json::List,
    types::{ENV_INSTANCE_TOKEN, HEADER_INSTANCE_TOKEN},
};

use crate::{
    config::GatewayConfig,
    drivers, exec,
    extract::{JsonBody, parse_json, require_instance_token},
    files::{self, Home},
    health,
    owner::HomeOwner,
    responses,
    sessions::SessionStore,
    terminal, wrap,
};

/// After the stop signal (and, in wrap mode, after the wrapped program is
/// gone) in-flight requests get this long to finish before the process exits.
pub const DRAIN: Duration = Duration::from_secs(1);

/// State shared by every handler.
pub struct AppState {
    pub config: GatewayConfig,
    /// Canonical home directory.
    pub home: PathBuf,
    /// Who owns the home: created files belong to it and commands run as it.
    pub owner: HomeOwner,
    pub files: Home,
    pub sessions: Arc<SessionStore>,
    pub responses: Arc<responses::ResponseManager>,
    pub clock: Arc<dyn Clock>,
    /// The image's own program in wrap mode (`--wrap -- <argv...>`), else `None`.
    pub wrapped: Option<Arc<wrap::Supervisor>>,
}

/// `Arc<AppState>` as used by axum.
pub type SharedState = Arc<AppState>;

/// Build the shared state: create the home and state directories and the
/// configured driver. A misconfigured driver disables the Agent API instead
/// of failing startup, so files, exec and health keep working.
pub fn build_state(config: GatewayConfig) -> Result<SharedState> {
    let files = Home::open(&config.home)?;
    let home = files.root().to_path_buf();
    let owner = files.owner().clone();
    tracing::info!(
        home = %home.display(),
        owner = %owner.name,
        uid = owner.uid,
        gid = owner.gid,
        switching = owner.switches(),
        "home owner resolved"
    );
    if owner.process_uid == 0 && owner.uid == 0 {
        tracing::warn!(
            home = %home.display(),
            "home is owned by root; agent commands will run as root"
        );
    }
    if config.instance_token.is_empty() {
        tracing::warn!(
            "{ENV_INSTANCE_TOKEN} is empty; every route answers without an instance token"
        );
    }
    let state_dir = config.state_dir();
    // A home the gateway cannot write (a foreign image running as another
    // user, a read-only mount) must not take health, exec and the wrapped
    // program down with it; sessions and responses report the error later.
    if let Err(e) = owner.create_dir_all_owned(&state_dir) {
        tracing::warn!(
            dir = %state_dir.display(),
            error = %e,
            "cannot create the state directory; sessions and responses will fail"
        );
    }
    let clock: Arc<dyn Clock> = Arc::new(RealClock);
    let sessions = Arc::new(SessionStore::new(&state_dir, clock.clone(), owner.clone()));
    let (driver, disabled_reason) = match drivers::build(&config) {
        Ok(d) => (d, String::new()),
        Err(e) => {
            tracing::error!(driver = %config.driver, error = %e, "agent driver misconfigured; Agent API disabled");
            (None, e.message)
        }
    };
    let responses = Arc::new(responses::ResponseManager::new(
        &state_dir,
        home.clone(),
        owner.clone(),
        sessions.clone(),
        driver,
        disabled_reason,
        clock.clone(),
    ));
    let wrapped = match &config.wrap {
        Some(spec) if !spec.argv.is_empty() => {
            Some(Arc::new(wrap::Supervisor::new(spec.clone(), clock.clone())))
        }
        Some(_) => {
            tracing::warn!("wrap mode without a command; serving only");
            None
        }
        None => None,
    };
    Ok(Arc::new(AppState {
        config,
        home,
        owner,
        files,
        sessions,
        responses,
        clock,
        wrapped,
    }))
}

/// Routes that answer without the instance token: `GET /v1/health` and
/// `GET /v1/version` (and their `HEAD`s), matched on the exact request path.
/// Everything else, the fallback included, goes through [`require_token`].
pub fn is_open_route(method: &Method, path: &str) -> bool {
    matches!(method, &Method::GET | &Method::HEAD) && matches!(path, "/v1/health" | "/v1/version")
}

/// Middleware over the whole router: when the gateway has an instance token,
/// every request outside [`is_open_route`] must carry it in
/// `x-warren-instance-token`, else `401 invalid_api_key`. Applied before
/// routing, so no spelling of a path (`//v1/files`, `/v1/./files`, unknown
/// routes) reaches a handler without the token.
pub async fn require_token(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    if !is_open_route(req.method(), req.uri().path())
        && let Err(e) = require_instance_token(req.headers(), &state.config.instance_token)
    {
        tracing::debug!(
            method = %req.method(),
            path = req.uri().path(),
            "request without a valid {HEADER_INSTANCE_TOKEN}"
        );
        return e.into_response();
    }
    next.run(req).await
}

/// The router with every route of the Agent API, behind [`require_token`].
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(health::root_handler))
        .route("/v1/health", get(health::health_handler))
        .route("/v1/version", get(health::version_handler))
        .route("/v1/responses", post(responses::create_handler))
        .route("/v1/responses/{id}", get(responses::get_handler))
        .route("/v1/responses/{id}/stream", get(responses::stream_handler))
        .route("/v1/responses/{id}/cancel", post(responses::cancel_handler))
        .route("/v1/sessions", get(list_sessions))
        .route(
            "/v1/sessions/{id}",
            get(get_session).patch(patch_session).delete(delete_session),
        )
        .route("/v1/models", get(list_models))
        .route(
            "/v1/files",
            get(files::list_handler).delete(files::delete_handler),
        )
        .route(
            "/v1/files/content",
            get(files::content_get_handler).put(files::content_put_handler),
        )
        .route("/v1/files/move", post(files::move_handler))
        .route("/v1/files/download", get(files::download_handler))
        .route("/v1/exec", post(exec::exec_handler))
        .route("/v1/shutdown", post(shutdown_handler))
        .fallback(not_found)
        // Bodies are capped explicitly: JSON through `JsonBody`, uploads in `files`.
        .layer(DefaultBodyLimit::disable())
        // `layer` (not `route_layer`) so the fallback is covered as well.
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn not_found() -> Error {
    Error::not_found("route")
}

/// `GET /v1/sessions`
async fn list_sessions(State(state): State<SharedState>) -> Result<Json<List<SessionSummary>>> {
    Ok(Json(List::new(state.sessions.list().await?)))
}

/// `GET /v1/sessions/{id}`
async fn get_session(
    State(state): State<SharedState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<SessionDetail>> {
    Ok(Json(state.sessions.get(&id).await?))
}

#[derive(Deserialize)]
struct PatchSession {
    name: String,
}

/// `PATCH /v1/sessions/{id}` body `{ "name" }`
async fn patch_session(
    State(state): State<SharedState>,
    UrlPath(id): UrlPath<String>,
    JsonBody(body): JsonBody,
) -> Result<Json<SessionDetail>> {
    let req: PatchSession = parse_json(&body, &["name"])?;
    if req.name.chars().count() > 200 {
        return Err(Error::invalid("name must be at most 200 characters"));
    }
    Ok(Json(state.sessions.rename(&id, &req.name).await?))
}

/// `DELETE /v1/sessions/{id}`
async fn delete_session(
    State(state): State<SharedState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>> {
    state.sessions.delete(&id).await?;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

/// `GET /v1/models`
async fn list_models(State(state): State<SharedState>) -> Json<List<ModelInfo>> {
    Json(List::new(vec![ModelInfo {
        id: state.config.model_id(),
    }]))
}

/// `POST /v1/shutdown`: power-off, available in microVM init mode. The
/// instance token is enforced by [`require_token`].
async fn shutdown_handler(State(state): State<SharedState>) -> Result<Json<Value>> {
    if !state.config.shutdown_enabled {
        return Err(Error::not_supported(
            "shutdown is only available when the gateway runs as the machine's init",
        ));
    }
    tracing::warn!("shutdown requested; powering off");
    let wrapped = state.wrapped.clone();
    tokio::spawn(async move {
        // Let the HTTP response leave before the machine goes away, and give
        // the wrapped program its SIGTERM first.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Some(w) = wrapped {
            w.shutdown(wrap::GRACE).await;
        }
        if let Err(e) = crate::init::power_off() {
            tracing::error!(error = %e, "power off failed");
        }
    });
    Ok(Json(json!({ "status": "powering_off" })))
}

/// Serve `state` on an already-bound listener until `shutdown` resolves.
pub async fn serve_listener(
    state: SharedState,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| Error::internal(format!("http server failed: {e}")))
}

/// SIGTERM and SIGINT listeners. Installed before anything that must not
/// outlive the gateway is started, so a signal arriving early still stops
/// the gateway cleanly.
pub struct StopSignals {
    term: Option<tokio::signal::unix::Signal>,
    int: Option<tokio::signal::unix::Signal>,
}

impl StopSignals {
    /// Install the handlers; a handler that cannot be installed is logged
    /// and never fires.
    pub fn install() -> StopSignals {
        use tokio::signal::unix::{SignalKind, signal};
        let listen = |kind: SignalKind, name: &str| match signal(kind) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for {name}");
                None
            }
        };
        StopSignals {
            term: listen(SignalKind::terminate(), "SIGTERM"),
            int: listen(SignalKind::interrupt(), "SIGINT"),
        }
    }

    /// Resolves on the first SIGTERM or SIGINT.
    pub async fn recv(mut self) {
        let term = async {
            match self.term.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let int = async {
            match self.int.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = term => {}
            _ = int => {}
        }
        tracing::info!("shutdown signal received");
    }
}

/// Resolves on SIGTERM or SIGINT.
pub async fn shutdown_signal() {
    StopSignals::install().recv().await
}

/// Run the gateway with `config`: bind, start the terminal supervisor and,
/// in wrap mode, the image's own program; serve until a termination signal
/// arrives. Returns the process exit code: the wrapped program's exit code
/// in wrap mode (as the container would have exited without the gateway),
/// `0` otherwise.
pub async fn serve(config: GatewayConfig) -> Result<i32> {
    let signals = StopSignals::install();
    let state = build_state(config)?;
    let listener = tokio::net::TcpListener::bind(state.config.bind)
        .await
        .map_err(|e| Error::internal(format!("cannot bind {}: {e}", state.config.bind)))?;
    tracing::info!(
        addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        home = %state.home.display(),
        driver = state.config.driver_name(),
        agent_ready = state.responses.enabled(),
        wrapping = state.wrapped.as_ref().map(|w| w.argv().join(" ")).unwrap_or_default(),
        version = crate::VERSION,
        "warren-gateway listening"
    );
    let terminal_task = if state.config.terminal {
        Some(terminal::spawn_supervisor(
            state.home.clone(),
            state.config.terminal_port,
            state.owner.clone(),
        ))
    } else {
        None
    };
    // The socket is bound and the server task starts below, so the wrapped
    // program can rely on the gateway being reachable from its first instruction.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(serve_listener(state.clone(), listener, async move {
        let _ = stop_rx.await;
    }));
    if let Some(w) = &state.wrapped {
        w.start();
    }
    let result = tokio::select! {
        res = &mut server => {
            // The server never stops on its own unless it failed.
            match res {
                Ok(r) => r.map(|()| 0),
                Err(e) => Err(Error::internal(format!("http server task failed: {e}"))),
            }
        }
        _ = signals.recv() => {
            let _ = stop_tx.send(());
            let code = match &state.wrapped {
                Some(w) => w.shutdown(wrap::GRACE).await.process_exit_code(),
                None => 0,
            };
            match tokio::time::timeout(DRAIN, &mut server).await {
                Ok(Ok(r)) => r.map(|()| code),
                Ok(Err(e)) => Err(Error::internal(format!("http server task failed: {e}"))),
                Err(_) => {
                    tracing::warn!("connections still open after the drain period; exiting");
                    server.abort();
                    Ok(code)
                }
            }
        }
    };
    if let Some(w) = &state.wrapped
        && w.status().running
    {
        // The server failed while the program is still up: do not leave it orphaned.
        w.shutdown(wrap::GRACE).await;
    }
    if let Some(t) = terminal_task {
        t.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_health_and_version_are_open() {
        assert!(is_open_route(&Method::GET, "/v1/health"));
        assert!(is_open_route(&Method::HEAD, "/v1/health"));
        assert!(is_open_route(&Method::GET, "/v1/version"));
        assert!(!is_open_route(&Method::POST, "/v1/health"));
        assert!(!is_open_route(&Method::GET, "/"));
        assert!(!is_open_route(&Method::GET, "/v1/files"));
        assert!(!is_open_route(&Method::GET, "/v1/health/"));
        assert!(!is_open_route(&Method::GET, "//v1/health"));
        assert!(!is_open_route(&Method::GET, "/v1/./health"));
        assert!(!is_open_route(&Method::GET, "/v1/health/../files"));
        assert!(!is_open_route(&Method::GET, "/nope"));
    }
}
