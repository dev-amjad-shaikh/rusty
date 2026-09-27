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

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::error::{Result, RustyError};
use crate::executor::GraphEvent;
use crate::graph::{Graph, GraphBuilder, Route};
use crate::invariant::CheckingChatModel;
use crate::journal::{Journal, PARENT_EVENT_KEY};
use crate::llm::{ChatMessage, ChatModel, ToolCall};
use crate::node::NodeOutput;
use crate::replay::{
    RecordingChatModel, RecordingTool, ReplaySource, ReplayingChatModel, ReplayingTool,
};
use crate::tool::{ToolExecutor, ToolRegistry, TOOL_ALLOWLIST_KEY};

/// The state channel the ReAct loop reads from and appends to. Declare it
/// with `Reducer::AddMessages` in the run's [`crate::state::StateSpec`].
pub const MESSAGES_CHANNEL: &str = "messages";

/// The plan tool's name: an agent writes its steps with it, and the loop
/// renders the newest plan into the task section on every model call.
pub const PLAN_TOOL: &str = "plan";
/// How a plan-check notice begins: the loop's own system message when a
/// final answer arrives with plan steps still open. Once per plan.
pub const PLAN_NOTICE_PREFIX: &str = "PLAN CHECK —";
/// The most steps a plan holds.
pub const PLAN_MAX_STEPS: usize = 12;

/// One step of an agent's plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    pub text: String,
    /// todo | doing | done | skipped
    #[serde(default = "plan_todo")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn plan_todo() -> String {
    "todo".to_owned()
}

/// A plan as the `plan` tool's arguments carry it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Plan {
    #[serde(default)]
    pub steps: Vec<PlanStep>,
}

impl Plan {
    /// The plan from a `plan` call's arguments: statuses normalized, the
    /// step count and texts bounded. None when there is no usable step.
    pub fn parse(args: &Value) -> Option<Plan> {
        let mut plan: Plan = serde_json::from_value(args.clone()).ok()?;
        plan.steps.retain(|s| !s.text.trim().is_empty());
        plan.steps.truncate(PLAN_MAX_STEPS);
        for step in &mut plan.steps {
            step.text = step.text.trim().chars().take(200).collect();
            let status = step.status.trim().to_lowercase();
            step.status = match status.as_str() {
                "doing" | "in_progress" | "in progress" => "doing".to_owned(),
                "done" | "complete" | "completed" => "done".to_owned(),
                "skipped" | "skip" => "skipped".to_owned(),
                _ => "todo".to_owned(),
            };
            step.note = step
                .note
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(|n| n.chars().take(200).collect());
        }
        (!plan.steps.is_empty()).then_some(plan)
    }

    /// Steps not done and not skipped.
    pub fn open(&self) -> Vec<&PlanStep> {
        self.steps
            .iter()
            .filter(|s| s.status == "todo" || s.status == "doing")
            .collect()
    }

    pub fn done(&self) -> usize {
        self.steps.iter().filter(|s| s.status == "done").count()
    }

    /// The plan as the model reads it in the task section.
    pub fn render(&self) -> String {
        let mut out = format!("Plan — {} of {} done", self.done(), self.steps.len());
        let open = self.open().len();
        if open > 0 {
            out.push_str(&format!(", {open} open"));
        }
        out.push_str(". Keep it current with the plan tool; answer when every step is done or skipped with a reason:
");
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&format!("{}. [{}] {}", i + 1, step.status, step.text));
            if let Some(note) = &step.note {
                out.push_str(&format!(" — {note}"));
            }
            out.push('\n');
        }
        out
    }
}

/// The newest plan in the thread, and the index of the message that
/// carried it: the last assistant message whose tool calls include `plan`.
pub fn latest_plan(messages: &[ChatMessage]) -> Option<(usize, Plan)> {
    messages.iter().enumerate().rev().find_map(|(i, m)| {
        if m.role != crate::llm::Role::Assistant {
            return None;
        }
        m.tool_calls
            .iter()
            .rev()
            .find(|c| c.name == PLAN_TOOL)
            .and_then(|c| Plan::parse(&c.arguments))
            .map(|p| (i, p))
    })
}

/// Whether a plan-check notice already followed the newest plan: the
/// check is made once per plan, so a model that answers past it is not
/// nudged forever.
pub fn plan_checked_since(messages: &[ChatMessage], plan_at: usize) -> bool {
    messages[plan_at..].iter().any(|m| {
        m.role == crate::llm::Role::System
            && m.content
                .as_deref()
                .is_some_and(|c| c.starts_with(PLAN_NOTICE_PREFIX))
    })
}

/// The notice a final answer with open steps gets, once.
pub fn plan_notice(plan: &Plan) -> String {
    let open: Vec<String> = plan
        .open()
        .iter()
        .map(|s| format!("[{}] {}", s.status, s.text))
        .collect();
    format!(
        "{PLAN_NOTICE_PREFIX} your plan has {} step{} not done: {}. Finish them, or mark each one skipped with a reason by calling `plan` again, then answer. Do not answer with steps still open.",
        open.len(),
        if open.len() == 1 { "" } else { "s" },
        open.join("; ")
    )
}

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
/// The situation section's text: the date and time, and whom the run is a
/// conversation with. Pure over its inputs, so record and replay agree.
pub fn situation_text(
    now: chrono::DateTime<chrono::Utc>,
    counterpart: Option<&crate::tool::Counterpart>,
) -> String {
    // The hour, not the minute: this text sits at the head of every model
    // call, and a prefix that changes each minute is a prefix the provider
    // cannot serve from its cache. A run's turns share the hour almost
    // always; a schedule or a tool answers the exact time when it matters.
    let mut text = format!(
        "Today is {} ({}). The time is about {} UTC.",
        now.format("%Y-%m-%d"),
        now.format("%A"),
        now.format("%H:00")
    );
    match counterpart {
        Some(crate::tool::Counterpart::Person(person)) => {
            text.push_str(&format!(
                " This conversation is with {person}; \"you\" means them."
            ));
        }
        Some(crate::tool::Counterpart::Nobody) => {
            text.push_str(
                " This run was started by a schedule or another system, not by a person: nobody is \
                 reading as you work, so do not address anyone as \"you\"; write for whoever reads the \
                 result later.",
            );
        }
        None => {}
    }
    text
}

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

/// The run-config key carrying the agent's standing instructions.
///
/// An agent is not a name over a graph: it is a *charter* — what it is for,
/// what it may assume, how it should answer — plus the tools it may use. The
/// graph is the same for every agent; the charter is what makes one agent
/// different from another.
///
/// It is **not** injected by this node. The model-visible-means-logged
/// invariant refuses any message the journal did not record, and a charter
/// prepended here is exactly that — `invariant violation: unlogged content at
/// message 0`. The charter therefore enters as the first message of the
/// thread, where it is journaled like every other, and this key exists so a
/// node can *see* what the run declared without being the one to add it.
pub const INSTRUCTIONS_KEY: &str = "rusty.instructions";

/// The run-config key carrying the context policy
/// ([`crate::context::ContextPolicy`], wire form). When present, the agent
/// node assembles every model call through a
/// [`crate::context::ContextPipeline`]: the thread's leading system message
/// — the charter — pinned as the identity section, the history compacted
/// under the policy, the tool schemas budgeted; the journaled model call is
/// the assembled request, and the compaction summarizer is journaled under
/// [`crate::context::CONTEXT_PIPELINE_PARENT`]. Absent, the node behaves as
/// before, byte-identically.
pub const CONTEXT_POLICY_KEY: &str = "rusty.context_policy";

/// The most a run may hold, in estimated tokens, when no context policy
/// compacts it: past this the loop stops with the words to set one, rather
/// than growing until the provider refuses. A deployment sets a policy
/// sized to its model's window; the library, which does not know the
/// window, bounds the run instead of guessing one.
pub const LIBRARY_CONTEXT_CEILING_TOKENS: u32 = 200_000;

/// The state channel that keeps the run's compaction summary between steps
/// ([`crate::context::StoredSummary`], `Overwrite` semantics): each
/// compaction revises the last summary with the turns since, or reuses it
/// when the watermark holds, instead of re-summarising the whole prefix
/// every step. A deployment declares the channel and sets
/// [`COMPACTION_STATE_KEY`] on the run; the node writes the channel only
/// then — a spec that does not declare it rejects the write.
pub const COMPACTION_CHANNEL: &str = "rusty.compaction";

/// Run-config key (`extra`) saying the run keeps its compaction summary in
/// [`COMPACTION_CHANNEL`].
pub const COMPACTION_STATE_KEY: &str = "rusty.compaction_state";

/// The run-config key carrying the skills the agent follows this run: a
/// list of [`crate::context::SkillSectionEntry`] in wire form, assembled
/// into the context's skills section when a context policy is on.
pub const SKILLS_KEY: &str = "rusty.skills";

/// The run-config key carrying the per-tool selection overlays
/// ([`crate::tool_select::ToolSelectionOverlay`] by tool name, wire form):
/// the builder's when-to-use note, tags, cost class, prerequisites. When a
/// context policy is on and this key or [`TOOL_OUTCOMES_KEY`] is present,
/// the agent node walks the governed tools path: manifests for the
/// invocation's registry ([`crate::tool_select::manifests_for_registry`])
/// go to the assembler, which runs the shortlist under the policy, records
/// the ranking in the section manifest, and renders a when-to-use note into
/// the schema the model reads. Absent both keys, the tools section packs
/// the raw schemas as before, byte-identically.
pub const TOOL_OVERLAYS_KEY: &str = "rusty.tool_overlays";

/// The run-config key carrying the per-tool journaled outcome snapshot
/// ([`crate::tool_select::ToolOutcomeStats`] by tool name, wire form) the
/// shortlist scores against. See [`TOOL_OVERLAYS_KEY`].
pub const TOOL_OUTCOMES_KEY: &str = "rusty.tool_outcomes";

/// The run-config key carrying who started the run (the declared
/// attribution, `{principal_id, name, kind}` on the server), so the tools
/// node can hand every dispatched tool its [`crate::tool::RunContext`].
pub const ATTRIBUTION_KEY: &str = "rusty.attribution";

/// The run-config key carrying the run's execution authority as admission
/// stamped it — `{tenant, actor, subject, via, admitted_at}` — for the same
/// [`crate::tool::RunContext`]: the tenant a tool acts in, the actor who
/// admitted the run, the subject it acts for.
pub const EXECUTION_KEY: &str = "rusty.execution";

/// The run-config key carrying the agent the run is of, for the same
/// [`crate::tool::RunContext`].
pub const AGENT_ID_KEY: &str = "rusty.agent_id";

/// The run-config key carrying whom the run is a conversation with
/// ([`crate::executor::RunConfig::counterpart`]): a person's id, or `null`
/// for nobody. Absent: undeclared, and a tool falls back to the
/// attribution's user principal.
pub const COUNTERPART_KEY: &str = "rusty.counterpart";

/// The run-config key carrying the agent's curated memory blocks, rendered
/// by the server at run start ([`crate::executor::RunConfig::memory_blocks`]).
/// They follow the situation text in the task section: ahead of the
/// history, refreshed per run, constant within it.
pub const MEMORY_BLOCKS_KEY: &str = "rusty.memory_blocks";

/// The run-config key carrying when the run started (RFC 3339), read from
/// the run's own clock once at the declaration and restored on replay —
/// the time the situation section states.
pub const STARTED_AT_KEY: &str = "rusty.started_at";

/// The provider the run calls, by the deployment's id, reaching the agent
/// node under this key: the agent's own choice (`studio_intent.model`) or a
/// draft's. The graph's model resolves it ([`ChatModel::select`]); an unknown
/// id runs on the graph's model, never a failure mid-run.
pub const MODEL_KEY: &str = "rusty.model";
/// The run's own fallback provider, by the deployment's id — composed with
/// the run's model (or the deployment's primary) for this run alone.
pub const FALLBACK_KEY: &str = "rusty.fallback_model";
/// The agent's sampling temperature for its own calls, when it set one.
pub const TEMPERATURE_KEY: &str = "rusty.temperature";

/// How many times the same tool call — same tools, same arguments — may be
/// requested within one turn. The first request runs. The second is refused
/// with a notice in the tool result's place, so the model can use what it
/// already has or change approach. The third ends the run: a loop that
/// re-asks the world the same question is not making progress, and the step
/// budget is not the place to find that out.
pub const STUCK_TURN_LIMIT: usize = 3;

/// How many read batches in a row may answer nothing the conversation had
/// not already seen before the loop refuses the next read: a run that keeps
/// reading and learns nothing is not making progress, whatever the
/// arguments. Three, like the identical-request bound: enough for a model
/// to try a couple of variations, not enough to burn a round on them.
pub const NO_NEW_FACT_LIMIT: usize = 3;

/// What a refused read returns when the last reads answered nothing new.
pub const NO_NEW_FACT_NOTICE: &str = "NOTHING NEW — not run. The last reads answered nothing this \
conversation had not already seen: each result was empty, or the same answer an earlier read \
already gave under another phrasing. Reading again the same way will not help. Change the \
approach — a different query, source or tool — or answer with what you have and say what you \
need. One more read that answers nothing new ends the run.";

/// What a refused repeat returns in place of a result.
pub const REPEATED_CALL_NOTICE: &str = "REPEATED CALL — not run. This exact call, with these \
exact arguments, already ran in this turn and its result is above. Use that result, or change \
the arguments or the approach. One more identical request ends the run.";

/// What an unanswered call gets when its tool reads and writes nothing: the
/// result was lost, the call is safe to make again.
pub const UNRECORDED_READ_NOTICE: &str = "NO RESULT RECORDED — the run ended before this \
call's result was recorded. The call changes nothing, so make it again if the result is still \
needed.";

/// What an unanswered call gets when its tool changes things: it may have
/// taken effect, and nobody repeats it on a guess.
pub const UNKNOWN_OUTCOME_NOTICE: &str = "OUTCOME UNKNOWN — the run ended before this \
call's result was recorded, and this call changes things, so it may have taken effect. It \
was not repeated. Check the current state of what it touches before acting again.";

/// One call [`repair_unpaired_tool_calls`] answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairedCall {
    /// The call's id.
    pub tool_call_id: String,
    /// The tool it named.
    pub tool: String,
    /// Whether the tool's effect is freely repeatable (the notice says
    /// "make it again") or not ("outcome unknown").
    pub repeatable: bool,
}

/// The interrupt a run raises to ask for approval: `{"kind": "approval",
/// "requests": [ApprovalNeeded…]}`. A server turns it into something a person
/// decides; the decision comes back as the resume value.
pub const APPROVAL_INTERRUPT_KIND: &str = "approval";

/// What the model is told for a call a person declined.
pub const DENIED_NOTICE: &str = "DENIED: a person declined this action";

/// The calls a resume value declines, each with the notice the model reads:
/// `{"kind": "approval", "decision": "deny", "by": "…", "reason": "…",
/// "call_ids": ["…"]}`. Anything else declines nothing.
fn denied_calls(resume: Option<&Value>) -> std::collections::HashMap<String, String> {
    let mut denied = std::collections::HashMap::new();
    let Some(value) = resume else { return denied };
    if value.get("kind").and_then(Value::as_str) != Some(APPROVAL_INTERRUPT_KIND)
        || value.get("decision").and_then(Value::as_str) != Some("deny")
    {
        return denied;
    }
    let by = value
        .get("by")
        .and_then(Value::as_str)
        .unwrap_or("a person");
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty())
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    let notice = format!(
        "{DENIED_NOTICE} ({by}{reason}). Do not retry it or work around it; say what you would have done and finish."
    );
    for id in value
        .get("call_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = id.as_str() {
            denied.insert(id.to_owned(), notice.clone());
        }
    }
    denied
}

/// Pair every assistant tool call that has no result with a notice in the
/// result's place, so the thread the model reads is well-formed and honest
/// about what happened. A run that stops between an assistant's tool calls
/// and their results — a crash, a stuck-turn stop, a failed tools step —
/// leaves exactly that gap, and a provider refuses the next request over
/// it. `repeatable(tool)` says whether the tool's effect is freely
/// repeatable; a tool the catalog no longer knows counts as not.
///
/// Notices go right after the calls' existing results, so the pairing a
/// provider checks holds. Returns what was answered, for the audit stream.
pub fn repair_unpaired_tool_calls(
    messages: &mut Vec<ChatMessage>,
    repeatable: impl Fn(&str) -> bool,
) -> Vec<RepairedCall> {
    use crate::llm::Role;
    let mut repaired = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let is_batch =
            messages[index].role == Role::Assistant && !messages[index].tool_calls.is_empty();
        if !is_batch {
            index += 1;
            continue;
        }
        let calls: Vec<(String, String)> = messages[index]
            .tool_calls
            .iter()
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect();
        let mut end = index + 1;
        let mut answered = std::collections::HashSet::new();
        while end < messages.len() && messages[end].role == Role::Tool {
            if let Some(id) = &messages[end].tool_call_id {
                answered.insert(id.clone());
            }
            end += 1;
        }
        for (id, tool) in calls {
            if answered.contains(&id) {
                continue;
            }
            let repeatable = repeatable(&tool);
            let notice = if repeatable {
                UNRECORDED_READ_NOTICE
            } else {
                UNKNOWN_OUTCOME_NOTICE
            };
            messages.insert(end, ChatMessage::tool_result(id.clone(), notice));
            end += 1;
            repaired.push(RepairedCall {
                tool_call_id: id,
                tool,
                repeatable,
            });
        }
        index = end;
    }
    repaired
}

/// A JSON value with object keys in sorted order, so two argument objects
/// that differ only in key order compare equal.
fn canonical(value: &Value) -> String {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: std::collections::BTreeMap<String, Value> =
                    map.iter().map(|(k, v)| (k.clone(), sort(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(value).to_string()
}

/// The batch's identity: every call as `name(canonical args)`, sorted.
fn call_signature(calls: &[crate::llm::ToolCall]) -> Vec<String> {
    let mut signature: Vec<String> = calls
        .iter()
        .map(|call| format!("{}({})", call.name, canonical(&call.arguments)))
        .collect();
    signature.sort();
    signature
}

/// How many earlier assistant messages of the current turn requested
/// exactly the batch the last message requests. A turn starts after the
/// most recent user message, so asking the same thing again next turn is a
/// new question, not a loop.
/// How many of the most recent tool-result batches, in a row, answered
/// nothing the conversation had not already seen: every result in the batch
/// byte-equal to some earlier successful result in this run. Failures and
/// the loop's own notices are neither facts nor progress: they are skipped
/// without breaking the streak. A pure function of the thread, so record
/// and replay reach it identically.
fn stale_results_in_a_row(messages: &[ChatMessage]) -> usize {
    // Batches in order: the results answering each assistant request.
    let mut batches: Vec<Vec<&str>> = Vec::new();
    for message in messages {
        if message.role == crate::llm::Role::Assistant && message.has_tool_calls() {
            batches.push(Vec::new());
        } else if message.role == crate::llm::Role::Tool {
            if let (Some(batch), Some(content)) = (batches.last_mut(), message.content.as_deref()) {
                batch.push(content);
            }
        }
    }
    let is_result = |content: &str| {
        !content.trim_start().starts_with("ERROR:")
            && content != REPEATED_CALL_NOTICE
            && content != NO_NEW_FACT_NOTICE
    };
    let mut seen: Vec<String> = Vec::new();
    let mut stale_of: Vec<Option<bool>> = Vec::with_capacity(batches.len());
    for batch in &batches {
        let results: Vec<&str> = batch.iter().copied().filter(|c| is_result(c)).collect();
        if results.is_empty() {
            stale_of.push(None);
            continue;
        }
        // A result that answered nothing is nothing new; one that answered
        // what an earlier result already did — the same notes under another
        // phrasing — is nothing new either.
        let facts: Vec<String> = results.iter().filter_map(|c| fact_body(c)).collect();
        let stale = facts.iter().all(|f| seen.contains(f));
        seen.extend(facts);
        stale_of.push(Some(stale));
    }
    // This turn's batches only: a person's new message is progress, and
    // the reads before it answered their question, not this one.
    let turn_start = messages
        .iter()
        .rposition(|m| m.role == crate::llm::Role::User)
        .map(|i| i + 1)
        .unwrap_or(0);
    let turn_batches = messages[turn_start..]
        .iter()
        .filter(|m| m.role == crate::llm::Role::Assistant && m.has_tool_calls())
        .count();
    let this_turn: Vec<&Option<bool>> = stale_of.iter().rev().take(turn_batches).collect();
    let mut streak = 0;
    for verdict in &this_turn {
        match verdict {
            Some(true) => streak += 1,
            Some(false) => break,
            None => {}
        }
    }
    // A read that answers something new between reads that answer nothing
    // keeps the streak short and the loop long: a turn that has spent
    // twice the bound on reads answering nothing new is stuck however the
    // reads were spaced.
    let stale_this_turn = this_turn.iter().filter(|v| ***v == Some(true)).count();
    if stale_this_turn >= NO_NEW_FACT_LIMIT * 2 {
        return streak.max(stale_this_turn);
    }
    streak
}

/// The keys a tool result echoes from its own request: not part of what it
/// answered, so two reads that found the same thing under two phrasings
/// compare equal once they are gone.
const ECHO_KEYS: [&str; 7] = ["asked", "query", "for", "limit", "scope", "table", "fields"];

/// What a tool result actually answered: `None` when it answered nothing —
/// empty, a bare "no result", an empty list, or an object whose every
/// value is empty, null, false or zero once the request's echo is removed
/// (`count: 0, found: false, notes: []`). Otherwise the answer in canonical
/// form, echo removed, so equal answers compare equal however they were
/// asked for.
fn fact_body(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("no result")
        || trimmed.eq_ignore_ascii_case("no results")
        || trimmed == "null"
    {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Some(trimmed.to_owned());
    };
    fn empty(value: &Value) -> bool {
        match value {
            Value::Null => true,
            Value::Bool(b) => !*b,
            Value::Number(n) => n.as_f64() == Some(0.0),
            Value::String(s) => s.trim().is_empty() || s.trim().eq_ignore_ascii_case("no result"),
            Value::Array(items) => items.is_empty(),
            Value::Object(map) => map.values().all(empty),
        }
    }
    let answered = match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| !ECHO_KEYS.contains(&k.as_str()))
                .collect(),
        ),
        other => other,
    };
    (!empty(&answered)).then(|| canonical(&answered))
}

/// Whether the loop already refused a read for answering nothing new since
/// the last batch that did answer something new.
fn refused_for_nothing_new_since_last_fact(messages: &[ChatMessage]) -> bool {
    let mut seen_notice = false;
    for message in messages.iter().rev() {
        if message.role != crate::llm::Role::Tool {
            continue;
        }
        match message.content.as_deref() {
            Some(c) if c == NO_NEW_FACT_NOTICE => seen_notice = true,
            Some(c) if c.trim_start().starts_with("ERROR:") || c == REPEATED_CALL_NOTICE => {}
            _ => {}
        }
        if seen_notice {
            return true;
        }
    }
    false
}

/// A tool result that is the system failing the call, rather than one of
/// the loop's own refusals — those are answers about the conversation, not
/// about the world.
fn world_failed(content: &str) -> bool {
    content.trim_start().starts_with("ERROR:")
        && !crate::tool::ToolFailure::parse(content)
            .is_some_and(|f| f.class == "bounded" || f.class == "reconcile_first")
}

/// How many times this turn already asked for exactly this batch of calls.
///
/// When every call in the batch only reads, the count starts after the last
/// call the system failed. A failure is the system saying the reader's
/// picture of it is wrong, and the read that built that picture is the
/// first thing worth taking again — refusing it as "you already have that
/// result" quotes an answer the system has just contradicted. Each such
/// re-read costs a real failure, and failures are bounded in their own
/// right, so the turn still ends. Writes never move: an earlier attempt may
/// have landed, and no unrelated failure makes sending it again safe.
fn identical_requests_this_turn(messages: &[ChatMessage], rereadable: bool) -> usize {
    let Some((last, earlier)) = messages.split_last() else {
        return 0;
    };
    let mut turn_start = earlier
        .iter()
        .rposition(|m| m.role == crate::llm::Role::User)
        .map(|i| i + 1)
        .unwrap_or(0);
    if rereadable {
        if let Some(failed) = earlier.iter().rposition(|m| {
            m.role == crate::llm::Role::Tool && m.content.as_deref().is_some_and(world_failed)
        }) {
            turn_start = turn_start.max(failed + 1);
        }
    }
    let signature = call_signature(&last.tool_calls);
    earlier[turn_start..]
        .iter()
        .enumerate()
        .filter(|(offset, m)| {
            if m.role != crate::llm::Role::Assistant
                || m.tool_calls.is_empty()
                || call_signature(&m.tool_calls) != signature
            {
                return false;
            }
            // A request whose every call failed for real, or was refused
            // for reconciliation, is not a result the model already has:
            // the failure policy governs those (and bounds them). Only a
            // `bounded` refusal counts here, so a model that keeps asking
            // still meets the hard stop.
            let start = turn_start + offset;
            let result_of = |tc: &crate::llm::ToolCall| {
                earlier[start..]
                    .iter()
                    .find(|r| r.tool_call_id.as_deref() == Some(tc.id.as_str()))
                    .and_then(|r| r.content.as_deref())
            };
            let all_failed = m.tool_calls.iter().all(|tc| {
                result_of(tc)
                    .and_then(crate::tool::ToolFailure::parse)
                    .is_some_and(|f| f.class != "bounded")
            });
            // A request the run ended before answering — a ceiling fell
            // between the call and its result — is not a result the model
            // has either: the notice in its place says to make the call
            // again, and a resumed run does exactly that.
            let all_unrecorded = m
                .tool_calls
                .iter()
                .all(|tc| result_of(tc) == Some(UNRECORDED_READ_NOTICE));
            !all_failed && !all_unrecorded
        })
        .count()
}

/// One stuck-turn episode on the run's repair ledger, when it has one.
fn record_stuck_turn(
    ctx: &crate::node::NodeContext,
    requests: usize,
    outcome: crate::repair::RepairOutcome,
) {
    let Some(ledger) = ctx.repair_ledger() else {
        return;
    };
    use crate::repair::{
        RepairAction, RepairComponent, RepairRecordBuilder, RepairRung, RepairTrigger,
    };
    let now = chrono::Utc::now();
    let record = RepairRecordBuilder::new()
        .component(RepairComponent::StuckTurnDetector)
        .trigger(RepairTrigger::StuckTurn {
            session_id: ctx.thread_id().to_owned(),
            phase: "tool_execution".to_owned(),
            stuck_for_ms: 0,
        })
        .action(RepairAction::StuckTurnIntervention {
            rung: RepairRung::InTurn,
        })
        .outcome(outcome)
        .start_time(now)
        .end_time(now)
        .session_id(ctx.thread_id())
        .citation(format!("step {}", ctx.step()))
        .attempt_count(requests as u32)
        .build();
    if let Err(error) = ledger.0.append(record) {
        tracing::warn!(%error, "stuck turn: the repair record was not appended");
    }
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
                let mut messages = read_messages(ctx.state())?;
                // Durable inbox (R0.13 parity wave): the executor hands the
                // step's drained batch — steering at a step boundary,
                // follow-ups and staged injections at a turn's wake — to
                // every invocation under INBOX_DELIVERY_KEY. The model sees
                // the batch as user-role input before its next call, and the
                // messages join the channel so the conversation history the
                // run persists carries them. Absent key: the pre-inbox
                // behavior, byte-identical.
                let delivered: Vec<crate::inbox::InboxMessage> =
                    match ctx.config().extra.get(crate::inbox::INBOX_DELIVERY_KEY) {
                        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                            RustyError::Node(format!(
                                "node `{AGENT_NODE}` received a malformed inbox delivery: {error}"
                            ))
                        })?,
                        None => Vec::new(),
                    };
                let inbox_messages: Vec<ChatMessage> = delivered
                    .iter()
                    .map(|message| {
                        ChatMessage::user(match &message.content {
                            Value::String(text) => text.clone(),
                            other => serde_json::to_string(other)
                                .expect("a JSON value always serializes"),
                        })
                    })
                    .collect();
                messages.extend(inbox_messages.iter().cloned());
                let invocation_registry = invocation_tools(&agent_tools, &ctx)?;
                let tool_schemas = sorted_tool_schemas(&invocation_registry);
                // The run's model, when it names one of the deployment's
                // providers; the graph's model otherwise.
                let chosen = ctx.config().extra.get(MODEL_KEY).and_then(Value::as_str);
                let fallback = ctx.config().extra.get(FALLBACK_KEY).and_then(Value::as_str);
                let model: Arc<dyn ChatModel> = match (chosen, fallback) {
                    // Its own pair: the chosen (or primary) provider bare, this fallback behind it.
                    (primary, Some(fb)) => model
                        .select_pair(primary.unwrap_or(""), fb)
                        .or_else(|| primary.and_then(|id| model.select(id)))
                        .unwrap_or_else(|| Arc::clone(&model)),
                    (Some(id), None) => model.select(id).unwrap_or_else(|| Arc::clone(&model)),
                    (None, None) => model,
                };
                // Evidence wiring is per invocation: the recording/replaying
                // wrappers carry the invocation's causal parent, which only
                // exists once the executor has journaled the node input.
                // The context policy, when the run declares one: every
                // model call is assembled through a pipeline — the charter
                // (the thread's leading system message) pinned as identity,
                // the history compacted, the tools budgeted. The evidence
                // wrapper sits INSIDE the assembler so the journaled model
                // call is the assembled request, and the summarizer slot is
                // wrapped per mode under the pipeline's own parent, so a
                // compaction is recorded and replayed like any other call.
                // Absent: the pre-pipeline chain, byte-identical.
                let policy = match ctx.config().extra.get(CONTEXT_POLICY_KEY) {
                    Some(value) => Some(crate::context::ContextPolicy::from_value(value)?),
                    None => None,
                };
                let unbounded = policy.is_none();
                let (dispatch, summarizer): (Arc<dyn ChatModel>, Option<Arc<dyn ChatModel>>) =
                    match &evidence {
                        EvidenceMode::None => match ctx.effect_journal() {
                            Some(journal) => {
                                let parent = invocation_parent(&ctx, AGENT_NODE)?;
                                (
                                    Arc::new(
                                        RecordingChatModel::new(
                                            Arc::clone(&model),
                                            journal.clone(),
                                            parent,
                                        )
                                        .node(AGENT_NODE),
                                    ),
                                    policy.as_ref().map(|_| -> Arc<dyn ChatModel> {
                                        Arc::new(RecordingChatModel::new(
                                            Arc::clone(&model),
                                            journal.clone(),
                                            crate::context::CONTEXT_PIPELINE_PARENT,
                                        ))
                                    }),
                                )
                            }
                            None => (
                                Arc::clone(&model),
                                policy.as_ref().map(|_| Arc::clone(&model)),
                            ),
                        },
                        EvidenceMode::Record(journal) => {
                            let parent = invocation_parent(&ctx, AGENT_NODE)?;
                            (
                                Arc::new(
                                    RecordingChatModel::new(
                                        Arc::clone(&model),
                                        journal.clone(),
                                        parent,
                                    )
                                    .node(AGENT_NODE),
                                ),
                                policy.as_ref().map(|_| -> Arc<dyn ChatModel> {
                                    Arc::new(RecordingChatModel::new(
                                        Arc::clone(&model),
                                        journal.clone(),
                                        crate::context::CONTEXT_PIPELINE_PARENT,
                                    ))
                                }),
                            )
                        }
                        EvidenceMode::Replay { source, journal } => {
                            let parent = invocation_parent(&ctx, AGENT_NODE)?;
                            (
                                Arc::new(ReplayingChatModel::new(
                                    Arc::clone(&model),
                                    source.clone(),
                                    journal.clone(),
                                    parent,
                                )),
                                policy.as_ref().map(|_| -> Arc<dyn ChatModel> {
                                    Arc::new(ReplayingChatModel::new(
                                        Arc::clone(&model),
                                        source.clone(),
                                        journal.clone(),
                                        crate::context::CONTEXT_PIPELINE_PARENT,
                                    ))
                                }),
                            )
                        }
                    };
                let keeps_summary = ctx
                    .config()
                    .extra
                    .get(COMPACTION_STATE_KEY)
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut assembler_handle: Option<Arc<crate::context::AssemblingChatModel>> = None;
                let model: Arc<dyn ChatModel> = match policy {
                    Some(policy) => {
                        let wants_situation = policy.task.is_some();
                        let mut pipeline = crate::context::ContextPipeline::new(policy)?;
                        if let Some(summarizer) = summarizer {
                            pipeline = pipeline.with_summarizer(summarizer);
                        }
                        // The summary the run stored at its last compaction.
                        if keeps_summary {
                            if let Some(prior) = ctx
                                .state()
                                .get_as::<crate::context::StoredSummary>(COMPACTION_CHANNEL)?
                            {
                                pipeline = pipeline.with_prior_summary(prior);
                            }
                        }
                        let skills: Vec<crate::context::SkillSectionEntry> =
                            match ctx.config().extra.get(SKILLS_KEY) {
                                Some(value) => {
                                    serde_json::from_value(value.clone()).map_err(|error| {
                                        RustyError::Node(format!(
                                            "run skills are malformed: {error}"
                                        ))
                                    })?
                                }
                                None => Vec::new(),
                            };
                        let mut assembler =
                            crate::context::AssemblingChatModel::new(dispatch, pipeline)
                                .with_identity_from_history()
                                .with_skills(skills);
                        // The situation, when the policy has a task section:
                        // today's date from the run's own clock (the journal's
                        // — logical on replay, so the text reproduces) and
                        // whom the run is a conversation with. A model with
                        // no clock dates nothing right; a model that does not
                        // know it is on a schedule addresses nobody as "you".
                        if wants_situation {
                            // The run's declared start, so a replay states the
                            // same time; a run without one (no declaration)
                            // reads its clock.
                            let now = ctx
                                .config()
                                .extra
                                .get(STARTED_AT_KEY)
                                .and_then(Value::as_str)
                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                .map(|t| t.with_timezone(&chrono::Utc))
                                .unwrap_or_else(|| match &evidence {
                                    EvidenceMode::Record(journal) => journal.clock().now(),
                                    EvidenceMode::Replay { journal, .. } => journal.clock().now(),
                                    EvidenceMode::None => ctx
                                        .effect_journal()
                                        .map(|journal| journal.clock().now())
                                        .unwrap_or_else(chrono::Utc::now),
                                });
                            let counterpart = ctx
                                .config()
                                .extra
                                .get(COUNTERPART_KEY)
                                .map(crate::tool::Counterpart::from_value);
                            let mut task = situation_text(now, counterpart.as_ref());
                            if let Some(blocks) = ctx
                                .config()
                                .extra
                                .get(MEMORY_BLOCKS_KEY)
                                .and_then(Value::as_str)
                            {
                                task.push_str("\n\n");
                                task.push_str(blocks);
                            }
                            // The newest plan, rendered where the model
                            // reads its situation, on every call.
                            if let Some((_, plan)) = latest_plan(&messages) {
                                task.push_str("\n\n");
                                task.push_str(&plan.render());
                            }
                            assembler = assembler.with_task(task);
                        }
                        // The memory section, when the policy has one: read
                        // through the journaled seam over the source the run
                        // declared — a store live, the recorded reads on
                        // replay — under the journal this invocation records
                        // on, so the read is evidence like the model call.
                        if let Some(source) = ctx.memory_source() {
                            let memory_journal = match &evidence {
                                EvidenceMode::Record(journal) => Some(journal.clone()),
                                EvidenceMode::Replay { journal, .. } => Some(journal.clone()),
                                EvidenceMode::None => ctx.effect_journal().cloned(),
                            };
                            if let Some(journal) = memory_journal {
                                assembler = assembler.with_memory(journal.memory(source.clone()));
                            }
                        }
                        // The governed tools path, when the run declares
                        // overlays or an outcome snapshot: manifests for
                        // exactly the tools this invocation may call, the
                        // overlays applied by name (an overlay naming a
                        // tool outside the allowlist fails closed here —
                        // a typo cannot silently drop governed metadata).
                        let overlays = ctx.config().extra.get(TOOL_OVERLAYS_KEY);
                        let outcomes = ctx.config().extra.get(TOOL_OUTCOMES_KEY);
                        if overlays.is_some() || outcomes.is_some() {
                            let overlays: std::collections::BTreeMap<
                                String,
                                crate::tool_select::ToolSelectionOverlay,
                            > = match overlays {
                                Some(value) => {
                                    serde_json::from_value(value.clone()).map_err(|error| {
                                        RustyError::Node(format!(
                                            "run tool overlays are malformed: {error}"
                                        ))
                                    })?
                                }
                                None => Default::default(),
                            };
                            let outcomes: std::collections::BTreeMap<
                                String,
                                crate::tool_select::ToolOutcomeStats,
                            > = match outcomes {
                                Some(value) => {
                                    serde_json::from_value(value.clone()).map_err(|error| {
                                        RustyError::Node(format!(
                                            "run tool outcomes are malformed: {error}"
                                        ))
                                    })?
                                }
                                None => Default::default(),
                            };
                            let manifests = crate::tool_select::manifests_for_registry(
                                &invocation_registry,
                                &overlays,
                            )?;
                            assembler = assembler
                                .with_tool_manifests(manifests)
                                .with_tool_outcomes(outcomes);
                        }
                        let shared = Arc::new(assembler);
                        assembler_handle = Some(Arc::clone(&shared));
                        shared
                    }
                    None => dispatch,
                };
                // The model-visible-means-logged invariant (EP-01-S05):
                // whenever the run journals, the dispatch model is wrapped
                // in the checker, anchored on this invocation's journaled
                // node-input event. A request the log cannot reconstruct
                // fails before any bytes reach the provider. Replay mode is
                // exempt: nothing live reaches a provider — the replaying
                // wrapper serves journaled responses and already enforces
                // request-hash equality with the recorded evidence. A
                // journal-less run has no log to reconstruct against and is
                // outside the durable substrate by construction.
                let check_journal = match &evidence {
                    EvidenceMode::None => ctx.effect_journal().cloned(),
                    EvidenceMode::Record(journal) => Some(journal.clone()),
                    EvidenceMode::Replay { .. } => None,
                };
                let model: Arc<dyn ChatModel> = match check_journal {
                    Some(journal) => Arc::new(CheckingChatModel::new(
                        model,
                        crate::invariant::InvariantChecker::new(journal),
                        ctx.config().thread_id.clone(),
                        AGENT_NODE,
                        invocation_parent(&ctx, AGENT_NODE)?,
                    )),
                    None => model,
                };
                // The agent's sampling, outermost: the assembler, the
                // recorder and the provider client all run inside it — the
                // client sends it, the journal records it with the call.
                let model: Arc<dyn ChatModel> = match ctx
                    .config()
                    .extra
                    .get(TEMPERATURE_KEY)
                    .and_then(Value::as_f64)
                {
                    Some(t) => Arc::new(crate::llm::ParamsChatModel::new(
                        model,
                        crate::llm::CallParams {
                            temperature: Some(t),
                        },
                    )),
                    None => model,
                };
                // No policy, no compaction: the library's ceiling stands in
                // for the window it does not know.
                if unbounded {
                    let bytes = serde_json::to_vec(&messages)
                        .map(|b| b.len() as u64)
                        .unwrap_or(0);
                    let estimated = crate::memory::estimated_tokens(bytes, 0);
                    if estimated > LIBRARY_CONTEXT_CEILING_TOKENS {
                        return Err(RustyError::Budget(format!(
                            "the conversation holds about {estimated} estimated tokens and no context policy compacts it — the library ceiling is {LIBRARY_CONTEXT_CEILING_TOKENS}; set a ContextPolicy on the run config (ContextPolicy::standard(<the model's window>)) so the history is compacted, or start a new thread"
                        )));
                    }
                }
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
                // A single message object is fine: AddMessages accepts one
                // message or an array and upserts/appends accordingly. The
                // inbox batch (when any) precedes the assistant message so
                // the persisted conversation carries the user-role input the
                // model actually saw.
                // The plan check: a final answer while the plan has steps
                // open is not the answer yet — once per plan, the loop says
                // so and the model gets another turn.
                let plan_check = if response.message.has_tool_calls() {
                    None
                } else {
                    latest_plan(&messages)
                        .filter(|(at, plan)| {
                            !plan.open().is_empty() && !plan_checked_since(&messages, *at)
                        })
                        .map(|(_, plan)| ChatMessage::system(plan_notice(&plan)))
                };
                if plan_check.is_some() {
                    tracing::info!(
                        node = AGENT_NODE,
                        "plan check: the answer came with steps open; one more turn"
                    );
                }
                let appended = if inbox_messages.is_empty() && plan_check.is_none() {
                    serde_json::to_value(&response.message)?
                } else {
                    let mut batch: Vec<Value> = inbox_messages
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<std::result::Result<_, _>>()?;
                    batch.push(serde_json::to_value(&response.message)?);
                    if let Some(notice) = &plan_check {
                        batch.push(serde_json::to_value(notice)?);
                    }
                    Value::Array(batch)
                };
                let mut output = NodeOutput::update(MESSAGES_CHANNEL, appended);
                // The summary this step's compaction produced, kept for the
                // next step (only where the deployment declared the channel).
                if keeps_summary {
                    if let Some(stored) = assembler_handle
                        .as_ref()
                        .and_then(|a| a.take_stored_summary())
                    {
                        output =
                            output.with_update(COMPACTION_CHANNEL, serde_json::to_value(stored)?);
                    }
                }
                Ok(output)
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
            // No progress is a fault the loop must see for itself. The
            // decision is a pure function of the thread, so record and
            // replay reach it identically; the refused batch leaves no tool
            // event, and its notice lands in the channel like any result.
            // Reads alone may be taken again after the system has failed a
            // call since — see `identical_requests_this_turn`. The registry
            // is what says which a call is, so the test is made here.
            let all_reads = last.tool_calls.iter().all(|call| {
                tool_executor.registry().get(&call.name).is_some_and(|t| {
                    matches!(
                        t.effect(),
                        crate::record::Effect::ReadOnly | crate::record::Effect::Pure
                    )
                })
            });
            let repeats = identical_requests_this_turn(&messages, all_reads);
            if repeats + 1 >= STUCK_TURN_LIMIT {
                record_stuck_turn(&ctx, repeats + 1, crate::repair::RepairOutcome::Failed);
                return Err(RustyError::Node(format!(
                    "no progress: {} requested {} times in this turn with identical \
                     arguments — the run stops rather than loop",
                    tool_names.join(", "),
                    repeats + 1
                )));
            }
            // Failures the loop can act on: a call that failed its bound in
            // this conversation, or a write whose earlier attempt may have
            // happened, is answered from the thread — the more useful verdict
            // than "you repeated yourself". The hard stop above still ends a
            // turn that asks the same thing three times.
            let refusals: Vec<Option<crate::tool::ToolFailure>> = last
                .tool_calls
                .iter()
                .map(|call| crate::tool::failure_policy(&messages, call, tool_executor.registry()))
                .collect();
            if !refusals.is_empty() && refusals.iter().all(Option::is_some) {
                let refused: Vec<ChatMessage> = last
                    .tool_calls
                    .iter()
                    .zip(refusals.iter())
                    .map(|(call, refusal)| {
                        ChatMessage::tool_result(
                            call.id.clone(),
                            format!(
                                "ERROR: {}",
                                serde_json::to_string(refusal.as_ref().expect("refused"))
                                    .unwrap_or_default()
                            ),
                        )
                    })
                    .collect();
                return Ok(NodeOutput::update(
                    MESSAGES_CHANNEL,
                    serde_json::to_value(&refused)?,
                ));
            }
            // Reads that keep answering nothing new: after the bound the
            // next read is refused with a notice; a read asked for after
            // the notice that still answered nothing new ends the run. Only
            // reads — a write is judged by the effect boundary and the
            // failure policy, never by whether its answer looked familiar.
            let stale = stale_results_in_a_row(&messages);
            if all_reads && stale >= NO_NEW_FACT_LIMIT {
                if refused_for_nothing_new_since_last_fact(&messages) {
                    record_stuck_turn(&ctx, stale, crate::repair::RepairOutcome::Failed);
                    return Err(RustyError::Node(format!(
                        "no progress: {stale} reads in a row answered nothing new ({} last) — the run stops rather than loop",
                        tool_names.join(", ")
                    )));
                }
                record_stuck_turn(&ctx, stale, crate::repair::RepairOutcome::Repaired);
                tracing::warn!(node = TOOLS_NODE, tools = ?tool_names, stale, "reads answering nothing new refused");
                let refused: Vec<ChatMessage> = last
                    .tool_calls
                    .iter()
                    .map(|call| ChatMessage::tool_result(call.id.clone(), NO_NEW_FACT_NOTICE))
                    .collect();
                return Ok(NodeOutput::update(
                    MESSAGES_CHANNEL,
                    serde_json::to_value(&refused)?,
                ));
            }
            if repeats >= 1 {
                record_stuck_turn(&ctx, repeats + 1, crate::repair::RepairOutcome::Repaired);
                tracing::warn!(
                    node = TOOLS_NODE,
                    tools = ?tool_names,
                    "repeated identical tool call refused"
                );
                let refused: Vec<ChatMessage> = last
                    .tool_calls
                    .iter()
                    .map(|call| ChatMessage::tool_result(call.id.clone(), REPEATED_CALL_NOTICE))
                    .collect();
                return Ok(NodeOutput::update(
                    MESSAGES_CHANNEL,
                    serde_json::to_value(&refused)?,
                ));
            }
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
            let mut tool_executor = match &evidence {
                EvidenceMode::None => match ctx.effect_journal() {
                    Some(journal) => {
                        let parent = invocation_parent(&ctx, TOOLS_NODE)?;
                        let mut wrapped = ToolRegistry::new();
                        for name in tool_executor.registry().names() {
                            let tool = tool_executor.registry().get(&name).expect(
                                "tool names iterated from a registry resolve in that registry",
                            );
                            wrapped.register_shared(Arc::new(
                                RecordingTool::new(tool, journal.clone(), parent.clone())
                                    .node(TOOLS_NODE),
                            ));
                        }
                        ToolExecutor::new(wrapped)
                            .with_guard_journal(journal.clone(), parent.clone())
                    }
                    None => tool_executor,
                },
                EvidenceMode::Record(journal) => {
                    let parent = invocation_parent(&ctx, TOOLS_NODE)?;
                    let mut wrapped = ToolRegistry::new();
                    for name in tool_executor.registry().names() {
                        let tool = tool_executor
                            .registry()
                            .get(&name)
                            .expect("tool names iterated from a registry resolve in that registry");
                        wrapped.register_shared(Arc::new(
                            RecordingTool::new(tool, journal.clone(), parent.clone())
                                .node(TOOLS_NODE),
                        ));
                    }
                    ToolExecutor::new(wrapped).with_guard_journal(journal.clone(), parent.clone())
                }
                EvidenceMode::Replay { source, journal } => {
                    let parent = invocation_parent(&ctx, TOOLS_NODE)?;
                    let mut wrapped = ToolRegistry::new();
                    for name in tool_executor.registry().names() {
                        let tool = tool_executor
                            .registry()
                            .get(&name)
                            .expect("tool names iterated from a registry resolve in that registry");
                        wrapped.register_shared(Arc::new(ReplayingTool::new(
                            tool,
                            source.clone(),
                            journal.clone(),
                            parent.clone(),
                        )));
                    }
                    ToolExecutor::new(wrapped).with_guard_journal(journal.clone(), parent.clone())
                }
            };
            // The executor attaches both cross-cutting boundaries to the
            // node context. Re-attach them after evidence wrapping so the
            // finalized, post-middleware call is admitted immediately before
            // the recording/replay wrapper (and ultimately the tool) runs.
            tool_executor = tool_executor
                .with_middleware(ctx.middleware().clone())
                .with_call_context(ctx.thread_id(), TOOLS_NODE)
                .with_tool_guards(ctx.tool_guards().to_vec());
            // Every dispatched tool can ask which run it acts in: who it is
            // for, which agent, the thread, the journal.
            {
                let journal = match &evidence {
                    EvidenceMode::Record(journal) => Some(journal.clone()),
                    EvidenceMode::Replay { journal, .. } => Some(journal.clone()),
                    EvidenceMode::None => ctx.effect_journal().cloned(),
                };
                tool_executor = tool_executor.with_run_context(crate::tool::RunContext {
                    run_id: journal
                        .as_ref()
                        .map(|j| j.run_id().to_owned())
                        .unwrap_or_default(),
                    thread_id: ctx.thread_id().to_owned(),
                    attribution: ctx.config().extra.get(ATTRIBUTION_KEY).cloned(),
                    execution: ctx.config().extra.get(EXECUTION_KEY).cloned(),
                    agent_id: ctx
                        .config()
                        .extra
                        .get(AGENT_ID_KEY)
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    counterpart: ctx
                        .config()
                        .extra
                        .get(COUNTERPART_KEY)
                        .map(crate::tool::Counterpart::from_value),
                    journal,
                });
            }
            if let Some(admission) = ctx.effect_admission() {
                tool_executor = tool_executor.with_effect_admission(admission.clone());
            }
            // A call admission would refuse for want of an approval pauses
            // the run here, before anything executes, and a person decides.
            // Approving puts tokens on the resumed run's config, so the same
            // calls pass admission on re-execution; denying arrives as the
            // resume value, and those calls become a refusal in words the
            // model can act on instead of a retry.
            // Exact replay reproduces a run that already asked and was
            // answered; it serves recorded results and never pauses.
            let replaying = matches!(evidence, EvidenceMode::Replay { .. });
            let mut denied = denied_calls(ctx.resume_value());
            // A call the failure policy refused is answered from the thread
            // and never asks for an approval it will not use.
            for (call, refusal) in last.tool_calls.iter().zip(refusals.iter()) {
                if let Some(failure) = refusal {
                    denied.entry(call.id.clone()).or_insert_with(|| {
                        format!(
                            "ERROR: {}",
                            serde_json::to_string(failure).unwrap_or_default()
                        )
                    });
                }
            }
            let needed: Vec<_> = match ctx.approval_gate() {
                Some(gate) if !replaying => tool_executor
                    .approvals_needed(gate, &last.tool_calls)
                    .into_iter()
                    .filter(|n| !denied.contains_key(&n.call_id))
                    .collect(),
                _ => Vec::new(),
            };
            if !needed.is_empty() {
                return Err(ctx.interrupt(json!({
                    "kind": APPROVAL_INTERRUPT_KIND,
                    "requests": needed,
                })));
            }
            let to_run: Vec<ToolCall> = last
                .tool_calls
                .iter()
                .filter(|c| !denied.contains_key(&c.id))
                .cloned()
                .collect();
            // Per-call error policy: see ToolExecutor::execute_batch docs.
            let mut ran = tool_executor.execute_batch(&to_run).await.into_iter();
            let results: Vec<ChatMessage> = last
                .tool_calls
                .iter()
                .map(|call| match denied.get(&call.id) {
                    Some(notice) => ChatMessage::tool_result(call.id.clone(), notice.clone()),
                    None => ran.next().expect("one result per call that ran"),
                })
                .collect();
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
        let messages = read_messages(&state)?;
        let last = messages.last();
        let needs_tools = last.map(ChatMessage::has_tool_calls).unwrap_or(false);
        // A plan-check notice is the loop's own word: the model answers again.
        let plan_check = last.is_some_and(|m| {
            m.role == crate::llm::Role::System
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.starts_with(PLAN_NOTICE_PREFIX))
        });
        Ok(if needs_tools {
            Route::Node(TOOLS_NODE.to_owned())
        } else if plan_check {
            Route::Node(AGENT_NODE.to_owned())
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

    #[test]
    fn the_newest_plan_is_read_from_the_plan_calls_and_rendered_with_its_open_steps() {
        let plan = |args: Value| {
            ChatMessage::assistant_tool_calls(vec![crate::llm::ToolCall::new("p", PLAN_TOOL, args)])
        };
        let messages = vec![
            ChatMessage::user("three checks"),
            plan(
                serde_json::json!({"steps": [{"text": "count open incidents"}, {"text": "find the newest"}, {"text": "who is assigned"}]}),
            ),
            ChatMessage::tool_result("p", "{}"),
            plan(
                serde_json::json!({"steps": [{"text": "count open incidents", "status": "done"}, {"text": "find the newest", "status": "in_progress"}, {"text": "who is assigned", "status": "skipped", "note": "no assignee field"}]}),
            ),
            ChatMessage::tool_result("p", "{}"),
        ];
        let (at, plan) = latest_plan(&messages).expect("a plan");
        assert_eq!(at, 3);
        assert_eq!(plan.done(), 1);
        assert_eq!(plan.open().len(), 1, "doing is open, skipped is not");
        assert_eq!(plan.steps[1].status, "doing");
        let rendered = plan.render();
        assert!(
            rendered.starts_with("Plan — 1 of 3 done, 1 open"),
            "{rendered}"
        );
        assert!(
            rendered.contains("3. [skipped] who is assigned — no assignee field"),
            "{rendered}"
        );
        assert!(!plan_checked_since(&messages, at));
        let mut nudged = messages.clone();
        nudged.push(ChatMessage::assistant("Done."));
        nudged.push(ChatMessage::system(plan_notice(&plan)));
        assert!(plan_checked_since(&nudged, at), "one check per plan");
        assert!(plan_notice(&plan).contains("1 step not done: [doing] find the newest"));
        // No plan call: nothing to render or check.
        assert!(latest_plan(&[ChatMessage::user("hi"), ChatMessage::assistant("hello")]).is_none());
    }

    #[test]
    fn a_call_the_run_ended_before_answering_is_not_a_repeat_when_made_again() {
        let call = |id: &str| {
            crate::llm::ToolCall::new(id, "desk.count", serde_json::json!({"q": "open"}))
        };
        // The halted run's call got the unrecorded notice; the resumed run
        // makes the same call: not a repeat — the notice asked for it.
        let messages = vec![
            ChatMessage::user("how many open?"),
            ChatMessage::assistant_tool_calls(vec![call("c1")]),
            ChatMessage::tool_result("c1", UNRECORDED_READ_NOTICE),
            ChatMessage::assistant_tool_calls(vec![call("c2")]),
        ];
        assert_eq!(identical_requests_this_turn(&messages, true), 0);
        // Answered for real, the same call again is a repeat.
        let answered = vec![
            ChatMessage::user("how many open?"),
            ChatMessage::assistant_tool_calls(vec![call("c1")]),
            ChatMessage::tool_result("c1", "{\"count\": 2}"),
            ChatMessage::assistant_tool_calls(vec![call("c2")]),
        ];
        assert_eq!(identical_requests_this_turn(&answered, true), 1);
    }

    #[test]
    fn empty_and_re_found_answers_are_nothing_new() {
        // Empty answers, however phrased, answer nothing.
        assert!(fact_body("no result").is_none());
        assert!(fact_body("{\"result\":[]}").is_none());
        assert!(fact_body(
            "{\"asked\":{\"contains\":\"x\"},\"count\":0,\"found\":false,\"notes\":[]}"
        )
        .is_none());
        // The same notes under two phrasings are one answer.
        let a = fact_body("{\"asked\":{\"contains\":\"catalog\"},\"count\":1,\"found\":true,\"notes\":[{\"text\":\"laptops\"}]}").unwrap();
        let b = fact_body("{\"asked\":{\"key\":\"catalog:laptops\"},\"found\":true,\"count\":1,\"notes\":[{\"text\":\"laptops\"}]}").unwrap();
        assert_eq!(a, b);
        assert_ne!(
            a,
            fact_body("{\"count\":1,\"notes\":[{\"text\":\"monitors\"}]}").unwrap()
        );
        // Three batches: a real answer, then the same answer re-found, then an empty one.
        let call = |id: &str| {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                id,
                "memory.recall",
                serde_json::json!({}),
            )])
        };
        let messages = vec![
            ChatMessage::user("what laptops do we have"),
            call("c1"),
            ChatMessage::tool_result("c1", "{\"asked\":{\"contains\":\"catalog\"},\"count\":1,\"notes\":[{\"text\":\"laptops\"}]}"),
            call("c2"),
            ChatMessage::tool_result("c2", "{\"asked\":{\"key\":\"catalog\"},\"count\":1,\"notes\":[{\"text\":\"laptops\"}]}"),
            call("c3"),
            ChatMessage::tool_result("c3", "{\"asked\":{\"contains\":\"monitors\"},\"count\":0,\"found\":false,\"notes\":[]}"),
        ];
        assert_eq!(
            stale_results_in_a_row(&messages),
            2,
            "the re-found and the empty answers are a streak of two"
        );
    }

    #[test]
    fn a_turn_spent_on_nothing_new_is_stuck_however_the_reads_are_spaced() {
        // Empty and re-found answers alternate with one new answer each
        // time, so no three are ever in a row — but six in one turn are.
        let call = |id: &str| {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                id,
                "memory.recall",
                serde_json::json!({}),
            )])
        };
        let mut messages = vec![ChatMessage::user("what laptops do we have")];
        let mut n = 0;
        for i in 0..7 {
            n += 1;
            let id = format!("c{n}");
            messages.push(call(&id));
            let content = if i % 2 == 0 {
                format!("{{\"asked\":{{\"contains\":\"q{i}\"}},\"count\":0,\"found\":false,\"notes\":[]}}")
            } else {
                format!("{{\"asked\":{{\"contains\":\"q{i}\"}},\"count\":1,\"notes\":[{{\"text\":\"note {i}\"}}]}}")
            };
            messages.push(ChatMessage::tool_result(&id, content));
        }
        // 4 empty, 3 new, none in a row beyond one: below the bound.
        assert!(
            stale_results_in_a_row(&messages) < NO_NEW_FACT_LIMIT,
            "{}",
            stale_results_in_a_row(&messages)
        );
        for i in 7..11 {
            n += 1;
            let id = format!("c{n}");
            messages.push(call(&id));
            let content = if i % 2 == 0 {
                format!("{{\"asked\":{{\"contains\":\"q{i}\"}},\"count\":0,\"found\":false,\"notes\":[]}}")
            } else {
                format!("{{\"asked\":{{\"contains\":\"q{i}\"}},\"count\":1,\"notes\":[{{\"text\":\"note {i}\"}}]}}")
            };
            messages.push(ChatMessage::tool_result(&id, content));
        }
        // 6 empty this turn: stuck, whatever came between.
        assert!(
            stale_results_in_a_row(&messages) >= NO_NEW_FACT_LIMIT,
            "{}",
            stale_results_in_a_row(&messages)
        );
        // A new turn starts the count over.
        messages.push(ChatMessage::user("and monitors?"));
        assert_eq!(stale_results_in_a_row(&messages), 0);
    }
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

    /// Without a context policy nothing compacts the history; the library
    /// ceiling stops the run with the words to set one, and a conversation
    /// under it runs as before.
    #[tokio::test]
    async fn without_a_policy_the_run_stops_at_the_library_ceiling() {
        let model = Arc::new(ScriptedModel::new(vec![
            ChatMessage::assistant("done"),
            ChatMessage::assistant("done"),
        ]));
        let graph = create_react_agent(model.clone(), registry()).unwrap();
        let huge = "x".repeat((LIBRARY_CONTEXT_CEILING_TOKENS as usize) * 5);
        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user(huge)).unwrap()]
        }))
        .unwrap();
        let ctx = NodeContext::new(state, NodeConfig::default());
        let error = graph
            .node(AGENT_NODE)
            .unwrap()
            .run(ctx)
            .await
            .expect_err("the ceiling stops the run");
        match error {
            RustyError::Budget(words) => assert!(
                words.contains("library ceiling") && words.contains("ContextPolicy"),
                "{words}"
            ),
            other => panic!("not a budget stop: {other:?}"),
        }
        assert!(
            model.seen_tool_schemas.lock().unwrap().is_empty(),
            "the model was never called"
        );

        let state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("x".repeat(1_000))).unwrap()]
        }))
        .unwrap();
        let ctx = NodeContext::new(state, NodeConfig::default());
        assert!(graph.node(AGENT_NODE).unwrap().run(ctx).await.is_ok());
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
