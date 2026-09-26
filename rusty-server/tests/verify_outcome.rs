//! A completed run is verified against its evidence by a judge; the verdict
//! rides the terminal payload and `GET /runs`, survives a restart, and a
//! judge that cannot be read is an honest `unverified`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

/// A tool that writes, so the catalog has one; the scripts never call it.
struct Post;

#[async_trait::async_trait]
impl Tool for Post {
    fn name(&self) -> &str {
        "post"
    }
    fn description(&self) -> &str {
        "Posts a note."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        // A write that needs no approval token, so the repair turn runs it.
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        Ok(json!({"posted": args.get("text").cloned().unwrap_or(Value::Null)}))
    }
}

fn app_at(store: PathBuf, judge_answer: Option<&str>) -> (Router, Seen) {
    app_with(
        store,
        vec![
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo", json!({"text": "hello"}))]),
            ChatMessage::assistant("The echo said: hello."),
        ],
        judge_answer.into_iter().map(str::to_owned).collect(),
    )
}

/// An app whose agent and judge each follow a script; the judge's lines
/// are consumed one verdict at a time.
fn app_with(store: PathBuf, agent_lines: Vec<ChatMessage>, judge_lines: Vec<String>) -> (Router, Seen) {
    let agent: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(agent_lines.into()),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Echo);
    tools.register(Post);
    let graph = create_react_agent(agent, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react", graph, spec, &tools).unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    if !judge_lines.is_empty() {
        let judge: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
            script: Mutex::new(judge_lines.into_iter().map(ChatMessage::assistant).collect()),
            seen: Arc::clone(&seen),
        });
        config = config.with_verifier(judge);
    }
    (router(registry, config), seen)
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-verify-{}", uuid::Uuid::new_v4()))
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

async fn one_turn(app: &Router) -> Value {
    let (_, thread) = call(app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [
            {"role": "system", "content": "CHARTER: call echo, then report what it said."},
            {"role": "user", "content": "say hello"}
        ]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    run
}

#[tokio::test]
async fn a_completed_run_is_verified_by_its_evidence_and_the_verdict_survives_a_restart() {
    let store = temp_store();
    let (app, seen) = app_at(
        store.clone(),
        Some("{\"verdict\": \"verified\", \"reason\": \"echo returned hello and the reply reports it\"}"),
    );
    let run = one_turn(&app).await;
    assert_eq!(run["status"], "success");
    assert_eq!(run["verification"]["verdict"], "verified", "{run}");
    assert_eq!(run["verification"]["reason"], "echo returned hello and the reply reports it");
    let evidence = &run["verification"]["evidence"];
    assert_eq!(evidence["calls"][0]["tool"], "echo");
    assert_eq!(evidence["calls"][0]["effect"], "read_only");
    assert_eq!(evidence["calls"][0]["outcome"], "ok");
    assert_eq!(evidence["writes"], 0);

    // The judge read the charter, the request, the call with its result,
    // and the reply — by the evidence, not the claim.
    let asked = seen.lock().unwrap().last().cloned().expect("the judge was asked");
    let prompt = asked[1].content.clone().unwrap();
    assert!(prompt.contains("CHARTER:\nCHARTER: call echo"), "{prompt}");
    assert!(prompt.contains("REQUEST:\nsay hello"), "{prompt}");
    assert!(prompt.contains("CALLED echo({\"text\":\"hello\"})\nRESULT: hello"), "{prompt}");
    assert!(prompt.contains("FINAL REPLY:\nThe echo said: hello."), "{prompt}");

    // Served on the run, live and after a restart.
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, runs) = call(&app, "GET", "/runs", None).await;
    let listed = runs.as_array().unwrap().iter().find(|r| r["run_id"] == run_id).unwrap();
    assert_eq!(listed["verification"]["verdict"], "verified");
    let (rebuilt, _) = app_at(store.clone(), None);
    let (_, runs) = call(&rebuilt, "GET", "/runs", None).await;
    let listed = runs.as_array().unwrap().iter().find(|r| r["run_id"] == run_id).unwrap();
    assert_eq!(listed["verification"]["verdict"], "verified", "{listed}");
    // And the run itself, recalled from its journal, still carries its
    // verdict and the thread it left — what Observe shows as what happened.
    let (status, recalled) = call(&rebuilt, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{recalled}");
    assert_eq!(recalled["verification"]["verdict"], "verified", "{recalled}");
    let messages = recalled["output"]["messages"].as_array().expect("the recalled run's thread");
    assert_eq!(messages.last().unwrap()["content"], "The echo said: hello.", "{recalled}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_judge_that_cannot_be_read_is_an_honest_unverified() {
    let store = temp_store();
    let (app, _) = app_at(store.clone(), Some("Looks fine to me."));
    let run = one_turn(&app).await;
    assert_eq!(run["verification"]["verdict"], "unverified", "{run}");
    assert!(run["verification"]["reason"].as_str().unwrap().contains("could not be read"));
    assert_eq!(run["verification"]["judge"]["answer"], "Looks fine to me.");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn without_a_judge_a_run_says_nothing_about_its_outcome() {
    let store = temp_store();
    let (app, _) = app_at(store.clone(), None);
    let run = one_turn(&app).await;
    assert_eq!(run["status"], "success");
    assert!(run.get("verification").is_none(), "{run}");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_failed_verdict_sends_the_model_back_once_and_the_second_verdict_stands() {
    let store = temp_store();
    // The model claims what it did not do; told so, it does it and reports.
    let (app, seen) = app_with(
        store.clone(),
        vec![
            ChatMessage::assistant("Done — the echo said: hello."),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo", json!({"text": "hello"}))]),
            ChatMessage::assistant("The echo said: hello."),
        ],
        vec![
            "{\"verdict\": \"failed\", \"reason\": \"the reply reports an echo no tool returned\"}".to_owned(),
            "{\"verdict\": \"verified\", \"reason\": \"echo returned hello and the reply reports it\"}".to_owned(),
        ],
    );
    let run = one_turn(&app).await;
    assert_eq!(run["status"], "success", "{run}");
    assert_eq!(run["verification"]["verdict"], "verified", "{run}");
    assert_eq!(run["verification"]["repaired"]["verdict"], "failed", "{run}");
    assert_eq!(run["verification"]["repaired"]["reason"], "the reply reports an echo no tool returned");
    assert_eq!(run["verification"]["evidence"]["calls"][0]["tool"], "echo");

    // The thread holds the whole turn: the claim, the judge's word as a
    // system message, the call it caused, and the reply that stands.
    let messages = run["output"]["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "system", "assistant", "tool", "assistant"], "{run}");
    let notice = messages[3]["content"].as_str().unwrap();
    assert!(notice.starts_with(rusty_agent_server::verify_outcome::REPAIR_NOTICE_PREFIX), "{notice}");
    assert!(notice.contains("judged this turn's outcome not achieved: the reply reports an echo no tool returned."), "{notice}");
    assert!(notice.contains("answer plainly with what was and was not done"), "{notice}");

    // The second judge saw the whole turn — the claim and the call alike.
    let asked = seen.lock().unwrap().last().cloned().expect("the judge was asked twice");
    let prompt = asked[1].content.clone().unwrap();
    assert!(prompt.contains("CALLED echo({\"text\":\"hello\"})\nRESULT: hello"), "{prompt}");
    assert!(prompt.contains("FINAL REPLY:\nThe echo said: hello."), "{prompt}");

    // Once: a second failed verdict is final, with the first beside it.
    let (app, _) = app_with(
        store.clone(),
        vec![ChatMessage::assistant("Done."), ChatMessage::assistant("Still done.")],
        vec![
            "{\"verdict\": \"failed\", \"reason\": \"nothing was called\"}".to_owned(),
            "{\"verdict\": \"failed\", \"reason\": \"still nothing was called\"}".to_owned(),
            "{\"verdict\": \"verified\", \"reason\": \"never asked\"}".to_owned(),
        ],
    );
    let run = one_turn(&app).await;
    assert_eq!(run["verification"]["verdict"], "failed", "{run}");
    assert_eq!(run["verification"]["reason"], "still nothing was called");
    assert_eq!(run["verification"]["repaired"]["reason"], "nothing was called");
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_unsettled_verdict_with_nothing_written_sends_a_writing_agent_back_too() {
    let store = temp_store();
    // The judge cannot settle it; nothing wrote; the agent could have.
    let (app, _) = app_with(
        store.clone(),
        vec![
            ChatMessage::assistant("Posted the note."),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("p1", "post", json!({"text": "hello"}))]),
            ChatMessage::assistant("Posted the note; the board took it."),
        ],
        vec![
            "{\"verdict\": \"unverified\", \"reason\": \"no call posted anything, yet the reply says it did\"}".to_owned(),
            "{\"verdict\": \"verified\", \"reason\": \"post returned and the reply reports it\"}".to_owned(),
        ],
    );
    let run = one_turn(&app).await;
    assert_eq!(run["verification"]["verdict"], "verified", "{run}");
    assert_eq!(run["verification"]["repaired"]["verdict"], "unverified", "{run}");
    assert_eq!(run["verification"]["evidence"]["writes"], 1, "{run}");
    let notice = run["output"]["messages"][3]["content"].as_str().unwrap();
    assert!(notice.starts_with("The verification step could not confirm this turn's outcome: no call posted anything"), "{notice}");

    let _ = std::fs::remove_dir_all(store);
}
