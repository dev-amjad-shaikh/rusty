//! Sandbox executor conformance tests (EP-05-S05, EP-05-S12).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::sandbox::{
    ContainerBackend, ContainerConfig, EnforcementLevel, LocalProcessBackend, LocalProcessConfig,
    RemoteBackend, RemoteConfig, SandboxExecutor, SandboxResult, ToolStub,
};
use rusty_agent_runtime::tool::{
    EffectClass, SandboxRequirement, Tool, ToolExecutor, ToolRegistry,
};

// ---------------------------------------------------------------------------
// Mock backends for testing the trait contract
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MockFullBackend {
    id: String,
}

#[async_trait]
impl SandboxExecutor for MockFullBackend {
    async fn send_tools(&self, _tools: &[ToolStub]) -> Result<()> {
        Ok(())
    }

    async fn send_variables(&self, _variables: &Value) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, command: &str, args: &[String]) -> Result<SandboxResult> {
        Ok(SandboxResult {
            stdout: format!("executed {command} with {} args", args.len()),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncated: false,
            duration_ms: 1,
        })
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Full
    }

    fn backend_id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone)]
struct MockPartialBackend {
    id: String,
}

#[async_trait]
impl SandboxExecutor for MockPartialBackend {
    async fn send_tools(&self, _tools: &[ToolStub]) -> Result<()> {
        Ok(())
    }

    async fn send_variables(&self, _variables: &Value) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, command: &str, args: &[String]) -> Result<SandboxResult> {
        Ok(SandboxResult {
            stdout: format!("partial {command} with {} args", args.len()),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncated: false,
            duration_ms: 1,
        })
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Partial
    }

    fn backend_id(&self) -> &str {
        &self.id
    }
}

// ---------------------------------------------------------------------------
// Tool fixtures
// ---------------------------------------------------------------------------

struct ReadNoneTool;

#[async_trait]
impl Tool for ReadNoneTool {
    fn name(&self) -> &str {
        "read_none"
    }
    fn description(&self) -> &str {
        "Read-only, no sandbox required."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        Ok(json!("read_ok"))
    }
}

struct ExecuteRequiredTool;

#[async_trait]
impl Tool for ExecuteRequiredTool {
    fn name(&self) -> &str {
        "execute_required"
    }
    fn description(&self) -> &str {
        "Execute class, sandbox required."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect_class(&self) -> EffectClass {
        EffectClass::Execute
    }
    fn sandbox_requirement(&self) -> SandboxRequirement {
        SandboxRequirement::Required
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        Ok(json!("execute_ok"))
    }
}

// ---------------------------------------------------------------------------
// Trait contract tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sandbox_executor_trait_contract() {
    let backend = MockFullBackend {
        id: "mock_full".into(),
    };

    backend.send_tools(&[]).await.unwrap();
    backend
        .send_variables(&json!({"key": "val"}))
        .await
        .unwrap();
    let result = backend.execute("test", &[]).await.unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert!(!result.truncated);
    assert_eq!(backend.enforcement(), EnforcementLevel::Full);
    assert_eq!(backend.backend_id(), "mock_full");
}

#[tokio::test]
async fn local_process_backend_runs_echo() {
    let config = LocalProcessConfig::new("/tmp")
        .unwrap()
        .allow_program("echo")
        .with_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let backend = LocalProcessBackend::new(config);
    let result = backend.execute("echo", &["hello".into()]).await.unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("hello"));
    assert_eq!(backend.enforcement(), EnforcementLevel::Partial);
    assert_eq!(backend.backend_id(), "local_process");
}

#[tokio::test]
async fn local_process_backend_honors_allowlist() {
    let config = LocalProcessConfig::new("/tmp")
        .unwrap()
        .allow_program("echo")
        .with_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let backend = LocalProcessBackend::new(config);
    let err = backend
        .execute("cat", &["/etc/passwd".into()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not in the policy allowlist"));
}

#[tokio::test]
async fn local_process_backend_timeout_kills() {
    let config = LocalProcessConfig::new("/tmp")
        .unwrap()
        .allow_program("sleep")
        .with_timeout(std::time::Duration::from_millis(100))
        .unwrap();
    let backend = LocalProcessBackend::new(config);
    let result = backend.execute("sleep", &["5".into()]).await.unwrap();
    assert!(result.timed_out);
}

#[test]
fn container_backend_reports_full_enforcement() {
    let config = ContainerConfig::new("rusty-sandbox:latest", "/tmp")
        .unwrap()
        .with_network(false)
        .with_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    let backend = ContainerBackend::new(config);
    assert_eq!(backend.enforcement(), EnforcementLevel::Full);
    assert_eq!(backend.backend_id(), "container");
}

#[test]
fn remote_backend_reports_partial_enforcement() {
    let config = RemoteConfig {
        endpoint: "https://sandbox.example.com".into(),
        credential: None,
        timeout: std::time::Duration::from_secs(30),
    };
    let backend = RemoteBackend::new(config);
    assert_eq!(backend.enforcement(), EnforcementLevel::Partial);
    assert_eq!(backend.backend_id(), "remote");
}

// ---------------------------------------------------------------------------
// ToolExecutor integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_executor_routes_in_process_without_sandbox() {
    let mut registry = ToolRegistry::new();
    registry.register(ReadNoneTool);
    let executor = ToolExecutor::new(registry);
    let result = executor
        .execute_one(&ToolCall::new("c1", "read_none", json!({})))
        .await
        .unwrap();
    assert_eq!(result, json!("read_ok"));
}

#[tokio::test]
async fn tool_executor_routes_sandboxed_with_full_backend() {
    let mut registry = ToolRegistry::new();
    registry.register(ExecuteRequiredTool);
    let sandbox = Arc::new(MockFullBackend {
        id: "mock_full".into(),
    });
    let executor = ToolExecutor::new(registry).with_sandbox(sandbox);
    let result = executor
        .execute_one(&ToolCall::new("c1", "execute_required", json!({})))
        .await
        .unwrap();
    assert!(
        result
            .get("stdout")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("executed")
    );
}

#[tokio::test]
async fn tool_executor_denies_required_on_partial_backend() {
    let mut registry = ToolRegistry::new();
    registry.register(ExecuteRequiredTool);
    let sandbox = Arc::new(MockPartialBackend {
        id: "mock_partial".into(),
    });
    let executor = ToolExecutor::new(registry).with_sandbox(sandbox);
    let err = executor
        .execute_one(&ToolCall::new("c1", "execute_required", json!({})))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("requires full sandbox enforcement"));
    assert!(msg.contains("mock_partial"));
}

#[tokio::test]
async fn tool_executor_fails_when_sandbox_required_but_none_attached() {
    struct ReadRequired;
    #[async_trait]
    impl Tool for ReadRequired {
        fn name(&self) -> &str {
            "read_required"
        }
        fn description(&self) -> &str {
            "Read tool requiring sandbox."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn sandbox_requirement(&self) -> SandboxRequirement {
            SandboxRequirement::Required
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(ReadRequired);
    let executor = ToolExecutor::new(registry);
    let err = executor
        .execute_one(&ToolCall::new("c1", "read_required", json!({})))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no sandbox backend is available"));
}

// ---------------------------------------------------------------------------
// Enforcement level serde round-trip
// ---------------------------------------------------------------------------

#[test]
fn enforcement_level_serde_roundtrip() {
    let full = EnforcementLevel::Full;
    let json = serde_json::to_string(&full).unwrap();
    assert_eq!(json, "\"full\"");
    let decoded: EnforcementLevel = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, EnforcementLevel::Full);

    let partial = EnforcementLevel::Partial;
    let json = serde_json::to_string(&partial).unwrap();
    assert_eq!(json, "\"partial\"");
    let decoded: EnforcementLevel = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, EnforcementLevel::Partial);
}

// ---------------------------------------------------------------------------
// SandboxResult serde round-trip
// ---------------------------------------------------------------------------

#[test]
fn sandbox_result_serde_roundtrip() {
    let result = SandboxResult {
        stdout: "out".into(),
        stderr: "err".into(),
        exit_code: Some(0),
        timed_out: false,
        truncated: false,
        duration_ms: 42,
    };
    let json = serde_json::to_string(&result).unwrap();
    let decoded: SandboxResult = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, result);
}
