//! Immutable assistant configuration history and guarded serving-pointer tests.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

fn graph(channel: &'static str) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel(channel, Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("work", move |_ctx: NodeContext| async move {
        Ok(NodeOutput::update(channel, json!(true)))
    });
    builder.set_entry_point("work");
    (builder.compile().unwrap(), spec)
}

fn registry() -> GraphRegistry {
    let mut registry = GraphRegistry::new();
    let (pipeline, pipeline_spec) = graph("pipeline_done");
    let (canary, canary_spec) = graph("canary_done");
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("canary", canary, canary_spec);
    registry
}

fn pipeline_registry() -> GraphRegistry {
    let mut registry = GraphRegistry::new();
    let (pipeline, pipeline_spec) = graph("pipeline_done");
    registry.register("pipeline", pipeline, pipeline_spec);
    registry
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-assistant-version-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn app(store: PathBuf) -> Router {
    router(
        registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store),
    )
}

fn pipeline_only_app(store: PathBuf) -> Router {
    router(
        pipeline_registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store),
    )
}

fn multi_tenant_app(store: PathBuf) -> Router {
    router(
        registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store)
            .with_tenant_key("acme", "acme-secret")
            .with_tenant_key("globex", "globex-secret"),
    )
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

async fn call_as(
    app: &Router,
    api_key: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(api_key) = api_key {
        builder = builder.header("x-api-key", api_key);
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

async fn create_assistant(app: &Router, id: &str) -> Value {
    let (status, value) = call(
        app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Evidence scout",
            "graph": "pipeline",
            "config": {"recursion_limit": 12, "model": "stable"},
            "metadata": {"owner": "quality"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {value}");
    value
}

fn version_payload(base: &str, name: &str, graph: &str, model: &str) -> Value {
    json!({
        "base_version_id": base,
        "name": name,
        "graph": graph,
        "config": {"recursion_limit": 12, "model": model},
        "metadata": {"owner": "quality"},
    })
}

#[tokio::test]
async fn immutable_versions_stage_activate_and_roll_back_without_rewriting_history() {
    let store = temp_store();
    let server = app(store.clone());
    let created = create_assistant(&server, "scout").await;
    let v1 = created["active_version_id"].as_str().unwrap().to_string();
    assert!(v1.starts_with("av-"));
    assert_eq!(v1.len(), 67);
    assert_eq!(created["version_count"], json!(1));

    let (status, history) = call(&server, "GET", "/assistants/scout/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["active_version_id"], json!(v1));
    assert_eq!(history["versions"].as_array().unwrap().len(), 1);
    assert_eq!(history["versions"][0]["active"], json!(true));

    let draft = version_payload(&v1, "Evidence scout canary", "canary", "candidate");
    let (status, staged) = call(
        &server,
        "POST",
        "/assistants/scout/versions",
        Some(draft.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "stage failed: {staged}");
    assert_eq!(staged["created"], json!(true));
    assert_eq!(staged["version"]["active"], json!(false));
    assert_eq!(staged["active_version_id"], json!(v1));
    assert_eq!(staged["version"]["parent_version_id"], json!(v1));
    let v2 = staged["version"]["version_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Content + parent addressing makes an exact retry idempotent.
    let (status, retried) = call(&server, "POST", "/assistants/scout/versions", Some(draft)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retried["created"], json!(false));
    assert_eq!(retried["version"]["version_id"], json!(v2));

    // Staging never changes what runs.
    let (status, active) = call(&server, "GET", "/assistants/scout", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(active["active_version_id"], json!(v1));
    assert_eq!(active["version_count"], json!(2));
    assert_eq!(active["name"], json!("Evidence scout"));
    assert_eq!(active["graph"], json!("pipeline"));
    assert_eq!(active["config"]["model"], json!("stable"));

    let (_, thread) = call(
        &server,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    let pipeline_thread = thread["thread_id"].as_str().unwrap();
    let (status, staged_run) = call(
        &server,
        "POST",
        &format!("/threads/{pipeline_thread}/runs/wait"),
        Some(json!({"assistant_id": "scout"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "staged run failed: {staged_run}");
    assert_eq!(staged_run["output"]["pipeline_done"], json!(true));

    let (status, _) = call(
        &server,
        "POST",
        &format!("/assistants/scout/versions/{v2}/activate"),
        Some(json!({"expected_active_version_id": v1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, active) = call(&server, "GET", "/assistants/scout", None).await;
    assert_eq!(active["active_version_id"], json!(v2));
    assert_eq!(active["name"], json!("Evidence scout canary"));
    assert_eq!(active["graph"], json!("canary"));
    assert_eq!(active["config"]["model"], json!("candidate"));

    let (_, thread) = call(
        &server,
        "POST",
        "/threads",
        Some(json!({"graph": "canary"})),
    )
    .await;
    let canary_thread = thread["thread_id"].as_str().unwrap();
    let (status, activated_run) = call(
        &server,
        "POST",
        &format!("/threads/{canary_thread}/runs/wait"),
        Some(json!({"assistant_id": "scout"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "activated run failed: {activated_run}"
    );
    assert_eq!(activated_run["output"]["canary_done"], json!(true));

    // Rollback is a guarded pointer move to the untouched first snapshot.
    let (status, rolled_back) = call(
        &server,
        "POST",
        &format!("/assistants/scout/versions/{v1}/activate"),
        Some(json!({"expected_active_version_id": v2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {rolled_back}");
    assert_eq!(rolled_back["activated"], json!(true));
    assert_eq!(rolled_back["assistant"]["active_version_id"], json!(v1));
    assert_eq!(rolled_back["assistant"]["graph"], json!("pipeline"));

    // The embedded history survives a fresh router/store instance.
    drop(server);
    let restarted = app(store.clone());
    let (status, history) = call(&restarted, "GET", "/assistants/scout/versions", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["versions"].as_array().unwrap().len(), 2);
    assert!(history["versions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|version| {
            version["version_id"] == json!(v2)
                && version["graph"] == json!("canary")
                && version["active"] == json!(false)
        }));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn stale_creation_and_activation_are_conflicts_and_concurrent_activation_has_one_winner() {
    let store = temp_store();
    let server = app(store.clone());
    let created = create_assistant(&server, "scout").await;
    let v1 = created["active_version_id"].as_str().unwrap().to_string();

    let (_, first) = call(
        &server,
        "POST",
        "/assistants/scout/versions",
        Some(version_payload(&v1, "First", "canary", "one")),
    )
    .await;
    let v2 = first["version"]["version_id"].as_str().unwrap().to_string();
    let (_, second) = call(
        &server,
        "POST",
        "/assistants/scout/versions",
        Some(version_payload(&v1, "Second", "pipeline", "two")),
    )
    .await;
    let v3 = second["version"]["version_id"]
        .as_str()
        .unwrap()
        .to_string();

    let left_uri = format!("/assistants/scout/versions/{v2}/activate");
    let right_uri = format!("/assistants/scout/versions/{v3}/activate");
    let left = call(
        &server,
        "POST",
        &left_uri,
        Some(json!({"expected_active_version_id": v1})),
    );
    let right = call(
        &server,
        "POST",
        &right_uri,
        Some(json!({"expected_active_version_id": v1})),
    );
    let (left, right) = tokio::join!(left, right);
    let statuses = [left.0, right.0];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1
    );
    let (_, current) = call(&server, "GET", "/assistants/scout", None).await;
    let active = current["active_version_id"].as_str().unwrap();
    assert!(active == v2 || active == v3);

    let (status, stale) = call(
        &server,
        "POST",
        "/assistants/scout/versions",
        Some(version_payload(&v1, "Stale", "pipeline", "stale")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(stale["message"].as_str().unwrap().contains(active));

    let (status, _) = call(
        &server,
        "POST",
        "/assistants/scout/versions",
        Some(version_payload(active, "Bad graph", "missing", "x")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &server,
        "GET",
        "/assistants/scout/versions/not-a-version",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn legacy_assistants_receive_one_deterministic_version_before_the_first_edit() {
    let store = temp_store();
    let assistants = store.join("assistants");
    std::fs::create_dir_all(&assistants).unwrap();
    std::fs::write(
        assistants.join("legacy.json"),
        serde_json::to_vec_pretty(&json!({
            "assistant_id": "legacy",
            "name": "Legacy",
            "graph": "pipeline",
            "config": {"model": "old"},
            "metadata": null,
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let server = app(store.clone());

    let (_, first) = call(&server, "GET", "/assistants/legacy", None).await;
    let v1 = first["active_version_id"].as_str().unwrap().to_string();
    assert_eq!(first["version_count"], json!(1));
    let (_, second) = call(&server, "GET", "/assistants/legacy", None).await;
    assert_eq!(second["active_version_id"], json!(v1));

    let (status, staged) = call(
        &server,
        "POST",
        "/assistants/legacy/versions",
        Some(version_payload(&v1, "Legacy revised", "pipeline", "new")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "legacy stage failed: {staged}");
    let raw: Value =
        serde_json::from_slice(&std::fs::read(assistants.join("legacy.json")).unwrap()).unwrap();
    assert_eq!(raw["versions"].as_array().unwrap().len(), 2);
    assert_eq!(raw["active_version_id"], json!(v1));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn oversized_pre_version_assistant_remains_readable_but_cannot_extend_its_lineage() {
    let store = temp_store();
    let assistants = store.join("assistants");
    std::fs::create_dir_all(&assistants).unwrap();
    std::fs::write(
        assistants.join("legacy-large.json"),
        serde_json::to_vec_pretty(&json!({
            "assistant_id": "legacy-large",
            "name": "Legacy large",
            "graph": "pipeline",
            "config": {"blob": "x".repeat(70 * 1024)},
            "metadata": null,
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let server = app(store.clone());

    let (status, visible) = call(&server, "GET", "/assistants/legacy-large", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(visible["version_count"], json!(1));
    let base = visible["active_version_id"].as_str().unwrap();
    let (status, body) = call(
        &server,
        "POST",
        "/assistants/legacy-large/versions",
        Some(version_payload(base, "Legacy next", "pipeline", "next")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("lineage boundary"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn version_and_lineage_byte_boundaries_reject_storage_amplification() {
    let store = temp_store();
    let server = app(store.clone());
    let oversized = "x".repeat(70 * 1024);
    let (status, _) = call(
        &server,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "oversized-initial",
            "name": "Oversized",
            "graph": "pipeline",
            "config": {"blob": oversized},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let created = create_assistant(&server, "bounded").await;
    let base = created["active_version_id"].as_str().unwrap().to_string();
    let (status, _) = call(
        &server,
        "POST",
        "/assistants/bounded/versions",
        Some(json!({
            "base_version_id": base.clone(),
            "name": "Too large",
            "graph": "pipeline",
            "config": {"blob": "x".repeat(70 * 1024)},
            "metadata": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let chunk = "y".repeat(60 * 1024);
    let mut accepted = 0usize;
    for index in 0..32 {
        let (status, body) = call(
            &server,
            "POST",
            "/assistants/bounded/versions",
            Some(json!({
                "base_version_id": base.clone(),
                "name": format!("Bounded {index}"),
                "graph": "pipeline",
                "config": {"blob": chunk.clone(), "index": index},
                "metadata": null,
            })),
        )
        .await;
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            assert!(body["message"]
                .as_str()
                .unwrap()
                .contains("lineage boundary"));
            break;
        }
        assert_eq!(status, StatusCode::CREATED, "bounded append failed: {body}");
        accepted += 1;
    }
    assert!(
        accepted > 1 && accepted < 32,
        "lineage byte ceiling was not reached"
    );
    let (_, history) = call(&server, "GET", "/assistants/bounded/versions", None).await;
    assert_eq!(history["versions"].as_array().unwrap().len(), accepted + 1);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn tampered_content_address_is_quarantined_on_restart() {
    let store = temp_store();
    let server = app(store.clone());
    create_assistant(&server, "tampered").await;
    drop(server);

    let path = store.join("assistants").join("tampered.json");
    let mut persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    persisted["versions"][0]["config"]["model"] = json!("modified-without-new-address");
    std::fs::write(&path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();

    let restarted = app(store.clone());
    let (status, _) = call(&restarted, "GET", "/assistants/tampered", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, catalog) = call(&restarted, "GET", "/assistants", None).await;
    assert!(catalog.as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn historical_version_with_removed_graph_cannot_be_activated() {
    let store = temp_store();
    let server = app(store.clone());
    let created = create_assistant(&server, "graph-drift").await;
    let v1 = created["active_version_id"].as_str().unwrap().to_string();
    let (_, staged) = call(
        &server,
        "POST",
        "/assistants/graph-drift/versions",
        Some(version_payload(&v1, "Canary", "canary", "next")),
    )
    .await;
    let v2 = staged["version"]["version_id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(server);

    let restarted = pipeline_only_app(store.clone());
    let (status, body) = call(
        &restarted,
        "POST",
        &format!("/assistants/graph-drift/versions/{v2}/activate"),
        Some(json!({"expected_active_version_id": v1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["message"].as_str().unwrap().contains("not registered"));
    let (_, current) = call(&restarted, "GET", "/assistants/graph-drift", None).await;
    assert_eq!(current["active_version_id"], json!(v1));
    assert_eq!(current["graph"], json!("pipeline"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn version_histories_are_tenant_isolated_even_for_the_same_external_id() {
    let store = temp_store();
    let server = multi_tenant_app(store.clone());
    let create = |name: &str| {
        json!({
            "assistant_id": "shared",
            "name": name,
            "graph": "pipeline",
            "config": {"model": name},
            "metadata": null,
        })
    };
    let (status, acme) = call_as(
        &server,
        Some("acme-secret"),
        "POST",
        "/assistants",
        Some(create("Acme")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let acme_v1 = acme["active_version_id"].as_str().unwrap();
    let (status, _) = call_as(
        &server,
        Some("globex-secret"),
        "POST",
        "/assistants",
        Some(create("Globex")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, staged) = call_as(
        &server,
        Some("acme-secret"),
        "POST",
        "/assistants/shared/versions",
        Some(version_payload(acme_v1, "Acme next", "canary", "next")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let acme_v2 = staged["version"]["version_id"].as_str().unwrap();

    let (_, acme_history) = call_as(
        &server,
        Some("acme-secret"),
        "GET",
        "/assistants/shared/versions",
        None,
    )
    .await;
    let (_, globex_history) = call_as(
        &server,
        Some("globex-secret"),
        "GET",
        "/assistants/shared/versions",
        None,
    )
    .await;
    assert_eq!(acme_history["versions"].as_array().unwrap().len(), 2);
    assert_eq!(globex_history["versions"].as_array().unwrap().len(), 1);
    let (status, _) = call_as(
        &server,
        Some("globex-secret"),
        "GET",
        &format!("/assistants/shared/versions/{acme_v2}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn version_count_boundary_is_enforced_without_moving_the_active_pointer() {
    let store = temp_store();
    let server = app(store.clone());
    let created = create_assistant(&server, "count-bounded").await;
    let base = created["active_version_id"].as_str().unwrap().to_string();
    for index in 1..256 {
        let (status, body) = call(
            &server,
            "POST",
            "/assistants/count-bounded/versions",
            Some(version_payload(
                &base,
                &format!("Version {index}"),
                "pipeline",
                &format!("model-{index}"),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "append {index} failed: {body}");
    }
    let (status, body) = call(
        &server,
        "POST",
        "/assistants/count-bounded/versions",
        Some(version_payload(
            &base,
            "Version 257",
            "pipeline",
            "overflow",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("256-version limit"));
    let (_, history) = call(&server, "GET", "/assistants/count-bounded/versions", None).await;
    assert_eq!(history["active_version_id"], json!(base));
    assert_eq!(history["versions"].as_array().unwrap().len(), 256);

    let _ = std::fs::remove_dir_all(store);
}
