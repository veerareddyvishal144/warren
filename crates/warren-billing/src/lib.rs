//! Rates, per-minute metering, wallet and limits.
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use warren_core::{Clock, Config, InstanceStopper, Result, Store};

pub struct Biller;

impl Biller {
    pub fn new(_store: Arc<dyn Store>, _cfg: Config, _clock: Arc<dyn Clock>) -> Arc<Biller> {
        unimplemented!("billing: not implemented")
    }
    /// Wire the component that stops instances when a wallet is exhausted.
    pub fn set_stopper(&self, _s: Arc<dyn InstanceStopper>) {
        unimplemented!("billing: not implemented")
    }
    /// Tick on `Config::meter_interval` until `shutdown` fires.
    pub async fn run(self: Arc<Self>, _shutdown: CancellationToken) -> Result<()> {
        unimplemented!("billing: not implemented")
    }
}
