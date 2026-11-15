//! Request parsing helpers shared by the handlers: strict JSON bodies, raw
//! query strings and the instance token check. All failures are
//! `warren_core::Error` values so clients always receive the JSON envelope.

use axum::{
    body::Bytes,
    extract::{FromRequest, Request},
    http::HeaderMap,
};
use serde::de::DeserializeOwned;
use warren_core::{
    Error, Result, errors::CODE_INVALID_REQUEST, json::MAX_BODY_BYTES, types::HEADER_INSTANCE_TOKEN,
};

/// A request body read with the [`MAX_BODY_BYTES`] cap. Oversized bodies are
/// answered with the JSON error envelope (`413 invalid_request`) instead of
/// axum's plain-text rejection.
pub struct JsonBody(pub Bytes);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = Error;

    async fn from_request(req: Request, _state: &S) -> Result<Self> {
        let too_large = || {
            Error::new(
                CODE_INVALID_REQUEST,
                413,
                format!("request body exceeds {MAX_BODY_BYTES} bytes"),
            )
        };
        if let Some(len) = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            && len > MAX_BODY_BYTES
        {
            return Err(too_large());
        }
        let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|e| {
                if e.to_string().contains("length limit") {
                    too_large()
                } else {
                    Error::invalid(format!("cannot read request body: {e}"))
                }
            })?;
        Ok(JsonBody(bytes))
    }
}

/// Parse a JSON object body, rejecting unknown fields with `invalid_request`.
pub fn parse_json<T: DeserializeOwned>(body: &[u8], allowed: &[&str]) -> Result<T> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::invalid(format!(
            "request body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(Error::invalid("request body must be a JSON object"));
    }
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| Error::invalid(format!("invalid JSON body: {e}")))?;
    let Some(obj) = value.as_object() else {
        return Err(Error::invalid("request body must be a JSON object"));
    };
    if let Some(unknown) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Error::invalid(format!("unknown field `{unknown}`")));
    }
    serde_json::from_value(value).map_err(|e| Error::invalid(format!("invalid request: {e}")))
}

/// Decode one parameter from a raw query string (`a=1&b=2`). `+` is a space.
pub fn query_param(raw: Option<&str>, key: &str) -> Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if decode(k)? != key {
            continue;
        }
        return Ok(Some(decode(v)?));
    }
    Ok(None)
}

fn decode(s: &str) -> Result<String> {
    let plus_as_space = s.replace('+', " ");
    urlencoding::decode(&plus_as_space)
        .map(|c| c.into_owned())
        .map_err(|_| {
            Error::invalid(format!(
                "query parameter {s:?} is not valid percent-encoded UTF-8"
            ))
        })
}

/// Required query parameter, `invalid_request` when missing or empty.
pub fn required_query(raw: Option<&str>, key: &str) -> Result<String> {
    match query_param(raw, key)? {
        Some(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(Error::invalid(format!("missing query parameter `{key}`"))),
    }
}

/// Enforce `x-warren-instance-token` when the gateway was given a token.
/// An empty `expected` disables the check (local runs without a token).
/// Missing or wrong tokens are `401 invalid_api_key`; the comparison takes
/// the same time whatever the presented value.
pub fn require_instance_token(headers: &HeaderMap, expected: &str) -> Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    let presented = headers
        .get(HEADER_INSTANCE_TOKEN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(Error::invalid_api_key(format!(
            "missing or invalid instance token: send it in the {HEADER_INSTANCE_TOKEN} header"
        )))
    }
}

/// Byte-wise comparison whose running time depends only on the lengths, so a
/// wrong token does not leak how many leading bytes matched. Unequal lengths
/// are reported after a full pass over the presented value.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    struct Body {
        name: String,
        #[serde(default)]
        count: u32,
    }

    #[test]
    fn strict_json() {
        let b: Body = parse_json(br#"{"name":"x","count":2}"#, &["name", "count"]).unwrap();
        assert_eq!((b.name.as_str(), b.count), ("x", 2));
        let e = parse_json::<Body>(br#"{"name":"x","extra":1}"#, &["name", "count"]).unwrap_err();
        assert_eq!(e.code, "invalid_request");
        assert!(e.message.contains("extra"));
        assert_eq!(parse_json::<Body>(b"", &["name"]).unwrap_err().status, 400);
        assert_eq!(
            parse_json::<Body>(b"[1]", &["name"]).unwrap_err().status,
            400
        );
        assert_eq!(
            parse_json::<Body>(b"{bad", &["name"]).unwrap_err().status,
            400
        );
        assert_eq!(
            parse_json::<Body>(br#"{"count":1}"#, &["name", "count"])
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn query_decoding() {
        assert_eq!(
            query_param(Some("path=a%2Fb+c&x=1"), "path")
                .unwrap()
                .as_deref(),
            Some("a/b c")
        );
        assert_eq!(query_param(Some("x=1"), "path").unwrap(), None);
        assert_eq!(query_param(None, "path").unwrap(), None);
        assert_eq!(
            query_param(Some("path"), "path").unwrap().as_deref(),
            Some("")
        );
        assert!(required_query(Some("path="), "path").is_err());
        assert_eq!(required_query(Some("path=.&y"), "path").unwrap(), ".");
        assert!(query_param(Some("path=%ff"), "path").is_err());
    }

    #[test]
    fn token_check() {
        let mut h = HeaderMap::new();
        assert!(require_instance_token(&h, "").is_ok());
        let e = require_instance_token(&h, "secret").unwrap_err();
        assert_eq!(e.status, 401);
        assert_eq!(e.code, "invalid_api_key");
        assert!(e.message.contains(HEADER_INSTANCE_TOKEN), "{}", e.message);
        for wrong in ["wrong", "secre", "secrets", "", "Secret"] {
            h.insert(HEADER_INSTANCE_TOKEN, HeaderValue::from_str(wrong).unwrap());
            let e = require_instance_token(&h, "secret").unwrap_err();
            assert_eq!(
                (e.status, e.code.as_str()),
                (401, "invalid_api_key"),
                "{wrong:?}"
            );
        }
        h.insert(HEADER_INSTANCE_TOKEN, HeaderValue::from_static("secret"));
        assert!(require_instance_token(&h, "secret").is_ok());
        h.insert(HEADER_INSTANCE_TOKEN, HeaderValue::from_static(" secret "));
        assert!(require_instance_token(&h, "secret").is_ok());
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
