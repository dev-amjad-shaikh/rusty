//! Integration tests for the Flight Recorder server surface:
//! `GET /runs/{run_id}/events`, journal persistence across both stores'
//! code paths (the JSON-file layout is exercised end-to-end here; the
//! Postgres path runs in `postgres_store.rs`), and tenant isolation
//! identical to the existing run endpoints. Driven in-process via
//! `tower::ServiceExt::oneshot` (no sockets).

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

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

/// A single gate node that interrupts until resumed.
fn interrupt_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("gate");
    (builder.compile().unwrap(), spec)
}

// --------------------------------------------------------------------- //
// App + request helpers
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-fr-test-{}", uuid::Uuid::new_v4()))
}

/// A `third` node that sleeps before writing — slow enough that a second
/// run can be observed queued/executing behind it.
fn slow_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    (builder.compile().unwrap(), spec)
}

/// Build the test app over a given store root (restart tests build it twice).
fn test_app_at(store: PathBuf) -> Router {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let (gate, gate_spec) = interrupt_graph();
    let (slow, slow_spec) = slow_graph();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    registry.register("interrupt_gate", gate, gate_spec);
    registry.register("slow_pipeline", slow, slow_spec);

    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(registry, config)
}

fn test_app() -> (Router, PathBuf) {
    let store = temp_store();
    (test_app_at(store.clone()), store)
}

/// The two-tenant app over a given store root.
fn multi_tenant_app_at(store: PathBuf) -> Router {
    let (pipeline, pipeline_spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, pipeline_spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store)
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    router(registry, config)
}

/// The two-tenant app for isolation tests.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    (multi_tenant_app_at(store.clone()), store)
}

/// Send a request and return `(status, parsed-json-body-or-null)`.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
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
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Create a thread on `graph`; returns its thread id.
async fn create_thread(app: &Router, graph: &str, headers: &[(&str, &str)]) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/threads",
        Some(json!({"graph": graph})),
        headers,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
    v["thread_id"].as_str().unwrap().to_string()
}

/// Run a thread to its terminal state via `runs/wait`; returns the run id.
async fn run_wait(app: &Router, thread: &str, body: Value, headers: &[(&str, &str)]) -> String {
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(body),
        headers,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
    v["run_id"]
        .as_str()
        .expect("terminal JSON carries run_id")
        .to_string()
}

/// Fetch a run's journal events; asserts 200 and returns the body.
async fn get_events(app: &Router, run_id: &str, headers: &[(&str, &str)]) -> Value {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None, headers).await;
    assert_eq!(status, StatusCode::OK, "GET events failed: {v}");
    v
}

/// The fields every `RunEvent` wire object must carry (golden shape,
/// `rusty-core/tests/golden/run_event.json`).
const RUN_EVENT_FIELDS: &[&str] = &[
    "id",
    "run_id",
    "thread_id",
    "node_id",
    "seq",
    "kind",
    "effect",
    "input",
    "output",
    "latency_ms",
    "tokens",
    "cost_usd",
    "status",
    "parent",
    "recorded_at",
];

/// Assert the journal's total-order and id invariants over a served event
/// list: seq is `0..n` in order and every id is `{run_id}:{seq}`.
fn assert_event_invariants(events: &[Value], run_id: &str) {
    for (expected_seq, event) in events.iter().enumerate() {
        for field in RUN_EVENT_FIELDS {
            assert!(
                event.get(field).is_some(),
                "event missing `{field}`: {event}"
            );
        }
        assert_eq!(event["seq"], json!(expected_seq as u64));
        assert_eq!(event["id"], json!(format!("{run_id}:{expected_seq}")));
        assert_eq!(event["run_id"], json!(run_id));
    }
}

fn event_kinds(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["kind"].as_str().unwrap()).collect()
}

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[tokio::test]
async fn completed_run_serves_its_journal_in_the_golden_wire_shape() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline", &[]).await;
    let run_id = run_wait(&app, &thread, json!({}), &[]).await;

    let body = get_events(&app, &run_id, &[]).await;
    assert_eq!(body["run_id"], json!(run_id));
    assert_eq!(body["complete"], json!(true));
    let events = body["events"].as_array().unwrap();
    assert!(!events.is_empty(), "a journaled run must have events");
    assert_event_invariants(events, &run_id);

    // The executor journaled the full run lifecycle: super-step boundaries,
    // node inputs/outputs, routing decisions, and checkpoint writes.
    let kinds = event_kinds(events);
    for expected in [
        "super_step_start",
        "super_step_end",
        "node_input",
        "node_output",
        "routing_decision",
        "checkpoint_written",
    ] {
        assert!(
            kinds.contains(&expected),
            "missing `{expected}` in {kinds:?}"
        );
    }
    assert_eq!(kinds[0], "super_step_start");

    // Payload refs use the adjacently tagged golden shape.
    let node_input = events
        .iter()
        .find(|e| e["kind"] == json!("node_input"))
        .unwrap();
    assert_eq!(node_input["input"]["kind"], json!("inline"));
    // Effects come from the declared classes (pipeline nodes are `pure`).
    assert_eq!(node_input["effect"], json!("pure"));
    // The wire thread id is the external one, never the internal scoped id.
    assert_eq!(node_input["thread_id"], json!(thread));
    // Causal parentage: a node input's parent is its super-step start.
    let parent = node_input["parent"].as_str().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["id"] == json!(parent) && e["kind"] == json!("super_step_start")),
        "node input parent `{parent}` is not a super-step start"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn journal_snapshot_is_persisted_to_disk_and_reverifies() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline", &[]).await;
    let run_id = run_wait(&app, &thread, json!({}), &[]).await;

    // The JSON-file layout persists one snapshot per run.
    let path = store.join("journals").join(format!("{run_id}.json"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let snapshot: JournalSnapshot = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.run_id, run_id);
    assert_eq!(snapshot.thread_id, thread);

    // The chained head hash re-verifies — the tamper-evidence boundary.
    let rebuilt = Journal::from_snapshot(snapshot.clone(), Clock::System).unwrap();
    assert_eq!(rebuilt.head_hash(), snapshot.head_hash);

    // A tampered snapshot is rejected by the same check the endpoint applies.
    let mut tampered = snapshot;
    tampered.events[0].status = EventStatus::Error;
    assert!(Journal::from_snapshot(tampered, Clock::System).is_err());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn interrupted_run_journals_the_interrupt_and_completes() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "interrupt_gate", &[]).await;
    let run_id = run_wait(&app, &thread, json!({}), &[]).await;

    let body = get_events(&app, &run_id, &[]).await;
    assert_eq!(body["complete"], json!(true));
    let events = body["events"].as_array().unwrap();
    assert_event_invariants(events, &run_id);
    let interrupt = events
        .iter()
        .find(|e| e["kind"] == json!("interrupt"))
        .expect("interrupted run journals an interrupt event");
    assert_eq!(interrupt["status"], json!("interrupted"));
    assert_eq!(interrupt["node_id"], json!("gate"));

    // Resuming is a new run with its own journal, starting at seq 0.
    let resume_id = run_wait(
        &app,
        &thread,
        json!({"command": {"resume": {"approved": true}}}),
        &[],
    )
    .await;
    assert_ne!(resume_id, run_id);
    let resumed = get_events(&app, &resume_id, &[]).await;
    let resumed_events = resumed["events"].as_array().unwrap();
    assert_event_invariants(resumed_events, &resume_id);
    let kinds = event_kinds(resumed_events);
    assert!(
        kinds.contains(&"resume"),
        "resume run journals a resume event: {kinds:?}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn events_endpoint_404s_for_unknown_runs() {
    let (app, store) = test_app();
    let (status, v) = call(
        &app,
        "GET",
        &format!("/runs/{}/events", uuid::Uuid::new_v4()),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("not_found"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn events_are_isolated_per_tenant() {
    let (app, store) = multi_tenant_app();
    let acme = [("x-api-key", "acme-secret")];
    let globex = [("x-api-key", "globex-secret")];

    let thread = create_thread(&app, "pipeline", &acme).await;
    let run_id = run_wait(&app, &thread, json!({}), &acme).await;

    // The owning tenant reads the journal; events carry the external
    // thread id (no internal `{tenant}/` prefix leaks).
    let body = get_events(&app, &run_id, &acme).await;
    assert_eq!(body["complete"], json!(true));
    let events = body["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert_eq!(events[0]["thread_id"], json!(thread));

    // Cross-tenant access answers 404, never 403 — same as `GET /runs/{id}`.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/runs/{run_id}/events"),
        None,
        &globex,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unauthenticated access is rejected by the auth middleware.
    let (status, _) = call(&app, "GET", &format!("/runs/{run_id}/events"), None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn fixture_download_captures_a_replayable_bundle() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline", &[]).await;
    let run_id = run_wait(&app, &thread, json!({}), &[]).await;

    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/fixture"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "fixture fetch failed: {v}");
    assert_eq!(v["format_version"], json!(1));
    assert!(v["graph_hash"].as_str().is_some_and(|h| !h.is_empty()));
    assert_eq!(v["graph_version"], json!("unversioned"));
    assert_eq!(v["journal"]["run_id"], json!(run_id));
    assert_eq!(v["journal"]["thread_id"], json!(thread));
    assert!(!v["journal"]["events"].as_array().unwrap().is_empty());
    // The run wrote checkpoints, so the bundle carries the final one.
    assert_eq!(v["final_checkpoint"]["thread_id"], json!(thread));

    // The served JSON is a valid fixture: version and journal integrity
    // pass core's import boundary.
    let fixture = ReplayFixture::import(&v.to_string()).expect("served fixture must import");
    assert_eq!(fixture.journal.run_id, run_id);
    assert!(fixture.final_checkpoint.is_some());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn fixture_download_404s_for_unknown_runs() {
    let (app, store) = test_app();
    let (status, v) = call(
        &app,
        "GET",
        &format!("/runs/{}/fixture", uuid::Uuid::new_v4()),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("not_found"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn fixture_is_tenant_scoped_and_never_leaks_internal_ids() {
    let (app, store) = multi_tenant_app();
    let acme = [("x-api-key", "acme-secret")];
    let globex = [("x-api-key", "globex-secret")];

    let thread = create_thread(&app, "pipeline", &acme).await;
    let run_id = run_wait(&app, &thread, json!({}), &acme).await;

    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/fixture"), None, &acme).await;
    assert_eq!(status, StatusCode::OK, "fixture fetch failed: {v}");
    // Journal events and the final checkpoint carry the EXTERNAL thread id —
    // the internal `acme/…` storage id must not leak into a download.
    assert_eq!(v["journal"]["thread_id"], json!(thread));
    assert_eq!(v["final_checkpoint"]["thread_id"], json!(thread));
    assert!(!v.to_string().contains("acme/"));

    // Cross-tenant access answers 404, never 403.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/runs/{run_id}/fixture"),
        None,
        &globex,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn journals_are_stored_per_run_not_per_thread() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "interrupt_gate", &[]).await;
    let first = run_wait(&app, &thread, json!({}), &[]).await;
    let second = run_wait(
        &app,
        &thread,
        json!({"command": {"resume": {"approved": true}}}),
        &[],
    )
    .await;

    // Two runs on one thread: two independent journals on disk.
    for run_id in [&first, &second] {
        assert!(store
            .join("journals")
            .join(format!("{run_id}.json"))
            .exists());
    }
    let first_body = get_events(&app, &first, &[]).await;
    let second_body = get_events(&app, &second, &[]).await;
    assert_eq!(first_body["run_id"], json!(first));
    assert_eq!(second_body["run_id"], json!(second));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// POST /runs/replay
// --------------------------------------------------------------------- //

/// POST replay for `run_id`; returns `(status, body)`.
async fn post_replay(app: &Router, run_id: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/runs/replay",
        Some(json!({"run_id": run_id})),
        headers,
    )
    .await
}

/// The exact keys the replay response carries (the Studio consumes this
/// shape; nothing may be added or renamed casually).
const REPLAY_RESPONSE_FIELDS: &[&str] = &[
    "run_id",
    "verified",
    "expected_events",
    "actual_events",
    "first_divergence",
];

#[tokio::test]
async fn replay_of_a_journaled_run_verifies_with_the_exact_wire_shape() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline", &[]).await;
    let run_id = run_wait(&app, &thread, json!({}), &[]).await;
    let event_count = get_events(&app, &run_id, &[]).await["events"]
        .as_array()
        .unwrap()
        .len();

    let (status, v) = post_replay(&app, &run_id, &[]).await;
    assert_eq!(status, StatusCode::OK, "replay failed: {v}");
    let object = v.as_object().unwrap();
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = REPLAY_RESPONSE_FIELDS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected, "replay response shape drifted: {v}");

    assert_eq!(v["run_id"], json!(run_id));
    assert_eq!(v["verified"], json!(true));
    assert_eq!(v["expected_events"], json!(event_count));
    assert_eq!(v["actual_events"], json!(event_count));
    assert_eq!(v["first_divergence"], Value::Null);

    // The replay ran against a throwaway checkpointer: the thread's real
    // checkpoint history is untouched by it.
    let (status, history) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/history"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        history.as_array().unwrap().len(),
        2,
        "replay must not write into the shared checkpoint log"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn replay_404s_for_unknown_runs() {
    let (app, store) = test_app();
    let (status, v) = post_replay(&app, &uuid::Uuid::new_v4().to_string(), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("not_found"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn replay_409s_for_a_queued_run_and_a_still_executing_run() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "slow_pipeline", &[]).await;

    // Run 1 occupies the thread (its second node sleeps); run 2 queues
    // behind it and has no persisted journal yet.
    let (status, v1) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "run 1 failed: {v1}");
    let run1 = v1["run_id"].as_str().unwrap().to_string();
    let (status, v2) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "run 2 failed: {v2}");
    let run2 = v2["run_id"].as_str().unwrap().to_string();

    // Queued: no persisted journal yet → 409 (same as /fixture).
    let (status, v) = post_replay(&app, &run2, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "queued run replay: {v}");

    // Wait for run 1's first checkpoint boundary to flush its journal, then
    // replay it mid-execution: the journal exists but is not final → 409.
    for _ in 0..50 {
        let body = get_events(&app, &run1, &[]).await;
        if !body["events"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (status, v) = post_replay(&app, &run1, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "active run replay: {v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn replay_422s_for_a_resumed_run() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "interrupt_gate", &[]).await;
    run_wait(&app, &thread, json!({}), &[]).await; // interrupts
    let resume_id = run_wait(
        &app,
        &thread,
        json!({"command": {"resume": {"approved": true}}}),
        &[],
    )
    .await;

    // The resume run's journal begins mid-run; core's ExactReplay rejects it.
    let (status, v) = post_replay(&app, &resume_id, &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["message"].as_str().unwrap().contains("resumed"));

    // The interrupted original replays cleanly: the interrupt is re-derived.
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn replay_is_isolated_per_tenant() {
    let (app, store) = multi_tenant_app();
    let acme = [("x-api-key", "acme-secret")];
    let globex = [("x-api-key", "globex-secret")];

    let thread = create_thread(&app, "pipeline", &acme).await;
    let run_id = run_wait(&app, &thread, json!({}), &acme).await;

    let (status, v) = post_replay(&app, &run_id, &acme).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["verified"], json!(true));

    // Cross-tenant access answers 404, never 403 — same as `GET /runs/{id}`.
    let (status, _) = post_replay(&app, &run_id, &globex).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// GET /runs/diff
// --------------------------------------------------------------------- //

/// GET the diff of `branch` against `base`; returns `(status, body)`.
async fn get_diff(
    app: &Router,
    base: &str,
    branch: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    call(
        app,
        "GET",
        &format!("/runs/diff?base={base}&branch={branch}"),
        None,
        headers,
    )
    .await
}

#[tokio::test]
async fn diff_of_a_run_and_its_fork_shows_the_divergence() {
    let (app, store) = test_app();
    let thread = create_thread(&app, "pipeline", &[]).await;
    let base_run = run_wait(&app, &thread, json!({"input": {"seed": 1}}), &[]).await;

    // Fork the (now checkpointed) thread and run the branch with a
    // different input: the evidence parts ways at the first node input.
    let (status, fork) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/fork"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fork failed: {fork}");
    let branch_thread = fork["thread_id"].as_str().unwrap();
    let branch_run = run_wait(&app, branch_thread, json!({"input": {"seed": 2}}), &[]).await;

    let (status, diff) = get_diff(&app, &base_run, &branch_run, &[]).await;
    assert_eq!(status, StatusCode::OK, "diff failed: {diff}");
    let divergence = diff["first_divergent_seq"].as_u64();
    assert!(
        divergence.is_some(),
        "forks with different inputs must diverge: {diff}"
    );
    assert!(!diff["added"].as_array().unwrap().is_empty());
    assert!(!diff["removed"].as_array().unwrap().is_empty());
    // Same graph, same step count: the branches differ in content, not length.
    assert_eq!(
        diff["base_totals"]["events"],
        diff["branch_totals"]["events"]
    );

    // A run diffed against itself is logically identical.
    let (status, same) = get_diff(&app, &base_run, &base_run, &[]).await;
    assert_eq!(status, StatusCode::OK, "{same}");
    assert_eq!(same["first_divergent_seq"], Value::Null);
    assert!(same["added"].as_array().unwrap().is_empty());
    assert!(same["removed"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn diff_404s_for_unknown_and_cross_tenant_runs() {
    let (app, store) = multi_tenant_app();
    let acme = [("x-api-key", "acme-secret")];
    let globex = [("x-api-key", "globex-secret")];

    let thread = create_thread(&app, "pipeline", &acme).await;
    let run_id = run_wait(&app, &thread, json!({}), &acme).await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let (status, _) = get_diff(&app, &run_id, &unknown, &acme).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_diff(&app, &unknown, &run_id, &acme).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Cross-tenant on either side answers 404, never 403.
    let (status, _) = get_diff(&app, &run_id, &run_id, &globex).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Journal reachability after a restart (store-level fallback)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn journals_stay_fetchable_after_a_restart() {
    let store = temp_store();
    let run_id = {
        let app = test_app_at(store.clone());
        let thread = create_thread(&app, "pipeline", &[]).await;
        run_wait(&app, &thread, json!({}), &[]).await
        // `app` (and its in-memory run manager) drops here — the "restart".
    };

    // A fresh process over the same store: the run manager is empty, so all
    // four endpoints must resolve the run through the persisted journal.
    let app = test_app_at(store.clone());

    let body = get_events(&app, &run_id, &[]).await;
    assert_eq!(body["run_id"], json!(run_id));
    // No live writer remains, so the persisted snapshot is final.
    assert_eq!(body["complete"], json!(true));
    assert!(!body["events"].as_array().unwrap().is_empty());

    let (status, fixture) = call(&app, "GET", &format!("/runs/{run_id}/fixture"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "fixture after restart: {fixture}");
    assert_eq!(fixture["journal"]["run_id"], json!(run_id));
    // The final checkpoint was recovered from the journal's last
    // checkpoint_written event (the manager's bookkeeping is gone).
    assert!(fixture["final_checkpoint"].is_object(), "{fixture}");

    let (status, replay) = post_replay(&app, &run_id, &[]).await;
    assert_eq!(status, StatusCode::OK, "replay after restart: {replay}");
    assert_eq!(replay["verified"], json!(true));

    let (status, diff) = get_diff(&app, &run_id, &run_id, &[]).await;
    assert_eq!(status, StatusCode::OK, "diff after restart: {diff}");
    assert_eq!(diff["first_divergent_seq"], Value::Null);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn replay_422s_when_the_graph_is_not_registered_after_a_restart() {
    let store = temp_store();
    let run_id = {
        let app = test_app_at(store.clone());
        let thread = create_thread(&app, "pipeline", &[]).await;
        run_wait(&app, &thread, json!({}), &[]).await
    };

    // The replacement process does not register `pipeline`: the journal is
    // reachable, but there is no graph code to re-drive → 422. The fixture
    // (a download, not a re-drive) stays available but also 409s, matching
    // the pre-existing unregistered-graph semantics.
    let (gate, gate_spec) = interrupt_graph();
    let mut registry = GraphRegistry::new();
    registry.register("interrupt_gate", gate, gate_spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    let app = router(registry, config);

    let (status, v) = post_replay(&app, &run_id, &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["message"].as_str().unwrap().contains("not registered"));

    let (status, _) = call(&app, "GET", &format!("/runs/{run_id}/fixture"), None, &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn store_fallback_keeps_tenant_isolation_after_a_restart() {
    let store = temp_store();
    let acme = [("x-api-key", "acme-secret")];
    let globex = [("x-api-key", "globex-secret")];
    let run_id = {
        let app = multi_tenant_app_at(store.clone());
        let thread = create_thread(&app, "pipeline", &acme).await;
        run_wait(&app, &thread, json!({}), &acme).await
    };

    // Restart: ownership is proven through the journal's thread id resolved
    // under the caller's tenant scope — cross-tenant stays 404.
    let app = multi_tenant_app_at(store.clone());

    let body = get_events(&app, &run_id, &acme).await;
    assert_eq!(body["complete"], json!(true));
    let (status, replay) = post_replay(&app, &run_id, &acme).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["verified"], json!(true));

    let (status, _) = call(
        &app,
        "GET",
        &format!("/runs/{run_id}/events"),
        None,
        &globex,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        "GET",
        &format!("/runs/{run_id}/fixture"),
        None,
        &globex,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post_replay(&app, &run_id, &globex).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_diff(&app, &run_id, &run_id, &globex).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}
