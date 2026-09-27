//! Cache hits, measured: a model call's cached prompt tokens land in the
//! journal, a run sums them (`GET /runs/{id}.usage`), and the providers
//! page shows the hit rate per model over the newest runs.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Usage};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Reports what a provider with prefix caching reports: 400 prompt tokens,
/// 360 of them served from the cache.
struct Cached;
#[async_trait::async_trait]
impl ChatModel for Cached {
    async fn chat(&self, _messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("noted"),
            model: Some("cached-model".into()),
            usage: Some(Usage {
                prompt_tokens: 400,
                completion_tokens: 5,
                total_tokens: 405,
                cached_tokens: Some(360),
                ..Usage::default()
            }),
        })
    }
}

fn app() -> (Router, std::path::PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-cache-hits-{}", uuid::Uuid::new_v4()));
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(Cached), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    (
        router(
            registry,
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
        ),
        store,
    )
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
async fn cached_prompt_tokens_are_journaled_summed_per_run_and_rated_per_model() {
    let (app, store) = app();
    let (_, thread) = call(
        &app,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": {"messages": [{"role": "user", "content": "hello"}]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    // The journal keeps the provider's cache figure on the model call.
    let (_, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    let model_call = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "model_call")
        .expect("a model call");
    assert_eq!(
        model_call["tokens"]["cached_tokens"],
        json!(360),
        "{model_call}"
    );

    // The run sums it.
    let (status, run) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["usage"]["model_calls"], json!(1));
    assert_eq!(run["usage"]["prompt_tokens"], json!(400));
    assert_eq!(run["usage"]["cached_tokens"], json!(360));
    assert_eq!(run["usage"]["cache_reported_calls"], json!(1));
    assert!(
        (run["usage"]["cache_hit_rate"].as_f64().unwrap() - 0.9).abs() < 1e-9,
        "{run}"
    );

    // The providers page rates it per model over the newest runs.
    let (status, providers) = call(&app, "GET", "/llm/providers?cache=1", None).await;
    assert_eq!(status, StatusCode::OK, "{providers}");
    let per_model = &providers["cache"]["models"]["cached-model"];
    assert_eq!(per_model["calls"], json!(1), "{providers}");
    assert_eq!(per_model["cached_tokens"], json!(360));
    assert!((per_model["cache_hit_rate"].as_f64().unwrap() - 0.9).abs() < 1e-9);

    let _ = std::fs::remove_dir_all(store);
}
