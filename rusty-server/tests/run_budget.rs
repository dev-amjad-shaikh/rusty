//! A per-run budget on the agent stops a run at the step that crosses it,
//! in words, and every terminal says what the run spent. The budget comes
//! from the assistant's intent (`studio_intent.budget`) the way its tools
//! and charter do; the run's terminal carries `spend` on success and on
//! the stop; a zero bound is refused at admission.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::llm::Usage;
use rusty_agent_runtime::tool::builtins::CalculatorTool;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Calls the calculator once, then answers; every call reports 60 tokens.
struct MeteredModel {
    calls: AtomicUsize,
}

#[async_trait]
impl ChatModel for MeteredModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = if n == 0 {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call-1",
                "calculator",
                json!({"operation": "multiply", "left": 6, "right": 7}),
            )])
        } else {
            ChatMessage::assistant("42")
        };
        Ok(ChatResponse {
            message,
            model: Some("metered-test".into()),
            usage: Some(Usage { prompt_tokens: 50, completion_tokens: 10, total_tokens: 60, ..Usage::default() }),
        })
    }
}

fn test_app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-run-budget-{}", uuid::Uuid::new_v4()));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent(Arc::new(MeteredModel { calls: AtomicUsize::new(0) }), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("capable", graph, spec, &tools).unwrap();
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
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, value)
}

async fn create_assistant(app: &Router, id: &str, budget: Value) {
    let (status, value) = call(
        app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Thrifty",
            "graph": "capable",
            "config": {"studio_intent": {"instructions": "Multiply, then answer.", "tools": [{"name": "calculator"}], "budget": budget}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
}

async fn create_thread(app: &Router) -> String {
    let (status, value) = call(app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value["thread_id"].as_str().unwrap().to_string()
}

fn input() -> Value {
    json!({"messages": [{"role": "user", "content": "what is 6 times 7?"}]})
}

#[tokio::test]
async fn a_run_stops_at_the_step_that_crosses_the_agents_budget_and_says_what_it_spent() {
    let (app, store) = test_app();
    // The studio's older intent shape wrote strings; the server reads both.
    create_assistant(&app, "thrifty", json!({"max_tokens": "100"})).await;
    let thread = create_thread(&app).await;
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"input": input(), "assistant_id": "thrifty"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], "error", "{terminal}");
    assert_eq!(terminal["error"], "budget_exhausted", "{terminal}");
    let message = terminal["message"].as_str().unwrap();
    assert!(message.contains("120 tokens spent against a limit of 100"), "{message}");
    assert_eq!(terminal["spend"]["tokens"], 120, "{terminal}");
    assert_eq!(terminal["spend"]["requests"], 2, "{terminal}");
    assert!(terminal["spend"]["cost_usd"].is_null(), "an unpriced model journals no cost: {terminal}");

    // The run declared its budget.
    let run_id = terminal["run_id"].as_str().unwrap();
    let (status, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let declared = events["events"].as_array().unwrap().iter().find(|e| e["kind"] == "run_config_declared").expect("declared");
    assert_eq!(declared.pointer("/output/value/budget/max_tokens"), Some(&json!(100)), "{declared}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_run_under_budget_finishes_with_its_spend_on_the_terminal() {
    let (app, store) = test_app();
    create_assistant(&app, "thrifty", json!({"max_tokens": 1000, "max_cost_usd": 0.5})).await;
    let thread = create_thread(&app).await;
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"input": input(), "assistant_id": "thrifty"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], "success", "{terminal}");
    assert_eq!(terminal["spend"]["tokens"], 120, "{terminal}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_zero_bound_is_refused_at_admission() {
    let (app, store) = test_app();
    create_assistant(&app, "thrifty", json!({})).await;
    let thread = create_thread(&app).await;
    let (status, body) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"input": input(), "assistant_id": "thrifty", "config": {"budget": {"max_tokens": 0}}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("max_tokens` is 0"), "{body}");
    let _ = std::fs::remove_dir_all(store);
}
