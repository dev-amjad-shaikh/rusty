//! Resolved capability sets at server admission: run-level tool
//! allowlists, inline capability-set composition, assistant-version
//! defaults, and the replay guard.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::builtins::{CalculatorTool, TextInspectorTool};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Test graphs
// --------------------------------------------------------------------- //

/// A model that attempts exactly one tool call on its first invocation
/// (when configured with a target) and answers "done" afterwards.
struct ScriptedModel {
    attempted: Option<String>,
    calls: AtomicUsize,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(tool) = &self.attempted {
                let args = match tool.as_str() {
                    "calculator" => json!({"operation": "multiply", "left": 6, "right": 7}),
                    _ => json!({"text": "hello world"}),
                };
                return Ok(ChatResponse {
                    message: ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "call-1", tool, args,
                    )]),
                    model: Some("scripted-test".into()),
                    usage: None,
                });
            }
        }
        Ok(ChatResponse {
            message: ChatMessage::assistant("done"),
            model: Some("scripted-test".into()),
            usage: None,
        })
    }
}

/// A ReAct agent over the built-in calculator and text inspector,
/// registered with its exact executable catalog.
fn capable_graph(attempted: Option<&str>) -> (Graph, StateSpec, ToolRegistry) {
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    tools.register(TextInspectorTool);
    let model = ScriptedModel {
        attempted: attempted.map(str::to_owned),
        calls: AtomicUsize::new(0),
    };
    let graph = create_react_agent(Arc::new(model), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    (graph, spec, tools)
}

/// A tool-owning graph whose nodes never call the model: the replay
/// endpoint can re-drive it (no servable effects), which exercises the
/// capability replay guard.
fn quiet_graph() -> (Graph, StateSpec, ToolRegistry) {
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("work", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("done")))
    });
    builder.set_entry_point("work");
    (builder.compile().unwrap(), spec, tools)
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-capability-set-test-{}", uuid::Uuid::new_v4()))
}

fn test_app(attempted: Option<&str>) -> (Router, PathBuf) {
    let store = temp_store();
    let (capable, capable_spec, capable_tools) = capable_graph(attempted);
    let (quiet, quiet_spec, quiet_tools) = quiet_graph();

    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("capable", capable, capable_spec, &capable_tools)
        .unwrap();
    registry
        .register_with_tools("quiet", quiet, quiet_spec, &quiet_tools)
        .unwrap();

    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    (router(registry, config), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn create_thread(app: &Router, graph: &str) -> String {
    let (status, value) = call(app, "POST", "/threads", Some(json!({"graph": graph}))).await;
    assert_eq!(status, StatusCode::CREATED, "thread creation failed: {value}");
    value["thread_id"].as_str().unwrap().to_string()
}

/// A blocking run; returns the terminal JSON.
async fn run_wait(app: &Router, thread_id: &str, payload: Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(payload),
    )
    .await
}

/// The run's final `role: "tool"` message contents, in order.
fn tool_messages(terminal: &Value) -> Vec<String> {
    terminal["output"]["messages"]
        .as_array()
        .expect("terminal output carries messages")
        .iter()
        .filter(|message| message["role"] == json!("tool"))
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect()
}

fn input() -> Value {
    json!({"messages": [{"role": "user", "content": "hi"}]})
}

// --------------------------------------------------------------------- //
// Run-level tool selection
// --------------------------------------------------------------------- //

#[tokio::test]
async fn subset_run_executes_the_allowed_tool_and_blocks_the_rest() {
    // Allowed: the calculator answers through the narrowed registry.
    let (app, store) = test_app(Some("calculator"));
    let thread = create_thread(&app, "capable").await;
    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "config": {"tool_allowlist": ["calculator"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert_eq!(terminal["status"], json!("success"));
    assert!(
        tool_messages(&terminal).iter().any(|m| m.contains("42")),
        "expected the calculator result, got: {terminal}"
    );
    let _ = std::fs::remove_dir_all(store);

    // Blocked: the model's call to a tool outside the subset returns a
    // structured tool message; the run itself completes.
    let (app, store) = test_app(Some("inspect_text"));
    let thread = create_thread(&app, "capable").await;
    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "config": {"tool_allowlist": ["calculator"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert_eq!(terminal["status"], json!("success"));
    let messages = tool_messages(&terminal);
    assert_eq!(messages.len(), 1, "expected one tool message: {terminal}");
    assert!(
        messages[0].contains("unknown tool `inspect_text`"),
        "expected a blocked-tool message, got: {}",
        messages[0]
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn empty_allowlist_is_a_tool_free_run() {
    let (app, store) = test_app(Some("calculator"));
    let thread = create_thread(&app, "capable").await;
    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "config": {"tool_allowlist": []}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert_eq!(terminal["status"], json!("success"));
    let messages = tool_messages(&terminal);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("unknown tool `calculator`"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn absent_allowlist_preserves_the_full_registry() {
    let (app, store) = test_app(Some("calculator"));
    let thread = create_thread(&app, "capable").await;
    let (status, terminal) = run_wait(&app, &thread, json!({"input": input()})).await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert_eq!(terminal["status"], json!("success"));
    assert!(tool_messages(&terminal).iter().any(|m| m.contains("42")));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn unknown_and_duplicate_members_fail_admission_on_every_endpoint() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "capable").await;

    for suffix in ["runs", "runs/wait", "runs/stream"] {
        let (status, value) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/{suffix}"),
            Some(json!({"input": input(), "config": {"tool_allowlist": ["web_search"]}})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{suffix}: {value}");
        assert_eq!(value["error"], json!("bad_request"), "{suffix}: {value}");
        assert!(
            value["message"].as_str().unwrap().contains("web_search"),
            "{suffix}: {value}"
        );

        let (status, value) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/{suffix}"),
            Some(
                json!({"input": input(), "config": {"tool_allowlist": ["calculator", "calculator"]}}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{suffix}: {value}");
        assert!(
            value["message"].as_str().unwrap().contains("duplicate"),
            "{suffix}: {value}"
        );
    }
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Inline capability sets
// --------------------------------------------------------------------- //

#[tokio::test]
async fn capability_set_composes_validates_and_excludes_a_bare_allowlist() {
    let (app, store) = test_app(Some("calculator"));
    let thread = create_thread(&app, "capable").await;

    // A declared set resolves like an allowlist; opaque skill/connector
    // references ride along under the set's content address.
    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({
            "input": input(),
            "config": {"capability_set": {
                "tools": ["calculator"],
                "skills": ["research-pack@1.2.0"],
                "connectors": ["search@prod"]
            }}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert_eq!(terminal["status"], json!("success"));
    assert!(tool_messages(&terminal).iter().any(|m| m.contains("42")));

    // Unknown members fail closed at admission.
    let (status, value) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "config": {"capability_set": {"tools": ["web_search"]}}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert!(value["message"].as_str().unwrap().contains("web_search"));

    // The two selection shapes are mutually exclusive.
    let (status, value) = run_wait(
        &app,
        &thread,
        json!({
            "input": input(),
            "config": {
                "tool_allowlist": ["calculator"],
                "capability_set": {"tools": ["calculator"]}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert!(value["message"]
        .as_str()
        .unwrap()
        .contains("name exactly one"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Assistant-version defaults
// --------------------------------------------------------------------- //

async fn create_assistant(app: &Router, id: &str, config: Value) {
    let (status, value) = call(
        app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Evidence scout",
            "graph": "capable",
            "config": config,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "assistant creation failed: {value}");
}

#[tokio::test]
async fn assistant_tool_selection_is_the_default_allowlist() {
    let (app, store) = test_app(Some("inspect_text"));
    create_assistant(
        &app,
        "scoped",
        json!({"studio_intent": {
            "format": "rusty.agent-intent/v3",
            "model": "model-v1",
            "tools": [{"name": "calculator", "effect": "pure"}]
        }}),
    )
    .await;
    let thread = create_thread(&app, "capable").await;

    // No run-level selection: the assistant's reviewed tools apply, so the
    // model's attempt at the unlisted inspector is blocked.
    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "assistant_id": "scoped"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    let messages = tool_messages(&terminal);
    assert_eq!(messages.len(), 1, "expected one tool message: {terminal}");
    assert!(messages[0].contains("unknown tool `inspect_text`"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn run_level_allowlist_overrides_the_assistant_default() {
    let (app, store) = test_app(Some("inspect_text"));
    create_assistant(
        &app,
        "scoped",
        json!({"studio_intent": {
            "format": "rusty.agent-intent/v3",
            "tools": [{"name": "calculator", "effect": "pure"}]
        }}),
    )
    .await;
    let thread = create_thread(&app, "capable").await;

    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({
            "input": input(),
            "assistant_id": "scoped",
            "config": {"tool_allowlist": ["inspect_text"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert!(
        tool_messages(&terminal)
            .iter()
            .any(|m| m.contains("\"words\":2")),
        "expected the inspector result, got: {terminal}"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn stale_assistant_default_fails_closed_at_admission() {
    let (app, store) = test_app(None);
    // A version stored against an older catalog names a tool the current
    // graph does not register; the record stays intact but its runs fail
    // closed rather than silently dropping the requirement.
    create_assistant(
        &app,
        "stale",
        json!({"studio_intent": {
            "format": "rusty.agent-intent/v3",
            "tools": [{"name": "web_search", "effect": "read_only"}]
        }}),
    )
    .await;
    let thread = create_thread(&app, "capable").await;

    let (status, value) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "assistant_id": "stale"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert!(value["message"].as_str().unwrap().contains("web_search"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn legacy_assistant_without_intent_stays_unrestricted() {
    let (app, store) = test_app(Some("calculator"));
    create_assistant(&app, "legacy", json!({"recursion_limit": 8})).await;
    let thread = create_thread(&app, "capable").await;

    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({"input": input(), "assistant_id": "legacy"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert!(tool_messages(&terminal).iter().any(|m| m.contains("42")));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Replay binding
// --------------------------------------------------------------------- //

#[tokio::test]
async fn replay_reresolves_the_pinned_selection_against_the_catalog() {
    let (app, store) = test_app(None);
    let thread = create_thread(&app, "quiet").await;
    let (status, terminal) = run_wait(
        &app,
        &thread,
        json!({"input": {}, "config": {"tool_allowlist": ["calculator"]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "run failed: {terminal}");
    assert_eq!(terminal["status"], json!("success"));
    let run_id = terminal["run_id"].as_str().unwrap().to_string();

    // The selection still resolves against the unchanged catalog, so the
    // replay guard passes and the journal verifies.
    let (status, value) = call(&app, "POST", "/runs/replay", Some(json!({"run_id": run_id}))).await;
    assert_eq!(status, StatusCode::OK, "replay failed: {value}");
    assert_eq!(value["verified"], json!(true));
    let _ = std::fs::remove_dir_all(store);
}
