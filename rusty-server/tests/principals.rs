//! Connector surface integration tests: the `/connectors/*` HTTP surface
//! over the default JSON-file backend — manifest registration (content
//! addressing, declaration validation), instantiation with schema
//! validation (the 422 field-path contract) and broker-sealed secrets,
//! the check gate (pre-save and live-instance), the derived catalog,
//! restart replay, and tenant isolation (404-never-403).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `knowledge.rs` convention. The check-gate failure test points at an
//! unresolvable instance name, so it is network-independent: DNS failure
//! and connection refused both land in the same `failed` verdict.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-connectors-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_at(store.clone()), store)
}

/// Open-mode app over a given store root (restart tests build it twice).
fn app_at(store: PathBuf) -> Router {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(GraphRegistry::new(), config)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Send a request with an optional auth header.
async fn call_as(
    app: &Router,
    auth: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = auth {
        builder = builder.header("X-Api-Key", key);
    }
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// --------------------------------------------------------------------- //
// Manifest registration
// --------------------------------------------------------------------- //

// --------------------------------------------------------------------- //
// Principals: a key is a credential; a principal is who presented it.
// --------------------------------------------------------------------- //
use rusty_agent_server::{Principal, PrincipalKind, Role};

fn principals_app() -> (Router, PathBuf) {
    let store = temp_store();
    let user = |id: &str, role: Role| Principal {
        id: id.to_owned(),
        name: id.to_owned(),
        kind: PrincipalKind::User,
        roles: vec![role],
    };
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_principal("default", user("ada", Role::Admin), "ada-key")
        .with_principal("default", user("bob", Role::Builder), "bob-key")
        .with_principal("default", user("olga", Role::Operator), "olga-key")
        .with_principal("default", user("ann", Role::Auditor), "ann-key");
    (router(GraphRegistry::new(), config), store)
}

#[tokio::test]
async fn me_says_who_the_key_is_and_what_they_may_do() {
    let (app, store) = principals_app();

    let (status, me) = call_as(&app, Some("bob-key"), "GET", "/me", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"]["id"], "bob");
    assert_eq!(me["principal"]["kind"], "user");
    assert_eq!(me["roles"], json!(["builder"]));
    let scopes = me["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|s| s == "assistants:write"), "{scopes:?}");
    assert!(
        !scopes.iter().any(|s| s == "assistants:activate"),
        "a builder does not activate: {scopes:?}"
    );

    // No key at all is nobody, and nobody is refused — this server has principals.
    let (status, _) = call(&app, "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn roles_separate_who_may_make_run_and_promote() {
    let (app, store) = principals_app();
    let assistant = json!({"name": "Analyst", "graph": "react_agent"});

    // Registry is empty in this harness, so creation fails on the graph —
    // but only after authorization passed. The auditor never gets that far.
    let (status, body) = call_as(
        &app,
        Some("ann-key"),
        "POST",
        "/assistants",
        Some(assistant.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an auditor changes nothing: {body}"
    );
    let (status, _) = call_as(
        &app,
        Some("olga-key"),
        "POST",
        "/assistants",
        Some(assistant.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an operator does not change what an agent is"
    );
    let (status, body) = call_as(
        &app,
        Some("bob-key"),
        "POST",
        "/assistants",
        Some(assistant.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a builder is admitted; only the graph is missing: {body}"
    );

    // Promotion is the operator's and the admin's, not the builder's.
    let activate = json!({"expected_active_version_id": "av-0000000000000000000000000000000000000000000000000000000000000000"});
    let (status, _) = call_as(
        &app,
        Some("bob-key"),
        "POST",
        "/assistants/x/versions/av-0/activate",
        Some(activate.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a builder cannot promote");
    let (status, _) = call_as(
        &app,
        Some("olga-key"),
        "POST",
        "/assistants/x/versions/av-0/activate",
        Some(activate),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "an operator is admitted to promote (and then told what is wrong)"
    );

    // Reading is everyone's.
    for key in ["ada-key", "bob-key", "olga-key", "ann-key"] {
        let (status, _) = call_as(&app, Some(key), "GET", "/assistants", None).await;
        assert_eq!(status, StatusCode::OK, "{key} may read");
    }

    let _ = std::fs::remove_dir_all(store);
}

/// The routes this month added keep the roles' lines: an auditor reads
/// everything and changes nothing — the standing approvals, the seed a
/// world would start from, the task board — while deciding, withdrawing,
/// making a world or a webhook are the builder's and the operator's as
/// their scopes say. Admission is checked before anything else, so a
/// refused role reads 403 and an admitted one reads what is wrong.
#[tokio::test]
async fn roles_gate_standing_approvals_worlds_and_the_board() {
    let (app, store) = principals_app();
    // Reading what stands is everyone's.
    for key in ["ada-key", "bob-key", "olga-key", "ann-key"] {
        let (status, body) = call_as(&app, Some(key), "GET", "/approvals/standing", None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{key} reads the standing approvals: {body}"
        );
        assert!(body["standing"].is_array(), "{body}");
        let (status, body) = call_as(&app, Some(key), "GET", "/tasks", None).await;
        assert_eq!(status, StatusCode::OK, "{key} reads the board: {body}");
        let (status, body) = call_as(
            &app,
            Some(key),
            "GET",
            "/worlds/starter?connector=nope",
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{key} is admitted to ask for a starter, and told there is no such connector: {body}"
        );
    }
    // Withdrawing and deciding change things: not the auditor's.
    let (status, body) = call_as(
        &app,
        Some("ann-key"),
        "DELETE",
        "/approvals/standing/nope",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an auditor withdraws nothing: {body}"
    );
    let (status, body) = call_as(
        &app,
        Some("ann-key"),
        "POST",
        "/approvals/nope/decide",
        Some(json!({"decision": "approve", "standing_hours": 24})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an auditor decides nothing: {body}"
    );
    for key in ["bob-key", "olga-key"] {
        let (status, body) =
            call_as(&app, Some(key), "DELETE", "/approvals/standing/nope", None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{key} is admitted to withdraw, and told there is none: {body}"
        );
        let (status, body) = call_as(
            &app,
            Some(key),
            "POST",
            "/approvals/nope/decide",
            Some(json!({"decision": "approve", "standing_hours": 24})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{key} is admitted to decide, and told nothing waits: {body}"
        );
    }
    // A world is made by a builder (datasets:write); a webhook by a builder
    // (triggers:write); an operator and an auditor are refused before the
    // body is read.
    let world = json!({"name": "twin", "connector": "nope", "stands_for": "nope.example"});
    for key in ["olga-key", "ann-key"] {
        let (status, body) = call_as(&app, Some(key), "POST", "/worlds", Some(world.clone())).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{key} makes no world: {body}"
        );
        let (status, body) = call_as(&app, Some(key), "POST", "/triggers", Some(json!({"name": "hook", "target": {"kind": "assistant", "id": "x"}, "action": "start_run", "world": "twin"}))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{key} makes no webhook: {body}"
        );
    }
    let (status, body) = call_as(&app, Some("bob-key"), "POST", "/worlds", Some(world)).await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "a builder is admitted to make a world: {body}"
    );
    let (status, body) = call_as(&app, Some("bob-key"), "POST", "/triggers", Some(json!({"name": "hook", "target": {"kind": "assistant", "id": "x"}, "action": "start_run", "world": "no-such-world"}))).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a builder is admitted, and told the world is unknown: {body}"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn open_mode_is_a_named_developer_not_nobody() {
    let (app, store) = app();
    let (status, me) = call(&app, "GET", "/me", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"]["id"], "dev");
    assert_eq!(me["roles"], json!(["admin"]));
    let _ = std::fs::remove_dir_all(store);
}
