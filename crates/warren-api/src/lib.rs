//! The Sandbox API under /v1.
use std::sync::Arc;
use axum::Router;
use warren_core::{Authenticator, Biller, Clock, Config, Lifecycle, Signer, Store, TemplateService};

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

/// Every /v1 route.
pub fn router(_d: Deps) -> Router {
    unimplemented!("api: not implemented")
}
