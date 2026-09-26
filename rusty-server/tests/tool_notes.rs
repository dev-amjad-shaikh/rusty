//! The builder's word on *when* a tool is for reaches the model on the tool
//! itself, and the run declares it. An assistant whose intent carries
//! `tools: [{name, when}]` runs under the deployment's context policy with
//! that note as the tool's selection overlay: the schema the model reads
//! ends with `When to use: …`, the journaled declaration carries the
//! overlay, and an overlay naming a tool the run cannot call is refused at
//! admission, in words.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::builtins::{CalculatorTool, TextInspectorTool};
use rusty_agent_server::{GraphRegistry, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A model that answers at once and keeps every tools argument it was handed.
struct WatchingModel {
    seen: Arc<Mutex<Vec<Vec<Value>>>>,
}

#[async_trait]
impl ChatModel for WatchingModel {
    async fn chat(&self, _messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        self.seen.lock().unwrap().push(tools.to_vec());
        Ok(ChatResponse {
            message: ChatMessage::assistant("done"),
            model: Some("watching-test".into()),
            usage: None,
        })
    }
}

fn test_app() -> (Router, PathBuf, Arc<Mutex<Vec<Vec<Value>>>>) {
    let store = std::env::temp_dir().join(format!("rusty-tool-notes-{}", uuid::Uuid::new_v4()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    tools.register(TextInspectorTool);
    let model = WatchingModel {
        seen: Arc::clone(&seen),
    };
    let graph = create_react_agent(Arc::new(model), tools.clone()).unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("capable", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_context_policy(rusty_agent_runtime::context::ContextPolicy::standard(8192));
    (router(registry, config), store, seen)
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

async fn create_assistant(app: &Router, id: &str, tools: Value) {
    let (status, value) = call(
        app,
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": id,
            "name": "Noted",
            "graph": "capable",
            "config": {"studio_intent": {"instructions": "Answer plainly.", "tools": tools}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
}

async fn create_thread(app: &Router) -> String {
    let (status, value) = call(app, "POST", "/threads", Some(json!({"graph": "capable"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value["thread_id"].as_str().unwrap().to_string()
}

fn input() -> Value {
    json!({"messages": [{"role": "user", "content": "what is 6 times 7?"}]})
}

#[tokio::test]
async fn the_when_note_reaches_the_model_on_the_tool_and_the_run_declares_it() {
    let (app, store, seen) = test_app();
    create_assistant(
        &app,
        "noted",
        json!([{"name": "calculator", "when": "to multiply numbers the person gives"}, {"name": "inspect_text"}]),
    )
    .await;
    let thread = create_thread(&app).await;
    let (status, terminal) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({"input": input(), "assistant_id": "noted"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    assert_eq!(terminal["status"], "success", "{terminal}");

    // The model read the note on the tool itself; the tool without a note
    // kept its plain description.
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one model call");
        let tools = &seen[0];
        assert_eq!(
            tools.len(),
            2,
            "both allow-listed tools were shown: {tools:?}"
        );
        let calculator = tools
            .iter()
            .find(|t| t["function"]["name"] == "calculator")
            .expect("calculator shown");
        let description = calculator["function"]["description"].as_str().unwrap();
        assert!(
            description.ends_with(" When to use: to multiply numbers the person gives"),
            "{description}"
        );
        let inspector = tools
            .iter()
            .find(|t| t["function"]["name"] == "inspect_text")
            .expect("inspector shown");
        assert!(
            !inspector["function"]["description"]
                .as_str()
                .unwrap()
                .contains("When to use")
        );
    }

    // The run declared the overlay, so a replay re-derives the same request.
    let run_id = terminal["run_id"].as_str().unwrap();
    let (status, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let declared = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "run_config_declared")
        .expect("the run declared its config");
    let overlays = declared
        .pointer("/output/value/tool_overlays")
        .expect("overlays declared");
    assert_eq!(
        overlays["calculator"]["when_to_use"].as_str(),
        Some("to multiply numbers the person gives"),
        "{overlays}"
    );
    assert!(
        overlays.get("inspect_text").is_none(),
        "no note, no overlay: {overlays}"
    );
    assert!(
        declared.pointer("/output/value/tool_outcomes").is_some(),
        "the outcome snapshot is declared too"
    );
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn an_overlay_naming_a_tool_the_run_cannot_call_is_refused_at_admission() {
    let (app, store, seen) = test_app();
    create_assistant(&app, "noted", json!([{"name": "calculator"}])).await;
    let thread = create_thread(&app).await;
    let (status, body) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({
            "input": input(),
            "assistant_id": "noted",
            "config": {"tool_overlays": {"inspect_text": {"when_to_use": "never — it is not on the list"}}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("`inspect_text`, which this run cannot call"),
        "{body}"
    );
    assert!(seen.lock().unwrap().is_empty(), "no model call was made");

    // An overlay that fails its own validation is refused in its words too.
    let long = "x".repeat(2000);
    let (status, body) = call(
        &app,
        "POST",
        &format!("/threads/{thread}/runs/wait"),
        Some(json!({
            "input": input(),
            "assistant_id": "noted",
            "config": {"tool_overlays": {"calculator": {"when_to_use": long}}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("`tool_overlays.calculator`"),
        "{body}"
    );
    let _ = std::fs::remove_dir_all(store);
}
