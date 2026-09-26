//! The interop bridge integration tests (R0.9 Rusty Capsules, wave 4):
//! the MCP tool surface (`POST /mcp`), the A2A task surface
//! (`/.well-known/agent-card.json`, `POST /a2a`), and the capsule
//! component-blob upload — all over the default JSON-file backend, driven
//! in-process via `tower::ServiceExt::oneshot` (the `capsules.rs`
//! convention).
//!
//! What these tests pin: the tool/card surfaces are *derived* from the
//! registry (never static), the JSON-RPC envelope semantics (errors in
//! the body, notifications unanswered), the run/cancel wiring behind
//! `tools/call`, the durable-task mapping behind A2A messages (tenant
//! isolation and quota included), and the blob route's digest discipline.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::record::sha256_hex;
use rusty_agent_server::{router, GraphRegistry, ServerConfig, TaskQuota};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness (the capsules.rs convention)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-bridges-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The graphs these tests drive. `pipeline` completes immediately (two
// appending nodes); `slow` sleeps past the cancellation window, so a
/// mid-run `notifications/cancelled` has something to catch. The channel
/// mix (append + deep-merge) is what the schema-derivation asserts read.
fn registry() -> GraphRegistry {
    use rusty_agent_runtime::prelude::*;

    let spec = StateSpec::new()
        .channel("log", Reducer::Append)
        .channel("scratch", Reducer::DeepMerge);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("first")))
    });
    builder.add_node("second", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("second")))
    });
    builder.set_entry_point("first");
    builder.add_edge("first", "second");
    let pipeline = builder.compile().unwrap();

    let slow_spec = StateSpec::new().channel("log", Reducer::Append);
    let mut slow_builder = GraphBuilder::new();
    slow_builder.add_node("slow", |_ctx: NodeContext| async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(NodeOutput::update("log", json!("slow")))
    });
    slow_builder.add_node("after", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("after")))
    });
    slow_builder.set_entry_point("slow");
    slow_builder.add_edge("slow", "after");
    // Two super-steps: run-level cancellation is observed at a checkpoint
    // boundary, so the cancellable window is between `slow` and `after`.
    let slow = slow_builder.compile().unwrap();

    let mut registry = GraphRegistry::new();
    registry.register("pipeline", pipeline, spec);
    registry.register("slow", slow, slow_spec);
    registry
}

/// An app over a fresh store with the config customized by `configure`.
fn app_with(configure: impl FnOnce(ServerConfig) -> ServerConfig) -> (Router, PathBuf) {
    let store = temp_store();
    let config = configure(ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        store.clone(),
    ));
    (router(registry(), config), store)
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    app_with(|config| config)
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
    let (status, text) = call_raw(app, auth, method, uri, body, None).await;
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Send a request; returns `(status, raw-body-text)` — the SSE tests read
/// the body as text, never as one JSON document.
async fn call_raw(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
    accept: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = auth {
        builder = builder.header(k, v);
    }
    if let Some(accept) = accept {
        builder = builder.header("accept", accept);
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
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// The JSON-RPC envelope for one request.
fn rpc(id: Value, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Parse an SSE body into its `data:` payloads (JSON-decoded), in order.
fn sse_data_events(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .flat_map(|chunk| {
            chunk.lines().filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|data| serde_json::from_str(data.trim()).unwrap_or(Value::Null))
            })
        })
        .collect()
}

// --------------------------------------------------------------------- //
// MCP bridge
// --------------------------------------------------------------------- //

#[tokio::test]
async fn mcp_initialize_and_tools_list_derive_schemas() {
    let (app, store) = app();

    let (status, v) = call(
        &app,
        "POST",
        "/mcp",
        Some(rpc(json!(1), "initialize", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "initialize failed: {v}");
    assert_eq!(v["result"]["protocolVersion"], json!("2025-03-26"));
    assert_eq!(v["result"]["serverInfo"]["name"], json!("rusty-server"));
    assert_eq!(
        v["result"]["capabilities"]["tools"]["listChanged"],
        json!(false)
    );

    let (status, v) = call(
        &app,
        "POST",
        "/mcp",
        Some(rpc(json!(2), "tools/list", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/list failed: {v}");
    let tools = v["result"]["tools"].as_array().unwrap();
    // Registry order is sorted: pipeline, slow.
    assert_eq!(tools.len(), 2, "one tool per registered graph: {v}");
    assert_eq!(tools[0]["name"], json!("pipeline"));
    assert_eq!(tools[1]["name"], json!("slow"));
    // The input schema is the graph's state spec, shaped by reducers:
    // append channels are arrays, deep-merge channels objects.
    let schema = &tools[0]["inputSchema"];
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["properties"]["log"], json!({ "type": "array" }));
    assert_eq!(schema["properties"]["scratch"], json!({ "type": "object" }));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn mcp_tools_list_grows_with_the_registry() {
    // A registry with one graph yields one tool — the list is derived,
    // never a static table that could drift.
    let store = temp_store();
    use rusty_agent_runtime::prelude::*;
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("only", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("only")))
    });
    builder.set_entry_point("only");
    let only = builder.compile().unwrap();
    let mut registry = GraphRegistry::new();
    registry.register("only", only, spec);
    let app = router(
        registry,
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
    );

    let (status, v) = call(
        &app,
        "POST",
        "/mcp",
        Some(rpc(json!(1), "tools/list", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/list failed: {v}");
    let tools = v["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "derived from this registry alone: {v}");
    assert_eq!(tools[0]["name"], json!("only"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn mcp_tools_call_json_roundtrip() {
    let (app, store) = app();

    let (status, v) = call(
        &app,
        "POST",
        "/mcp",
        Some(rpc(
            json!("call-1"),
            "tools/call",
            json!({
                "name": "pipeline",
                "arguments": { "log": ["seed"] },
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/call failed: {v}");
    let result = &v["result"];
    assert!(
        result.get("isError").is_none(),
        "a successful run is not an error result: {v}"
    );
    assert_eq!(
        result["structuredContent"]["log"],
        json!(["seed", "first", "second"]),
        "the graph ran with the arguments as its input state: {v}"
    );
    assert_eq!(result["content"][0]["type"], json!("text"));
    assert_eq!(v["id"], json!("call-1"));

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn mcp_tools_call_sse_streams_progress_then_result() {
    let (app, store) = app();

    let (status, text) = call_raw(
        &app,
        None,
        "POST",
        "/mcp",
        Some(rpc(
            json!("call-2"),
            "tools/call",
            json!({
                "name": "pipeline",
                "arguments": {},
                "_meta": { "progressToken": "tok-1" },
            }),
        )),
        Some("text/event-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "SSE tools/call failed: {text}");
    let events = sse_data_events(&text);
    assert!(events.len() >= 2, "progress frames plus the result: {text}");

    // Every event but the last is a progress notification naming the
    // caller's token; the last is the JSON-RPC response.
    for event in &events[..events.len() - 1] {
        assert_eq!(event["method"], json!("notifications/progress"), "{text}");
        assert_eq!(event["params"]["progressToken"], json!("tok-1"), "{text}");
    }
    let last = events.last().unwrap();
    assert_eq!(last["id"], json!("call-2"), "{text}");
    assert_eq!(
        last["result"]["structuredContent"]["log"],
        json!(["first", "second"]),
        "{text}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn mcp_cancel_notification_cancels_the_run() {
    let (app, store) = app();

    // The call runs in the background: `slow`'s first node sleeps 300ms,
    // and the cancellation notification lands inside that window — the
    // run stops at the boundary before `after` ever executes.
    let request_app = app.clone();
    let request = tokio::spawn(async move {
        call_raw(
            &request_app,
            None,
            "POST",
            "/mcp",
            Some(rpc(
                json!("call-9"),
                "tools/call",
                json!({
                    "name": "slow",
                    "arguments": {},
                    "_meta": { "progressToken": "tok-9" },
                }),
            )),
            Some("text/event-stream"),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, _v) = call(
        &app,
        "POST",
        "/mcp",
        Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": "call-9" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "notifications answer 202");

    let (status, text) = request.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    let events = sse_data_events(&text);
    let last = events.last().unwrap();
    assert_eq!(last["result"]["isError"], json!(true), "{text}");
    assert!(
        last["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cancelled"),
        "the terminal answer carries the cancellation: {text}"
    );

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn mcp_error_envelopes() {
    let (app, store) = app();

    // Unknown tool name: -32602 (the name is a parameter).
    let (_status, v) = call(
        &app,
        "POST",
        "/mcp",
        Some(rpc(
            json!(1),
            "tools/call",
            json!({ "name": "nope", "arguments": {} }),
        )),
    )
    .await;
    assert_eq!(v["error"]["code"], json!(-32602), "{v}");

    // Unknown method: -32601.
    let (_status, v) = call(
        &app,
        "POST",
        "/mcp",
        Some(rpc(json!(2), "resources/list", json!({}))),
    )
    .await;
    assert_eq!(v["error"]["code"], json!(-32601), "{v}");

    // Malformed JSON: -32700.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], json!(-32700), "{v}");

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// A2A bridge
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a2a_agent_card_is_derived_and_deterministic() {
    let (app, store) = app();

    let (status, card) = call(&app, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, StatusCode::OK, "agent card failed: {card}");
    assert_eq!(
        card,
        json!({
            "name": "rusty-server",
            "description": "Rusty graph runtime — every registered graph is an A2A skill",
            "protocolVersion": "0.3.0",
            "version": env!("CARGO_PKG_VERSION"),
            "url": "/a2a",
            "capabilities": { "streaming": true, "pushNotifications": false },
            "defaultInputModes": ["application/json"],
            "defaultOutputModes": ["application/json"],
            "skills": [
                {
                    "id": "pipeline",
                    "name": "pipeline",
                    "description": "Rusty graph `pipeline` (channels: log, scratch)",
                    "tags": ["log", "scratch"],
                    "inputModes": ["application/json"],
                    "outputModes": ["application/json"],
                },
                {
                    "id": "slow",
                    "name": "slow",
                    "description": "Rusty graph `slow` (channels: log)",
                    "tags": ["log"],
                    "inputModes": ["application/json"],
                    "outputModes": ["application/json"],
                },
            ],
            "provider": { "organization": "rusty" },
        }),
        "the golden card shape: {card}"
    );

    // Deterministic: a second read is byte-identical.
    let (_s, again) = call(&app, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(card, again);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a2a_message_send_get_cancel_lifecycle() {
    let (app, store) = app();

    let message = json!({
        "role": "user",
        "messageId": "msg-1",
        "contextId": "ctx-1",
        "parts": [{ "kind": "text", "text": "hello" }],
    });
    let (status, v) = call(
        &app,
        "POST",
        "/a2a",
        Some(rpc(json!(1), "message/send", json!({ "message": message }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "message/send failed: {v}");
    let task = &v["result"];
    let task_id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["contextId"], json!("ctx-1"));
    assert_eq!(task["status"]["state"], json!("submitted"));

    // Idempotent redelivery: the same messageId names the same task.
    let (_s, v) = call(
        &app,
        "POST",
        "/a2a",
        Some(rpc(json!(2), "message/send", json!({ "message": message }))),
    )
    .await;
    assert_eq!(v["result"]["id"], json!(task_id), "dedup on messageId: {v}");

    // tasks/get: a plain message stays submitted (queued for external
    // workers) — the mapping, not execution, is the bridge's contract here.
    let (_s, v) = call(
        &app,
        "POST",
        "/a2a",
        Some(rpc(json!(3), "tasks/get", json!({ "id": task_id }))),
    )
    .await;
    assert_eq!(v["result"]["status"]["state"], json!("submitted"), "{v}");

    // tasks/cancel → canceled; a second cancel is TASK_NOT_CANCELABLE.
    let (_s, v) = call(
        &app,
        "POST",
        "/a2a",
        Some(rpc(json!(4), "tasks/cancel", json!({ "id": task_id }))),
    )
    .await;
    assert_eq!(v["result"]["status"]["state"], json!("canceled"), "{v}");
    let (_s, v) = call(
        &app,
        "POST",
        "/a2a",
        Some(rpc(json!(5), "tasks/cancel", json!({ "id": task_id }))),
    )
    .await;
    assert_eq!(v["error"]["code"], json!(-32002), "{v}");

    // Unknown task: TASK_NOT_FOUND.
    let (_s, v) = call(
        &app,
        "POST",
        "/a2a",
        Some(rpc(json!(6), "tasks/get", json!({ "id": "nope" }))),
    )
    .await;
    assert_eq!(v["error"]["code"], json!(-32001), "{v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a2a_tasks_are_tenant_isolated() {
    let (app, store) = app_with(|config| {
        config
            .with_tenant_key("alpha", "alpha-key")
            .with_tenant_key("beta", "beta-key")
    });

    let message = json!({
        "role": "user",
        "messageId": "msg-tenant",
        "contextId": "ctx-tenant",
        "parts": [{ "kind": "text", "text": "hello" }],
    });
    let (status, v) = call_as(
        &app,
        Some(("x-api-key", "alpha-key")),
        "POST",
        "/a2a",
        Some(rpc(json!(1), "message/send", json!({ "message": message }))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "message/send failed: {v}");
    let task_id = v["result"]["id"].as_str().unwrap().to_string();

    // Beta cannot see alpha's task — not-found, the same posture as the
    // native surface (one tenant cannot probe another's resources).
    let (_s, v) = call_as(
        &app,
        Some(("x-api-key", "beta-key")),
        "POST",
        "/a2a",
        Some(rpc(json!(2), "tasks/get", json!({ "id": task_id }))),
    )
    .await;
    assert_eq!(v["error"]["code"], json!(-32001), "{v}");

    // Alpha sees it.
    let (_s, v) = call_as(
        &app,
        Some(("x-api-key", "alpha-key")),
        "POST",
        "/a2a",
        Some(rpc(json!(3), "tasks/get", json!({ "id": task_id }))),
    )
    .await;
    assert_eq!(v["result"]["id"], json!(task_id), "{v}");

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a2a_submission_respects_the_task_quota() {
    let (app, store) = app_with(|config| {
        config.with_task_quota(TaskQuota {
            max_queued: Some(1),
            ..TaskQuota::default()
        })
    });

    let send = |n: u64| {
        rpc(
            json!(n),
            "message/send",
            json!({
                "message": {
                    "role": "user",
                    "messageId": format!("msg-{n}"),
                    "contextId": "ctx-quota",
                    "parts": [{ "kind": "text", "text": "hello" }],
                },
            }),
        )
    };
    let (status, v) = call(&app, "POST", "/a2a", Some(send(1))).await;
    assert_eq!(status, StatusCode::OK, "first send failed: {v}");
    assert!(v.get("error").is_none(), "{v}");

    // The bridge is not a quota bypass: the second submission hits the
    // same gate `POST /tasks` would, surfaced as a JSON-RPC error.
    let (_s, v) = call(&app, "POST", "/a2a", Some(send(2))).await;
    assert!(v.get("error").is_some(), "over-quota send must fail: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("quota_exceeded"),
        "the quota gate's own verdict: {v}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Capsule blob upload
// --------------------------------------------------------------------- //

/// A minimal valid manifest naming `bytes` as its artifact (the capsule
/// registry tests cover registration itself; here the manifest exists to
/// pin a build digest).
fn blob_manifest(name: &str, version: &str, bytes: &[u8]) -> Value {
    use rusty_agent_runtime::capsule::{
        CapsuleIdentity, CapsuleInterface, CapsuleManifest, WORLD_V1,
    };
    use rusty_agent_runtime::record::Effect;
    use std::collections::BTreeSet;

    let manifest = CapsuleManifest {
        identity: CapsuleIdentity {
            name: name.into(),
            description: None,
        },
        version: version.into(),
        build_digest: sha256_hex(bytes),
        interface: CapsuleInterface {
            world: WORLD_V1.into(),
            input_schema: None,
            output_schema: None,
        },
        effects: BTreeSet::from([Effect::ReadOnly]),
        capabilities: BTreeSet::new(),
        budget: Default::default(),
    };
    serde_json::to_value(manifest).unwrap()
}

#[tokio::test]
async fn capsule_blob_upload_verifies_the_digest() {
    let (app, store) = app();
    let bytes = b"fake-component-bytes".as_slice();

    // Register the manifest, then upload exactly the bytes it names.
    let (status, v) = call(
        &app,
        "POST",
        "/capsules",
        Some(json!({ "manifest": blob_manifest("probe", "1.0.0", bytes) })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    let capsule_id = v["capsule_id"].as_str().unwrap().to_string();

    let put = async |body: &[u8]| {
        let request = Request::builder()
            .method("PUT")
            .uri(format!("/capsules/{capsule_id}/blob"))
            .body(Body::from(body.to_vec()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let raw: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        (status, value)
    };

    let (status, v) = put(bytes).await;
    assert_eq!(status, StatusCode::CREATED, "blob upload failed: {v}");
    assert_eq!(v["sha256"], json!(sha256_hex(bytes)));
    assert_eq!(v["bytes"], json!(bytes.len()));

    // A converged re-upload (identical bytes) is the idempotent create.
    let (status, v) = put(bytes).await;
    assert_eq!(status, StatusCode::CREATED, "re-upload must converge: {v}");

    // Bytes the manifest does not name: 422 (the digest checkpoint).
    let (status, _v) = put(b"different-bytes").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // An unknown capsule: 404.
    let request = Request::builder()
        .method("PUT")
        .uri("/capsules/nope/blob")
        .body(Body::from(bytes.to_vec()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Postgres backend (live database required)
//
// Gated on `RUSTY_TEST_DATABASE_URL`; unset skips cleanly so the suite is
// green without a database (the capsules.rs convention). Every test uses
// a dedicated tenant, so repeated runs against one scratch database never
// interfere.
// --------------------------------------------------------------------- //

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg_url() -> Option<String> {
        std::env::var("RUSTY_TEST_DATABASE_URL").ok()
    }

    /// The blob store on Postgres: upload, converge on identical bytes.
    /// The file backend's semantics, exact over `BYTEA` — the executor
    /// path (`capsules_release.rs`) reads blobs back through the same SQL.
    #[tokio::test]
    async fn postgres_blob_upload_roundtrip() {
        let Some(url) = pg_url() else {
            eprintln!("RUSTY_TEST_DATABASE_URL unset; skipping");
            return;
        };
        let tenant = format!("bridgespg-{}", uuid::Uuid::new_v4());
        let auth = Some(("x-api-key", "pg-secret"));
        let (app, store) = app_with(|config| {
            config
                .with_postgres(url)
                .with_tenant_key(tenant, "pg-secret")
        });
        let bytes = b"pg-component-bytes".as_slice();

        let (status, v) = call_as(
            &app,
            auth,
            "POST",
            "/capsules",
            Some(json!({ "manifest": blob_manifest("pg-blob", "1.0.0", bytes) })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pg register failed: {v}");
        let capsule_id = v["capsule_id"].as_str().unwrap().to_string();

        let put = async |body: &[u8]| {
            let mut builder = Request::builder()
                .method("PUT")
                .uri(format!("/capsules/{capsule_id}/blob"));
            if let Some((k, v)) = auth {
                builder = builder.header(k, v);
            }
            let response = app
                .clone()
                .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let raw: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null),
            )
        };

        let (status, v) = put(bytes).await;
        assert_eq!(status, StatusCode::CREATED, "pg blob upload failed: {v}");
        assert_eq!(v["sha256"], json!(sha256_hex(bytes)));

        // Identical bytes converge (the insert's ON CONFLICT plus the
        // digest comparison — upload retries are safe).
        let (status, _v) = put(bytes).await;
        assert_eq!(status, StatusCode::CREATED, "pg re-upload must converge");

        let _ = std::fs::remove_dir_all(store);
    }
}
