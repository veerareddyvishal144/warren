//! Default-type instances as Docker containers, preferring gVisor (runsc).
use warren_core::{Config, Error, Result};

pub struct DockerRuntime;

impl DockerRuntime {
    /// Connect to the local Docker daemon and prepare the warren networks.
    pub async fn new(_cfg: Config) -> Result<DockerRuntime> {
        Err(Error::internal("dockerrt: not implemented"))
    }
}
