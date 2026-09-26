//! Failures the loop can act on, through the real ReAct path: a call that
//! failed its bound is refused from the thread instead of dispatched; a
//! write whose answer was lost is not sent again until something was read.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolFailure, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Scripted(Mutex<VecDeque<ChatMessage>>);
#[async_trait::async_trait]
impl ChatModel for Scripted {
    async fn chat(&self, _m: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        let message = self.0.lock().unwrap().pop_front().ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse { message, model: Some("scripted".into()), usage: None })
    }
}

/// A read that always fails the same way; counts real dispatches.
struct BrokenRead(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for BrokenRead {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "A lookup that the system refuses."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ToolFailure::new("invalid_arguments", "lookup", "HTTP 400: Invalid table incdent", true, false, "fix the arguments").into_error())
    }
}

/// A write whose first answer is lost; counts real dispatches.
struct LossyPost(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for LossyPost {
    fn name(&self) -> &str {
        "post"
    }
    fn description(&self) -> &str {
        "Posts; the first answer is lost on the wire."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }
    fn carries_approval(&self, _call: &ToolCall) -> bool {
        true
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Err(ToolFailure::new("unknown_outcome", "post", "the request was sent and the answer was lost", true, false, "read back first").into_error())
        } else {
            Ok(json!({"posted": args}))
        }
    }
}

struct ReadBack;
#[async_trait::async_trait]
impl Tool for ReadBack {
    fn name(&self) -> &str {
        "read_posts"
    }
    fn description(&self) -> &str {
        "Reads what was posted."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!({"posts": []}))
    }
}

fn app(script: Vec<ChatMessage>) -> (Router, std::path::PathBuf, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let store = std::env::temp_dir().join(format!("rusty-server-tool-failures-{}", uuid::Uuid::new_v4()));
    let lookups = Arc::new(AtomicUsize::new(0));
    let posts = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(BrokenRead(Arc::clone(&lookups)));
    tools.register(LossyPost(Arc::clone(&posts)));
    tools.register(ReadBack);
    let graph = create_react_agent(Arc::new(Scripted(Mutex::new(script.into()))), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react", graph, spec, &tools).unwrap();
    (router(registry, ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())), store, lookups, posts)
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

async fn run(app: &Router, text: &str) -> Vec<Value> {
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    run_on(app, &thread_id, text).await
}

async fn run_on(app: &Router, thread_id: &str, text: &str) -> Vec<Value> {
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {MESSAGES_CHANNEL: [{"role": "user", "content": text}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");
    run["output"]["messages"].as_array().cloned().unwrap_or_default()
}

fn tool_results(messages: &[Value]) -> Vec<String> {
    messages.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_owned()).collect()
}

#[tokio::test]
async fn the_same_failing_call_is_refused_at_its_bound_without_being_dispatched() {
    // Three turns on one thread, the same wrong call each time: the first
    // two fail for real, the third is refused from the thread.
    let ask = |id: &str| ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "lookup", json!({"table": "incdent"}))]);
    let (app, store, lookups, _) = app(vec![
        ask("c1"), ChatMessage::assistant("that table name was refused"),
        ask("c2"), ChatMessage::assistant("refused again"),
        ask("c3"), ChatMessage::assistant("the table name is wrong; I asked the person"),
    ]);
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let first = tool_results(&run_on(&app, &thread_id, "count rows in incdent").await);
    assert_eq!(ToolFailure::parse(&first[0]).unwrap().class, "invalid_arguments", "{first:?}");
    let second = tool_results(&run_on(&app, &thread_id, "try again").await);
    assert_eq!(ToolFailure::parse(second.last().unwrap()).unwrap().class, "invalid_arguments", "{second:?}");
    let third = tool_results(&run_on(&app, &thread_id, "once more").await);
    let refusal = ToolFailure::parse(third.last().unwrap()).expect("a structured refusal");
    assert_eq!(refusal.class, "bounded", "{third:?}");
    assert!(refusal.next.contains("change the arguments"));
    assert_eq!(lookups.load(Ordering::SeqCst), 2, "the third identical call never reached the tool");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_write_whose_answer_was_lost_is_read_back_before_it_is_sent_again() {
    let post = |id: &str| ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "post", json!({"text": "hello"}))]);
    let read = ChatMessage::assistant_tool_calls(vec![ToolCall::new("r1", "read_posts", json!({}))]);
    let (app, store, _, posts) = app(vec![post("c1"), post("c2"), read, post("c3"), ChatMessage::assistant("posted once, verified")]);
    let messages = run(&app, "post hello").await;
    let results = tool_results(&messages);
    assert_eq!(results.len(), 4, "{results:?}");
    assert_eq!(ToolFailure::parse(&results[0]).unwrap().class, "unknown_outcome");
    let second = ToolFailure::parse(&results[1]).expect("refused from the thread");
    assert_eq!(second.class, "reconcile_first", "{results:?}");
    assert!(results[2].contains("posts"), "the read answered: {}", results[2]);
    assert!(results[3].contains("posted"), "after the read the write went through: {}", results[3]);
    assert_eq!(posts.load(Ordering::SeqCst), 2, "sent once, lost; refused; sent again after the read");
    let _ = std::fs::remove_dir_all(store);
}
