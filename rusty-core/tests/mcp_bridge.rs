//! Integration tests: the in-process MCP bridge.
//!
//! Covers discovery parity, dispatch parity, mount-time refusal, and error
//! paths for the in-process MCP server that exposes native Rusty tools.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::mcp::InProcessMcpBridge;
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

// ---------------------------------------------------------------------------
// Test tools
// ---------------------------------------------------------------------------

struct CountingTool {
    name: &'static str,
    effect: Effect,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "A test tool that counts invocations."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    async fn call(&self, _args: Value) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!("ok"))
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input back."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"message": {"type": "string"}}})
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        Ok(args.get("message").cloned().unwrap_or(Value::Null))
    }
}

struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, _args: Value) -> Result<Value> {
        Err(RustyError::Tool("intentional failure".into()))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn registry_with(tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register_shared(tool);
    }
    registry
}

// ---------------------------------------------------------------------------
// (1) Discovery parity: tools/list returns correct metadata.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_lists_tools_with_schemas() {
    let registry = Arc::new(registry_with(vec![
        Arc::new(EchoTool),
        Arc::new(FailingTool),
    ]));
    let bridge = InProcessMcpBridge::new(registry);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"));
    assert!(names.contains(&"fail"));

    let echo = tools.iter().find(|t| t.name == "echo").unwrap();
    assert_eq!(echo.description, "Echoes the input back.");
    assert_eq!(
        echo.input_schema,
        json!({"type": "object", "properties": {"message": {"type": "string"}}})
    );
}

// ---------------------------------------------------------------------------
// (2) Dispatch parity: tools/call executes and returns result.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_executes_tool_and_returns_result() {
    let registry = Arc::new(registry_with(vec![Arc::new(EchoTool)]));
    let bridge = InProcessMcpBridge::new(registry);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();

    let result = client
        .call_tool("echo", json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(result, Value::String("hello".into()));
}

#[tokio::test]
async fn dispatch_counts_invocations() {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(registry_with(vec![Arc::new(CountingTool {
        name: "count",
        effect: Effect::ReadOnly,
        calls: calls.clone(),
    })]));
    let bridge = InProcessMcpBridge::new(registry);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();

    client.call_tool("count", json!({})).await.unwrap();
    client.call_tool("count", json!({})).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// (3) Error handling: tool failure returns isError response.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failing_tool_returns_error() {
    let registry = Arc::new(registry_with(vec![Arc::new(FailingTool)]));
    let bridge = InProcessMcpBridge::new(registry);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();

    let err = client.call_tool("fail", json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("intentional failure"), "got: {msg}");
}

#[tokio::test]
async fn unknown_tool_returns_error() {
    let registry = Arc::new(ToolRegistry::new());
    let bridge = InProcessMcpBridge::new(registry);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();

    let err = client.call_tool("missing", json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown tool: missing"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// (4) Mount-time refusal: non-allowed effect is rejected.
// ---------------------------------------------------------------------------

#[test]
fn mount_refuses_non_allowed_effect() {
    let registry = Arc::new(registry_with(vec![Arc::new(CountingTool {
        name: "write",
        effect: Effect::NonIdempotent,
        calls: Arc::new(AtomicUsize::new(0)),
    })]));

    let result = InProcessMcpBridge::new(registry).client();
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert_eq!(err.tool_name, "write");
    assert_eq!(err.effect, Effect::NonIdempotent);
}

#[tokio::test]
async fn mount_accepts_readonly_and_pure_by_default() {
    let registry = Arc::new(registry_with(vec![
        Arc::new(EchoTool), // ReadOnly
    ]));
    assert!(InProcessMcpBridge::new(registry).client().is_ok());
}

#[tokio::test]
async fn mount_accepts_explicitly_allowed_effect() {
    let registry = Arc::new(registry_with(vec![Arc::new(CountingTool {
        name: "write",
        effect: Effect::NonIdempotent,
        calls: Arc::new(AtomicUsize::new(0)),
    })]));

    let bridge = InProcessMcpBridge::new(registry).with_allowed_effects(vec![
        Effect::Pure,
        Effect::ReadOnly,
        Effect::NonIdempotent,
    ]);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "write");
}

// ---------------------------------------------------------------------------
// (5) Multiple clients from the same bridge see the same tools.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_clients_share_server() {
    let registry = Arc::new(registry_with(vec![Arc::new(EchoTool)]));
    let bridge = InProcessMcpBridge::new(registry);

    let client_a = bridge.client().unwrap();
    client_a.initialize().await.unwrap();
    let client_b = bridge.client().unwrap();
    client_b.initialize().await.unwrap();

    let tools_a = client_a.list_tools().await.unwrap();
    let tools_b = client_b.list_tools().await.unwrap();
    assert_eq!(tools_a.len(), 1);
    assert_eq!(tools_b.len(), 1);
}

// ---------------------------------------------------------------------------
// (6) into_tools returns McpToolAdapter wrappers.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn into_tools_produces_callable_adapters() {
    let registry = Arc::new(registry_with(vec![Arc::new(EchoTool)]));
    let bridge = InProcessMcpBridge::new(registry);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();

    let tools = client.into_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "echo");

    let result = tools[0].call(json!({"message": "hi"})).await.unwrap();
    assert_eq!(result, Value::String("hi".into()));
}
