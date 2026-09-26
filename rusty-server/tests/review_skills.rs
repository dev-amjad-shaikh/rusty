//! Skills from episodes (SI3): the post-run review names the method a
//! verified turn used; the same method in three verified runs becomes a
//! skill registered under the review's name and a version of the agent
//! that follows it, filed for the candidate gate and a person. Through
//! the real paths: runs a scripted judge verifies and reviews.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

type Script = Arc<Mutex<VecDeque<ChatMessage>>>;

struct Scripted(Script);

#[async_trait]
impl ChatModel for Scripted {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let message = self.0.lock().unwrap().pop_front().ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse { message, model: Some("scripted".into()), usage: None })
    }
}

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "A desk read."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!({"count": 2, "rows": ["INC0010001", "INC0010002"]}))
    }
}

fn app() -> (Router, PathBuf, Script, Script) {
    let store = std::env::temp_dir().join(format!("rusty-server-review-skills-{}", uuid::Uuid::new_v4()));
    let agent: Script = Arc::new(Mutex::new(VecDeque::new()));
    let judge: Script = Arc::new(Mutex::new(VecDeque::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Named("desk.count"));
    tools.register(Named("desk.list"));
    let graph = create_react_agent(Arc::new(Scripted(Arc::clone(&agent))), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_verifier(Arc::new(Scripted(Arc::clone(&judge))));
    (router(registry, config), store, agent, judge)
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

/// One verified run of the desk that counts, then lists; the judge
/// verifies it and, as the reviewer, names the method.
async fn counting_run(app: &Router, agent: &Script, judge: &Script) -> Value {
    counting_run_named(app, agent, judge, "count-then-list-open-incidents").await
}

/// The same run, with the reviewer naming the method `name`.
async fn counting_run_named(app: &Router, agent: &Script, judge: &Script, name: &str) -> Value {
    *agent.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "desk.count", json!({"q": "open"}))]),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c2", "desk.list", json!({"q": "open"}))]),
        ChatMessage::assistant("2 open incidents; the newest is INC0010002."),
    ]);
    *judge.lock().unwrap() = VecDeque::from(vec![
        ChatMessage::assistant("{\"verdict\": \"verified\", \"reason\": \"the count and the list say so\"}"),
        ChatMessage::assistant(format!("{{\"person_facts\": [], \"preferences\": [], \"work_facts\": [], \"unanswered\": [], \"procedure\": {{\"name\": \"{name}\", \"when\": \"Use when a person asks how many incidents are open and what the newest is.\"}}}}")),
    ]);
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "how many open incidents, and the newest?"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["verification"]["verdict"], "verified", "{run}");
    // The review runs after the verdict, on its own task.
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    for _ in 0..200 {
        let (status, review) = call(app, "GET", &format!("/runs/{run_id}/review"), None).await;
        if status == StatusCode::OK && review.get("procedure").is_some() {
            return review;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the review of {run_id} never landed");
}

#[tokio::test]
async fn a_method_that_worked_three_times_becomes_a_skill_the_agent_is_proposed_to_follow() {
    let (app, store, agent, judge) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react", "config": {"instructions": "You count and list.", "studio_intent": {"instructions": "You count and list.", "tools": [{"name": "desk.count"}, {"name": "desk.list"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let v1 = made["active_version_id"].as_str().unwrap().to_owned();

    let first = counting_run(&app, &agent, &judge).await;
    assert_eq!(first["procedure"]["counted"], true, "{first}");
    assert_eq!(first["procedure"]["method"], "desk.count + desk.list");
    assert_eq!(first["procedure"]["runs"], 1);
    assert!(first["procedure"].get("proposed").is_none(), "one run is not a habit: {first}");
    // The next review calls the same tools something else: the method keeps
    // the name it was first given, and the report says what it was called now.
    let second = counting_run_named(&app, &agent, &judge, "list-and-count-incidents").await;
    assert_eq!(second["procedure"]["runs"], 2, "{second}");
    assert_eq!(second["procedure"]["name"], "count-then-list-open-incidents", "{second}");
    assert_eq!(second["procedure"]["named_now"], "list-and-count-incidents", "{second}");
    let third = counting_run(&app, &agent, &judge).await;
    assert_eq!(third["procedure"]["runs"], 3, "{third}");
    assert_eq!(third["procedure"]["proposed"]["skill"], "count-then-list-open-incidents", "{third}");
    let version_id = third["procedure"]["proposed"]["version_id"].as_str().unwrap().to_owned();

    // The skill is on the plane under the review's name, with the method.
    let (status, skill) = call(&app, "GET", "/skills/count-then-list-open-incidents", None).await;
    assert_eq!(status, StatusCode::OK, "{skill}");
    let text = skill.to_string();
    assert!(text.contains("desk.count") && text.contains("desk.list"), "{skill}");
    assert!(text.contains("post-run review"), "{skill}");

    // The agent has a version filed for it that follows the skill; the
    // running version is untouched — a person or the policy applies it.
    let (_, a) = call(&app, "GET", "/assistants/desk", None).await;
    assert_eq!(a["active_version_id"], json!(v1));
    let (_, version) = call(&app, "GET", &format!("/assistants/desk/versions/{version_id}"), None).await;
    assert_eq!(version["version"]["config"]["studio_intent"]["skills"], json!(["count-then-list-open-incidents"]), "{version}");
    assert_eq!(version["version"]["metadata"]["proposed_by"]["name"], "the post-run review");
    assert!(version["version"]["metadata"]["why"].as_str().unwrap().contains("3 verified runs"), "{version}");

    // A fourth run counts, but the method is proposed once.
    let fourth = counting_run(&app, &agent, &judge).await;
    assert_eq!(fourth["procedure"]["runs"], 4);
    assert!(fourth["procedure"].get("proposed").is_none(), "{fourth}");

    let _ = std::fs::remove_dir_all(store);
}
