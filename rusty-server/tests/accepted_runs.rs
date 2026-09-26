//! A finished run outlives the process that ran it. Its accepted record and
//! its journal answer `GET /runs/{id}` — with the exact accepted input —
//! keep its agent on the listing, and let an evaluation case bind to it
//! after a restart. A run no server accepted stays unknown.

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

struct AnswerModel;

#[async_trait]
impl ChatModel for AnswerModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        Ok(ChatResponse { message: ChatMessage::assistant("42"), model: Some("answer-test".into()), usage: None })
    }
}

/// The app over a given store root (the restart builds it twice).
fn test_app_at(store: PathBuf) -> Router {
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent(Arc::new(AnswerModel), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("capable", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
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
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, value)
}

fn input() -> Value {
    json!({"messages": [{"role": "user", "content": "what is 6 times 7?"}]})
}

#[tokio::test]
async fn a_finished_run_is_recalled_after_a_restart_and_can_become_a_case() {
    let store = std::env::temp_dir().join(format!("rusty-accepted-runs-{}", uuid::Uuid::new_v4()));
    let app = test_app_at(store.clone());
    let (status, v) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "keeper",
            "name": "Keeper",
            "graph": "capable",
            "config": {"studio_intent": {"instructions": "Answer.", "tools": [{"name": "calculator"}]}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let thread = v["thread_id"].as_str().unwrap().to_string();
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "keeper", "input": input()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let run_id = terminal["run_id"].as_str().unwrap().to_string();

    // Held in this process: the detail carries the exact accepted input —
    // the payload as admitted, so the agent's charter rides in front of
    // the user's message. That admitted form is what a case binds to.
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let accepted = v["input"].clone();
    let messages = accepted["messages"].as_array().expect("messages");
    assert_eq!(messages.last().unwrap(), &input()["messages"][0], "{accepted}");
    assert_eq!(v["assistant_id"], json!("keeper"));
    drop(app);

    // A new server over the same store: the run is recalled from its
    // accepted record and its journal, not from memory.
    let app = test_app_at(store.clone());
    let (status, v) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "recalled after restart: {v}");
    assert_eq!(v["input"], accepted, "the exact accepted input survives");
    assert_eq!(v["assistant_id"], json!("keeper"));
    assert_eq!(v["thread_id"], json!(thread));
    assert_eq!(v["status"], json!("success"), "the journal's verdict is the status");

    // The listing keeps the run's agent.
    let (status, list) = call(&app, "GET", "/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let listed = list.as_array().unwrap().iter().find(|r| r["run_id"] == json!(run_id)).expect("listed after restart");
    assert_eq!(listed["assistant_id"], json!("keeper"), "{listed}");

    // A case binds to the recovered run.
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({
            "name": "keeper",
            "version": "v1",
            "cases": [{
                "id": "answers-forty-two",
                "input": accepted,
                "expect": {"forbid_tools": ["calculator"]},
                "tags": ["smoke"],
                "source": {"run_id": run_id, "thread_id": thread, "agent_id": "keeper", "captured_at": "2026-01-01T00:00:00Z"}
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "a case binds after a restart: {v}");
    assert_eq!(v["case_count"], json!(1));

    // A run no server accepted stays unknown.
    let (status, _) = call(&app, "GET", "/runs/never-accepted", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The dataset keeps growing after its earliest evidence is gone: a new
    // version carries the stored case unchanged (trusted as it stands) plus
    // a case from a run this server holds.
    let (_, v) = call(&app, "GET", "/datasets/keeper/versions/v1/cases", None).await;
    let held = v["cases"][0].clone();
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"assistant_id": "keeper", "input": input()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let second_run = terminal["run_id"].as_str().unwrap().to_string();
    let (_, second) = call(&app, "GET", &format!("/runs/{second_run}"), None).await;
    std::fs::remove_file(store.join("accepted_runs").join(format!("{run_id}.json"))).unwrap();
    let (status, _) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the first run's record is gone");
    let second_case = json!({
        "id": "answers-again",
        "input": second["input"],
        "expect": {},
        "tags": [],
        "source": {"run_id": second_run, "thread_id": thread, "agent_id": "keeper", "captured_at": "2026-01-01T00:00:00Z"}
    });
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "keeper", "version": "v2", "cases": [held.clone(), second_case.clone()]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "grows past its gone evidence: {v}");
    assert_eq!(v["case_count"], json!(2));

    // A changed case is a new claim, and its run is gone: refused.
    let mut altered = held.clone();
    altered["tags"] = json!(["altered"]);
    let (status, v) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "keeper", "version": "v3", "cases": [altered, second_case]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an altered case needs its run: {v}");

    let _ = std::fs::remove_dir_all(store);
}
