//! An agent produces a document that outlives its reply: `artifacts.write`
//! files it as a named, versioned artifact of the run — the commitment in
//! the run's own journal, the bytes readable by address, the name's
//! versions accumulating — and answers what the agent needs to say.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{ToolRegistry, ToolSource};
use rusty_agent_server::{router, GraphRegistry, PlatformTools, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Writes the brief, then its adaptation, then says what it filed. The
/// brief's text carries the person's words, so a second run writes a
/// second version rather than converging on the first.
struct Writer;

#[async_trait::async_trait]
impl ChatModel for Writer {
    async fn chat(&self, messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        let asked = messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let answered = messages.iter().filter(|m| m.role == Role::Tool).count();
        // A reader: asked to build on the brief, it reads the artifact
        // first and answers the figure it read there.
        if asked.starts_with("Read ") {
            let message = if answered == 0 {
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "r1",
                    "artifacts.read",
                    json!({"name": "q3-laptop-spend"}),
                )])
            } else {
                let read: Value = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Tool)
                    .and_then(|m| m.content.as_deref())
                    .and_then(|c| serde_json::from_str(c).ok())
                    .unwrap_or(Value::Null);
                let text = read["text"].as_str().unwrap_or("");
                let figure = text
                    .split("Validated total: ")
                    .nth(1)
                    .and_then(|t| t.split(' ').next())
                    .unwrap_or("?");
                ChatMessage::assistant(format!(
                    "Read {} v{} of {} ({} bytes, from run {}): the validated total is {figure}.",
                    read["name"].as_str().unwrap_or("?"),
                    read["version"],
                    read["of"],
                    read["bytes"],
                    read["run_id"].as_str().map(|r| &r[..8]).unwrap_or("?")
                ))
            };
            return Ok(ChatResponse {
                message,
                model: Some("writer".into()),
                usage: None,
            });
        }
        let message = match answered {
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "w1",
                "artifacts.write",
                json!({"name": "q3-laptop-spend", "text": format!("# Q3 laptop spend\n\nAsked: {asked}\n\nValidated total: 38,900 (the 41,200 figure counted a cancelled order).")}),
            )]),
            1 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "w2",
                "artifacts.write",
                json!({"name": "q3-laptop-spend-for-engineering", "text": "# Q3 laptop spend — for engineering managers\n\nValidated total: 38,900. Refresh plans stand."}),
            )]),
            _ => {
                let filed: Vec<String> = messages
                    .iter()
                    .filter(|m| m.role == Role::Tool)
                    .filter_map(|m| m.content.as_deref())
                    .filter_map(|c| serde_json::from_str::<Value>(c).ok())
                    .map(|v| format!("{} v{}", v["name"], v["version"]))
                    .collect();
                ChatMessage::assistant(format!(
                    "Filed {} — validated total 38,900.",
                    filed.join(" and ")
                ))
            }
        };
        Ok(ChatResponse {
            message,
            model: Some("writer".into()),
            usage: None,
        })
    }
}

fn app() -> (Router, std::path::PathBuf) {
    let store =
        std::env::temp_dir().join(format!("rusty-artifacts-write-{}", uuid::Uuid::new_v4()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(Arc::new(Writer), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools);
    (router(registry, config), store)
}

async fn raw(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
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
    (
        status,
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, bytes) = raw(app, method, uri, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn run(app: &Router, words: &str) -> Value {
    let (_, thread) = call(
        app,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": words}]}, "assistant_id": "reconciler"}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    run
}

fn tool_results(run: &Value) -> Vec<Value> {
    run["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .filter_map(|c| serde_json::from_str(c).ok())
        .collect()
}

#[tokio::test]
async fn an_agent_files_a_named_artifact_and_a_second_run_adds_a_version() {
    let (app, store) = app();
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "reconciler", "name": "Spend Reconciler", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Reconcile figures and write briefs.", "tools": [{"name": "artifacts.write"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // The first run files the brief and its adaptation, each at version 0.
    let first = run(&app, "Reconcile the Q3 laptop spend.").await;
    let filed = tool_results(&first);
    assert_eq!(filed.len(), 2, "{first}");
    assert_eq!(filed[0]["name"], "q3-laptop-spend");
    assert_eq!(filed[0]["version"], 0);
    assert_eq!(filed[0]["created"], true);
    assert!(
        filed[0]["text_head"]
            .as_str()
            .unwrap()
            .starts_with("# Q3 laptop spend"),
        "{}",
        filed[0]
    );
    assert_eq!(filed[1]["name"], "q3-laptop-spend-for-engineering");
    assert_eq!(first["output"]["messages"].as_array().unwrap().last().unwrap()["content"], "Filed \"q3-laptop-spend\" v0 and \"q3-laptop-spend-for-engineering\" v0 — validated total 38,900.");

    // The commitment is in the run's own journal; the bytes are readable
    // by address and say what the agent wrote.
    let run_id = first["run_id"].as_str().unwrap();
    let (_, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "artifact_committed").count(),
        2,
        "{kinds:?}"
    );
    let artifact_id = filed[0]["artifact_id"].as_str().unwrap();
    let (status, bytes) = raw(
        &app,
        "GET",
        &format!("/artifacts/{artifact_id}/bytes"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("Validated total: 38,900"), "{text}");
    let (status, named) = call(&app, "GET", "/artifacts/names/q3-laptop-spend", None).await;
    assert_eq!(status, StatusCode::OK, "{named}");
    assert_eq!(
        named["artifact"]["lineage"]["run_id"]
            .as_str()
            .or_else(|| named["lineage"]["run_id"].as_str()),
        Some(run_id),
        "{named}"
    );

    // A second run under the same name is a second version; the
    // adaptation, written again byte for byte, converges rather than
    // duplicating.
    let second = run(
        &app,
        "Reconcile the Q3 laptop spend again, after the credit note.",
    )
    .await;
    let filed = tool_results(&second);
    assert_eq!(filed[0]["name"], "q3-laptop-spend");
    assert_eq!(filed[0]["version"], 1, "{}", filed[0]);
    assert_eq!(filed[1]["created"], false, "{}", filed[1]);
    let (status, versions) = call(
        &app,
        "GET",
        "/artifacts/names/q3-laptop-spend/versions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let count = versions["versions"]
        .as_array()
        .map(Vec::len)
        .or_else(|| versions.as_array().map(Vec::len))
        .unwrap_or(0);
    assert_eq!(count, 2, "{versions}");

    // A later run reads the brief back by name — the newest version, with
    // its lineage — and builds on what it read rather than on memory.
    let (status, made) = call(&app, "POST", "/assistants", Some(json!({
        "assistant_id": "reader", "name": "Board Brief Writer", "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "Read the brief, then write for the board.", "tools": [{"name": "artifacts.read"}, {"name": "artifacts.write"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (_, thread) = call(
        &app,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, read_run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": {"messages": [{"role": "user", "content": "Read the Q3 brief and tell me the validated total."}]}, "assistant_id": "reader"}))).await;
    assert_eq!(status, StatusCode::OK, "{read_run}");
    let read = &tool_results(&read_run)[0];
    assert_eq!(read["name"], "q3-laptop-spend");
    assert_eq!(read["version"], 1, "the newest version: {read}");
    assert_eq!(read["of"], 2);
    assert_eq!(read["truncated"], false);
    assert!(
        read["text"]
            .as_str()
            .unwrap()
            .contains("after the credit note"),
        "the second version's text: {read}"
    );
    assert_eq!(
        read["run_id"], second["run_id"],
        "the lineage names the run that wrote the head"
    );
    let said = read_run["output"]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        said.contains("v1 of 2") && said.contains("38,900"),
        "{said}"
    );

    let _ = std::fs::remove_dir_all(store);
}
