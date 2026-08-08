//! Runtime tool-effect admission integration tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use rusty_agent_runtime::effects::{
    ApprovalToken, CompensationHandler, CompensationRegistry, EffectAdmissionContext,
    EffectRequest, EffectViolation,
};
use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::executor::{Executor, RunConfig};
use rusty_agent_runtime::graph::GraphBuilder;
use rusty_agent_runtime::journal::Journal;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::middleware::{Decision, Middleware, MiddlewareChain, ToolInvocation};
use rusty_agent_runtime::node::{NodeConfig, NodeContext, NodeOutput};
use rusty_agent_runtime::react::{create_react_agent, TOOLS_NODE};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::replay::RecordingTool;
use rusty_agent_runtime::state::{State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolExecutor, ToolRegistry};

struct CountingTool {
    name: &'static str,
    effect: Effect,
    keyed: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "A test tool with a visible invocation count."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.keyed
            .then(|| format!("{}:{}", self.name, args.get("id").unwrap_or(&Value::Null)))
    }

    async fn call(&self, args: Value) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(args)
    }
}

fn registry_with(tool: CountingTool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    registry
}

fn result_content(results: &[ChatMessage]) -> &str {
    results[0].content.as_deref().expect("tool result content")
}

#[test]
fn runtime_context_enforces_the_full_effect_ladder() {
    let scope = "run-17";
    let pure = EffectRequest::new("rank", Effect::Pure, &json!({"q": "rust"}), None);
    let read = EffectRequest::new("fetch", Effect::ReadOnly, &json!({"id": 7}), None);
    let keyless = EffectRequest::new("upsert", Effect::Idempotent, &json!({"id": 7}), None);
    let keyed = EffectRequest::new(
        "upsert",
        Effect::Idempotent,
        &json!({"id": 7}),
        Some("doc:7".into()),
    );
    let reserving = EffectRequest::new(
        "reserve",
        Effect::Compensatable,
        &json!({"seat": "14A"}),
        None,
    );
    let sending = EffectRequest::new(
        "send_email",
        Effect::NonIdempotent,
        &json!({"to": "operator@example.com"}),
        None,
    );

    let empty = EffectAdmissionContext::new(scope);
    assert!(empty.admit(&pure).is_ok());
    assert!(empty.admit(&read).is_ok());
    assert_eq!(
        empty.admit(&keyless).unwrap_err(),
        EffectViolation::MissingIdempotencyKey {
            kind: "upsert".into()
        }
    );
    assert!(empty.admit(&keyed).is_ok());
    assert_eq!(
        empty.admit(&reserving).unwrap_err(),
        EffectViolation::MissingCompensation {
            kind: "reserve".into()
        }
    );
    assert!(matches!(
        empty.admit(&sending),
        Err(EffectViolation::MissingApproval { .. })
    ));

    let mut compensations = CompensationRegistry::new();
    let rollback: CompensationHandler =
        Arc::new(|output| Ok(json!({"cancelled": output.get("seat")})));
    compensations.register("reserve", rollback);
    let approval = ApprovalToken::approve(sending.effect_id(scope), "policy:reviewed");
    let admitted = EffectAdmissionContext::new(scope)
        .with_compensations(compensations)
        .with_approvals([approval]);
    assert!(admitted.admit(&reserving).unwrap().compensation().is_some());
    assert_eq!(
        admitted.admit(&sending).unwrap().effect_id(),
        &sending.effect_id(scope)
    );
    assert!(matches!(
        admitted.admit(&sending),
        Err(EffectViolation::MissingApproval { .. })
    ));
}

#[tokio::test]
async fn enforcement_is_opt_in_and_exact_approval_gates_the_body() {
    let calls = Arc::new(AtomicUsize::new(0));
    let args = json!({"message": "ship it"});
    let call = ToolCall::new("c1", "publish", args.clone());

    let legacy = ToolExecutor::new(registry_with(CountingTool {
        name: "publish",
        effect: Effect::NonIdempotent,
        keyed: false,
        calls: calls.clone(),
    }));
    let results = legacy.execute_batch(std::slice::from_ref(&call)).await;
    assert_eq!(result_content(&results), args.to_string());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    calls.store(0, Ordering::SeqCst);
    let strict_registry = registry_with(CountingTool {
        name: "publish",
        effect: Effect::NonIdempotent,
        keyed: false,
        calls: calls.clone(),
    });
    let denied = ToolExecutor::new(strict_registry.clone())
        .with_effect_admission(EffectAdmissionContext::new("run-17"));
    let results = denied.execute_batch(std::slice::from_ref(&call)).await;
    assert!(result_content(&results).contains("effect admission denied"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let request = strict_registry
        .get("publish")
        .unwrap()
        .effect_request(&call);
    let approval = ApprovalToken::approve(request.effect_id("run-17"), "ops:amjad");
    let allowed = ToolExecutor::new(strict_registry)
        .with_effect_admission(EffectAdmissionContext::new("run-17").with_approvals([approval]));
    let results = allowed.execute_batch(&[call]).await;
    assert_eq!(result_content(&results), args.to_string());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn identical_irreversible_calls_need_distinct_occurrence_approvals() {
    let calls = Arc::new(AtomicUsize::new(0));
    let args = json!({"message": "same payload"});
    let first = ToolCall::new("call-1", "publish", args.clone());
    let second = ToolCall::new("call-2", "publish", args);
    let registry = registry_with(CountingTool {
        name: "publish",
        effect: Effect::NonIdempotent,
        keyed: false,
        calls: calls.clone(),
    });
    let first_request = registry.get("publish").unwrap().effect_request(&first);
    let approval = ApprovalToken::approve(first_request.effect_id("run-17"), "ops:amjad");
    let executor = ToolExecutor::new(registry)
        .with_effect_admission(EffectAdmissionContext::new("run-17").with_approvals([approval]));

    let results = executor.execute_batch(&[first, second]).await;
    assert!(!result_content(&results[0..1]).starts_with("ERROR:"));
    assert!(result_content(&results[1..2]).contains("effect admission denied"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct CustomRequestTool;

#[async_trait]
impl Tool for CustomRequestTool {
    fn name(&self) -> &str {
        "custom"
    }

    fn description(&self) -> &str {
        "Uses a domain-specific effect request."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn effect_request(&self, call: &ToolCall) -> EffectRequest {
        EffectRequest::new(
            "custom_semantic_effect",
            Effect::NonIdempotent,
            &json!({"semantic_id": call.arguments.get("semantic_id")}),
            None,
        )
    }

    async fn call(&self, args: Value) -> Result<Value> {
        Ok(args)
    }
}

#[test]
fn recording_wrapper_preserves_custom_effect_requests() {
    let inner: Arc<dyn Tool> = Arc::new(CustomRequestTool);
    let wrapped = RecordingTool::new(
        inner.clone(),
        Journal::new("run-17", "thread-17", Default::default()),
        "parent-event",
    );
    let call = ToolCall::new("call-1", "custom", json!({"semantic_id": "invoice-9"}));
    assert_eq!(wrapped.effect_request(&call), inner.effect_request(&call));
}

struct RewriteArguments(Value);

#[async_trait]
impl Middleware for RewriteArguments {
    fn name(&self) -> &str {
        "rewrite_arguments"
    }

    async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
        call.set_arguments(self.0.clone());
        Decision::Continue
    }
}

#[tokio::test]
async fn middleware_rewrites_are_admitted_after_the_rewrite() {
    let calls = Arc::new(AtomicUsize::new(0));
    let original = json!({"amount": 10});
    let rewritten = json!({"amount": 99});
    let registry = registry_with(CountingTool {
        name: "charge",
        effect: Effect::NonIdempotent,
        keyed: false,
        calls: calls.clone(),
    });
    let call = ToolCall::new("c1", "charge", original.clone());
    let original_request = registry.get("charge").unwrap().effect_request(&call);
    let original_approval =
        ApprovalToken::approve(original_request.effect_id("run-17"), "ops:original");

    let denied = ToolExecutor::new(registry.clone())
        .with_middleware(MiddlewareChain::new().layer(RewriteArguments(rewritten.clone())))
        .with_effect_admission(
            EffectAdmissionContext::new("run-17").with_approvals([original_approval]),
        );
    let results = denied.execute_batch(std::slice::from_ref(&call)).await;
    assert!(result_content(&results).contains("requires an explicit approval token"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let rewritten_call = ToolCall::new("c1", "charge", rewritten.clone());
    let rewritten_request = registry
        .get("charge")
        .unwrap()
        .effect_request(&rewritten_call);
    let rewritten_approval =
        ApprovalToken::approve(rewritten_request.effect_id("run-17"), "ops:rewritten");
    let allowed = ToolExecutor::new(registry)
        .with_middleware(MiddlewareChain::new().layer(RewriteArguments(rewritten.clone())))
        .with_effect_admission(
            EffectAdmissionContext::new("run-17").with_approvals([rewritten_approval]),
        );
    let results = allowed.execute_batch(&[call]).await;
    assert_eq!(result_content(&results), rewritten.to_string());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn react_tools_node_automatically_uses_context_admission() {
    let calls = Arc::new(AtomicUsize::new(0));
    let args = json!({"message": "hello"});
    let registry = registry_with(CountingTool {
        name: "send",
        effect: Effect::NonIdempotent,
        keyed: false,
        calls: calls.clone(),
    });
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::new()),
    });
    let graph = create_react_agent(model, registry.clone()).unwrap();
    let call = ToolCall::new("c1", "send", args.clone());
    let state = State::from_value(json!({
        "messages": [serde_json::to_value(ChatMessage::assistant_tool_calls(vec![
            call.clone()
        ])).unwrap()]
    }))
    .unwrap();
    let config = NodeConfig {
        thread_id: "run-17".into(),
        ..NodeConfig::default()
    };

    let denied = graph
        .node(TOOLS_NODE)
        .unwrap()
        .run(
            NodeContext::new(state.clone(), config.clone())
                .with_effect_admission(EffectAdmissionContext::new("run-17")),
        )
        .await
        .unwrap();
    let denied: Vec<ChatMessage> =
        serde_json::from_value(denied.updates["messages"].clone()).unwrap();
    assert!(result_content(&denied).contains("effect admission denied"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let request = registry.get("send").unwrap().effect_request(&call);
    let approval = ApprovalToken::approve(request.effect_id("run-17"), "ops:amjad");
    let allowed = graph
        .node(TOOLS_NODE)
        .unwrap()
        .run(NodeContext::new(state, config).with_effect_admission(
            EffectAdmissionContext::new("run-17").with_approvals([approval]),
        ))
        .await
        .unwrap();
    let allowed: Vec<ChatMessage> =
        serde_json::from_value(allowed.updates["messages"].clone()).unwrap();
    assert_eq!(result_content(&allowed), args.to_string());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn executor_propagates_run_approvals_into_node_contexts() {
    let request = EffectRequest::new(
        "publish",
        Effect::NonIdempotent,
        &json!({"release": "0.8"}),
        None,
    );
    let approval = ApprovalToken::approve(request.effect_id("run-17"), "ops:amjad");
    let admitted = Arc::new(AtomicUsize::new(0));
    let first_seen = admitted.clone();
    let first_request = request.clone();
    let mut builder = GraphBuilder::new();
    builder.add_node("consume", move |ctx: NodeContext| {
        let request = first_request.clone();
        let seen = first_seen.clone();
        async move {
            let context = ctx
                .effect_admission()
                .ok_or_else(|| RustyError::Node("missing effect admission context".into()))?;
            context
                .admit(&request)
                .map_err(|error| RustyError::Node(error.to_string()))?;
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(NodeOutput::empty())
        }
    });
    let second_seen = admitted.clone();
    builder.add_node("verify_spent", move |ctx: NodeContext| {
        let request = request.clone();
        let seen = second_seen.clone();
        async move {
            let context = ctx
                .effect_admission()
                .ok_or_else(|| RustyError::Node("missing effect admission context".into()))?;
            if !matches!(
                context.admit(&request),
                Err(EffectViolation::MissingApproval { .. })
            ) {
                return Err(RustyError::Node(
                    "approval token was reusable in a later super-step".into(),
                ));
            }
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(NodeOutput::empty())
        }
    });
    builder.add_edge("consume", "verify_spent");
    builder.set_entry_point("consume");
    let graph = builder.compile().unwrap();

    Executor::new()
        .with_effect_admission(CompensationRegistry::new())
        .run(
            &graph,
            &StateSpec::new(),
            State::new(),
            RunConfig::new("run-17").with_effect_approvals([approval]),
        )
        .await
        .unwrap();
    assert_eq!(admitted.load(Ordering::SeqCst), 2);
}
