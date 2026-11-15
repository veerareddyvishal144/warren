//! Raw requests to instance URLs through the edge, for the behaviour the SDK
//! does not model: the authentication variants, signed-URL redirects and
//! cookies, CORS preflights, preview ports and plain-text 502s.

use std::sync::Arc;

use reqwest::{Method, header::HeaderMap};
use warren_core::{COOKIE_NAME, HEADER_API_KEY, Result};
use warren_sdk::AgentClient;

use crate::{
    Daemon, REQUEST_TIMEOUT, ensure,
    util::{api_error, cookie_value, preview_bytes, transport},
};

/// How an instance request authenticates at the edge.
#[derive(Debug, Clone, Copy)]
pub enum Auth<'a> {
    /// No credentials: the edge must answer `401 invalid_api_key`.
    None,
    /// `X-Warren-Key: <key>`.
    Key(&'a str),
    /// `Authorization: Bearer <key>`.
    Bearer(&'a str),
    /// `Cookie: warren_session=<token>` as set by a signed URL.
    Cookie(&'a str),
}

/// One instance's URLs as seen with one workspace key. Requests use a client
/// that never follows redirects and has a long timeout (wake-on-request).
#[derive(Clone)]
pub struct Edge {
    daemon: Arc<Daemon>,
    id: String,
    key: String,
}

impl Edge {
    pub fn new(daemon: Arc<Daemon>, id: &str, key: &str) -> Edge {
        Edge {
            daemon,
            id: id.to_string(),
            key: key.to_string(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// `http://<id>[-<preview_port>].<base>:<public>` + `path_and_query`.
    pub fn url(&self, preview_port: u16, path_and_query: &str) -> String {
        format!(
            "{}{path_and_query}",
            self.daemon.instance_url(&self.id, preview_port)
        )
    }

    /// The SDK's Agent API client for this instance with the edge's key.
    pub fn agent(&self) -> Result<AgentClient> {
        let url = self.daemon.instance_url(&self.id, 0);
        let http = self.daemon.client_for_url(&url)?;
        Ok(AgentClient::with_http(url, self.key.as_str(), http))
    }

    /// Send a request and return the raw response (no status mapping).
    /// `body` is `(content-type, bytes)`; `headers` are extra request headers.
    pub async fn send(
        &self,
        method: Method,
        preview_port: u16,
        path_and_query: &str,
        auth: Auth<'_>,
        body: Option<(&str, Vec<u8>)>,
        headers: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let url = self.url(preview_port, path_and_query);
        let client = self.daemon.raw_client_for_url(&url)?;
        let mut req = client
            .request(method.clone(), &url)
            .timeout(REQUEST_TIMEOUT);
        req = match auth {
            Auth::None => req,
            Auth::Key(k) => req.header(HEADER_API_KEY, k),
            Auth::Bearer(k) => req.bearer_auth(k),
            Auth::Cookie(token) => req.header("cookie", format!("{COOKIE_NAME}={token}")),
        };
        if let Some((ct, bytes)) = body {
            req = req.header("content-type", ct).body(bytes);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.send()
            .await
            .map_err(|e| transport(&format!("{method} {url}"), e))
    }

    /// GET with a chosen authentication; the raw response.
    pub async fn get(
        &self,
        preview_port: u16,
        path_and_query: &str,
        auth: Auth<'_>,
    ) -> Result<reqwest::Response> {
        self.send(Method::GET, preview_port, path_and_query, auth, None, &[])
            .await
    }

    /// GET with the workspace key, expecting 2xx: (status, headers, body).
    pub async fn get_ok(
        &self,
        preview_port: u16,
        path_and_query: &str,
    ) -> Result<(u16, HeaderMap, Vec<u8>)> {
        let resp = self
            .get(preview_port, path_and_query, Auth::Key(&self.key))
            .await?;
        read_ok(resp).await
    }
}

/// A header value as a string (empty when absent or not UTF-8).
pub fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The `warren_session` cookie value from a response's `Set-Cookie` headers.
pub fn session_cookie(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| cookie_value(v, COOKIE_NAME).map(str::to_string))
}

/// Read a response expected to be 2xx: (status, headers, body). Other statuses
/// become an `Error` with the status and the daemon's or gateway's code.
pub async fn read_ok(resp: reqwest::Response) -> Result<(u16, HeaderMap, Vec<u8>)> {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| transport("read response body", e))?;
    if (200..300).contains(&status) {
        Ok((status, headers, bytes.to_vec()))
    } else {
        Err(api_error(status, &bytes))
    }
}

/// Assert a raw response has `status` and, when `code` is given, an error
/// envelope with that code. Returns the body for further checks.
pub async fn expect_status(
    resp: reqwest::Response,
    status: u16,
    code: Option<&str>,
) -> Result<Vec<u8>> {
    let got = resp.status().as_u16();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| transport("read response body", e))?
        .to_vec();
    ensure!(
        got == status,
        "expected HTTP {status}, got {got}: {}",
        preview_bytes(&bytes)
    );
    if let Some(code) = code {
        let e = api_error(got, &bytes);
        ensure!(
            e.code == code,
            "expected error code {code}, got {} ({})",
            e.code,
            e.message
        );
    }
    Ok(bytes)
}
