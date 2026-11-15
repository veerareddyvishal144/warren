//! Signup, workspace, API keys and billing.

use http::StatusCode;
use serde_json::json;
use warren_core::{Config, MICROS_PER_DOLLAR};

use super::harness::{Harness, NOW, code, message, test_config};

#[tokio::test]
async fn signup_grants_free_credit_and_mints_a_key() {
    let h = Harness::new();
    let (status, v) = h
        .call("POST", "/v1/signup", None, Some(json!({ "name": "Acme" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let ws = &v["workspace"];
    assert!(ws["id"].as_str().unwrap().starts_with("ws_"));
    assert_eq!(ws["name"], "Acme");
    assert_eq!(ws["wallet_micros"], 5 * MICROS_PER_DOLLAR);
    assert_eq!(ws["top_up_total_micros"], 0);
    assert_eq!(ws["free_credit_granted"], true);
    assert_eq!(ws["created_at"], NOW);
    assert_eq!(v["wallet_micros"], 5 * MICROS_PER_DOLLAR);
    let key = v["api_key"].as_str().unwrap();
    assert!(key.starts_with("sk_live_") && key.len() == 48, "{key}");
    assert_eq!(v["key_id"].as_str().unwrap().len(), 32);

    // The key authenticates and the workspace view adds the awake figures.
    let (status, w) = h.call("GET", "/v1/workspace", Some(key), None).await;
    assert_eq!(status, StatusCode::OK, "{w}");
    assert_eq!(w["id"], ws["id"]);
    assert_eq!(w["awake_limit"], 3);
    assert_eq!(w["awake_count"], 0);
    assert_eq!(w["wallet_micros"], 5 * MICROS_PER_DOLLAR);

    // The first key is named "default" and never exposes its hash.
    let (_, keys) = h.call("GET", "/v1/api-keys", Some(key), None).await;
    let data = keys["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["name"], "default");
    assert_eq!(data[0]["id"], v["key_id"]);
    assert!(data[0].get("hash").is_none());
    assert_eq!(data[0]["prefix"].as_str().unwrap(), &key[..14]);

    // The credit is a ledger entry.
    let (_, ledger) = h.call("GET", "/v1/billing/ledger", Some(key), None).await;
    let entries = ledger["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "credit");
    assert_eq!(entries[0]["micros"], 5 * MICROS_PER_DOLLAR);
}

#[tokio::test]
async fn signup_defaults_and_disabled_credit() {
    let h = Harness::with_config(Config {
        free_credit_micros: 0,
        ..test_config()
    });
    let (status, v) = h.call("POST", "/v1/signup", None, None).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["workspace"]["name"], "workspace");
    assert_eq!(v["workspace"]["wallet_micros"], 0);
    assert_eq!(v["workspace"]["free_credit_granted"], false);
    assert_eq!(v["wallet_micros"], 0);
    let (status, v) = h
        .call("POST", "/v1/signup", None, Some(json!({ "name": "   " })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["workspace"]["name"], "workspace");
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

#[tokio::test]
async fn billing_view_and_top_up_rules() {
    // Without --dev the only provider is refused.
    let h = Harness::new();
    let acct = h.signup().await;
    let (status, v) = h.call("GET", "/v1/billing", Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["wallet_micros"], 5 * MICROS_PER_DOLLAR);
    assert_eq!(v["top_up_total_micros"], 0);
    assert_eq!(v["awake_limit"], 3);
    assert_eq!(v["awake_count"], 0);
    assert_eq!(v["free_credit_granted"], true);
    assert_eq!(v["rates"]["vcpu_month_micros"], 800_000);
    let (status, v) = h
        .call(
            "POST",
            "/v1/billing/top-up",
            Some(&acct.key),
            Some(json!({ "amount_micros": 10_000_000 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&v), "not_supported");
    assert!(message(&v).contains("--dev"), "{v}");

    // With --dev.
    let h = Harness::with_config(Config {
        dev_mode: true,
        ..test_config()
    });
    let acct = h.signup().await;
    let (status, v) = h
        .call(
            "POST",
            "/v1/billing/top-up",
            Some(&acct.key),
            Some(json!({ "amount_micros": 10_000_000, "provider": "dev" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["workspace"]["wallet_micros"], 15 * MICROS_PER_DOLLAR);
    assert_eq!(
        v["workspace"]["top_up_total_micros"],
        10 * MICROS_PER_DOLLAR
    );
    assert_eq!(v["entry"]["kind"], "topup");
    assert_eq!(v["entry"]["micros"], 10 * MICROS_PER_DOLLAR);
    assert_eq!(v["entry"]["id"].as_str().unwrap().len(), 32);
    let (_, b) = h.call("GET", "/v1/billing", Some(&acct.key), None).await;
    assert_eq!(b["awake_limit"], 10);
    assert_eq!(b["top_up_total_micros"], 10 * MICROS_PER_DOLLAR);

    // Amount validation belongs to the biller and surfaces as invalid_request.
    let (status, v) = h
        .call(
            "POST",
            "/v1/billing/top-up",
            Some(&acct.key),
            Some(json!({ "amount_micros": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&v), "invalid_request");
    // Unknown providers do not exist.
    let (status, v) = h
        .call(
            "POST",
            "/v1/billing/top-up",
            Some(&acct.key),
            Some(json!({ "amount_micros": 10_000_000, "provider": "stripe" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&v), "not_supported");
    // amount_micros is required.
    let (status, v) = h
        .call(
            "POST",
            "/v1/billing/top-up",
            Some(&acct.key),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("amount_micros"), "{v}");
}

#[tokio::test]
async fn ledger_is_a_list_envelope_newest_first_with_limit() {
    let h = Harness::with_config(Config {
        dev_mode: true,
        ..test_config()
    });
    let acct = h.signup().await;
    h.clock.advance(60);
    h.call(
        "POST",
        "/v1/billing/top-up",
        Some(&acct.key),
        Some(json!({ "amount_micros": 10_000_000 })),
    )
    .await;
    let (status, v) = h
        .call("GET", "/v1/billing/ledger", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["kind"], "topup");
    assert_eq!(data[1]["kind"], "credit");
    assert_eq!(v.as_object().unwrap().len(), 1, "only the data key: {v}");
    let (_, v) = h
        .call("GET", "/v1/billing/ledger?limit=1", Some(&acct.key), None)
        .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    let (status, v) = h
        .call("GET", "/v1/billing/ledger?limit=0", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("limit"), "{v}");
    let (status, _) = h
        .call("GET", "/v1/billing/ledger?limit=abc", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
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
