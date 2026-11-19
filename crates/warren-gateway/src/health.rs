//! `GET /v1/health` and `GET /v1/version`.

use axum::{Json, extract::State};
use serde::Serialize;
use serde_json::{Value, json};
use warren_core::agentapi::Health;

use crate::{VERSION, owner::HomeOwner, server::SharedState, wrap::WrappedStatus};

/// The health document: the contract's [`Health`] fields, who owns the
/// persistent home (and therefore runs agent commands) and, in wrap mode,
/// the state of the wrapped program.
#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    #[serde(flatten)]
    pub health: Health,
    /// The user agent commands run as and created files belong to.
    pub home_owner: HomeOwnerInfo,
    /// Present only when the gateway wraps the image's own entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrapped: Option<WrappedStatus>,
}

/// `home_owner` in the health document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HomeOwnerInfo {
    pub uid: u32,
    /// Name from `/etc/passwd` or the system's user database (Open Directory
    /// on macOS), or the numeric uid as a string when unknown.
    pub name: String,
}

impl From<&HomeOwner> for HomeOwnerInfo {
    fn from(o: &HomeOwner) -> Self {
        HomeOwnerInfo {
            uid: o.uid,
            name: o.name.clone(),
        }
    }
}

/// The current health document for `state`.
pub fn report(state: &SharedState) -> HealthReport {
    HealthReport {
        health: Health {
            healthy: true,
            agent_ready: state.responses.enabled(),
            driver: state.config.driver_name().to_string(),
            version: VERSION.to_string(),
        },
        home_owner: HomeOwnerInfo::from(&state.owner),
        wrapped: state.wrapped.as_ref().map(|w| w.status()),
    }
}

/// `GET /v1/health`: the gateway is healthy once it answers; `agent_ready`
/// tells whether `POST /v1/responses` works on this instance; `wrapped`
/// (wrap mode only) tells whether the image's own program is still running.
pub async fn health_handler(State(state): State<SharedState>) -> Json<HealthReport> {
    Json(report(&state))
}

/// `GET /v1/version`
pub async fn version_handler() -> Json<Value> {
    Json(json!({ "version": VERSION }))
}

/// `GET /`: a small identification document for browsers hitting the instance URL.
pub async fn root_handler(State(state): State<SharedState>) -> Json<Value> {
    let mut doc = json!({
        "name": "warren-gateway",
        "version": VERSION,
        "instance_id": state.config.instance_id,
        "agent_ready": state.responses.enabled(),
        "driver": state.config.driver_name(),
    });
    if let Some(w) = &state.wrapped {
        doc["wrapped"] = json!(w.status());
    }
    Json(doc)
}
