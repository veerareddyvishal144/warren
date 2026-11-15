//! A router wired to the fakes plus request helpers.

use std::sync::Arc;

use axum::{Router, body::Body};
use http::{
    HeaderMap, Request, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::Value;
use tower::ServiceExt;
use warren_core::{Config, HmacSigner, KeyAuthenticator};

use super::fakes::{FakeBiller, FakeLifecycle, FakeTemplates, FixedClock, MemStore};
use crate::{Deps, router};

/// 2026-01-01T00:00:00Z
pub const NOW: i64 = 1_767_225_600;
pub const SECRET: &str = "test-secret";

pub struct Harness {
    pub app: Router,
    pub store: Arc<MemStore>,
    pub lifecycle: Arc<FakeLifecycle>,
    pub config: Config,
    pub clock: Arc<FixedClock>,
    pub signer: Arc<HmacSigner>,
}

/// A signed-up workspace and its first key.
pub struct Account {
    pub workspace_id: String,
    pub key: String,
    pub key_id: String,
}

pub fn test_config() -> Config {
    Config {
        version: "test".into(),
        ..Config::default()
    }
}

impl Harness {
    pub fn new() -> Self {
        Self::with_config(test_config())
    }

    pub fn with_config(config: Config) -> Self {
        let clock = Arc::new(FixedClock::new(NOW));
        let store = Arc::new(MemStore::default());
        let templates = Arc::new(FakeTemplates::new(
            store.clone(),
            config.clone(),
            clock.clone(),
        ));
        templates.seed();
        let biller = Arc::new(FakeBiller::new(store.clone(), clock.clone()));
        let lifecycle = Arc::new(FakeLifecycle::new(
            store.clone(),
            templates.clone(),
            biller.clone(),
            config.clone(),
            clock.clone(),
        ));
        let auth = Arc::new(KeyAuthenticator::new(store.clone(), clock.clone()));
        let signer = Arc::new(HmacSigner::new(SECRET, clock.clone()));
        let app = router(Deps {
            store: store.clone(),
            lifecycle: lifecycle.clone(),
            biller,
            templates,
            auth,
            signer: signer.clone(),
            config: config.clone(),
            clock: clock.clone(),
        });
        Harness {
            app,
            store,
            lifecycle,
            config,
            clock,
            signer,
        }
    }

    /// Send a request; returns status, headers and the JSON body (`Null` when empty).
    pub async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, headers, json)
    }

    /// JSON request with an optional Bearer key.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(k) = key {
            b = b.header(AUTHORIZATION, format!("Bearer {k}"));
        }
        let body = match body {
            Some(v) => {
                b = b.header(CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let (status, _, json) = self.send(b.body(body).unwrap()).await;
        (status, json)
    }

    /// Raw body with an optional Bearer key, for malformed payloads.
    pub async fn call_raw(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: impl Into<Body>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(CONTENT_TYPE, "application/json");
        if let Some(k) = key {
            b = b.header(AUTHORIZATION, format!("Bearer {k}"));
        }
        let (status, _, json) = self.send(b.body(body.into()).unwrap()).await;
        (status, json)
    }

    pub async fn signup(&self) -> Account {
        let (status, v) = self
            .call(
                "POST",
                "/v1/signup",
                None,
                Some(serde_json::json!({ "name": "Test" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        Account {
            workspace_id: v["workspace"]["id"].as_str().unwrap().to_string(),
            key: v["api_key"].as_str().unwrap().to_string(),
            key_id: v["key_id"].as_str().unwrap().to_string(),
        }
    }

    /// Create an instance and return its JSON.
    pub async fn create_instance(&self, key: &str, body: Value) -> Value {
        let (status, v) = self
            .call("POST", "/v1/instances", Some(key), Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        v
    }
}

/// The error code of an error envelope.
pub fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

/// The error message of an error envelope.
pub fn message(v: &Value) -> &str {
    v["error"]["message"].as_str().unwrap_or("")
}
