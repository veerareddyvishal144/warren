//! End-to-end test harness: starts a real `warrend` against Docker, talks to
//! it through DNS-pinned HTTP clients built by the SDK, and kills it on drop.
//! The tests live in `tests/e2e.rs` and are `#[ignore]`d; run them with
//! `cargo test -p warren-e2e -- --ignored --test-threads=1` or `scripts/e2e.sh`.
//!
//! Typed calls go through `warren_sdk::Client` and `warren_sdk::AgentClient`
//! (see [`Workspace`]); [`Edge`] sends raw requests to instance URLs for the
//! edge behaviour the SDK does not model (missing or foreign keys, signed-URL
//! redirects, cookies, CORS, 502s).
//!
//! Environment knobs:
//! - `WARREN_BIN`: the daemon binary (default `<repo>/target/release/warrend`).
//! - `WARREN_GATEWAY_BIN`: the static (musl) gateway every daemon gets as
//!   `--gateway-bin` and injects into registered images (default
//!   `<repo>/target/x86_64-unknown-linux-musl/release/warren-gateway`, then
//!   `<repo>/target-musl/...`); starting a daemon fails when none exists.
//! - `WARREN_E2E_VM=1`: start the daemon with `--firecracker --kernel ...` and
//!   run the vm scenario (`WARREN_E2E_KERNEL`, `WARREN_E2E_FIRECRACKER_BIN`;
//!   `WARREN_E2E_FIRECRACKER_JAILER=/path/to/jailer` adds `--firecracker-jailer`
//!   so the scenario runs through the jailer).
//! - `WARREN_E2E_DOCKER_CHECKPOINTS=1`: start every daemon with
//!   `--docker-checkpoints` and run `default_sleep_keeps_memory` (the Docker
//!   engine must be in experimental mode; point `DOCKER_HOST` at one).
//! - `WARREN_E2E_KEEP=1`: keep each daemon's data dir and log after the test.
//! - `WARREN_E2E_LOG`: daemon log filter (default `info`).
//! - `WARREN_E2E_IMAGE_TAG`: tag of the built-in images the daemon should use
//!   (`--image-tag`; default `dev`).
//! - `WARREN_E2E_TMP`: where temp dirs go (default the system temp dir).

use std::{net::SocketAddr, time::Duration};

use warren_core::Result;

pub mod daemon;
pub mod edge;
pub mod util;
pub mod workspace;

pub use daemon::{
    DEFAULT_GATEWAY_BINS, Daemon, MAX_AWAKE_INSTANCES, MIN_IDLE_SECONDS, MUSL_GATEWAY_REL,
};
pub use edge::{Auth, Edge, expect_status, header, read_ok, session_cookie};
pub use util::{
    CODE_E2E, api_error, cookie_value, encode_query, fail, host_of, now_unix, poll_until,
};
pub use warren_sdk::{self, AgentClient, Client, SdkError, SseEvent};
pub use workspace::{RUNNING_TIMEOUT, TRANSITION_TIMEOUT, Workspace};

/// Whole-request timeout of the harness's raw requests. Generous because a
/// request to a sleeping instance is held while it wakes.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// The SDK's DNS-pinned client for one host: `warren_sdk::client_with_resolve`
/// mapping `host` to `addr` (the URL's port is used; `addr`'s port is ignored).
/// Follows redirects like every SDK client.
pub fn client_with_resolve(host: &str, addr: SocketAddr) -> Result<reqwest::Client> {
    warren_sdk::try_client_with_resolve(&[(host, addr)]).map_err(warren_core::Error::from)
}

/// The SDK's builder with the same DNS pin but redirects disabled, for
/// inspecting the signed-URL `302` and its `Set-Cookie`.
pub fn raw_client_with_resolve(host: &str, addr: SocketAddr) -> Result<reqwest::Client> {
    warren_sdk::http_builder()
        .resolve(host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| util::transport("build http client", e))
}

/// Names of SSE events in order, for assertions and error messages.
pub fn event_names(events: &[SseEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_pinned_clients() {
        let addr: SocketAddr = "127.0.0.1:7337".parse().unwrap();
        client_with_resolve("api.warren.localhost", addr).expect("sdk client builds");
        raw_client_with_resolve("ab12cd34ef.warren.localhost", addr).expect("raw client builds");
    }

    #[test]
    fn names_events() {
        let events = vec![
            SseEvent {
                event: "response.created".into(),
                data: serde_json::json!({}),
            },
            SseEvent {
                event: "response.completed".into(),
                data: serde_json::json!({}),
            },
        ];
        assert_eq!(
            event_names(&events),
            vec!["response.created", "response.completed"]
        );
    }
}
