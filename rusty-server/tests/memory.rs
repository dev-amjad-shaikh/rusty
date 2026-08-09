//! Governed memory integration tests (R0.8 Rusty Learn, wave 1): the
//! `/memory` HTTP surface over the default JSON-file backend — write
//! gates (scope authorization), content-address dedupe, structured
//! retrieval with the token-bounded assembly, tenant isolation,
//! artifact spill with self-contained reads, restart durability (the
//! wave-1 exit criterion 1a), and route-journaled memory events
//! landing in a run's Flight Recorder journal.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `flight_recorder.rs` convention. Live-Postgres coverage of the same
//! semantics (exit criterion 1b) is the gated section at the bottom.
//!
//! The journaled-events test needs a completed run's persisted journal,
//! so one small pipeline graph is registered (the flight-recorder
//! harness's); every other test keeps the registry empty.

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
    std::env::temp_dir().join(format!("rusty-server-memory-test-{}", uuid::Uuid::new_v4()))
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

/// Two-tenant app for the isolation tests.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(GraphRegistry::new(), config), store)
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

/// A human-authored write payload; fields merge over the defaults.
fn write_payload(overrides: Value) -> Value {
    let mut base = json!({
        "kind": "fact",
        "scope": {"scope": "user", "id": "user-7"},
        "content": {"timezone": "Asia/Dubai"},
        "author": {"type": "human", "human_id": "amjad"},
    });
    let base_map = base.as_object_mut().unwrap();
    for (key, value) in overrides.as_object().unwrap() {
        base_map.insert(key.clone(), value.clone());
    }
    base
}

/// Write a record; asserts 201 and returns the response body.
async fn write(app: &Router, payload: Value) -> Value {
    let (status, v) = call(app, "POST", "/memory", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "write failed: {v}");
    assert_eq!(v["created"], json!(true));
    v
}

// --------------------------------------------------------------------- //
// Round trip, dedupe, restart durability
// --------------------------------------------------------------------- //

#[tokio::test]
async fn memory_write_read_query_round_trip() {
    let (app, store) = app();
    let v = write(
        &app,
        write_payload(json!({"key": "timezone", "tags": ["prefs"]})),
    )
    .await;
    let memory_id = v["memory_id"].as_str().unwrap().to_string();
    // Content addressing: a 64-char hex digest.
    assert_eq!(memory_id.len(), 64);
    // Human-authored confidence defaults to 1.0; provenance is mandatory
    // and travels with the record.
    assert_eq!(v["record"]["confidence"], json!(1.0));
    assert_eq!(
        v["record"]["provenance"]["author"],
        json!({"type": "human", "human_id": "amjad"})
    );
    assert_eq!(
        v["record"]["scope"],
        json!({"scope": "user", "id": "user-7"})
    );

    // GET by content address returns the same record.
    let (status, got) = call(&app, "GET", &format!("/memory/{memory_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {got}");
    assert_eq!(got, v["record"]);

    // Structured query by scope + key + tags finds it.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "key": "timezone",
            "tags": ["prefs"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {v}");
    let records = v["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["memory_id"], json!(memory_id));
    // A non-matching tag misses (equality, every listed tag).
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"tags": ["prefs", "other"]})),
    )
    .await;
    assert_eq!(v["records"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn memory_dedupe_returns_created_false() {
    let (app, store) = app();
    // `written_at` is part of provenance, and provenance is part of the
    // content address — so the idempotent-retry case names its learning
    // instant explicitly (the default is per-request `now`, and two
    // genuinely different learnings of the same content are distinct
    // records by design).
    let learned = json!({"written_at": "2026-01-15T10:00:00Z"});
    let first = write(&app, write_payload(learned.clone())).await;
    // The identical write (same content + provenance ⇒ same address) does
    // not create: 200 with `created: false` and the same memory id.
    let (status, second) = call(&app, "POST", "/memory", Some(write_payload(learned))).await;
    assert_eq!(status, StatusCode::OK, "dedupe write failed: {second}");
    assert_eq!(second["created"], json!(false));
    assert_eq!(second["memory_id"], first["memory_id"]);
    // A different provenance is a different record (origin is identity).
    let other = write(
        &app,
        write_payload(json!({
            "written_at": "2026-01-15T10:00:00Z",
            "author": {"type": "human", "human_id": "reviewer"},
        })),
    )
    .await;
    assert_ne!(other["memory_id"], first["memory_id"]);
    let _ = std::fs::remove_dir_all(store);
}

/// Wave-1 exit criterion 1a: memory survives a server restart on the
/// JSON-file backend.
#[tokio::test]
async fn memory_survives_a_server_restart() {
    let (first_app, store) = app();
    let written = write(
        &first_app,
        write_payload(json!({"key": "timezone", "tags": ["prefs"]})),
    )
    .await;
    let memory_id = written["memory_id"].as_str().unwrap().to_string();
    drop(first_app);

    // Rebuild the server over the same store root: the record reads back
    // by address and answers the same query.
    let second_app = app_at(store.clone());
    let (status, got) = call(&second_app, "GET", &format!("/memory/{memory_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get after restart failed: {got}");
    assert_eq!(got, written["record"]);
    let (status, v) = call(
        &second_app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "user-7"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query after restart failed: {v}");
    assert_eq!(v["records"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Write gates (scope authorization)
// --------------------------------------------------------------------- //

#[tokio::test]
async fn memory_write_gates_enforce_scope_authorization() {
    let (app, store) = app();

    // Run scope is runtime-only.
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(
            json!({"scope": {"scope": "run", "id": "run-1"}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "run scope must 400: {v}");

    // Agent scope: unknown agent → 404.
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(
            json!({"scope": {"scope": "agent", "id": "ghost"}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown agent must 404: {v}");

    // Agent scope: a registered agent whose manifest does NOT declare the
    // private state scope → 403.
    let (status, v) = call(
        &app,
        "POST",
        "/agents",
        Some(json!({
            "agent_id": "unscoped",
            "manifest": {"agent_kind": "researcher", "manifest_version": "researcher/1.0.0"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(
            json!({"scope": {"scope": "agent", "id": "unscoped"}}),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "undeclared private scope must 403: {v}"
    );

    // Agent scope: with `private` declared the write lands.
    let (status, v) = call(
        &app,
        "POST",
        "/agents",
        Some(json!({
            "agent_id": "researcher-7",
            "manifest": {
                "agent_kind": "researcher",
                "manifest_version": "researcher/1.4.0",
                "scopes": ["private", "team"],
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(json!({
            "scope": {"scope": "agent", "id": "researcher-7"},
            "author": {"type": "agent", "agent_id": "researcher-7"},
            "confidence": 0.8,
        }))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "declared agent scope must write: {v}"
    );

    // Tenant scope: the scope id must be the caller's own tenant (open
    // mode runs as `default`).
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(
            json!({"scope": {"scope": "tenant", "id": "acme"}}),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "foreign tenant scope must 403: {v}"
    );
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(
            json!({"scope": {"scope": "tenant", "id": "default"}}),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "own tenant scope must write: {v}"
    );

    // Confidence: required for non-human authors.
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(json!({
            "author": {"type": "distiller", "name": "nightly"},
            "content": {"distilled": true},
        }))),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing confidence must 400: {v}"
    );
    // …and rejected outside (0, 1].
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(
            json!({"confidence": 1.5, "content": {"other": 1}}),
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "out-of-range confidence must 400: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Retrieval semantics: filters, ranking, budget, supersession, expiry
// --------------------------------------------------------------------- //

/// Three user-7 records with distinct ranks, plus one user-8 record:
/// priorities 10 / 0 / 0, confidences 0.5 / 0.9 / 0.3.
async fn seed_ranked(app: &Router) -> (String, String, String) {
    let high_priority = write(
        app,
        write_payload(json!({
            "content": {"fact": "low-confidence-but-prioritized"},
            "priority": 10,
            "confidence": 0.5,
            "tags": ["ranked"],
        })),
    )
    .await;
    let high_confidence = write(
        app,
        write_payload(json!({
            "content": {"fact": "confident"},
            "confidence": 0.9,
            "tags": ["ranked"],
        })),
    )
    .await;
    let low_confidence = write(
        app,
        write_payload(json!({
            "content": {"fact": "unsure"},
            "confidence": 0.3,
            "tags": ["ranked"],
        })),
    )
    .await;
    write(
        app,
        write_payload(json!({
            "scope": {"scope": "user", "id": "user-8"},
            "content": {"fact": "someone-else"},
        })),
    )
    .await;
    (
        high_priority["memory_id"].as_str().unwrap().to_string(),
        high_confidence["memory_id"].as_str().unwrap().to_string(),
        low_confidence["memory_id"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn memory_query_filters_rank_and_budget() {
    let (app, store) = app();
    let (prioritized, confident, unsure) = seed_ranked(&app).await;

    // Scope + kind filter, no budget: the assembly's total order —
    // priority first, then confidence.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "user-7"}, "kinds": ["fact"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {v}");
    let ids: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![prioritized.as_str(), confident.as_str(), unsure.as_str()]
    );

    // min_confidence filters before ranking.
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "min_confidence": 0.4,
        })),
    )
    .await;
    let ids: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![prioritized.as_str(), confident.as_str()]);

    // A budget answers the assembly shape: ranked ids, packed records,
    // the accounting the packing applied. Generous budget → all three.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "budget": {"max_tokens": 100000},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "budgeted query failed: {v}");
    assert_eq!(v["truncated"], json!(false));
    assert_eq!(v["memory_ids"], json!([prioritized, confident, unsure]));
    assert_eq!(v["token_accounting"]["budget_tokens"], json!(100000));
    assert_eq!(v["token_accounting"]["bytes_per_token"], json!(4));
    assert_eq!(v["token_accounting"]["margin_percent"], json!(20));

    // A tight budget truncates honestly: the top-ranked record fits, the
    // rest drop with `truncated: true`.
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "budget": {"max_tokens": 15},
        })),
    )
    .await;
    assert_eq!(v["truncated"], json!(true));
    assert_eq!(v["memory_ids"].as_array().unwrap().len(), 1);

    // A hard budget (overflow: fail) refuses loudly → 422.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "budget": {"max_tokens": 15, "overflow": "fail"},
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "hard overflow must 422: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn memory_supersession_and_expiry_are_retrieval_filters() {
    let (app, store) = app();
    let original = write(&app, write_payload(json!({"key": "timezone"}))).await;
    let replacement = write(
        &app,
        write_payload(json!({
            "content": {"timezone": "Asia/Riyadh"},
            "key": "timezone",
            "supersedes": original["memory_id"],
        })),
    )
    .await;

    // Default retrieval hides the superseded record; it is retained as
    // evidence (still fetchable by address) and explicit on request.
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone"})),
    )
    .await;
    let ids: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![replacement["memory_id"].as_str().unwrap()]);
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone", "include_superseded": true})),
    )
    .await;
    assert_eq!(v["records"].as_array().unwrap().len(), 2);
    let (status, _) = call(
        &app,
        "GET",
        &format!("/memory/{}", original["memory_id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "superseded is retained, not deleted"
    );

    // Expiry: a record past its TTL drops out of default retrieval, is
    // explicit on request. (Expiry evaluates against `as_of`, resolved
    // at read time.)
    let expired = write(
        &app,
        write_payload(json!({
            "content": {"promo": "ended"},
            "expires_at": "2000-01-01T00:00:00Z",
        })),
    )
    .await;
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "user-7"}})),
    )
    .await;
    let ids: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memory_id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&expired["memory_id"].as_str().unwrap()));
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"include_expired": true})),
    )
    .await;
    let ids: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memory_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&expired["memory_id"].as_str().unwrap()));

    // Validity-at-time: the window must contain `valid_at`.
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"key": "timezone", "valid_at": "1990-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(
        v["records"].as_array().unwrap().len(),
        0,
        "nothing was true in 1990"
    );

    let _ = std::fs::remove_dir_all(store);
}

/// A summary supersedes the sources it names in
/// `evidence.source_memory_ids` (wave 2): consolidation supersedes what
/// it consolidates, so default retrieval serves the summary alone while
/// the sources stay queryable as evidence.
#[tokio::test]
async fn memory_summary_supersedes_its_sources_in_default_retrieval() {
    let (app, store) = app();
    let a = write(&app, write_payload(json!({"content": {"fact": "alpha"}}))).await;
    let b = write(&app, write_payload(json!({"content": {"fact": "beta"}}))).await;
    let source_ids = vec![
        a["memory_id"].as_str().unwrap().to_string(),
        b["memory_id"].as_str().unwrap().to_string(),
    ];
    let summary = write(
        &app,
        write_payload(json!({
            "kind": "summary",
            "content": {"combined": ["alpha", "beta"]},
            "author": {"type": "distiller", "name": "test-distiller"},
            "confidence": 1.0,
            "evidence": {"source_memory_ids": source_ids},
        })),
    )
    .await;

    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "user", "id": "user-7"}})),
    )
    .await;
    let ids: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memory_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![summary["memory_id"].as_str().unwrap()],
        "default retrieval serves the summary alone"
    );
    let (_, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "include_superseded": true,
        })),
    )
    .await;
    assert_eq!(
        v["records"].as_array().unwrap().len(),
        3,
        "sources stay queryable as evidence"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn memory_is_tenant_isolated() {
    let (app, store) = multi_tenant_app();
    let acme = Some(("x-api-key", "acme-secret"));
    let globex = Some(("x-api-key", "globex-secret"));

    let (status, v) = call_as(
        &app,
        acme,
        "POST",
        "/memory",
        Some(write_payload(json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acme write failed: {v}");
    let memory_id = v["memory_id"].as_str().unwrap().to_string();

    // Cross-tenant reads are 404, never 403 — the other tenant's memory
    // simply does not exist here.
    let (status, _) = call_as(&app, globex, "GET", &format!("/memory/{memory_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // …and queries never leak across the boundary.
    let (_, v) = call_as(&app, globex, "POST", "/memory/query", Some(json!({}))).await;
    assert_eq!(v["records"].as_array().unwrap().len(), 0);
    let (_, v) = call_as(&app, acme, "POST", "/memory/query", Some(json!({}))).await;
    assert_eq!(v["records"].as_array().unwrap().len(), 1);

    // The default tenant's namespace is the unprefixed one: an open-mode
    // server over the same store sees neither tenant's records.
    let default_app = app_at(store.clone());
    let (_, v) = call(&default_app, "POST", "/memory/query", Some(json!({}))).await;
    assert_eq!(v["records"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Artifact spill
// --------------------------------------------------------------------- //

#[tokio::test]
async fn memory_artifact_spill_serves_self_contained_records() {
    let (first_app, store) = app();
    // A body past the 4 KiB inline threshold spills, content-addressed.
    let big_content = json!({"transcript": "x".repeat(9000), "turns": 42});
    let written = write(
        &first_app,
        write_payload(json!({"content": big_content.clone()})),
    )
    .await;
    // The served record is self-contained: the body re-inlines on read
    // (an `inline` payload carrying the full value), indistinguishable
    // from a small record.
    assert_eq!(written["record"]["content"]["kind"], json!("inline"));
    assert_eq!(written["record"]["content"]["value"], big_content);
    // …but on disk it spilled: the blob lives under memory_artifacts/,
    // the record file under memory/ carries only the reference.
    let artifacts = store.join("memory_artifacts");
    assert!(
        artifacts.exists(),
        "spilled body must land in the artifact store"
    );
    let record_file = store
        .join("memory")
        .join(format!("{}.json", written["memory_id"].as_str().unwrap()));
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(record_file).unwrap()).unwrap();
    assert_eq!(on_disk["content"]["kind"], json!("artifact"));

    // …and the spill survives a restart: reads still re-inline.
    drop(first_app);
    let second_app = app_at(store.clone());
    let (status, got) = call(
        &second_app,
        "GET",
        &format!("/memory/{}", written["memory_id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get after restart failed: {got}");
    assert_eq!(got["content"]["kind"], json!("inline"));
    assert_eq!(got["content"]["value"], big_content);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Route-journaled memory events
// --------------------------------------------------------------------- //

/// `first -> second`, appending to a `log` channel (the flight-recorder
/// harness's minimal pipeline).
fn pipeline_app_at(store: PathBuf) -> Router {
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

#[tokio::test]
async fn journaled_memory_write_and_read_land_in_the_runs_journal() {
    let store = temp_store();
    let app = pipeline_app_at(store.clone());

    // A completed run: its journal is persisted at completion.
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
    let thread_id = v["thread_id"].as_str().unwrap().to_string();
    let (status, v) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // A write attributed to the run lands in its journal as a
    // `memory_write` event: idempotent effect, derived effect key, the
    // stored record as output, parented to the journal's head.
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(write_payload(json!({
            "content": {"lesson": "journal me"},
            "run_id": run_id.clone(),
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "journaled write failed: {v}");
    let memory_id = v["memory_id"].as_str().unwrap().to_string();

    // A budgeted read attributed to the run lands as a `memory_read`
    // event: the resolved query + budget as input, the assembly as
    // output.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "user", "id": "user-7"},
            "budget": {"max_tokens": 100000},
            "run_id": run_id.clone(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "journaled read failed: {v}");
    assert_eq!(v["memory_ids"], json!([memory_id.clone()]));

    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    let events = v["events"].as_array().unwrap();
    let write_event = events
        .iter()
        .find(|e| e["kind"] == json!("memory_write"))
        .expect("the memory write is journaled");
    assert_eq!(write_event["effect"], json!("idempotent"));
    assert_eq!(
        write_event["input"]["value"]["effect_key"],
        json!(format!("memory:user:user-7:{memory_id}"))
    );
    assert_eq!(
        write_event["output"]["value"]["memory_id"],
        json!(memory_id)
    );
    let read_event = events
        .iter()
        .find(|e| e["kind"] == json!("memory_read"))
        .expect("the memory read is journaled");
    assert_eq!(read_event["effect"], json!("read_only"));
    assert_eq!(
        read_event["input"]["value"]["query"]["scope"],
        json!({"scope": "user", "id": "user-7"})
    );
    // as_of resolved at read time and journaled with the request.
    assert!(read_event["input"]["value"]["query"]["as_of"].is_string());
    assert_eq!(
        read_event["output"]["value"]["memory_ids"],
        json!([memory_id])
    );
    // Causal parentage: each journaled memory event parents to the
    // journal's head at append time, so the write precedes the read.
    assert_eq!(read_event["parent"], write_event["id"]);

    // A journaled read without a budget is a contract error (400): the
    // journaled request is the resolved query plus its budget.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"run_id": run_id.clone()})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unbudgeted journaled read must 400: {v}"
    );

    // Journaling into another tenant's run is refused silently (the write
    // itself lands; the evidence does not cross the boundary).
    let multi = {
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
            .with_tenant_key("acme", "acme-secret");
        use rusty_agent_runtime::prelude::*;
        let spec = StateSpec::new().channel("log", Reducer::Append);
        let mut builder = GraphBuilder::new();
        builder.add_node("first", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("first")))
        });
        builder.set_entry_point("first");
        let mut registry = GraphRegistry::new();
        registry.register("pipeline", builder.compile().unwrap(), spec);
        router(registry, config)
    };
    let acme = Some(("x-api-key", "acme-secret"));
    let (status, v) = call_as(
        &multi,
        acme,
        "POST",
        "/memory",
        Some(write_payload(json!({
            "content": {"lesson": "not your run"},
            "run_id": run_id.clone(),
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "the write itself lands: {v}");
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK);
    let writes = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == json!("memory_write"))
        .count();
    assert_eq!(writes, 1, "no event journaled across the tenant boundary");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Every test uses a dedicated tenant (`mempg-<uuid>`) whose records are
// scoped under a per-run prefix, so repeated runs against one scratch
// database never interfere; the database itself is throwaway.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("DATABASE_URL").ok()
    }

    /// A Postgres-backed app with a dedicated tenant for this test run.
    fn pg_app(url: &str, tenant: &str) -> Router {
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), temp_store())
            .with_postgres(url.to_string())
            .with_tenant_key(tenant, "pg-secret");
        router(GraphRegistry::new(), config)
    }

    /// Wave-1 exit criterion 1b: memory survives a server restart on the
    /// Postgres backend — records, spill, and query semantics included.
    #[tokio::test]
    #[ignore = "requires a live Postgres (DATABASE_URL)"]
    async fn postgres_memory_survives_a_restart() {
        let Some(url) = pg_url() else {
            eprintln!("DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("mempg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));

        let first = pg_app(&url, &tenant);
        // One inline record and one spilled record (past the inline
        // threshold — the PostgresArtifactStore path).
        let small = write_payload(json!({"key": "timezone", "tags": ["prefs"]}));
        let (status, v) = call_as(&first, auth, "POST", "/memory", Some(small)).await;
        assert_eq!(status, StatusCode::CREATED, "pg write failed: {v}");
        let small_id = v["memory_id"].as_str().unwrap().to_string();
        let small_record = v["record"].clone();

        let big_content = json!({"transcript": "y".repeat(9000)});
        let (status, v) = call_as(
            &first,
            auth,
            "POST",
            "/memory",
            Some(write_payload(json!({
                "kind": "summary",
                "content": big_content.clone(),
                "confidence": 0.9,
            }))),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg spill write failed: {v}");
        let big_id = v["memory_id"].as_str().unwrap().to_string();
        drop(first);

        // Restart: a fresh app over the same database serves both
        // records, self-contained.
        let second = pg_app(&url, &tenant);
        let (status, got) =
            call_as(&second, auth, "GET", &format!("/memory/{small_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "pg get after restart failed: {got}");
        assert_eq!(got, small_record);
        let (status, got) = call_as(&second, auth, "GET", &format!("/memory/{big_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "pg spill get failed: {got}");
        assert_eq!(
            got["content"]["value"], big_content,
            "spilled bodies re-inline on Postgres too"
        );

        // The column-mapped query path: kind + key + confidence filters
        // and the rank-ordered response.
        let (status, v) = call_as(
            &second,
            auth,
            "POST",
            "/memory/query",
            Some(json!({"kinds": ["summary"], "min_confidence": 0.5})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pg query failed: {v}");
        let records = v["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["memory_id"], json!(big_id));
        let (_, v) = call_as(
            &second,
            auth,
            "POST",
            "/memory/query",
            Some(json!({"key": "timezone", "budget": {"max_tokens": 100000}})),
        )
        .await;
        assert_eq!(v["memory_ids"], json!([small_id]));

        // A different tenant sees nothing (the isolation boundary is the
        // tenant column, unchanged).
        let other_tenant = format!("mempg-{}", uuid::Uuid::new_v4());
        let other = pg_app(&url, &other_tenant);
        let (status, _) = call_as(&other, auth, "GET", &format!("/memory/{small_id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
