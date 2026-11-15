//! The single front door: Host-header dispatch, instance auth, wake-on-request,
//! reverse proxy.
use std::sync::Arc;
use axum::Router;
use warren_core::{Authenticator, Config, Lifecycle, Signer};

pub struct Deps {
    pub config: Config,
    pub api: Router,
    pub dashboard: Router,
    pub lifecycle: Arc<dyn Lifecycle>,
    pub auth: Arc<dyn Authenticator>,
    pub signer: Arc<dyn Signer>,
}

/// The root router served on the single listener.
pub fn router(_d: Deps) -> Router {
    unimplemented!("edge: not implemented")
}
