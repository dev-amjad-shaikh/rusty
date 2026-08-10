//! Reversible assistant archive/restore lifecycle and serving guards.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
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
        "rusty-assistant-lifecycle-test-{}",
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

async fn create_assistant(app: &Router, key: Option<&str>, id: &str) -> Value {
    let (status, value) = call_as(
        app,
        key,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Lifecycle scout",
            "graph": "pipeline",
            "config": {"recursion_limit": 12},
            "metadata": {"owner": "quality"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {value}");
    value
}

fn lifecycle_payload(assistant: &Value) -> Value {
    json!({"expected_active_version_id": assistant["active_version_id"]})
}

#[tokio::test]
async fn archive_is_idempotent_preserves_lineage_and_blocks_only_new_runs() {
    let store = temp_store();
    let server = app(store.clone());
    let assistant = create_assistant(&server, None, "scout").await;
    let (thread_status, thread) = call(
        &server,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(thread_status, StatusCode::CREATED);

    let (archive_status, archive) = call(
        &server,
        "POST",
        "/assistants/scout/archive",
        Some(lifecycle_payload(&assistant)),
    )
    .await;
    assert_eq!(archive_status, StatusCode::OK, "archive failed: {archive}");
    assert_eq!(archive["changed"], true);
    assert_eq!(archive["lifecycle"], "archived");
    assert!(archive["assistant"]["archived_at"].is_string());
    assert_eq!(
        archive["assistant"]["active_version_id"],
        assistant["active_version_id"]
    );
    assert_eq!(archive["assistant"]["version_count"], 1);

    let (repeat_status, repeat) = call(
        &server,
        "POST",
        "/assistants/scout/archive",
        Some(lifecycle_payload(&assistant)),
    )
    .await;
    assert_eq!(repeat_status, StatusCode::OK);
    assert_eq!(repeat["changed"], false);
    assert_eq!(
        repeat["assistant"]["archived_at"],
        archive["assistant"]["archived_at"]
    );

    let run_uri = format!(
        "/threads/{}/runs/wait",
        thread["thread_id"].as_str().unwrap()
    );
    let (blocked_status, blocked) = call(
        &server,
        "POST",
        &run_uri,
        Some(json!({"assistant_id": "scout", "input": {}})),
    )
    .await;
    assert_eq!(blocked_status, StatusCode::CONFLICT);
    assert_eq!(blocked["error"], "assistant_archived");

    let (_, versions) = call(&server, "GET", "/assistants/scout/versions", None).await;
    assert_eq!(versions["versions"].as_array().unwrap().len(), 1);
    let (_, catalog) = call(&server, "GET", "/assistants", None).await;
    assert!(catalog[0]["archived_at"].is_string());
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn restore_survives_restart_and_stale_review_never_moves_lifecycle() {
    let store = temp_store();
    let server = app(store.clone());
    let assistant = create_assistant(&server, None, "scout").await;
    let payload = lifecycle_payload(&assistant);
    let (status, _) = call(
        &server,
        "POST",
        "/assistants/scout/archive",
        Some(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    drop(server);

    let restarted = app(store.clone());
    let (_, archived) = call(&restarted, "GET", "/assistants/scout", None).await;
    assert!(archived["archived_at"].is_string());
    let (stale_status, _) = call(
        &restarted,
        "POST",
        "/assistants/scout/restore",
        Some(json!({"expected_active_version_id": format!("av-{}", "0".repeat(64))})),
    )
    .await;
    assert_eq!(stale_status, StatusCode::CONFLICT);
    let (_, still_archived) = call(&restarted, "GET", "/assistants/scout", None).await;
    assert!(still_archived["archived_at"].is_string());

    let (restore_status, restored) = call(
        &restarted,
        "POST",
        "/assistants/scout/restore",
        Some(payload),
    )
    .await;
    assert_eq!(restore_status, StatusCode::OK, "restore failed: {restored}");
    assert_eq!(restored["changed"], true);
    assert_eq!(restored["lifecycle"], "active");
    assert!(restored["assistant"].get("archived_at").is_none());

    let (_, thread) = call(
        &restarted,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    let run_uri = format!(
        "/threads/{}/runs/wait",
        thread["thread_id"].as_str().unwrap()
    );
    let (run_status, run) = call(
        &restarted,
        "POST",
        &run_uri,
        Some(json!({"assistant_id": "scout", "input": {}})),
    )
    .await;
    assert_eq!(run_status, StatusCode::OK, "restored run failed: {run}");
    std::fs::remove_dir_all(store).unwrap();
}

#[tokio::test]
async fn lifecycle_is_tenant_scoped() {
    let store = temp_store();
    let server = tenant_app(store.clone());
    let assistant = create_assistant(&server, Some("acme-secret"), "scout").await;
    let (hidden_status, _) = call_as(
        &server,
        Some("globex-secret"),
        "POST",
        "/assistants/scout/archive",
        Some(lifecycle_payload(&assistant)),
    )
    .await;
    assert_eq!(hidden_status, StatusCode::NOT_FOUND);
    let (archive_status, _) = call_as(
        &server,
        Some("acme-secret"),
        "POST",
        "/assistants/scout/archive",
        Some(lifecycle_payload(&assistant)),
    )
    .await;
    assert_eq!(archive_status, StatusCode::OK);
    let (_, globex) = call_as(&server, Some("globex-secret"), "GET", "/assistants", None).await;
    assert_eq!(globex, json!([]));
    std::fs::remove_dir_all(store).unwrap();
}
