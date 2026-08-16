//! Knowledge plane integration tests (capability-harness slice #4): the
//! `/knowledge/*` HTTP surface over the default JSON-file backend — the
//! full journey: register → list → query with citations → correct (the old
//! version hidden from retrieval, still addressable as evidence) →
//! retention plan/apply with tombstones → restart persistence → tenant
//! isolation (404-never-403) → result ceilings — plus the governed
//! `search_knowledge` tool adapter (governed backend and in-memory
//! fallback).
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `memory.rs` convention.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::knowledge::{InMemoryContentAddressedStore, KnowledgeBase};
use rusty_agent_runtime::memory::{MemoryScope, ScopeAddress};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::builtins::KnowledgeDocument;
use rusty_agent_runtime::tool::Tool;
use rusty_agent_server::{router, GovernedKnowledgeSearchTool, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-knowledge-test-{}",
        uuid::Uuid::new_v4()
    ))
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
    auth: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = auth {
        builder = builder.header("X-Api-Key", key);
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

/// A registration body: 60 short lines so ingestion chunks it several ways.
fn source_body(marker: &str) -> String {
    (0..60)
        .map(|i| format!("line {i:04} about {marker} retrieval\n"))
        .collect()
}

fn register_payload(id: &str, marker: &str) -> Value {
    json!({
        "source_id": id,
        "kind": "text",
        "title": format!("The {marker} manual"),
        "author": "human:curator",
        "body": source_body(marker),
    })
}

// --------------------------------------------------------------------- //
// Registration
// --------------------------------------------------------------------- //

#[tokio::test]
async fn register_source_validates_and_converges() {
    let (app, store) = app();

    let (status, receipt) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(register_payload("manual", "governed")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    assert_eq!(receipt["source_id"], "manual");
    assert_eq!(receipt["version"], 1);
    assert_eq!(receipt["created"], true);
    assert!(receipt["content_hash"].as_str().unwrap().len() == 64);
    assert!(receipt["chunk_count"].as_u64().unwrap() > 1);

    // Idempotent on content: the same body re-registered converges.
    let (status, again) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(register_payload("manual", "governed")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["created"], false);
    assert_eq!(again["content_hash"], receipt["content_hash"]);

    // A different body under the same id is a correction, not a
    // registration — fail closed.
    let (status, _) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(register_payload("manual", "changed")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Contract violations: malformed id, empty title, out-of-range
    // confidence, confidence required for non-human authors, empty body,
    // already-expired TTL, and a cross-tenant scope (404, never 403).
    for payload in [
        json!({"source_id": "has spaces", "kind": "text", "title": "t", "author": "human:a", "body": "b"}),
        json!({"source_id": "s", "kind": "text", "title": "  ", "author": "human:a", "body": "b"}),
        json!({"source_id": "s", "kind": "text", "title": "t", "author": "human:a", "body": "b", "confidence": 1.5}),
        json!({"source_id": "s", "kind": "text", "title": "t", "author": "agent:a", "body": "b"}),
        json!({"source_id": "s", "kind": "text", "title": "t", "author": "human:a", "body": ""}),
        json!({"source_id": "s", "kind": "text", "title": "t", "author": "human:a", "body": "b",
               "retention": {"policy": "ttl", "expires_at": "2020-01-01T00:00:00Z"}}),
    ] {
        let (status, body) = call(&app, "POST", "/knowledge/sources", Some(payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, _) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(json!({
            "source_id": "s", "kind": "text", "title": "t", "author": "human:a", "body": "b",
            "scope": {"scope": "tenant", "id": "someone-else"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-tenant scope is 404, never 403");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Listing, metadata, chunk fetch
// --------------------------------------------------------------------- //

#[tokio::test]
async fn list_and_fetch_serve_metadata_and_cited_chunks() {
    let (app, store) = app();
    for (id, marker) in [("beta", "storage"), ("alpha", "retrieval")] {
        let (status, _) = call(&app, "POST", "/knowledge/sources", Some(register_payload(id, marker))).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, list) = call(&app, "GET", "/knowledge/sources", None).await;
    assert_eq!(status, StatusCode::OK);
    let sources = list["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["source_id"], "alpha", "sorted by source id");
    assert_eq!(sources[1]["source_id"], "beta");
    assert!(sources[0].get("body").is_none(), "a listing is metadata only");
    assert!(sources[0]["chunk_count"].as_u64().unwrap() > 1);
    assert_eq!(list["tombstones"].as_array().unwrap().len(), 0);

    // One source: metadata plus its chunk inventory.
    let (status, detail) = call(&app, "GET", "/knowledge/sources/alpha", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["source"]["title"], "The retrieval manual");
    assert_eq!(detail["versions"], 1);
    let chunks = detail["chunks"].as_array().unwrap();
    assert!(chunks.len() > 1);
    assert_eq!(chunks[0]["chunk_id"], "alpha#0");

    // The chunk fetch, by bare index and by full id.
    let (status, chunk) = call(&app, "GET", "/knowledge/sources/alpha/chunks/0", None).await;
    assert_eq!(status, StatusCode::OK, "{chunk}");
    let citation = &chunk["citation"];
    assert_eq!(citation["source_id"], "alpha");
    assert_eq!(citation["chunk_id"], "alpha#0");
    assert_eq!(citation["title"], "The retrieval manual");
    assert!(citation["content_address"].as_str().unwrap().len() == 64);
    assert!(chunk["text"].as_str().unwrap().contains("retrieval"));
    let (status, by_full_id) = call(
        &app,
        "GET",
        "/knowledge/sources/alpha/chunks/alpha%230",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_full_id, chunk, "bare index and full id resolve alike");

    // Unknowns are 404.
    for uri in [
        "/knowledge/sources/nope",
        "/knowledge/sources/alpha/chunks/99",
        "/knowledge/sources/alpha/chunks/nope%239",
    ] {
        let (status, _) = call(&app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Query: citations, determinism, ceilings
// --------------------------------------------------------------------- //

#[tokio::test]
async fn query_returns_cited_chunks_within_ceilings() {
    let (app, store) = app();
    let (status, _) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(register_payload("manual", "governed")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, answer) = call(
        &app,
        "POST",
        "/knowledge/query",
        Some(json!({"text": "governed retrieval"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let results = answer["results"].as_array().unwrap();
    assert!(results.len() > 2, "the corpus must exceed the test ceiling");
    for result in results {
        let citation = &result["citation"];
        assert_eq!(citation["source_id"], "manual");
        assert!(result["text"].as_str().unwrap().contains("governed"));
        assert!(result["score"].as_f64().unwrap() > 0.0);
        assert!(citation["byte_end"].as_u64().unwrap() > citation["byte_start"].as_u64().unwrap_or(0));
    }

    // Determinism: an identical query answers byte-identically.
    let (_, again) = call(
        &app,
        "POST",
        "/knowledge/query",
        Some(json!({"text": "governed retrieval"})),
    )
    .await;
    assert_eq!(answer, again);

    // The count ceiling truncates, keeping rank order.
    let (status, bounded) = call(
        &app,
        "POST",
        "/knowledge/query",
        Some(json!({"text": "governed retrieval", "limits": {"max_results": 2, "max_bytes": 65536}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bounded_results = bounded["results"].as_array().unwrap();
    assert_eq!(bounded_results.len(), 2);
    assert_eq!(bounded_results[0], results[0]);
    assert_eq!(bounded_results[1], results[1]);

    // Invalid limits and a termless query are 400.
    for payload in [
        json!({"text": "governed", "limits": {"max_results": 0, "max_bytes": 1024}}),
        json!({"text": "…", "limits": {"max_results": 5, "max_bytes": 1024}}),
    ] {
        let (status, _) = call(&app, "POST", "/knowledge/query", Some(payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Corrections: supersession with evidence
// --------------------------------------------------------------------- //

#[tokio::test]
async fn correction_hides_the_old_version_but_keeps_it_addressable() {
    let (app, store) = app();
    let (status, receipt) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(json!({
            "source_id": "policy",
            "kind": "text",
            "title": "The policy",
            "author": "human:curator",
            "body": "the apple policy stands",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v1_hash = receipt["content_hash"].as_str().unwrap().to_owned();

    let (status, corrected) = call(
        &app,
        "POST",
        "/knowledge/sources/policy/correct",
        Some(json!({"author": "human:editor", "body": "the banana policy stands"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{corrected}");
    assert_eq!(corrected["version"], 2);
    assert_eq!(corrected["supersedes"], json!(v1_hash));

    // The old version stops serving; the term unique to it finds nothing.
    let (_, answer) = call(&app, "POST", "/knowledge/query", Some(json!({"text": "apple"}))).await;
    assert_eq!(answer["results"].as_array().unwrap().len(), 0);
    let (_, answer) = call(&app, "POST", "/knowledge/query", Some(json!({"text": "banana"}))).await;
    assert_eq!(answer["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        answer["results"][0]["citation"]["source_hash"],
        json!(corrected["content_hash"])
    );

    // Evidence: the superseded chunk stays addressable via the version pin.
    let (status, old_chunk) = call(
        &app,
        "GET",
        &format!("/knowledge/sources/policy/chunks/0?version={v1_hash}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{old_chunk}");
    assert_eq!(old_chunk["text"], "the apple policy stands");
    // The source read shows the chain length.
    let (_, detail) = call(&app, "GET", "/knowledge/sources/policy", None).await;
    assert_eq!(detail["versions"], 2);

    // Discipline: unknown source 404, byte-identical correction 400.
    let (status, _) = call(
        &app,
        "POST",
        "/knowledge/sources/nope/correct",
        Some(json!({"author": "human:editor", "body": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        "POST",
        "/knowledge/sources/policy/correct",
        Some(json!({"author": "human:editor", "body": "the banana policy stands"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Retention: dry-run, apply, tombstones
// --------------------------------------------------------------------- //

#[tokio::test]
async fn retention_plans_then_purges_with_tombstones() {
    let (app, store) = app();
    let (status, receipt) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(json!({
            "source_id": "ephemeral",
            "kind": "text",
            "title": "Expiring notes",
            "author": "human:curator",
            "body": source_body("ephemeral"),
            "retention": {"policy": "ttl", "expires_at": "2030-01-01T00:00:00Z"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(register_payload("pinned", "permanent")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Now: nothing is purgeable. At 2031: exactly the expired source.
    let (_, plan_now) = call(&app, "POST", "/knowledge/retention/plan", Some(json!({}))).await;
    assert_eq!(plan_now["entries"].as_array().unwrap().len(), 0);
    let (status, plan) = call(
        &app,
        "POST",
        "/knowledge/retention/plan",
        Some(json!({"as_of": "2031-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = plan["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["source_id"], "ephemeral");
    assert_eq!(entries[0]["source_hash"], receipt["content_hash"]);
    assert!(entries[0]["chunk_count"].as_u64().unwrap() > 1);
    // A plan changes nothing: the source still reads back.
    let (status, _) = call(&app, "GET", "/knowledge/sources/ephemeral", None).await;
    assert_eq!(status, StatusCode::OK);

    // Apply executes the plan exactly; the pinned source is untouched.
    let (status, receipt_apply) = call(
        &app,
        "POST",
        "/knowledge/retention/apply",
        Some(json!({"as_of": "2031-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt_apply}");
    assert_eq!(receipt_apply["plan"], plan, "apply executes the dry-run plan");
    let tombstones = receipt_apply["tombstones"].as_array().unwrap();
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0]["source_id"], "ephemeral");
    assert_eq!(tombstones[0]["reason"], "expired");
    assert_eq!(tombstones[0]["purged_at"], "2031-01-01T00:00:00Z");
    assert_eq!(
        tombstones[0]["purged_hashes"],
        json!([receipt["content_hash"]])
    );

    // The purged source resolves to its tombstone; its chunks are 404; the
    // pinned source keeps serving.
    let (status, purged) = call(&app, "GET", "/knowledge/sources/ephemeral", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(purged["tombstone"]["source_id"], "ephemeral");
    let (status, _) = call(&app, "GET", "/knowledge/sources/ephemeral/chunks/0", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, answer) = call(
        &app,
        "POST",
        "/knowledge/query",
        Some(json!({"text": "permanent"})),
    )
    .await;
    assert!(!answer["results"].as_array().unwrap().is_empty());

    // The listing carries the tombstone; a second sweep is a no-op.
    let (_, list) = call(&app, "GET", "/knowledge/sources", None).await;
    assert_eq!(list["sources"].as_array().unwrap().len(), 1);
    assert_eq!(list["tombstones"].as_array().unwrap().len(), 1);
    let (_, second) = call(
        &app,
        "POST",
        "/knowledge/retention/apply",
        Some(json!({"as_of": "2032-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(second["plan"]["entries"].as_array().unwrap().len(), 0);
    assert_eq!(second["tombstones"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart persistence
// --------------------------------------------------------------------- //

#[tokio::test]
async fn knowledge_survives_a_restart() {
    let store = temp_store();
    let app = app_at(store.clone());
    let (status, receipt) = call(
        &app,
        "POST",
        "/knowledge/sources",
        Some(register_payload("manual", "durable")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let content_hash = receipt["content_hash"].as_str().unwrap().to_owned();
    drop(app);

    // A fresh router over the same store: the boot-rebuilt plane serves
    // the registration, the query, and the chunk fetch.
    let app = app_at(store.clone());
    let (status, detail) = call(&app, "GET", "/knowledge/sources/manual", None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["source"]["content_hash"], json!(content_hash));
    assert!(detail["chunks"].as_array().unwrap().len() > 1);
    let (status, answer) = call(
        &app,
        "POST",
        "/knowledge/query",
        Some(json!({"text": "durable retrieval"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!answer["results"].as_array().unwrap().is_empty());
    let (status, chunk) = call(&app, "GET", "/knowledge/sources/manual/chunks/0", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(chunk["text"].as_str().unwrap().contains("durable"));

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tenants_are_isolated_404_never_403() {
    let (app, store) = multi_tenant_app();
    let (status, _) = call_as(
        &app,
        Some("acme-secret"),
        "POST",
        "/knowledge/sources",
        Some(register_payload("acme-manual", "confidential")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Globex sees nothing: empty query, empty listing, 404 on the source,
    // the chunk, and the correction path — never 403.
    let (_, answer) = call_as(
        &app,
        Some("globex-secret"),
        "POST",
        "/knowledge/query",
        Some(json!({"text": "confidential"})),
    )
    .await;
    assert_eq!(answer["results"].as_array().unwrap().len(), 0);
    let (_, list) = call_as(&app, Some("globex-secret"), "GET", "/knowledge/sources", None).await;
    assert_eq!(list["sources"].as_array().unwrap().len(), 0);
    for (method, uri, body) in [
        ("GET", "/knowledge/sources/acme-manual", None),
        ("GET", "/knowledge/sources/acme-manual/chunks/0", None),
        (
            "POST",
            "/knowledge/sources/acme-manual/correct",
            Some(json!({"author": "human:spy", "body": "x"})),
        ),
    ] {
        let (status, _) = call_as(&app, Some("globex-secret"), method, uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    // Globex's retention sweep cannot see acme's sources.
    let (_, plan) = call_as(
        &app,
        Some("globex-secret"),
        "POST",
        "/knowledge/retention/plan",
        Some(json!({"as_of": "2999-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(plan["entries"].as_array().unwrap().len(), 0);

    // Acme still sees its own source (default scope is the caller's tenant).
    let (_, answer) = call_as(
        &app,
        Some("acme-secret"),
        "POST",
        "/knowledge/query",
        Some(json!({"text": "confidential"})),
    )
    .await;
    assert!(!answer["results"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// The governed search_knowledge adapter
// --------------------------------------------------------------------- //

#[tokio::test]
async fn governed_search_tool_queries_the_plane_and_falls_back() {
    // Governed: a KnowledgeBase over the in-memory core store (the plane's
    // semantics are covered end-to-end over HTTP above; the adapter cares
    // about the tool contract, not the backend).
    let base = KnowledgeBase::new(Arc::new(InMemoryContentAddressedStore::new()));
    let scope = ScopeAddress::new(MemoryScope::Tenant, "default");
    base.register_source(
        rusty_agent_runtime::knowledge::SourceRegistration {
            source_id: "guide".to_owned(),
            scope: scope.clone(),
            kind: rusty_agent_runtime::knowledge::SourceKind::Text,
            title: "The guide".to_owned(),
            author: "human:curator".to_owned(),
            confidence: 1.0,
            retention: rusty_agent_runtime::knowledge::RetentionPolicy::Pinned,
        },
        "the governed tool evidence path",
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let tool = GovernedKnowledgeSearchTool::governed(
        base,
        scope,
        vec![KnowledgeDocument {
            id: "legacy".to_owned(),
            title: "Legacy doc".to_owned(),
            text: "in-memory fallback text".to_owned(),
        }],
    )
    .unwrap();
    assert_eq!(tool.name(), "search_knowledge");
    assert_eq!(tool.effect(), Effect::ReadOnly);

    let answer = tool
        .call(json!({"query": "governed evidence", "limit": 3}))
        .await
        .unwrap();
    let results = answer["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "The guide");
    assert_eq!(
        results[0]["citation"]["source_id"], "guide",
        "the governed result carries its citation"
    );

    // Fallback: without a configured base the adapter is the built-in tool.
    let fallback = GovernedKnowledgeSearchTool::in_memory(vec![KnowledgeDocument {
        id: "legacy".to_owned(),
        title: "Legacy doc".to_owned(),
        text: "in-memory fallback text".to_owned(),
    }])
    .unwrap();
    let answer = fallback.call(json!({"query": "fallback"})).await.unwrap();
    let results = answer["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], "legacy");
    assert!(
        results[0].get("citation").is_none(),
        "the fallback keeps the built-in shape exactly"
    );
}
