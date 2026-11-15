//! Request plumbing shared by the Sandbox and Agent clients: authentication
//! headers, URL building, status checks and body decoding.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use warren_core::HEADER_API_KEY;

use crate::error::{SdkError, api_error, snippet};

/// `User-Agent` sent with every request.
pub const USER_AGENT: &str = concat!("warren-sdk/", env!("CARGO_PKG_VERSION"));

/// Default per-request timeout for ordinary JSON calls. Exec, file transfers and
/// event streams set their own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for raw file reads, writes and downloads.
pub const FILE_TIMEOUT: Duration = Duration::from_secs(600);

/// Time to establish a TCP connection with the clients this crate builds.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A borrowed view of a client's HTTP handle and key.
#[derive(Clone, Copy)]
pub(crate) struct Http<'a> {
    pub http: &'a reqwest::Client,
    pub key: &'a str,
}

impl Http<'_> {
    /// A request carrying the key as both `Authorization: Bearer` and
    /// `x-warren-key` (when a key is set), without `Accept` or a timeout.
    pub fn raw_request(&self, method: Method, url: &str) -> RequestBuilder {
        let mut rb = self.http.request(method, url);
        if !self.key.is_empty() {
            rb = rb.bearer_auth(self.key).header(HEADER_API_KEY, self.key);
        }
        rb
    }

    /// A request for a JSON endpoint: auth headers, `Accept: application/json`
    /// and the default timeout.
    pub fn json_request(&self, method: Method, url: &str) -> RequestBuilder {
        self.raw_request(method, url)
            .header(ACCEPT, "application/json")
            .timeout(DEFAULT_TIMEOUT)
    }

    /// Send and decode the JSON body of a 2xx answer.
    pub async fn json<T: DeserializeOwned>(&self, rb: RequestBuilder) -> Result<T, SdkError> {
        decode_json(send_checked(rb).await?).await
    }

    /// Send and return the raw bytes of a 2xx answer.
    pub async fn bytes(&self, rb: RequestBuilder) -> Result<Vec<u8>, SdkError> {
        let resp = send_checked(rb).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Send, check the status and drop the body.
    pub async fn empty(&self, rb: RequestBuilder) -> Result<(), SdkError> {
        send_checked(rb).await.map(|_| ())
    }
}

/// Send a request and turn a non-2xx answer into [`SdkError::Api`].
pub(crate) async fn send_checked(rb: RequestBuilder) -> Result<Response, SdkError> {
    let resp = rb.send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    Err(SdkError::Api(api_error(status.as_u16(), &body)))
}

/// Decode a 2xx body, naming the URL and quoting the body on failure.
pub(crate) async fn decode_json<T: DeserializeOwned>(resp: Response) -> Result<T, SdkError> {
    let url = resp.url().to_string();
    let body = resp.bytes().await?;
    serde_json::from_slice(&body)
        .map_err(|e| SdkError::decode(&url, format!("{e} (body: {:?})", snippet(&body, 200))))
}

/// `Content-Type` for raw file uploads.
pub(crate) fn octet_stream(rb: RequestBuilder) -> RequestBuilder {
    rb.header(CONTENT_TYPE, "application/octet-stream")
}

/// Percent-encode everything but RFC 3986 unreserved characters. Used for path
/// segments and query values so ids, template pins and file paths round-trip.
pub fn encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
    }
    out
}

/// `base + path + encoded query`. `path` must already have encoded segments and
/// start with `/`; empty query lists add nothing.
pub fn build_url(base: &str, path: &str, query: &[(&str, String)]) -> String {
    let mut url = format!("{}{path}", base.trim_end_matches('/'));
    let mut sep = '?';
    for (k, v) in query {
        url.push(sep);
        url.push_str(&encode(k));
        url.push('=');
        url.push_str(&encode(v));
        sep = '&';
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode("ab12cd34ef"), "ab12cd34ef");
        assert_eq!(encode("my-tpl@3"), "my-tpl%403");
        assert_eq!(encode("/a b?c&d=e"), "%2Fa%20b%3Fc%26d%3De");
        assert_eq!(encode("héllo"), "h%C3%A9llo");
        assert_eq!(encode("~._-"), "~._-");
    }

    #[test]
    fn builds_urls() {
        assert_eq!(
            build_url("http://h:1/v1/", "/instances", &[]),
            "http://h:1/v1/instances"
        );
        assert_eq!(
            build_url(
                "http://h:1/v1",
                "/x/logs",
                &[("tail", "200".to_string()), ("since", "1 2".to_string())]
            ),
            "http://h:1/v1/x/logs?tail=200&since=1%202"
        );
    }
}
