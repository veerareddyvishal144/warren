//! VM-type instances as Firecracker microVMs built from the template image.
use warren_core::{Config, Error, Result};

pub struct FirecrackerRuntime;

impl FirecrackerRuntime {
    /// Prepare the Firecracker runtime (binary, kernel, bridge network).
    pub async fn new(_cfg: Config) -> Result<FirecrackerRuntime> {
        Err(Error::internal("firecracker: not implemented"))
    }
}
