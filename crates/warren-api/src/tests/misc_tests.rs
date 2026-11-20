//! Health, version and templates.

use http::StatusCode;
use serde_json::json;
use warren_core::Config;

use super::harness::{Harness, NOW, code, message, test_config};

#[tokio::test]
async fn health_and_version() {
    let h = Harness::new();
    let (status, v) = h.call("GET", "/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v,
        json!({ "healthy": true, "version": "test", "runtimes": ["docker"], "dev_mode": false, "time": NOW })
    );
    let h = Harness::with_config(Config {
        enable_firecracker: true,
        dev_mode: true,
        ..test_config()
    });
    let (_, v) = h.call("GET", "/v1/health", None, None).await;
    assert_eq!(v["runtimes"], json!(["docker", "firecracker"]));
    assert_eq!(v["dev_mode"], true);
    let (status, v) = h.call("GET", "/v1/version", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!({ "version": "test" }));
}

#[tokio::test]
async fn templates_catalog_register_pin_and_delete() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (status, v) = h.call("GET", "/v1/templates", Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "warren-agent",
            "warren-base",
            "warren-claude",
            "warren-opencode"
        ]
    );
    let (status, v) = h
        .call("GET", "/v1/templates/warren-base", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["scope"], "system");
    assert_eq!(v["image_ref"], "warren/base:dev");
    assert_eq!(v["default_port"], 3737);
    let (status, v) = h
        .call("GET", "/v1/templates/no-such", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&v), "not_found");
    let (status, _) = h
        .call("GET", "/v1/templates/Not%20Valid", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Register, publish a second revision, pin.
    let (status, v) = h
        .call(
            "POST",
            "/v1/templates",
            Some(&acct.key),
            Some(json!({ "name": "my-tool", "image_ref": "alpine:3.20", "default_port": 8080 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "my-tool");
    assert_eq!(v["revision"], 1);
    assert_eq!(v["scope"], "workspace");
    assert_eq!(v["workspace_id"], acct.workspace_id);
    assert_eq!(v["default_port"], 8080);
    let (status, v) = h
        .call(
            "POST",
            "/v1/templates",
            Some(&acct.key),
            Some(json!({ "name": "my-tool", "image_ref": "alpine:3.21", "default_port": 8080 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["revision"], 2);
    let (status, v) = h
        .call("GET", "/v1/templates/my-tool@1", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["revision"], 1);
    assert_eq!(v["image_ref"], "alpine:3.20");
    let (_, v) = h
        .call("GET", "/v1/templates/my-tool", Some(&acct.key), None)
        .await;
    assert_eq!(v["revision"], 2);
    let (status, _) = h
        .call("GET", "/v1/templates/my-tool@9", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, v) = h.call("GET", "/v1/templates", Some(&acct.key), None).await;
    let names: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "warren-agent",
            "warren-base",
            "warren-claude",
            "warren-opencode",
            "my-tool"
        ],
        "system first, then workspace"
    );
    // Other workspaces do not see it.
    let other = h.signup().await;
    let (status, _) = h
        .call("GET", "/v1/templates/my-tool", Some(&other.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Validation.
    for (body, text) in [
        (
            json!({ "name": "warren-mine", "image_ref": "x" }),
            "reserved",
        ),
        (json!({ "name": "Bad", "image_ref": "x" }), "template name"),
        (json!({ "name": "ok-name", "image_ref": "  " }), "image_ref"),
        (json!({ "name": "ok-name" }), "missing field `image_ref`"),
        (
            json!({ "name": "ok-name", "image_ref": "x", "mount_path": "rel" }),
            "mount_path",
        ),
        (
            json!({ "name": "ok-name", "image_ref": "x", "env": { "bad-key": "v" } }),
            "env key",
        ),
        (
            json!({ "name": "ok-name", "image_ref": "x", "cpu": 2 }),
            "unknown field `cpu`",
        ),
    ] {
        let (status, v) = h
            .call("POST", "/v1/templates", Some(&acct.key), Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {v}");
        assert!(message(&v).contains(text), "{body} -> {v}");
    }

    // Delete: refused while in use, then allowed.
    let inst = h
        .create_instance(&acct.key, json!({ "template": "my-tool" }))
        .await;
    assert_eq!(inst["template_name"], "my-tool");
    assert_eq!(inst["template_revision"], 2);
    assert_eq!(inst["default_port"], 8080);
    let (status, v) = h
        .call("DELETE", "/v1/templates/my-tool", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "conflict");
    h.call(
        "DELETE",
        &format!("/v1/instances/{}", inst["id"].as_str().unwrap()),
        Some(&acct.key),
        None,
    )
    .await;
    let (status, v) = h
        .call("DELETE", "/v1/templates/my-tool", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "name": "my-tool", "deleted": true }));
    let (status, _) = h
        .call("DELETE", "/v1/templates/my-tool", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .call("DELETE", "/v1/templates/my-tool@1", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cors_allows_only_the_dashboard_origin() {
    use axum::body::Body;
    use http::{
        Request,
        header::{ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_ORIGIN, ORIGIN},
    };
    let h = Harness::new();
    let dashboard = "http://warren.localhost:7337";
    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/v1/health")
            .header(ORIGIN, origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization")
            .body(Body::empty())
            .unwrap()
    };
    let (status, headers, _) = h.send(preflight(dashboard)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), dashboard);
    assert_eq!(
        headers.get(ACCESS_CONTROL_ALLOW_CREDENTIALS).unwrap(),
        "true"
    );

    for origin in [
        "http://evil.example",
        "http://ab12cd34ef.warren.localhost:7337",
        "https://warren.localhost:7337",
    ] {
        let (_, headers, _) = h.send(preflight(origin)).await;
        assert!(
            headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
            "{origin} must not be granted"
        );
        let req = Request::builder()
            .method("GET")
            .uri("/v1/health")
            .header(ORIGIN, origin)
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = h.send(req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
            "{origin}"
        );
    }
    let req = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .header(ORIGIN, dashboard)
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = h.send(req).await;
    assert_eq!(headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), dashboard);

    let cfg = warren_core::Config {
        public_scheme: "https".into(),
        public_port: 443,
        base_domain: "warren.example".into(),
        ..warren_core::Config::default()
    };
    assert_eq!(
        crate::routes::dashboard_origin(&cfg),
        "https://warren.example"
    );
}
