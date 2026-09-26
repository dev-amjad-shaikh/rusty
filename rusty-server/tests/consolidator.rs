//! The Consolidator: the platform's third agent reads what a skill's
//! followers did in their verified runs and files a better procedure —
//! which enters the gate like any revision: held while the followers'
//! suites judge it, promoted by a person. Never auto-registered as current.
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::Role;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry, ToolSource};
use rusty_agent_server::{router, GraphRegistry, PlatformTools, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// One model plays every agent, by charter. The desk looks up before it
/// answers whenever its skill says so. The Consolidator reads the skill,
/// reviews the follower, and files the step the verified runs took.
struct Everyone;

#[async_trait::async_trait]
impl ChatModel for Everyone {
    async fn chat(&self, messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        let system = messages.iter().filter(|m| m.role == Role::System).filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
        let answered = messages.iter().filter(|m| m.role == Role::Tool).count();
        let message = if system.contains("You are the Consolidator") {
            match answered {
                0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "skills.read", json!({"name": "count-well"}))]),
                1 => ChatMessage::assistant_tool_calls(vec![ToolCall::new("c2", "runs.review", json!({"agent": "desk"}))]),
                2 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c3",
                    "skills.revise",
                    json!({"skill": "count-well", "add_step": "Call lookup with no arguments and answer with its count; never answer from memory.", "why": "Every verified run called lookup before answering; the procedure never says to."}),
                )]),
                _ => {
                    let filed = messages.iter().rev().find(|m| m.role == Role::Tool).and_then(|m| m.content.as_deref()).and_then(|c| serde_json::from_str::<Value>(c).ok()).unwrap_or(Value::Null);
                    ChatMessage::assistant(format!("The verified runs all looked up first. Filed \"Call lookup with no arguments…\" as revision {} (held: {}).", filed["revision"], filed["held"]))
                }
            }
        } else if system.contains("LOOKUP FIRST") || system.contains("Call lookup with no arguments") {
            if answered == 0 {
                ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "lookup", json!({}))])
            } else {
                ChatMessage::assistant("there are 168")
            }
        } else if answered == 0 {
            // The desk under the plain procedure: it looks up anyway — the
            // trajectory the Consolidator will find.
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "lookup", json!({}))])
        } else {
            ChatMessage::assistant("there are 168")
        };
        Ok(ChatResponse { message, model: Some("everyone".into()), usage: None })
    }
}

struct Lookup;

#[async_trait::async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "Counts."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!({"count": 168}))
    }
}

fn app() -> (Router, std::path::PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-consolidator-{}", uuid::Uuid::new_v4()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.register(Lookup);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(Arc::new(Everyone), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools)
        .with_context_policy(rusty_agent_runtime::context::ContextPolicy::standard(8192));
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

async fn platform_agent(app: &Router, name: &str) -> Value {
    for _ in 0..200 {
        let (_, list) = call(app, "GET", "/assistants", None).await;
        let agents = list.as_array().cloned().or_else(|| list["assistants"].as_array().cloned()).unwrap_or_default();
        if let Some(found) = agents.iter().find(|a| a["name"] == name) {
            return found.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the platform never seeded {name}");
}

async fn run_as(app: &Router, assistant_id: &str, words: &str) -> (String, String, Value) {
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react_agent"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": words}]}, "assistant_id": assistant_id}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    (thread_id, run["run_id"].as_str().unwrap().to_owned(), run)
}

async fn finished(app: &Router, name: &str, version: &str, evaluation_id: &str) -> Value {
    for _ in 0..400 {
        let (_, list) = call(app, "GET", &format!("/datasets/{name}/versions/{version}/evaluations"), None).await;
        let evaluations = list["evaluations"].as_array().cloned().or_else(|| list.as_array().cloned()).unwrap_or_default();
        if let Some(e) = evaluations.iter().find(|e| e["evaluation_id"] == evaluation_id) {
            if e["status"] == "done" {
                return e.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("evaluation {evaluation_id} never finished");
}

#[tokio::test]
async fn the_consolidator_files_what_the_verified_runs_did_and_the_gate_holds_it_until_the_followers_pass() {
    let (app, store) = app();
    let consolidator = platform_agent(&app, "Consolidator").await;
    let consolidator_id = consolidator["assistant_id"].as_str().unwrap().to_owned();
    let tools: Vec<String> = consolidator["config"]["studio_intent"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    assert!(tools.iter().any(|t| t == "skills.revise") && tools.iter().any(|t| t == "runs.review"), "{tools:?}");

    // A skill whose procedure says nothing about looking up, and a desk
    // that follows it — and looks up anyway, verified.
    let skill_md = "---\nname: count-well\ndescription: How to count and answer.\nallowed-tools: lookup\n---\n\n# Count well\n\nAnswer with the count, in one sentence.\n";
    let (status, r) = call(&app, "POST", "/skills", Some(json!({"skill_md": skill_md}))).await;
    assert_eq!(status, StatusCode::CREATED, "{r}");
    assert_eq!(r["revision"], json!(1));
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "follow your skills", "studio_intent": {"skills": ["count-well"], "tools": [{"name": "lookup"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (thread_id, run_id, run) = run_as(&app, "desk", "how many?").await;
    assert!(run.to_string().contains("168"), "{run}");
    let (_, kept) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let (status, dataset) = call(&app, "POST", "/datasets", Some(json!({"name": "desk-counts", "version": "1", "cases": [{
        "id": "counts-via-lookup",
        "input": kept["input"],
        "expect": {"tool_trajectory": [{"name": "lookup"}]},
        "source": {"run_id": run_id, "thread_id": thread_id, "agent_id": "desk", "captured_at": "2026-09-10T10:00:00Z"}
    }]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{dataset}");

    // The Consolidator, asked the way the Skills page asks it.
    let (_, _, said) = run_as(&app, &consolidator_id, "Consolidate the skill count-well: read its procedure and its followers' recent runs, name the trajectory the verified runs share that the procedure does not state, and file one revision with skills.revise if there is one.").await;
    let reply = said.to_string();
    assert!(reply.contains("revision 2"), "the Consolidator filed revision 2 and said so: {reply}");
    assert!(reply.contains("held: true"), "held at the gate: {reply}");

    // The revision is the Consolidator's, held; the follower's suite is
    // judging it; the current revision stays 1 meanwhile.
    let (_, receipt) = call(&app, "GET", "/skills/count-well", None).await;
    assert_eq!(receipt["current"], json!(1), "{receipt}");
    assert_eq!(receipt["candidate"], json!(2), "{receipt}");
    let (_, revision) = call(&app, "GET", "/skills/count-well/versions/2", None).await;
    assert_eq!(revision["provenance"]["author"], "Consolidator", "{revision}");
    let (_, evidence) = call(&app, "GET", "/skills/count-well/evidence?revision=2", None).await;
    let suites = evidence["evidence"]["suites"].as_array().cloned().unwrap_or_default();
    assert_eq!(suites.len(), 1, "{evidence}");
    let judged = suites[0]["evaluation_id"].as_str().expect("the gate started the follower's suite").to_owned();
    let done = finished(&app, "desk-counts", "1", &judged).await;
    assert_eq!(done["passed"], 1, "the follower passes under the revision: {done}");

    // A person promotes it; the followers run revision 2 from here.
    let (status, promoted) = call(&app, "POST", "/skills/count-well/promote", Some(json!({"revision": 2}))).await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    let (_, receipt) = call(&app, "GET", "/skills/count-well", None).await;
    assert_eq!(receipt["current"], json!(2), "{receipt}");
    let (_, body) = call(&app, "GET", "/skills/count-well/body", None).await;
    let text = body["body"].as_str().unwrap_or_default().to_owned();
    assert!(text.contains("**First:** Call lookup with no arguments"), "the step went first: {text}");
    assert!(text.contains("Answer with the count"), "the rest stands: {text}");

    let _ = std::fs::remove_dir_all(store);
}
