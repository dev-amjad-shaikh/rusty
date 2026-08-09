//! Hardening integration tests: cross-tenant id validation, thread-record
//! durability across a router rebuild, rollback guards, the SSE attach
//! endpoint (`GET /runs/{id}/stream`) with `Last-Event-ID` replay, the cron
//! interval clamp, reserved-name rejection, and the unknown-`before` 400.
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets).

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

const ACME: (&str, &str) = ("x-api-key", "acme-secret");
const GLOBEX: (&str, &str) = ("x-api-key", "globex-secret");

// --------------------------------------------------------------------- //
// Test graphs
// --------------------------------------------------------------------- //

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

/// A slow single node, so a background run has an observable active phase.
fn slow_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("slow", |_ctx: NodeContext| async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("slow");
    (builder.compile().unwrap(), spec)
}

// --------------------------------------------------------------------- //
// App + request helpers
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-hardening-test-{}",
        uuid::Uuid::new_v4()
    ))
}

fn registry() -> GraphRegistry {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let (slow, slow_spec) = slow_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("slow", slow, slow_spec);
    registry
}

/// Build the open (dev-mode) router over `store`. Parameterized by store
/// path so tests can rebuild the router on the SAME path — the restart
/// stand-in for the durability test.
fn app(store: &std::path::Path) -> Router {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf());
    router(registry(), config)
}

fn test_app() -> (Router, PathBuf) {
    let store = temp_store();
    (app(&store), store)
}

/// A two-tenant app (acme / globex), for the cross-tenant id test.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(registry(), config), store)
}

/// Send a request and return `(status, parsed-json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, bytes) = call_raw(app, method, uri, body, &[]).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Send a request with explicit auth headers; returns `(status, json)`.
async fn call_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let headers: Vec<(&str, &str)> = auth.into_iter().collect();
    let (status, bytes) = call_raw(app, method, uri, body, &headers).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Send a request and return `(status, raw-body-bytes)`.
async fn call_raw(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in extra_headers {
        builder = builder.header(*k, *v);
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
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, bytes)
}

/// Create a thread on `graph`; returns its thread id.
async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Run a thread to completion; returns the terminal JSON (carries `run_id`).
async fn run_wait(app: &Router, thread: &str) -> Value {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
    assert_eq!(v["status"], json!("success"), "run did not succeed: {v}");
    v
}

/// Poll `GET /runs/{run_id}` until terminal; returns the terminal body.
async fn wait_terminal(app: &Router, run_id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (status, v) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        if matches!(
            v["status"].as_str(),
            Some("success" | "interrupted" | "error")
        ) {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run `{run_id}` never reached a terminal state"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Parse `event:`/`data:`/`id:` SSE blocks from a raw stream body.
fn parse_sse(body: &str) -> Vec<(String, Value, String)> {
    let mut frames = Vec::new();
    for block in body.split("\n\n") {
        let mut event = String::new();
        let mut data = String::new();
        let mut id = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                data = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("id:") {
                id = v.trim().to_string();
            }
        }
        if !event.is_empty() {
            frames.push((event, serde_json::from_str(&data).unwrap(), id));
        }
    }
    frames
}

/// The per-run sequence number of a frame id (`{checkpoint}:{step}:{seq}`).
fn frame_seq(id: &str) -> u64 {
    id.rsplit(':').next().unwrap().parse().unwrap()
}

// --------------------------------------------------------------------- //
// (a) Cross-tenant assistant_id in the run body
// --------------------------------------------------------------------- //

#[tokio::test]
async fn slashed_or_cross_tenant_assistant_id_is_rejected() {
    let (app, store) = multi_tenant_app();

    // acme creates an assistant.
    let (status, v) = call_as(
        &app,
        Some(ACME),
        "POST",
        "/assistants",
        Some(json!({"name": "acme-bot", "graph": "pipeline", "assistant_id": "acme-bot"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "assistant create failed: {v}");

    // globex creates a thread.
    let (status, v) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();

    // The internal (tenant-scoped) form `acme/acme-bot` smuggled into a run
    // body would resolve ACME's record if passed through unchecked —
    // `validate_client_id` rejects the `/` up front with a 400.
    let (status, v) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "acme/acme-bot"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "slashed assistant_id must be 400: {v}"
    );
    assert_eq!(v["error"], json!("bad_request"));

    // The plain external id from another tenant simply does not resolve in
    // globex's namespace: 404, never a cross-tenant hit.
    let (status, _) = call_as(
        &app,
        Some(GLOBEX),
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "acme-bot"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// (b) Thread durability across a router rebuild (restart stand-in)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn thread_and_checkpoints_survive_router_rebuild() {
    let store = temp_store();

    // First "process": create a thread and run it to completion.
    let app1 = app(&store);
    let thread = create_thread(&app1, "pipeline").await;
    let terminal = run_wait(&app1, &thread).await;
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));
    drop(app1);

    // Second "process": a fresh router over the SAME store path reloads the
    // thread records, so pre-restart checkpoints stay reachable through the
    // API (this is what thread durability buys: no orphaned checkpoints).
    let app2 = app(&store);
    let (status, v) = call(&app2, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "pre-restart thread 404d after rebuild: {v}"
    );
    assert_eq!(v["values"]["log"], json!(["first", "second"]));
    assert!(v["checkpoint"]["checkpoint_id"].as_str().is_some());

    let (status, v) = call(
        &app2,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 2);

    // …and the thread keeps working: a post-restart run executes normally
    // (a fresh run without resume/checkpoint_id starts from empty state, so
    // the output is `["first", "second"]` again) and its checkpoints append
    // to the pre-restart history.
    let terminal = run_wait(&app2, &thread).await;
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));
    let (_, v) = call(
        &app2,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 4);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// (c) Rollback guards
// --------------------------------------------------------------------- //

#[tokio::test]
async fn rollback_suffix_violation_is_409() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline").await;

    // Two completed runs: run1's checkpoints are no longer the history tail.
    let run1 = run_wait(&app, &thread).await;
    let run1_id = run1["run_id"].as_str().unwrap().to_string();
    let _run2 = run_wait(&app, &thread).await;

    let (status, v) = call(
        &app,
        "DELETE",
        &format!("/threads/{thread}/runs/{run1_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "mid-history rollback must be 409: {v}"
    );
    assert_eq!(v["error"], json!("conflict"));

    // History is untouched.
    let (_, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 4);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn rollback_active_run_or_busy_thread_is_409() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "slow").await;

    // run1 completes, leaving checkpoints eligible for rollback.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let run1_id = v["run_id"].as_str().unwrap().to_string();
    let terminal = wait_terminal(&app, &run1_id).await;
    assert_eq!(terminal["status"], json!("success"));

    // run2 occupies the thread slot (slow graph: ~400 ms active phase).
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let run2_id = v["run_id"].as_str().unwrap().to_string();

    // Rolling back the ACTIVE run is refused.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/threads/{thread}/runs/{run2_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "active-run rollback must be 409"
    );

    // Rolling back the FINISHED run while the thread is busy is refused too:
    // a queued or newly-started run could be executing from those very
    // checkpoints.
    let (status, v) = call(
        &app,
        "DELETE",
        &format!("/threads/{thread}/runs/{run1_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "busy-thread rollback must be 409: {v}"
    );

    // Drain run2 so the thread is idle again.
    let terminal = wait_terminal(&app, &run2_id).await;
    assert_eq!(terminal["status"], json!("success"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn rollback_file_backend_success_path() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline").await;
    let terminal = run_wait(&app, &thread).await;
    let run_id = terminal["run_id"].as_str().unwrap().to_string();

    // Happy path on the JSON-file backend: the run's two checkpoints (one
    // per super-step) are deleted and the thread re-anchors to the pre-run
    // state. (On the Postgres backend this endpoint answers 409 instead of
    // silently deleting nothing — covered by the live-PG suite when
    // DATABASE_URL is set; no live Postgres here by design.)
    let (status, v) = call(
        &app,
        "DELETE",
        &format!("/threads/{thread}/runs/{run_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");
    assert_eq!(v["deleted_checkpoints"], json!(2));
    assert_eq!(v["remaining_checkpoints"], json!(0));

    // State is back to the pre-run baseline.
    let (status, v) = call(&app, "GET", &format!("/threads/{thread}/state"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["values"], json!({}));
    assert_eq!(v["checkpoint"], Value::Null);

    // The thread stays usable: a fresh run starts a new history.
    let terminal = run_wait(&app, &thread).await;
    assert_eq!(terminal["output"]["log"], json!(["first", "second"]));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// (d) GET /runs/{id}/stream: attach + Last-Event-ID replay
// --------------------------------------------------------------------- //

#[tokio::test]
async fn attach_stream_replays_log_then_follows_live() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "slow").await;

    // Background run on the slow graph: attach while it is still active.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "background run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // Attach: the body completes once the run's `end` frame closes the
    // stream, carrying replayed + live frames in sequence order.
    let (status, bytes) = call_raw(&app, "GET", &format!("/runs/{run_id}/stream"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let frames = parse_sse(&body);
    let events: Vec<&str> = frames.iter().map(|(e, _, _)| e.as_str()).collect();
    assert_eq!(
        events,
        ["metadata", "updates", "values", "end"],
        "unexpected attach frame sequence; raw body:\n{body}"
    );
    assert_eq!(frames[0].1["run_id"], json!(run_id));
    // `updates` carries the post-reducer value (Overwrite channel → `true`).
    assert_eq!(frames[1].1["updates"]["done"], json!(true));
    assert_eq!(frames[3].1["status"], json!("success"));
    // Sequence numbers are strictly increasing, 1-based.
    let seqs: Vec<u64> = frames.iter().map(|(_, _, id)| frame_seq(id)).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);

    // Re-attach after the run finished WITHOUT a cursor: the whole event
    // log replays.
    let (status, bytes) = call_raw(&app, "GET", &format!("/runs/{run_id}/stream"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let replay = parse_sse(std::str::from_utf8(&bytes).unwrap());
    assert_eq!(replay.len(), frames.len());
    assert_eq!(replay[0].0, "metadata");

    // Re-attach WITH Last-Event-ID set to the metadata frame's id: the
    // already-seen frame is skipped and replay resumes after it.
    let last_seen = frames[0].2.clone();
    let (status, bytes) = call_raw(
        &app,
        "GET",
        &format!("/runs/{run_id}/stream"),
        None,
        &[("last-event-id", &last_seen)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let resumed = parse_sse(std::str::from_utf8(&bytes).unwrap());
    let events: Vec<&str> = resumed.iter().map(|(e, _, _)| e.as_str()).collect();
    assert_eq!(
        events,
        ["updates", "values", "end"],
        "Last-Event-ID must skip already-seen frames"
    );
    for (_, _, id) in &resumed {
        assert!(frame_seq(id) > frame_seq(&last_seen));
    }

    // Unknown runs 404 on the attach endpoint too.
    let (status, _) = call(&app, "GET", "/runs/no-such-run/stream", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// (e) Cron interval clamp
// --------------------------------------------------------------------- //

#[tokio::test]
async fn cron_interval_above_one_year_is_400() {
    let (app, store) = test_app();

    // Way past the one-year ceiling (and past what timestamp math survives):
    // rejected at validation, never reaching the scheduler.
    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "interval_secs": 99999999999u64})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "huge interval must be 400: {v}"
    );
    assert_eq!(v["error"], json!("bad_request"));

    // The ceiling itself (one year) is accepted — then deleted so the
    // scheduler does not hold it.
    let (status, v) = call(
        &app,
        "POST",
        "/crons",
        Some(json!({"graph": "pipeline", "interval_secs": 31_536_000u64})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "one-year interval rejected: {v}"
    );
    let cron_id = v["cron_id"].as_str().unwrap();
    let (status, _) = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// (f) Reserved layout names rejected as client-chosen ids
// --------------------------------------------------------------------- //

#[tokio::test]
async fn reserved_id_names_are_400() {
    let (app, store) = test_app();

    // These names own directories at the store root (`threads/`,
    // `assistants/`, `crons/`, `store/`) or inside each checkpoint dir
    // (`latest`); claiming one as a resource id would write into platform
    // directories. The guard is uniform across id kinds.
    for id in ["threads", "latest", "assistants", "crons", "store"] {
        let (status, v) = call(
            &app,
            "POST",
            "/threads",
            Some(json!({"graph": "pipeline", "thread_id": id})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "reserved thread_id `{id}` accepted: {v}"
        );

        let (status, v) = call(
            &app,
            "POST",
            "/assistants",
            Some(json!({"name": "x", "graph": "pipeline", "assistant_id": id})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "reserved assistant_id `{id}` accepted: {v}"
        );
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// (g) Unknown history `before` cursor
// --------------------------------------------------------------------- //

#[tokio::test]
async fn unknown_before_cursor_is_400() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline").await;
    let _ = run_wait(&app, &thread).await;

    // A cursor that silently reset to the full history would send
    // paginating clients into infinite loops — the endpoint answers 400.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({"before": "no-such-checkpoint"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown `before` must be 400: {v}"
    );
    assert_eq!(v["error"], json!("bad_request"));

    // Sanity: a real checkpoint id still paginates.
    let (_, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
    )
    .await;
    let newest = v[0]["checkpoint"]["checkpoint_id"].as_str().unwrap();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({"before": newest})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}
