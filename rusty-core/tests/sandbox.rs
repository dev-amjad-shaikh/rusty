//! Sandbox executor conformance tests (EP-05-S05, EP-05-S12).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

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
    assert!(result
        .get("stdout")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("executed"));
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

// ---------------------------------------------------------------------------
// R04: one admitted-invocation boundary before placement — a sandboxed call
// reaches its backend only once admitted, exactly like an in-process one.
// ---------------------------------------------------------------------------

/// A Full backend that counts every execution it is handed.
#[derive(Debug, Clone)]
struct CountingBackend {
    id: String,
    executed: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl SandboxExecutor for CountingBackend {
    async fn send_tools(&self, _tools: &[ToolStub]) -> Result<()> {
        Ok(())
    }
    async fn send_variables(&self, _variables: &Value) -> Result<()> {
        Ok(())
    }
    async fn execute(&self, command: &str, _args: &[String]) -> Result<SandboxResult> {
        self.executed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(SandboxResult {
            stdout: format!("{command} ran"),
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

/// An irreversible, sandbox-required tool: the placement class R04 names.
struct DeployTool;

#[async_trait]
impl Tool for DeployTool {
    fn name(&self) -> &str {
        "deploy"
    }
    fn description(&self) -> &str {
        "Ship a build. Irreversible; runs in the sandbox."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> rusty_agent_runtime::record::Effect {
        rusty_agent_runtime::record::Effect::NonIdempotent
    }
    fn effect_class(&self) -> EffectClass {
        EffectClass::Execute
    }
    fn sandbox_requirement(&self) -> SandboxRequirement {
        SandboxRequirement::Required
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        panic!("a sandboxed tool never runs in-process");
    }
}

#[tokio::test]
async fn a_sandboxed_call_is_admitted_before_it_is_placed() {
    use rusty_agent_runtime::effects::{ApprovalToken, EffectAdmissionContext};
    use rusty_agent_runtime::llm::ToolCall;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let executed = Arc::new(AtomicUsize::new(0));
    let backend: Arc<dyn SandboxExecutor> = Arc::new(CountingBackend {
        id: "counting".into(),
        executed: executed.clone(),
    });
    let call = ToolCall::new("c1", "deploy", json!({"build": "1.4.2"}));

    // Refused: an irreversible effect with no approval never reaches the
    // backend — the sandbox is not a way around the boundary.
    let mut registry = ToolRegistry::new();
    registry.register(DeployTool);
    let denied = ToolExecutor::new(registry)
        .with_sandbox(backend.clone())
        .with_effect_admission(EffectAdmissionContext::new("run-r04"));
    let err = denied.execute_one(&call).await.unwrap_err().to_string();
    assert!(err.contains("effect admission denied"), "got: {err}");
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "a refused sandboxed call produced no execution"
    );

    // Admitted: the exact approval for this occurrence lets it through to
    // the backend, once.
    let mut registry = ToolRegistry::new();
    registry.register(DeployTool);
    let approval = ApprovalToken::approve(
        registry
            .get("deploy")
            .unwrap()
            .effect_request(&call)
            .effect_id("run-r04"),
        "ops:amjad",
    );
    let allowed = ToolExecutor::new(registry)
        .with_sandbox(backend.clone())
        .with_effect_admission(EffectAdmissionContext::new("run-r04").with_approvals([approval]));
    let result = allowed.execute_one(&call).await.expect("admitted");
    assert_eq!(result["stdout"], "deploy ran");
    assert_eq!(executed.load(Ordering::SeqCst), 1);
    // The token was consumed by that one occurrence.
    let err = allowed.execute_one(&call).await.unwrap_err().to_string();
    assert!(err.contains("effect admission denied"), "got: {err}");
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

/// The recorded world a shadow serves from: one answer for `deploy`.
#[derive(Debug)]
struct RecordedDeploy(Option<Value>);

impl rusty_agent_runtime::effects::ShadowOutcomeSource for RecordedDeploy {
    fn serve(&self, kind: &str, _recorded_request: &Value) -> Option<Value> {
        (kind == "deploy").then(|| self.0.clone()).flatten()
    }
}

#[tokio::test]
async fn a_shadowed_sandboxed_call_is_served_from_the_record_and_never_placed() {
    use rusty_agent_runtime::effects::{EffectAdmissionContext, ShadowRefusal};
    use rusty_agent_runtime::llm::ToolCall;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let executed = Arc::new(AtomicUsize::new(0));
    let backend: Arc<dyn SandboxExecutor> = Arc::new(CountingBackend {
        id: "counting".into(),
        executed: executed.clone(),
    });
    let refusals: Arc<Mutex<Vec<ShadowRefusal>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let refusals = refusals.clone();
        Arc::new(move |r: &ShadowRefusal| refusals.lock().unwrap().push(r.clone()))
    };
    let call = ToolCall::new("c1", "deploy", json!({"build": "1.4.2"}));

    // Recorded: the shadow answers from the journal; the backend never runs.
    let mut registry = ToolRegistry::new();
    registry.register(DeployTool);
    let served = ToolExecutor::new(registry)
        .with_sandbox(backend.clone())
        .with_effect_admission(EffectAdmissionContext::shadow(
            "run-shadow",
            Arc::new(RecordedDeploy(Some(
                json!({"stdout": "deploy ran (recorded)"}),
            ))),
            sink.clone(),
        ));
    let result = served
        .execute_one(&call)
        .await
        .expect("served from the record");
    assert_eq!(result["stdout"], "deploy ran (recorded)");
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "a shadowed sandboxed call produced no execution"
    );
    assert_eq!(refusals.lock().unwrap().len(), 1);
    assert!(refusals.lock().unwrap()[0].served);

    // Diverged: no recorded answer — the refusal surfaces, still nothing runs.
    let mut registry = ToolRegistry::new();
    registry.register(DeployTool);
    let diverged = ToolExecutor::new(registry)
        .with_sandbox(backend.clone())
        .with_effect_admission(EffectAdmissionContext::shadow(
            "run-shadow",
            Arc::new(RecordedDeploy(None)),
            sink,
        ));
    let err = diverged.execute_one(&call).await.unwrap_err().to_string();
    assert!(err.contains("effect admission denied"), "got: {err}");
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert_eq!(refusals.lock().unwrap().len(), 2);
    assert!(!refusals.lock().unwrap()[1].served);
}

#[tokio::test]
async fn a_sandboxed_call_is_journaled_like_an_in_process_one() {
    use rusty_agent_runtime::effects::{ApprovalToken, EffectAdmissionContext};
    use rusty_agent_runtime::journal::{Clock, Journal};
    use rusty_agent_runtime::llm::ToolCall;
    use rusty_agent_runtime::record::{Effect, RunEventKind};
    use std::sync::atomic::AtomicUsize;

    let backend: Arc<dyn SandboxExecutor> = Arc::new(CountingBackend {
        id: "counting".into(),
        executed: Arc::new(AtomicUsize::new(0)),
    });
    let journal = Journal::new("run-r04-journal", "thread-r04", Clock::System);
    let call = ToolCall::new("c1", "deploy", json!({"build": "1.4.2"}));
    let mut registry = ToolRegistry::new();
    registry.register(DeployTool);
    let approval = ApprovalToken::approve(
        registry
            .get("deploy")
            .unwrap()
            .effect_request(&call)
            .effect_id("run-r04-journal"),
        "ops:amjad",
    );
    let executor = ToolExecutor::new(registry)
        .with_sandbox(backend)
        .with_guard_journal(journal.clone(), "node-input:7")
        .with_effect_admission(
            EffectAdmissionContext::new("run-r04-journal").with_approvals([approval]),
        );

    executor
        .execute_one(&call)
        .await
        .expect("admitted and placed");

    let events = journal.events();
    let recorded: Vec<_> = events
        .iter()
        .filter(|e| e.kind == RunEventKind::ToolCall)
        .collect();
    assert_eq!(
        recorded.len(),
        1,
        "one ToolCall event for the sandboxed call: {events:?}"
    );
    let event = recorded[0];
    assert_eq!(
        event.effect,
        Effect::NonIdempotent,
        "the tool's declared effect, not the backend's"
    );
    assert_eq!(event.parent.as_deref(), Some("node-input:7"));
    let input = event
        .input
        .as_ref()
        .and_then(|i| journal.resolve(i))
        .expect("the input payload");
    assert_eq!(input["tool"], "deploy");
    assert_eq!(input["arguments"]["build"], "1.4.2");
    let output = event
        .output
        .as_ref()
        .and_then(|o| journal.resolve(o))
        .expect("the output payload");
    assert_eq!(output["stdout"], "deploy ran");
}
