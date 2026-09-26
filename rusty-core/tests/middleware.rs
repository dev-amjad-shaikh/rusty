//! Integration tests: the Middleware/Interceptor SDK end-to-end through the
//! super-step executor.
//!
//! Covers:
//! (a) stacked layers wrap every node invocation in onion order (before
//!     hooks in registration order, after hooks in reverse), with inbound
//!     state mutations visible to nodes;
//! (b) a before-hook rejection short-circuits the node and fails the run
//!     with the rejection's canonical structured message — never a panic;
//! (c) a before-hook short-circuit returns a substitute node output that
//!     merges and routes like a real one;
//! (d) the chain reaches tool calls inside nodes via
//!     `NodeContext::middleware` + `ToolExecutor::with_middleware`
//!     (ToolCallBlocklist integration);
//! (e) the chain reaches model calls inside nodes via `MiddlewareChatModel`;
//! (f) RequestLogger emits tracing events without changing the outcome.
//!
//! No network access; all nodes, models, and tools are in-memory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool::Tool;
use serde_json::{json, Value};

/// Shared, thread-safe record of hook/node invocations, for order proofs.
#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);

impl Trace {
    fn record(&self, entry: impl Into<String>) {
        self.0.lock().expect("trace lock").push(entry.into());
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().expect("trace lock").clone()
    }
}

// ---------------------------------------------------------------------------
// (a) Stacked layers: onion ordering + mutation through the executor.
// ---------------------------------------------------------------------------

/// Records node hooks as `Lx:before|after:<node>@<step>`; L1 also injects a
/// state channel inbound to prove mutation propagation into the node.
struct Recorder {
    id: &'static str,
    trace: Trace,
    inject: bool,
}

#[async_trait]
impl Middleware for Recorder {
    fn name(&self) -> &str {
        self.id
    }

    async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
        self.trace.record(format!(
            "{}:before:{}@{}",
            self.id,
            call.node(),
            call.step()
        ));
        if self.inject {
            call.insert("injected", json!(true));
        }
        Decision::Continue
    }

    async fn after_node(&self, call: &NodeCall, _output: &mut NodeOutput) -> Decision<NodeOutput> {
        self.trace
            .record(format!("{}:after:{}@{}", self.id, call.node(), call.step()));
        Decision::Continue
    }
}

#[tokio::test]
async fn stacked_layers_wrap_each_node_in_onion_order() {
    let trace = Trace::default();
    let spec = StateSpec::new()
        .channel("log", Reducer::Append)
        .channel("injected", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    for name in ["a", "b"] {
        let trace = trace.clone();
        builder.add_node(name, move |ctx: NodeContext| {
            let trace = trace.clone();
            async move {
                // The L1 before-hook's state mutation is visible here.
                assert_eq!(
                    ctx.state().get("injected"),
                    Some(&json!(true)),
                    "node {name} must observe the before-hook mutation"
                );
                // The chain reaches node code through the context.
                assert_eq!(ctx.middleware().len(), 2);
                trace.record(format!("run:{name}@{}", ctx.step()));
                Ok(NodeOutput::update("log", json!(name)))
            }
        });
    }
    builder.set_entry_point("a");
    builder.add_edge("a", "b");
    let graph = builder.compile().expect("valid graph compiles");

    let executor = Executor::new()
        .layer(Recorder {
            id: "L1",
            trace: trace.clone(),
            inject: true,
        })
        .layer(Recorder {
            id: "L2",
            trace: trace.clone(),
            inject: false,
        });
    assert_eq!(
        executor.middleware().names().collect::<Vec<_>>(),
        ["L1", "L2"]
    );

    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-mw-order"))
        .await
        .expect("run succeeds");

    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(state.get("log"), Some(&json!(["a", "b"])));
        }
        other => panic!("expected Done, got {other:?}"),
    }

    // One node per super-step, so the interleaving is fully deterministic:
    // before-hooks in registration order, after-hooks in reverse, per node.
    assert_eq!(
        trace.entries(),
        vec![
            "L1:before:a@0",
            "L2:before:a@0",
            "run:a@0",
            "L2:after:a@0",
            "L1:after:a@0",
            "L1:before:b@1",
            "L2:before:b@1",
            "run:b@1",
            "L2:after:b@1",
            "L1:after:b@1",
        ]
    );
}

// ---------------------------------------------------------------------------
// (b) Rejection at the node boundary fails the run structurally.
// ---------------------------------------------------------------------------

/// Rejects every run of node `victim` with a structured reason.
struct Guard {
    victim: &'static str,
}

#[async_trait]
impl Middleware for Guard {
    fn name(&self) -> &str {
        "guard"
    }

    async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
        if call.node() == self.victim {
            Decision::Reject(
                Rejection::new(self.name(), InterceptPoint::NodeRun, "policy")
                    .with_detail(format!("node `{}` requires approval", self.victim)),
            )
        } else {
            Decision::Continue
        }
    }
}

#[tokio::test]
async fn reject_at_node_boundary_fails_run_with_structured_error() {
    let ran_b = Arc::new(AtomicBool::new(false));
    let spec = StateSpec::new().channel("log", Reducer::Append);

    let mut builder = GraphBuilder::new();
    builder.add_node("a", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("a")))
    });
    let flag = ran_b.clone();
    builder.add_node("b", move |_ctx: NodeContext| {
        let flag = flag.clone();
        async move {
            flag.store(true, Ordering::SeqCst);
            Ok(NodeOutput::update("log", json!("b")))
        }
    });
    builder.set_entry_point("a");
    builder.add_edge("a", "b");
    let graph = builder.compile().expect("valid graph compiles");

    let err = Executor::new()
        .layer(Guard { victim: "b" })
        .run(&graph, &spec, State::new(), RunConfig::new("t-mw-reject"))
        .await
        .unwrap_err();

    // The rejection surfaces as a node-run failure carrying the Rejection's
    // canonical structured message (which layer, which point, which reason).
    match err {
        RustyError::Node(message) => {
            assert!(
                message.contains("rejected by middleware `guard` at node_run: policy"),
                "got: {message}"
            );
            assert!(message.contains('b'), "got: {message}");
        }
        other => panic!("expected RustyError::Node, got {other:?}"),
    }
    // The rejected node never executed.
    assert!(!ran_b.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// (c) Short-circuit at the node boundary returns a substitute output.
// ---------------------------------------------------------------------------

/// Short-circuits node `victim` with a substitute output.
struct Skip {
    victim: &'static str,
}

#[async_trait]
impl Middleware for Skip {
    fn name(&self) -> &str {
        "skip"
    }

    async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
        if call.node() == self.victim {
            Decision::ShortCircuit(
                NodeOutput::update("log", json!("b-substitute"))
                    .with_update("result", json!("from-middleware")),
            )
        } else {
            Decision::Continue
        }
    }
}

#[tokio::test]
async fn short_circuit_substitutes_node_output_and_run_continues() {
    let ran_b = Arc::new(AtomicBool::new(false));
    let ran_c = Arc::new(AtomicBool::new(false));
    let spec = StateSpec::new()
        .channel("log", Reducer::Append)
        .channel("result", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("a", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("a")))
    });
    let flag = ran_b.clone();
    builder.add_node("b", move |_ctx: NodeContext| {
        let flag = flag.clone();
        async move {
            flag.store(true, Ordering::SeqCst);
            Ok(NodeOutput::update("log", json!("b")))
        }
    });
    let flag = ran_c.clone();
    builder.add_node("c", move |ctx: NodeContext| {
        let flag = flag.clone();
        async move {
            flag.store(true, Ordering::SeqCst);
            // The substitute output merged at the barrier like a real one.
            assert_eq!(ctx.state().get("log"), Some(&json!(["a", "b-substitute"])));
            Ok(NodeOutput::empty())
        }
    });
    builder.set_entry_point("a");
    builder.add_edge("a", "b");
    builder.add_edge("b", "c");
    let graph = builder.compile().expect("valid graph compiles");

    let outcome = Executor::new()
        .layer(Skip { victim: "b" })
        .run(&graph, &spec, State::new(), RunConfig::new("t-mw-skip"))
        .await
        .expect("run succeeds despite the skipped node");

    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(state.get("log"), Some(&json!(["a", "b-substitute"])));
            assert_eq!(state.get("result"), Some(&json!("from-middleware")));
        }
        other => panic!("expected Done, got {other:?}"),
    }
    // b never ran, but routing continued to c over the substitute output.
    assert!(!ran_b.load(Ordering::SeqCst));
    assert!(ran_c.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// (d) ToolCallBlocklist reaches tool calls inside nodes.
// ---------------------------------------------------------------------------

struct DangerTool;

#[async_trait]
impl Tool for DangerTool {
    fn name(&self) -> &str {
        "danger"
    }
    fn description(&self) -> &str {
        "Must never execute."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        panic!("blocklisted tool executed");
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its `text` argument."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

#[tokio::test]
async fn tool_blocklist_reaches_tool_calls_inside_nodes() {
    let spec = StateSpec::new().channel("results", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("tools", |ctx: NodeContext| async move {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry.register(DangerTool);
        // The chain plumbing: executor layers reach this node's tool calls
        // through the context.
        let executor = ToolExecutor::new(registry)
            .with_middleware(ctx.middleware().clone())
            .with_call_context(ctx.thread_id(), "tools");
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"text": "allowed"})),
            ToolCall::new("c2", "danger", json!({})),
        ];
        let results = executor.execute_batch(&calls).await;
        let contents: Vec<&str> = results
            .iter()
            .map(|m| m.content.as_deref().unwrap_or(""))
            .collect();
        Ok(NodeOutput::update("results", json!(contents)))
    });
    builder.set_entry_point("tools");
    let graph = builder.compile().expect("valid graph compiles");

    let outcome = Executor::new()
        .layer(ToolCallBlocklist::new(["danger"]))
        .run(&graph, &spec, State::new(), RunConfig::new("t-mw-tools"))
        .await
        .expect("a blocked tool does not fail the run");

    let state = outcome.state().clone();
    let results = state
        .get("results")
        .and_then(Value::as_array)
        .expect("results channel must exist");
    assert_eq!(results[0], json!("allowed"));
    let blocked = results[1].as_str().unwrap();
    assert!(blocked.starts_with("ERROR:"), "got: {blocked}");
    assert!(blocked.contains("tool_call_blocklist"), "got: {blocked}");
    assert!(blocked.contains("tool_blocked"), "got: {blocked}");
}

// ---------------------------------------------------------------------------
// (e) MiddlewareChatModel reaches model calls inside nodes.
// ---------------------------------------------------------------------------

/// A mock model answering with its message count.
struct CountingModel;

#[async_trait]
impl ChatModel for CountingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant(format!("saw {} messages", messages.len())),
            model: None,
            usage: None,
        })
    }
}

/// Injects a system message inbound, uppercases the content outbound.
struct Shout;

#[async_trait]
impl Middleware for Shout {
    fn name(&self) -> &str {
        "shout"
    }

    async fn before_model(&self, call: &mut ModelCall) -> Decision<ChatResponse> {
        call.messages_mut().insert(0, ChatMessage::system("rules"));
        Decision::Continue
    }

    async fn after_model(
        &self,
        _call: &ModelCall,
        response: &mut ChatResponse,
    ) -> Decision<ChatResponse> {
        if let Some(content) = &mut response.message.content {
            *content = content.to_uppercase();
        }
        Decision::Continue
    }
}

#[tokio::test]
async fn middleware_chat_model_reaches_model_calls_inside_nodes() {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("agent", |ctx: NodeContext| async move {
        let model = MiddlewareChatModel::new(Arc::new(CountingModel), ctx.middleware().clone())
            .thread(ctx.thread_id())
            .node("agent");
        let response = model.chat(&[ChatMessage::user("hi")], &[]).await?;
        Ok(NodeOutput::update(
            "answer",
            json!(response.message.content.unwrap_or_default()),
        ))
    });
    builder.set_entry_point("agent");
    let graph = builder.compile().expect("valid graph compiles");

    let outcome = Executor::new()
        .layer(Shout)
        .run(&graph, &spec, State::new(), RunConfig::new("t-mw-model"))
        .await
        .expect("run succeeds");

    // The system message reached the model (2 messages seen) and the
    // after-hook rewrite reached the state.
    assert_eq!(
        outcome.state().get("answer"),
        Some(&json!("SAW 2 MESSAGES"))
    );
}

// ---------------------------------------------------------------------------
// (f) RequestLogger: observability without behavior change.
// ---------------------------------------------------------------------------

/// A minimal `tracing::Subscriber` recording formatted event fields, so the
/// test can assert on the logger's emissions (same approach as the
/// executor's own instrumentation test).
#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<String>>>,
}

struct FieldVisitor(String);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={:?} ", field.name(), value);
    }
}

impl tracing::Subscriber for EventCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = FieldVisitor(String::new());
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

#[tokio::test]
async fn request_logger_emits_events_without_changing_outcome() {
    let capture = EventCapture::default();
    let events = capture.events.clone();
    // Global default (not thread-local): callsite interest caches are
    // rebuilt per thread, and tokio worker threads must see the subscriber.
    // `set_global_default` may only be called once per process; this is the
    // only test in this binary that installs a subscriber.
    tracing::subscriber::set_global_default(capture)
        .expect("no other test in this binary may install a global tracing subscriber");

    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("only", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("log", json!("x")))
    });
    builder.set_entry_point("only");
    let graph = builder.compile().expect("valid graph compiles");

    let outcome = Executor::new()
        .layer(RequestLogger::new())
        .run(&graph, &spec, State::new(), RunConfig::new("t-mw-logger"))
        .await
        .expect("run succeeds");

    // Identical semantics: the run completes with the expected state.
    assert_eq!(outcome.state().get("log"), Some(&json!(["x"])));

    let captured = events.lock().unwrap();
    assert!(
        captured
            .iter()
            .any(|e| e.contains("node") && e.contains("only")),
        "expected a RequestLogger event naming the node, got: {captured:?}"
    );
}
