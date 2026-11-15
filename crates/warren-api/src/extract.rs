//! Request extractors that fail with Warren's JSON error envelope instead of
//! axum's plain-text rejections.

use std::{collections::BTreeMap, fmt::Display, str::FromStr};

use axum::extract::{FromRequest, FromRequestParts, Request};
use http::{header::CONTENT_LENGTH, request::Parts};
use serde::de::DeserializeOwned;
use warren_core::{Error, Result, json::MAX_BODY_BYTES};

/// A JSON request body bounded by [`MAX_BODY_BYTES`].
///
/// The request types carry `deny_unknown_fields`, so an unknown field, a wrong
/// type or malformed JSON all become `400 invalid_request` carrying serde's
/// message (which names the offending field). An empty body reads as `{}` so
/// endpoints whose fields are all optional accept a bare `POST`. The
/// `Content-Type` header is not required.
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Error;

    async fn from_request(req: Request, _state: &S) -> Result<Self> {
        let declared = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok());
        if let Some(len) = declared
            && len > MAX_BODY_BYTES
        {
            return Err(Error::invalid(format!(
                "request body is {len} bytes; the limit is {MAX_BODY_BYTES} bytes"
            )));
        }
        let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|e| {
                Error::invalid(format!(
                    "could not read the request body: {e} (limit {MAX_BODY_BYTES} bytes)"
                ))
            })?;
        parse_json(&bytes).map(JsonBody)
    }
}

/// Decode a JSON body; whitespace-only input is treated as `{}`.
pub fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let body: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        bytes
    };
    serde_json::from_slice(body).map_err(|e| {
        let what = match e.classify() {
            serde_json::error::Category::Data => "invalid request body",
            _ => "malformed JSON body",
        };
        Error::invalid(format!("{what}: {e}"))
    })
}

/// The decoded query string with typed, range-checked accessors.
#[derive(Debug, Clone, Default)]
pub struct Query(BTreeMap<String, String>);

impl<S: Send + Sync> FromRequestParts<S> for Query {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self> {
        Query::parse(parts.uri.query().unwrap_or(""))
    }
}

impl Query {
    /// Decode `a=1&b=2`; a repeated name keeps its last value.
    pub fn parse(raw: &str) -> Result<Self> {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw)
            .map_err(|e| Error::invalid(format!("malformed query string: {e}")))?;
        Ok(Query(pairs.into_iter().collect()))
    }

    /// The raw value of a parameter.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// An integer parameter within `min..=max`; `default` when absent or empty.
    pub fn int<T>(&self, name: &str, default: T, min: T, max: T) -> Result<T>
    where
        T: FromStr + PartialOrd + Display + Copy,
    {
        let Some(raw) = self.get(name).map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(default);
        };
        let value: T = raw
            .parse()
            .map_err(|_| Error::invalid(format!("query parameter {name} must be an integer")))?;
        if value < min || value > max {
            return Err(Error::invalid(format!(
                "query parameter {name} must be between {min} and {max}"
            )));
        }
        Ok(value)
    }
}
