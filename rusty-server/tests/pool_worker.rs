//! An agent bound to a pool consumes the task queue: a task enqueued in its
//! pool becomes a real run of the agent and settles with the reply as the
//! result; a task in another pool stays queued; a run that pauses for an
//! approval fails the task without retry.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::builtins::CalculatorTool;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct EchoModel;

#[async_trait]
impl ChatModel for EchoModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let asked = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        Ok(ChatResponse {
            message: ChatMessage::assistant(format!("You asked: {asked}. The answer is 42.")),
            model: Some("echo".into()),
            usage: None,
        })
    }
}

/// A model that names a record no tool returned: the commonest fabricated
/// completion, and the one the verifier decides without a judge.
struct InventingModel;

#[async_trait]
impl ChatModel for InventingModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("Filed INC0099999 for you.".to_owned()),
            model: Some("inventor".into()),
            usage: None,
        })
    }
}

/// A judge that must never be consulted: the verifier's rules decide first.
struct NeverAsked;

#[async_trait]
impl ChatModel for NeverAsked {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        panic!("the judge must not be asked when a rule already decided");
    }
}

/// The pool app with an agent that invents, and a verifier on the server.
fn verified_app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-pool-worker-{}", uuid::Uuid::new_v4()));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent(Arc::new(InventingModel), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("inventor", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_verifier(Arc::new(NeverAsked));
    (router(registry, config), store)
}

fn test_app() -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-pool-worker-{}", uuid::Uuid::new_v4()));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent(Arc::new(EchoModel), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("capable", graph, spec, &tools)
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

async fn settled(app: &Router, task_id: &str) -> Value {
    for _ in 0..200 {
        let (_, t) = call(app, "GET", &format!("/tasks/{task_id}"), None).await;
        if matches!(t["status"].as_str(), Some("completed" | "failed" | "dead")) {
            return t;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (_, t) = call(app, "GET", &format!("/tasks/{task_id}"), None).await;
    t
}

#[tokio::test]
async fn an_agent_bound_to_a_pool_works_its_tasks_as_real_runs() {
    let (app, store) = test_app();
    let (status, v) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "briefer", "name": "Briefer", "graph": "capable",
        "config": {"studio_intent": {"instructions": "Answer briefly.", "tools": [{"name": "calculator"}], "pool": "briefs"}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // A task in the agent's pool and one in another.
    let (status, mine) = call(&app, "POST", "/tasks", Some(json!({"kind": "brief", "pool": "briefs", "payload": {"message": "How far is 5 miles in km?"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    let (_, other) = call(
        &app,
        "POST",
        "/tasks",
        Some(json!({"kind": "brief", "pool": "elsewhere", "payload": {"message": "nobody's"}})),
    )
    .await;
    let mine_id = mine["task_id"].as_str().unwrap().to_string();
    let other_id = other["task_id"].as_str().unwrap().to_string();

    let done = settled(&app, &mine_id).await;
    assert_eq!(
        done["status"],
        json!("completed"),
        "the agent settled it: {done}"
    );
    assert_eq!(done["lease"], Value::Null);
    let reply = done["result"]["reply"]
        .as_str()
        .expect("the reply is the result");
    assert!(reply.contains("42"), "{reply}");
    assert_eq!(done["result"]["agent"], json!("briefer"));
    // The run is an ordinary run, in Observe, with the pool as its channel.
    let run_id = done["result"]["run_id"].as_str().unwrap();
    let (status, run) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["assistant_id"], json!("briefer"));
    assert_eq!(run["metadata"]["channel"], json!("pool"));
    assert_eq!(run["metadata"]["task_id"], json!(mine_id));
    assert_eq!(run["status"], json!("success"));
    // The other pool's task is nobody's.
    let (_, still) = call(&app, "GET", &format!("/tasks/{other_id}"), None).await;
    assert_eq!(still["status"], json!("queued"), "{still}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_task_is_settled_by_its_runs_verdict_not_by_the_run_ending() {
    let (app, store) = verified_app();
    let (status, v) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "filer", "name": "Filer", "graph": "inventor",
        "config": {"studio_intent": {"instructions": "File it.", "tools": [{"name": "calculator"}], "pool": "filing"}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (status, task) = call(&app, "POST", "/tasks", Some(json!({"kind": "file", "pool": "filing", "max_attempts": 1, "payload": {"message": "Please file the printer outage."}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_id = task["task_id"].as_str().unwrap().to_string();

    // The run ends in success and names INC0099999, which no tool returned:
    // the verifier fails it, and the task is not completed with that reply.
    let done = settled(&app, &task_id).await;
    assert_ne!(
        done["status"],
        json!("completed"),
        "a failed verdict is not a result: {done}"
    );
    let text = done.to_string();
    assert!(text.contains("the verifier failed the run"), "{done}");
    assert!(text.contains("INC0099999"), "{done}");
    // Its one attempt was its last: the task is dead, and the person who
    // queued it is told, once, in the Inbox.
    assert_eq!(done["status"], json!("dead"), "{done}");
    let mut told = Value::Null;
    for _ in 0..50 {
        let (_, notices) = call(&app, "GET", "/notices", None).await;
        if let Some(n) = notices["notices"]
            .as_array()
            .and_then(|a| a.iter().find(|n| n["about"]["task_id"] == json!(task_id)))
        {
            told = n.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!told.is_null(), "the queuer is told");
    assert_eq!(
        told["title"],
        json!("Your task file could not be done"),
        "{told}"
    );
    assert_eq!(told["about"]["state"], json!("dead"));
    assert!(
        told["text"]
            .as_str()
            .unwrap_or("")
            .contains("the verifier failed the run"),
        "{told}"
    );
    assert!(
        told["text"]
            .as_str()
            .unwrap_or("")
            .contains("After 1 attempt in pool filing"),
        "{told}"
    );

    let _ = std::fs::remove_dir_all(store);
}

/// A queue of work rehearsed in the stand-in: a task that names a world in
/// its payload is worked there — the agent's run declares the world — and
/// a world nobody holds keeps the task out of the queue.
#[tokio::test]
async fn a_task_that_names_a_world_is_worked_in_that_world() {
    let (app, store) = test_app();
    let (status, v) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "briefer", "name": "Briefer", "graph": "capable",
        "config": {"studio_intent": {"instructions": "Answer briefly.", "tools": [{"name": "calculator"}], "pool": "briefs"}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "ledger-twin", "connector": "ledger", "stands_for": "ledger.invalid", "dialect": "ledger-api"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();

    let (status, refused) = call(&app, "POST", "/tasks", Some(json!({"kind": "brief", "pool": "briefs", "payload": {"message": "5 miles in km?", "world": "no-such-world"}}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    let (status, mine) = call(&app, "POST", "/tasks", Some(json!({"kind": "brief", "pool": "briefs", "payload": {"message": "How far is 5 miles in km?", "world": "ledger-twin"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    let mine_id = mine["task_id"].as_str().unwrap().to_string();
    let (_, kept) = call(&app, "GET", &format!("/tasks/{mine_id}"), None).await;
    assert_eq!(
        kept["payload"]["world"],
        json!(world_id),
        "the payload names the world by id: {kept}"
    );
    assert_eq!(kept["payload"]["world_name"], json!("ledger-twin"));
    let done = settled(&app, &mine_id).await;
    assert_eq!(done["status"], json!("completed"), "{done}");
    let run_id = done["result"]["run_id"].as_str().unwrap();
    let (_, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    let declared = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("run_config_declared"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        declared["output"]["value"]["world"],
        json!(world_id),
        "the run was worked in the world: {declared}"
    );
    let _ = std::fs::remove_dir_all(store);
}
