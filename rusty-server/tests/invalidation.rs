//! Invalidation integration tests: the `/skills/invalidate` HTTP surface.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `memory.rs` convention. Promotion histories are pre-seeded on disk so
//! the skill plane loads them at boot.

use std::path::{Path, PathBuf};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-invalidation-test-{}", uuid::Uuid::new_v4()))
}

fn app_at(store: PathBuf) -> Router {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(GraphRegistry::new(), config)
}

async fn call_raw(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
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
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, content_type, bytes)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, _, bytes) = call_raw(app, method, uri, body).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
}

fn register_payload(name: &str, body: &str) -> Value {
    json!({
        "skill_md": skill_md(name, &format!("The {name} skill."), body),
        "author": "operator:ada",
    })
}

fn register_payload_with_deps(name: &str, body: &str, deps: &str) -> Value {
    json!({
        "skill_md": format!(
            "---\nname: {name}\ndescription: The {name} skill.\ndependencies: {deps}\n---\n\n{body}\n"
        ),
        "author": "operator:ada",
    })
}

async fn publish(app: &Router, name: &str, body: &str) -> Value {
    let (status, v) = call(app, "POST", "/skills", Some(register_payload(name, body))).await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    v
}

/// Seed a promotion history file so the skill plane loads the skill as Promoted.
fn seed_promoted(store: &Path, name: &str) {
    let dir = store.join("skill-promotions");
    std::fs::create_dir_all(&dir).unwrap();
    let history = json!({
        "promotions": [{
            "name": name,
            "revision": 1,
            "content_hash": "0".repeat(64),
            "status": "promoted",
            "gate_run_id": "run-1",
            "author": "test",
            "created_at": "2024-01-01T00:00:00Z",
        }]
    });
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&history).unwrap(),
    )
    .unwrap();
}

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[tokio::test]
async fn invalidation_demotes_promoted_dependent() {
    let store = temp_store();
    let first_app = app_at(store.clone());
    publish(&first_app, "ticket-triage", "Create a ticket.").await;
    drop(first_app);

    // Seed the skill as promoted before the invalidation app loads.
    seed_promoted(&store, "ticket-triage");

    let app = app_at(store.clone());

    // Call invalidation for a dependency the skill does not declare.
    let (status, body) = call(
        &app,
        "POST",
        "/skills/invalidate",
        Some(json!({
            "dependency_id": "tool:search",
            "old_fingerprint": "old",
            "new_fingerprint": "new",
            "change_source": "tool_reregistered",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["affected"], json!(0));
    assert_eq!(body["demoted"].as_array().unwrap().len(), 0);

    // Now register a skill that DOES declare the dependency.
    let (status, _) = call(
        &app,
        "POST",
        "/skills",
        Some(register_payload_with_deps(
            "dep-skill",
            "Use tool.",
            "tool:search",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    drop(app);

    seed_promoted(&store, "dep-skill");

    let app = app_at(store.clone());
    let (status, body) = call(
        &app,
        "POST",
        "/skills/invalidate",
        Some(json!({
            "dependency_id": "tool:search",
            "old_fingerprint": "old",
            "new_fingerprint": "new",
            "change_source": "tool_reregistered",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], json!(1));
    let demoted = body["demoted"].as_array().unwrap();
    assert_eq!(demoted.len(), 1);
    assert_eq!(demoted[0], json!("dep-skill"));

    // Verify a gap entry was filed.
    let gap_ids = body["gap_ids"].as_array().unwrap();
    assert_eq!(gap_ids.len(), 1);

    // Verify the repair record was emitted.
    let (status, repairs) = call(&app, "GET", "/repairs", None).await;
    assert_eq!(status, StatusCode::OK);
    let records = repairs.as_array().unwrap();
    assert!(!records.is_empty(), "repair record should be emitted");
    let last = records.last().unwrap();
    assert_eq!(last["component"], json!("dependency_invalidation"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn invalidation_skips_non_promoted() {
    let store = temp_store();
    let app = app_at(store.clone());

    let (status, _) = call(
        &app,
        "POST",
        "/skills",
        Some(register_payload_with_deps(
            "trial-skill",
            "Use tool.",
            "tool:search",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = call(
        &app,
        "POST",
        "/skills/invalidate",
        Some(json!({
            "dependency_id": "tool:search",
            "old_fingerprint": "old",
            "new_fingerprint": "new",
            "change_source": "tool_reregistered",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["affected"], json!(1));
    assert_eq!(body["demoted"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn invalidation_batch_multiple_dependents() {
    let store = temp_store();
    let app = app_at(store.clone());

    for name in ["skill-a", "skill-b"] {
        let (status, _) = call(
            &app,
            "POST",
            "/skills",
            Some(register_payload_with_deps(name, "Use tool.", "tool:search")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    // Unrelated skill.
    let (status, _) = call(
        &app,
        "POST",
        "/skills",
        Some(register_payload_with_deps(
            "skill-c",
            "Use other.",
            "tool:other",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    drop(app);

    seed_promoted(&store, "skill-a");
    seed_promoted(&store, "skill-b");

    let app = app_at(store.clone());
    let (status, body) = call(
        &app,
        "POST",
        "/skills/invalidate",
        Some(json!({
            "dependency_id": "tool:search",
            "old_fingerprint": "old",
            "new_fingerprint": "new",
            "change_source": "tool_reregistered",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["affected"], json!(2));
    let demoted = body["demoted"].as_array().unwrap();
    assert_eq!(demoted.len(), 2);

    let gap_ids = body["gap_ids"].as_array().unwrap();
    assert_eq!(gap_ids.len(), 2);

    let _ = std::fs::remove_dir_all(store);
}
