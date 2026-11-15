//! Authentication, public routes, fallbacks and CORS.

use axum::body::Body;
use http::{Request, StatusCode};
use serde_json::json;
use warren_core::{ApiKey, ids};

use super::harness::{Harness, NOW, code, message};

#[tokio::test]
async fn missing_key_is_401_with_envelope() {
    let h = Harness::new();
    let (status, v) = h.call("GET", "/v1/workspace", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&v), "invalid_api_key");
    assert!(message(&v).contains("x-warren-key"), "{v}");
}

#[tokio::test]
async fn unknown_and_malformed_keys_are_401() {
    let h = Harness::new();
    h.signup().await;
    let (status, v) = h
        .call(
            "GET",
            "/v1/instances",
            Some("sk_live_doesnotexist0000000000000000000000000"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&v), "invalid_api_key");
    let (status, _) = h
        .call("GET", "/v1/instances", Some("not-a-key"), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Non-Bearer Authorization schemes are rejected too.
    let req = Request::builder()
        .uri("/v1/instances")
        .header("authorization", "Basic abc")
        .body(Body::empty())
        .unwrap();
    let (status, _, v) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");
}

#[tokio::test]
async fn expired_key_is_401() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (plain, hash, prefix) = ids::new_api_key();
    h.store.lock().keys.push(ApiKey {
        id: ids::new_id(),
        workspace_id: acct.workspace_id.clone(),
        name: "old".into(),
        prefix,
        hash,
        created_at: NOW - 100,
        expires_at: NOW - 1,
        last_used_at: 0,
    });
    let (status, v) = h.call("GET", "/v1/workspace", Some(&plain), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(message(&v).contains("expired"), "{v}");
}

#[tokio::test]
async fn x_warren_key_header_is_accepted() {
    let h = Harness::new();
    let acct = h.signup().await;
    let req = Request::builder()
        .uri("/v1/workspace")
        .header("x-warren-key", &acct.key)
        .body(Body::empty())
        .unwrap();
    let (status, _, v) = h.send(req).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["id"], acct.workspace_id);
    // Authorization wins when both are present, so a bad Bearer fails even with a good x-warren-key.
    let req = Request::builder()
        .uri("/v1/workspace")
        .header("authorization", "Bearer sk_live_bad")
        .header("x-warren-key", &acct.key)
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = h.send(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn public_routes_need_no_key() {
    let h = Harness::new();
    let (status, v) = h.call("GET", "/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["healthy"], true);
    let (status, v) = h.call("GET", "/v1/version", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["version"], "test");
    let (status, _) = h.call("POST", "/v1/signup", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn unknown_routes_and_methods_answer_json() {
    let h = Harness::new();
    let (status, v) = h.call("GET", "/v1/nope", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&v), "not_found");
    let (status, v) = h.call("DELETE", "/v1/health", None, None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(code(&v), "invalid_request");
    assert!(message(&v).contains("DELETE"), "{v}");
}

#[tokio::test]
async fn cors_preflight_mirrors_origin() {
    let h = Harness::new();
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/v1/instances")
        .header("origin", "http://warren.localhost:7337")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "x-warren-key, content-type",
        )
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = h.send(req).await;
    assert!(status.is_success(), "{status}");
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "http://warren.localhost:7337"
    );
    assert_eq!(
        headers.get("access-control-allow-credentials").unwrap(),
        "true"
    );
    let allowed = headers
        .get("access-control-allow-headers")
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        allowed.contains("x-warren-key") && allowed.contains("authorization"),
        "{allowed}"
    );
    // Plain responses carry the origin too.
    let acct = h.signup().await;
    let req = Request::builder()
        .uri("/v1/workspace")
        .header("origin", "http://warren.localhost:7337")
        .header("authorization", format!("Bearer {}", acct.key))
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = h.send(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "http://warren.localhost:7337"
    );
}
