//! The prebuilt ReAct agent (LangGraph `create_react_agent` parity).
//!
//! [`create_react_agent`] assembles the classic reasoning-acting loop as a
//! two-node cyclic graph over a single `messages` channel
//! ([`Reducer::AddMessages`](crate::state::Reducer::AddMessages)):
//!
//! ```text
//!         ┌──────────────────────────────────────────┐
//!         │                                          │
//!         ▼                                          │
//!      [agent] ── last message has tool_calls? ──► [tools]
//!         │                                          │
//!         └─ no tool_calls ──► End ◄── static edge ──┘
//! ```
//!
//! - **`agent`** — serializes the `messages` channel into
//!   [`ChatMessage`]s, calls [`ChatModel::chat`] with the registry's
//!   OpenAI-format tool schemas, and appends the assistant message (final
//!   answer *or* tool-call request) back onto `messages`.
//! - **`tools`** — takes the `tool_calls` of the last assistant message,
//!   dispatches them in parallel through [`ToolExecutor::execute_batch`],
//!   and appends one `role: "tool"` message per call.
//! - **Routing** — a conditional edge on `agent` routes to `tools` when the
//!   last message carries tool calls, otherwise to [`Route::End`]; a static
//!   edge loops `tools → agent` so the model observes the tool results.
//!
//! The caller drives the returned [`Graph`] with a [`crate::state::StateSpec`]
//! declaring `messages` with `Reducer::AddMessages` and an initial state
//! seeding the conversation (see `examples/react_agent.rs`).
//!
//! Four flavors exist: [`create_react_agent`] (the agent node calls
//! [`ChatModel::chat`]; no [`crate::executor::GraphEvent::Token`] events),
//! [`create_react_agent_streaming`] (the agent node calls
//! [`ChatModel::chat_stream`] and forwards deltas as
//! [`crate::executor::GraphEvent::Token`]s into the run's event channel),
//! and the Flight Recorder pair [`create_react_agent_with_recording`] /
//! [`create_react_agent_replaying`].
//!
//! # Flight Recorder
//!
//! [`create_react_agent_with_recording`] wires the run's [`Journal`] into
//! both nodes: every model call is journaled through
//! [`crate::replay::RecordingChatModel`] and every tool call through
//! [`crate::replay::RecordingTool`], in the canonical
//! [`crate::replay::model_call_request`] / [`crate::replay::tool_call_request`]
//! payload shapes, parented per iteration to the invocation's node-input
//! event (the executor hands its id over via
//! [`crate::journal::PARENT_EVENT_KEY`]). Attach the same journal to the run
//! with [`crate::executor::RunConfig::with_journal`].
//! [`create_react_agent_replaying`] is the mirror image for exact replay:
//! the same topology with [`crate::replay::ReplayingChatModel`] /
//! [`crate::replay::ReplayingTool`] answering from the recorded journal —
//! zero outbound calls, so the wrapped model and tools may be
//! panic-on-call sentinels. See `examples/react_record_replay.rs` for the
//! full record → replay loop.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::error::{Result, RustyError};
use crate::executor::GraphEvent;
use crate::graph::{Graph, GraphBuilder, Route};
use crate::journal::{Journal, PARENT_EVENT_KEY};
use crate::llm::{ChatMessage, ChatModel};
use crate::node::NodeOutput;
use crate::replay::{
    RecordingChatModel, RecordingTool, ReplaySource, ReplayingChatModel, ReplayingTool,
};
use crate::tool::{ToolExecutor, ToolRegistry, TOOL_ALLOWLIST_KEY};

/// The state channel the ReAct loop reads from and appends to. Declare it
/// with `Reducer::AddMessages` in the run's [`crate::state::StateSpec`].
pub const MESSAGES_CHANNEL: &str = "messages";

/// The name of the model-calling node in the compiled graph.
pub const AGENT_NODE: &str = "agent";

/// The name of the tool-dispatch node in the compiled graph.
pub const TOOLS_NODE: &str = "tools";

/// Read and deserialize the `messages` channel from a state snapshot.
///
/// A missing channel yields an empty conversation (the run may legitimately
/// start before any message is seeded); a malformed channel is a hard error.
fn read_messages(state: &crate::state::State) -> Result<Vec<ChatMessage>> {
    Ok(state
        .get_as::<Vec<ChatMessage>>(MESSAGES_CHANNEL)?
        .unwrap_or_default())
}

/// How the prebuilt agent's model and tool calls relate to the Flight
/// Recorder: not at all (the default), journaled live (record mode), or
/// served from a recorded journal (exact-replay mode).
#[derive(Debug, Clone)]
enum EvidenceMode {
    /// No recording — the pre-R0.5 behavior, byte-identical by construction
    /// (the wrappers are never built and no parent key is read).
    None,

    /// Journal every model/tool call through the recording wrappers.
    Record(Journal),

    /// Answer every model/tool call from the recorded journal; the wrapped
    /// implementations are carried for identity and never invoked.
    Replay {
        /// The serving cursor over the recorded run's effects.
        source: ReplaySource,
        /// The replay run's own journal (the recorded run's identity).
        journal: Journal,
    },
}

/// The causal parent for effects a node invocation records: the id of the
/// invocation's node-input journal event, delivered by the executor under
/// [`PARENT_EVENT_KEY`]. A missing key means the graph is being driven by
/// something other than [`crate::executor::Executor::run`] (a hand-rolled
/// harness, a unit test) — evidence recorded without its causal anchor
/// would misrepresent the run, so this is a hard error rather than a
/// silently unparented event.
fn invocation_parent(ctx: &crate::node::NodeContext, node: &str) -> Result<String> {
    ctx.config()
        .extra
        .get(PARENT_EVENT_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            RustyError::Node(format!(
                "node `{node}` is wired for Flight Recorder evidence but the run supplied no \
                 `{PARENT_EVENT_KEY}` — drive the graph through `Executor::run`, which hands each \
                 invocation its node-input event id as the causal parent"
            ))
        })
}

/// The registry's OpenAI-format tool schemas in a canonical order.
///
/// [`ToolRegistry`] is `HashMap`-backed, so [`ToolRegistry::schemas`] order
/// is process-random — harmless for a live call, but the Flight Recorder
/// hashes the model-call request payload and exact replay matches on that
/// hash, so the schema list must serialize identically in the recording and
/// replaying graphs (two distinct registry instances). Sorting by tool name
/// makes the request canonical across processes. Applied to every flavor so
/// all variants of the prebuilt agent put identical content on the wire.
fn sorted_tool_schemas(tools: &ToolRegistry) -> Vec<Value> {
    fn tool_name(schema: &Value) -> &str {
        schema
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
    let mut schemas = tools.schemas();
    schemas.sort_by(|a, b| tool_name(a).cmp(tool_name(b)));
    schemas
}

fn invocation_tools(tools: &ToolRegistry, ctx: &crate::node::NodeContext) -> Result<ToolRegistry> {
    let Some(value) = ctx.config().extra.get(TOOL_ALLOWLIST_KEY) else {
        return Ok(tools.clone());
    };
    let allowlist: Vec<String> = serde_json::from_value(value.clone())
        .map_err(|error| RustyError::Node(format!("run tool allowlist is malformed: {error}")))?;
    tools.restricted_to(&allowlist)
}

/// Build a prebuilt ReAct agent graph over `model` and `tools`.
///
/// The returned graph has exactly two nodes ([`AGENT_NODE`], [`TOOLS_NODE`]),
/// a conditional edge `agent → tools | End`, and a static edge
/// `tools → agent`. It is stateless with respect to any single run: clone it
/// freely and drive it with the [`crate::executor::Executor`].
///
/// The graph never errors at build time for an empty registry — a tool-less
/// agent simply answers directly on the first `agent` pass.
///
/// **This variant never emits [`GraphEvent::Token`]:** the agent node calls
/// [`ChatModel::chat`]. Use [`create_react_agent_streaming`] to stream token
/// deltas into the run's event channel.
pub fn create_react_agent(model: Arc<dyn ChatModel>, tools: ToolRegistry) -> Result<Graph> {
    build_react_agent(model, tools, None, EvidenceMode::None)
}

/// Build a prebuilt ReAct agent graph whose `agent` node streams token
/// deltas as [`GraphEvent::Token`]s through `token_tx`
/// ([`ChatModel::chat_stream`] under the hood; LangGraph's `messages`
/// stream mode).
///
/// Typically `token_tx` is a clone of the run's event sender
/// ([`crate::executor::RunConfig::token_tx`]) so token deltas interleave with
/// the executor's own events on one channel. Forwarding is best-effort
/// (`try_send`): a full or closed channel drops tokens but never aborts the
/// run.
///
/// Identical to [`create_react_agent`] in topology and behavior otherwise;
/// models that only implement [`ChatModel::chat`] work unchanged (the
/// trait's default `chat_stream` delivers the whole answer as one token).
pub fn create_react_agent_streaming(
    model: Arc<dyn ChatModel>,
    tools: ToolRegistry,
    token_tx: mpsc::Sender<GraphEvent>,
) -> Result<Graph> {
    build_react_agent(model, tools, Some(token_tx), EvidenceMode::None)
}

/// Build a prebuilt ReAct agent graph that journals every model and tool
/// call into `journal` (Flight Recorder, R0.5).
///
/// Identical to [`create_react_agent`] in topology and behavior; the only
/// delta is evidence: the `agent` node wraps the model in
/// [`crate::replay::RecordingChatModel`] and the `tools` node wraps each
/// dispatched tool in [`crate::replay::RecordingTool`], so the journal
/// gains `model_call` / `tool_call` events in the canonical
/// [`crate::replay::model_call_request`] / [`crate::replay::tool_call_request`]
/// shapes. Each event's causal parent is the invocation's node-input event
/// ([`PARENT_EVENT_KEY`]), so iteration *N*'s model call hangs off iteration
/// *N*'s `agent` input, and each tool call off its `tools` input.
///
/// Attach the same journal to the run ([`crate::executor::RunConfig::with_journal`])
/// so node and executor evidence share one journal; for a byte-identical
/// replay later, record under the determinism seams
/// ([`crate::journal::Clock::logical`] + [`crate::journal::RngSource::seeded`]).
/// Replay the recorded run with [`create_react_agent_replaying`] under
/// [`crate::replay::ExactReplay`].
///
/// There is deliberately no streaming recording flavor: the streaming
/// variant's token forwarding is a live-observability concern, and the
/// recording wrappers record through [`ChatModel::chat`].
pub fn create_react_agent_with_recording(
    model: Arc<dyn ChatModel>,
    tools: ToolRegistry,
    journal: Journal,
) -> Result<Graph> {
    build_react_agent(model, tools, None, EvidenceMode::Record(journal))
}

/// Build a prebuilt ReAct agent graph that answers every model and tool
/// call from a recorded journal instead of executing it (exact replay).
///
/// The replaying analogue of [`create_react_agent_with_recording`]: same
/// topology, but the nodes wrap `model` and `tools` in
/// [`crate::replay::ReplayingChatModel`] / [`crate::replay::ReplayingTool`],
/// which serve each call from `source` (matched by sequence + canonical
/// request hash) and re-journal it into `journal`. **The wrapped model and
/// tools are never invoked** — carry them for their identity (effect class,
/// tool schemas) and pass panic-on-call sentinels to prove the
/// zero-outbound guarantee. The registry must offer the same tool
/// identities (name, description, parameter schema) as the recorded run's:
/// schema content feeds the model-call request hash.
///
/// Build `source` and `journal` from an [`crate::replay::ExactReplay`]
/// session (`source()` / `fresh_journal()`) and drive the graph via
/// [`crate::replay::ExactReplay::run_and_verify`]; see
/// `examples/react_record_replay.rs`.
pub fn create_react_agent_replaying(
    model: Arc<dyn ChatModel>,
    tools: ToolRegistry,
    source: ReplaySource,
    journal: Journal,
) -> Result<Graph> {
    build_react_agent(model, tools, None, EvidenceMode::Replay { source, journal })
}

fn build_react_agent(
    model: Arc<dyn ChatModel>,
    tools: ToolRegistry,
    token_tx: Option<mpsc::Sender<GraphEvent>>,
    evidence: EvidenceMode,
) -> Result<Graph> {
    let agent_tools = tools.clone();
    let tool_executor = ToolExecutor::new(tools);

    let agent_node = {
        let model = Arc::clone(&model);
        let evidence = evidence.clone();
        move |ctx: crate::node::NodeContext| {
            let model = Arc::clone(&model);
            let evidence = evidence.clone();
            let agent_tools = agent_tools.clone();
            let token_tx = token_tx.clone();
            async move {
                let messages = read_messages(ctx.state())?;
                let tool_schemas = sorted_tool_schemas(&invocation_tools(&agent_tools, &ctx)?);
                // Evidence wiring is per invocation: the recording/replaying
                // wrappers carry the invocation's causal parent, which only
                // exists once the executor has journaled the node input.
                let model: Arc<dyn ChatModel> = match &evidence {
                    EvidenceMode::None => match ctx.effect_journal() {
                        Some(journal) => Arc::new(
                            RecordingChatModel::new(
                                model,
                                journal.clone(),
                                invocation_parent(&ctx, AGENT_NODE)?,
                            )
                            .node(AGENT_NODE),
                        ),
                        None => model,
                    },
                    EvidenceMode::Record(journal) => Arc::new(
                        RecordingChatModel::new(
                            model,
                            journal.clone(),
                            invocation_parent(&ctx, AGENT_NODE)?,
                        )
                        .node(AGENT_NODE),
                    ),
                    EvidenceMode::Replay { source, journal } => Arc::new(ReplayingChatModel::new(
                        model,
                        source.clone(),
                        journal.clone(),
                        invocation_parent(&ctx, AGENT_NODE)?,
                    )),
                };
                tracing::debug!(
                    node = AGENT_NODE,
                    messages = messages.len(),
                    tools = tool_schemas.len(),
                    "calling chat model"
                );
                let response = match token_tx {
                    Some(tx) => {
                        model
                            .chat_stream(&messages, &tool_schemas, &mut |chunk| {
                                if !chunk.delta.is_empty() {
                                    let _ = tx.try_send(GraphEvent::Token {
                                        node: AGENT_NODE.to_owned(),
                                        delta: chunk.delta,
                                    });
                                }
                            })
                            .await?
                    }
                    None => model.chat(&messages, &tool_schemas).await?,
                };
                let appended = serde_json::to_value(&response.message)?;
                // A single message object is fine: AddMessages accepts one
                // message or an array and upserts/appends accordingly.
                Ok(NodeOutput::update(MESSAGES_CHANNEL, appended))
            }
        }
    };

    let tools_node = move |ctx: crate::node::NodeContext| {
        let tool_executor = tool_executor.clone();
        let evidence = evidence.clone();
        async move {
            let messages = read_messages(ctx.state())?;
            let last = messages.last().ok_or_else(|| {
                RustyError::Node(format!(
                    "node `{TOOLS_NODE}` ran with an empty `{MESSAGES_CHANNEL}` channel"
                ))
            })?;
            if !last.has_tool_calls() {
                return Err(RustyError::Node(format!(
                    "node `{TOOLS_NODE}` expected the last message to carry tool calls"
                )));
            }
            let tool_names: Vec<&str> = last
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            tracing::debug!(
                node = TOOLS_NODE,
                calls = last.tool_calls.len(),
                tools = ?tool_names,
                "dispatching tool calls"
            );
            // Evidence wiring is per invocation, like the agent node's: in
            // record/replay mode each tool is wrapped with the invocation's
            // causal parent, then dispatched through the same batch executor
            // (parallel, order-preserving, panic-containing).
            let tool_executor =
                ToolExecutor::new(invocation_tools(tool_executor.registry(), &ctx)?);
            let mut tool_executor =
                match &evidence {
                    EvidenceMode::None => match ctx.effect_journal() {
                        Some(journal) => {
                            let parent = invocation_parent(&ctx, TOOLS_NODE)?;
                            let mut wrapped = ToolRegistry::new();
                            for name in tool_executor.registry().names() {
                                let tool = tool_executor.registry().get(name).expect(
                                    "tool names iterated from a registry resolve in that registry",
                                );
                                wrapped.register_shared(Arc::new(
                                    RecordingTool::new(tool, journal.clone(), parent.clone())
                                        .node(TOOLS_NODE),
                                ));
                            }
                            ToolExecutor::new(wrapped)
                        }
                        None => tool_executor,
                    },
                    EvidenceMode::Record(journal) => {
                        let parent = invocation_parent(&ctx, TOOLS_NODE)?;
                        let mut wrapped = ToolRegistry::new();
                        for name in tool_executor.registry().names() {
                            let tool = tool_executor.registry().get(name).expect(
                                "tool names iterated from a registry resolve in that registry",
                            );
                            wrapped.register_shared(Arc::new(
                                RecordingTool::new(tool, journal.clone(), parent.clone())
                                    .node(TOOLS_NODE),
                            ));
                        }
                        ToolExecutor::new(wrapped)
                    }
                    EvidenceMode::Replay { source, journal } => {
                        let parent = invocation_parent(&ctx, TOOLS_NODE)?;
                        let mut wrapped = ToolRegistry::new();
                        for name in tool_executor.registry().names() {
                            let tool = tool_executor.registry().get(name).expect(
                                "tool names iterated from a registry resolve in that registry",
                            );
                            wrapped.register_shared(Arc::new(ReplayingTool::new(
                                tool,
                                source.clone(),
                                journal.clone(),
                                parent.clone(),
                            )));
                        }
                        ToolExecutor::new(wrapped)
                    }
                };
            // The executor attaches both cross-cutting boundaries to the
            // node context. Re-attach them after evidence wrapping so the
            // finalized, post-middleware call is admitted immediately before
            // the recording/replay wrapper (and ultimately the tool) runs.
            tool_executor = tool_executor
                .with_middleware(ctx.middleware().clone())
                .with_call_context(ctx.thread_id(), TOOLS_NODE);
            if let Some(admission) = ctx.effect_admission() {
                tool_executor = tool_executor.with_effect_admission(admission.clone());
            }
            // Per-call error policy: see ToolExecutor::execute_batch docs.
            let results = tool_executor.execute_batch(&last.tool_calls).await;
            let appended = serde_json::to_value(&results)?;
            Ok(NodeOutput::update(MESSAGES_CHANNEL, appended))
        }
    };

    let mut builder = GraphBuilder::new();
    builder.add_node(AGENT_NODE, agent_node);
    builder.add_node(TOOLS_NODE, tools_node);
    builder.set_entry_point(AGENT_NODE);

    // Route on the post-barrier state: the appended assistant message decides.
    builder.add_conditional_edges(AGENT_NODE, |state| async move {
        let needs_tools = read_messages(&state)?
            .last()
            .map(ChatMessage::has_tool_calls)
            .unwrap_or(false);
        Ok(if needs_tools {
            Route::Node(TOOLS_NODE.to_owned())
        } else {
            Route::End
        })
    });
    builder.add_edge(TOOLS_NODE, AGENT_NODE);

    builder.compile()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Edge;
    use crate::llm::{ChatResponse, TokenChunk, ToolCall};
    use crate::node::{Node, NodeConfig, NodeContext};
    use crate::state::{Reducer, State, StateSpec};
    use crate::tool::Tool;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A scripted model: pops one canned response per `chat` call.
    struct ScriptedModel {
        script: Mutex<VecDeque<ChatMessage>>,
        seen_tool_schemas: Mutex<Vec<usize>>,
    }

    impl ScriptedModel {
        fn new(script: Vec<ChatMessage>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                seen_tool_schemas: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ChatModel for ScriptedModel {
        async fn chat(&self, _messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
            self.seen_tool_schemas.lock().unwrap().push(tools.len());
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

    /// A model whose `chat_stream` emits real deltas (accumulating the full
    /// answer, as wire-backed implementations do).
    struct StreamingModel;

    #[async_trait]
    impl ChatModel for StreamingModel {
        async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
            Ok(ChatResponse {
                message: ChatMessage::assistant("streamed"),
                model: None,
                usage: None,
            })
        }
        async fn chat_stream(
            &self,
            messages: &[ChatMessage],
            tools: &[Value],
            on_token: &mut (dyn FnMut(TokenChunk) + Send),
        ) -> Result<ChatResponse> {
            for delta in ["str", "eamed"] {
                on_token(TokenChunk {
                    delta: delta.to_owned(),
                    finish: false,
                    raw: None,
                });
            }
            self.chat(messages, tools).await
        }
    }

    #[tokio::test]
    async fn streaming_variant_forwards_token_events() {
        let (tx, mut rx) = mpsc::channel::<GraphEvent>(8);
        let model: Arc<dyn ChatModel> = Arc::new(StreamingModel);
        let graph = create_react_agent_streaming(model, registry(), tx).unwrap();

        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
        }))
        .unwrap();
        let ctx = NodeContext::new(state, NodeConfig::default());
        let out = graph.node(AGENT_NODE).unwrap().run(ctx).await.unwrap();

        // The accumulated response is appended exactly as in chat().
        let appended = out.updates.get(MESSAGES_CHANNEL).unwrap();
        let msg: ChatMessage = serde_json::from_value(appended.clone()).unwrap();
        assert_eq!(msg.content.as_deref(), Some("streamed"));

        // Both deltas arrived as Token events on the forwarded channel.
        let mut deltas = String::new();
        for _ in 0..2 {
            match rx.try_recv().expect("two token events") {
                GraphEvent::Token { node, delta } => {
                    assert_eq!(node, AGENT_NODE);
                    deltas.push_str(&delta);
                }
                other => panic!("expected Token event, got {other:?}"),
            }
        }
        assert_eq!(deltas, "streamed");
    }

    /// The non-streaming variant must emit no Token events (it calls chat()).
    #[tokio::test]
    async fn non_streaming_variant_emits_no_token_events() {
        let model: Arc<dyn ChatModel> =
            Arc::new(ScriptedModel::new(vec![ChatMessage::assistant("done")]));
        let graph = create_react_agent(model, registry()).unwrap();
        // No token sender is even available to this graph: the assertion is
        // structural (create_react_agent takes no channel), documented here
        // so the two variants do not drift.
        assert!(graph.has_node(AGENT_NODE));
    }

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its input."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn call(&self, args: Value) -> Result<Value> {
            Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Echo);
        r
    }

    #[test]
    fn graph_topology_is_the_react_loop() {
        let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![]));
        let graph = create_react_agent(model, registry()).unwrap();

        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.entry_point(), AGENT_NODE);
        assert!(graph.has_node(AGENT_NODE));
        assert!(graph.has_node(TOOLS_NODE));

        // agent: one conditional edge; tools: one static edge back to agent.
        let agent_edges = graph.outgoing_edges(AGENT_NODE);
        assert_eq!(agent_edges.len(), 1);
        assert!(matches!(agent_edges[0], Edge::Conditional { .. }));
        let tools_edges = graph.outgoing_edges(TOOLS_NODE);
        assert_eq!(tools_edges.len(), 1);
        assert!(matches!(
            tools_edges[0],
            Edge::Direct { from, to } if from == TOOLS_NODE && to == AGENT_NODE
        ));
    }

    #[tokio::test]
    async fn agent_node_appends_assistant_message_and_sees_schemas() {
        let model = Arc::new(ScriptedModel::new(vec![ChatMessage::assistant("done")]));
        let graph = create_react_agent(model.clone(), registry()).unwrap();

        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
        }))
        .unwrap();
        let ctx = NodeContext::new(state, NodeConfig::default());
        let out = graph.node(AGENT_NODE).unwrap().run(ctx).await.unwrap();

        let appended = out.updates.get(MESSAGES_CHANNEL).unwrap();
        let msg: ChatMessage = serde_json::from_value(appended.clone()).unwrap();
        assert_eq!(msg.content.as_deref(), Some("done"));

        // The registry's schemas were passed to the model.
        assert_eq!(model.seen_tool_schemas.lock().unwrap().as_slice(), &[1]);
    }

    #[tokio::test]
    async fn tools_node_executes_pending_calls_in_order() {
        let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![]));
        let graph = create_react_agent(model, registry()).unwrap();

        let calls = vec![
            ToolCall::new("c1", "echo", json!({"text": "a"})),
            ToolCall::new("c2", "echo", json!({"text": "b"})),
        ];
        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [
                serde_json::to_value(ChatMessage::assistant_tool_calls(calls)).unwrap()
            ]
        }))
        .unwrap();
        let ctx = NodeContext::new(state, NodeConfig::default());
        let out = graph.node(TOOLS_NODE).unwrap().run(ctx).await.unwrap();

        let appended = out.updates.get(MESSAGES_CHANNEL).unwrap();
        let msgs: Vec<ChatMessage> = serde_json::from_value(appended.clone()).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[0].content.as_deref(), Some("a"));
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(msgs[1].content.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn router_follows_tool_calls_else_ends() {
        let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![]));
        let graph = create_react_agent(model, registry()).unwrap();
        let edges = graph.outgoing_edges(AGENT_NODE);
        let router = match edges[0] {
            Edge::Conditional { router, .. } => router,
            _ => panic!("expected conditional edge"),
        };

        let with_calls = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::assistant_tool_calls(vec![
                ToolCall::new("c1", "echo", json!({"text": "x"})),
            ]))
            .unwrap()]
        }))
        .unwrap();
        assert_eq!(
            router(with_calls).await.unwrap(),
            Route::Node(TOOLS_NODE.to_owned())
        );

        let final_answer = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::assistant("42")).unwrap()]
        }))
        .unwrap();
        assert_eq!(router(final_answer).await.unwrap(), Route::End);
    }

    /// Drive the loop by hand (one super-step at a time, through the public
    /// `StateSpec` merge) to prove the wiring end-to-end without depending on
    /// the concurrently-implemented `Executor::run`.
    #[tokio::test]
    async fn manual_super_steps_reproduce_the_react_loop() {
        let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "echo",
                json!({"text": "hello"}),
            )]),
            ChatMessage::assistant("echoed: hello"),
        ]));
        let graph = create_react_agent(model, registry()).unwrap();
        let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
        let mut state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("say hello")).unwrap()]
        }))
        .unwrap();

        // Step 0: agent -> assistant tool-call request.
        let out = graph
            .node(AGENT_NODE)
            .unwrap()
            .run(NodeContext::new(state.clone(), NodeConfig::default()))
            .await
            .unwrap();
        spec.apply_single(&mut state, AGENT_NODE, out.updates)
            .unwrap();

        // Route: tool calls present -> tools.
        let edges = graph.outgoing_edges(AGENT_NODE);
        let route = match edges[0] {
            Edge::Conditional { router, .. } => router(state.clone()).await.unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(route, Route::Node(TOOLS_NODE.to_owned()));

        // Step 1: tools -> tool result message.
        let out = graph
            .node(TOOLS_NODE)
            .unwrap()
            .run(NodeContext::new(state.clone(), NodeConfig::default()))
            .await
            .unwrap();
        spec.apply_single(&mut state, TOOLS_NODE, out.updates)
            .unwrap();

        // Step 2: agent -> final answer; route -> End.
        let out = graph
            .node(AGENT_NODE)
            .unwrap()
            .run(NodeContext::new(state.clone(), NodeConfig::default()))
            .await
            .unwrap();
        spec.apply_single(&mut state, AGENT_NODE, out.updates)
            .unwrap();
        let route = match edges[0] {
            Edge::Conditional { router, .. } => router(state.clone()).await.unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(route, Route::End);

        // Full transcript: user, assistant(tool_calls), tool, assistant(final).
        let msgs: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3].content.as_deref(), Some("echoed: hello"));
    }

    // ---- Flight Recorder wiring (record / replay flavors) ----

    use crate::journal::{Clock, Journal};
    use crate::replay::ReplaySource;

    fn recording_journal() -> Journal {
        Journal::new("run-react-test", "thread-react-test", Clock::System)
    }

    #[test]
    fn recording_and_replaying_variants_share_the_react_topology() {
        let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![]));
        let journal = recording_journal();
        let source = ReplaySource::new(&journal.snapshot());
        let recording =
            create_react_agent_with_recording(model.clone(), registry(), journal.clone()).unwrap();
        let replaying =
            create_react_agent_replaying(model.clone(), registry(), source, journal).unwrap();

        for graph in [recording, replaying] {
            assert_eq!(graph.node_count(), 2);
            assert_eq!(graph.entry_point(), AGENT_NODE);
            let agent_edges = graph.outgoing_edges(AGENT_NODE);
            assert_eq!(agent_edges.len(), 1);
            assert!(matches!(agent_edges[0], Edge::Conditional { .. }));
            let tools_edges = graph.outgoing_edges(TOOLS_NODE);
            assert_eq!(tools_edges.len(), 1);
            assert!(matches!(
                tools_edges[0],
                Edge::Direct { from, to } if from == TOOLS_NODE && to == AGENT_NODE
            ));
        }
    }

    /// Node closures driven outside `Executor::run` have no
    /// `PARENT_EVENT_KEY`; recording without a causal anchor must fail
    /// loudly, not journal an unparented event.
    #[tokio::test]
    async fn recording_nodes_error_without_the_executor_parent_event() {
        let model: Arc<dyn ChatModel> =
            Arc::new(ScriptedModel::new(vec![ChatMessage::assistant("done")]));
        let graph =
            create_react_agent_with_recording(model, registry(), recording_journal()).unwrap();

        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
        }))
        .unwrap();
        let err = graph
            .node(AGENT_NODE)
            .unwrap()
            .run(NodeContext::new(state, NodeConfig::default()))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, RustyError::Node(_)), "got: {message}");
        assert!(message.contains(PARENT_EVENT_KEY), "got: {message}");

        // The tools node fails the same way, after its input validation.
        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::assistant_tool_calls(vec![
                ToolCall::new("c1", "echo", json!({"text": "x"})),
            ]))
            .unwrap()]
        }))
        .unwrap();
        let err = graph
            .node(TOOLS_NODE)
            .unwrap()
            .run(NodeContext::new(state, NodeConfig::default()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains(PARENT_EVENT_KEY), "got: {err}");
    }

    /// A model that captures the tool names it was offered, in order.
    struct SchemaRecorder {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ChatModel for SchemaRecorder {
        async fn chat(&self, _messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
            *self.seen.lock().unwrap() = tools
                .iter()
                .map(|schema| {
                    schema
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                })
                .collect();
            Ok(ChatResponse {
                message: ChatMessage::assistant("done"),
                model: None,
                usage: None,
            })
        }
    }

    struct Zeta;

    #[async_trait]
    impl Tool for Zeta {
        fn name(&self) -> &str {
            "zeta"
        }
        fn description(&self) -> &str {
            "Alphabetically after echo."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    /// Registries are HashMap-backed (random iteration order); the prebuilt
    /// agent sorts schemas by tool name so the model request — which exact
    /// replay hashes — is canonical across registry instances and processes.
    /// All flavors share this via `build_react_agent`.
    #[tokio::test]
    async fn tool_schemas_reach_the_model_in_canonical_name_order() {
        let model = Arc::new(SchemaRecorder {
            seen: Mutex::new(Vec::new()),
        });

        // Two registries, same tools, opposite insertion orders.
        let mut forward = ToolRegistry::new();
        forward.register(Echo);
        forward.register(Zeta);
        let mut reverse = ToolRegistry::new();
        reverse.register(Zeta);
        reverse.register(Echo);

        for registry in [forward, reverse] {
            let graph = create_react_agent(model.clone(), registry).unwrap();
            let state = State::from_value(json!({
                MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
            }))
            .unwrap();
            graph
                .node(AGENT_NODE)
                .unwrap()
                .run(NodeContext::new(state, NodeConfig::default()))
                .await
                .unwrap();
            assert_eq!(model.seen.lock().unwrap().as_slice(), ["echo", "zeta"]);
        }
    }

    #[tokio::test]
    async fn run_allowlist_limits_model_schemas_and_tool_dispatch() {
        let model = Arc::new(SchemaRecorder {
            seen: Mutex::new(Vec::new()),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Echo);
        tools.register(Zeta);
        let graph = create_react_agent(model.clone(), tools).unwrap();
        let allowed = serde_json::json!(["echo"]);

        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
        }))
        .unwrap();
        let mut config = NodeConfig::default();
        config
            .extra
            .insert(TOOL_ALLOWLIST_KEY.to_owned(), allowed.clone());
        graph
            .node(AGENT_NODE)
            .unwrap()
            .run(NodeContext::new(state, config))
            .await
            .unwrap();
        assert_eq!(model.seen.lock().unwrap().as_slice(), ["echo"]);

        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::assistant_tool_calls(vec![
                ToolCall::new("blocked", "zeta", json!({})),
            ]))
            .unwrap()]
        }))
        .unwrap();
        let mut config = NodeConfig::default();
        config.extra.insert(TOOL_ALLOWLIST_KEY.to_owned(), allowed);
        let out = graph
            .node(TOOLS_NODE)
            .unwrap()
            .run(NodeContext::new(state, config))
            .await
            .unwrap();
        let messages: Vec<ChatMessage> =
            serde_json::from_value(out.updates[MESSAGES_CHANNEL].clone()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("blocked"));
        assert!(messages[0]
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("unknown tool `zeta`"));
    }
}
