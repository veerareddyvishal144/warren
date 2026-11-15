//! Instance orchestration across store, runtimes and biller.
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use warren_core::{Biller, Clock, Config, Result, Runtimes, Store, TemplateService};

pub struct Deps {
    pub store: Arc<dyn Store>,
    pub runtimes: Runtimes,
    pub biller: Arc<dyn Biller>,
    pub templates: Arc<dyn TemplateService>,
    pub config: Config,
    pub clock: Arc<dyn Clock>,
}

pub struct Manager;

impl Manager {
    /// Validate deps and return the manager. Background loops start in `run`.
    pub fn new(_d: Deps) -> Result<Arc<Manager>> {
        unimplemented!("lifecycle: not implemented")
    }
    /// Reconciler, idle sleeper, metrics sampler and disk refresh until shutdown.
    pub async fn run(self: Arc<Self>, _shutdown: CancellationToken) -> Result<()> {
        unimplemented!("lifecycle: not implemented")
    }
}
