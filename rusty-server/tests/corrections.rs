//! The correction loop and memory operations integration tests (R0.8 Rusty
//! Learn, wave 2): the `/memory/corrections`, `/memory/consolidate`,
//! `/memory/conflicts`, `/memory/forget`, and `/memory/forget_scope`
//! surfaces over the default JSON-file backend — attribution and
//! candidacy, the dataset-example derivation, same-key auto-supersession,
//! consolidation as a durable task, conflict detection (flags, never
//! resolves), forgetting with metadata-only journaled tombstones, restart
//! durability (the wave-2 exit criteria), and tenant isolation.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `memory.rs` convention. The journaled-evidence tests need completed
//! runs' persisted journals, so the pipeline graph is registered in every
//! app here. Live-Postgres coverage of the same semantics is the gated
//! section at the bottom (`RUSTY_TEST_DATABASE_URL`).

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-corrections-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// `first -> second`, appending to a `log` channel (the flight-recorder
/// harness's minimal pipeline). Registered in every app: the
/// journaled-evidence tests need completed runs.
fn app_at(store: PathBuf) -> Router {
    use rusty_agent_runtime::prelude::*;
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
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(registry, config)
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_at(store.clone()), store)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, None, method, uri, body).await
}

/// Send a request with an optional auth header.
async fn call_as(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = auth {
        builder = builder.header(k, v);
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

/// Register an agent declaring the `private` state scope (the manifest
/// gate for agent-scoped memory).
async fn register_agent(app: &Router, agent_id: &str) {
    let (status, v) = call(
        app,
        "POST",
        "/agents",
        Some(json!({
            "agent_id": agent_id,
            "manifest": {
                "agent_kind": "researcher",
                "manifest_version": "researcher/1.0.0",
                "accepts": {"summarize": {"kind": "application/json"}},
                "scopes": ["private"],
                "budget": {"max_tokens": 100000},
            },
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "agent registration failed: {v}"
    );
}

/// Create a thread and run it to completion; returns the run id.
async fn run_pipeline(app: &Router) -> String {
    let (status, v) = call(
        &app.clone(),
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    v["run_id"].as_str().unwrap().to_string()
}

/// The run's journaled events (Flight Recorder).
async fn events_of(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

/// A human-authored memory write; asserts 201 and returns the body.
async fn write_memory(app: &Router, overrides: Value) -> Value {
    let mut payload = json!({
        "kind": "fact",
        "scope": {"scope": "user", "id": "user-7"},
        "content": {"note": "baseline"},
        "author": {"type": "human", "human_id": "amjad"},
    });
    let base = payload.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base.insert(key.clone(), value.clone());
    }
    let (status, v) = call(app, "POST", "/memory", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "write failed: {v}");
    assert_eq!(v["created"], json!(true));
    v
}

/// A correction payload; fields merge over the defaults.
fn correction_payload(overrides: Value) -> Value {
    let mut base = json!({
        "correction_id": format!("corr-{}", uuid::Uuid::new_v4()),
        "author": "amjad",
        "target": {"type": "prompt", "prompt_hash": "a".repeat(64)},
        "corrected": {"answer": "the corrected behavior"},
        "scope": {"scope": "user", "id": "user-7"},
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

/// Drive one consolidation end to end through the public surface, the
/// shape an application worker takes: enqueue the durable task, claim it,
/// run the caller's distiller over the named records, write the summary
/// through the governed write path (the task payload's `written_at`, so a
/// retried execution names the same learning instant and converges), and
/// settle the task. Returns the summary's memory id.
async fn run_consolidation(
    app: &Router,
    scope: Value,
    memory_ids: Vec<String>,
    distill: impl Fn(&[Value]) -> Value,
) -> String {
    let (status, v) = call(
        app,
        "POST",
        "/memory/consolidate",
        Some(json!({
            "scope": scope,
            "memory_ids": memory_ids,
            "distiller": "test-distiller",
        })),
    )
    .await;
    assert!(
        status == StatusCode::CREATED
            || (status == StatusCode::OK && v["deduplicated"] == json!(true)),
        "enqueue failed: {v}"
    );
    assert_eq!(v["kind"], json!("memory_consolidation"));
    let task_id = v["task_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        app,
        "POST",
        "/tasks/claim",
        Some(json!({"worker_id": "consolidator-1", "lease_ms": 30000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {v}");
    let task = &v["task"];
    assert_eq!(task["task_id"], json!(task_id));
    assert_eq!(task["kind"], json!("memory_consolidation"));
    let payload = &task["payload"];
    let ids: Vec<String> = payload["memory_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "the task names the sorted source set");
    assert!(
        payload["written_at"].is_string(),
        "written_at minted at enqueue"
    );

    let mut sources = Vec::new();
    for id in &ids {
        let (status, v) = call(app, "GET", &format!("/memory/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "source read failed: {v}");
        sources.push(v);
    }
    let confidence = sources
        .iter()
        .map(|record| record["confidence"].as_f64().unwrap())
        .fold(f64::INFINITY, f64::min);
    let content = distill(&sources);
    let (status, v) = call(
        app,
        "POST",
        "/memory",
        Some(json!({
            "kind": "summary",
            "scope": payload["scope"].clone(),
            "content": content,
            "author": {"type": "distiller", "name": "test-distiller"},
            "confidence": confidence,
            "evidence": {"source_memory_ids": ids},
            "written_at": payload["written_at"].clone(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "summary write failed: {v}");
    let summary_id = v["memory_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        app,
        "POST",
        &format!("/tasks/{task_id}/complete"),
        Some(json!({
            "worker_id": "consolidator-1",
            "result": {"memory_id": summary_id},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "complete failed: {v}");
    summary_id
}

// --------------------------------------------------------------------- //
// Wave-2 exit criterion, first half: a correction at agent scope produces
// an attributed candidate memory and a dataset example
// --------------------------------------------------------------------- //

#[tokio::test]
async fn agent_scope_correction_yields_an_attributed_candidate_and_example() {
    let (app, store) = app();
    register_agent(&app, "researcher-7").await;
    let run_id = run_pipeline(&app).await;

    // The correction targets a journaled node-input event: the example is
    // built from the input the run actually saw.
    let events = events_of(&app, &run_id).await;
    let target = events
        .iter()
        .find(|event| event["kind"] == json!("node_input"))
        .expect("the pipeline journals node inputs")
        .clone();
    let target_id = target["id"].as_str().unwrap().to_string();
    let target_input = target["input"]["value"].clone();

    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "correction_id": "corr-agent-1",
            "target": {"type": "run_event", "run_id": run_id, "event_id": target_id},
            "corrected": {"answer": "42", "unit": "AED"},
            "scope": {"scope": "agent", "id": "researcher-7"},
            "rationale": "the run quoted the pre-2024 rate",
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "correction failed: {v}");
    assert_eq!(v["correction_id"], json!("corr-agent-1"));
    assert_eq!(
        v["attribution"],
        json!("human:amjad via correction:corr-agent-1")
    );
    assert_eq!(v["candidate"], json!(true), "agent scope is candidacy");

    // The candidate memory: attribution in provenance, the correction in
    // evidence, confidence 1.0, candidacy pending.
    let candidate = &v["record"];
    assert_eq!(candidate["kind"], json!("fact"));
    assert_eq!(
        candidate["provenance"]["author"],
        json!({"type": "human", "human_id": "amjad"})
    );
    assert_eq!(
        candidate["provenance"]["evidence"]["correction_id"],
        json!("corr-agent-1")
    );
    assert_eq!(
        candidate["provenance"]["evidence"]["run_id"],
        json!(run_id.clone())
    );
    assert_eq!(candidate["confidence"], json!(1.0));
    assert_eq!(candidate["candidacy"], json!("pending"));

    // The dataset example: the input the run saw plus the corrected
    // behavior, same attribution and candidacy.
    let example_id = v["example_id"].as_str().expect("an example is derived");
    let (status, example) = call(&app, "GET", &format!("/memory/{example_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "example read failed: {example}");
    assert_eq!(example["kind"], json!("example"));
    assert_eq!(example["content"]["value"]["input"], target_input);
    assert_eq!(
        example["content"]["value"]["corrected"],
        json!({"answer": "42", "unit": "AED"})
    );
    assert_eq!(
        example["provenance"]["evidence"]["correction_id"],
        json!("corr-agent-1")
    );
    assert_eq!(example["candidacy"], json!("pending"));

    // Both are queryable as candidates.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"candidates_only": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "candidates query failed: {v}");
    assert_eq!(v["records"].as_array().unwrap().len(), 2);

    // The derived writes journal through the memory-write seam into the
    // corrected run, attribution in their provenance — there is no
    // correction event kind.
    let events = events_of(&app, &run_id).await;
    let writes: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == json!("memory_write"))
        .collect();
    assert_eq!(writes.len(), 2, "candidate and example both journal");
    assert!(writes.iter().all(|event| {
        event["output"]["value"]["provenance"]["evidence"]["correction_id"] == json!("corr-agent-1")
    }));
    assert!(!events
        .iter()
        .any(|event| event["kind"] == json!("correction")));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn run_scope_correction_is_adopted_directly_and_journaled() {
    let (app, store) = app();
    let run_id = run_pipeline(&app).await;
    let events = events_of(&app, &run_id).await;
    let target_id = events
        .iter()
        .find(|event| event["kind"] == json!("node_input"))
        .expect("pipeline journal records node input events")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "correction_id": "corr-run-1",
            "target": {"type": "run_event", "run_id": run_id, "event_id": target_id},
            "scope": {"scope": "run", "id": run_id},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "correction failed: {v}");
    assert_eq!(v["candidate"], json!(false), "run scope adopts directly");
    assert!(
        !v["record"].as_object().unwrap().contains_key("candidacy"),
        "an adopted record carries no candidacy mark"
    );

    // The example is adopted with it — scope decides the path for both.
    let example_id = v["example_id"].as_str().unwrap();
    let (status, example) = call(&app, "GET", &format!("/memory/{example_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!example.as_object().unwrap().contains_key("candidacy"));

    // Both journaled into the run they belong to.
    let events = events_of(&app, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == json!("memory_write"))
            .count(),
        2
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn memory_target_correction_inherits_key_and_auto_supersedes() {
    let (app, store) = app();
    let original = write_memory(
        &app,
        json!({
            "key": "timezone",
            "content": {"tz": "UTC+1"},
        }),
    )
    .await;
    let original_id = original["memory_id"].as_str().unwrap().to_string();

    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "correction_id": "corr-mem-1",
            "target": {"type": "memory", "memory_id": original_id},
            "corrected": {"tz": "UTC+4"},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "correction failed: {v}");
    assert_eq!(
        v["superseded"],
        json!(original_id.clone()),
        "open question 5: same-key correction writes auto-supersede"
    );
    assert_eq!(
        v["record"]["key"],
        json!("timezone"),
        "the key is inherited"
    );

    // Default retrieval serves the correction alone; the prior record
    // stays queryable as evidence — and there is nothing to flag: the
    // supersession is disciplined replacement, not conflict.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let records = v["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["content"]["value"], json!({"tz": "UTC+4"}));
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone", "include_superseded": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["records"].as_array().unwrap().len(), 2);
    let (status, v) = call(&app, "POST", "/memory/conflicts", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["conflicts"], json!([]));

    // A retried submission of the same correction converges on the
    // derived record's content address.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "correction_id": "corr-mem-1",
            "target": {"type": "memory", "memory_id": original_id},
            "corrected": {"tz": "UTC+4"},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "retry converges: {v}");
    assert_eq!(v["created"], json!(false));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn correction_endpoints_validate_attribution_and_targets() {
    let (app, store) = app();

    // An unattributed correction never reaches the handler: the contract
    // rejects it at deserialization (422).
    let mut wire = correction_payload(json!({}));
    wire["author"] = json!("  ");
    let (status, _) = call(&app, "POST", "/memory/corrections", Some(wire)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut wire = correction_payload(json!({}));
    wire.as_object_mut().unwrap().remove("author");
    let (status, _) = call(&app, "POST", "/memory/corrections", Some(wire)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown targets answer 404 (unknown or cross-tenant are
    // indistinguishable by design).
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "target": {"type": "memory", "memory_id": "f".repeat(64)},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown memory target: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "target": {"type": "run_event", "run_id": "run-nope", "event_id": "run-nope:1"},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown run target: {v}");

    // An unknown event inside a real run's journal is a 404 too.
    let run_id = run_pipeline(&app).await;
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "target": {"type": "run_event", "run_id": run_id, "event_id": format!("{run_id}:999")},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown event target: {v}");

    // Agent scope still gates on the manifest (404 unregistered), and the
    // shared gate keeps `POST /memory` rejecting run scope (400) — the
    // correction loop is the one governed exception.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "scope": {"scope": "agent", "id": "ghost"},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unregistered agent: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(json!({
            "kind": "fact",
            "scope": {"scope": "run", "id": run_id},
            "content": {"note": "not allowed"},
            "author": {"type": "human", "human_id": "amjad"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "run-scope write: {v}");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Consolidation and the memory operations
// --------------------------------------------------------------------- //

#[tokio::test]
async fn consolidation_distills_sources_into_a_superseding_summary() {
    let (app, store) = app();
    let a = write_memory(&app, json!({"key": "a", "content": {"fact": "alpha"}})).await;
    let b = write_memory(&app, json!({"key": "b", "content": {"fact": "beta"}})).await;
    let ids = vec![
        a["memory_id"].as_str().unwrap().to_string(),
        b["memory_id"].as_str().unwrap().to_string(),
    ];

    // Enqueue dedupes on scope + sorted source set: a retried submission
    // names the same live task.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/consolidate",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "memory_ids": ids,
            "distiller": "test-distiller",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "enqueue failed: {v}");
    let task_id = v["task_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        "/memory/consolidate",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "memory_ids": ids,
            "distiller": "test-distiller",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "retry dedupes: {v}");
    assert_eq!(v["deduplicated"], json!(true));
    assert_eq!(v["task_id"], json!(task_id));

    let summary_id = run_consolidation(
        &app,
        json!({"scope": "user", "id": "user-7"}),
        ids.clone(),
        |sources| {
            json!({
                "combined": sources
                    .iter()
                    .map(|record| record["content"]["value"]["fact"].clone())
                    .collect::<Vec<_>>(),
            })
        },
    )
    .await;

    // The summary names its sources in evidence and supersedes them:
    // default retrieval serves the summary alone, and the sources stay
    // queryable as evidence.
    let (status, summary) = call(&app, "GET", &format!("/memory/{summary_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["kind"], json!("summary"));
    assert_eq!(
        summary["provenance"]["author"],
        json!({"type": "distiller", "name": "test-distiller"})
    );
    let mut named: Vec<String> = summary["provenance"]["evidence"]["source_memory_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect();
    named.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(named, expected);

    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "user-7"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let records = v["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "only the summary serves by default");
    assert_eq!(records[0]["memory_id"], json!(summary_id));
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"include_superseded": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["records"].as_array().unwrap().len(), 3);

    // Enqueue-time validation: unknown ids 404, cross-scope sets 400, an
    // empty set 400 — a task that cannot read its inputs must not queue.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/consolidate",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "memory_ids": ["f".repeat(64)],
            "distiller": "test-distiller",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown source: {v}");
    let other = write_memory(
        &app,
        json!({"scope": {"scope": "user", "id": "someone-else"}}),
    )
    .await;
    let (status, v) = call(
        &app,
        "POST",
        "/memory/consolidate",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "memory_ids": [other["memory_id"].as_str().unwrap()],
            "distiller": "test-distiller",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "cross-scope set: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/memory/consolidate",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "memory_ids": [],
            "distiller": "test-distiller",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty set: {v}");

    let _ = std::fs::remove_dir_all(store);
}

/// Wave-2 exit criterion, second half: `forget` removes the record and
/// invalidates its dependent summaries, with the tombstone journaled and
/// the store verified clean by query.
#[tokio::test]
async fn forget_removes_the_record_invalidates_summaries_and_journals_a_metadata_only_tombstone() {
    let (app, store) = app();
    let suffix = uuid::Uuid::new_v4().to_string();
    let secret_a = format!("erasure-alpha-{suffix}");
    let secret_summary = format!("erasure-distilled-{suffix}");
    let a = write_memory(&app, json!({"key": "a", "content": {"fact": secret_a}})).await;
    let b = write_memory(&app, json!({"key": "b", "content": {"fact": "beta"}})).await;
    let a_id = a["memory_id"].as_str().unwrap().to_string();
    let b_id = b["memory_id"].as_str().unwrap().to_string();

    // Consolidate first, so the erasure has a dependent summary to
    // invalidate.
    let summary_id = run_consolidation(
        &app,
        json!({"scope": "user", "id": "user-7"}),
        vec![a_id.clone(), b_id.clone()],
        move |sources| {
            json!({
                "combined": sources
                    .iter()
                    .map(|record| record["content"]["value"]["fact"].clone())
                    .collect::<Vec<_>>(),
                "marker": secret_summary,
            })
        },
    )
    .await;

    let run_id = run_pipeline(&app).await;
    let (status, v) = call(
        &app,
        "POST",
        "/memory/forget",
        Some(json!({
            "memory_id": a_id,
            "reason": "retracted",
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "forget failed: {v}");
    assert_eq!(v["forgotten"], json!([a_id.clone()]));
    assert_eq!(
        v["invalidated"],
        json!([summary_id.clone()]),
        "the dependent summary is invalidated with the record"
    );
    assert_eq!(
        v["tombstone"],
        json!({
            "memory_id": a_id,
            "scope": {"scope": "user", "id": "user-7"},
            "reason": "retracted",
            "invalidated": [summary_id.clone()],
        })
    );

    // The store is verified clean by query: the forgotten record and its
    // dependent summary are gone — even with superseded records included —
    // while the untouched source stands.
    let (status, _) = call(&app, "GET", &format!("/memory/{a_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "GET", &format!("/memory/{summary_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"include_superseded": true, "include_expired": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let records = v["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["memory_id"], json!(b_id));

    // The tombstone is journaled — and carries metadata only: the whole
    // events payload contains neither the forgotten record's content nor
    // the summary it distilled into.
    let events = events_of(&app, &run_id).await;
    let tombstones: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == json!("memory_forget"))
        .collect();
    assert_eq!(tombstones.len(), 1, "one tombstone is journaled");
    let tombstone = tombstones[0];
    assert_eq!(tombstone["effect"], json!("idempotent"));
    assert_eq!(
        tombstone["input"]["value"]["effect_key"],
        json!(format!("memory_forget:user:user-7:{a_id}"))
    );
    assert_eq!(tombstone["output"]["value"]["memory_id"], json!(a_id));
    assert_eq!(tombstone["output"]["value"]["reason"], json!("retracted"));
    assert_eq!(
        tombstone["output"]["value"]["invalidated"],
        json!([summary_id])
    );
    let serialized = serde_json::to_string(&events).unwrap();
    assert!(
        !serialized.contains("erasure-alpha-"),
        "the tombstone must not carry the forgotten content"
    );
    assert!(
        !serialized.contains("erasure-distilled-"),
        "the tombstone must not carry the invalidated summary's content"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn forget_scope_erases_exactly_one_scope() {
    let (app, store) = app();
    let a = write_memory(
        &app,
        json!({"scope": {"scope": "user", "id": "erasure-me"}, "key": "a"}),
    )
    .await;
    let b = write_memory(
        &app,
        json!({"scope": {"scope": "user", "id": "erasure-me"}, "key": "b"}),
    )
    .await;
    let bystander = write_memory(
        &app,
        json!({"scope": {"scope": "user", "id": "bystander"}, "key": "c"}),
    )
    .await;
    let a_id = a["memory_id"].as_str().unwrap().to_string();
    let b_id = b["memory_id"].as_str().unwrap().to_string();
    let bystander_id = bystander["memory_id"].as_str().unwrap().to_string();

    // A summary in another scope that names an erased record: the
    // dependent-invalidation walk crosses scopes, because the derivation
    // did.
    let derived = write_memory(
        &app,
        json!({
            "kind": "summary",
            "scope": {"scope": "user", "id": "bystander"},
            "content": {"derived": true},
            "author": {"type": "distiller", "name": "test-distiller"},
            "confidence": 0.8,
            "evidence": {"source_memory_ids": [a_id.clone()]},
        }),
    )
    .await;
    let derived_id = derived["memory_id"].as_str().unwrap().to_string();

    let run_id = run_pipeline(&app).await;
    let (status, v) = call(
        &app,
        "POST",
        "/memory/forget_scope",
        Some(json!({
            "scope": {"scope": "user", "id": "erasure-me"},
            "reason": "erasure_request",
            "run_id": run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "forget_scope failed: {v}");
    let mut forgotten: Vec<String> = v["forgotten"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect();
    forgotten.sort();
    let mut expected = vec![a_id.clone(), b_id.clone()];
    expected.sort();
    assert_eq!(forgotten, expected);
    assert_eq!(
        v["invalidated"],
        json!([derived_id.clone()]),
        "the cross-scope dependent summary is invalidated"
    );
    assert_eq!(
        v["tombstones"],
        json!(2),
        "one tombstone per forgotten record"
    );

    // Exactly one scope was erased: the bystander's own record stands.
    let (status, _) = call(&app, "GET", &format!("/memory/{a_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "GET", &format!("/memory/{b_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&app, "GET", &format!("/memory/{derived_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call(&app, "GET", &format!("/memory/{bystander_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "bystander stands: {v}");

    // Both tombstones are journaled, metadata-only.
    let events = events_of(&app, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == json!("memory_forget"))
            .count(),
        2
    );

    // Erasure is idempotent: a second request erases nothing.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/forget_scope",
        Some(json!({
            "scope": {"scope": "user", "id": "erasure-me"},
            "reason": "erasure_request",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["forgotten"], json!([]));
    assert_eq!(v["tombstones"], json!(0));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn conflict_detection_flags_contradictions_without_resolving() {
    let (app, store) = app();
    let one = write_memory(
        &app,
        json!({
            "key": "timezone",
            "content": {"tz": "UTC+4"},
            "author": {"type": "distiller", "name": "inferred-v1"},
            "confidence": 0.9,
        }),
    )
    .await;
    let two = write_memory(
        &app,
        json!({
            "key": "timezone",
            "content": {"tz": "UTC+1"},
            "author": {"type": "distiller", "name": "inferred-v2"},
            "confidence": 0.8,
        }),
    )
    .await;
    let one_id = one["memory_id"].as_str().unwrap().to_string();
    let two_id = two["memory_id"].as_str().unwrap().to_string();

    let (status, v) = call(&app, "POST", "/memory/conflicts", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "conflicts failed: {v}");
    let conflicts = v["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "the contradictory pair is flagged");
    let conflict = &conflicts[0];
    assert_eq!(conflict["key"], json!("timezone"));
    assert_eq!(conflict["scope"], json!({"scope": "user", "id": "user-7"}));
    let mut flagged: Vec<String> = conflict["memory_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect();
    flagged.sort();
    let mut expected = vec![one_id.clone(), two_id.clone()];
    expected.sort();
    assert_eq!(flagged, expected);

    // The scope filter narrows the listing.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/conflicts",
        Some(json!({"scope": {"scope": "user", "id": "someone-else"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["conflicts"], json!([]));

    // Detection is evidence; resolution is governance: both records still
    // serve, and nothing was superseded.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["records"].as_array().unwrap().len(),
        2,
        "flagged records are not resolved"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability and tenant isolation
// --------------------------------------------------------------------- //

/// Wave-2 exit criterion, durability half: correction-derived records and
/// journaled tombstones survive a server restart on the JSON backend.
#[tokio::test]
async fn corrections_and_tombstones_survive_a_restart() {
    let (first_app, store) = app();
    register_agent(&first_app, "researcher-7").await;
    let run_id = run_pipeline(&first_app).await;
    let events = events_of(&first_app, &run_id).await;
    let target_id = events
        .iter()
        .find(|event| event["kind"] == json!("node_input"))
        .expect("pipeline journal records node input events")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, v) = call(
        &first_app,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "correction_id": "corr-restart-1",
            "target": {"type": "run_event", "run_id": run_id, "event_id": target_id},
            "corrected": {"lesson": "durable attribution"},
            "scope": {"scope": "agent", "id": "researcher-7"},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "correction failed: {v}");
    let candidate_id = v["memory_id"].as_str().unwrap().to_string();
    let example_id = v["example_id"].as_str().unwrap().to_string();

    let secret = format!("restart-erasure-{}", uuid::Uuid::new_v4());
    let doomed = write_memory(&first_app, json!({"content": {"note": secret}})).await;
    let doomed_id = doomed["memory_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &first_app,
        "POST",
        "/memory/forget",
        Some(json!({"memory_id": doomed_id, "reason": "retracted", "run_id": run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "forget failed: {v}");
    drop(first_app);

    // Restart: a fresh app over the same store root. The candidate and
    // example serve with their attribution intact; the forgotten record
    // stays gone; the journaled evidence holds both halves.
    let second_app = app_at(store.clone());
    let (status, candidate) =
        call(&second_app, "GET", &format!("/memory/{candidate_id}"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "candidate after restart: {candidate}"
    );
    assert_eq!(
        candidate["provenance"]["evidence"]["correction_id"],
        json!("corr-restart-1")
    );
    assert_eq!(candidate["candidacy"], json!("pending"));
    let (status, example) = call(&second_app, "GET", &format!("/memory/{example_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "example after restart: {example}");
    let (status, _) = call(&second_app, "GET", &format!("/memory/{doomed_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the erasure survives too");
    let (status, v) = call(
        &second_app,
        "POST",
        "/memory/query",
        Some(json!({"candidates_only": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["records"].as_array().unwrap().len(), 2);

    let events = events_of(&second_app, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == json!("memory_write"))
            .count(),
        2
    );
    let tombstones: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == json!("memory_forget"))
        .collect();
    assert_eq!(tombstones.len(), 1, "the tombstone survives the restart");
    assert_eq!(
        tombstones[0]["output"]["value"]["memory_id"],
        json!(doomed_id)
    );
    let serialized = serde_json::to_string(&events).unwrap();
    assert!(
        !serialized.contains("restart-erasure-"),
        "the tombstone carries metadata only, across restarts"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn corrections_and_forgetting_are_tenant_isolated() {
    let store = temp_store();
    use rusty_agent_runtime::prelude::*;
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.set_entry_point("first");
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    let app = router(registry, config);
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    // Acme's run, record, and correction.
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/threads",
        Some(json!({"graph": "pipeline"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/memory",
        Some(json!({
            "kind": "fact",
            "scope": {"scope": "user", "id": "user-7"},
            "content": {"note": "acme only"},
            "author": {"type": "human", "human_id": "amjad"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let memory_id = v["memory_id"].as_str().unwrap().to_string();

    // Globex cannot read, correct, forget, or erase acme's evidence:
    // unknown and cross-tenant are indistinguishable by design.
    let (status, _) = call_as(&app, globex, "GET", &format!("/memory/{memory_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        globex,
        "POST",
        "/memory/forget",
        Some(json!({"memory_id": memory_id, "reason": "retracted"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        globex,
        "POST",
        "/memory/corrections",
        Some(correction_payload(json!({
            "target": {"type": "run_event", "run_id": run_id, "event_id": format!("{run_id}:1")},
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call_as(
        &app,
        globex,
        "POST",
        "/memory/forget_scope",
        Some(json!({
            "scope": {"scope": "tenant", "id": "acme"},
            "reason": "erasure_request",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Acme's record stands untouched.
    let (status, _) = call_as(&app, acme, "GET", &format!("/memory/{memory_id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly so the suite is
// green without a database. Every test uses a dedicated tenant whose
// records are scoped under a per-run prefix, so repeated runs against one
// scratch database never interfere; the database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// A Postgres-backed pipeline app with a dedicated tenant.
    fn pg_app(url: &str, tenant: &str) -> Router {
        use rusty_agent_runtime::prelude::*;
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
        let mut registry = GraphRegistry::new();
        registry.register("pipeline", builder.compile().unwrap(), spec);
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), temp_store())
            .with_postgres(url.to_string())
            .with_tenant_key(tenant, "pg-secret");
        router(registry, config)
    }

    /// Wave-2 exit criteria on Postgres: the correction's attributed
    /// candidate and example, consolidation's source supersession through
    /// the column-mapped path, and forgetting's metadata-only tombstone —
    /// all surviving a restart.
    #[tokio::test]
    async fn postgres_corrections_consolidation_and_forgetting_survive_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("corrpg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));

        let first = pg_app(&url, &tenant);
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/agents",
            Some(json!({
                "agent_id": "researcher-7",
                "manifest": {
                    "agent_kind": "researcher",
                    "manifest_version": "researcher/1.0.0",
                    "accepts": {"summarize": {"kind": "application/json"}},
                    "scopes": ["private"],
                    "budget": {"max_tokens": 100000},
                },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg agent failed: {v}");

        // A completed run, and a correction at agent scope targeting one
        // of its journaled events.
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/threads",
            Some(json!({"graph": "pipeline"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg thread failed: {v}");
        let thread_id = v["thread_id"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg run failed: {v}");
        let run_id = v["run_id"].as_str().unwrap().to_string();
        let (status, v) =
            call_as(&first, auth, "GET", &format!("/runs/{run_id}/events"), None).await;
        assert_eq!(status, StatusCode::OK);
        let target_id = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == json!("node_input"))
            .expect("pipeline journal records node input events")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/memory/corrections",
            Some(correction_payload(json!({
                "correction_id": "corr-pg-1",
                "target": {"type": "run_event", "run_id": run_id, "event_id": target_id},
                "corrected": {"lesson": "postgres attribution"},
                "scope": {"scope": "agent", "id": "researcher-7"},
            }))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg correction failed: {v}");
        let candidate_id = v["memory_id"].as_str().unwrap().to_string();
        let example_id = v["example_id"].as_str().unwrap().to_string();

        // Two sources and a consolidation through the durable task: the
        // column-mapped superseded-set scan must honor the summary's
        // source naming.
        let (status, a) = call_as(
            &first,
            auth,
            "POST",
            "/memory",
            Some(json!({
                "kind": "fact",
                "scope": {"scope": "user", "id": "user-7"},
                "key": "a",
                "content": {"fact": "alpha-pg"},
                "author": {"type": "human", "human_id": "amjad"},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg write a failed: {a}");
        let (status, b) = call_as(
            &first,
            auth,
            "POST",
            "/memory",
            Some(json!({
                "kind": "fact",
                "scope": {"scope": "user", "id": "user-7"},
                "key": "b",
                "content": {"fact": "beta-pg"},
                "author": {"type": "human", "human_id": "amjad"},
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg write b failed: {b}");
        let a_id = a["memory_id"].as_str().unwrap().to_string();
        let b_id = b["memory_id"].as_str().unwrap().to_string();

        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/memory/consolidate",
            Some(json!({
                "scope": {"scope": "user", "id": "user-7"},
                "memory_ids": [a_id.clone(), b_id.clone()],
                "distiller": "test-distiller",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg enqueue failed: {v}");
        let task_id = v["task_id"].as_str().unwrap().to_string();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/tasks/claim",
            Some(json!({"worker_id": "consolidator-1", "lease_ms": 30000})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg claim failed: {v}");
        let payload = v["task"]["payload"].clone();
        let mut ids: Vec<String> = payload["memory_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect();
        ids.sort();
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/memory",
            Some(json!({
                "kind": "summary",
                "scope": payload["scope"].clone(),
                "content": {"combined": ["alpha-pg", "beta-pg"]},
                "author": {"type": "distiller", "name": "test-distiller"},
                "confidence": 1.0,
                "evidence": {"source_memory_ids": ids},
                "written_at": payload["written_at"].clone(),
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg summary failed: {v}");
        let summary_id = v["memory_id"].as_str().unwrap().to_string();
        let (status, _) = call_as(
            &first,
            auth,
            "POST",
            &format!("/tasks/{task_id}/complete"),
            Some(json!({"worker_id": "consolidator-1", "result": {"memory_id": summary_id}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg complete failed");

        // The column-mapped query path: default retrieval serves the
        // summary alone.
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/memory/query",
            Some(json!({"scope": {"scope": "user", "id": "user-7"}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg query failed: {v}");
        let records = v["records"].as_array().unwrap();
        assert_eq!(records.len(), 1, "only the summary serves by default");
        assert_eq!(records[0]["memory_id"], json!(summary_id.clone()));

        // Forget one source with the tombstone journaled into the run.
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/memory/forget",
            Some(json!({"memory_id": a_id, "reason": "retracted", "run_id": run_id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg forget failed: {v}");
        assert_eq!(v["invalidated"], json!([summary_id.clone()]));
        drop(first);

        // Restart against the same database: attribution intact, erasure
        // held, evidence complete.
        let second = pg_app(&url, &tenant);
        let (status, candidate) = call_as(
            &second,
            auth,
            "GET",
            &format!("/memory/{candidate_id}"),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "pg candidate after restart: {candidate}"
        );
        assert_eq!(
            candidate["provenance"]["evidence"]["correction_id"],
            json!("corr-pg-1")
        );
        assert_eq!(candidate["candidacy"], json!("pending"));
        let (status, _) =
            call_as(&second, auth, "GET", &format!("/memory/{example_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "pg example after restart");
        let (status, _) = call_as(&second, auth, "GET", &format!("/memory/{a_id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) =
            call_as(&second, auth, "GET", &format!("/memory/{summary_id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, v) = call_as(
            &second,
            auth,
            "POST",
            "/memory/query",
            Some(json!({
                "scope": {"scope": "user", "id": "user-7"},
                "include_superseded": true,
                "include_expired": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg clean query failed: {v}");
        let records = v["records"].as_array().unwrap();
        assert_eq!(records.len(), 1, "the store is verified clean by query");
        assert_eq!(records[0]["memory_id"], json!(b_id));

        let (status, v) = call_as(
            &second,
            auth,
            "GET",
            &format!("/runs/{run_id}/events"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg events failed: {v}");
        let events = v["events"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == json!("memory_write"))
                .count(),
            2
        );
        let tombstones: Vec<&Value> = events
            .iter()
            .filter(|event| event["kind"] == json!("memory_forget"))
            .collect();
        assert_eq!(tombstones.len(), 1, "the tombstone survives Postgres too");
        assert_eq!(
            tombstones[0]["output"]["value"]["invalidated"],
            json!([summary_id])
        );
        let serialized = serde_json::to_string(&events).unwrap();
        assert!(!serialized.contains("alpha-pg"));
    }
}
