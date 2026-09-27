//! A platform door acts in the calling run's tenant, as its actor — not in
//! the default tenant (Astra R03). Two tenants each own an agent; a run in
//! one asks `agents.list` and sees its own tenant's agents only.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::llm::Role as ChatRole;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::ToolSource;
use rusty_agent_server::{router, GraphRegistry, PlatformTools, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Calls `agents.list` once, then answers with what came back.
struct ListingModel;

#[async_trait]
impl ChatModel for ListingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let tool_result = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::Tool)
            .and_then(|m| m.content.clone());
        let message = match tool_result {
            Some(result) => ChatMessage::assistant(format!("agents here: {result}")),
            None => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "agents.list",
                json!({}),
            )]),
        };
        Ok(ChatResponse {
            message,
            model: Some("listing-test".into()),
            usage: None,
        })
    }
}

fn test_app() -> (Router, PathBuf) {
    let store =
        std::env::temp_dir().join(format!("rusty-tenant-authority-{}", uuid::Uuid::new_v4()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(Arc::new(ListingModel), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools)
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(registry, config), store)
}

async fn call(
    app: &Router,
    key: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Api-Key", key);
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn own_agent(app: &Router, key: &str, name: &str) {
    let (status, body) = call(app, key, "POST", "/assistants", Some(json!({
        "assistant_id": "desk", "name": name, "graph": "react_agent",
        "config": {"studio_intent": {"instructions": "List the agents.", "tools": [{"name": "agents.list"}]}},
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_platform_door_acts_in_the_calling_runs_tenant_not_the_default() {
    let (app, store) = test_app();
    own_agent(&app, "acme-secret", "Acme Desk").await;
    own_agent(&app, "globex-secret", "Globex Desk").await;
    for (key, mine, theirs) in [
        ("acme-secret", "Acme Desk", "Globex Desk"),
        ("globex-secret", "Globex Desk", "Acme Desk"),
    ] {
        let (status, thread) = call(
            &app,
            key,
            "POST",
            "/threads",
            Some(json!({"graph": "react_agent"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{thread}");
        let thread_id = thread["thread_id"].as_str().unwrap();
        let (status, terminal) = call(
            &app,
            key,
            "POST",
            &format!("/threads/{thread_id}/runs/wait"),
            Some(json!({"input": {"messages": [{"role": "user", "content": "which agents are here?"}]}, "assistant_id": "desk"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{terminal}");
        assert_eq!(terminal["status"], "success", "{terminal}");
        let said = terminal["output"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["role"] == "assistant")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .to_owned();
        assert!(said.contains(mine), "{key}: {said}");
        assert!(
            !said.contains(theirs),
            "{key} saw another tenant's agent: {said}"
        );
        // The run's execution block names the tenant the door acted in.
        let run_id = terminal["run_id"].as_str().unwrap();
        let (_, run) = call(&app, key, "GET", &format!("/runs/{run_id}"), None).await;
        assert_eq!(
            run["metadata"]["execution"]["tenant"],
            json!(key.trim_end_matches("-secret")),
            "{run}"
        );
    }
    let _ = std::fs::remove_dir_all(store);
}
