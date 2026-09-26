//! A provider's prices, kept and served: input, output and the rate for
//! prompt tokens the provider served from its cache — without which
//! cached tokens would bill at the full input rate and every cost after
//! the manifest moved last would read too high.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ModelPricing, Usage};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Quiet;

#[async_trait::async_trait]
impl ChatModel for Quiet {
    async fn chat(&self, _m: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse { message: ChatMessage::assistant("noted"), model: Some("quiet".into()), usage: None })
    }
}

fn app(store: &std::path::Path) -> Router {
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(Quiet), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    router(registry, ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf()))
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

#[tokio::test]
async fn a_providers_three_prices_are_kept_and_served_and_cached_tokens_bill_at_their_own_rate() {
    let store = std::env::temp_dir().join(format!("rusty-pricing-{}", uuid::Uuid::new_v4()));
    let app = app(&store);
    let (status, saved) = call(&app, "PUT", "/llm/providers", Some(json!({
        "providers": [{"id": "fw", "name": "Fireworks", "base_url": "https://api.fireworks.ai/inference/v1", "model": "accounts/fireworks/models/glm-5p2", "api_key": "not-a-real-key",
                       "price_input_per_m": 1.40, "price_output_per_m": 4.40, "price_cached_input_per_m": 0.14}],
        "primary": "fw", "fallback": null
    }))).await;
    assert!(status.is_success(), "{saved}");
    let (status, served) = call(&app, "GET", "/llm/providers", None).await;
    assert_eq!(status, StatusCode::OK, "{served}");
    let fw = served["providers"].as_array().unwrap().iter().find(|p| p["id"] == "fw").expect("the provider");
    assert_eq!(fw["price_input_per_m"], json!(1.40));
    assert_eq!(fw["price_output_per_m"], json!(4.40));
    assert_eq!(fw["price_cached_input_per_m"], json!(0.14), "{fw}");

    // The rate the client bills with: a prompt three-quarters served from
    // the cache costs a fraction of the same prompt cold.
    let pricing = ModelPricing::new(1.40, 4.40).with_cached_input(0.14);
    let cold = pricing.cost_usd(&Usage { prompt_tokens: 4_000, completion_tokens: 100, total_tokens: 4_100, cached_tokens: None, reasoning_tokens: None });
    let warm = pricing.cost_usd(&Usage { prompt_tokens: 4_000, completion_tokens: 100, total_tokens: 4_100, cached_tokens: Some(3_000), reasoning_tokens: None });
    assert!((cold - (4_000.0 * 1.40 + 100.0 * 4.40) / 1e6).abs() < 1e-9);
    assert!((warm - (1_000.0 * 1.40 + 3_000.0 * 0.14 + 100.0 * 4.40) / 1e6).abs() < 1e-9);
    assert!(warm < cold / 2.0, "cold {cold} warm {warm}");

    let _ = std::fs::remove_dir_all(store);
}
