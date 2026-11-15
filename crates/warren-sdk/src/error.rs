//! The SDK error type and the mapping from HTTP responses to it.

use std::fmt::Write as _;

use warren_core::errors::{
    CODE_BAD_GATEWAY, CODE_CONFLICT, CODE_FORBIDDEN, CODE_INSUFFICIENT_BALANCE, CODE_INTERNAL,
    CODE_INVALID_API_KEY, CODE_INVALID_REQUEST, CODE_NOT_FOUND, CODE_TIMEOUT, CODE_TRY_AGAIN,
};
use warren_core::json::ErrorEnvelope;

/// Everything a client call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// The server answered with a non-2xx status. The payload is the decoded
    /// `{"error": {...}}` envelope with `status` set to the HTTP status; when the
    /// body was not an envelope (a plain-text `502` from the edge, for instance)
    /// the error is synthesised from the status code and the body text.
    #[error("{0}")]
    Api(#[from] warren_core::Error),
    /// The request never completed: DNS, connect, TLS, timeout or a failure while
    /// reading the body.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// A 2xx body (or a stream frame) could not be decoded into the expected type.
    #[error("decode: {0}")]
    Decode(String),
}

impl SdkError {
    /// The server-side error, when this is an [`SdkError::Api`].
    pub fn api(&self) -> Option<&warren_core::Error> {
        match self {
            SdkError::Api(e) => Some(e),
            _ => None,
        }
    }

    /// The stable error code (`not_found`, `conflict`, ...) of an API error.
    pub fn code(&self) -> Option<&str> {
        self.api().map(|e| e.code.as_str())
    }

    /// The HTTP status of an API error, or of a transport error that carries one.
    pub fn status(&self) -> Option<u16> {
        match self {
            SdkError::Api(e) => Some(e.status),
            SdkError::Http(e) => e.status().map(|s| s.as_u16()),
            SdkError::Decode(_) => None,
        }
    }

    /// `true` for a `404 not_found` answer.
    pub fn is_not_found(&self) -> bool {
        self.code() == Some(CODE_NOT_FOUND)
    }

    /// `true` when the server reported `timeout` or the request itself timed out.
    pub fn is_timeout(&self) -> bool {
        match self {
            SdkError::Api(e) => e.code == CODE_TIMEOUT,
            SdkError::Http(e) => e.is_timeout(),
            SdkError::Decode(_) => false,
        }
    }

    /// Build a decode error with context.
    pub fn decode(context: impl std::fmt::Display, cause: impl std::fmt::Display) -> Self {
        SdkError::Decode(format!("{context}: {cause}"))
    }
}

impl From<SdkError> for warren_core::Error {
    /// Lets code that returns `warren_core::Result` use `?` on SDK calls. API
    /// errors pass through unchanged; transport errors become `timeout` (504),
    /// `invalid_request` (400, a malformed URL or header) or `try_again` (503);
    /// decode errors become `internal` (500) with the cause kept in the message.
    fn from(e: SdkError) -> Self {
        match e {
            SdkError::Api(e) => e,
            SdkError::Http(e) => {
                let target = e.url().map(|u| format!(" to {u}")).unwrap_or_default();
                let cause = error_chain(&e);
                if e.is_timeout() {
                    warren_core::Error::new(
                        CODE_TIMEOUT,
                        504,
                        format!("request{target} timed out: {cause}"),
                    )
                } else if e.is_builder() {
                    warren_core::Error::new(
                        CODE_INVALID_REQUEST,
                        400,
                        format!("cannot build request{target}: {cause}"),
                    )
                } else {
                    warren_core::Error::new(
                        CODE_TRY_AGAIN,
                        503,
                        format!("request{target} failed: {cause}"),
                    )
                }
            }
            SdkError::Decode(msg) => {
                warren_core::Error::new(CODE_INTERNAL, 500, format!("decode: {msg}"))
            }
        }
    }
}

/// The error code the API would use for a bare HTTP status.
pub fn code_for_status(status: u16) -> &'static str {
    match status {
        400 => CODE_INVALID_REQUEST,
        401 => CODE_INVALID_API_KEY,
        402 => CODE_INSUFFICIENT_BALANCE,
        403 => CODE_FORBIDDEN,
        404 => CODE_NOT_FOUND,
        409 => CODE_CONFLICT,
        408 | 504 => CODE_TIMEOUT,
        429 | 503 => CODE_TRY_AGAIN,
        502 => CODE_BAD_GATEWAY,
        _ => CODE_INTERNAL,
    }
}

/// Decode a non-2xx body. Accepts the `{"error": {...}}` envelope, a bare error
/// object, and anything else (synthesised from the status and the body text).
pub fn api_error(status: u16, body: &[u8]) -> warren_core::Error {
    if let Ok(env) = serde_json::from_slice::<ErrorEnvelope>(body) {
        let mut e = env.error;
        e.status = status;
        return e;
    }
    if let Ok(mut e) = serde_json::from_slice::<warren_core::Error>(body)
        && !e.code.is_empty()
    {
        e.status = status;
        return e;
    }
    let text = snippet(body, 512);
    let message = if text.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {text}")
    };
    warren_core::Error::new(code_for_status(status), status, message)
}

/// The first `max` characters of a body as text, whitespace-trimmed, for messages.
pub(crate) fn snippet(body: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &text[..i]),
        None => text.to_string(),
    }
}

/// `Display` of an error followed by every `source()` in the chain.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let text = s.to_string();
        if !out.contains(&text) {
            let _ = write!(out, ": {text}");
        }
        cur = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_is_decoded_with_status() {
        let body = br#"{"error":{"code":"conflict","message":"instance is stopped","details":{"status":"stopped"}}}"#;
        let e = api_error(409, body);
        assert_eq!(e.code, "conflict");
        assert_eq!(e.status, 409);
        assert_eq!(e.message, "instance is stopped");
        assert_eq!(e.details.unwrap()["status"], "stopped");
    }

    #[test]
    fn bare_error_object_is_accepted() {
        let e = api_error(
            404,
            br#"{"code":"not_found","message":"instance not found"}"#,
        );
        assert_eq!(e.code, "not_found");
        assert_eq!(e.status, 404);
    }

    #[test]
    fn plain_text_is_synthesised() {
        let e = api_error(502, b"this template has no default port\n");
        assert_eq!(e.code, "bad_gateway");
        assert_eq!(e.status, 502);
        assert_eq!(e.message, "HTTP 502: this template has no default port");
        let e = api_error(418, b"");
        assert_eq!(e.code, "internal");
        assert_eq!(e.message, "HTTP 418");
        let e = api_error(401, br#"{"unexpected": true}"#);
        assert_eq!(e.code, "invalid_api_key");
    }

    #[test]
    fn snippet_truncates_on_char_boundary() {
        assert_eq!(snippet("  héllo wörld  ".as_bytes(), 4), "héll...");
        assert_eq!(snippet(b"short", 10), "short");
    }

    #[test]
    fn converts_to_core_error() {
        let api = SdkError::Api(warren_core::Error::not_found("instance"));
        assert!(api.is_not_found());
        assert_eq!(api.status(), Some(404));
        let core: warren_core::Error = api.into();
        assert_eq!(core.code, "not_found");

        let decode = SdkError::Decode("bad json".into());
        assert_eq!(decode.status(), None);
        let core: warren_core::Error = decode.into();
        assert_eq!(core.code, "internal");
        assert!(core.message.contains("bad json"));
    }

    #[test]
    fn implements_std_error() {
        fn takes_error<E: std::error::Error + Send + Sync + 'static>(_e: E) {}
        takes_error(SdkError::Decode("x".into()));
        let shown = SdkError::Api(warren_core::Error::conflict("busy")).to_string();
        assert_eq!(shown, "conflict: busy");
    }
}
