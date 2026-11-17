//! Signup, workspace and API keys.

use http::StatusCode;
use serde_json::{Value, json};
use warren_core::{Config, InstanceStatus};

use super::harness::{Harness, NOW, code, message, test_config};

#[tokio::test]
async fn signup_mints_a_workspace_and_a_key() {
    let h = Harness::new();
    let (status, v) = h
        .call("POST", "/v1/signup", None, Some(json!({ "name": "Acme" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let body = v.as_object().unwrap();
    assert_eq!(body.len(), 3, "exactly workspace, api_key, key_id: {v}");
    let ws = &v["workspace"];
    assert!(ws["id"].as_str().unwrap().starts_with("ws_"));
    assert_eq!(ws["name"], "Acme");
    assert_eq!(ws["created_at"], NOW);
    let fields = ws.as_object().unwrap();
    assert_eq!(fields.len(), 3, "no monetary fields on the workspace: {ws}");
    let key = v["api_key"].as_str().unwrap();
    assert!(key.starts_with("sk_live_") && key.len() == 48, "{key}");
    assert_eq!(v["key_id"].as_str().unwrap().len(), 32);

    // The key authenticates and the workspace view adds the awake figures;
    // the default configuration has no awake limit, reported as null.
    let (status, w) = h.call("GET", "/v1/workspace", Some(key), None).await;
    assert_eq!(status, StatusCode::OK, "{w}");
    assert_eq!(w["id"], ws["id"]);
    assert_eq!(w["name"], "Acme");
    assert_eq!(w["created_at"], NOW);
    let view = w.as_object().unwrap();
    assert!(view.contains_key("awake_limit"), "{w}");
    assert_eq!(w["awake_limit"], Value::Null);
    assert_eq!(w["awake_count"], 0);
    assert_eq!(
        view.len(),
        5,
        "id, name, created_at, awake_limit, awake_count: {w}"
    );

    // The first key is named "default" and never exposes its hash.
    let (_, keys) = h.call("GET", "/v1/api-keys", Some(key), None).await;
    let data = keys["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["name"], "default");
    assert_eq!(data[0]["id"], v["key_id"]);
    assert!(data[0].get("hash").is_none());
    assert_eq!(data[0]["prefix"].as_str().unwrap(), &key[..14]);
}

#[tokio::test]
async fn signup_defaults_the_name() {
    let h = Harness::new();
    let (status, v) = h.call("POST", "/v1/signup", None, None).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["workspace"]["name"], "workspace");
    let (status, v) = h
        .call("POST", "/v1/signup", None, Some(json!({ "name": "   " })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["workspace"]["name"], "workspace");
}

#[tokio::test]
async fn workspace_view_reports_the_configured_awake_limit() {
    let h = Harness::with_config(Config {
        max_awake_instances: Some(3),
        ..test_config()
    });
    let acct = h.signup().await;
    let (status, w) = h.call("GET", "/v1/workspace", Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::OK, "{w}");
    assert_eq!(w["awake_limit"], 3);
    assert_eq!(w["awake_count"], 0);

    let id = h.create_instance(&acct.key, json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.store.lock().instances.get_mut(&id).unwrap().status = InstanceStatus::Running;
    let (_, w) = h.call("GET", "/v1/workspace", Some(&acct.key), None).await;
    assert_eq!(w["awake_limit"], 3);
    assert_eq!(w["awake_count"], 1);
}

#[tokio::test]
async fn billing_routes_are_gone() {
    let h = Harness::new();
    let acct = h.signup().await;
    for (method, path) in [
        ("GET", "/v1/billing"),
        ("POST", "/v1/billing/top-up"),
        ("GET", "/v1/billing/ledger"),
    ] {
        let (status, v) = h.call(method, path, Some(&acct.key), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {v}");
        assert_eq!(code(&v), "not_found", "{method} {path}: {v}");
    }
}

#[tokio::test]
async fn signup_rejects_unknown_fields_and_bad_json() {
    let h = Harness::new();
    let (status, v) = h
        .call("POST", "/v1/signup", None, Some(json!({ "nmae": "x" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&v), "invalid_request");
    assert!(message(&v).contains("unknown field `nmae`"), "{v}");
    let (status, v) = h.call_raw("POST", "/v1/signup", None, "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("malformed JSON"), "{v}");
    let (status, v) = h
        .call("POST", "/v1/signup", None, Some(json!({ "name": 5 })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("invalid type"), "{v}");
}

#[tokio::test]
async fn api_keys_create_use_and_delete() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (status, v) = h
        .call(
            "POST",
            "/v1/api-keys",
            Some(&acct.key),
            Some(json!({ "name": "ci", "expires_at": NOW + 3600 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["key"]["name"], "ci");
    assert_eq!(v["key"]["expires_at"], NOW + 3600);
    assert_eq!(v["key"]["workspace_id"], acct.workspace_id);
    let plain = v["api_key"].as_str().unwrap().to_string();
    let id = v["key"]["id"].as_str().unwrap().to_string();
    assert!(plain.starts_with("sk_live_"));

    let (status, w) = h.call("GET", "/v1/workspace", Some(&plain), None).await;
    assert_eq!(status, StatusCode::OK, "{w}");

    // Newest first.
    h.clock.advance(10);
    let (_, v2) = h
        .call("POST", "/v1/api-keys", Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(v2["key"]["name"], "key");
    let (_, keys) = h.call("GET", "/v1/api-keys", Some(&acct.key), None).await;
    let names: Vec<&str> = keys["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["name"].as_str().unwrap())
        .collect();
    assert_eq!(names[0], "key");
    assert_eq!(names.len(), 3);

    // Past expiry is rejected.
    let (status, v) = h
        .call(
            "POST",
            "/v1/api-keys",
            Some(&acct.key),
            Some(json!({ "expires_at": NOW - 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("expires_at"), "{v}");

    let (status, v) = h
        .call(
            "DELETE",
            &format!("/v1/api-keys/{id}"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "id": id, "deleted": true }));
    let (status, _) = h.call("GET", "/v1/workspace", Some(&plain), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, v) = h
        .call(
            "DELETE",
            &format!("/v1/api-keys/{id}"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&v), "not_found");

    // Another workspace cannot delete our key.
    let other = h.signup().await;
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/v1/api-keys/{}", acct.key_id),
            Some(&other.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Deleting the key in use is allowed.
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/v1/api-keys/{}", acct.key_id),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h.call("GET", "/v1/workspace", Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A signup request as seen from a peer address.
fn signup_from(ip: &str) -> http::Request<axum::body::Body> {
    use axum::extract::ConnectInfo;
    let addr: std::net::SocketAddr = format!("{ip}:40000").parse().unwrap();
    let mut req = http::Request::builder()
        .method("POST")
        .uri("/v1/signup")
        .body(axum::body::Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

#[tokio::test]
async fn signup_can_be_disabled() {
    let h = Harness::with_config(Config {
        signup_enabled: false,
        ..test_config()
    });
    let (status, v) = h.call("POST", "/v1/signup", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(code(&v), "not_found");
    assert!(h.store.lock().workspaces.is_empty());
}

#[tokio::test]
async fn signup_is_rate_limited_per_client_ip() {
    let h = Harness::new();
    for i in 0..10 {
        let (status, _, v) = h.send(signup_from("203.0.113.7")).await;
        assert_eq!(status, StatusCode::CREATED, "signup {i}: {v}");
    }
    let (status, _, v) = h.send(signup_from("203.0.113.7")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(code(&v), "try_again");
    assert!(message(&v).contains("10 per hour"), "{v}");

    // Another address has its own budget; IPv4-mapped IPv6 is the same address.
    let (status, _, _) = h.send(signup_from("203.0.113.8")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = h.send(signup_from("[::ffff:203.0.113.7]")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // The budget refills over the hour.
    h.clock.advance(6 * 60);
    let (status, _, _) = h.send(signup_from("203.0.113.7")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = h.send(signup_from("203.0.113.7")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Loopback is not exempt outside dev mode.
    for _ in 0..10 {
        h.send(signup_from("127.0.0.1")).await;
    }
    let (status, _, _) = h.send(signup_from("127.0.0.1")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn signup_without_a_peer_shares_one_bucket_and_dev_loopback_is_exempt() {
    let h = Harness::new();
    for _ in 0..10 {
        let (status, _) = h.call("POST", "/v1/signup", None, None).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, v) = h.call("POST", "/v1/signup", None, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");

    let h = Harness::with_config(Config {
        dev_mode: true,
        ..test_config()
    });
    for ip in ["127.0.0.1", "[::1]", "[::ffff:127.0.0.1]"] {
        for i in 0..12 {
            let (status, _, v) = h.send(signup_from(ip)).await;
            assert_eq!(status, StatusCode::CREATED, "{ip} signup {i}: {v}");
        }
    }
    // Dev mode exempts loopback only.
    for _ in 0..10 {
        h.send(signup_from("203.0.113.9")).await;
    }
    let (status, _, _) = h.send(signup_from("203.0.113.9")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}
