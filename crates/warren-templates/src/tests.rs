//! Behaviour of [`Templates`] against the in-memory fakes.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use warren_core::{
    Config, Error, InstanceType, RegisterTemplateRequest, Result, Runtime, Runtimes, Shape, Store,
    Template, TemplateScope, TemplateService,
    errors::{
        CODE_CONFLICT, CODE_FORBIDDEN, CODE_INTERNAL, CODE_INVALID_REQUEST, CODE_NOT_FOUND,
        CODE_NOT_SUPPORTED,
    },
};

use crate::{
    DEFAULT_MOUNT_PATH, Templates,
    fakes::{FakeRuntime, FakeStore, FixedClock},
};

const NOW: i64 = 1_763_000_000;
const WS: &str = "ws_aaaaaaaaaaaaaaaaaaaa";
const OTHER_WS: &str = "ws_bbbbbbbbbbbbbbbbbbbb";

struct World {
    store: Arc<FakeStore>,
    runtime: Arc<FakeRuntime>,
    clock: Arc<FixedClock>,
    cfg: Config,
    templates: Arc<Templates>,
}

fn runtimes(rt: &Arc<FakeRuntime>) -> Runtimes {
    let mut map: HashMap<InstanceType, Arc<dyn Runtime>> = HashMap::new();
    map.insert(InstanceType::Default, rt.clone());
    Arc::new(map)
}

fn world() -> World {
    let store = FakeStore::new();
    let runtime = FakeRuntime::new();
    let clock = FixedClock::new(NOW);
    let cfg = Config::default();
    let templates = Templates::with_clock(
        store.clone(),
        runtimes(&runtime),
        cfg.clone(),
        clock.clone(),
    );
    World {
        store,
        runtime,
        clock,
        cfg,
        templates,
    }
}

/// A world whose catalog is already seeded.
async fn seeded() -> World {
    let w = world();
    w.templates.ensure_system_templates().await.unwrap();
    w
}

fn request(name: &str, image_ref: &str) -> RegisterTemplateRequest {
    RegisterTemplateRequest {
        name: name.into(),
        image_ref: image_ref.into(),
        ..Default::default()
    }
}

fn code<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.code,
    }
}

fn names(ts: &[Template]) -> Vec<&str> {
    ts.iter().map(|t| t.name.as_str()).collect()
}

// ---- ensure_system_templates -------------------------------------------------

#[tokio::test]
async fn seeds_the_catalog_once() {
    let w = world();
    w.templates.ensure_system_templates().await.unwrap();
    let wanted = w.cfg.system_templates();
    assert_eq!(w.store.upsert_count(), wanted.len());
    for want in &wanted {
        let got = w.store.get_template("", &want.name).await.unwrap();
        assert_eq!(got.scope, TemplateScope::System);
        assert_eq!(got.workspace_id, "");
        assert_eq!(got.revision, 1);
        assert_eq!(got.created_at, NOW);
        assert_eq!(got.updated_at, NOW);
        assert_eq!(got.image_ref, want.image_ref);
        assert_eq!(got.default_port, want.default_port);
        assert_eq!(got.mount_path, want.mount_path);
        assert_eq!(got.agent, want.agent);
        assert_eq!(got.env, want.env);
        assert_eq!(got.types, want.types);
        assert_eq!(got.default_shape, want.default_shape);
        assert_eq!(got.description, want.description);
    }
    // Running again later changes nothing: no upsert, so no revision bump.
    w.clock.set(NOW + 3600);
    w.templates.ensure_system_templates().await.unwrap();
    w.templates.ensure_system_templates().await.unwrap();
    assert_eq!(w.store.upsert_count(), wanted.len());
    for want in &wanted {
        let got = w.store.get_template("", &want.name).await.unwrap();
        assert_eq!(got.revision, 1, "{}", want.name);
        assert_eq!(got.updated_at, NOW, "{}", want.name);
        assert_eq!(w.store.revisions("", &want.name), vec![1]);
    }
}

#[tokio::test]
async fn changed_definition_publishes_a_new_revision_and_keeps_created_at() {
    let w = world();
    let catalog = w.cfg.system_templates();
    // An older daemon stored warren-base on a different port with a digest.
    let mut old = catalog[0].clone();
    old.default_port = 8080;
    old.image_digest = "sha256:old".into();
    old.created_at = NOW - 86_400;
    old.updated_at = NOW - 86_400;
    old.revision = 1;
    w.store.seed(old.clone());
    // warren-agent is unchanged apart from its description.
    let mut described = catalog[1].clone();
    described.description = "an older blurb".into();
    described.created_at = NOW - 86_400;
    described.updated_at = NOW - 86_400;
    w.store.seed(described);
    // warren-agent again with the same types in another order, still unchanged.
    let mut reordered = catalog[2].clone();
    reordered.types.reverse();
    reordered.created_at = NOW - 86_400;
    w.store.seed(reordered);

    w.templates.ensure_system_templates().await.unwrap();

    assert_eq!(w.store.upsert_count(), 1, "only warren-base changed");
    let sent = w.store.last_upsert();
    assert_eq!(sent.name, "warren-base");
    assert_eq!(sent.revision, 2, "the service asks for the next revision");
    assert_eq!(sent.scope, TemplateScope::System);
    assert_eq!(sent.workspace_id, "");
    let base = w.store.get_template("", "warren-base").await.unwrap();
    assert_eq!(base.revision, 2);
    assert_eq!(base.default_port, catalog[0].default_port);
    assert_eq!(base.created_at, NOW - 86_400, "created_at is preserved");
    assert_eq!(base.updated_at, NOW);
    assert_eq!(base.image_digest, "sha256:old", "same image, digest kept");
    assert_eq!(w.store.revisions("", "warren-base"), vec![1, 2]);
    let agent = w.store.get_template("", "warren-agent").await.unwrap();
    assert_eq!(agent.revision, 1);
    assert_eq!(agent.description, "an older blurb");
    let opencode = w.store.get_template("", "warren-opencode").await.unwrap();
    assert_eq!(opencode.revision, 1);
}

#[tokio::test]
async fn new_image_tag_publishes_new_revisions_without_a_stale_digest() {
    let w = seeded().await;
    for t in w.cfg.system_templates() {
        let mut with_digest = w.store.get_template("", &t.name).await.unwrap();
        with_digest.image_digest = "sha256:dev".into();
        with_digest.revision = 1;
        w.store.seed(with_digest);
    }
    let mut cfg = w.cfg.clone();
    cfg.image_tag = "v2".into();
    w.clock.set(NOW + 60);
    let upgraded =
        Templates::with_clock(w.store.clone(), runtimes(&w.runtime), cfg, w.clock.clone());
    upgraded.ensure_system_templates().await.unwrap();
    for name in ["warren-base", "warren-agent", "warren-opencode"] {
        let t = w.store.get_template("", name).await.unwrap();
        assert_eq!(t.revision, 2, "{name}");
        assert!(t.image_ref.ends_with(":v2"), "{name}: {}", t.image_ref);
        assert_eq!(t.image_digest, "", "{name}: a new image has no digest yet");
        assert_eq!(t.created_at, NOW);
        assert_eq!(t.updated_at, NOW + 60);
    }
    assert!(w.runtime.pulls().is_empty(), "seeding never pulls images");
}

#[tokio::test]
async fn ensure_propagates_store_failures() {
    let w = world();
    *w.store.fail_get.lock().unwrap() = Some(Error::internal("disk on fire"));
    assert_eq!(
        code(w.templates.ensure_system_templates().await),
        CODE_INTERNAL
    );
    assert_eq!(w.store.upsert_count(), 0);
}

// ---- resolve / get -----------------------------------------------------------

#[tokio::test]
async fn resolves_bare_names() {
    let w = seeded().await;
    let base = w.templates.resolve(WS, "warren-base").await.unwrap();
    assert_eq!(base.scope, TemplateScope::System);
    assert_eq!(base.name, "warren-base");
    assert_eq!(
        w.templates.resolve("", "warren-agent").await.unwrap().name,
        "warren-agent"
    );
    assert_eq!(
        w.templates.resolve(WS, " warren-base ").await.unwrap().name,
        "warren-base"
    );
    w.templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:1"))
        .await
        .unwrap();
    let mine = w.templates.resolve(WS, "my-agent").await.unwrap();
    assert_eq!(mine.scope, TemplateScope::Workspace);
    assert_eq!(mine.workspace_id, WS);
    assert_eq!(
        code(w.templates.resolve(OTHER_WS, "my-agent").await),
        CODE_NOT_FOUND
    );
    assert_eq!(code(w.templates.resolve(WS, "nope").await), CODE_NOT_FOUND);
}

#[tokio::test]
async fn resolves_pinned_workspace_revisions() {
    let w = seeded().await;
    w.templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:1"))
        .await
        .unwrap();
    w.templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:2"))
        .await
        .unwrap();
    let current = w.templates.resolve(WS, "my-agent").await.unwrap();
    assert_eq!(current.revision, 2);
    assert_eq!(current.image_ref, "ghcr.io/acme/agent:2");
    let pinned = w.templates.resolve(WS, "my-agent@1").await.unwrap();
    assert_eq!(pinned.revision, 1);
    assert_eq!(pinned.image_ref, "ghcr.io/acme/agent:1");
    assert_eq!(
        w.templates
            .resolve(WS, "my-agent@2")
            .await
            .unwrap()
            .revision,
        2
    );
    assert_eq!(
        code(w.templates.resolve(WS, "my-agent@3").await),
        CODE_NOT_FOUND
    );
    assert_eq!(
        code(w.templates.resolve(OTHER_WS, "my-agent@1").await),
        CODE_NOT_FOUND
    );
    assert_eq!(
        code(w.templates.resolve(WS, "unknown@1").await),
        CODE_NOT_FOUND
    );
}

#[tokio::test]
async fn system_pins_must_match_the_current_revision() {
    let w = seeded().await;
    assert_eq!(
        w.templates
            .resolve(WS, "warren-base@1")
            .await
            .unwrap()
            .revision,
        1
    );
    let err = w.templates.resolve(WS, "warren-base@2").await.unwrap_err();
    assert_eq!(err.code, CODE_NOT_FOUND);
    assert_eq!(err.message, "template revision not found");
    assert_eq!(
        code(w.templates.resolve(WS, "warren-base@7").await),
        CODE_NOT_FOUND
    );
}

#[tokio::test]
async fn rejects_malformed_references() {
    let w = seeded().await;
    for bad in [
        "",
        "  ",
        "@1",
        "warren-base@",
        "warren-base@0",
        "warren-base@x",
        "warren-base@1@2",
    ] {
        assert_eq!(
            code(w.templates.resolve(WS, bad).await),
            CODE_INVALID_REQUEST,
            "{bad:?}"
        );
        assert_eq!(
            code(w.templates.get(WS, bad).await),
            CODE_INVALID_REQUEST,
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn get_delegates_to_the_store_and_accepts_pins() {
    let w = seeded().await;
    assert_eq!(
        w.templates.get(WS, "warren-opencode").await.unwrap().name,
        "warren-opencode"
    );
    assert_eq!(code(w.templates.get(WS, "missing").await), CODE_NOT_FOUND);
    w.templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:1"))
        .await
        .unwrap();
    w.templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:2"))
        .await
        .unwrap();
    assert_eq!(w.templates.get(WS, "my-agent").await.unwrap().revision, 2);
    assert_eq!(w.templates.get(WS, "my-agent@1").await.unwrap().revision, 1);
    assert_eq!(
        code(w.templates.get(WS, "my-agent@9").await),
        CODE_NOT_FOUND
    );
    assert_eq!(
        code(w.templates.get(WS, "warren-base@2").await),
        CODE_NOT_FOUND
    );
}

// ---- list --------------------------------------------------------------------

#[tokio::test]
async fn lists_system_templates_first_then_workspace_templates_by_name() {
    let w = seeded().await;
    w.templates
        .register(WS, &request("zeta", "img:z"))
        .await
        .unwrap();
    w.templates
        .register(WS, &request("alpha", "img:a"))
        .await
        .unwrap();
    w.templates
        .register(OTHER_WS, &request("theirs", "img:t"))
        .await
        .unwrap();
    let mine = w.templates.list(WS).await.unwrap();
    assert_eq!(
        names(&mine),
        vec![
            "warren-agent",
            "warren-base",
            "warren-opencode",
            "alpha",
            "zeta"
        ]
    );
    let theirs = w.templates.list(OTHER_WS).await.unwrap();
    assert_eq!(
        names(&theirs),
        vec!["warren-agent", "warren-base", "warren-opencode", "theirs"]
    );
    let anonymous = w.templates.list("").await.unwrap();
    assert_eq!(
        names(&anonymous),
        vec!["warren-agent", "warren-base", "warren-opencode"]
    );
}

// ---- register ----------------------------------------------------------------

#[tokio::test]
async fn register_pulls_the_image_and_stores_the_template() {
    let w = seeded().await;
    let req = RegisterTemplateRequest {
        name: "my-agent".into(),
        image_ref: " ghcr.io/acme/agent:1 ".into(),
        default_port: 8080,
        mount_path: String::new(),
        agent: true,
        description: " Runs my agent. ".into(),
        env: BTreeMap::from([("MODEL".to_string(), "gpt".to_string())]),
    };
    let t = w.templates.register(WS, &req).await.unwrap();
    assert_eq!(w.runtime.pulls(), vec!["ghcr.io/acme/agent:1"]);
    assert_eq!(t.name, "my-agent");
    assert_eq!(t.scope, TemplateScope::Workspace);
    assert_eq!(t.workspace_id, WS);
    assert_eq!(t.image_ref, "ghcr.io/acme/agent:1");
    assert_eq!(
        t.image_digest,
        FakeRuntime::digest_for("ghcr.io/acme/agent:1")
    );
    assert_eq!(t.default_port, 8080);
    assert_eq!(t.mount_path, DEFAULT_MOUNT_PATH);
    assert!(t.agent);
    assert_eq!(t.description, "Runs my agent.");
    assert_eq!(t.types, vec![InstanceType::Default, InstanceType::Vm]);
    assert_eq!(
        t.default_shape,
        Shape {
            cpu: 2,
            memory: 4,
            disk: 4
        }
    );
    assert_eq!(t.env, req.env);
    assert_eq!(t.revision, 1);
    assert_eq!(t.created_at, NOW);
    assert_eq!(t.updated_at, NOW);
    assert_eq!(w.store.get_template(WS, "my-agent").await.unwrap(), t);
    // A custom mount path is normalized.
    let mut req2 = request("worker", "img:w");
    req2.mount_path = "/srv/work/".into();
    assert_eq!(
        w.templates.register(WS, &req2).await.unwrap().mount_path,
        "/srv/work"
    );
}

#[tokio::test]
async fn registering_again_publishes_a_new_revision_and_keeps_created_at() {
    let w = seeded().await;
    let first = w
        .templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:1"))
        .await
        .unwrap();
    w.clock.set(NOW + 500);
    let second = w
        .templates
        .register(WS, &request("my-agent", "ghcr.io/acme/agent:2"))
        .await
        .unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(second.revision, 2);
    assert_eq!(second.created_at, NOW, "created_at survives republishing");
    assert_eq!(second.updated_at, NOW + 500);
    assert_eq!(second.image_ref, "ghcr.io/acme/agent:2");
    assert_eq!(
        second.image_digest,
        FakeRuntime::digest_for("ghcr.io/acme/agent:2")
    );
    assert_eq!(
        w.store.last_upsert().revision,
        2,
        "the service asks for the next revision"
    );
    assert_eq!(w.store.revisions(WS, "my-agent"), vec![1, 2]);
    assert_eq!(
        w.runtime.pulls(),
        vec!["ghcr.io/acme/agent:1", "ghcr.io/acme/agent:2"]
    );
    // The same name in another workspace is independent and starts at 1.
    let theirs = w
        .templates
        .register(OTHER_WS, &request("my-agent", "img:x"))
        .await
        .unwrap();
    assert_eq!(theirs.revision, 1);
    assert_eq!(theirs.workspace_id, OTHER_WS);
    assert_eq!(w.templates.get(WS, "my-agent").await.unwrap().revision, 2);
}

#[tokio::test]
async fn register_validates_before_pulling() {
    let w = seeded().await;
    let before = w.store.upsert_count();
    let cases: Vec<(&str, RegisterTemplateRequest)> = vec![
        ("uppercase name", request("My-Agent", "img:1")),
        ("short name", request("ab", "img:1")),
        ("underscore", request("my_agent", "img:1")),
        ("reserved prefix", request("warren-custom", "img:1")),
        ("pin in name", request("my-agent@1", "img:1")),
        ("empty image", request("my-agent", "")),
        ("blank image", request("my-agent", "   ")),
        ("image with space", request("my-agent", "img 1")),
        (
            "relative mount",
            RegisterTemplateRequest {
                mount_path: "home/agent".into(),
                ..request("my-agent", "img:1")
            },
        ),
        (
            "root mount",
            RegisterTemplateRequest {
                mount_path: "/".into(),
                ..request("my-agent", "img:1")
            },
        ),
        (
            "dotdot mount",
            RegisterTemplateRequest {
                mount_path: "/home/../x".into(),
                ..request("my-agent", "img:1")
            },
        ),
        (
            "bad env key",
            RegisterTemplateRequest {
                env: BTreeMap::from([("lower".to_string(), "v".to_string())]),
                ..request("my-agent", "img:1")
            },
        ),
    ];
    for (label, req) in cases {
        assert_eq!(
            code(w.templates.register(WS, &req).await),
            CODE_INVALID_REQUEST,
            "{label}"
        );
    }
    assert!(
        w.runtime.pulls().is_empty(),
        "nothing is pulled for an invalid request"
    );
    assert_eq!(
        w.store.upsert_count(),
        before,
        "nothing is stored for an invalid request"
    );
    assert_eq!(
        code(
            w.templates
                .register("", &request("my-agent", "img:1"))
                .await
        ),
        CODE_INVALID_REQUEST
    );
}

#[tokio::test]
async fn register_without_a_default_runtime_is_not_supported() {
    let store = FakeStore::new();
    let templates = Templates::with_clock(
        store.clone(),
        Arc::new(HashMap::new()),
        Config::default(),
        FixedClock::new(NOW),
    );
    templates.ensure_system_templates().await.unwrap();
    let before = store.upsert_count();
    assert_eq!(
        code(templates.register(WS, &request("my-agent", "img:1")).await),
        CODE_NOT_SUPPORTED
    );
    assert_eq!(store.upsert_count(), before);
    assert_eq!(code(templates.get(WS, "my-agent").await), CODE_NOT_FOUND);
}

#[tokio::test]
async fn register_propagates_pull_failures_and_stores_nothing() {
    let w = seeded().await;
    *w.runtime.fail_pull.lock().unwrap() = Some(Error::invalid("image not found: img:missing"));
    let before = w.store.upsert_count();
    let err = w
        .templates
        .register(WS, &request("my-agent", "img:missing"))
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INVALID_REQUEST);
    assert!(err.message.contains("img:missing"));
    assert_eq!(w.runtime.pulls(), vec!["img:missing"]);
    assert_eq!(w.store.upsert_count(), before);
    assert_eq!(code(w.templates.get(WS, "my-agent").await), CODE_NOT_FOUND);
}

#[tokio::test]
async fn register_propagates_store_failures() {
    let w = seeded().await;
    *w.store.fail_get.lock().unwrap() = Some(Error::internal("locked"));
    assert_eq!(
        code(
            w.templates
                .register(WS, &request("my-agent", "img:1"))
                .await
        ),
        CODE_INTERNAL
    );
    assert!(w.runtime.pulls().is_empty());
}

// ---- delete ------------------------------------------------------------------

#[tokio::test]
async fn delete_rules() {
    let w = seeded().await;
    w.templates
        .register(WS, &request("my-agent", "img:1"))
        .await
        .unwrap();
    w.templates
        .register(WS, &request("busy", "img:2"))
        .await
        .unwrap();
    w.store.set_in_use(WS, "busy", 2);

    assert_eq!(
        code(w.templates.delete(WS, "warren-base").await),
        CODE_FORBIDDEN
    );
    assert_eq!(
        code(w.templates.delete(WS, "warren-base@1").await),
        CODE_NOT_FOUND
    );
    let busy = w.templates.delete(WS, "busy").await.unwrap_err();
    assert_eq!(busy.code, CODE_CONFLICT);
    assert!(busy.message.contains('2'), "{}", busy.message);
    assert_eq!(
        code(w.templates.delete(WS, "missing").await),
        CODE_NOT_FOUND
    );
    assert_eq!(
        code(w.templates.delete(OTHER_WS, "my-agent").await),
        CODE_NOT_FOUND
    );
    assert_eq!(
        code(w.templates.delete("", "my-agent").await),
        CODE_INVALID_REQUEST
    );

    w.templates.delete(WS, "my-agent").await.unwrap();
    assert_eq!(code(w.templates.get(WS, "my-agent").await), CODE_NOT_FOUND);
    assert_eq!(
        code(w.templates.delete(WS, "my-agent").await),
        CODE_NOT_FOUND
    );
    assert_eq!(
        names(&w.templates.list(WS).await.unwrap()),
        vec!["warren-agent", "warren-base", "warren-opencode", "busy"]
    );
    assert!(
        w.templates.get(WS, "warren-base").await.is_ok(),
        "the catalog is untouched"
    );
}
