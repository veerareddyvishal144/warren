//! Instances: create, validation, list/get, actions, exec/logs/metrics,
//! signed URLs, backups.

use http::StatusCode;
use serde_json::{Value, json};
use warren_core::{Config, Error, InstanceStatus, Signer};

use super::harness::{Harness, NOW, code, message, test_config};

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn create_instance_happy_path() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (status, v) = h
        .call("POST", "/v1/instances", Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let id = id_of(&v);
    assert_eq!(id.len(), 10);
    assert_eq!(v["status"], "provisioning");
    assert_eq!(v["workspace_id"], acct.workspace_id);
    assert_eq!(v["template"], "warren-base");
    assert_eq!(v["template_name"], "warren-base");
    assert_eq!(v["template_revision"], 1);
    assert_eq!(v["type"], "default");
    assert_eq!(v["resources"], json!({ "cpu": 2, "memory": 4, "disk": 4 }));
    assert_eq!(v["network"], "allow-all");
    assert_eq!(v["auto_sleep"], false);
    assert_eq!(v["idle_timeout_seconds"], 900);
    assert_eq!(v["default_port"], 3737);
    assert_eq!(v["agent_api"], false);
    assert_eq!(v["runtime"], "docker");
    assert_eq!(v["url"], h.config.instance_url(&id));
    assert_eq!(v["url"], format!("http://{id}.warren.localhost:7337"));
    assert_eq!(v["created_at"], NOW);
    // Internal fields never leak.
    for hidden in ["instance_token", "runtime_ref", "ip", "storage_path"] {
        assert!(v.get(hidden).is_none(), "{hidden} leaked: {v}");
    }
    assert_eq!(h.lifecycle.calls(), vec!["create"]);
}

#[tokio::test]
async fn create_instance_with_options() {
    let h = Harness::new();
    let acct = h.signup().await;
    let body = json!({
        "template": "warren-agent",
        "name": "Prod agent",
        "type": "default",
        "resources": { "cpu": 4, "memory": 8 },
        "env": { "APP_ENV": "production" },
        "metadata": { "plan": "pro" },
        "auto_sleep": true,
        "idle_timeout_seconds": 600,
        "network": "deny-all"
    });
    let v = h.create_instance(&acct.key, body).await;
    assert_eq!(v["name"], "Prod agent");
    assert_eq!(v["agent_api"], true);
    assert_eq!(v["resources"], json!({ "cpu": 4, "memory": 8, "disk": 6 }));
    assert_eq!(v["env"]["APP_ENV"], "production");
    assert_eq!(v["metadata"]["plan"], "pro");
    assert_eq!(v["auto_sleep"], true);
    assert_eq!(v["idle_timeout_seconds"], 600);
    assert_eq!(v["network"], "deny-all");
    assert!(v.get("budget").is_none(), "no budget on instances: {v}");
}

#[tokio::test]
async fn create_instance_validation_errors() {
    let h = Harness::new();
    let acct = h.signup().await;
    let cases: Vec<(Value, &str, &str)> = vec![
        (
            json!({ "env": { "lower": "x" } }),
            "invalid_request",
            "env key",
        ),
        (
            json!({ "env": { "A": "x".repeat(5000) } }),
            "invalid_request",
            "env value",
        ),
        (
            json!({ "idle_timeout_seconds": 10 }),
            "invalid_request",
            "idle_timeout_seconds",
        ),
        (
            json!({ "idle_timeout_seconds": 100000 }),
            "invalid_request",
            "idle_timeout_seconds",
        ),
        (
            json!({ "metadata": (0..33).map(|i| (i.to_string(), "v".to_string())).collect::<std::collections::BTreeMap<_, _>>() }),
            "invalid_request",
            "metadata",
        ),
        (
            json!({ "type": "performance" }),
            "not_supported",
            "performance",
        ),
        (
            json!({ "type": "quantum" }),
            "invalid_request",
            "unknown variant",
        ),
        (
            json!({ "template": "Bad_Name" }),
            "invalid_request",
            "template",
        ),
        (
            json!({ "template": "mine@0" }),
            "invalid_request",
            "revision",
        ),
        (
            json!({ "template": "no-such-template" }),
            "not_found",
            "template",
        ),
        (
            json!({ "resources": { "cpu": 0 } }),
            "invalid_request",
            "resources.cpu",
        ),
        (
            json!({ "resources": { "cpu": 2, "memory": 8 } }),
            "invalid_request",
            "shape",
        ),
        (
            json!({ "name": "x".repeat(101) }),
            "invalid_request",
            "name",
        ),
        (
            json!({ "colour": "blue" }),
            "invalid_request",
            "unknown field `colour`",
        ),
        (
            json!({ "resources": { "gpu": 1 } }),
            "invalid_request",
            "unknown field `gpu`",
        ),
        (json!([1, 2]), "invalid_request", "invalid type"),
    ];
    for (body, want_code, want_text) in cases {
        let (status, v) = h
            .call("POST", "/v1/instances", Some(&acct.key), Some(body.clone()))
            .await;
        let expected = if want_code == "not_found" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        assert_eq!(status, expected, "{body} -> {v}");
        assert_eq!(code(&v), want_code, "{body} -> {v}");
        assert!(message(&v).contains(want_text), "{body} -> {v}");
    }
    // Malformed JSON and oversized bodies.
    let (status, v) = h
        .call_raw("POST", "/v1/instances", Some(&acct.key), "{\"name\": ")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("malformed JSON"), "{v}");
    let big = format!(
        "{{\"name\": \"{}\"}}",
        "x".repeat(warren_core::json::MAX_BODY_BYTES + 10)
    );
    let (status, v) = h
        .call_raw("POST", "/v1/instances", Some(&acct.key), big)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("limit"), "{v}");
    // Only the two cases that need the template (unknown name, cpu/memory pair) reach the lifecycle.
    assert_eq!(
        h.lifecycle.calls(),
        vec!["create", "create"],
        "the API answers every other error itself"
    );
}

#[tokio::test]
async fn create_instance_propagates_limit_errors() {
    let h = Harness::with_config(Config {
        max_awake_instances: Some(1),
        ..test_config()
    });
    let acct = h.signup().await;
    h.lifecycle.fail_next_with(Error::instance_limit(3, 3));
    let (status, v) = h
        .call("POST", "/v1/instances", Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&v), "instance_limit_reached");
    // Internal errors are sanitized.
    h.lifecycle
        .fail_next_with(Error::internal("docker socket vanished"));
    let (status, v) = h
        .call("POST", "/v1/instances", Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(code(&v), "internal");
    assert!(v["error"].get("details").is_none(), "{v}");
    // The fake lifecycle's real rule: the configured awake limit is enforced.
    let id = id_of(&h.create_instance(&acct.key, json!({})).await);
    h.store.lock().instances.get_mut(&id).unwrap().status = InstanceStatus::Running;
    let (status, v) = h
        .call("POST", "/v1/instances", Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "instance_limit_reached");
}

#[tokio::test]
async fn list_and_get_instances() {
    let h = Harness::new();
    let acct = h.signup().await;
    let first = id_of(
        &h.create_instance(&acct.key, json!({ "name": "first" }))
            .await,
    );
    h.clock.advance(5);
    let second = id_of(
        &h.create_instance(&acct.key, json!({ "name": "second" }))
            .await,
    );
    let (status, v) = h.call("GET", "/v1/instances", Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["data"]
    );
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], second, "newest first");
    assert_eq!(data[1]["id"], first);
    for inst in data {
        assert_eq!(
            inst["url"],
            h.config.instance_url(inst["id"].as_str().unwrap())
        );
    }
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{first}"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["name"], "first");
    assert_eq!(v["url"], h.config.instance_url(&first));
    // Unknown, malformed and foreign ids are all 404.
    let (status, v) = h
        .call("GET", "/v1/instances/zzzzzzzzzz", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&v), "not_found");
    let (status, _) = h
        .call("GET", "/v1/instances/NOT-VALID", Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let other = h.signup().await;
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/instances/{first}"),
            Some(&other.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, v) = h.call("GET", "/v1/instances", Some(&other.key), None).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn lifecycle_actions_and_patch() {
    let h = Harness::new();
    let acct = h.signup().await;
    let id = id_of(&h.create_instance(&acct.key, json!({})).await);
    let p = |action: &str| format!("/v1/instances/{id}/{action}");
    for (action, want) in [
        ("stop", "stopping"),
        ("start", "starting"),
        ("sleep", "sleeping"),
        ("restart", "restarting"),
    ] {
        let (status, v) = h.call("POST", &p(action), Some(&acct.key), None).await;
        assert_eq!(status, StatusCode::OK, "{action}: {v}");
        assert_eq!(v, json!({ "id": id, "status": want }), "{action}");
    }
    let (status, v) = h
        .call(
            "PATCH",
            &format!("/v1/instances/{id}"),
            Some(&acct.key),
            Some(json!({ "name": "renamed", "auto_sleep": true, "idle_timeout_seconds": 1200 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "renamed");
    assert_eq!(v["auto_sleep"], true);
    assert_eq!(v["idle_timeout_seconds"], 1200);
    assert_eq!(v["url"], h.config.instance_url(&id));
    let (status, v) = h
        .call(
            "PATCH",
            &format!("/v1/instances/{id}"),
            Some(&acct.key),
            Some(json!({ "template": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("unknown field `template`"), "{v}");
    let (status, v) = h
        .call(
            "PATCH",
            &format!("/v1/instances/{id}"),
            Some(&acct.key),
            Some(json!({ "idle_timeout_seconds": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");

    // Resize, update, fork.
    let (status, v) = h
        .call("POST", &p("resize"), Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("at least one"), "{v}");
    let (status, v) = h
        .call(
            "POST",
            &p("resize"),
            Some(&acct.key),
            Some(json!({ "disk": 8 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["id"], id);
    assert_eq!(v["resources"]["disk"], 8);
    assert!(v.get("status").is_some());
    let (status, v) = h
        .call("POST", &p("update"), Some(&acct.key), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "updating");
    assert_eq!(v["image_ref"], "warren/base:dev");
    assert_eq!(v["template_revision"], 1);
    let (status, v) = h
        .call(
            "POST",
            &p("update"),
            Some(&acct.key),
            Some(json!({ "template": "bad name" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    let (status, v) = h
        .call(
            "POST",
            &p("fork"),
            Some(&acct.key),
            Some(json!({ "name": "copy" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let fork_id = id_of(&v);
    assert_ne!(fork_id, id);
    assert_eq!(v["parent_id"], id);
    assert_eq!(v["name"], "copy");
    assert_eq!(v["status"], "provisioning");
    assert_eq!(v["url"], h.config.instance_url(&fork_id));
    let (status, _) = h
        .call(
            "POST",
            &p("fork"),
            Some(&acct.key),
            Some(json!({ "env": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "fork has no env field");

    // Delete.
    let (status, v) = h
        .call(
            "DELETE",
            &format!("/v1/instances/{id}"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "id": id, "status": "deleting" }));
    let (status, _) = h
        .call("GET", &format!("/v1/instances/{id}"), Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h.call("POST", &p("stop"), Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_logs_and_metrics() {
    let h = Harness::new();
    let acct = h.signup().await;
    let id = id_of(&h.create_instance(&acct.key, json!({})).await);
    let (status, v) = h
        .call(
            "POST",
            &format!("/v1/instances/{id}/exec"),
            Some(&acct.key),
            Some(json!({ "command": "echo hi", "user": "root" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["stdout"], "ran: echo hi");
    assert_eq!(v["duration_ms"], 1);
    for bad in [
        json!({}),
        json!({ "command": "  " }),
        json!({ "command": "ls", "user": "a b" }),
        json!({ "command": "ls", "workdir": "rel" }),
        json!({ "command": "ls", "env": { "1x": "y" } }),
        json!({ "command": "ls", "shell": true }),
    ] {
        let (status, v) = h
            .call(
                "POST",
                &format!("/v1/instances/{id}/exec"),
                Some(&acct.key),
                Some(bad.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {v}");
    }
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/logs?tail=50&since=100"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["logs"], "tail=50 since=100\n");
    let (_, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/logs"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(v["logs"], "tail=200 since=0\n");
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/logs?tail=abc"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    h.store.lock().metrics.push((
        id.clone(),
        warren_core::MetricPoint {
            at: NOW - 60,
            cpu_percent: 1.5,
            memory_bytes: 10,
            disk_bytes: 20,
        },
    ));
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/metrics"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["cpu_percent"], 1.5);
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/metrics?hours=0"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn signed_urls_verify_with_the_same_signer() {
    let h = Harness::new();
    let acct = h.signup().await;
    let id = id_of(&h.create_instance(&acct.key, json!({})).await);
    let token_of = |url: &str| url.split("warren_token=").nth(1).unwrap().to_string();

    // Defaults: default port, "/", one hour.
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/signed-url"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let url = v["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("{}/?warren_token=", h.config.instance_url(&id))),
        "{url}"
    );
    assert_eq!(v["expires_at"], NOW + 3600);
    assert_eq!(h.signer.verify(&token_of(url)).unwrap(), (id.clone(), 0));

    // Explicit non-default port: preview host, token carries the port.
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/signed-url?port=7681&ttl=600&path=/term"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let url = v["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!(
            "{}/term?warren_token=",
            h.config.preview_url(&id, 7681)
        )),
        "{url}"
    );
    assert_eq!(
        url,
        format!(
            "http://{id}-7681.warren.localhost:7337/term?warren_token={}",
            token_of(url)
        )
    );
    assert_eq!(v["expires_at"], NOW + 600);
    assert_eq!(h.signer.verify(&token_of(url)).unwrap(), (id.clone(), 7681));

    // The default port spelled out is the bare URL with a port-0 token.
    let (_, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/signed-url?port=3737"),
            Some(&acct.key),
            None,
        )
        .await;
    let url = v["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("{}/?warren_token=", h.config.instance_url(&id))),
        "{url}"
    );
    assert_eq!(h.signer.verify(&token_of(url)).unwrap(), (id.clone(), 0));

    // Paths are made absolute; an existing query is extended.
    let (_, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/signed-url?path=app%3Fx%3D1"),
            Some(&acct.key),
            None,
        )
        .await;
    let url = v["url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!(
            "{}/app?x=1&warren_token=",
            h.config.instance_url(&id)
        )),
        "{url}"
    );

    // A different signer rejects the token; an expired clock does too.
    let other = warren_core::HmacSigner::new("other", h.clock.clone());
    assert!(other.verify(&token_of(url)).is_err());
    h.clock.advance(3601);
    assert!(h.signer.verify(&token_of(url)).is_err());
    h.clock.set(NOW);

    // Validation.
    for (query, text) in [
        ("ttl=10", "ttl"),
        ("ttl=90000", "ttl"),
        ("port=70000", "port"),
        ("port=-1", "port"),
        ("path=/a%20b", "path"),
    ] {
        let (status, v) = h
            .call(
                "GET",
                &format!("/v1/instances/{id}/signed-url?{query}"),
                Some(&acct.key),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {v}");
        assert!(message(&v).contains(text), "{query}: {v}");
    }
    // Ownership is checked first.
    let other = h.signup().await;
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/signed-url"),
            Some(&other.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
}

#[tokio::test]
async fn backups_and_restore() {
    let h = Harness::new();
    let acct = h.signup().await;
    let id = id_of(&h.create_instance(&acct.key, json!({})).await);
    let (status, v) = h
        .call(
            "POST",
            &format!("/v1/instances/{id}/backups"),
            Some(&acct.key),
            Some(json!({ "note": "before upgrade" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["instance_id"], id);
    assert_eq!(v["note"], "before upgrade");
    assert!(v.get("path").is_none());
    let backup_id = id_of(&v);
    h.clock.advance(1);
    let (status, _) = h
        .call(
            "POST",
            &format!("/v1/instances/{id}/backups"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = h
        .call(
            "GET",
            &format!("/v1/instances/{id}/backups"),
            Some(&acct.key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[1]["id"], backup_id, "newest first");
    let (status, v) = h
        .call(
            "POST",
            &format!("/v1/instances/{id}/restore"),
            Some(&acct.key),
            Some(json!({ "backup_id": backup_id })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["id"], id);
    assert_eq!(v["url"], h.config.instance_url(&id));
    let (status, v) = h
        .call(
            "POST",
            &format!("/v1/instances/{id}/restore"),
            Some(&acct.key),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(message(&v).contains("backup_id"), "{v}");
    let (status, _) = h
        .call(
            "POST",
            &format!("/v1/instances/{id}/restore"),
            Some(&acct.key),
            Some(json!({ "backup_id": "nope" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
