//! # Rusty Core
//!
//! Rusty Core (the `rusty-agent-runtime` crate) is a LangGraph-style agentic
//! core engine in Rust. It models
//! agent workflows as **cyclic graphs over shared state**:
//!
//! - **State & channels** ([`state`]): every state key is a *channel* with a
//!   per-key [`state::Reducer`] defining merge semantics. Nodes return partial
//!   updates; the engine merges them via reducers. `LastValue`-style channels
//!   (the default) accept **at most one write per super-step**, otherwise an
//!   [`error::RustyError::InvalidUpdate`] is raised.
//! - **Nodes** ([`node`]): async functions (or [`node::Node`] trait impls)
//!   receiving a [`node::NodeContext`] (immutable state snapshot + config +
//!   interrupt/resume helpers) and returning a [`node::NodeOutput`] — partial
//!   state updates plus an optional [`node::Command`] for dynamic routing.
//! - **Graph** ([`graph`]): a thin builder ([`graph::GraphBuilder`]) with
//!   normal edges, conditional edges returning a [`graph::Route`] (including
//!   dynamic fan-out via [`graph::Send`]), and structural validation when
//!   you call [`graph::GraphBuilder::compile`].
//! - **Execution** ([`executor`]): a Pregel/BSP-inspired super-step loop —
//!   *plan → run active nodes in parallel over an immutable snapshot →
//!   barrier → merge writes via reducers → route → checkpoint* — emitting
//!   [`executor::GraphEvent`]s for streaming.
//! - **Persistence** ([`checkpoint`]): thread-scoped, versioned snapshots via
//!   the [`checkpoint::Checkpointer`] trait; includes an in-memory saver
//!   and a durable pure-`serde_json` file saver, plus a `postgres`-feature
//!   `PostgresCheckpointer` (see the `checkpoint_postgres` module).
//! - **LLM & tools** ([`llm`], [`tool`]): a [`llm::ChatModel`] abstraction
//!   with an OpenAI-compatible client, and a [`tool::ToolRegistry`] /
//!   [`tool::ToolExecutor`] for parallel tool-call dispatch — everything
//!   needed for the prebuilt ReAct pattern ([`react`]).
//! - **MCP** ([`mcp`]): call any MCP server's tools from [`tool::Tool`]
//!   impls over stdio transport; MCP tool servers register into the
//!   registry/executor exactly like native tools.
//! - **Middleware** ([`middleware`]): ordered interception layers around
//!   node runs, model calls, and tool calls — observe, mutate, reject, or
//!   short-circuit with tower-style onion semantics. Layers attach to the
//!   executor via [`executor::Executor::layer`].
//! - **Remote nodes** ([`remote`]): a [`remote::RemoteNode`] executes node
//!   work on a remote worker over HTTP behind the same [`node::Node`] trait;
//!   HITL interrupts cross the wire.
//! - **Durable work** ([`durable`]): the shared R0.6 contracts for
//!   effectively-once distributed activities — the [`durable::ErrorClass`]
//!   retry taxonomy, the [`durable::RetryDecision`] policy mapping, and the
//!   serde-versioned [`durable::TaskEnvelope`]. Queue, leases, and workers
//!   live in `rusty-server` / `rusty-worker`; these are the pure contracts
//!   both sides agree on.
//! - **Agent fabric** ([`agents`], R0.7): the durable-agent
//!   contracts — stable [`agents::AgentId`] identity with its
//!   `agent:{id}` mailbox/thread addressing grammar, the versioned
//!   [`agents::CapabilityManifest`] (accepted message kinds, declared
//!   [`agents::StateScope`]s, budget ceiling), the agent
//!   [`record::RunEventKind`] variants (`AgentSpawn`, `MailboxSend`, …),
//!   and the typed coordination patterns (wave 3:
//!   [`agents::CoordinationContract`] — delegate / fan-out / race /
//!   quorum — with their outcomes and violation vocabulary).
//!   The registry, activation leases, turn-serialized mailbox, and the
//!   pattern runtime live in `rusty-server`; these are the pure contracts
//!   both sides agree on. [`team_trace`] is the read-side half: it
//!   stitches verified journal snapshots back into one cross-journal
//!   causal tree.
//! - **Effect kernel v2** ([`effects`], R0.7): the R0.5 [`record::Effect`]
//!   taxonomy moved into the type system — marker traits declare an effect's
//!   safety class at compile time, deterministic [`effects::EffectId`]s let
//!   recovery ask whether an effect already committed, and irreversible
//!   effects execute only behind an explicit [`effects::ApprovalToken`].
//!   Opt-in: the untyped [`record::Effect`] path behaves exactly as before.
//! - **Replay** ([`replay`]): exact replay of journaled runs — model, tool,
//!   remote, and WASM calls are served from the journal instead of executed —
//!   plus branch diffs between journal snapshots and portable
//!   [`replay::ReplayFixture`] bundles for CI.
//! - **Governed memory** ([`memory`], R0.8 wave 1): the record model
//!   ([`memory::MemoryRecord`] — content-addressed, scoped, attributed,
//!   superseding, expiring), structured retrieval with a token-bounded
//!   deterministic assembly ([`memory::MemoryQuery`] +
//!   [`memory::ContextBudget`]), and the journaled seam: reads are
//!   [`record::RunEventKind::MemoryRead`] ([`record::Effect::ReadOnly`],
//!   served byte-identically by exact replay), writes are
//!   [`record::RunEventKind::MemoryWrite`] ([`record::Effect::Idempotent`]
//!   under a derived key). The store backends and endpoints live in
//!   `rusty-server`; these are the pure contracts both sides agree on.
//! - **WASM nodes** (`wasm_node`, feature `wasm`): sandboxed WebAssembly
//!   modules run as graph nodes via Wasmtime.
//!
//! ## Quick sketch
//!
//! ```no_run
//! use rusty_agent_runtime::prelude::*;
//!
//! # async fn demo() -> Result<()> {
//! let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
//!
//! let mut builder = GraphBuilder::new();
//! builder.add_node("agent", |ctx: NodeContext| async move {
//!     let _state = ctx.state();
//!     Ok(NodeOutput::update("messages", serde_json::json!({"role": "assistant", "content": "hi"})))
//! });
//! builder.set_entry_point("agent");
//! let graph = builder.compile()?;
//!
//! let outcome = Executor::new()
//!     .run(&graph, &spec, State::new(), RunConfig::new("thread-1"))
//!     .await?;
//! # let _ = outcome;
//! # Ok(())
//! # }
//! ```

pub mod agents;
pub mod checkpoint;
#[cfg(feature = "postgres")]
pub mod checkpoint_postgres;
pub mod durable;
pub mod effects;
pub mod error;
pub mod executor;
pub mod graph;
pub mod journal;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod middleware;
pub mod node;
pub mod react;
pub mod record;
pub mod remote;
pub mod replay;
pub mod state;
pub mod team_trace;
pub mod tool;
#[cfg(feature = "wasm")]
pub mod wasm_node;

/// Convenience re-exports of the main public API.
pub mod prelude {
    pub use crate::agents::{
        agent_id_from_recipient, AgentBudget, AgentId, CapabilityManifest, ContextGrant,
        CoordinationContract, CoordinationKind, CoordinationMessage, CoordinationOutcome,
        CoordinationStatus, CoordinationViolation, DelegateContract, Delegation, EscalationNotice,
        FanOutContract, MemberDisposition, MemberFailurePolicy, MemberSettlement, QuorumContract,
        QuorumOutcome, QuorumResolver, QuorumResolverRecord, QuorumTally, RaceContract,
        RestartPolicy, StateScope, SupervisionAttempt, SupervisionPolicy, SupervisionTrigger,
        AGENT_RECIPIENT_PREFIX, COORDINATION_RESULT_KIND, ESCALATION_MESSAGE_KIND,
    };
    pub use crate::checkpoint::{
        Checkpoint, Checkpointer, InMemoryCheckpointer, JsonFileCheckpointer,
    };
    #[cfg(feature = "postgres")]
    pub use crate::checkpoint_postgres::PostgresCheckpointer;
    pub use crate::durable::{
        backoff_delay_ms, classify_retry, ArtifactContract, ErrorClass, RetryDecision, TaskBudget,
        TaskEnvelope, BASE_RETRY_DELAY_MS, MAX_RETRY_DELAY_MS, TASK_ENVELOPE_FORMAT_VERSION,
    };
    pub use crate::effects::{
        admit_compensatable, admit_irreversible, admit_retry, admit_speculation, derive_effect_id,
        ApprovalToken, CompensatableEffect, CompensationHandler, CompensationRegistry, EffectId,
        EffectViolation, IdempotentEffect, IrreversibleEffect, PureEffect, ReadOnlyEffect,
        TypedEffect, EFFECT_ID_DOMAIN,
    };
    pub use crate::error::{Result, RustyError};
    pub use crate::executor::{ExecutionOutcome, Executor, GraphEvent, RunConfig};
    pub use crate::graph::{ConditionalRouter, Edge, Graph, GraphBuilder, Route, Send};
    pub use crate::journal::{
        Clock, EventDraft, Journal, JournalSnapshot, RngSource, PARENT_EVENT_KEY,
    };
    pub use crate::llm::{
        ChatMessage, ChatModel, ChatResponse, OpenAiCompatibleClient, Role, ToolCall, Usage,
    };
    pub use crate::memory::{
        apply_query, assemble, derive_memory_id, estimated_tokens, memory_effect_key,
        memory_read_request, BudgetOverflow, ContextBudget, InMemoryMemoryStore, JournaledMemory,
        MemoryAssembly, MemoryEvidence, MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord,
        MemoryReplaySource, MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor, ScopeAddress,
        TokenAccounting, ValidityWindow, DEFAULT_TOKEN_MARGIN_PERCENT, MEMORY_SCHEMA_VERSION,
        TOKEN_BYTES_PER_ESTIMATE,
    };
    pub use crate::middleware::{
        Decision, InterceptPoint, Middleware, MiddlewareChain, MiddlewareChatModel, ModelCall,
        NodeCall, Rejection, RequestLogger, ToolCallBlocklist, ToolInvocation,
    };
    pub use crate::node::{Command, Node, NodeConfig, NodeContext, NodeOutput};
    pub use crate::react::{
        create_react_agent, create_react_agent_replaying, create_react_agent_streaming,
        create_react_agent_with_recording,
    };
    pub use crate::record::{
        ArtifactRef, CapsuleVersion, CheckpointHeader, DecisionAction, DecisionEvent,
        DecisionFamily, DecisionOutcome, Effect, EffectReceipt, EventStatus, JournalRef,
        PayloadRef, PolicyVersion, RunEvent, RunEventKind, RunManifest, CURRENT_FORMAT_VERSION,
    };
    pub use crate::replay::{
        BranchDiff, BranchTotals, ChannelDiff, ExactReplay, FixtureMetadata, LogicalClockParams,
        RecordingChatModel, RecordingTool, ReplayFixture, ReplayOutcome, ReplayParams,
        ReplaySource, ReplayingChatModel, ReplayingTool, ServedEffect, StepDiff,
        FIXTURE_FORMAT_VERSION,
    };
    pub use crate::state::{Reducer, State, StateSpec};
    pub use crate::team_trace::{TeamTrace, TeamTraceNode};
    pub use crate::tool::{Tool, ToolExecutor, ToolRegistry};
}
