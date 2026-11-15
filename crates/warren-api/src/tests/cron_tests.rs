//! Crons: agent-only, validation, next-run computation, patch semantics.

use chrono::TimeZone;
use http::StatusCode;
use serde_json::{Value, json};

use super::harness::{Harness, NOW, code, message};

async fn agent_and_base(h: &Harness, key: &str) -> (String, String) {
    let agent = h
        .create_instance(key, json!({ "template": "warren-agent" }))
        .await;
    let base = h.create_instance(key, json!({})).await;
    (
        agent["id"].as_str().unwrap().to_string(),
        base["id"].as_str().unwrap().to_string(),
    )
}

fn crons_path(id: &str) -> String {
    format!("/v1/instances/{id}/crons")
}

#[tokio::test]
async fn crons_need_an_agent_template() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (_, base) = agent_and_base(&h, &acct.key).await;
    let body = json!({ "prompt": "summarize inbox", "schedule": "0 9 * * *" });
    let (status, v) = h
        .call("POST", &crons_path(&base), Some(&acct.key), Some(body))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(code(&v), "not_supported");
    assert!(message(&v).contains("agent"), "{v}");
    let (status, v) = h
        .call("GET", &crons_path(&base), Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code(&v), "not_supported");
    // Unknown or foreign instances are 404 before anything else.
    let (status, _) = h
        .call("GET", &crons_path("zzzzzzzzzz"), Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cron_validation() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (agent, _) = agent_and_base(&h, &acct.key).await;
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "prompt": "p", "schedule": "* * * *" }),
            "five fields",
        ),
        (
            json!({ "prompt": "p", "schedule": "0 0 * * * *" }),
            "five fields",
        ),
        (
            json!({ "prompt": "p", "schedule": "@daily" }),
            "five fields",
        ),
        (
            json!({ "prompt": "p", "schedule": "99 * * * *" }),
            "invalid cron schedule",
        ),
        (
            json!({ "prompt": "p", "schedule": "0 9 * * *", "timezone": "Mars/Olympus" }),
            "unknown timezone",
        ),
        (json!({ "prompt": "", "schedule": "0 9 * * *" }), "prompt"),
        (
            json!({ "prompt": "   ", "schedule": "0 9 * * *" }),
            "prompt",
        ),
        (
            json!({ "prompt": "x".repeat(8193), "schedule": "0 9 * * *" }),
            "8192",
        ),
        (json!({ "schedule": "0 9 * * *" }), "missing field `prompt`"),
        (json!({ "prompt": "p" }), "missing field `schedule`"),
        (
            json!({ "prompt": "p", "schedule": "0 9 * * *", "cron": "x" }),
            "unknown field `cron`",
        ),
        (
            json!({ "prompt": "p", "schedule": "0 9 * * *", "name": "n".repeat(101) }),
            "name",
        ),
    ];
    for (body, text) in cases {
        let (status, v) = h
            .call(
                "POST",
                &crons_path(&agent),
                Some(&acct.key),
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {v}");
        assert_eq!(code(&v), "invalid_request", "{body} -> {v}");
        assert!(message(&v).contains(text), "{body} -> {v}");
    }
    assert!(h.store.lock().crons.is_empty());
}

#[tokio::test]
async fn cron_create_computes_next_run_in_timezone() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (agent, _) = agent_and_base(&h, &acct.key).await;
    let body = json!({ "prompt": "morning report", "schedule": " 30  9 * * * ", "timezone": "America/New_York", "name": "report" });
    let (status, v) = h
        .call("POST", &crons_path(&agent), Some(&acct.key), Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["instance_id"], agent);
    assert_eq!(v["workspace_id"], acct.workspace_id);
    assert_eq!(v["name"], "report");
    assert_eq!(v["prompt"], "morning report");
    assert_eq!(v["schedule"], "30 9 * * *", "whitespace normalized");
    assert_eq!(v["timezone"], "America/New_York");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["created_at"], NOW);
    assert_eq!(v["updated_at"], NOW);
    assert!(v.get("last_run_at").is_none());
    // 2026-01-01 09:30 in New York (EST, UTC-5) is 14:30 UTC.
    let expected = chrono_tz::America::New_York
        .with_ymd_and_hms(2026, 1, 1, 9, 30, 0)
        .unwrap()
        .timestamp();
    assert_eq!(expected, NOW + 14 * 3600 + 30 * 60);
    assert_eq!(v["next_run_at"], expected);
    assert_eq!(
        v["next_run_at"],
        crate::next_run_at("30 9 * * *", "America/New_York", NOW).unwrap()
    );
    let stored = h.store.lock().crons[0].clone();
    assert_eq!(stored.next_run_at, expected);
    assert_eq!(stored.id, v["id"].as_str().unwrap());

    // No timezone means UTC; disabled crons still get a next run.
    let (status, v) = h
        .call(
            "POST",
            &crons_path(&agent),
            Some(&acct.key),
            Some(json!({ "prompt": "nightly", "schedule": "0 0 * * *", "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["timezone"], "");
    assert_eq!(v["enabled"], false);
    assert_eq!(v["next_run_at"], NOW + 86_400);
}

#[tokio::test]
async fn cron_patch_recomputes_next_run_only_when_schedule_or_timezone_change() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (agent, _) = agent_and_base(&h, &acct.key).await;
    let (_, v) = h
        .call(
            "POST",
            &crons_path(&agent),
            Some(&acct.key),
            Some(json!({ "prompt": "noon", "schedule": "0 12 * * *" })),
        )
        .await;
    let cid = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["next_run_at"], NOW + 12 * 3600);
    let path = format!("{}/{cid}", crons_path(&agent));

    // Advance the clock: a cosmetic patch keeps next_run_at.
    h.clock.advance(3600);
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            Some(&acct.key),
            Some(json!({ "name": "renamed", "enabled": false, "prompt": "noon v2" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "renamed");
    assert_eq!(v["enabled"], false);
    assert_eq!(v["prompt"], "noon v2");
    assert_eq!(v["next_run_at"], NOW + 12 * 3600);
    assert_eq!(v["updated_at"], NOW + 3600);

    // Same schedule written differently is not a change.
    let (_, v) = h
        .call(
            "PATCH",
            &path,
            Some(&acct.key),
            Some(json!({ "schedule": "0  12 * * *" })),
        )
        .await;
    assert_eq!(v["next_run_at"], NOW + 12 * 3600);

    // Berlin is UTC+1 in January: noon Berlin is 11:00 UTC.
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            Some(&acct.key),
            Some(json!({ "timezone": "Europe/Berlin" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["timezone"], "Europe/Berlin");
    assert_eq!(v["next_run_at"], NOW + 11 * 3600);

    // A new schedule is evaluated from the current clock (NOW + 1h).
    let (_, v) = h
        .call(
            "PATCH",
            &path,
            Some(&acct.key),
            Some(json!({ "schedule": "*/15 * * * *" })),
        )
        .await;
    assert_eq!(v["schedule"], "*/15 * * * *");
    assert_eq!(v["next_run_at"], NOW + 3600 + 900);

    // Invalid patches change nothing.
    for bad in [
        json!({ "schedule": "x" }),
        json!({ "timezone": "Nowhere" }),
        json!({ "prompt": "" }),
        json!({ "id": "y" }),
    ] {
        let (status, v) = h
            .call("PATCH", &path, Some(&acct.key), Some(bad.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {v}");
    }
    let (_, v) = h.call("GET", &path, Some(&acct.key), None).await;
    assert_eq!(v["schedule"], "*/15 * * * *");
    assert_eq!(v["timezone"], "Europe/Berlin");
}

#[tokio::test]
async fn cron_list_get_delete_and_ownership() {
    let h = Harness::new();
    let acct = h.signup().await;
    let (agent, _) = agent_and_base(&h, &acct.key).await;
    let (_, a) = h
        .call(
            "POST",
            &crons_path(&agent),
            Some(&acct.key),
            Some(json!({ "prompt": "a", "schedule": "0 1 * * *" })),
        )
        .await;
    h.clock.advance(1);
    let (_, b) = h
        .call(
            "POST",
            &crons_path(&agent),
            Some(&acct.key),
            Some(json!({ "prompt": "b", "schedule": "0 2 * * *" })),
        )
        .await;
    let (status, v) = h
        .call("GET", &crons_path(&agent), Some(&acct.key), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], b["id"], "newest first");
    assert_eq!(data[1]["id"], a["id"]);
    let cid = a["id"].as_str().unwrap();
    let path = format!("{}/{cid}", crons_path(&agent));
    let (status, v) = h.call("GET", &path, Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["prompt"], "a");
    // Another workspace sees nothing.
    let other = h.signup().await;
    let (status, _) = h.call("GET", &path, Some(&other.key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h.call("DELETE", &path, Some(&other.key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Delete.
    let (status, v) = h.call("DELETE", &path, Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!({ "id": cid, "deleted": true }));
    let (status, _) = h.call("GET", &path, Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h.call("DELETE", &path, Some(&acct.key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.store.lock().crons.len(), 1);
}
