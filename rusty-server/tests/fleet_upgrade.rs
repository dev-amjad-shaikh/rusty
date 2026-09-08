//! Fleet upgrades at safe boundaries (EP-08-S08): the first
//! assistant-bound run pins the thread to the immutable version it
//! admitted under, a global activate moves only new sessions, an upgrade
//! operation rolls pinned sessions forward with adoption exclusively at
//! the next turn boundary, initiation is idempotent on its key, a target
//! that stops resolving fails the session without breaking it, and
//! tenants never see each other's operations.
//!
//! Driven in-process via `tower::ServiceExt::oneshot`, the
//! `assistant_lifecycle.rs` convention.

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
    std::env::temp_dir().join(format!("rusty-fleet-upgrade-test-{}", uuid::Uuid::new_v4()))
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

async fn call_as(
    app: &Router,
    key: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        builder = builder.header("x-api-key", key);
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

/// Create the assistant; returns its active (root) version id.
async fn create_assistant(app: &Router, key: Option<&str>, id: &str, config: Value) -> String {
    let (status, value) = call_as(
        app,
        key,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Fleet scout",
            "graph": "pipeline",
            "config": config,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {value}");
    value["active_version_id"].as_str().unwrap().to_string()
}

/// Publish a child version; returns its version id.
async fn create_version(
    app: &Router,
    key: Option<&str>,
    id: &str,
    base: &str,
    config: Value,
) -> String {
    let (status, value) = call_as(
        app,
        key,
        "POST",
        &format!("/assistants/{id}/versions"),
        Some(json!({
            "base_version_id": base,
            "name": "Fleet scout",
            "graph": "pipeline",
            "config": config,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "version failed: {value}");
    value["version"]["version_id"].as_str().unwrap().to_string()
}

/// Create a thread and run it once bound to the assistant; returns the
/// thread id. The first assistant-bound run is where the pin lands.
async fn run_with_assistant(app: &Router, key: Option<&str>, assistant: &str) -> String {
    let (status, thread) = call_as(
        app,
        key,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_string();
    run_turn(app, key, &thread_id, assistant, StatusCode::OK).await;
    thread_id
}

/// Run one turn on the thread, asserting the expected status.
async fn run_turn(
    app: &Router,
    key: Option<&str>,
    thread_id: &str,
    assistant: &str,
    expected: StatusCode,
) -> Value {
    let (status, run) = call_as(
        app,
        key,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"assistant_id": assistant, "input": {}})),
    )
    .await;
    assert_eq!(status, expected, "run failed: {run}");
    run
}

/// Open an upgrade operation, asserting `expected`.
async fn initiate(
    app: &Router,
    key: Option<&str>,
    assistant: &str,
    target: &str,
    idem: &str,
    expected: StatusCode,
) -> Value {
    let (status, value) = call_as(
        app,
        key,
        "POST",
        &format!("/assistants/{assistant}/upgrades"),
        Some(json!({"target_version_id": target, "idempotency_key": idem})),
    )
    .await;
    assert_eq!(status, expected, "initiate failed: {value}");
    value
}

async fn get_operation(app: &Router, assistant: &str, operation_id: &str) -> Value {
    let (status, value) = call(
        app,
        "GET",
        &format!("/assistants/{assistant}/upgrades/{operation_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get failed: {value}");
    value
}

#[tokio::test]
async fn first_run_pins_and_the_upgrade_adopts_at_the_next_turn() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, None, "bot", json!({"recursion_limit": 8})).await;
    let thread = run_with_assistant(&server, None, "bot").await;
    let v2 = create_version(&server, None, "bot", &v1, json!({"recursion_limit": 9})).await;

    let operation = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::CREATED).await;
    let operation_id = operation["operation_id"].as_str().unwrap().to_string();
    assert!(operation_id.starts_with("up-"));
    assert_eq!(operation["target_version_id"], v2);
    assert_eq!(operation["open"], true);
    let sessions = operation["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["thread_id"], thread);
    assert_eq!(sessions[0]["from_version_id"], v1);
    assert_eq!(sessions[0]["status"]["state"], "awaiting_turn");

    // Initiation never re-pins: only a turn boundary adopts.
    let listed = get_operation(&server, "bot", &operation_id).await;
    assert_eq!(listed["sessions"][0]["status"]["state"], "awaiting_turn");

    run_turn(&server, None, &thread, "bot", StatusCode::OK).await;
    let adopted = get_operation(&server, "bot", &operation_id).await;
    assert_eq!(adopted["sessions"][0]["status"]["state"], "adopted");
    assert!(adopted["sessions"][0]["status"]["at"].is_string());
    assert_eq!(adopted["open"], false);

    // The list surface answers the same operation, newest first.
    let (status, list) = call(&server, "GET", "/assistants/bot/upgrades", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["upgrades"].as_array().unwrap().len(), 1);
    assert_eq!(list["upgrades"][0]["operation_id"], operation_id);

    // A converged operation leaves later turns untouched.
    run_turn(&server, None, &thread, "bot", StatusCode::OK).await;
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn the_pin_holds_when_the_active_version_moves() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, None, "bot", json!({})).await;
    let thread = run_with_assistant(&server, None, "bot").await;
    let v2 = create_version(&server, None, "bot", &v1, json!({"recursion_limit": 9})).await;

    // Activate v2 globally: new sessions serve it, the pinned thread does
    // not move.
    let (status, activated) = call(
        &server,
        "POST",
        &format!("/assistants/bot/versions/{v2}/activate"),
        Some(json!({"expected_active_version_id": v1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate failed: {activated}");
    run_turn(&server, None, &thread, "bot", StatusCode::OK).await;

    // The operation's snapshot proves the pin held v1 through activation.
    let operation = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::CREATED).await;
    let sessions = operation["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["thread_id"], thread);
    assert_eq!(sessions[0]["from_version_id"], v1);

    // A session started after the activation pins v2 directly and never
    // joins an operation whose target it already governs by.
    let fresh = run_with_assistant(&server, None, "bot").await;
    let operation_id = operation["operation_id"].as_str().unwrap();
    let listed = get_operation(&server, "bot", operation_id).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_ne!(listed["sessions"][0]["thread_id"], fresh);
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn initiation_replays_on_its_key_and_conflicts_on_a_new_target() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, None, "bot", json!({})).await;
    let thread = run_with_assistant(&server, None, "bot").await;
    let v2 = create_version(&server, None, "bot", &v1, json!({"recursion_limit": 9})).await;
    let v3 = create_version(&server, None, "bot", &v1, json!({"recursion_limit": 11})).await;

    let operation = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::CREATED).await;
    let operation_id = operation["operation_id"].as_str().unwrap().to_string();

    // Byte-identical replay: the same operation, no duplicate session
    // rows, no second adoption later.
    let replay = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::OK).await;
    assert_eq!(replay["operation_id"], operation_id);
    assert_eq!(replay["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(replay["sessions"][0]["status"]["state"], "awaiting_turn");

    // The key spent on a different target is a conflict, never a silently
    // repurposed operation.
    let conflict = initiate(&server, None, "bot", &v3, "roll-1", StatusCode::CONFLICT).await;
    assert_eq!(conflict["error"], "upgrade_key_conflict");

    // Convergence, then a replay still answers the converged operation.
    run_turn(&server, None, &thread, "bot", StatusCode::OK).await;
    let converged = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::OK).await;
    assert_eq!(converged["operation_id"], operation_id);
    assert_eq!(converged["sessions"][0]["status"]["state"], "adopted");
    assert_eq!(converged["open"], false);
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn one_open_operation_per_assistant_and_input_validation() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, None, "bot", json!({})).await;
    let _thread = run_with_assistant(&server, None, "bot").await;
    let v2 = create_version(&server, None, "bot", &v1, json!({"recursion_limit": 9})).await;
    let (status, _) = call(
        &server,
        "POST",
        &format!("/assistants/bot/versions/{v2}/activate"),
        Some(json!({"expected_active_version_id": v1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v3 = create_version(&server, None, "bot", &v2, json!({"recursion_limit": 11})).await;

    let first = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::CREATED).await;
    let second = initiate(&server, None, "bot", &v3, "roll-2", StatusCode::CONFLICT).await;
    assert_eq!(second["error"], "upgrade_in_progress");
    assert!(
        second["message"]
            .as_str()
            .unwrap()
            .contains(first["operation_id"].as_str().unwrap())
    );

    // An unknown target is a 404 even while another operation is open.
    let unknown = format!("av-{}", "0".repeat(64));
    let missing = initiate(
        &server,
        None,
        "bot",
        &unknown,
        "roll-3",
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(missing["error"], "not_found");

    let (status, malformed) = call(
        &server,
        "POST",
        "/assistants/bot/upgrades",
        Some(json!({"target_version_id": "not-a-version", "idempotency_key": "roll-4"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{malformed}");
    let (status, empty_key) = call(
        &server,
        "POST",
        "/assistants/bot/upgrades",
        Some(json!({"target_version_id": v2, "idempotency_key": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{empty_key}");
    let (status, long_key) = call(
        &server,
        "POST",
        "/assistants/bot/upgrades",
        Some(json!({"target_version_id": v2, "idempotency_key": "k".repeat(257)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{long_key}");

    // The assistant itself must exist.
    let (status, ghost) = call(
        &server,
        "POST",
        "/assistants/ghost/upgrades",
        Some(json!({"target_version_id": v2, "idempotency_key": "roll-5"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{ghost}");
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn a_target_that_stops_resolving_fails_the_session_and_the_thread_stays_serviceable() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, None, "bot", json!({})).await;
    let thread = run_with_assistant(&server, None, "bot").await;
    let v2 = create_version(&server, None, "bot", &v1, json!({"recursion_limit": 9})).await;
    let operation = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::CREATED).await;
    let operation_id = operation["operation_id"].as_str().unwrap().to_string();

    // Retract v2 out from under the open operation: drop the leaf from
    // the lineage on disk (v1 stays root and active, so the lineage still
    // validates), then rebuild the router on the same store — pins and
    // operations reload, the target no longer resolves.
    let assistant_file = store.join("assistants").join("bot.json");
    let mut record: Value =
        serde_json::from_str(&std::fs::read_to_string(&assistant_file).unwrap()).unwrap();
    let versions = record["versions"].as_array_mut().unwrap();
    versions.retain(|version| version["version_id"] != v2);
    std::fs::write(&assistant_file, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    drop(server);
    let restarted = app(store.clone());

    // The boundary fails the session with the typed error; the run itself
    // completes under the prior version — the fleet never stalls.
    run_turn(&restarted, None, &thread, "bot", StatusCode::OK).await;
    let failed = get_operation(&restarted, "bot", &operation_id).await;
    assert_eq!(failed["sessions"][0]["status"]["state"], "failed");
    assert!(
        failed["sessions"][0]["status"]["error"]
            .as_str()
            .unwrap()
            .contains(&v2)
    );
    assert!(failed["sessions"][0]["status"]["at"].is_string());
    assert_eq!(failed["open"], false);

    // Still serviceable afterwards, on the same pin.
    run_turn(&restarted, None, &thread, "bot", StatusCode::OK).await;
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn the_adopted_version_config_governs_the_run() {
    let store = temp_store();
    let server = app(store.clone());
    let v1 = create_assistant(&server, None, "bot", json!({})).await;
    let thread = run_with_assistant(&server, None, "bot").await;
    // v2's reviewed tool selection names a tool the graph does not
    // register: the moment v2 governs, admission answers 400 — the
    // observable proof of which version the run admitted under.
    let v2 = create_version(
        &server,
        None,
        "bot",
        &v1,
        json!({"studio_intent": {"tools": [{"name": "no-such-tool"}]}}),
    )
    .await;

    // Pinned to v1, the run is unrestricted and succeeds.
    run_turn(&server, None, &thread, "bot", StatusCode::OK).await;
    let operation = initiate(&server, None, "bot", &v2, "roll-1", StatusCode::CREATED).await;
    let operation_id = operation["operation_id"].as_str().unwrap();

    // The adopting turn admits under v2: the session records the adoption
    // and v2's config supplies the run's defaults, which the capability
    // check then rejects.
    let run = run_turn(&server, None, &thread, "bot", StatusCode::BAD_REQUEST).await;
    assert!(run["message"].as_str().unwrap().contains("no-such-tool"));
    let adopted = get_operation(&server, "bot", operation_id).await;
    assert_eq!(adopted["sessions"][0]["status"]["state"], "adopted");
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn tenants_are_isolated() {
    let store = temp_store();
    let server = tenant_app(store.clone());
    let acme = Some("acme-secret");
    let globex = Some("globex-secret");

    let v1_acme = create_assistant(&server, acme, "bot", json!({})).await;
    let v1_globex = create_assistant(&server, globex, "bot", json!({})).await;
    let thread = run_with_assistant(&server, acme, "bot").await;
    let v2_acme = create_version(
        &server,
        acme,
        "bot",
        &v1_acme,
        json!({"recursion_limit": 9}),
    )
    .await;

    let operation = initiate(
        &server,
        acme,
        "bot",
        &v2_acme,
        "roll-1",
        StatusCode::CREATED,
    )
    .await;
    let operation_id = operation["operation_id"].as_str().unwrap().to_string();
    assert_eq!(operation["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(operation["sessions"][0]["thread_id"], thread);

    // Globex's same-named assistant is a different record: acme's target
    // does not resolve there, and its own upgrade covers zero sessions —
    // no globex session was ever pinned to this assistant.
    let missing = initiate(
        &server,
        globex,
        "bot",
        &v2_acme,
        "roll-1",
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(missing["error"], "not_found");
    let v2_globex = create_version(
        &server,
        globex,
        "bot",
        &v1_globex,
        json!({"recursion_limit": 9}),
    )
    .await;
    let globex_op = initiate(
        &server,
        globex,
        "bot",
        &v2_globex,
        "roll-1",
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(globex_op["sessions"].as_array().unwrap().len(), 0);
    assert_ne!(globex_op["operation_id"], operation_id);

    // One tenant's operation is another's 404 — never a leak.
    let (status, _) = call_as(
        &server,
        globex,
        "GET",
        &format!("/assistants/bot/upgrades/{operation_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, list) = call_as(&server, globex, "GET", "/assistants/bot/upgrades", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["upgrades"].as_array().unwrap().len(), 1);
    assert_eq!(
        list["upgrades"][0]["operation_id"],
        globex_op["operation_id"]
    );

    // Acme's session still adopts on its own boundary.
    run_turn(&server, acme, &thread, "bot", StatusCode::OK).await;
    let (status, adopted) = call_as(
        &server,
        acme,
        "GET",
        &format!("/assistants/bot/upgrades/{operation_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(adopted["sessions"][0]["status"]["state"], "adopted");
    std::fs::remove_dir_all(store).unwrap();
}
