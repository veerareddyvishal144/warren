//! `warren-mcp`: an MCP server over stdio that exposes Warren instances as
//! tools. Configured by `WARREN_API_URL` and `WARREN_API_KEY`; logs go to
//! stderr (`RUST_LOG` selects the level) because stdout carries the protocol.

use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;
use warren_mcp::{Settings, WarrenServer, sdk::SdkBackend};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let settings = Settings::from_env()?;
    if !settings.api_key.starts_with(warren_core::ids::KEY_PREFIX) {
        tracing::warn!(
            "WARREN_API_KEY does not start with {}; the daemon will reject it",
            warren_core::ids::KEY_PREFIX
        );
    }
    tracing::info!(api_url = %settings.api_url, version = env!("CARGO_PKG_VERSION"), "starting warren-mcp on stdio");

    let server = WarrenServer::new(SdkBackend::connect(&settings));
    let running = server.serve(stdio()).await?;
    let reason = running.waiting().await?;
    tracing::info!(?reason, "warren-mcp stopped");
    Ok(())
}
