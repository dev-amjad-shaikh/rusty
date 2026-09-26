//! A deployment with a context policy assembles every run's model calls:
//! the run's journal holds the assembled request (the section manifest
//! rides in it), the charter leads it, and the policy is declared in the
//! run's config event. Without a policy the journal holds the raw request,
//! as before.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::context::{ContextPolicy, MANIFEST_MESSAGE_NAME};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse { message, model: Some("scripted".into()), usage: None })
    }
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-context-policy-{}", uuid::Uuid::new_v4()))
}

fn app(policy: Option<ContextPolicy>) -> (Router, PathBuf) {
    let store = temp_store();
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(vec![ChatMessage::assistant("hello back")].into()),
    });
    let graph = create_react_agent(model, ToolRegistry::new()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register("react", graph, spec);
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    if let Some(policy) = policy {
        config = config.with_context_policy(policy);
    }
    (router(registry, config), store)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// One turn on a fresh thread, the way the studio sends it: the charter as
/// the journaled first message, then the person's words. Returns the run's
/// journaled events.
async fn one_turn(app: &Router) -> Vec<Value> {
    let (status, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let input = json!({ "input": { MESSAGES_CHANNEL: [
        {"role": "system", "content": "CHARTER: greet briefly."},
        {"role": "user", "content": "hi"}
    ]}});
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(input)).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (status, body) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["events"].as_array().unwrap().clone()
}

fn model_call_messages(events: &[Value]) -> Vec<Value> {
    let call = events
        .iter()
        .find(|e| e["kind"] == "model_call" && e["node_id"] == "agent")
        .expect("a journaled model call");
    call["input"]["value"]["messages"]
        .as_array()
        .cloned()
        .or_else(|| call["input"]["messages"].as_array().cloned())
        .unwrap_or_else(|| panic!("model call input shape: {}", call["input"]))
}

#[tokio::test]
async fn with_a_policy_the_journaled_call_is_the_assembled_request() {
    let (app, store) = app(Some(ContextPolicy::standard(8_000)));
    let events = one_turn(&app).await;

    let messages = model_call_messages(&events);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "CHARTER: greet briefly.");
    assert!(
        messages.iter().any(|m| m["name"] == MANIFEST_MESSAGE_NAME),
        "the section manifest rides in the journaled request: {messages:?}"
    );
    assert_eq!(
        messages.iter().filter(|m| m["content"] == "CHARTER: greet briefly.").count(),
        1,
        "the charter appears once"
    );

    let declared = events
        .iter()
        .find(|e| e["kind"] == "run_config_declared")
        .expect("the run declares its config");
    let declaration = &declared["output"]["value"];
    let declaration = if declaration.is_null() { &declared["output"] } else { declaration };
    assert_eq!(
        declaration["context_policy"]["schema_version"], "context-policy-v1",
        "the policy is declared evidence: {declaration}"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn without_a_policy_the_journaled_call_is_the_raw_request() {
    let (app, store) = app(None);
    let events = one_turn(&app).await;
    let messages = model_call_messages(&events);
    assert!(!messages.iter().any(|m| m["name"] == MANIFEST_MESSAGE_NAME));
    assert_eq!(messages.len(), 2);
    let _ = std::fs::remove_dir_all(store);
}

/// A builder sets an agent's context — its window and what compaction
/// keeps — on the agent; its runs declare that policy, another agent's
/// runs declare the deployment's, and `/info` says the deployment's
/// numbers so the form can show them as the default.
#[tokio::test]
async fn an_agents_own_context_is_the_policy_its_runs_declare() {
    let (app, store) = app(Some(ContextPolicy::standard(32_000).with_memory_section(2_048)));
    let (status, info) = call(&app, "GET", "/info", None).await;
    assert_eq!(status, StatusCode::OK, "{info}");
    assert_eq!(info["context"]["budget_tokens"], 32_000, "{info}");
    assert_eq!(info["context"]["keep_recent_messages"], 8);
    assert_eq!(info["context"]["memory"], true);

    let (status, made) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "lean", "name": "Lean Triage", "graph": "react", "config": {"studio_intent": {"instructions": "Greet.", "context": {"budget_tokens": "8000", "keep_recent_messages": 3}}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "plain", "name": "Plain", "graph": "react", "config": {"studio_intent": {"instructions": "Greet."}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    async fn declared_for(app: &Router, assistant: &str) -> Value {
        let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
        let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
        let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": assistant, "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "hi"}]}}))).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        let (_, body) = call(app, "GET", &format!("/runs/{}/events", run["run_id"].as_str().unwrap()), None).await;
        let declared = body["events"].as_array().unwrap().iter().find(|e| e["kind"] == "run_config_declared").cloned().expect("declared");
        declared["output"]["value"]["context_policy"].clone()
    }
    let lean = declared_for(&app, "lean").await;
    assert_eq!(lean["budget"]["max_tokens"], 8_000, "the agent's window: {lean}");
    assert_eq!(lean["compaction"]["keep_recent_messages"], 3, "{lean}");
    assert!(lean["memory"].is_object(), "the deployment's memory section stays: {lean}");
    let plain = declared_for(&app, "plain").await;
    assert_eq!(plain["budget"]["max_tokens"], 32_000, "the deployment's: {plain}");
    assert_eq!(plain["compaction"]["keep_recent_messages"], 8);
    let _ = std::fs::remove_dir_all(store);
}
