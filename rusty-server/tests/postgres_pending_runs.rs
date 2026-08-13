//! Restart survival for the durable pending-run queue against Postgres —
//! the same scenario as `durable_pending_runs.rs` (occupy a thread, park
//! two runs, crash, reboot over the same store, both runs drain in FIFO
//! order), proving the `server_pending_runs` table carries the queue
//! across a restart.
//!
//! Gated two ways — none of this runs in the default test suite:
//!
//! 1. compile-time: the whole file is `cfg(feature = "postgres")`;
//! 2. run-time: every test is `#[ignore]` and requires `DATABASE_URL`.
//!
//! Run them (single-threaded, per repo practice for live-DB tests):
//!
//! ```bash
//! DATABASE_URL=postgres://user:pass@localhost/rusty_test \
//!   cargo test --features postgres --test postgres_pending_runs -- --ignored --test-threads=1
//! ```

#![cfg(feature = "postgres")]

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// A gated graph: `wait` parks the run for a minute (the occupying run —
/// long enough to survive the "crash" and the whole test; the test
/// runtime aborts the detached task at the end). Otherwise the run takes
/// `pace` milliseconds (default 250), leaving an observable window for
/// the status-transition and FIFO assertions.
fn gated_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new()
        .channel("wait", Reducer::Overwrite)
        .channel("pace", Reducer::Overwrite)
        .channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("work", |ctx: NodeContext| async move {
        if ctx
            .state()
            .get("wait")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            tokio::time::sleep(Duration::from_secs(60)).await;
        } else {
            let pace = ctx
                .state()
                .get("pace")
                .and_then(Value::as_u64)
                .unwrap_or(250);
            tokio::time::sleep(Duration::from_millis(pace)).await;
        }
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("work");
    (builder.compile().unwrap(), spec)
}

/// The database these tests run against; panics with guidance when unset.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must point at a scratch Postgres database \
         (e.g. postgres://user:pass@localhost/rusty_test)",
    )
}

/// A server whose checkpointer AND server store are Postgres-backed.
/// `store_path` is irrelevant to Postgres persistence; a temp dir keeps
/// the signature honest (the `postgres_store` suite's convention).
fn postgres_app() -> Router {
    let (graph, spec) = gated_graph();
    let mut registry = GraphRegistry::new();
    registry.register("gated", graph, spec);
    let store_path: PathBuf =
        std::env::temp_dir().join(format!("rusty-server-pg-pending-{}", uuid::Uuid::new_v4()));
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_path)
        .with_postgres(database_url())
        // Two parked runs need queue depth 2 (the default cap is 1).
        .with_max_concurrent_runs_per_thread(4);
    router(registry, config)
}

/// Send a request in-process; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
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

/// Submit a background run; returns its run id.
async fn submit_run(app: &Router, thread: &str, input: Value) -> (String, String) {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({"input": input})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "submit run failed: {v}");
    (
        v["run_id"].as_str().unwrap().to_string(),
        v["status"].as_str().unwrap().to_string(),
    )
}

/// Poll both runs until each reaches `terminal`, returning the observed
/// status changes as an ordered event log of `(label, status)`. A 404
/// (the run is not in this process's manager yet — the boot restore is
/// still landing) is ridden out, not recorded.
async fn wait_both(app: &Router, run2: &str, run3: &str, terminal: &str) -> Vec<(String, String)> {
    let mut events: Vec<(String, String)> = Vec::new();
    let mut last2 = String::new();
    let mut last3 = String::new();
    for _ in 0..400 {
        for (label, run, last) in [("run2", run2, &mut last2), ("run3", run3, &mut last3)] {
            let (_, v) = call(app, "GET", &format!("/runs/{run}"), None).await;
            if let Some(status) = v["status"].as_str() {
                if status != last.as_str() {
                    events.push((label.to_string(), status.to_string()));
                    *last = status.to_string();
                }
            }
        }
        if last2 == terminal && last3 == terminal {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("runs never reached `{terminal}`: {events:?}");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (live Postgres)"]
async fn restart_restores_queued_runs_in_fifo_order() {
    // --- Server A: occupy the thread, park two runs behind it. --------
    let app_a = postgres_app();
    let (status, v) = call(&app_a, "POST", "/threads", Some(json!({"graph": "gated"}))).await;
    assert_eq!(status, StatusCode::CREATED, "create thread failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();

    let (_occupier, occupier_status) = submit_run(&app_a, &thread, json!({"wait": true})).await;
    assert_eq!(occupier_status, "running");
    let (run2, run2_status) = submit_run(&app_a, &thread, json!({"pace": 800})).await;
    assert_eq!(run2_status, "pending");
    let (run3, run3_status) = submit_run(&app_a, &thread, json!({"pace": 50})).await;
    assert_eq!(run3_status, "pending");
    for run in [&run2, &run3] {
        let (_, v) = call(&app_a, "GET", &format!("/runs/{run}"), None).await;
        assert_eq!(v["status"], json!("pending"), "run {run} on server A: {v}");
    }

    // --- THE CRASH: the router drops with the occupier mid-step and two
    // runs parked — no drain, no cleanup. ---
    drop(app_a);

    // --- Server B over the same database. The boot restore is async;
    // the transition polls ride out its landing. ---
    let app_b = postgres_app();
    let events = wait_both(&app_b, &run2, &run3, "success").await;

    let seq_of = |label: &str| -> Vec<&str> {
        events
            .iter()
            .filter(|(l, _)| l == label)
            .map(|(_, s)| s.as_str())
            .collect()
    };
    let seq2 = seq_of("run2");
    assert!(seq2.contains(&"running"), "run2 transitions: {seq2:?}");
    assert_eq!(seq2.last(), Some(&"success"), "run2 transitions: {seq2:?}");
    // run3 queued behind run2's 800 ms execution, so its whole
    // pending → running → success sequence is observable (run2's pending
    // window is the boot instant itself — not pollable — but server A
    // already proved its pending state above).
    assert_eq!(seq_of("run3"), ["pending", "running", "success"]);
    // FIFO: run2 finished before run3 ever started.
    let run2_done = events
        .iter()
        .position(|e| e == &("run2".to_string(), "success".to_string()));
    let run3_ran = events
        .iter()
        .position(|e| e == &("run3".to_string(), "running".to_string()));
    assert!(
        run2_done < run3_ran,
        "run3 started before run2 finished: {events:?}"
    );

    drop(app_b);
}
