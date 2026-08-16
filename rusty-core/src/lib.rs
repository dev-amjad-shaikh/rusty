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
//! - **A2A client** ([`a2a`], R0.9 wave 4): an [`a2a::A2aNode`] delegates a
//!   node's work to a remote A2A agent behind the same [`node::Node`] trait —
//!   journaled as one replay-servable `RemoteCall` event with the derived
//!   `messageId` idempotency handle, replay-served outcomes with no outbound
//!   calls, and `tasks/cancel` propagation on run cancellation.
//! - **Durable work** ([`durable`]): the shared R0.6 contracts for
//!   effectively-once distributed activities — the [`durable::ErrorClass`]
//!   retry taxonomy, the [`durable::RetryDecision`] policy mapping, and the
//!   serde-versioned [`durable::TaskEnvelope`]. Queue, leases, and workers
//!   live in `rusty-agent-server` / `rusty-worker`; these are the pure contracts
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
//!   pattern runtime live in `rusty-agent-server`; these are the pure contracts
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
//! - **The runtime digital twin** ([`twin`], R0.10 wave 2): deterministic
//!   re-execution of recorded runs that answers what plain replay cannot —
//!   seeded [`twin::FaultSchedule`]s landing at decision points and the
//!   effect boundary, seeded schedule randomization of each super-step's
//!   parallel task set, counterfactual branches over one changed decision
//!   compared as [`replay::BranchDiff`] evidence, and shadow policies whose
//!   decisions journal with roles and propensities alongside the acting
//!   floor's. Bounded by the honest edge: only decisions that change *when
//!   and whether* effects execute are evaluable, and every
//!   [`twin::TwinReport`] says so.
//! - **Governed memory** ([`memory`], R0.8 wave 1): the record model
//!   ([`memory::MemoryRecord`] — content-addressed, scoped, attributed,
//!   superseding, expiring), structured retrieval with a token-bounded
//!   deterministic assembly ([`memory::MemoryQuery`] +
//!   [`memory::ContextBudget`]), and the journaled seam: reads are
//!   [`record::RunEventKind::MemoryRead`] ([`record::Effect::ReadOnly`],
//!   served byte-identically by exact replay), writes are
//!   [`record::RunEventKind::MemoryWrite`] ([`record::Effect::Idempotent`]
//!   under a derived key). The store backends and endpoints live in
//!   `rusty-agent-server`; these are the pure contracts both sides agree on.
//! - **Learning candidates** ([`learn`], R0.8 wave 3): the candidate
//!   pipeline — content-addressed [`learn::Candidate`]s (identity is
//!   integrity), the evaluation composition seam ([`learn::CandidateEvaluator`]
//!   with its replay + experiment + verdict payload), the declared
//!   [`learn::PromotionEnvelope`] and its gate ([`learn::admit_promotion`],
//!   out-of-envelope promotions requiring a scoped
//!   [`effects::ApprovalToken`]), canary binding by seeded draw, and the
//!   active-version pointer with byte-exact rollback. Every transition
//!   journals through the `CandidateCreated` / `CandidateEvaluated` /
//!   `CandidatePromoted` / `CandidateRolledBack` event kinds.
//! - **The configuration registry** ([`registry`], R0.11): the
//!   prompt/configuration registry — named, owned [`registry::ArtifactRecord`]s
//!   indexing the candidate pipeline (a commit *is* a candidate, never a fork
//!   of it), environment tags on surface keys ([`learn::EnvironmentTag`]) so
//!   one deployment promotes per environment through the unchanged pointer
//!   machinery, [`registry::diff_candidates`] views computed on read,
//!   never stored, and admission-time resolution (wave 2): a run's named
//!   artifacts bind through the tagged version pointer to a candidate
//!   ([`registry::pointer_admission`]) whose content the manifest pins by
//!   digest, the digest ↔ version join journaled as
//!   [`registry::ConfigResolution`]. The store backends and endpoints live in
//!   `rusty-agent-server`; these are the pure contracts both sides agree on.
//! - **The credential/connection broker** ([`broker`], R0.11 wave 3): no
//!   tool ever holds a raw credential. The connection entity
//!   ([`broker::ConnectionRecord`] — consent scope set as the ceiling
//!   everything narrows against, status, health), the sealed-storage shape
//!   ([`broker::StoredConnection`] — ciphertext and a wrapped data key
//!   only, plaintext on neither backend ever), and the handle lifecycle:
//!   short-lived, opaque, non-serializable [`broker::CredentialHandle`]s
//!   whose validity is self-contained in signed claims while revocation
//!   reads live connection state at every resolution — a revoked
//!   connection fails closed at the next tool call with a typed,
//!   journaled [`broker::BrokerDenial`]. [`broker::CredentialMediator`] /
//!   [`broker::MediatedTool`] mediate `ToolExecutor` dispatch, and behind
//!   `wasm` `broker::BrokeredCapsuleHost` turns capsule `Secret`
//!   grants into broker-issued handles; in both, resolution returns the
//!   credential bytes to the host-side connector, never to tool code.
//!   The store backends, envelope cryptography, master key, and
//!   endpoints live in `rusty-agent-server`; these are the pure
//!   contracts both sides agree on.
//! - **WASM nodes** (`wasm_node`, feature `wasm`): sandboxed WebAssembly
//!   modules run as graph nodes via Wasmtime.
//! - **Rusty Capsules** ([`capsule`], R0.9 wave 1): the content-addressed
//!   [`capsule::CapsuleManifest`] — identity, version, build digest, the
//!   declared WIT-world interface, the closed [`capsule::CapabilityGrant`]
//!   set (the whole reach), and the [`capsule::ResourceBudget`] — plus the
//!   journaled payloads ([`capsule::CapsuleResolution`] /
//!   [`capsule::CapsuleUse`] / [`capsule::CapsuleDenial`]) that make
//!   capability enforcement attributable. The capability host
//!   (`capsule_host`, feature `wasm`) instantiates components against the
//!   declared world with imports linked only where granted — deny by
//!   default, structurally — and the registry resolving `RunManifest`
//!   capsule pins to content addresses lives in `rusty-agent-server`; these are
//!   the pure contracts both sides agree on.
//! - **Signed run receipts** ([`receipt`], R0.9 wave 3): the Ed25519-signed
//!   [`receipt::RunReceipt`] over the journal head, the run manifest
//!   digests and resolved capsule ids, the effect and denials ledgers, and
//!   the policy versions — plus [`receipt::verify_receipt`], which
//!   re-walks the journal's own digests and answers with a typed
//!   [`receipt::VerifiedRun`] or a [`receipt::ReceiptRejection`] naming the
//!   mismatched component. The key lifecycle (first-boot generation,
//!   journaled rotation, the key history old receipts verify against)
//!   lives in `rusty-agent-server`; these are the pure contracts both sides
//!   agree on.
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

pub mod a2a;
pub mod agents;
pub mod artifact;
pub mod broker;
pub mod capability;
pub mod capsule;
#[cfg(feature = "wasm")]
pub mod capsule_host;
pub mod checkpoint;
#[cfg(feature = "postgres")]
pub mod checkpoint_postgres;
pub mod connector;
pub mod deploy;
pub mod durable;
pub mod effects;
pub mod error;
pub mod executor;
pub mod graph;
pub mod journal;
pub mod learn;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod middleware;
pub mod node;
#[cfg(feature = "genai")]
pub mod provider_genai;
pub mod react;
pub mod receipt;
pub mod record;
pub mod registry;
pub mod remote;
pub mod replay;
pub mod skill;
pub mod state;
pub mod team_trace;
pub mod tool;
pub mod twin;
#[cfg(feature = "wasm")]
pub mod wasm_node;

/// Convenience re-exports of the main public API.
pub mod prelude {
    pub use crate::a2a::{A2aNode, A2A_PROTOCOL_VERSION};
    pub use crate::agents::{
        agent_id_from_recipient, AgentBudget, AgentId, CapabilityManifest, ContextGrant,
        CoordinationContract, CoordinationKind, CoordinationMessage, CoordinationOutcome,
        CoordinationStatus, CoordinationViolation, DelegateContract, Delegation, EscalationNotice,
        FanOutContract, MemberDisposition, MemberFailurePolicy, MemberSettlement, QuorumContract,
        QuorumOutcome, QuorumResolver, QuorumResolverRecord, QuorumTally, RaceContract,
        RestartPolicy, StateScope, SupervisionAttempt, SupervisionPolicy, SupervisionTrigger,
        AGENT_RECIPIENT_PREFIX, COORDINATION_RESULT_KIND, ESCALATION_MESSAGE_KIND,
    };
    pub use crate::artifact::{
        commit_artifact, ArtifactCommitment, ArtifactError, ArtifactLineage, ArtifactVersion,
        CommitDeclaration, MediaKind, RetentionPolicy, RunArtifact,
    };
    #[cfg(feature = "wasm")]
    pub use crate::broker::BrokeredCapsuleHost;
    pub use crate::broker::{
        new_connection_id, new_handle_id, scopes_missing, BrokerDenial, BrokerDenialReason,
        ClassifiedFailure, ConnectionConsent, ConnectionHealth, ConnectionProvider,
        ConnectionReauthRequired, ConnectionRecord, ConnectionRefresh, ConnectionRevocation,
        ConnectionStatus, CredentialBroker, CredentialHandle, CredentialMediator,
        CredentialRequirement, CredentialTool, CredentialUse, HandleClaims, HandleIssuance,
        IssueRequest, MediatedTool, OAuthFailure, OAuthProvider, ResolvedCredential,
        ScriptedOAuthProvider, SealedCredential, StoredConnection, TokenGrant, TokenMaterial,
        CONNECTION_ID_PREFIX, HANDLE_ID_PREFIX, HANDLE_TOKEN_PREFIX, SEALED_FORMAT_VERSION,
    };
    pub use crate::capsule::{
        any_grant_of_kind, derive_capsule_id, network_grant_covers, CapabilityGrant,
        CapabilityKind, CapsuleDenial, CapsuleId, CapsuleIdentity, CapsuleInterface,
        CapsuleManifest, CapsuleResolution, CapsuleUse, FilesystemMode, ResourceBudget,
        SUPPORTED_WORLDS, WORLD_V1,
    };
    pub use crate::checkpoint::{
        Checkpoint, Checkpointer, InMemoryCheckpointer, JsonFileCheckpointer,
    };
    #[cfg(feature = "postgres")]
    pub use crate::checkpoint_postgres::PostgresCheckpointer;
    pub use crate::deploy::{
        deployment_admission, deployment_surface, derive_revision_id, pin_set_digest,
        revision_promotion_effect_id, scoped_secret_name, validate_secret_name, CanaryClearance,
        CanaryDeclaration, CanaryDeployment, DeployError, DeploymentPointer, DeploymentResolved,
        DeploymentRevision, EnvSecretAct, EnvSecretDenial, EnvSecretRecord, EnvSecretRevocation,
        Environment, EnvironmentDeclaration, GateCheckRecord, GateDecisionRecord, GateDeclaration,
        GateEvaluation, GateVerdict, RegistryPin, RevisionContent, RevisionGateEvaluator,
        RevisionId, RevisionPromotion, RevisionRegistration, RevisionRollback, ShadowRunOutcome,
        ShadowRunStarted, ShadowVerdict, StoredEnvSecret, MAX_SECRET_NAME_LEN,
        REVISION_PROMOTION_EFFECT_KIND,
    };
    pub use crate::durable::{
        backoff_delay_ms, backoff_delay_ms_with, classify_retry, classify_retry_with_policy,
        resolve_retry_parameters, resolve_timeout_bound_ms, retry_decision_event,
        retry_legal_actions, retry_selected_action, timeout_decision_event, timeout_legal_actions,
        timeout_selected_action, ArtifactContract, ErrorClass, LatencyPercentiles,
        ResolvedRetryParameters, RetryDecision, TaskBudget, TaskEnvelope, BASE_RETRY_DELAY_MS,
        MAX_RETRY_DELAY_MS, TASK_ENVELOPE_FORMAT_VERSION,
    };
    pub use crate::effects::{
        admit_compensatable, admit_irreversible, admit_retry, admit_speculation, derive_effect_id,
        ApprovalToken, CompensatableEffect, CompensationHandler, CompensationRegistry, EffectId,
        EffectViolation, IdempotentEffect, IrreversibleEffect, PureEffect, ReadOnlyEffect,
        ShadowOutcomeSource, ShadowRefusal, ShadowRefusalSink, TypedEffect, EFFECT_ID_DOMAIN,
    };
    pub use crate::error::{LlmErrorClass, Result, RustyError};
    pub use crate::executor::{ExecutionOutcome, Executor, GraphEvent, RunConfig};
    pub use crate::graph::{ConditionalRouter, Edge, Graph, GraphBuilder, Route, Send};
    pub use crate::journal::{
        Clock, EventDraft, Journal, JournalSnapshot, RngSource, PARENT_EVENT_KEY,
    };
    pub use crate::learn::{
        admit_promotion, canary_admits, candidate_effect_key, derive_candidate_id,
        detect_policy_drift, distill_retry_parameters, distill_timeout_parameters,
        evaluation_effect_key, promotion_effect_id, promotion_effect_key, rollback_effect_key,
        surface_for_kind, AutoPromotion, CanaryBinding, Candidate, CandidateContent,
        CandidateEvaluation, CandidateEvaluator, CandidateId, CandidateKind, CandidateOverlay,
        CandidateRecord, CandidateStatus, DriftBaseline, DriftThresholds, EnvelopeRule,
        EnvironmentTag, EvaluationRequest, EvaluationThresholds, EvaluationVerdict, EvidenceSpan,
        GrantDirection, LearnError, MiddlewareLayerConfig, PolicyDriftReport, PromotionAuthority,
        PromotionDecision, PromotionEnvelope, PromotionReceipt, PromotionRefusal, ReplayDivergence,
        ReplaySummary, RetryLearningConfig, RollbackReceipt, SurfaceKey, TimeoutLearningConfig,
        TwinCandidateEvaluator, VersionPointer, CANARY_DRAW_DOMAIN, PROMOTION_EFFECT_KIND,
        SURFACE_TAG_SEPARATOR,
    };
    pub use crate::llm::{
        ChatMessage, ChatModel, ChatResponse, ModelPricing, OpenAiCompatibleClient, Role, ToolCall,
        Usage,
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
        instantiate_composition, Decision, InterceptPoint, Middleware, MiddlewareChain,
        MiddlewareChatModel, ModelCall, NodeCall, Rejection, RequestLogger, ToolCallBlocklist,
        ToolInvocation,
    };
    pub use crate::node::{Command, Node, NodeConfig, NodeContext, NodeOutput};
    #[cfg(feature = "genai")]
    pub use crate::provider_genai::GenaiChatModel;
    pub use crate::react::{
        create_react_agent, create_react_agent_replaying, create_react_agent_streaming,
        create_react_agent_with_recording,
    };
    pub use crate::receipt::{
        derive_key_id, manifest_digest, mint_receipt, verify_receipt, PublicKey, ReceiptRejection,
        RunReceipt, SigningKey, SigningKeyRotation, VerifiedRun, RECEIPT_FORMAT_VERSION,
    };
    pub use crate::record::{
        derive_policy_version, ArtifactRef, BackoffParameters, CapsuleVersion, CheckpointHeader,
        ConcurrencyPolicyParameters, DecisionAction, DecisionEvent, DecisionFamily,
        DecisionOutcome, DecisionRole, Effect, EffectReceipt, EventStatus, ExecutorPolicy,
        JournalRef, PayloadRef, PolicyVersion, RetryPolicyParameters, RunEvent, RunEventKind,
        RunManifest, TimeoutPolicyParameters, CURRENT_FORMAT_VERSION, POLICY_MAX_ATTEMPTS_ENVELOPE,
        POLICY_MAX_DELAY_ENVELOPE_MS,
    };
    pub use crate::registry::{
        diff_candidates, pointer_admission, resolution_pin, ArtifactCommit, ArtifactRecord,
        ConfigResolution, LeafChange, LeafModification, PointerBinding, RegistryDiff,
        RegistryError, TextDiffLine, MAX_ARTIFACT_NAME_LEN,
    };
    pub use crate::replay::{
        BranchDiff, BranchTotals, ChannelDiff, ExactReplay, FixtureMetadata, JournalShadowSource,
        LogicalClockParams, RecordingChatModel, RecordingTool, ReplayFixture, ReplayOutcome,
        ReplayParams, ReplaySource, ReplayingChatModel, ReplayingTool, ServedEffect, StepDiff,
        FIXTURE_FORMAT_VERSION,
    };
    pub use crate::state::{Reducer, State, StateSpec};
    pub use crate::team_trace::{TeamTrace, TeamTraceNode};
    pub use crate::tool::{Tool, ToolExecutor, ToolRegistry};
    pub use crate::twin::{
        CounterfactualBranch, CounterfactualFork, DecisionContext, FaultAnchor, FaultInjection,
        FaultSchedule, InjectedFault, Interleaving, ParameterizedPolicy, RecordedAnswer,
        StaticFloor, Twin, TwinMetrics, TwinOutcome, TwinPolicy, TwinReport, TwinRunConfig,
        TwinWorkItem, TwinWorld, UnevaluableCase, DEFAULT_CONCURRENCY_LADDER,
        DEFAULT_TIMEOUT_LADDER, MIN_TIMEOUT_RUNG_MS, TWIN_FORK_POLICY_VERSION, TWIN_REPORT_BOUND,
    };
}
