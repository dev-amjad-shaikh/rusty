//! A run that asks the world the same question three times stops, and the
//! episode is on the repair ledger a person can read. The stop leaves the
//! thread with an assistant's tool calls and no results; the next turn
//! answers them before the model sees the thread, and that repair is on the
//! ledger too.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::llm::Role;
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL, UNRECORDED_READ_NOTICE};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

type Seen = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
    seen: Seen,
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse { message, model: Some("scripted".into()), usage: None })
    }
}

struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        Ok(args.get("text").cloned().unwrap_or(Value::Null))
    }
}

fn app() -> (Router, PathBuf, Seen) {
    let store = std::env::temp_dir().join(format!("rusty-server-stuck-{}", uuid::Uuid::new_v4()));
    let call = |id: &str| ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "echo", json!({"text": "hi"}))]);
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(
            vec![call("c1"), call("c2"), call("c3"), ChatMessage::assistant("recovered: the earlier call's result was lost, so here is a fresh answer")].into(),
        ),
        seen: Arc::clone(&seen),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Echo);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    // Registered with its tools, the way the product registers a graph: the
    // catalog is what says whether a lost call was a read or a write.
    registry.register_with_tools("react", graph, spec, &tools).unwrap();
    (router(registry, ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())), store, seen)
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

#[tokio::test]
async fn a_run_that_loops_on_one_call_stops_and_is_on_the_repair_ledger() {
    let (app, store, seen) = app();
    let (status, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "loop"}] } })),
    )
    .await;
    assert_eq!(run["status"], "error", "{run}");
    let message = run.to_string();
    assert!(message.contains("no progress"), "{message}");

    let (status, repairs) = call(&app, "GET", "/repairs", None).await;
    assert_eq!(status, StatusCode::OK, "{repairs}");
    let records = repairs["records"]
        .as_array()
        .cloned()
        .or_else(|| repairs.as_array().cloned())
        .unwrap_or_else(|| panic!("repairs shape: {repairs}"));
    let stuck: Vec<&Value> = records
        .iter()
        .filter(|r| r["component"] == "stuck_turn_detector")
        .collect();
    assert_eq!(stuck.len(), 2, "{repairs}");
    let mut outcomes: Vec<&str> = stuck.iter().map(|r| r["outcome"].as_str().unwrap()).collect();
    outcomes.sort_unstable();
    assert_eq!(outcomes, vec!["failed", "repaired"]);

    // The stop left the thread with `c3` unanswered. The next turn answers
    // it before the model reads the thread — echo is a read, so the notice
    // says the result was lost — and the turn succeeds.
    let (_, next) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "are you ok?"}] } })),
    )
    .await;
    assert_eq!(next["status"], "success", "{next}");
    let last_request = seen.lock().unwrap().last().cloned().expect("the model was called");
    let batch = last_request
        .iter()
        .position(|m| m.tool_calls.iter().any(|c| c.id == "c3"))
        .expect("the stopped batch is in the thread");
    let answer = &last_request[batch + 1];
    assert_eq!(answer.role, Role::Tool, "{last_request:?}");
    assert_eq!(answer.tool_call_id.as_deref(), Some("c3"));
    assert_eq!(answer.content.as_deref(), Some(UNRECORDED_READ_NOTICE));
    assert_eq!(last_request.last().unwrap().content.as_deref(), Some("are you ok?"));

    let (_, repairs) = call(&app, "GET", "/repairs", None).await;
    let records = repairs["records"].as_array().cloned().or_else(|| repairs.as_array().cloned()).unwrap();
    let crash: Vec<&Value> = records.iter().filter(|r| r["component"] == "crash_repair").collect();
    assert_eq!(crash.len(), 1, "{repairs}");
    assert_eq!(crash[0]["trigger"]["trigger"], "crash");
    assert!(crash[0]["citations"][0].as_str().unwrap().starts_with("c3:echo:lost"), "{}", crash[0]);
    let _ = std::fs::remove_dir_all(store);
}
