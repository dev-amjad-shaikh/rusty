//! Learning-policy declaration and enforcement (EP-08-S07): the policy is
//! declared on the immutable assistant version and validated at creation;
//! the hunting cycle spends the declared budget exactly, with the stoppage
//! counted and never silently exceeded; a policy change reaches a running
//! session only when the session adopts the new version at a turn boundary
//! (EP-08-S08); the promotion gate's suite reference and approval scope
//! govern from the blueprint; and the effective-policy read answers the
//! declaration with the live budget consumption.
//!
//! Driven in-process via `tower::ServiceExt::oneshot`, the
//! `fleet_upgrade.rs` convention.

use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

fn registry() -> GraphRegistry {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("work", |_ctx: NodeContext| async move {
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("work");
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    registry
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-learning-policy-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn app(store: PathBuf) -> Router {
    router(
        registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store),
    )
}

fn tenant_app(store: PathBuf) -> Router {
    router(
        registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store)
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret"),
    )
}

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

async fn call_as(
    app: &Router,
    key: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = key {
        builder = builder.header(k, v);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

// --------------------------------------------------------------------- //
// Fixtures
// --------------------------------------------------------------------- //

fn policy_config(max_hunts: u32) -> Value {
    json!({
        "learning_policy": {
            "hunting_budget": {"max_hunts_per_cycle": max_hunts, "max_probes_per_cycle": 4},
        }
    })
}

/// Create the assistant; returns its active (root) version id.
async fn create_assistant(app: &Router, id: &str, config: Value) -> String {
    let (status, value) = call(
        app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Learning scout",
            "graph": "pipeline",
            "config": config,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {value}");
    value["active_version_id"].as_str().unwrap().to_string()
}

/// Publish a child version; returns its version id.
async fn create_version(app: &Router, id: &str, base: &str, config: Value) -> String {
    let (status, value) = call(
        app,
        "POST",
        &format!("/assistants/{id}/versions"),
        Some(json!({
            "base_version_id": base,
            "name": "Learning scout",
            "graph": "pipeline",
            "config": config,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "version failed: {value}");
    value["version"]["version_id"].as_str().unwrap().to_string()
}

/// Create a thread and run it once bound to the assistant — the pin lands.
async fn pin_thread(app: &Router, assistant: &str) -> String {
    let (status, thread) = call(app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();
    run_turn(app, &thread_id, assistant).await;
    thread_id
}

async fn run_turn(app: &Router, thread_id: &str, assistant: &str) {
    let (status, run) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"assistant_id": assistant, "input": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {run}");
}

/// File one gap; returns its id. `n` keeps statements distinct.
async fn file_gap(app: &Router, n: u32) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/gaps/file",
        Some(json!({
            "subject": {"question_shape": {"text": format!("topic {n}")}},
            "statement": format!("No answer for question {n}"),
            "evidence": [{"kind": "interaction_event", "id": format!("ie-{n}")}],
            "origin": "operator",
            "closure_criteria": {"block_filled": {"block_label": format!("runbook-{n}")}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "filing failed: {v}");
    v["gap_id"].as_str().unwrap().to_string()
}

/// Run one hunting cycle; returns the raw response.
async fn cycle(app: &Router, body: Value) -> (StatusCode, Value) {
    call(app, "POST", "/hunts/cycle", Some(body)).await
}

/// The effective-policy read.
async fn effective_policy(
    app: &Router,
    assistant: &str,
    thread: Option<&str>,
) -> (StatusCode, Value) {
    let uri = match thread {
        Some(thread) => format!("/assistants/{assistant}/learning-policy?thread_id={thread}"),
        None => format!("/assistants/{assistant}/learning-policy"),
    };
    call(app, "GET", &uri, None).await
}

// --------------------------------------------------------------------- //
// Declaration and validation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_declaration_is_validated_at_creation() {
    let store = temp_store();
    let server = app(store.clone());

    // An invalid declaration never enters the lineage: every violation is
    // named in the 400.
    let (status, value) = call(
        &server,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "bad",
            "name": "Learning scout",
            "graph": "pipeline",
            "config": {"learning_policy": {"hunting_budget": {"max_hunts_per_cycle": 0, "max_probes_per_cycle": 4}}},
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid must fail: {value}"
    );
    let message = value["error"].as_str().unwrap_or_default().to_string()
        + value["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("max_hunts_per_cycle must be at least 1"),
        "the violation is named: {value}"
    );

    // A valid declaration creates, and the version id covers the policy.
    let v1 = create_assistant(&server, "learner", policy_config(2)).await;

    // A child version with an invalid declaration fails the same way.
    let (status, value) = call(
        &server,
        "POST",
        "/assistants/learner/versions",
        Some(json!({
            "base_version_id": v1,
            "name": "Learning scout",
            "graph": "pipeline",
            "config": {"learning_policy": {"hunting_budget": {"max_hunts_per_cycle": 65, "max_probes_per_cycle": 4}}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "invalid child: {value}");
    let message = value["error"].as_str().unwrap_or_default().to_string()
        + value["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("max_hunts_per_cycle must be at most 64"),
        "the ceiling violation is named: {value}"
    );

    // A valid child creates.
    create_version(&server, "learner", &v1, policy_config(3)).await;
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_effective_policy_read_answers_declared_or_floor_with_consumption() {
    let store = temp_store();
    let server = app(store.clone());

    // An assistant that declares nothing floors — and the answer says so.
    create_assistant(&server, "plain", json!({"recursion_limit": 8})).await;
    let (status, value) = effective_policy(&server, "plain", None).await;
    assert_eq!(status, StatusCode::OK, "floor read failed: {value}");
    assert_eq!(value["declared"], json!(false));
    assert_eq!(
        value["policy"]["hunting_budget"]["max_hunts_per_cycle"],
        json!(64)
    );
    assert_eq!(value["policy"]["review_fork_enabled"], json!(true));
    assert_eq!(value["consumption"]["hunts_in_flight"], json!(0));

    // A declaring assistant answers its own declaration.
    let v1 = create_assistant(&server, "learner", policy_config(2)).await;
    file_gap(&server, 1).await;
    let (status, value) = effective_policy(&server, "learner", None).await;
    assert_eq!(status, StatusCode::OK, "declared read failed: {value}");
    assert_eq!(value["declared"], json!(true));
    assert_eq!(value["version_id"], json!(v1));
    assert_eq!(
        value["policy"]["hunting_budget"]["max_hunts_per_cycle"],
        json!(2)
    );
    assert_eq!(
        value["policy"]["hunting_budget"]["max_probes_per_cycle"],
        json!(4)
    );

    // Unknown assistant is a 404.
    let (status, _) = effective_policy(&server, "ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The hunting budget, enforced exactly
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_scoped_cycle_spends_the_declared_budget_exactly() {
    let store = temp_store();
    let server = app(store.clone());
    create_assistant(&server, "learner", policy_config(2)).await;
    for n in 1..=4 {
        file_gap(&server, n).await;
    }

    // The declared budget drives: exactly two hunts, the stoppage counted.
    let (status, value) = cycle(&server, json!({"assistant_id": "learner"})).await;
    assert_eq!(status, StatusCode::OK, "cycle failed: {value}");
    assert_eq!(value["budget"], json!(2));
    assert_eq!(value["cycle_hunts"].as_array().unwrap().len(), 2);
    assert_eq!(value["deferred"], json!(2), "the stoppage is counted");
    assert_eq!(value["policy"]["declared"], json!(true));

    // The caller may narrow the declared budget…
    let (status, value) = cycle(&server, json!({"assistant_id": "learner", "max_hunts": 1})).await;
    assert_eq!(status, StatusCode::OK, "narrowed cycle failed: {value}");
    assert_eq!(value["budget"], json!(1));
    assert_eq!(value["deferred"], json!(1));

    // …never exceed it — never silently.
    let (status, value) = cycle(&server, json!({"assistant_id": "learner", "max_hunts": 64})).await;
    assert_eq!(status, StatusCode::OK, "clamped cycle failed: {value}");
    assert_eq!(value["budget"], json!(2), "the declared budget bounds");
    assert_eq!(value["cycle_hunts"].as_array().unwrap().len(), 1);

    // The live consumption reads back against the budget.
    let (status, value) = effective_policy(&server, "learner", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["consumption"]["hunts_in_flight"], json!(4));

    // `thread_id` without `assistant_id`, and a zero budget, are 400s.
    let (status, _) = cycle(&server, json!({"thread_id": "t-1"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = cycle(&server, json!({"assistant_id": "learner", "max_hunts": 0})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An unknown assistant is a 404; the queue is untouched.
    let (status, _) = cycle(&server, json!({"assistant_id": "ghost"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_pinned_thread_names_the_governing_version() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, "learner", policy_config(1)).await;
    create_assistant(&server, "other", policy_config(5)).await;
    let thread = pin_thread(&server, "learner").await;
    for n in 1..=3 {
        file_gap(&server, n).await;
    }

    // The pinned session's cycle reads the pinned version's policy.
    let (status, value) = cycle(
        &server,
        json!({"assistant_id": "learner", "thread_id": thread}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pinned cycle failed: {value}");
    assert_eq!(value["budget"], json!(1));
    assert_eq!(value["policy"]["version_id"], json!(v1));

    // A thread pinned to another assistant is a 409 — the ambiguity is
    // never resolved silently.
    let (status, _) = cycle(
        &server,
        json!({"assistant_id": "other", "thread_id": thread}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_policy_change_reaches_the_session_only_at_adoption() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, "learner", policy_config(1)).await;
    let thread = pin_thread(&server, "learner").await;
    for n in 1..=6 {
        file_gap(&server, n).await;
    }

    // Under v1 the cycle hunts exactly one.
    let (status, value) = cycle(
        &server,
        json!({"assistant_id": "learner", "thread_id": &thread}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "v1 cycle failed: {value}");
    assert_eq!(value["budget"], json!(1));
    assert_eq!(value["policy"]["version_id"], json!(v1));

    // Publish v2 with a wider budget and open the upgrade. Nothing about
    // the session's background behavior changes yet — the in-flight policy
    // is the pin's, and the pin moves only at a turn boundary.
    let v2 = create_version(&server, "learner", &v1, policy_config(3)).await;
    let (status, value) = call(
        &server,
        "POST",
        "/assistants/learner/upgrades",
        Some(json!({"target_version_id": v2, "idempotency_key": "roll-policy"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "initiate failed: {value}");

    let (status, value) = effective_policy(&server, "learner", Some(&thread)).await;
    assert_eq!(status, StatusCode::OK, "pre-adoption read failed: {value}");
    assert_eq!(value["version_id"], json!(v1), "never mid-cycle");
    assert_eq!(
        value["policy"]["hunting_budget"]["max_hunts_per_cycle"],
        json!(1)
    );
    let (status, value) = cycle(
        &server,
        json!({"assistant_id": "learner", "thread_id": &thread}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pre-adoption cycle failed: {value}");
    assert_eq!(
        value["budget"],
        json!(1),
        "the open upgrade changes nothing in flight"
    );
    assert_eq!(value["policy"]["version_id"], json!(v1));

    // The next turn adopts at the boundary; the cycle after it runs under
    // the new policy.
    run_turn(&server, &thread, "learner").await;
    let (status, value) = effective_policy(&server, "learner", Some(&thread)).await;
    assert_eq!(status, StatusCode::OK, "post-adoption read failed: {value}");
    assert_eq!(value["version_id"], json!(v2));
    let (status, value) = cycle(
        &server,
        json!({"assistant_id": "learner", "thread_id": &thread}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "post-adoption cycle failed: {value}"
    );
    assert_eq!(value["budget"], json!(3), "the adopted policy governs");
    assert_eq!(value["policy"]["version_id"], json!(v2));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The promotion gate
// --------------------------------------------------------------------- //

fn skill_md(name: &str, gate: Option<&str>) -> String {
    match gate {
        Some(gate) => format!(
            "---\nname: {name}\ndescription: The {name} skill.\neval-gate: {gate}\n---\n\nDo the work.\n"
        ),
        None => format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\nDo the work.\n"),
    }
}

async fn register_skill(app: &Router, name: &str, gate: Option<&str>) {
    let (status, v) = call(
        app,
        "POST",
        "/skills",
        Some(json!({"skill_md": skill_md(name, gate), "author": "operator:ada"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
}

async fn promote(app: &Router, name: &str, body: Value) -> (StatusCode, Value) {
    let mut payload = json!({"author": "operator:ada"});
    for (key, value) in body.as_object().unwrap() {
        payload[key] = value.clone();
    }
    call(
        app,
        "POST",
        &format!("/skills/{name}/promote"),
        Some(payload),
    )
    .await
}

#[tokio::test]
async fn the_policy_gate_governs_promotion_from_the_blueprint() {
    let store = temp_store();
    let server = app(store.clone());
    register_skill(&server, "ungated-skill", None).await;

    // Pre-policy behavior: a skill with no eval_gate and no policy context
    // is a 422.
    let (status, value) = promote(&server, "ungated-skill", json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "ungated: {value}");
    assert_eq!(value["error"], json!("no_gate_declared"));

    // The blueprint's declared suite reference supplies the gate: the
    // promotion now reaches the evaluator — which fails closed here, a
    // loud failure, never an ungated promotion (AC 3).
    create_assistant(
        &server,
        "gated",
        json!({"learning_policy": {"skill_promotion": {"eval_suite_ref": "suite://support-v3"}}}),
    )
    .await;
    let (status, value) = promote(&server, "ungated-skill", json!({"assistant_id": "gated"})).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the policy's gate runs and fails loudly: {value}"
    );
    assert_ne!(value["error"], json!("no_gate_declared"));

    // An unknown assistant context is a 404.
    let (status, _) = promote(&server, "ungated-skill", json!({"assistant_id": "ghost"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn the_declared_approval_scope_demands_its_own_approval() {
    let store = temp_store();
    let server = app(store.clone());
    register_skill(&server, "gated-skill", Some("suite-a")).await;
    create_assistant(
        &server,
        "gated",
        json!({"learning_policy": {"skill_promotion": {"approval_scope": "ops:learning"}}}),
    )
    .await;

    // No approval: 403 naming the required scope.
    let (status, value) = promote(&server, "gated-skill", json!({"assistant_id": "gated"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "approval demanded: {value}");
    assert_eq!(value["error"], json!("approval_required"));
    assert!(
        value["message"].as_str().unwrap().contains("ops:learning"),
        "the required scope is named: {value}"
    );

    // An approval minted by another scope admits nothing.
    let (status, value) = promote(
        &server,
        "gated-skill",
        json!({
            "assistant_id": "gated",
            "approval": {"effect_id": "ab12cd", "approved_by": "ops:other"},
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the scope must match: {value}"
    );

    // The scope's own approval passes the gate's approval demand; the
    // suite run still fails closed here — loudly, never ungated.
    let (status, value) = promote(
        &server,
        "gated-skill",
        json!({
            "assistant_id": "gated",
            "approval": {"effect_id": "ab12cd", "approved_by": "ops:learning"},
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "approval admitted, the gate fails loudly: {value}"
    );
    assert_ne!(value["error"], json!("approval_required"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenancy
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tenants_never_see_each_others_policies() {
    let store = temp_store();
    let server = tenant_app(store.clone());
    let (status, value) = call_as(
        &server,
        Some(ACME),
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "learner",
            "name": "Learning scout",
            "graph": "pipeline",
            "config": policy_config(2),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {value}");

    let (status, _) = call_as(
        &server,
        Some(GLOBEX),
        "GET",
        "/assistants/learner/learning-policy",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &server,
        Some(GLOBEX),
        "POST",
        "/hunts/cycle",
        Some(json!({"assistant_id": "learner"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, value) = call_as(
        &server,
        Some(ACME),
        "GET",
        "/assistants/learner/learning-policy",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["declared"], json!(true));
    let _ = std::fs::remove_dir_all(store);
}
