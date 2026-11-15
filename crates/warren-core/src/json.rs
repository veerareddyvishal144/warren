//! JSON conventions shared by the API, the edge and the gateway.

use serde::{Deserialize, Serialize};

use crate::Error;

/// Every list endpoint returns `{"data": [...]}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct List<T> {
    pub data: Vec<T>,
}

impl<T> List<T> {
    pub fn new(data: Vec<T>) -> Self {
        List { data }
    }
}

/// Every error response is `{"error": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: Error,
}

impl From<Error> for ErrorEnvelope {
    fn from(e: Error) -> Self {
        ErrorEnvelope { error: e.public() }
    }
}

/// Bound on JSON request bodies.
pub const MAX_BODY_BYTES: usize = 4 << 20;

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        if self.is_internal() {
            tracing::error!(code = %self.code, details = ?self.details, "internal error");
        }
        let status = axum::http::StatusCode::from_u16(self.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        (status, axum::Json(ErrorEnvelope::from(self))).into_response()
    }
}
