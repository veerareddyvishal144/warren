//! Template catalog, workspace templates and pins.
use std::sync::Arc;
use warren_core::{Config, Runtimes, Store};

pub struct Templates;

impl Templates {
    pub fn new(_store: Arc<dyn Store>, _runtimes: Runtimes, _cfg: Config) -> Arc<Templates> {
        unimplemented!("templates: not implemented")
    }
}
