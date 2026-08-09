//! Live-Postgres integration tests for the `postgres` feature: the
//! `PostgresStore` CRUD surface (assistants / crons / threads / KV — the
//! four `server_*` tables created by the auto-migration, `server_threads`
//! included) plus the Postgres-backed run checkpointer, exercised
//! end-to-end over HTTP.
//!
//! Gated two ways — none of this runs in the default test suite:
//!
//! 1. compile-time: the whole file is `cfg(feature = "postgres")`;
//! 2. run-time: every test is `#[ignore]` and requires `DATABASE_URL`.
//!
//! Run them with:
//!
//! ```bash
//! DATABASE_URL=postgres://user:pass@localhost/rusty_test \
//!   cargo test --features postgres --test postgres_store -- --ignored
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// `first -> second`, appending to a `log` channel.
fn pipeline_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    (builder.compile().unwrap(), spec)
}

/// The database these tests run against; panics with guidance when unset.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a scratch Postgres database \
         (e.g. postgres://user:pass@localhost/rusty_test)",
    )
}

/// An app whose checkpointer AND server store are Postgres-backed.
fn postgres_app() -> Router {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    // store_path is irrelevant to Postgres persistence; a temp dir keeps
    // the signature honest.
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-test-{}", uuid::Uuid::new_v4()));
    let config =
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path).with_postgres(database_url());
    router(registry, config)
}

/// Send a request and return `(status, parsed-json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Unique id/key fragment so repeated runs against a shared scratch
/// database never collide.
fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// --------------------------------------------------------------------- //
// Assistants (server_assistants table)
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_assistant_crud() {
    let app = postgres_app();
    let assistant_id = format!("asst-{}", uniq());

    // Create → 201.
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": assistant_id,
            "name": "pg-bot",
            "graph": "pipeline",
            "config": {"recursion_limit": 10},
            "metadata": {"backend": "postgres"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "assistant create failed: {v}");
    assert_eq!(v["assistant_id"], json!(assistant_id));

    // Duplicate id → 409 (ON CONFLICT DO NOTHING).
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": assistant_id,
            "name": "pg-bot-2",
            "graph": "pipeline",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "expected conflict: {v}");

    // Fetch → the JSONB payload round-trips intact.
    let (status, v) = call(&app, "GET", &format!("/assistants/{assistant_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["name"], json!("pg-bot"));
    assert_eq!(v["config"]["recursion_limit"], json!(10));
    assert_eq!(v["metadata"]["backend"], json!("postgres"));

    // List contains it; unknown id → 404.
    let (status, v) = call(&app, "GET", "/assistants", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["assistant_id"] == json!(assistant_id)));
    let (status, _) = call(&app, "GET", "/assistants/no-such-assistant", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --------------------------------------------------------------------- //
// Crons (server_crons table)
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_cron_crud() {
    let app = postgres_app();
    let cron_id = format!("cron-{}", uniq());

    // Create → 201 (long interval: it must not fire during the test).
    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({
            "cron_id": cron_id,
            "graph": "pipeline",
            "interval_secs": 3600,
            "input": {"seed": 1},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "cron create failed: {v}");
    assert_eq!(v["cron_id"], json!(cron_id));
    assert_eq!(v["runs_fired"], json!(0));

    // Duplicate id → 409.
    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({
            "cron_id": cron_id,
            "graph": "pipeline",
            "interval_secs": 3600,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "expected conflict: {v}");

    // List round-trips the JSONB payload.
    let (status, v) = call(&app, "GET", "/crons", None).await;
    assert_eq!(status, StatusCode::OK);
    let cron = v
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["cron_id"] == json!(cron_id))
        .expect("cron missing from list");
    assert_eq!(cron["interval_secs"], json!(3600));
    assert_eq!(cron["input"], json!({"seed": 1}));

    // Delete → 200, then gone.
    let (status, v) = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["deleted"], json!(true));
    let (status, _) = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --------------------------------------------------------------------- //
// KV store (server_kv table)
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_kv_crud() {
    let app = postgres_app();
    let ns = format!("pg-ns-{}", uniq());

    // Create → 201.
    let (status, v) = call(
        &app,
        "PUT",
        &format!("/store/{ns}/user-1"),
        Some(json!({"preference": "dark-mode"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "kv put failed: {v}");
    assert_eq!(v["namespace"], json!(ns));
    assert_eq!(v["value"]["preference"], json!("dark-mode"));
    let created_at = v["created_at"].as_str().unwrap().to_string();

    // Fetch.
    let (status, v) = call(&app, "GET", &format!("/store/{ns}/user-1"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"]["preference"], json!("dark-mode"));

    // Overwrite → 200 with created_at preserved (upsert mapping).
    let (status, v) = call(
        &app,
        "PUT",
        &format!("/store/{ns}/user-1"),
        Some(json!({"preference": "light-mode"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "kv overwrite failed: {v}");
    assert_eq!(v["value"]["preference"], json!("light-mode"));
    assert_eq!(v["created_at"], json!(created_at));

    // Second key + namespace listing, sorted by key.
    let (status, _) = call(
        &app,
        "PUT",
        &format!("/store/{ns}/user-2"),
        Some(json!([1, 2])),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(&app, "GET", &format!("/store/{ns}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["user-1", "user-2"]);

    // Delete → 200; fetch + re-delete → 404.
    let (status, v) = call(&app, "DELETE", &format!("/store/{ns}/user-1"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["deleted"], json!(true));
    let (status, _) = call(&app, "GET", &format!("/store/{ns}/user-1"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "DELETE", &format!("/store/{ns}/user-1"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --------------------------------------------------------------------- //
// Runs (rusty_checkpoints table via PostgresCheckpointer)
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_run_checkpoints_and_time_travel() {
    let app = postgres_app();

    // The info endpoint reports the Postgres backend.
    let (status, v) = call(&app, "GET", "/info", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["checkpointer"], json!("postgres"));
    assert_eq!(v["server_store"], json!("postgres"));

    // A full run against the Postgres checkpointer.
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread create failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    assert_eq!(v["output"]["log"], json!(["first", "second"]));

    // History comes back out of Postgres; fork + replay work there too.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = v.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let step0_id = items[1]["checkpoint"]["checkpoint_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({"checkpoint_id": step0_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork failed: {v}");
    assert_eq!(v["checkpoints_copied"], json!(1));
    let fork = v["thread_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{fork}/runs/wait"),
        Some(json!({"checkpoint": {"checkpoint_id": step0_id}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replay failed: {v}");
    assert_eq!(v["output"]["log"], json!(["first", "second"]));
}

// --------------------------------------------------------------------- //
// Threads (server_threads table)
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_threads_survive_router_rebuild_and_rollback_is_409() {
    let app = postgres_app();

    // Create a thread (persisted in server_threads) and run it.
    let (status, v) = call(
        &app,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline", "thread_id": format!("t-{}", uniq())})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread create failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // A fresh router over the same DATABASE_URL (restart stand-in) reloads
    // the thread record, so pre-restart checkpoints stay reachable.
    let app2 = postgres_app();
    let (status, v) = call(&app2, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "pre-restart thread 404d after rebuild: {v}"
    );
    assert_eq!(v["values"]["log"], json!(["first", "second"]));

    // Rollback answers 409 on the Postgres backend rather than silently
    // deleting nothing.
    let (status, v) = call(
        &app2,
        "DELETE",
        &format!("/threads/{thread}/runs/{run_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Postgres rollback must be 409: {v}"
    );
}

// --------------------------------------------------------------------- //
// Flight Recorder journals (server_journals table)
// --------------------------------------------------------------------- //

#[tokio::test]
#[ignore = "requires a live Postgres (DATABASE_URL)"]
async fn postgres_run_journal_is_persisted_and_served() {
    let app = postgres_app();

    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread create failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // The completed run's journal is served from server_journals.
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events fetch failed: {v}");
    assert_eq!(v["run_id"], json!(run_id));
    assert_eq!(v["complete"], json!(true));
    let events = v["events"].as_array().unwrap();
    assert!(!events.is_empty(), "journaled run must have events");
    for (seq, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], json!(seq as u64));
        assert_eq!(event["id"], json!(format!("{run_id}:{seq}")));
        assert_eq!(event["thread_id"], json!(thread));
    }
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"checkpoint_written"));
}
