//! One cron firing: wake the instance, post the prompt to its gateway, turn
//! whatever happened into `last_status`, compute the next run and save.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use serde_json::json;
use tracing::Instrument;
use warren_core::{
    Clock, Cron, Error, HEADER_INSTANCE_TOKEN, Lifecycle, Store,
    agentapi::{ResponseObject, ResponseStatus, ResponsesRequest},
    json::ErrorEnvelope,
};

use crate::schedule;

/// `last_status` of a successful firing.
pub const STATUS_OK: &str = "ok";
/// `last_status` recorded when the schedule or timezone can no longer be
/// evaluated; the cron is disabled at the same time.
pub const STATUS_INVALID_SCHEDULE: &str = "invalid schedule";
/// `last_status` is capped at this many characters so a verbose error cannot
/// bloat the row.
pub(crate) const MAX_STATUS_CHARS: usize = 500;
/// How many times a failing `update_cron` is attempted before giving up.
pub(crate) const SAVE_ATTEMPTS: u32 = 3;
/// Pause between save attempts.
pub(crate) const SAVE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Everything a firing needs; shared by all firing tasks of one scheduler.
pub(crate) struct Firer {
    pub store: Arc<dyn Store>,
    pub lifecycle: Arc<dyn Lifecycle>,
    pub clock: Arc<dyn Clock>,
    pub client: reqwest::Client,
    pub gateway_port: u16,
    pub request_timeout: Duration,
}

impl Firer {
    /// Fire `cron` once and persist the result. Never fails: every error becomes
    /// the cron's `last_status`, and a store failure is logged.
    pub async fn fire(&self, cron: Cron) {
        let span =
            tracing::info_span!("cron_fire", cron_id = %cron.id, instance_id = %cron.instance_id);
        self.fire_inner(cron).instrument(span).await
    }

    async fn fire_inner(&self, cron: Cron) {
        let now = self.clock.now_unix();
        let status = self.deliver(&cron).await;
        if status == STATUS_OK {
            tracing::info!(status = %status, "cron fired");
        } else {
            tracing::warn!(status = %status, "cron fired");
        }
        // The firing may have taken minutes: re-read the row so an edit made in
        // the meantime (a PATCH, a disable) is kept and a deleted cron is not
        // written back.
        let mut latest = match self.store.get_cron(&cron.instance_id, &cron.id).await {
            Ok(latest) => latest,
            Err(e) if e.is_not_found() => {
                tracing::info!("cron was deleted while firing; result dropped");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, details = ?e.details, "cron: re-reading the cron failed; saving the firing snapshot");
                cron
            }
        };
        latest.last_status = truncate(status, MAX_STATUS_CHARS);
        latest.last_run_at = now;
        self.advance(&mut latest, now);
        self.save(&latest).await;
    }

    /// Set `next_run_at` to the first firing after `now`. When the schedule or
    /// timezone cannot be evaluated the cron is disabled with
    /// [`STATUS_INVALID_SCHEDULE`] so it does not come back every poll.
    pub fn advance(&self, cron: &mut Cron, now: i64) {
        match schedule::next_run(&cron.schedule, &cron.timezone, now) {
            Ok(next) => cron.next_run_at = next,
            Err(e) => {
                tracing::warn!(
                    cron_id = %cron.id,
                    schedule = %cron.schedule,
                    timezone = %cron.timezone,
                    error = %e.message,
                    "cron schedule cannot be evaluated; disabling"
                );
                cron.enabled = false;
                cron.last_status = STATUS_INVALID_SCHEDULE.to_string();
            }
        }
        cron.updated_at = now;
    }

    /// Persist the cron. A failing store is retried a few times before giving
    /// up with an error log; the cron then stays due and fires again at the
    /// next poll.
    pub async fn save(&self, cron: &Cron) {
        for attempt in 1..=SAVE_ATTEMPTS {
            match self.store.update_cron(cron).await {
                Ok(()) => return,
                Err(e) if e.is_not_found() => {
                    tracing::info!(cron_id = %cron.id, "cron vanished before its result could be saved");
                    return;
                }
                Err(e) if attempt < SAVE_ATTEMPTS => {
                    tracing::warn!(cron_id = %cron.id, attempt, error = %e, details = ?e.details, "cron: saving failed; retrying");
                    tokio::time::sleep(SAVE_RETRY_DELAY).await;
                }
                Err(e) => {
                    tracing::error!(cron_id = %cron.id, error = %e, details = ?e.details, "cron: saving the result failed; it stays due");
                }
            }
        }
    }

    /// Wake the instance, post the prompt and describe the outcome as a
    /// `last_status` value.
    async fn deliver(&self, cron: &Cron) -> String {
        if let Err(e) = self.lifecycle.ensure_awake(&cron.instance_id).await {
            return format!("wake failed: {}", describe(&e));
        }
        let (endpoint, token) = match self.lifecycle.gateway_credentials(&cron.instance_id).await {
            Ok(creds) => creds,
            Err(e) => return format!("error: {}", describe(&e)),
        };
        let url = gateway_url(&endpoint, self.gateway_port);
        let body = ResponsesRequest {
            input: cron.prompt.clone(),
            metadata: Some(json!({ "cron_id": cron.id })),
            ..Default::default()
        };
        tracing::debug!(url = %url, "posting cron prompt");
        self.lifecycle.record_activity(&cron.instance_id);
        let mut request = self.client.post(&url).json(&body);
        if !token.is_empty() {
            request = request.header(HEADER_INSTANCE_TOKEN, token);
        }
        let sent = request.send().await;
        self.lifecycle.record_activity(&cron.instance_id);
        let response = match sent {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return format!(
                    "error: no response within {}s",
                    self.request_timeout.as_secs()
                );
            }
            Err(e) => return format!("error: {}", error_chain(&e)),
        };
        let status = response.status();
        let text = match response.text().await {
            Ok(t) => t,
            Err(e) if e.is_timeout() => {
                return format!(
                    "error: response not finished within {}s",
                    self.request_timeout.as_secs()
                );
            }
            Err(e) => return format!("error: {}", error_chain(&e)),
        };
        interpret(status, &text)
    }
}

/// Turn the gateway's answer into a `last_status`: `ok` for a 2xx whose body is
/// not a failed response object, otherwise `error: ...` with the HTTP status
/// and the error message when the body carries one.
pub(crate) fn interpret(status: reqwest::StatusCode, body: &str) -> String {
    if status.is_success() {
        return match serde_json::from_str::<ResponseObject>(body) {
            Ok(r) if r.status == ResponseStatus::Failed => {
                let reason = r
                    .error
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| "agent reported failure".to_string());
                format!("error: {reason}")
            }
            _ => STATUS_OK.to_string(),
        };
    }
    let message = serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .map(|e| e.error.message)
        .filter(|m| !m.trim().is_empty());
    match message {
        Some(m) => format!("error: HTTP {} {}", status.as_u16(), m.trim()),
        None => format!("error: HTTP {}", status.as_u16()),
    }
}

/// `http://<endpoint>:<port>/v1/responses`. `endpoint` is normally the bare IP
/// the lifecycle reports; an IPv6 address is bracketed, and an endpoint that
/// already carries a port (`ip:port`) is used as is.
pub fn gateway_url(endpoint: &str, port: u16) -> String {
    let endpoint = endpoint.trim();
    if let Ok(addr) = endpoint.parse::<SocketAddr>() {
        return format!("http://{addr}/v1/responses");
    }
    if let Ok(ip) = endpoint.parse::<IpAddr>() {
        return format!("http://{}/v1/responses", SocketAddr::new(ip, port));
    }
    format!("http://{endpoint}:{port}/v1/responses")
}

/// `code: message`, with the cause instead of the generic message for internal
/// errors so the status says what actually went wrong.
fn describe(e: &Error) -> String {
    let cause = e
        .details
        .as_ref()
        .and_then(|d| d.get("cause"))
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty());
    match cause {
        Some(c) if e.is_internal() => format!("{}: {c}", e.code),
        _ => format!("{}: {}", e.code, e.message),
    }
}

/// The error and every source below it, joined with `: `, so a transport
/// failure reads like `error sending request ...: client error (Connect):
/// Connection refused (os error 111)`.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        source = s.source();
    }
    parts.join(": ")
}

/// Keep at most `max` characters, marking the cut.
fn truncate(mut s: String, max: usize) -> String {
    if let Some((cut, _)) = s.char_indices().nth(max) {
        s.truncate(cut);
        s.push_str("...");
    }
    s
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::*;

    #[test]
    fn gateway_urls() {
        assert_eq!(
            gateway_url("172.18.0.5", 3737),
            "http://172.18.0.5:3737/v1/responses"
        );
        assert_eq!(
            gateway_url(" 172.18.0.5 ", 3737),
            "http://172.18.0.5:3737/v1/responses"
        );
        assert_eq!(
            gateway_url("fd00::5", 3737),
            "http://[fd00::5]:3737/v1/responses"
        );
        assert_eq!(
            gateway_url("172.18.0.5:4000", 3737),
            "http://172.18.0.5:4000/v1/responses"
        );
        assert_eq!(
            gateway_url("[fd00::5]:4000", 3737),
            "http://[fd00::5]:4000/v1/responses"
        );
        assert_eq!(
            gateway_url("gateway.internal", 3737),
            "http://gateway.internal:3737/v1/responses"
        );
    }

    #[test]
    fn interprets_gateway_answers() {
        let completed = r#"{"id":"r1","session_id":"s1","status":"completed","output_text":"done","created_at":1}"#;
        assert_eq!(interpret(StatusCode::OK, completed), "ok");
        assert_eq!(interpret(StatusCode::CREATED, "not json"), "ok");
        assert_eq!(interpret(StatusCode::OK, ""), "ok");
        let failed = r#"{"id":"r1","session_id":"s1","status":"failed","created_at":1,"error":"model unavailable"}"#;
        assert_eq!(
            interpret(StatusCode::OK, failed),
            "error: model unavailable"
        );
        let failed_blank = r#"{"id":"r1","session_id":"s1","status":"failed","created_at":1}"#;
        assert_eq!(
            interpret(StatusCode::OK, failed_blank),
            "error: agent reported failure"
        );
        let envelope =
            r#"{"error":{"code":"not_supported","message":"this template has no agent"}}"#;
        assert_eq!(
            interpret(StatusCode::NOT_FOUND, envelope),
            "error: HTTP 404 this template has no agent"
        );
        assert_eq!(
            interpret(StatusCode::BAD_GATEWAY, "<html>nope</html>"),
            "error: HTTP 502"
        );
        assert_eq!(
            interpret(StatusCode::INTERNAL_SERVER_ERROR, ""),
            "error: HTTP 500"
        );
    }

    #[test]
    fn describes_errors() {
        assert_eq!(
            describe(&Error::conflict("instance is stopped")),
            "conflict: instance is stopped"
        );
        assert_eq!(
            describe(&Error::not_found("instance")),
            "not_found: instance not found"
        );
        assert_eq!(
            describe(&Error::internal("docker socket closed")),
            "internal: docker socket closed"
        );
        let bare = Error::new(warren_core::errors::CODE_INTERNAL, 500, "Internal error.");
        assert_eq!(describe(&bare), "internal: Internal error.");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("short".into(), 10), "short");
        assert_eq!(truncate("exactly10!".into(), 10), "exactly10!");
        assert_eq!(truncate("exactly10!x".into(), 10), "exactly10!...");
        assert_eq!(truncate("ééééé".into(), 3), "ééé...");
    }

    #[test]
    fn error_chains_join_sources() {
        let inner =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "Connection refused");
        let outer = Error::internal(&inner);
        assert_eq!(error_chain(&outer), "internal: Internal error.");
        assert_eq!(error_chain(&inner), "Connection refused");
    }
}
