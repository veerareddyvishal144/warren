//! End-to-end tests against a real `warrend` and Docker. Every test is
//! `#[ignore]`d; run them with
//! `cargo test -p warren-e2e -- --ignored --test-threads=1` or `scripts/e2e.sh`.
//!
//! Each test starts its own daemon on a free port with a fresh data directory
//! and the static gateway as `--gateway-bin`, signs up its own workspace,
//! deletes the instances it created (also after a failure) and prints the
//! daemon log tail when it fails. The template images (`warren/base:dev`,
//! `warren/agent:dev`) must exist locally; `alpine:3.20` and
//! `node:24-bookworm-slim` are pulled by the daemon when the templates test
//! registers them (`scripts/e2e.sh` pulls them beforehand).

use std::{
    collections::BTreeMap, future::Future, panic::AssertUnwindSafe, sync::Arc, time::Duration,
};

use futures::{FutureExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use warren_core::{
    COOKIE_NAME, Config, CreateInstanceRequest, ENV_AGENT_DRIVER, Error, ExecRequest, ForkRequest,
    GATEWAY_PORT, Instance, InstanceStatus, InstanceType, LedgerKind, MICROS_PER_DOLLAR,
    PatchInstanceRequest, RegisterTemplateRequest, Result, Shape, ShapeRequest, TERMINAL_PORT,
    TemplateScope,
    agentapi::{EVENT_COMPLETED, EVENT_CREATED, EVENT_DELTA, ResponseStatus, ResponsesRequest},
    errors::{
        CODE_CONFLICT, CODE_FORBIDDEN, CODE_INSTANCE_LIMIT_REACHED, CODE_INVALID_API_KEY,
        CODE_INVALID_REQUEST, CODE_NOT_FOUND, CODE_NOT_SUPPORTED,
    },
};
use warren_e2e::{
    Auth, Daemon, METER_INTERVAL, SdkError, SseEvent, TRANSITION_TIMEOUT, Workspace, ensure,
    event_names, expect_status, fail, header, now_unix, poll_until, session_cookie,
};
use warren_sdk::{CronPatchRequest, CronRequest, EventStream, SignupResponse};

/// Every case must finish well inside the five-minute budget.
const CASE_TIMEOUT: Duration = Duration::from_secs(290);

/// What a test body gets: the daemon, its own workspace and the signup answer.
#[derive(Clone)]
struct Ctx {
    daemon: Arc<Daemon>,
    ws: Workspace,
    signup: SignupResponse,
}

/// Start a daemon, sign up, run `body`, clean up, print the log tail on failure.
async fn run_case<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Ctx) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let daemon = match Daemon::start().await {
        Ok(d) => Arc::new(d),
        Err(e) => panic!("{name}: cannot start warrend: {e}"),
    };
    let (ws, signup) = match Workspace::signup(daemon.clone(), &format!("e2e-{name}")).await {
        Ok(x) => x,
        Err(e) => {
            daemon.dump_log_tail(80);
            panic!("{name}: signup failed: {e}");
        }
    };
    let cx = Ctx {
        daemon: daemon.clone(),
        ws: ws.clone(),
        signup,
    };
    let outcome =
        tokio::time::timeout(CASE_TIMEOUT, AssertUnwindSafe(body(cx)).catch_unwind()).await;
    ws.cleanup().await;
    let result = match outcome {
        Ok(Ok(r)) => r,
        Ok(Err(payload)) => Err(fail(format!(
            "panicked: {}",
            panic_message(payload.as_ref())
        ))),
        Err(_) => Err(fail(format!("timed out after {CASE_TIMEOUT:?}"))),
    };
    if let Err(e) = result {
        daemon.dump_log_tail(120);
        panic!("{name} failed: {e}");
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// An exec request for a shell command as the image user.
fn cmd(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.to_string(),
        ..Default::default()
    }
}

/// API.md answers lifecycle actions with a transitional status (`stopping`,
/// `starting`, ...); an implementation that completes the transition before it
/// answers reports the final one. Both are accepted.
fn settles(got: InstanceStatus, transitional: InstanceStatus, done: InstanceStatus) -> bool {
    got == transitional || got == done
}

/// Assert an SDK call failed with `status` and `code`.
fn expect_api_error<T: std::fmt::Debug>(
    what: &str,
    r: std::result::Result<T, SdkError>,
    status: u16,
    code: &str,
) -> Result<()> {
    match r {
        Err(e) if e.status() == Some(status) && e.code() == Some(code) => Ok(()),
        Err(e) => Err(fail(format!("{what}: expected {status} {code}, got {e}"))),
        Ok(v) => Err(fail(format!(
            "{what}: expected {status} {code}, got success: {v:?}"
        ))),
    }
}

/// Read a gateway `GET /v1/health` answer: it must be `200` with
/// `"healthy": true`; the whole document is returned for further checks
/// (for example the `wrapped` object of an injected gateway).
async fn health_json(resp: reqwest::Response) -> Result<Value> {
    let body = expect_status(resp, 200, None).await?;
    let health: Value =
        serde_json::from_slice(&body).map_err(|e| fail(format!("gateway health JSON: {e}")))?;
    ensure!(health["healthy"] == true, "gateway health: {health}");
    Ok(health)
}

/// Drain an SDK event stream within `timeout`.
async fn collect_stream(stream: EventStream, timeout: Duration) -> Result<Vec<SseEvent>> {
    let mut stream = Box::pin(stream);
    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            events.push(ev?);
        }
        Ok::<(), Error>(())
    };
    match tokio::time::timeout(timeout, drain).await {
        Ok(r) => r?,
        Err(_) => return Err(fail(format!("stream did not finish within {timeout:?}"))),
    }
    Ok(events)
}

/// Forks and sources woken after a copy may settle as `running`, `stopped` or
/// `sleeping`; start them when needed and return the running instance.
async fn ensure_running(ws: &Workspace, id: &str) -> Result<Instance> {
    let settled = poll_until(
        "a settled status",
        TRANSITION_TIMEOUT,
        Duration::from_secs(1),
        || async move {
            let inst = ws.sdk().instance(id).await?;
            let done = matches!(
                inst.status,
                InstanceStatus::Running
                    | InstanceStatus::Stopped
                    | InstanceStatus::Sleeping
                    | InstanceStatus::Failed
            );
            Ok::<_, Error>(done.then_some(inst))
        },
    )
    .await?;
    match settled.status {
        InstanceStatus::Running => Ok(settled),
        InstanceStatus::Stopped | InstanceStatus::Sleeping => {
            ws.sdk().start(id).await?;
            ws.wait_running(id).await
        }
        other => Err(fail(format!(
            "instance {id} is {other}: {}",
            settled.status_reason
        ))),
    }
}

// ---------------------------------------------------------------------------
// signup_and_billing

#[tokio::test]
#[ignore]
async fn signup_and_billing() {
    run_case("signup_and_billing", signup_and_billing_body).await
}

async fn signup_and_billing_body(cx: Ctx) -> Result<()> {
    let sdk = cx.ws.sdk();

    // Health and version need no key.
    let health = sdk.health().await?;
    ensure!(health.healthy, "health: {health:?}");
    ensure!(
        health.runtimes.iter().any(|r| r == "docker"),
        "health should list the docker runtime: {health:?}"
    );
    ensure!(!sdk.version().await?.is_empty(), "version should be set");

    // Signup granted the free credit once.
    let signup = &cx.signup;
    ensure!(
        signup.api_key.starts_with("sk_live_") && signup.api_key.len() == 48,
        "api key format: {}",
        signup.api_key
    );
    ensure!(
        signup.workspace.id.starts_with("ws_") && signup.workspace.id.len() == 23,
        "workspace id: {}",
        signup.workspace.id
    );
    ensure!(!signup.key_id.is_empty(), "key_id missing: {signup:?}");
    ensure!(
        signup.wallet_micros == 5 * MICROS_PER_DOLLAR,
        "free credit should be $5, got {} micros",
        signup.wallet_micros
    );
    let billing = sdk.billing().await?;
    ensure!(
        billing.free_credit_granted,
        "free_credit_granted: {billing:?}"
    );
    ensure!(
        billing.wallet_micros == 5 * MICROS_PER_DOLLAR,
        "wallet: {billing:?}"
    );
    ensure!(
        billing.awake_limit == 3,
        "the awake limit before any top-up is 3: {billing:?}"
    );
    ensure!(
        billing.awake_count == 0 && billing.top_up_total_micros == 0,
        "fresh counters: {billing:?}"
    );
    ensure!(
        billing.rates.vcpu_month_micros > 0 && billing.rates.disk_gb_month_micros > 0,
        "rates: {billing:?}"
    );
    let ledger = sdk.ledger(100).await?;
    ensure!(
        ledger
            .iter()
            .any(|e| e.kind == LedgerKind::Credit && e.micros == 5 * MICROS_PER_DOLLAR),
        "the ledger should hold the free credit entry: {ledger:?}"
    );

    // Keys: a bad key is 401; a second key works until it is deleted.
    let bogus = format!("sk_live_{}", "x".repeat(40));
    expect_api_error(
        "bad key",
        sdk.with_key(bogus).workspace().await,
        401,
        CODE_INVALID_API_KEY,
    )?;
    expect_api_error(
        "no key",
        sdk.with_key("").workspace().await,
        401,
        CODE_INVALID_API_KEY,
    )?;
    let created = sdk.create_api_key(Some("second"), None).await?;
    ensure!(
        created.api_key.starts_with("sk_live_") && created.key.name == "second",
        "created key: {created:?}"
    );
    ensure!(
        sdk.api_keys().await?.len() == 2,
        "two keys after creating one"
    );
    let second = sdk.with_key(created.api_key.clone());
    let workspace = second.workspace().await?;
    ensure!(
        workspace.workspace.id == cx.ws.workspace_id(),
        "the new key maps to the same workspace: {workspace:?}"
    );
    ensure!(
        workspace.awake_limit == 3 && workspace.awake_count == 0,
        "workspace counters: {workspace:?}"
    );
    let deleted = sdk.delete_api_key(&created.key.id).await?;
    ensure!(
        deleted.deleted && deleted.id == created.key.id,
        "delete key: {deleted:?}"
    );
    expect_api_error(
        "deleted key",
        second.workspace().await,
        401,
        CODE_INVALID_API_KEY,
    )?;
    ensure!(
        sdk.api_keys().await?.len() == 1,
        "one key after deleting the second"
    );

    // A dev top-up of 10 USD raises the awake limit to 10 and lands in the ledger.
    expect_api_error(
        "top-up below the minimum",
        sdk.top_up(MICROS_PER_DOLLAR, Some("dev")).await,
        400,
        CODE_INVALID_REQUEST,
    )?;
    expect_api_error(
        "unknown provider",
        sdk.top_up(10 * MICROS_PER_DOLLAR, Some("card")).await,
        400,
        CODE_NOT_SUPPORTED,
    )?;
    let topup = sdk.top_up(10 * MICROS_PER_DOLLAR, Some("dev")).await?;
    ensure!(
        topup.entry.kind == LedgerKind::Topup && topup.entry.micros == 10 * MICROS_PER_DOLLAR,
        "top-up entry: {:?}",
        topup.entry
    );
    ensure!(
        topup.workspace.wallet_micros == 15 * MICROS_PER_DOLLAR,
        "wallet after top-up: {:?}",
        topup.workspace
    );
    ensure!(
        topup.workspace.top_up_total_micros == 10 * MICROS_PER_DOLLAR,
        "top-up total: {:?}",
        topup.workspace
    );
    let billing = sdk.billing().await?;
    ensure!(
        billing.awake_limit == 10,
        "the awake limit after the first top-up is 10: {billing:?}"
    );
    ensure!(
        billing.wallet_micros == 15 * MICROS_PER_DOLLAR
            && billing.top_up_total_micros == 10 * MICROS_PER_DOLLAR,
        "billing: {billing:?}"
    );
    ensure!(
        sdk.workspace().await?.awake_limit == 10,
        "workspace view agrees on the limit"
    );
    let ledger = sdk.ledger(100).await?;
    ensure!(
        ledger
            .iter()
            .any(|e| e.kind == LedgerKind::Topup && e.micros == 10 * MICROS_PER_DOLLAR),
        "the ledger should hold the top-up: {ledger:?}"
    );
    ensure!(
        ledger.len() >= 2 && ledger[0].at >= ledger[ledger.len() - 1].at,
        "the ledger is newest first: {ledger:?}"
    );

    // A running instance produces compute entries. The meter ticks every
    // METER_INTERVAL but charges whole minutes, so the first entry lands about a
    // minute after the instance first reached running.
    let inst = cx.ws.create_and_wait("warren-base", &[]).await?;
    ensure!(
        inst.meter_started_at > 0,
        "meter_started_at is set once running: {inst:?}"
    );
    ensure!(
        sdk.workspace().await?.awake_count == 1,
        "one awake instance"
    );
    let id = inst.id.as_str();
    let entry = poll_until(
        "a compute ledger entry",
        Duration::from_secs(150),
        2 * METER_INTERVAL,
        || async move {
            let ledger = sdk.ledger(200).await?;
            Ok::<_, Error>(
                ledger
                    .into_iter()
                    .find(|e| e.kind == LedgerKind::Compute && e.instance_id == id),
            )
        },
    )
    .await?;
    ensure!(
        entry.micros < 0 && entry.minutes >= 1.0,
        "compute entries are negative whole minutes: {entry:?}"
    );
    let billing = sdk.billing().await?;
    ensure!(
        billing.wallet_micros < 15 * MICROS_PER_DOLLAR,
        "the wallet decreased: {billing:?}"
    );
    ensure!(
        billing.wallet_micros > 14 * MICROS_PER_DOLLAR,
        "a minute of 2 vCPU / 4 GB costs far less than a dollar: {billing:?}"
    );
    let _ = sdk.metrics(id, 1).await?;
    cx.ws.delete_and_wait(id).await?;
    ensure!(
        sdk.workspace().await?.awake_count == 0,
        "deleted instances do not count"
    );

    // The 402 insufficient_balance path cannot be reached here: a fresh wallet
    // holds $5 and creating 2 vCPU / 4 GB / 4 GB needs about $0.16 (one day of
    // rate), so the awake limit trips long before the balance does. warren-billing
    // covers the balance rule and the exhausted-wallet stop with unit tests.
    Ok(())
}

// ---------------------------------------------------------------------------
// create_exec_edge

#[tokio::test]
#[ignore]
async fn create_exec_edge() {
    run_case("create_exec_edge", create_exec_edge_body).await
}

async fn create_exec_edge_body(cx: Ctx) -> Result<()> {
    let d = &cx.daemon;
    let sdk = cx.ws.sdk();
    let key = cx.ws.api_key();

    // Validation before anything is created.
    let base = CreateInstanceRequest {
        template: Some("warren-base".into()),
        ..Default::default()
    };
    expect_api_error(
        "performance type",
        sdk.create_instance(&CreateInstanceRequest {
            kind: Some(InstanceType::Performance),
            ..base.clone()
        })
        .await,
        400,
        CODE_NOT_SUPPORTED,
    )?;
    expect_api_error(
        "unknown template",
        sdk.create_instance(&CreateInstanceRequest {
            template: Some("no-such-template".into()),
            ..Default::default()
        })
        .await,
        404,
        CODE_NOT_FOUND,
    )?;
    expect_api_error(
        "bad env key",
        sdk.create_instance(&CreateInstanceRequest {
            env: Some(BTreeMap::from([("lower".to_string(), "x".to_string())])),
            ..base.clone()
        })
        .await,
        400,
        CODE_INVALID_REQUEST,
    )?;
    expect_api_error(
        "bad shape",
        sdk.create_instance(&CreateInstanceRequest {
            shape: Some(ShapeRequest {
                cpu: Some(2),
                memory: Some(8),
                disk: None,
            }),
            ..base.clone()
        })
        .await,
        400,
        CODE_INVALID_REQUEST,
    )?;
    match cx
        .ws
        .call_with_key(
            key,
            Method::POST,
            "/instances",
            Some(&json!({ "bogus": 1 })),
        )
        .await
    {
        Err(e) if e.status == 400 && e.code == CODE_INVALID_REQUEST => {}
        other => {
            return Err(fail(format!(
                "unknown JSON fields are rejected with 400 invalid_request, got {other:?}"
            )));
        }
    }

    let inst = cx.ws.create_and_wait("warren-base", &[]).await?;
    let id = inst.id.as_str();
    ensure!(
        inst.url == d.instance_url(id, 0),
        "url {} should be {} (base domain and public port)",
        inst.url,
        d.instance_url(id, 0)
    );
    ensure!(
        inst.template_name == "warren-base"
            && inst.template_revision == 1
            && inst.kind == InstanceType::Default,
        "template fields: {inst:?}"
    );
    ensure!(
        inst.runtime == "docker"
            && inst.default_port == GATEWAY_PORT
            && inst.mount_path == "/home/agent"
            && !inst.agent_api,
        "fields: {inst:?}"
    );
    ensure!(
        inst.shape
            == Shape {
                cpu: 2,
                memory: 4,
                disk: 4
            },
        "default shape: {:?}",
        inst.shape
    );
    ensure!(
        inst.workspace_id == cx.ws.workspace_id() && inst.region == "local" && inst.started_at > 0,
        "ownership and timestamps: {inst:?}"
    );
    ensure!(
        !inst.auto_sleep && inst.idle_timeout_seconds == 900,
        "defaults: {inst:?}"
    );
    ensure!(
        sdk.instances().await?.iter().any(|i| i.id == id),
        "the list should include the instance"
    );

    // exec: image user, explicit users, exit codes, stderr, workdir, env.
    let r = sdk.exec(id, &cmd("uname -a")).await?;
    ensure!(
        r.exit_code == 0 && r.stdout.contains("Linux"),
        "uname -a: {r:?}"
    );
    let r = sdk.exec(id, &cmd("id -u")).await?;
    ensure!(
        r.exit_code == 0 && r.stdout.trim().parse::<u32>().is_ok(),
        "id -u as the image user: {r:?}"
    );
    let r = sdk
        .exec(
            id,
            &ExecRequest {
                command: "id -u".into(),
                user: "agent".into(),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        r.stdout.trim() == "1000",
        "the agent user is uid 1000: {r:?}"
    );
    let r = sdk
        .exec(
            id,
            &ExecRequest {
                command: "id -u".into(),
                user: "root".into(),
                ..Default::default()
            },
        )
        .await?;
    ensure!(r.stdout.trim() == "0", "exec as root: {r:?}");
    let r = sdk.exec(id, &cmd("echo oops >&2; exit 3")).await?;
    ensure!(
        r.exit_code == 3 && r.stderr.contains("oops"),
        "exit code and stderr: {r:?}"
    );
    let r = sdk
        .exec(
            id,
            &ExecRequest {
                command: "pwd".into(),
                workdir: "/tmp".into(),
                ..Default::default()
            },
        )
        .await?;
    ensure!(r.stdout.trim() == "/tmp", "workdir: {r:?}");
    let env = BTreeMap::from([("E2E_VAR".to_string(), "present".to_string())]);
    let r = sdk
        .exec(
            id,
            &ExecRequest {
                command: "echo $E2E_VAR".into(),
                env,
                ..Default::default()
            },
        )
        .await?;
    ensure!(r.stdout.trim() == "present", "exec env: {r:?}");
    let r = sdk.exec(id, &cmd("echo $WARREN_INSTANCE_ID")).await?;
    ensure!(r.stdout.trim() == id, "the platform env is injected: {r:?}");
    let r = sdk
        .exec(id, &cmd("echo $WARREN_INSTANCE_TOKEN | wc -c"))
        .await?;
    ensure!(
        r.stdout.trim() != "1",
        "the instance token is present in the instance env: {r:?}"
    );
    let _ = sdk.logs(id, 50, 0).await?;

    // Edge authentication.
    let edge = cx.ws.edge(id);
    let resp = edge.get(0, "/v1/health", Auth::Key(key)).await?;
    let body = expect_status(resp, 200, None).await?;
    let health: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| fail(format!("gateway health JSON: {e}")))?;
    ensure!(
        health["healthy"] == true,
        "gateway health through the edge: {health}"
    );
    let resp = edge.get(0, "/v1/health", Auth::Bearer(key)).await?;
    expect_status(resp, 200, None).await?;
    let resp = edge.get(0, "/v1/health", Auth::None).await?;
    expect_status(resp, 401, Some(CODE_INVALID_API_KEY)).await?;
    let resp = edge
        .get(0, "/v1/health", Auth::Key("sk_live_not_a_real_key"))
        .await?;
    expect_status(resp, 401, Some(CODE_INVALID_API_KEY)).await?;
    let (other, _) = Workspace::signup(d.clone(), "e2e-other-workspace").await?;
    let resp = edge
        .get(0, "/v1/health", Auth::Key(other.api_key()))
        .await?;
    expect_status(resp, 404, Some(CODE_NOT_FOUND)).await?;
    expect_api_error(
        "another workspace's GET",
        other.sdk().instance(id).await,
        404,
        CODE_NOT_FOUND,
    )?;
    expect_api_error(
        "another workspace's exec",
        other.sdk().exec(id, &cmd("id")).await,
        404,
        CODE_NOT_FOUND,
    )?;

    // Preview URL: ttyd on 7681 answers; an unused port routes but is 502.
    let resp = edge.get(TERMINAL_PORT, "/", Auth::Key(key)).await?;
    expect_status(resp, 200, None).await?;
    let resp = edge.get(5999, "/", Auth::Key(key)).await?;
    ensure!(
        resp.status().as_u16() == 502,
        "a port nothing listens on is 502, got {}",
        resp.status()
    );

    // CORS preflight on the instance host.
    let preflight = [
        ("origin", "http://example.test"),
        ("access-control-request-method", "GET"),
        ("access-control-request-headers", "x-warren-key"),
    ];
    let resp = edge
        .send(
            Method::OPTIONS,
            0,
            "/v1/health",
            Auth::None,
            None,
            &preflight,
        )
        .await?;
    ensure!(
        resp.status().is_success(),
        "preflight status {}",
        resp.status()
    );
    ensure!(
        header(resp.headers(), "access-control-allow-origin") == "http://example.test",
        "preflight origin: {:?}",
        resp.headers()
    );
    ensure!(
        header(resp.headers(), "access-control-allow-headers")
            .to_ascii_lowercase()
            .contains("x-warren-key"),
        "preflight headers: {:?}",
        resp.headers()
    );

    // Signed URL for the terminal: 302 with the session cookie, then the cookie
    // alone works there and nowhere else.
    let preview = d.instance_url(id, TERMINAL_PORT);
    let signed = sdk.signed_url(id, TERMINAL_PORT, 600, "/").await?;
    ensure!(
        signed.url.starts_with(&preview) && signed.url.contains("warren_token="),
        "signed url {} should be under {preview}",
        signed.url
    );
    let now = now_unix();
    ensure!(
        (now + 500..=now + 700).contains(&signed.expires_at),
        "expires_at about now+600: {} (now {now})",
        signed.expires_at
    );
    let resp = d
        .raw_client_for_url(&signed.url)?
        .get(&signed.url)
        .send()
        .await
        .map_err(|e| fail(format!("GET signed url: {e}")))?;
    ensure!(
        resp.status().as_u16() == 302,
        "a signed url redirects, got {}",
        resp.status()
    );
    let cookie = session_cookie(&resp).ok_or_else(|| {
        fail(format!(
            "no {COOKIE_NAME} cookie on the 302: {:?}",
            resp.headers()
        ))
    })?;
    let set_cookie = header(resp.headers(), "set-cookie");
    ensure!(
        set_cookie.contains("HttpOnly") && set_cookie.contains("Path=/"),
        "cookie attributes: {set_cookie}"
    );
    let location = header(resp.headers(), "location");
    ensure!(
        !location.is_empty() && !location.contains("warren_token"),
        "location drops the token: {location:?}"
    );
    let target = if location.starts_with('/') {
        format!("{preview}{location}")
    } else {
        location.clone()
    };
    let resp = d
        .raw_client_for_url(&target)?
        .get(&target)
        .header("cookie", format!("{COOKIE_NAME}={cookie}"))
        .send()
        .await
        .map_err(|e| fail(format!("GET {target} with cookie: {e}")))?;
    ensure!(
        resp.status().as_u16() == 200,
        "the session cookie opens {target}, got {}",
        resp.status()
    );
    let resp = edge.get(TERMINAL_PORT, "/", Auth::Cookie(&cookie)).await?;
    expect_status(resp, 200, None).await?;
    let resp = edge.get(0, "/v1/health", Auth::Cookie(&cookie)).await?;
    expect_status(resp, 401, Some(CODE_INVALID_API_KEY)).await?;
    let resp = edge
        .get(TERMINAL_PORT, "/", Auth::Cookie("not.a.token"))
        .await?;
    expect_status(resp, 401, Some(CODE_INVALID_API_KEY)).await?;

    // A signed URL without a port means the default port.
    let signed = sdk.signed_url(id, 0, 120, "/v1/health").await?;
    ensure!(
        signed.url.starts_with(&d.instance_url(id, 0)),
        "default-port signed url: {}",
        signed.url
    );
    let resp = d
        .raw_client_for_url(&signed.url)?
        .get(&signed.url)
        .send()
        .await
        .map_err(|e| fail(format!("GET signed url: {e}")))?;
    ensure!(
        resp.status().as_u16() == 302,
        "default-port signed url redirects, got {}",
        resp.status()
    );
    let cookie = session_cookie(&resp).ok_or_else(|| fail("no cookie on the default-port 302"))?;
    let resp = edge.get(0, "/v1/health", Auth::Cookie(&cookie)).await?;
    let body = expect_status(resp, 200, None).await?;
    ensure!(
        String::from_utf8_lossy(&body).contains("healthy"),
        "health through the cookie: {}",
        String::from_utf8_lossy(&body)
    );
    expect_api_error(
        "ttl below the minimum",
        sdk.signed_url(id, 0, 10, "/").await,
        400,
        CODE_INVALID_REQUEST,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// agent_chat_streaming

#[tokio::test]
#[ignore]
async fn agent_chat_streaming() {
    run_case("agent_chat_streaming", agent_chat_streaming_body).await
}

async fn agent_chat_streaming_body(cx: Ctx) -> Result<()> {
    let inst = cx
        .ws
        .create_and_wait("warren-agent", &[(ENV_AGENT_DRIVER, "echo")])
        .await?;
    ensure!(
        inst.agent_api && inst.template_name == "warren-agent",
        "agent template: {inst:?}"
    );
    ensure!(
        inst.env.get(ENV_AGENT_DRIVER).map(String::as_str) == Some("echo"),
        "instance env: {:?}",
        inst.env
    );
    let agent = cx.ws.agent(&inst.id)?;
    let agent_ref = &agent;
    let health = poll_until(
        "the agent gateway to be ready",
        Duration::from_secs(60),
        Duration::from_secs(1),
        || async move {
            let h = agent_ref.health().await?;
            Ok::<_, Error>(h.agent_ready.then_some(h))
        },
    )
    .await?;
    ensure!(
        health.healthy && health.driver == "echo",
        "gateway health: {health:?}"
    );
    ensure!(!agent.version().await?.is_empty(), "gateway version");
    let models = agent.models().await?;
    ensure!(!models.is_empty(), "models: {models:?}");

    // A plain response.
    let r = agent
        .respond(&ResponsesRequest {
            input: "hi".into(),
            ..Default::default()
        })
        .await?;
    ensure!(
        r.status == ResponseStatus::Completed && r.output_text == "echo: hi",
        "echo reply: {r:?}"
    );
    ensure!(
        !r.id.is_empty()
            && !r.session_id.is_empty()
            && r.created_at > 0
            && r.completed_at.is_some(),
        "response fields: {r:?}"
    );
    let again = agent.get_response(&r.id).await?;
    ensure!(
        again.id == r.id && again.output_text == "echo: hi" && again.session_id == r.session_id,
        "stored response: {again:?}"
    );

    // Streaming into a new session: created, deltas, completed.
    let events = collect_stream(
        agent.stream(&ResponsesRequest {
            input: "stream me".into(),
            ..Default::default()
        }),
        Duration::from_secs(60),
    )
    .await?;
    let names = event_names(&events);
    ensure!(
        names.first() == Some(&EVENT_CREATED),
        "first event: {names:?}"
    );
    ensure!(
        names.last() == Some(&EVENT_COMPLETED),
        "last event: {names:?}"
    );
    let deltas: Vec<&SseEvent> = events.iter().filter(|e| e.event == EVENT_DELTA).collect();
    ensure!(
        deltas.len() >= 2,
        "the echo driver streams at least two deltas: {names:?}"
    );
    let joined: String = deltas.iter().filter_map(|e| e.str_field("delta")).collect();
    ensure!(
        joined == "echo: stream me",
        "deltas concatenate to the reply: {joined:?}"
    );
    let created = &events[0];
    let completed = &events[events.len() - 1];
    ensure!(
        completed.str_field("output_text") == Some("echo: stream me"),
        "completed payload: {:?}",
        completed.data
    );
    ensure!(
        completed.str_field("status") == Some("completed"),
        "completed status: {:?}",
        completed.data
    );
    let stream_session = created
        .str_field("session_id")
        .ok_or_else(|| fail(format!("created carries session_id: {:?}", created.data)))?;
    ensure!(
        completed.str_field("session_id") == Some(stream_session),
        "completed names the same session: {:?}",
        completed.data
    );
    ensure!(
        stream_session != r.session_id,
        "a request without session_id opens a new session"
    );
    let stream_id = completed
        .str_field("id")
        .ok_or_else(|| fail(format!("completed carries id: {:?}", completed.data)))?;
    let replay = collect_stream(agent.response_stream(stream_id), Duration::from_secs(30)).await?;
    ensure!(
        event_names(&replay).last() == Some(&EVENT_COMPLETED),
        "a stored response replays to completed: {:?}",
        event_names(&replay)
    );

    // The first session continues and lists both user messages.
    let r2 = agent
        .respond(&ResponsesRequest {
            input: "again".into(),
            session_id: Some(r.session_id.clone()),
            ..Default::default()
        })
        .await?;
    ensure!(
        r2.session_id == r.session_id && r2.output_text == "echo: again",
        "continued reply: {r2:?}"
    );
    let detail = agent.session(&r.session_id).await?;
    let users: Vec<&str> = detail
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .collect();
    ensure!(
        users == ["hi", "again"],
        "the session lists the two user messages in order: {users:?}"
    );
    ensure!(
        detail
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .count()
            == 2,
        "two assistant replies: {:?}",
        detail.messages
    );
    let sessions = agent.sessions().await?;
    ensure!(sessions.len() == 2, "two sessions: {sessions:?}");
    ensure!(
        sessions[0].updated_at >= sessions[1].updated_at,
        "sessions are newest first: {sessions:?}"
    );
    let summary = sessions
        .iter()
        .find(|s| s.id == r.session_id)
        .ok_or_else(|| fail("the first session is listed"))?;
    ensure!(
        summary.message_count == 4,
        "message_count counts both roles: {summary:?}"
    );

    // `!` runs a shell command and returns its output.
    let r3 = agent
        .respond(&ResponsesRequest {
            input: "!echo hello".into(),
            ..Default::default()
        })
        .await?;
    ensure!(
        r3.status == ResponseStatus::Completed && r3.output_text.trim() == "hello",
        "shell passthrough: {r3:?}"
    );

    // Session rename and delete.
    agent.rename_session(&r.session_id, "renamed").await?;
    ensure!(
        agent.session(&r.session_id).await?.name == "renamed",
        "renamed session"
    );
    agent.delete_session(&r.session_id).await?;
    expect_api_error(
        "deleted session",
        agent.session(&r.session_id).await,
        404,
        CODE_NOT_FOUND,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// files_api

#[tokio::test]
#[ignore]
async fn files_api() {
    run_case("files_api", files_api_body).await
}

async fn files_api_body(cx: Ctx) -> Result<()> {
    let sdk = cx.ws.sdk();
    let inst = cx.ws.create_and_wait("warren-base", &[]).await?;
    let id = inst.id.as_str();
    let agent = cx.ws.agent(id)?;
    let edge = cx.ws.edge(id);

    let put = agent
        .write_file("notes/a.txt", b"hello files".to_vec())
        .await?;
    ensure!(put.path == "notes/a.txt" && put.size == 11, "put: {put:?}");
    ensure!(
        agent.read_file("notes/a.txt").await? == b"hello files",
        "read back"
    );
    let listing = agent.list_files("notes").await?;
    ensure!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "a.txt" && e.kind == "file" && e.size == 11),
        "listing: {listing:?}"
    );
    let home = agent.list_files(".").await?;
    ensure!(
        home.entries
            .iter()
            .any(|e| e.name == "notes" && e.kind == "dir"),
        "home listing: {home:?}"
    );
    let r = sdk
        .exec(id, &cmd(&format!("cat {}/notes/a.txt", inst.mount_path)))
        .await?;
    ensure!(
        r.stdout == "hello files",
        "exec sees the file on the persistent disk: {r:?}"
    );

    let put = agent
        .write_file("notes/a.txt", b"hello again".to_vec())
        .await?;
    ensure!(put.size == 11, "overwrite: {put:?}");
    agent.move_file("notes/a.txt", "notes/b.txt").await?;
    ensure!(
        agent.read_file("notes/b.txt").await? == b"hello again",
        "moved content"
    );
    expect_api_error(
        "the moved file is gone",
        agent.read_file("notes/a.txt").await,
        404,
        CODE_NOT_FOUND,
    )?;

    let (_, headers, bytes) = edge.get_ok(0, "/v1/files/download?path=notes").await?;
    ensure!(
        header(&headers, "content-type").starts_with("application/gzip"),
        "download content type: {:?}",
        headers
    );
    ensure!(
        bytes.len() > 2 && bytes[0] == 0x1f && bytes[1] == 0x8b,
        "download is gzip"
    );
    let same = agent.download("notes").await?;
    ensure!(
        same.len() > 2 && same[0] == 0x1f && same[1] == 0x8b,
        "sdk download is gzip"
    );

    let del = agent.delete_file("notes/b.txt").await?;
    ensure!(del.deleted && del.path == "notes/b.txt", "delete: {del:?}");
    expect_api_error(
        "deleted file",
        agent.read_file("notes/b.txt").await,
        404,
        CODE_NOT_FOUND,
    )?;

    // Traversal and absolute paths outside the home are forbidden.
    for path in ["../../etc/passwd", "/etc/passwd", "notes/../../x"] {
        expect_api_error(
            &format!("read {path}"),
            agent.read_file(path).await,
            403,
            CODE_FORBIDDEN,
        )?;
    }
    expect_api_error(
        "write outside home",
        agent.write_file("../escape.txt", b"x".to_vec()).await,
        403,
        CODE_FORBIDDEN,
    )?;
    expect_api_error(
        "list outside home",
        agent.list_files("..").await,
        403,
        CODE_FORBIDDEN,
    )?;
    expect_api_error(
        "delete outside home",
        agent.delete_file("../x").await,
        403,
        CODE_FORBIDDEN,
    )?;
    expect_api_error(
        "move outside home",
        agent.move_file("notes", "../notes").await,
        403,
        CODE_FORBIDDEN,
    )?;
    let resp = edge
        .get(
            0,
            "/v1/files/content?path=../../etc/passwd",
            Auth::Key(cx.ws.api_key()),
        )
        .await?;
    expect_status(resp, 403, Some(CODE_FORBIDDEN)).await?;

    // The Agent API is not served on a non-agent template.
    expect_api_error(
        "responses on warren-base",
        agent
            .respond(&ResponsesRequest {
                input: "hi".into(),
                ..Default::default()
            })
            .await,
        404,
        CODE_NOT_SUPPORTED,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// lifecycle

#[tokio::test]
#[ignore]
async fn lifecycle() {
    run_case("lifecycle", lifecycle_body).await
}

async fn lifecycle_body(cx: Ctx) -> Result<()> {
    let sdk = cx.ws.sdk();
    let key = cx.ws.api_key();
    let inst = cx.ws.create_and_wait("warren-base", &[]).await?;
    let id = inst.id.as_str();
    let edge = cx.ws.edge(id);

    // stop -> stopped: the URL answers 409, exec is refused, nothing is awake.
    let answer = sdk.stop(id).await?;
    ensure!(
        settles(
            answer.status,
            InstanceStatus::Stopping,
            InstanceStatus::Stopped
        ) && answer.id == id,
        "stop answers stopping: {answer:?}"
    );
    let stopped = cx
        .ws
        .wait_for_status(id, InstanceStatus::Stopped, TRANSITION_TIMEOUT)
        .await?;
    ensure!(stopped.stopped_at > 0, "stopped_at is set: {stopped:?}");
    let resp = edge.get(0, "/v1/health", Auth::Key(key)).await?;
    expect_status(resp, 409, Some(CODE_CONFLICT)).await?;
    expect_api_error(
        "exec on a stopped instance",
        sdk.exec(id, &cmd("true")).await,
        409,
        CODE_CONFLICT,
    )?;
    ensure!(
        sdk.workspace().await?.awake_count == 0,
        "stopped instances never count as awake"
    );

    // start -> running.
    let answer = sdk.start(id).await?;
    ensure!(
        settles(
            answer.status,
            InstanceStatus::Starting,
            InstanceStatus::Running
        ),
        "start answers starting: {answer:?}"
    );
    cx.ws.wait_running(id).await?;
    let resp = edge.get(0, "/v1/health", Auth::Key(key)).await?;
    expect_status(resp, 200, None).await?;

    // restart -> restarting -> running.
    let answer = sdk.restart(id).await?;
    ensure!(
        settles(
            answer.status,
            InstanceStatus::Restarting,
            InstanceStatus::Running
        ),
        "restart answers restarting: {answer:?}"
    );
    cx.ws.wait_running(id).await?;

    // sleep -> sleeping; a request to the URL wakes it.
    let answer = sdk.sleep(id).await?;
    ensure!(
        answer.status == InstanceStatus::Sleeping,
        "sleep answers sleeping: {answer:?}"
    );
    let sleeping = cx
        .ws
        .wait_for_status(id, InstanceStatus::Sleeping, TRANSITION_TIMEOUT)
        .await?;
    ensure!(sleeping.slept_at > 0, "slept_at is set: {sleeping:?}");
    ensure!(
        sdk.workspace().await?.awake_count == 0,
        "sleeping instances never count as awake"
    );
    let resp = edge.get(0, "/v1/health", Auth::Key(key)).await?;
    expect_status(resp, 200, None).await?;
    let woke = cx.ws.wait_running(id).await?;
    ensure!(
        woke.status == InstanceStatus::Running,
        "awake after the request"
    );

    // auto-sleep after 5 idle seconds (the daemon runs with --min-idle-seconds 5).
    let patched = sdk
        .patch_instance(
            id,
            &PatchInstanceRequest {
                auto_sleep: Some(true),
                idle_timeout_seconds: Some(5),
                name: Some("napper".into()),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        patched.auto_sleep && patched.idle_timeout_seconds == 5 && patched.name == "napper",
        "patched: {patched:?}"
    );
    cx.ws
        .wait_for_status(id, InstanceStatus::Sleeping, Duration::from_secs(40))
        .await?;

    // exec wakes it.
    let r = sdk.exec(id, &cmd("echo awake")).await?;
    ensure!(
        r.exit_code == 0 && r.stdout.trim() == "awake",
        "exec on a sleeping instance: {r:?}"
    );
    let now = sdk.instance(id).await?;
    ensure!(
        now.status == InstanceStatus::Running,
        "exec leaves the instance running, got {}",
        now.status
    );
    expect_api_error(
        "idle timeout below the minimum",
        sdk.patch_instance(
            id,
            &PatchInstanceRequest {
                idle_timeout_seconds: Some(1),
                ..Default::default()
            },
        )
        .await,
        400,
        CODE_INVALID_REQUEST,
    )?;
    sdk.patch_instance(
        id,
        &PatchInstanceRequest {
            auto_sleep: Some(false),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// fork_backup_restore_resize_update

#[tokio::test]
#[ignore]
async fn fork_backup_restore_resize_update() {
    run_case(
        "fork_backup_restore_resize_update",
        fork_backup_restore_resize_update_body,
    )
    .await
}

async fn fork_backup_restore_resize_update_body(cx: Ctx) -> Result<()> {
    let sdk = cx.ws.sdk();
    let inst = cx.ws.create_and_wait("warren-base", &[]).await?;
    let id = inst.id.as_str();
    let agent = cx.ws.agent(id)?;
    agent.write_file("marker.txt", b"original".to_vec()).await?;

    // fork: the copy carries the file; the source comes back.
    let fork = sdk
        .fork(
            id,
            &ForkRequest {
                name: Some("fork".into()),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        fork.id != id && fork.parent_id == id && fork.name == "fork",
        "fork identity: {fork:?}"
    );
    ensure!(
        fork.template_name == inst.template_name && fork.url == cx.daemon.instance_url(&fork.id, 0),
        "fork fields: {fork:?}"
    );
    ensure_running(&cx.ws, &fork.id).await?;
    let copy = cx.ws.agent(&fork.id)?.read_file("marker.txt").await?;
    ensure!(
        copy == b"original",
        "the fork carries the file: {:?}",
        String::from_utf8_lossy(&copy)
    );
    ensure_running(&cx.ws, id).await?;
    ensure!(
        agent.read_file("marker.txt").await? == b"original",
        "the source keeps its file"
    );

    // backup, overwrite, restore.
    let backup = sdk.create_backup(id, "before change").await?;
    ensure!(
        backup.instance_id == id && backup.size_bytes > 0 && backup.note == "before change",
        "backup: {backup:?}"
    );
    ensure!(
        sdk.backups(id).await?.iter().any(|b| b.id == backup.id),
        "backup listed"
    );
    agent.write_file("marker.txt", b"changed".to_vec()).await?;
    ensure!(
        agent.read_file("marker.txt").await? == b"changed",
        "overwritten"
    );
    let restored = sdk.restore(id, &backup.id).await?;
    ensure!(
        restored.id == id,
        "restore answers the instance: {restored:?}"
    );
    cx.ws.wait_running(id).await?;
    ensure!(
        agent.read_file("marker.txt").await? == b"original",
        "restore brings the original content back"
    );
    expect_api_error(
        "restore an unknown backup",
        sdk.restore(id, &"0".repeat(32)).await,
        404,
        CODE_NOT_FOUND,
    )?;

    // resize 2 vCPU / 4 GB -> 4 vCPU / 8 GB.
    let resized = sdk
        .resize(
            id,
            &ShapeRequest {
                cpu: Some(4),
                memory: Some(8),
                disk: None,
            },
        )
        .await?;
    ensure!(
        resized.resources.cpu == 4 && resized.resources.memory == 8,
        "resize answers the new shape: {resized:?}"
    );
    let inst = cx.ws.wait_running(id).await?;
    ensure!(
        inst.shape.cpu == 4 && inst.shape.memory == 8 && inst.shape.disk >= 4,
        "resources after resize: {:?}",
        inst.shape
    );
    ensure!(
        sdk.resize(
            id,
            &ShapeRequest {
                disk: Some(2),
                ..Default::default()
            }
        )
        .await
        .is_err(),
        "disk may only grow"
    );
    ensure!(
        agent.read_file("marker.txt").await? == b"original",
        "resize keeps the disk"
    );

    // update with the same template: running again with the file intact.
    let updated = sdk.update(id, Some("warren-base")).await?;
    ensure!(
        settles(
            updated.status,
            InstanceStatus::Updating,
            InstanceStatus::Running
        ) && updated.template_revision == 1,
        "update: {updated:?}"
    );
    ensure!(
        updated.image_ref == inst.image_ref,
        "same image: {updated:?}"
    );
    cx.ws.wait_running(id).await?;
    ensure!(
        agent.read_file("marker.txt").await? == b"original",
        "update keeps mount_path data"
    );
    let r = sdk
        .exec(id, &cmd(&format!("cat {}/marker.txt", inst.mount_path)))
        .await?;
    ensure!(
        r.stdout == "original",
        "exec sees the file after update: {r:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// limits

#[tokio::test]
#[ignore]
async fn limits() {
    run_case("limits", limits_body).await
}

async fn limits_body(cx: Ctx) -> Result<()> {
    let sdk = cx.ws.sdk();
    let req = CreateInstanceRequest {
        template: Some("warren-base".into()),
        ..Default::default()
    };
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(sdk.create_instance(&req).await?.id);
    }
    ensure!(
        sdk.workspace().await?.awake_count == 3,
        "provisioning instances count as awake"
    );

    // The fourth awake instance is refused on the free tier, with details.
    match sdk.create_instance(&req).await {
        Err(e) if e.status() == Some(409) && e.code() == Some(CODE_INSTANCE_LIMIT_REACHED) => {
            let details = e.api().and_then(|a| a.details.clone()).unwrap_or_default();
            ensure!(
                details["limit"] == 3 && details["awake"] == 3,
                "limit details: {details}"
            );
        }
        other => {
            return Err(fail(format!(
                "the fourth instance should be 409 instance_limit_reached, got {other:?}"
            )));
        }
    }
    for id in &ids {
        cx.ws.wait_running(id).await?;
    }

    // Stopping one frees a slot; starting it again while four are awake is refused.
    sdk.stop(&ids[0]).await?;
    cx.ws
        .wait_for_status(&ids[0], InstanceStatus::Stopped, TRANSITION_TIMEOUT)
        .await?;
    ensure!(
        sdk.workspace().await?.awake_count == 2,
        "two awake after a stop"
    );
    let fourth = sdk.create_instance(&req).await?;
    ensure!(
        fourth.status.awake(),
        "the new instance is awake: {fourth:?}"
    );
    expect_api_error(
        "start over the limit",
        sdk.start(&ids[0]).await,
        409,
        CODE_INSTANCE_LIMIT_REACHED,
    )?;
    cx.ws.wait_running(&fourth.id).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// crons

#[tokio::test]
#[ignore]
async fn crons() {
    run_case("crons", crons_body).await
}

async fn crons_body(cx: Ctx) -> Result<()> {
    let sdk = cx.ws.sdk();
    let inst = cx
        .ws
        .create_and_wait("warren-agent", &[(ENV_AGENT_DRIVER, "echo")])
        .await?;
    let id = inst.id.as_str();
    let before = now_unix();
    let cron = sdk
        .create_cron(
            id,
            &CronRequest {
                name: Some("every-minute".into()),
                ..CronRequest::new("cron hello", "* * * * *")
            },
        )
        .await?;
    ensure!(
        cron.enabled
            && cron.schedule == "* * * * *"
            && cron.prompt == "cron hello"
            && cron.instance_id == id,
        "cron: {cron:?}"
    );
    ensure!(
        cron.name == "every-minute" && cron.last_run_at == 0,
        "fresh cron: {cron:?}"
    );
    ensure!(
        cron.next_run_at > before && cron.next_run_at <= before + 61,
        "next_run_at is the next minute: {} (now {before})",
        cron.next_run_at
    );
    ensure!(sdk.crons(id).await?.len() == 1, "one cron listed");
    ensure!(
        sdk.cron(id, &cron.id).await?.id == cron.id,
        "cron fetched by id"
    );
    expect_api_error(
        "bad schedule",
        sdk.create_cron(id, &CronRequest::new("x", "not a cron"))
            .await,
        400,
        CODE_INVALID_REQUEST,
    )?;

    // It fires at the next minute boundary; the scheduler polls every 30 s.
    let cron_id = cron.id.as_str();
    let fired = poll_until(
        "the cron to fire",
        Duration::from_secs(120),
        Duration::from_secs(5),
        || async move {
            let c = sdk.cron(id, cron_id).await?;
            Ok::<_, Error>((c.last_run_at != 0).then_some(c))
        },
    )
    .await?;
    ensure!(
        fired.last_run_at >= before && !fired.last_status.is_empty(),
        "fired: {fired:?}"
    );
    ensure!(
        fired.next_run_at > fired.last_run_at,
        "next_run_at advances after a firing: {fired:?}"
    );

    // The firing posted the prompt into a new session.
    let agent = cx.ws.agent(id)?;
    let sessions = agent.sessions().await?;
    ensure!(!sessions.is_empty(), "a firing creates a session");
    let mut found = false;
    for s in &sessions {
        let detail = agent.session(&s.id).await?;
        if detail
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content == "cron hello")
        {
            found = true;
            break;
        }
    }
    ensure!(
        found,
        "a session holds the cron prompt as a user message: {sessions:?}"
    );

    // Disable, then delete.
    let patched = sdk
        .patch_cron(
            id,
            cron_id,
            &CronPatchRequest {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    ensure!(!patched.enabled, "disabled: {patched:?}");
    sdk.delete_cron(id, cron_id).await?;
    ensure!(sdk.crons(id).await?.is_empty(), "no crons after delete");
    expect_api_error(
        "deleted cron",
        sdk.cron(id, cron_id).await,
        404,
        CODE_NOT_FOUND,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// templates

#[tokio::test]
#[ignore]
async fn templates() {
    run_case("templates", templates_body).await
}

async fn templates_body(cx: Ctx) -> Result<()> {
    let d = &cx.daemon;
    let sdk = cx.ws.sdk();
    let key = cx.ws.api_key();
    let catalog = sdk.templates().await?;
    let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
    for name in ["warren-base", "warren-agent", "warren-opencode"] {
        ensure!(
            catalog
                .iter()
                .any(|t| t.name == name && t.scope == TemplateScope::System),
            "catalog lists {name}: {names:?}"
        );
    }
    let base = sdk.template("warren-base").await?;
    ensure!(
        base.default_port == GATEWAY_PORT && !base.agent && base.mount_path == "/home/agent",
        "warren-base: {base:?}"
    );
    expect_api_error(
        "unknown template",
        sdk.template("no-such-template").await,
        404,
        CODE_NOT_FOUND,
    )?;
    let reserved = RegisterTemplateRequest {
        name: "warren-mine".into(),
        image_ref: "alpine:3.20".into(),
        ..Default::default()
    };
    expect_api_error(
        "reserved prefix",
        sdk.register_template(&reserved).await,
        400,
        CODE_INVALID_REQUEST,
    )?;
    let bad_name = RegisterTemplateRequest {
        name: "Bad Name".into(),
        image_ref: "alpine:3.20".into(),
        ..Default::default()
    };
    expect_api_error(
        "bad name",
        sdk.register_template(&bad_name).await,
        400,
        CODE_INVALID_REQUEST,
    )?;

    let req = RegisterTemplateRequest {
        name: "e2e-alpine".into(),
        image_ref: "alpine:3.20".into(),
        default_port: 0,
        mount_path: "/home".into(),
        agent: false,
        description: "alpine for the e2e suite".into(),
        env: BTreeMap::new(),
    };
    let tpl = sdk.register_template(&req).await?;
    ensure!(
        tpl.name == "e2e-alpine" && tpl.revision == 1 && tpl.scope == TemplateScope::Workspace,
        "registered: {tpl:?}"
    );
    ensure!(
        tpl.image_ref == "alpine:3.20"
            && tpl.mount_path == "/home"
            && tpl.default_port == 0
            && !tpl.agent,
        "fields: {tpl:?}"
    );
    ensure!(
        !tpl.image_digest.is_empty(),
        "the image digest is recorded on registration: {tpl:?}"
    );
    ensure!(
        tpl.workspace_id == cx.ws.workspace_id(),
        "owned by the workspace: {tpl:?}"
    );
    ensure!(
        sdk.template("e2e-alpine@1").await?.revision == 1,
        "pinned revision resolves"
    );
    expect_api_error(
        "unknown revision",
        sdk.template("e2e-alpine@7").await,
        404,
        CODE_NOT_FOUND,
    )?;
    let listed = sdk.templates().await?;
    let pos = |n: &str| listed.iter().position(|t| t.name == n);
    ensure!(
        pos("e2e-alpine") > pos("warren-opencode"),
        "system templates come first: {:?}",
        listed.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // An instance from it. alpine:3.20 ships no gateway and no long-running
    // process: its /bin/sh exits at once without a stdin. The daemon injects the
    // static gateway at Config::INJECTED_GATEWAY_PATH and runs it as the
    // entrypoint in wrap mode, where it supervises sh as a child and keeps
    // serving, so the instance reaches running and exec works.
    let inst = cx.ws.create_and_wait("e2e-alpine", &[]).await?;
    let id = inst.id.as_str();
    ensure!(
        inst.template_name == "e2e-alpine" && inst.template_revision == 1,
        "instance template: {inst:?}"
    );
    ensure!(
        inst.default_port == 0
            && inst.mount_path == "/home"
            && !inst.agent_api
            && inst.image_ref == "alpine:3.20",
        "instance fields: {inst:?}"
    );
    let r = sdk.exec(id, &cmd("cat /etc/alpine-release")).await?;
    ensure!(
        r.exit_code == 0 && r.stdout.contains("3.20"),
        "alpine release: {r:?}"
    );
    let r = sdk
        .exec(id, &cmd("touch /home/from-e2e && ls /home"))
        .await?;
    ensure!(
        r.exit_code == 0 && r.stdout.contains("from-e2e"),
        "the mount path is writable: {r:?}"
    );
    let r = sdk
        .exec(
            id,
            &ExecRequest {
                command: format!("test -f {}", Config::INJECTED_GATEWAY_PATH),
                user: "root".into(),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        r.exit_code == 0,
        "the gateway is injected at {}: {r:?}",
        Config::INJECTED_GATEWAY_PATH
    );

    // default_port 0: the bare URL serves nothing, <id>-3737 reaches the
    // injected gateway, whose health reports the wrapped entrypoint. sh has
    // usually exited (code 0) by the time the daemon sees the gateway healthy;
    // poll for `running: false` to cover the race and accept any exit code.
    let edge = cx.ws.edge(id);
    let resp = edge.get(0, "/", Auth::Key(key)).await?;
    let body = expect_status(resp, 502, None).await?;
    ensure!(
        String::from_utf8_lossy(&body).contains("no default port"),
        "502 text: {}",
        String::from_utf8_lossy(&body)
    );
    let edge_ref = &edge;
    let health = poll_until(
        "the wrapped entrypoint of the alpine instance to exit",
        Duration::from_secs(60),
        Duration::from_secs(1),
        || async move {
            let resp = edge_ref
                .get(GATEWAY_PORT, "/v1/health", Auth::Key(key))
                .await?;
            let health = health_json(resp).await?;
            let wrapped = &health["wrapped"];
            ensure!(
                wrapped.is_object(),
                "the health of an injected gateway carries `wrapped`: {health}"
            );
            Ok::<_, Error>((wrapped["running"] == false).then_some(health))
        },
    )
    .await?;
    let exit_code = &health["wrapped"]["exit_code"];
    ensure!(
        exit_code.is_null() || exit_code.is_i64(),
        "wrapped.exit_code is an integer or null: {health}"
    );

    // The files API works through the preview URL, confined to the mount path.
    let files = cx.ws.agent_on_port(id, GATEWAY_PORT)?;
    let put = files
        .write_file("e2e/hello.txt", b"hello from alpine".to_vec())
        .await?;
    ensure!(
        put.path == "e2e/hello.txt" && put.size == 17,
        "put through <id>-3737: {put:?}"
    );
    ensure!(
        files.read_file("e2e/hello.txt").await? == b"hello from alpine",
        "read back through <id>-3737"
    );
    let listing = files.list_files("e2e").await?;
    ensure!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "hello.txt" && e.kind == "file" && e.size == 17),
        "listing through <id>-3737: {listing:?}"
    );
    let r = sdk.exec(id, &cmd("cat /home/e2e/hello.txt")).await?;
    ensure!(
        r.stdout == "hello from alpine",
        "the file landed under /home: {r:?}"
    );
    expect_api_error(
        "cron on a non-agent template",
        sdk.create_cron(id, &CronRequest::new("x", "* * * * *"))
            .await,
        400,
        CODE_NOT_SUPPORTED,
    )?;

    // In use: delete is refused; a new revision publishes without disturbing it.
    expect_api_error(
        "delete a template in use",
        sdk.delete_template("e2e-alpine").await,
        409,
        CODE_CONFLICT,
    )?;
    let rev2 = sdk
        .register_template(&RegisterTemplateRequest {
            description: "second revision".into(),
            ..req.clone()
        })
        .await?;
    ensure!(rev2.revision == 2, "second revision: {rev2:?}");
    ensure!(
        sdk.template("e2e-alpine").await?.revision == 2
            && sdk.template("e2e-alpine@1").await?.revision == 1,
        "both revisions readable"
    );
    ensure!(
        sdk.instance(id).await?.template_revision == 1,
        "the instance keeps its revision"
    );

    // A registered image with a default port: node:24-bookworm-slim ships no
    // gateway either and its CMD (`node`) exits at once without a stdin. With
    // default_port 3737 the bare instance URL reaches the injected gateway.
    let node_req = RegisterTemplateRequest {
        name: "e2e-node".into(),
        image_ref: "node:24-bookworm-slim".into(),
        default_port: GATEWAY_PORT,
        mount_path: "/home/node".into(),
        agent: false,
        description: "node for the e2e suite".into(),
        env: BTreeMap::new(),
    };
    let node_tpl = sdk.register_template(&node_req).await?;
    ensure!(
        node_tpl.default_port == GATEWAY_PORT && node_tpl.revision == 1,
        "registered node: {node_tpl:?}"
    );
    let node = cx.ws.create_and_wait("e2e-node", &[]).await?;
    let node_id = node.id.as_str();
    ensure!(
        node.default_port == GATEWAY_PORT
            && node.image_ref == "node:24-bookworm-slim"
            && node.url == d.instance_url(node_id, 0),
        "node instance: {node:?}"
    );
    let resp = cx
        .ws
        .edge(node_id)
        .get(0, "/v1/health", Auth::Key(key))
        .await?;
    let health = health_json(resp).await?;
    ensure!(
        health["wrapped"].is_object(),
        "the bare URL of an injected image reaches its gateway: {health}"
    );
    let r = sdk.exec(node_id, &cmd("node --version")).await?;
    ensure!(
        r.exit_code == 0 && r.stdout.trim().starts_with("v24"),
        "node --version: {r:?}"
    );
    let node_files = cx.ws.agent(node_id)?;
    node_files
        .write_file("hello.js", b"console.log('hi')".to_vec())
        .await?;
    let r = sdk.exec(node_id, &cmd("node /home/node/hello.js")).await?;
    ensure!(
        r.stdout.trim() == "hi",
        "a file written through the bare URL runs under the mount path: {r:?}"
    );
    cx.ws.delete_and_wait(node_id).await?;
    let deleted = sdk.delete_template("e2e-node").await?;
    ensure!(deleted.deleted, "delete the node template: {deleted:?}");

    cx.ws.delete_and_wait(id).await?;
    let deleted = sdk.delete_template("e2e-alpine").await?;
    ensure!(
        deleted.deleted && deleted.name == "e2e-alpine",
        "delete template: {deleted:?}"
    );
    expect_api_error(
        "deleted template",
        sdk.template("e2e-alpine").await,
        404,
        CODE_NOT_FOUND,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// vm_create_exec (WARREN_E2E_VM=1 only)

#[tokio::test]
#[ignore]
async fn vm_create_exec() {
    if !Daemon::vm_requested() {
        eprintln!(
            "vm_create_exec: skipped; set WARREN_E2E_VM=1 (needs root, /dev/kvm, firecracker and a kernel)"
        );
        return;
    }
    run_case("vm_create_exec", vm_create_exec_body).await
}

async fn vm_create_exec_body(cx: Ctx) -> Result<()> {
    ensure!(
        cx.daemon.vm_enabled(),
        "the daemon does not report the firecracker runtime: {}",
        cx.daemon.health()
    );
    let sdk = cx.ws.sdk();
    let req = CreateInstanceRequest {
        template: Some("warren-base".into()),
        kind: Some(InstanceType::Vm),
        ..Default::default()
    };
    let inst = sdk.create_instance(&req).await?;
    ensure!(inst.kind == InstanceType::Vm, "type vm: {inst:?}");
    let inst = cx.ws.wait_running(&inst.id).await?;
    let id = inst.id.as_str();
    ensure!(inst.runtime == "firecracker", "runtime: {inst:?}");
    let r = sdk.exec(id, &cmd("uname -r")).await?;
    ensure!(
        r.exit_code == 0 && !r.stdout.trim().is_empty(),
        "uname -r in the vm: {r:?}"
    );
    let r = sdk
        .exec(
            id,
            &cmd(&format!(
                "echo vm-ok > {0}/vm.txt && cat {0}/vm.txt",
                inst.mount_path
            )),
        )
        .await?;
    ensure!(r.stdout.trim() == "vm-ok", "the home is writable: {r:?}");
    let resp = cx
        .ws
        .edge(id)
        .get(0, "/v1/health", Auth::Key(cx.ws.api_key()))
        .await?;
    expect_status(resp, 200, None).await?;
    cx.ws.delete_and_wait(id).await?;
    Ok(())
}
