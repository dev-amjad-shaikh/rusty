//! The deployment's context policy pins the thread's leading system message
//! as the identity section, and the engine refuses to invent one — a run
//! whose policy demands identity but carries none used to fail mid-run with
//! "a run without it is a wiring bug". The wiring is the deployment's own:
//! it chose the policy, so admission supplies the minimum identity — a
//! charter naming the graph — journaled as message 0 like any other
//! message. An explicit `config.instructions` or a turn already opening
//! with a system message wins; a deployment without the policy leaves
//! runs untouched.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::context::ContextPolicy;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct AnswerModel;

#[async_trait]
impl ChatModel for AnswerModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("42"),
            model: Some("answer-test".into()),
            usage: None,
        })
    }
}

fn app_at(store: PathBuf, policy: bool) -> Router {
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(AnswerModel), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("capable", graph, spec, &tools)
        .unwrap();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    if policy {
        config = config.with_context_policy(ContextPolicy::standard(4_096));
    }
    router(registry, config)
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

fn user_input() -> Value {
    json!({"messages": [{"role": "user", "content": "what is 6 times 7?"}]})
}

fn fresh_store(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rusty-{tag}-{}", uuid::Uuid::new_v4()))
}

async fn thread_and_run(app: &Router, payload: Value) -> (StatusCode, Value) {
    let (status, v) = call(app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    call(
        app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(payload),
    )
    .await
}

/// The accepted run's input is the payload as admitted — message 0 is
/// whatever charter admission put in front of the person's message.
async fn accepted_first_message(app: &Router, terminal: &Value) -> Value {
    let run_id = terminal["run_id"].as_str().unwrap().to_string();
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v["input"]["messages"].as_array().unwrap()[0].clone()
}

#[tokio::test]
async fn a_bare_graph_run_under_the_deployment_policy_hears_a_default_charter() {
    let app = app_at(fresh_store("deployment-charter-default"), true);
    let (status, terminal) = thread_and_run(&app, json!({"input": user_input()})).await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], json!("success"), "{terminal}");
    let first = accepted_first_message(&app, &terminal).await;
    assert_eq!(first["role"], json!("system"), "{first}");
    let content = first["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("`capable`"),
        "the default charter names the graph: {content}"
    );
}

#[tokio::test]
async fn an_explicit_charter_wins_over_the_default() {
    let app = app_at(fresh_store("deployment-charter-explicit"), true);
    let (status, terminal) = thread_and_run(
        &app,
        json!({
            "input": user_input(),
            "config": {"instructions": "You are bespoke."}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], json!("success"), "{terminal}");
    let first = accepted_first_message(&app, &terminal).await;
    assert_eq!(first["content"], json!("You are bespoke."), "{first}");
}

#[tokio::test]
async fn a_turn_opening_with_a_system_message_keeps_it() {
    let app = app_at(fresh_store("deployment-charter-system-first"), true);
    let input = json!({
        "messages": [
            {"role": "system", "content": "Custom charter."},
            {"role": "user", "content": "what is 6 times 7?"},
        ]
    });
    let (status, terminal) = thread_and_run(&app, json!({"input": input})).await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], json!("success"), "{terminal}");
    let first = accepted_first_message(&app, &terminal).await;
    assert_eq!(first["content"], json!("Custom charter."), "{first}");
}

#[tokio::test]
async fn a_deployment_without_the_policy_leaves_runs_unchartered() {
    let app = app_at(fresh_store("deployment-charter-no-policy"), false);
    let (status, terminal) = thread_and_run(&app, json!({"input": user_input()})).await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], json!("success"), "{terminal}");
    let first = accepted_first_message(&app, &terminal).await;
    assert_eq!(
        first["role"],
        json!("user"),
        "no policy, no charter: {first}"
    );
}
