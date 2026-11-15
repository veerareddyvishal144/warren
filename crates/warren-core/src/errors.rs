use serde::{Deserialize, Serialize};

use crate::types::{dollars, Money};

/// Stable error codes. Branch on the code, show the message.
pub const CODE_INVALID_REQUEST: &str = "invalid_request";
pub const CODE_INVALID_API_KEY: &str = "invalid_api_key";
pub const CODE_FORBIDDEN: &str = "forbidden";
pub const CODE_NOT_FOUND: &str = "not_found";
pub const CODE_CONFLICT: &str = "conflict";
pub const CODE_INSTANCE_LIMIT_REACHED: &str = "instance_limit_reached";
pub const CODE_INSUFFICIENT_BALANCE: &str = "insufficient_balance";
pub const CODE_TRY_AGAIN: &str = "try_again";
pub const CODE_NOT_SUPPORTED: &str = "not_supported";
pub const CODE_TIMEOUT: &str = "timeout";
pub const CODE_BAD_GATEWAY: &str = "bad_gateway";
pub const CODE_INTERNAL: &str = "internal";

/// The one error type used across Warren. Serializes as the body of
/// `{"error": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Error {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip)]
    pub status: u16,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: &str, status: u16, message: impl Into<String>) -> Self {
        Error { code: code.to_string(), message: message.into(), details: None, status }
    }
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(CODE_INVALID_REQUEST, 400, message)
    }
    pub fn invalid_api_key(message: impl Into<String>) -> Self {
        let m: String = message.into();
        Self::new(CODE_INVALID_API_KEY, 401, if m.is_empty() { "Missing or invalid API key.".to_string() } else { m })
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(CODE_FORBIDDEN, 403, message)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(CODE_NOT_FOUND, 404, format!("{what} not found"))
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(CODE_CONFLICT, 409, message)
    }
    pub fn try_again(message: impl Into<String>) -> Self {
        Self::new(CODE_TRY_AGAIN, 503, message)
    }
    pub fn not_supported(message: impl Into<String>) -> Self {
        Self::new(CODE_NOT_SUPPORTED, 400, message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(CODE_TIMEOUT, 504, message)
    }
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(CODE_BAD_GATEWAY, 502, message)
    }
    pub fn insufficient_balance(required: Money, balance: Money) -> Self {
        Self::new(
            CODE_INSUFFICIENT_BALANCE,
            402,
            format!("Wallet balance {:.6} is below the {:.6} required.", dollars(balance), dollars(required)),
        )
        .with_details(serde_json::json!({ "required_micros": required, "balance_micros": balance }))
    }
    pub fn instance_limit(limit: u32, awake: u32) -> Self {
        Self::new(
            CODE_INSTANCE_LIMIT_REACHED,
            409,
            format!("This workspace may run {limit} instances at once and {awake} are awake."),
        )
        .with_details(serde_json::json!({ "limit": limit, "awake": awake }))
    }
    /// Wrap an unexpected failure. The cause is kept in `details` for logs; the
    /// JSON layer strips it before answering clients.
    pub fn internal(cause: impl std::fmt::Display) -> Self {
        Self::new(CODE_INTERNAL, 500, "Internal error.").with_details(serde_json::json!({ "cause": cause.to_string() }))
    }
    pub fn is_not_found(&self) -> bool {
        self.code == CODE_NOT_FOUND
    }
    pub fn is_internal(&self) -> bool {
        self.code == CODE_INTERNAL
    }
    /// The client-safe copy: internal errors lose their details.
    pub fn public(&self) -> Error {
        if self.is_internal() { Error::new(CODE_INTERNAL, 500, "Internal error.") } else { self.clone() }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::internal(format!("{e:#}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::internal(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::internal(e)
    }
}

/// Returned by `Runtime::resize` when the new shape cannot be applied live; the
/// caller stops, resizes again and starts.
pub fn requires_restart() -> Error {
    Error::new(CODE_TRY_AGAIN, 503, "resize requires restart").with_details(serde_json::json!({ "requires_restart": true }))
}

pub fn is_requires_restart(e: &Error) -> bool {
    e.details.as_ref().and_then(|d| d.get("requires_restart")).and_then(|v| v.as_bool()).unwrap_or(false)
}
