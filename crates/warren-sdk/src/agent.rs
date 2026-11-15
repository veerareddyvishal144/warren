//! The Agent API client: the gateway inside one instance, reached through the
//! edge at the instance URL.

use reqwest::Method;
use reqwest::header::ACCEPT;
use serde_json::json;
use warren_core::agentapi::{
    DirListing, Health, Message, ModelInfo, MoveRequest, ResponseObject, ResponsesRequest,
    SessionDetail, SessionSummary,
};
use warren_core::json::List;
use warren_core::{DEFAULT_EXEC_TIMEOUT_SEC, ExecRequest, ExecResult, HEADER_INSTANCE_TOKEN};

use crate::error::SdkError;
use crate::http::{FILE_TIMEOUT, Http, build_url, encode, octet_stream};
use crate::sse::EventStream;
use crate::types::{CancelResponse, DeletedFile, VersionResponse, WriteFileResponse};

/// Client for the Agent API of one instance.
///
/// `base_url` is the instance URL (`http://<id>.<base>[:port]`); the `/v1`
/// prefix is added by the client. Requests carry the workspace key as both
/// `Authorization: Bearer` and `x-warren-key` so the edge accepts them.
#[derive(Debug, Clone)]
pub struct AgentClient {
    /// Instance URL without a trailing slash or `/v1`.
    pub base_url: String,
    pub key: String,
    pub http: reqwest::Client,
}

impl AgentClient {
    /// A client with a default HTTP client.
    pub fn new(instance_url: impl Into<String>, api_key: impl Into<String>) -> AgentClient {
        Self::with_http(instance_url, api_key, crate::http_client())
    }

    /// A client over a caller-provided `reqwest::Client`.
    pub fn with_http(
        instance_url: impl Into<String>,
        api_key: impl Into<String>,
        http: reqwest::Client,
    ) -> AgentClient {
        AgentClient {
            base_url: normalize_base_url(&instance_url.into()),
            key: api_key.into().trim().to_string(),
            http,
        }
    }

    fn http(&self) -> Http<'_> {
        Http {
            http: &self.http,
            key: &self.key,
        }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        build_url(&self.base_url, &format!("/v1{path}"), query)
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, SdkError> {
        let h = self.http();
        h.json(h.json_request(Method::GET, &self.url(path, query)))
            .await
    }

    async fn send_json<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T, SdkError> {
        let h = self.http();
        h.json(h.json_request(method, &self.url(path, &[])).json(body))
            .await
    }

    // ----- health ---------------------------------------------------------------------

    /// `GET /v1/health`.
    pub async fn health(&self) -> Result<Health, SdkError> {
        self.get("/health", &[]).await
    }

    /// `GET /v1/version`.
    pub async fn version(&self) -> Result<String, SdkError> {
        self.get::<VersionResponse>("/version", &[])
            .await
            .map(|v| v.version)
    }

    // ----- responses ------------------------------------------------------------------

    /// `POST /v1/responses` without streaming; waits for the whole turn. The
    /// HTTP timeout is generous (an agent turn may run shell commands).
    pub async fn respond(&self, req: &ResponsesRequest) -> Result<ResponseObject, SdkError> {
        let mut req = req.clone();
        req.stream = Some(false);
        let h = self.http();
        let rb = h
            .json_request(Method::POST, &self.url("/responses", &[]))
            .timeout(std::time::Duration::from_secs(
                u64::from(DEFAULT_EXEC_TIMEOUT_SEC) + 60,
            ))
            .json(&req);
        h.json(rb).await
    }

    /// `POST /v1/responses` with `stream: true`. The request is sent on first
    /// poll; events arrive as `response.created`, `response.output_text.delta`,
    /// optional `response.tool_call`, then `response.completed` or
    /// `response.failed`.
    pub fn stream(&self, req: &ResponsesRequest) -> EventStream {
        let mut req = req.clone();
        req.stream = Some(true);
        let rb = self
            .http()
            .raw_request(Method::POST, &self.url("/responses", &[]))
            .header(ACCEPT, "text/event-stream")
            .json(&req);
        EventStream::connect(rb)
    }

    /// `GET /v1/responses/{id}`.
    pub async fn get_response(&self, id: &str) -> Result<ResponseObject, SdkError> {
        self.get(&format!("/responses/{}", encode(id)), &[]).await
    }

    /// `GET /v1/responses/{id}/stream`: replays stored events, follows a live
    /// response and ends with `response.completed`.
    pub fn response_stream(&self, id: &str) -> EventStream {
        let rb = self
            .http()
            .raw_request(
                Method::GET,
                &self.url(&format!("/responses/{}/stream", encode(id)), &[]),
            )
            .header(ACCEPT, "text/event-stream");
        EventStream::connect(rb)
    }

    /// `POST /v1/responses/{id}/cancel`.
    pub async fn cancel(&self, id: &str) -> Result<CancelResponse, SdkError> {
        self.send_json(
            Method::POST,
            &format!("/responses/{}/cancel", encode(id)),
            &json!({}),
        )
        .await
    }

    // ----- sessions -------------------------------------------------------------------

    /// `GET /v1/sessions`, newest first.
    pub async fn sessions(&self) -> Result<Vec<SessionSummary>, SdkError> {
        self.get::<List<SessionSummary>>("/sessions", &[])
            .await
            .map(|l| l.data)
    }

    /// `GET /v1/sessions/{id}` with its messages.
    pub async fn session(&self, id: &str) -> Result<SessionDetail, SdkError> {
        self.get(&format!("/sessions/{}", encode(id)), &[]).await
    }

    /// The messages of a session, oldest first.
    pub async fn messages(&self, id: &str) -> Result<Vec<Message>, SdkError> {
        self.session(id).await.map(|s| s.messages)
    }

    /// `PATCH /v1/sessions/{id}` with `{ "name" }`. The answer body is not
    /// specified by the contract and is ignored.
    pub async fn rename_session(&self, id: &str, name: &str) -> Result<(), SdkError> {
        let h = self.http();
        let rb = h
            .json_request(
                Method::PATCH,
                &self.url(&format!("/sessions/{}", encode(id)), &[]),
            )
            .json(&json!({ "name": name }));
        h.empty(rb).await
    }

    /// `DELETE /v1/sessions/{id}`.
    pub async fn delete_session(&self, id: &str) -> Result<(), SdkError> {
        let h = self.http();
        h.empty(h.json_request(
            Method::DELETE,
            &self.url(&format!("/sessions/{}", encode(id)), &[]),
        ))
        .await
    }

    /// `GET /v1/models`.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, SdkError> {
        self.get::<List<ModelInfo>>("/models", &[])
            .await
            .map(|l| l.data)
    }

    // ----- files ----------------------------------------------------------------------

    /// `GET /v1/files?path=`. Paths are relative to the persistent home; an
    /// empty path lists the home itself.
    pub async fn list_files(&self, path: &str) -> Result<DirListing, SdkError> {
        let p = if path.is_empty() { "." } else { path };
        self.get("/files", &[("path", p.to_string())]).await
    }

    /// `GET /v1/files/content?path=` -> the raw bytes.
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, SdkError> {
        let h = self.http();
        let rb = h
            .raw_request(
                Method::GET,
                &self.url("/files/content", &[("path", path.to_string())]),
            )
            .timeout(FILE_TIMEOUT);
        h.bytes(rb).await
    }

    /// `PUT /v1/files/content?path=` with a raw body; parents are created.
    pub async fn write_file(
        &self,
        path: &str,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<WriteFileResponse, SdkError> {
        let h = self.http();
        let rb = octet_stream(h.raw_request(
            Method::PUT,
            &self.url("/files/content", &[("path", path.to_string())]),
        ))
        .header(ACCEPT, "application/json")
        .timeout(FILE_TIMEOUT)
        .body(bytes.into());
        h.json(rb).await
    }

    /// `DELETE /v1/files?path=`.
    pub async fn delete_file(&self, path: &str) -> Result<DeletedFile, SdkError> {
        let h = self.http();
        h.json(h.json_request(
            Method::DELETE,
            &self.url("/files", &[("path", path.to_string())]),
        ))
        .await
    }

    /// `POST /v1/files/move` with `{ "from", "to" }`. The answer body is not
    /// specified by the contract and is ignored.
    pub async fn move_file(&self, from: &str, to: &str) -> Result<(), SdkError> {
        let h = self.http();
        let body = MoveRequest {
            from: from.to_string(),
            to: to.to_string(),
        };
        h.empty(
            h.json_request(Method::POST, &self.url("/files/move", &[]))
                .json(&body),
        )
        .await
    }

    /// `GET /v1/files/download?path=` -> a gzip tarball of the directory.
    pub async fn download(&self, path: &str) -> Result<Vec<u8>, SdkError> {
        let h = self.http();
        let rb = h
            .raw_request(
                Method::GET,
                &self.url("/files/download", &[("path", path.to_string())]),
            )
            .timeout(FILE_TIMEOUT);
        h.bytes(rb).await
    }

    // ----- exec -----------------------------------------------------------------------

    /// `POST /v1/exec` with `x-warren-instance-token: <token>`. This is the
    /// daemon's path into vm-type instances; the token is the per-instance
    /// secret only the daemon knows.
    pub async fn exec(&self, token: &str, req: &ExecRequest) -> Result<ExecResult, SdkError> {
        let secs = if req.timeout_seconds > 0 {
            req.timeout_seconds
        } else {
            DEFAULT_EXEC_TIMEOUT_SEC
        };
        let h = self.http();
        let rb = h
            .json_request(Method::POST, &self.url("/exec", &[]))
            .header(HEADER_INSTANCE_TOKEN, token)
            .timeout(std::time::Duration::from_secs(u64::from(secs) + 60))
            .json(req);
        h.json(rb).await
    }
}

/// Trim, drop trailing slashes and a trailing `/v1` (the client adds it).
pub(crate) fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_urls() {
        let want = "http://ab12cd34ef.warren.localhost:7337";
        assert_eq!(
            normalize_base_url("http://ab12cd34ef.warren.localhost:7337"),
            want
        );
        assert_eq!(
            normalize_base_url("http://ab12cd34ef.warren.localhost:7337/"),
            want
        );
        assert_eq!(
            normalize_base_url("http://ab12cd34ef.warren.localhost:7337/v1/"),
            want
        );
        assert_eq!(
            normalize_base_url(" http://ab12cd34ef.warren.localhost:7337/v1 "),
            want
        );
    }

    #[test]
    fn urls_get_the_v1_prefix() {
        let c = AgentClient::with_http("http://h:1/", "k", reqwest::Client::new());
        assert_eq!(
            c.url("/files", &[("path", "a b".to_string())]),
            "http://h:1/v1/files?path=a%20b"
        );
    }
}
