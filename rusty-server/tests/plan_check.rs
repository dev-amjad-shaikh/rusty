//! A plan the platform can see (CORE C4): the `plan` tool's steps are
//! rendered into the model's situation on every call, and a final answer
//! that arrives with steps open is sent back once with a plan-check
//! notice — the model works or skips the steps, then answers.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL, PLAN_NOTICE_PREFIX};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{ToolRegistry, ToolSource};
use rusty_agent_server::{router, GraphRegistry, PlatformTools, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

type Script = Arc<Mutex<VecDeque<ChatMessage>>>;
type Seen = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

struct Scripted(Script, Seen);

#[async_trait]
impl ChatModel for Scripted {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.1.lock().unwrap().push(messages.to_vec());
        let message = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted".into()),
            usage: None,
        })
    }
}

fn app() -> (Router, PathBuf, Script, Seen) {
    let store =
        std::env::temp_dir().join(format!("rusty-server-plan-check-{}", uuid::Uuid::new_v4()));
    let script: Script = Arc::new(Mutex::new(VecDeque::new()));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(
        Arc::new(Scripted(Arc::clone(&script), Arc::clone(&seen))),
        tools.clone(),
    )
    .unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    // The task section — the situation, the plan — is assembled under a
    // context policy, as every deployment runs.
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools)
        .with_context_policy(rusty_agent_runtime::context::ContextPolicy::standard(8192));
    (router(registry, config), store, script, seen)
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
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_plan_is_shown_each_turn_and_an_answer_with_open_steps_is_sent_back_once() {
    let (app, store, script, seen) = app();
    *script.lock().unwrap() = VecDeque::from(vec![
        // The plan: two steps.
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "p1",
            "plan",
            json!({"steps": [{"text": "count the open incidents"}, {"text": "name the newest"}]}),
        )]),
        // An answer with both still open — sent back.
        ChatMessage::assistant("There are some incidents."),
        // The model marks the plan done and answers.
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "p2",
            "plan",
            json!({"steps": [{"text": "count the open incidents", "status": "done"}, {"text": "name the newest", "status": "skipped", "note": "no list tool here"}]}),
        )]),
        ChatMessage::assistant(
            "2 open incidents; the newest could not be named without a list tool.",
        ),
    ]);
    // An agent with a charter: under a context policy the identity is pinned at admission.
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "desk", "name": "Desk", "graph": "react_agent", "config": {"instructions": "You count incidents.", "studio_intent": {"instructions": "You count incidents.", "tools": [{"name": "plan"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (_, thread) = call(
        &app,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "desk", "input": {MESSAGES_CHANNEL: [{"role": "user", "content": "how many open, and the newest?"}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");
    let messages = run["output"][MESSAGES_CHANNEL].as_array().unwrap();
    // The check landed once, after the premature answer, and the loop went on.
    let notices: Vec<&Value> = messages
        .iter()
        .filter(|m| {
            m["role"] == "system"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with(PLAN_NOTICE_PREFIX))
        })
        .collect();
    assert_eq!(notices.len(), 1, "{messages:?}");
    assert!(
        notices[0]["content"]
            .as_str()
            .unwrap()
            .contains("2 steps not done"),
        "{}",
        notices[0]
    );
    let last = messages.last().unwrap();
    assert_eq!(
        last["content"],
        "2 open incidents; the newest could not be named without a list tool."
    );
    // The plan tool answered with the counts.
    let plan_results: Vec<Value> = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            serde_json::from_str(m["content"].as_str().unwrap_or("null")).unwrap_or(Value::Null)
        })
        .collect();
    assert_eq!(plan_results[0]["open"], 2, "{}", plan_results[0]);
    assert_eq!(plan_results[1]["open"], 0, "{}", plan_results[1]);
    // The run's progress record says how it got there: two tool calls, the
    // plan as last written — one step done, one skipped with its note.
    let run_id = run["run_id"].as_str().unwrap();
    let (status, page) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let progress = &page["progress"];
    assert_eq!(progress["tool_calls"], 2, "{progress}");
    assert_eq!(progress["last_tool"], "plan", "{progress}");
    assert_eq!(progress["plan"]["done"], 1, "{progress}");
    assert_eq!(progress["plan"]["open"], 0, "{progress}");
    assert_eq!(
        progress["plan"]["steps"][1]["status"], "skipped",
        "{progress}"
    );
    assert_eq!(
        progress["plan"]["steps"][1]["note"], "no list tool here",
        "{progress}"
    );
    assert!(progress["updated_at"].as_str().is_some(), "{progress}");
    // Every model call after the plan saw it in its situation.
    let calls = seen.lock().unwrap();
    // The task section lands wherever the assembler puts it; every message the model saw is read.
    let situation_of = |m: &[ChatMessage]| {
        m.iter()
            .filter(|x| x.role != Role::Assistant)
            .filter_map(|x| x.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !situation_of(&calls[0]).contains("Plan —"),
        "no plan yet on the first call"
    );
    assert!(
        situation_of(&calls[1]).contains("Plan — 0 of 2 done, 2 open"),
        "call 1 saw: {:?}",
        calls[1]
            .iter()
            .map(|m| (
                format!("{:?}", m.role),
                m.content
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect::<String>()
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        situation_of(&calls[3]).contains("Plan — 1 of 2 done"),
        "{}",
        situation_of(&calls[3])
    );
    let _ = std::fs::remove_dir_all(store);
}
