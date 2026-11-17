//! Client tests against the in-crate test server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::json;
use warren_core::agentapi::{ResponseStatus, ResponsesRequest};
use warren_core::{CreateInstanceRequest, ExecRequest, InstanceStatus, InstanceType, NetworkMode};

use crate::testserver::{Request, Response, TestServer};
use crate::{AgentClient, Client, SdkError};

const KEY: &str = "sk_live_0123456789abcdefghijklmnopqrstuvwxyzABCD";

fn instance_json(id: &str, status: &str) -> serde_json::Value {
    json!({
        "id": id, "workspace_id": "ws_abcdefghij0123456789", "name": "Production agent",
        "template": "warren-agent", "template_name": "warren-agent", "template_revision": 1,
        "image_ref": "warren/agent:dev", "type": "default", "status": status, "status_reason": "",
        "resources": { "cpu": 2, "memory": 4, "disk": 4 }, "region": "local",
        "metadata": { "plan": "pro" }, "env": { "APP_ENV": "production" },
        "network": "allow-all", "auto_sleep": true, "idle_timeout_seconds": 900,
        "url": format!("http://{id}.warren.localhost:7337"), "default_port": 3737, "mount_path": "/home/agent",
        "agent_api": true, "parent_id": "", "created_at": 1763000000, "updated_at": 1763000100,
        "started_at": 1763000050, "stopped_at": 0, "slept_at": 0, "last_activity_at": 1763000100,
        "runtime": "docker", "disk_used_bytes": 123456
    })
}

fn assert_auth(req: &Request) {
    assert_eq!(
        req.header("authorization"),
        Some(format!("Bearer {KEY}").as_str()),
        "{} {}",
        req.method,
        req.path
    );
    assert_eq!(
        req.header("x-warren-key"),
        Some(KEY),
        "{} {}",
        req.method,
        req.path
    );
}

// ----- error envelope ---------------------------------------------------------------

#[tokio::test]
async fn error_envelope_is_decoded() {
    let server = TestServer::start(|_| {
        Response::json(
            409,
            json!({ "error": { "code": "instance_limit_reached", "message": "This workspace may run 3 instances at once and 3 are awake.", "details": { "limit": 3, "awake": 3 } } }),
        )
    })
    .await;
    let client = Client::new(server.api_url(), KEY);
    let err = client.start("ab12cd34ef").await.unwrap_err();
    let SdkError::Api(e) = &err else {
        panic!("expected an API error, got {err:?}")
    };
    assert_eq!(e.code, "instance_limit_reached");
    assert_eq!(e.status, 409);
    assert_eq!(
        e.message,
        "This workspace may run 3 instances at once and 3 are awake."
    );
    assert_eq!(e.details.as_ref().unwrap()["limit"], 3);
    assert_eq!(err.status(), Some(409));
    assert_eq!(
        err.to_string(),
        "instance_limit_reached: This workspace may run 3 instances at once and 3 are awake."
    );
    let core: warren_core::Error = err.into();
    assert_eq!(core.code, "instance_limit_reached");
}

#[tokio::test]
async fn non_json_errors_are_synthesised_from_the_status() {
    let server = TestServer::start(|req| match req.path.as_str() {
        "/v1/instances/stopped001" => Response::text(502, "this template has no default port"),
        _ => Response::bytes(500, "text/html", b"<html>boom</html>".to_vec()),
    })
    .await;
    let client = Client::new(server.api_url(), KEY);
    let err = client.instance("stopped001").await.unwrap_err();
    assert_eq!(err.code(), Some("bad_gateway"));
    assert_eq!(err.status(), Some(502));
    assert!(
        err.to_string()
            .contains("this template has no default port"),
        "{err}"
    );
    let err = client.instances().await.unwrap_err();
    assert_eq!(err.code(), Some("internal"));
    assert_eq!(err.status(), Some(500));
}

#[tokio::test]
async fn bad_json_on_success_is_a_decode_error() {
    let server = TestServer::start(|_| Response::text(200, "not json")).await;
    let client = Client::new(server.api_url(), KEY);
    let err = client.instances().await.unwrap_err();
    assert!(matches!(err, SdkError::Decode(_)), "{err:?}");
    assert!(err.to_string().contains("not json"), "{err}");
    assert_eq!(err.status(), None);
}

#[tokio::test]
async fn connection_failures_are_http_errors() {
    let server = TestServer::start(|_| Response::text(200, "{}")).await;
    let url = server.api_url();
    drop(server);
    let client = Client::new(url, KEY);
    let err = client.instances().await.unwrap_err();
    assert!(matches!(err, SdkError::Http(_)), "{err:?}");
    let core: warren_core::Error = err.into();
    assert_eq!(core.code, "try_again");
    assert!(core.message.contains("/v1/instances"), "{}", core.message);
}

// ----- sandbox api ------------------------------------------------------------------

#[tokio::test]
async fn sandbox_list_instances_sends_both_auth_headers() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/instances") => Response::json(
            200,
            json!({ "data": [instance_json("ab12cd34ef", "running")] }),
        ),
        _ => Response::not_found(),
    })
    .await;
    let client = Client::new(server.api_url(), KEY);
    let list = client.instances().await.unwrap();
    assert_eq!(list.len(), 1);
    let inst = &list[0];
    assert_eq!(inst.id, "ab12cd34ef");
    assert_eq!(inst.status, InstanceStatus::Running);
    assert_eq!(inst.kind, InstanceType::Default);
    assert_eq!(inst.network, NetworkMode::AllowAll);
    assert_eq!(inst.shape.memory, 4);
    assert_eq!(inst.url, "http://ab12cd34ef.warren.localhost:7337");
    assert_eq!(inst.metadata["plan"], "pro");
    let req = server.last_request();
    assert_auth(&req);
    assert_eq!(req.header("accept"), Some("application/json"));
    assert!(req.header("user-agent").unwrap().starts_with("warren-sdk/"));
}

#[tokio::test]
async fn sandbox_create_instance_posts_the_request_body() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v1/instances") => {
            let body = req.json();
            assert_eq!(
                body,
                json!({ "template": "warren-agent", "name": "bot", "auto_sleep": true })
            );
            Response::json(201, instance_json("zz99yy88xx", "provisioning"))
        }
        _ => Response::not_found(),
    })
    .await;
    let client = Client::new(server.url(), KEY); // no /v1: added by the client
    let req = CreateInstanceRequest {
        template: Some("warren-agent".into()),
        name: Some("bot".into()),
        auto_sleep: Some(true),
        ..Default::default()
    };
    let inst = client.create_instance(&req).await.unwrap();
    assert_eq!(inst.id, "zz99yy88xx");
    assert_eq!(inst.status, InstanceStatus::Provisioning);
    assert_eq!(
        server.last_request().header("content-type"),
        Some("application/json")
    );
}

#[tokio::test]
async fn sandbox_query_parameters_are_encoded() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/instances/ab12cd34ef/signed-url") => {
            Response::json(200, json!({ "url": "http://ab12cd34ef-7681.warren.localhost:7337/?warren_token=t", "expires_at": 1763003600 }))
        }
        ("GET", "/v1/instances/ab12cd34ef/logs") => Response::json(200, json!({ "logs": "line1\nline2\n" })),
        ("GET", "/v1/instances/ab12cd34ef/metrics") => {
            Response::json(200, json!({ "data": [{ "at": 1, "cpu_percent": 1.5, "memory_bytes": 2, "disk_bytes": 3 }] }))
        }
        _ => Response::not_found(),
    })
    .await;
    let client = Client::new(server.api_url(), KEY);

    let signed = client
        .signed_url("ab12cd34ef", 7681, 3600, "/a b?c=d")
        .await
        .unwrap();
    assert_eq!(signed.expires_at, 1763003600);
    let req = server.last_request();
    assert_eq!(req.query, "port=7681&ttl=3600&path=%2Fa%20b%3Fc%3Dd");
    assert_eq!(req.query_param("path").as_deref(), Some("/a b?c=d"));

    assert_eq!(
        client.logs("ab12cd34ef", 200, 1763000000).await.unwrap(),
        "line1\nline2\n"
    );
    assert_eq!(server.last_request().query, "tail=200&since=1763000000");
    assert_eq!(
        client.logs("ab12cd34ef", 0, 0).await.unwrap(),
        "line1\nline2\n"
    );
    assert_eq!(server.last_request().query, "");

    let points = client.metrics("ab12cd34ef", 1).await.unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].cpu_percent, 1.5);
    assert_eq!(server.last_request().query, "hours=1");
}

#[tokio::test]
async fn sandbox_lifecycle_and_misc_endpoints_decode() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/health") => Response::json(200, json!({ "healthy": true, "version": "dev", "runtimes": ["docker"], "time": 1763000000 })),
        ("GET", "/v1/version") => Response::json(200, json!({ "version": "dev" })),
        ("POST", "/v1/instances/ab12cd34ef/stop") => Response::json(200, json!({ "id": "ab12cd34ef", "status": "stopping" })),
        ("DELETE", "/v1/instances/ab12cd34ef") => Response::json(200, json!({ "id": "ab12cd34ef", "status": "deleting" })),
        ("POST", "/v1/instances/ab12cd34ef/resize") => {
            assert_eq!(req.json(), json!({ "disk": 8 }));
            Response::json(200, json!({ "id": "ab12cd34ef", "status": "running", "resources": { "cpu": 2, "memory": 4, "disk": 8 } }))
        }
        ("POST", "/v1/instances/ab12cd34ef/update") => {
            assert_eq!(req.json(), json!({ "template": "my-tpl@2" }));
            Response::json(200, json!({ "id": "ab12cd34ef", "status": "updating", "image_ref": "x/y:z", "template_revision": 2 }))
        }
        ("POST", "/v1/instances/ab12cd34ef/exec") => {
            assert_eq!(req.json(), json!({ "command": "echo hi" }));
            Response::json(200, json!({ "exit_code": 0, "stdout": "hi\n", "stderr": "", "duration_ms": 12 }))
        }
        // Path segments arrive percent-encoded; axum decodes them on the server.
        ("GET", "/v1/templates/my-tpl%402") => Response::json(
            200,
            json!({ "name": "my-tpl", "scope": "workspace", "workspace_id": "ws_x", "image_ref": "x/y:z", "default_port": 3737,
                "mount_path": "/home/agent", "agent": false, "default_shape": { "cpu": 2, "memory": 4, "disk": 4 },
                "revision": 2, "created_at": 1, "updated_at": 2 }),
        ),
        ("DELETE", "/v1/templates/my-tpl") => Response::json(200, json!({ "name": "my-tpl", "deleted": true })),
        ("GET", "/v1/workspace") => Response::json(
            200,
            json!({ "id": "ws_x", "name": "me", "created_at": 1, "awake_limit": 3, "awake_count": 1 }),
        ),
        ("POST", "/v1/instances/ab12cd34ef/crons") => {
            assert_eq!(req.json(), json!({ "prompt": "summarise", "schedule": "0 9 * * *", "timezone": "Europe/Dublin" }));
            Response::json(
                201,
                json!({ "id": "c1", "instance_id": "ab12cd34ef", "workspace_id": "ws_x", "prompt": "summarise", "schedule": "0 9 * * *",
                    "timezone": "Europe/Dublin", "enabled": true, "created_at": 1, "updated_at": 1 }),
            )
        }
        ("DELETE", "/v1/instances/ab12cd34ef/crons/c1") => Response::json(200, json!({ "id": "c1", "deleted": true })),
        _ => Response::not_found(),
    })
    .await;
    let client = Client::new(server.api_url(), KEY);

    let health = client.health().await.unwrap();
    assert!(health.healthy);
    assert_eq!(health.runtimes, vec!["docker".to_string()]);
    assert_eq!(client.version().await.unwrap(), "dev");

    let s = client.stop("ab12cd34ef").await.unwrap();
    assert_eq!(s.status, InstanceStatus::Stopping);
    assert_eq!(
        client.delete_instance("ab12cd34ef").await.unwrap().status,
        InstanceStatus::Deleting
    );

    let r = client
        .resize(
            "ab12cd34ef",
            &warren_core::ShapeRequest {
                disk: Some(8),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.resources.disk, 8);
    let u = client.update("ab12cd34ef", Some("my-tpl@2")).await.unwrap();
    assert_eq!(u.template_revision, 2);
    assert_eq!(u.status, InstanceStatus::Updating);

    let out = client
        .exec(
            "ab12cd34ef",
            &ExecRequest {
                command: "echo hi".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, "hi\n");
    assert!(!out.truncated);

    let tpl = client.template("my-tpl@2").await.unwrap();
    assert_eq!(tpl.revision, 2);
    assert_eq!(server.last_request().path, "/v1/templates/my-tpl%402");
    assert!(client.delete_template("my-tpl").await.unwrap().deleted);

    let ws = client.workspace().await.unwrap();
    assert_eq!(ws.workspace.id, "ws_x");
    assert_eq!(ws.awake_limit, Some(3));
    assert_eq!(ws.awake_count, 1);

    let mut cron = crate::CronRequest::new("summarise", "0 9 * * *");
    cron.timezone = Some("Europe/Dublin".into());
    let created = client.create_cron("ab12cd34ef", &cron).await.unwrap();
    assert_eq!(created.id, "c1");
    client.delete_cron("ab12cd34ef", "c1").await.unwrap();
    for req in server.requests() {
        assert_auth(&req);
    }
}

#[tokio::test]
async fn signup_sends_no_credentials() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v1/signup") => {
            assert_eq!(req.json(), json!({ "name": "me" }));
            Response::json(
                201,
                json!({ "workspace": { "id": "ws_x", "name": "me", "created_at": 1 }, "api_key": KEY, "key_id": "k1" }),
            )
        }
        _ => Response::not_found(),
    })
    .await;
    let client = Client::new(server.api_url(), "sk_live_stale");
    let signup = client.signup(Some("me")).await.unwrap();
    assert_eq!(signup.api_key, KEY);
    assert_eq!(signup.workspace.id, "ws_x");
    assert_eq!(signup.key_id, "k1");
    let req = server.last_request();
    assert_eq!(req.header("authorization"), None);
    assert_eq!(req.header("x-warren-key"), None);
    let authed = client.with_key(signup.api_key);
    assert_eq!(authed.key, KEY);

    // An empty key sends no auth headers either.
    let anon = Client::new(server.api_url(), "");
    anon.signup(None).await.unwrap_err(); // body {} does not match the handler's assertion -> 500
    assert_eq!(server.last_request().header("authorization"), None);
}

#[tokio::test]
async fn wait_for_status_polls_until_the_target() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let server = TestServer::start(move |req| {
        assert_eq!(req.path, "/v1/instances/ab12cd34ef");
        let n = c.fetch_add(1, Ordering::SeqCst);
        Response::json(
            200,
            instance_json("ab12cd34ef", if n < 2 { "starting" } else { "running" }),
        )
    })
    .await;
    let client = Client::new(server.api_url(), KEY);
    let started = Instant::now();
    let inst = client
        .wait_for_status(
            "ab12cd34ef",
            InstanceStatus::Running,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(inst.status, InstanceStatus::Running);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "two polls apart: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn wait_for_status_times_out_and_fails_fast() {
    let server = TestServer::start(|req| match req.path.as_str() {
        "/v1/instances/slow000000" => Response::json(200, instance_json("slow000000", "starting")),
        "/v1/instances/dead000000" => {
            let mut v = instance_json("dead000000", "failed");
            v["status_reason"] = json!("image pull failed");
            Response::json(200, v)
        }
        "/v1/instances/gone000000" => Response::error(404, "not_found", "instance not found"),
        _ => Response::not_found(),
    })
    .await;
    let client = Client::new(server.api_url(), KEY);

    let err = client
        .wait_for_status(
            "slow000000",
            InstanceStatus::Running,
            Duration::from_millis(1200),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("timeout"));
    assert!(err.is_timeout());
    assert!(err.to_string().contains("last status: starting"), "{err}");

    let err = client
        .wait_for_status(
            "dead000000",
            InstanceStatus::Running,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("conflict"));
    assert!(err.to_string().contains("image pull failed"), "{err}");

    let err = client
        .wait_for_status(
            "gone000000",
            InstanceStatus::Running,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(err.is_not_found());

    let inst = client
        .wait_for_status(
            "gone000000",
            InstanceStatus::Deleted,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(inst.status, InstanceStatus::Deleted);
    assert_eq!(inst.id, "gone000000");
}

#[tokio::test]
async fn client_with_resolve_pins_a_host_to_the_test_server() {
    let server = TestServer::start(|req| {
        assert_eq!(req.path, "/v1/health");
        assert!(
            req.header("host")
                .unwrap()
                .starts_with("api.warren.localhost"),
            "{:?}",
            req.header("host")
        );
        Response::json(
            200,
            json!({ "healthy": true, "version": "dev", "runtimes": ["docker"], "time": 1 }),
        )
    })
    .await;
    let http = crate::client_with_resolve(&[("api.warren.localhost", server.addr)]);
    let client = Client::with_http(
        format!("http://api.warren.localhost:{}/v1", server.addr.port()),
        KEY,
        http,
    );
    assert!(client.health().await.unwrap().healthy);
    assert!(crate::try_client_with_resolve(&[]).is_ok());
}

// ----- agent api --------------------------------------------------------------------

#[tokio::test]
async fn agent_respond_decodes_the_response_object() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v1/responses") => {
            assert_eq!(req.json(), json!({ "input": "hello", "session_id": "s1", "stream": false }));
            Response::json(
                200,
                json!({ "id": "r1", "session_id": "s1", "status": "completed", "output_text": "echo: hello",
                    "created_at": 1763000000000i64, "completed_at": 1763000001000i64, "usage": { "input_tokens": 1 } }),
            )
        }
        ("GET", "/v1/health") => Response::json(200, json!({ "healthy": true, "agent_ready": true, "driver": "echo", "version": "dev" })),
        ("POST", "/v1/responses/r1/cancel") => Response::json(200, json!({ "id": "r1", "status": "cancelled" })),
        ("GET", "/v1/sessions") => Response::json(200, json!({ "data": [{ "id": "s1", "name": "", "created_at": 1, "updated_at": 2, "message_count": 2 }] })),
        ("GET", "/v1/sessions/s1") => Response::json(
            200,
            json!({ "id": "s1", "name": "", "created_at": 1, "updated_at": 2, "messages": [{ "role": "user", "content": "hello", "at": 1 }, { "role": "assistant", "content": "echo: hello", "at": 2 }] }),
        ),
        ("PATCH", "/v1/sessions/s1") => {
            assert_eq!(req.json(), json!({ "name": "renamed" }));
            Response::json(200, json!({ "id": "s1", "name": "renamed" }))
        }
        ("DELETE", "/v1/sessions/s1") => Response::json(200, json!({ "id": "s1", "deleted": true })),
        ("GET", "/v1/models") => Response::json(200, json!({ "data": [{ "id": "default" }] })),
        _ => Response::not_found(),
    })
    .await;
    let agent = AgentClient::new(format!("{}/", server.url()), KEY);
    let req = ResponsesRequest {
        input: "hello".into(),
        session_id: Some("s1".into()),
        ..Default::default()
    };
    let resp = agent.respond(&req).await.unwrap();
    assert_eq!(resp.id, "r1");
    assert_eq!(resp.status, ResponseStatus::Completed);
    assert_eq!(resp.output_text, "echo: hello");
    assert_eq!(resp.completed_at, Some(1763000001000));
    assert_auth(&server.last_request());

    let health = agent.health().await.unwrap();
    assert_eq!(health.driver, "echo");
    assert_eq!(
        agent.cancel("r1").await.unwrap().status,
        ResponseStatus::Cancelled
    );
    assert_eq!(agent.sessions().await.unwrap()[0].message_count, 2);
    assert_eq!(
        agent.messages("s1").await.unwrap()[1].content,
        "echo: hello"
    );
    agent.rename_session("s1", "renamed").await.unwrap();
    agent.delete_session("s1").await.unwrap();
    assert_eq!(agent.models().await.unwrap()[0].id, "default");
}

#[tokio::test]
async fn agent_stream_parses_events_across_chunk_boundaries() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v1/responses") => {
            let body = req.json();
            assert_eq!(body["stream"], true);
            assert_eq!(req.header("accept"), Some("text/event-stream"));
            Response::sse(&[
                ("event: response.created\ndata: {\"id\":\"r1\",\"sess", 0),
                ("ion_id\":\"s1\"}\n\nevent: response.output_text.delta\r\ndata: {\"delta\":\"ec", 30),
                ("ho: \"}\r\n\r\n: comment\r", 30),
                ("\nevent: response.output_text.delta\ndata: {\"delta\":\"h", 30),
                ("\u{e9}llo\"}\n\nevent: response.completed\ndata: {\"id\":\"r1\",\"session_id\":\"s1\",", 30),
                ("\"output_text\":\"echo: h\u{e9}llo\",\"status\":\"completed\"}\n\n", 30),
            ])
        }
        _ => Response::not_found(),
    })
    .await;
    let agent = AgentClient::new(server.url(), KEY);
    let req = ResponsesRequest {
        input: "héllo".into(),
        ..Default::default()
    };
    let events: Vec<_> = agent.stream(&req).collect().await;
    let events: Vec<_> = events.into_iter().map(|e| e.unwrap()).collect();
    let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        names,
        [
            "response.created",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    assert_eq!(events[0].data["session_id"], "s1");
    let text: String = events.iter().filter_map(|e| e.str_field("delta")).collect();
    assert_eq!(text, "echo: héllo");
    assert_eq!(events[3].str_field("output_text"), Some("echo: héllo"));
    assert_auth(&server.last_request());
}

#[tokio::test]
async fn agent_stream_reports_http_errors_as_the_first_item() {
    let server =
        TestServer::start(|_| Response::error(404, "not_supported", "this template has no agent"))
            .await;
    let agent = AgentClient::new(server.url(), KEY);
    let mut stream = agent.stream(&ResponsesRequest {
        input: "x".into(),
        ..Default::default()
    });
    let first = stream.next().await.unwrap().unwrap_err();
    assert_eq!(first.code(), Some("not_supported"));
    assert_eq!(first.status(), Some(404));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn agent_files_round_trip_raw_bytes() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("PUT", "/v1/files/content") => {
            assert_eq!(req.query_param("path").as_deref(), Some("notes/a b.txt"));
            assert_eq!(req.header("content-type"), Some("application/octet-stream"));
            assert_eq!(req.body, b"hello\x00world");
            Response::json(200, json!({ "path": "notes/a b.txt", "size": 11 }))
        }
        ("GET", "/v1/files/content") => Response::bytes(200, "text/plain", b"hello\x00world".to_vec()),
        ("GET", "/v1/files") => {
            assert_eq!(req.query_param("path").as_deref(), Some("."));
            Response::json(200, json!({ "path": ".", "entries": [{ "name": "notes", "type": "dir", "size": 0, "modified_at": 1 }] }))
        }
        ("DELETE", "/v1/files") => Response::json(200, json!({ "path": "notes/a b.txt", "deleted": true })),
        ("POST", "/v1/files/move") => {
            assert_eq!(req.json(), json!({ "from": "a", "to": "b" }));
            Response::json(200, json!({ "from": "a", "to": "b" }))
        }
        ("GET", "/v1/files/download") => Response::bytes(200, "application/gzip", vec![0x1f, 0x8b, 8, 0]),
        _ => Response::not_found(),
    })
    .await;
    let agent = AgentClient::new(server.url(), KEY);
    let written = agent
        .write_file("notes/a b.txt", b"hello\x00world".to_vec())
        .await
        .unwrap();
    assert_eq!(written.size, 11);
    assert_eq!(
        agent.read_file("notes/a b.txt").await.unwrap(),
        b"hello\x00world"
    );
    let listing = agent.list_files("").await.unwrap();
    assert_eq!(listing.entries[0].kind, "dir");
    assert!(agent.delete_file("notes/a b.txt").await.unwrap().deleted);
    agent.move_file("a", "b").await.unwrap();
    assert_eq!(
        agent.download("notes").await.unwrap(),
        vec![0x1f, 0x8b, 8, 0]
    );
    for req in server.requests() {
        assert_auth(&req);
    }
}

#[tokio::test]
async fn agent_exec_sends_the_instance_token() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v1/exec") => {
            assert_eq!(req.header("x-warren-instance-token"), Some("tok123"));
            assert_eq!(req.json(), json!({ "command": "uname -r", "user": "root", "timeout_seconds": 5 }));
            Response::json(200, json!({ "exit_code": 0, "stdout": "4.19.0-gvisor\n", "stderr": "", "duration_ms": 3, "truncated": false }))
        }
        _ => Response::not_found(),
    })
    .await;
    let agent = AgentClient::new(server.url(), KEY);
    let req = ExecRequest {
        command: "uname -r".into(),
        user: "root".into(),
        timeout_seconds: 5,
        ..Default::default()
    };
    let out = agent.exec("tok123", &req).await.unwrap();
    assert_eq!(out.stdout, "4.19.0-gvisor\n");
    assert_auth(&server.last_request());
}

#[tokio::test]
async fn sandbox_client_derives_agent_clients() {
    let client = Client::new("http://api.warren.localhost:7337/v1", KEY);
    let inst: warren_core::Instance =
        serde_json::from_value(instance_json("ab12cd34ef", "running")).unwrap();
    let agent = client.agent_for(&inst);
    assert_eq!(agent.base_url, "http://ab12cd34ef.warren.localhost:7337");
    assert_eq!(agent.key, KEY);
    let agent = client.agent("http://ab12cd34ef-7681.warren.localhost:7337/v1/");
    assert_eq!(
        agent.base_url,
        "http://ab12cd34ef-7681.warren.localhost:7337"
    );
}
