//! Scheduled prompts for agent instances.
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use warren_core::{Clock, Config, Lifecycle, Result, Store};

pub struct Scheduler;

impl Scheduler {
    pub fn new(_store: Arc<dyn Store>, _lifecycle: Arc<dyn Lifecycle>, _cfg: Config, _clock: Arc<dyn Clock>) -> Scheduler {
        unimplemented!("cron: not implemented")
    }
    pub async fn run(self, _shutdown: CancellationToken) -> Result<()> {
        unimplemented!("cron: not implemented")
    }
}
