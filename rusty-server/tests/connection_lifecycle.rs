//! R05, second half — the connection a paused action runs through is named
//! on the approval; revoking it while the approval waits means the approved
//! action cannot use it (refused by name, nothing sent); the run's evidence
//! names the connection after it is gone; the binding history outlives a
//! restart. Driven in-process, no sockets; the connector's host is
//! unresolvable so nothing could ever be sent even by mistake.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::connector::{
    ConnectorManifest, ConnectorOperation, HttpMethod, OperationEffect,
};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{MESSAGES_CHANNEL, create_react_agent};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{ConnectionTools, GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
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
        Ok(ChatResponse {
            message,
            model: Some("scripted".into()),
            usage: None,
        })
    }
}

/// The app: a react agent whose registry is built over the live connection
/// tools, the same way the demo server builds it.
fn app_at(store: &std::path::Path, script: Vec<ChatMessage>) -> (Router, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(script.into()),
        seen: Arc::clone(&seen),
    });
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
        .with_connection_tools(connection_tools);
    (router(registry, config), seen)
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

async fn wait_terminal(app: &Router, run_id: &str) -> Value {
    for _ in 0..200 {
        let (_, run) = call(app, "GET", &format!("/runs/{run_id}"), None).await;
        if matches!(
            run["status"].as_str(),
            Some("success") | Some("error") | Some("interrupted")
        ) {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run {run_id} never reached a terminal state");
}

/// An echo board: one read, one irreversible post, no credentials, on a host
/// that does not resolve.
fn echo_manifest() -> ConnectorManifest {
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "properties": {}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    ConnectorManifest::new(
        "echo",
        "1",
        "Echo Board",
        "A board that echoes what is posted; posting is final.",
        "https://echo.invalid/docs",
        "https://echo.invalid",
        spec,
        vec![
            op("ping", HttpMethod::Get, "/ping", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("post", HttpMethod::Post, "/post", OperationEffect::Irreversible, json!({"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}})),
        ],
        "ping",
    )
    .expect("the echo manifest validates")
}

async fn connect_echo(app: &Router) -> String {
    let (status, receipt) = call(
        app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(echo_manifest()).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, instance) = call(
        app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    assert_eq!(
        instance["tools"],
        json!(["echo.post"]),
        "the check operation is not a tool: {instance}"
    );
    instance["instance_id"].as_str().unwrap().to_owned()
}

fn post_call() -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
        "c1",
        "echo.post",
        json!({"text": "hello board"}),
    )])
}

#[tokio::test]
async fn a_connection_revoked_while_an_approval_waits_cannot_be_used_by_the_approved_action() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-connection-lifecycle-{}",
        uuid::Uuid::new_v4()
    ));
    let (app, seen) = app_at(
        &store,
        vec![
            post_call(),
            ChatMessage::assistant("the board refused it; nothing was posted"),
        ],
    );
    let instance_id = connect_echo(&app).await;

    // The run pauses before the post; the approval names the connection
    // the post would run through.
    let (status, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (_, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({ "input": { MESSAGES_CHANNEL: [{"role": "user", "content": "post hello board"}] } })),
    )
    .await;
    assert_eq!(run["status"], "interrupted", "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    let request = &list["approvals"][0]["requests"][0];
    assert_eq!(request["tool"], "echo.post");
    assert_eq!(
        request["connection"]["instance_id"],
        json!(instance_id),
        "{list}"
    );
    assert_eq!(request["connection"]["name"], "Echo Board");
    assert!(request["connection"].get("revoked_at").is_none(), "{list}");

    // Revoked while it waits: the list says so, before anyone approves.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/connectors/instances/{instance_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
    let request = &list["approvals"][0]["requests"][0];
    assert!(
        request["connection"]["revoked_at"].is_string(),
        "revoked while waiting: {list}"
    );
    let (_, instances) = call(&app, "GET", "/connectors/instances", None).await;
    assert!(
        instances["instances"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "{instances}"
    );

    // Approving resumes the run; the post cannot use the connection — the
    // refusal names it, and the model is told. Nothing was sent.
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let resumed = decided["resumed_run_id"].as_str().unwrap().to_owned();
    let run = wait_terminal(&app, &resumed).await;
    assert_eq!(run["status"], "success", "{run}");
    let told = format!(
        "{:?}",
        seen.lock()
            .unwrap()
            .last()
            .expect("the model was asked again")
    );
    assert!(
        told.contains("was revoked")
            && told.contains("nothing was sent")
            && told.contains("Echo Board"),
        "the model was told why: {told}"
    );

    // The run's evidence names the connection it ran through, revoked.
    assert_eq!(
        run["connections"][0]["instance_id"],
        json!(instance_id),
        "{run}"
    );
    assert_eq!(run["connections"][0]["name"], "Echo Board");
    assert!(run["connections"][0]["revoked_at"].is_string(), "{run}");
    assert_eq!(
        run["connections"][0]["tools"],
        json!([{"tool": "echo.post", "calls": 1}])
    );

    // A restart keeps the history: the old run still names its connection.
    drop(app);
    let (app, _) = app_at(&store, vec![]);
    let mut named = Value::Null;
    for _ in 0..100 {
        let (_, run) = call(&app, "GET", &format!("/runs/{resumed}"), None).await;
        if !run["connections"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true)
        {
            named = run;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        named["connections"][0]["name"], "Echo Board",
        "after a restart: {named}"
    );
    assert!(named["connections"][0]["revoked_at"].is_string());

    let _ = std::fs::remove_dir_all(store);
}
