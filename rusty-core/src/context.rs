//! Context engineering (R0.13 wave 1): the deterministic, budgeted, journaled
//! context assembly pipeline.
//!
//! The design doc is `docs/agent-core-design.md` ("Context engineering"). The
//! governing claim: an agent's context is the highest-leverage ungoverned
//! surface left in the runtime, and it assembles the same way everything
//! since R0.5 has assembled — deterministic assembly over journaled evidence.
//! One type, [`ContextPipeline`], driven by a versioned [`ContextPolicy`]
//! (carried through the candidate pipeline as
//! [`crate::learn::CandidateContent::ContextPolicy`], surface
//! `context:{name}`), turns a run's state into a [`ContextAssembly`]: the
//! exact message list and tool schemas handed to [`ChatModel::chat`], plus
//! the section manifest recording what every section carried.
//!
//! # Sections
//!
//! Six sections in canonical order ([`SECTION_ORDER`]), each enabled and
//! budgeted by the policy: `identity` (system prompt, pinned at admission),
//! `task` (the current instruction), `skills` (tier-1 metadata, tier-2 bodies
//! of selected skills), `tools` (the shortlisted schemas — the `tools`
//! argument, not messages), `memory` (governed recall through
//! [`JournaledMemory`]), `history` (the verbatim `messages` channel,
//! compacted when triggered). Per-section budgets are declared in the policy;
//! the pipeline enforces them and the total ([`ContextPolicy::budget`])
//! against one accounting.
//!
//! The pipeline's invariants are the release's contract:
//!
//! - **Determinism is structural.** Equal inputs and equal policy produce a
//!   byte-equal assembly. Section producers are pure functions over their
//!   inputs; ordering is declared, not incidental; the pipeline never reads a
//!   clock (the journaled memory read stamps `as_of` through the run's clock
//!   with the shipped live/replay parity).
//! - **Budgets compose.** Section costs are counted through the pinned
//!   [`TokenCounter`]; a section that overflows its budget applies its
//!   overflow rule — truncate for memory/history (and any section the policy
//!   declares truncatable), fail for identity, whose default is
//!   [`BudgetOverflow::Fail`]: a system prompt that does not fit is a
//!   configuration error, not a truncation. The manifest message's own
//!   estimated tokens come off the top of the total budget before sections
//!   pack; a total overflow is absorbed by shrinking the truncatable sections
//!   (history first, then memory) and fails loud when neither can absorb it.
//! - **The assembly is the journal payload.** No new event kind: the
//!   assembled messages *are* the journaled `ModelCall` input, and the
//!   section manifest rides inside it as a reserved, model-visible metadata
//!   message ([`MANIFEST_MESSAGE_NAME`]) — the sole carrier, because
//!   `ChatModel` is `chat(messages, tools)` and there is no request
//!   side-channel. Its wording is pinned here and by the golden assembly, so
//!   a wording change is a visible, reviewable diff.
//!
//! # Token accounting
//!
//! [`TokenCounter`] is the seam the R0.8 design anticipated:
//! `count(&[ChatMessage], model_id) -> u32`, with the shipped estimate
//! (serialized bytes ÷ [`TOKEN_BYTES_PER_ESTIMATE`], plus the declared
//! margin) as the built-in floor implementation ([`EstimatedTokenCounter`])
//! and provider-precise tokenizers pluggable per model id. The policy pins
//! which counter applies ([`TokenizerPin`]); the manifest records which
//! counter ran. A provider counter must be local and pure — a bundled
//! tokenizer table, never a call. Multi-item sections (history, memory,
//! tools) are accounted per item through the same counter: conservative,
//! deterministic, and uniform across counters.
//!
//! # Mid-run history compaction
//!
//! When the history section's estimated cost exceeds the policy's trigger,
//! the pipeline issues a summarization call over the oldest span (keeping
//! the most recent [`CompactionPolicy::keep_recent_messages`] verbatim) and
//! substitutes the summary — marked as generated — in the *assembled*
//! history section. The `messages` channel itself is untouched: the journal
//! and checkpoints keep the verbatim history as evidence, so compaction is
//! revisable (a later evaluation can re-assemble with a different trigger).
//! The watermark — how many leading history messages the summary replaced —
//! is recorded in the section manifest.
//!
//! Price the cost amplification before pinning a trigger: once compaction
//! fires, every later assembly re-summarizes the (growing) prefix — one
//! summarization call per assembly over a longer span. `trigger_tokens` and
//! `keep_recent_messages` are cost policy, not just quality policy.
//!
//! The summarization call journals and replays like every other model call,
//! through the per-mode wiring the design fixes (the `ChatModel` seam carries
//! no parent, no replay source, no mode switch, so the wiring is
//! construction-time knowledge):
//!
//! - recording mode: `RecordingChatModel::new(summarizer, journal.clone(),
//!   CONTEXT_PIPELINE_PARENT)`;
//! - replay mode: `ReplayingChatModel::new(sentinel, source.clone(), journal,
//!   parent)` over the run's own shared `ReplaySource` (it is `Clone`; the
//!   compaction call is one more journaled `ModelCall` in the run's stream,
//!   served in order by sequence + canonical request hash);
//! - unjournaled mode: the bare summarizer.
//!
//! Pipeline-internal effects cannot learn the invocation's node-input parent,
//! so they journal under the static, documented parent
//! [`CONTEXT_PIPELINE_PARENT`] — causal attachment is to the run, with the
//! true ordering recovered from journal sequence numbers. Replay determinism
//! follows: the trigger is a pure function of the history prefix plus the
//! pinned policy, and the summary is replay-served, so a replayed pipeline
//! re-fires at the same watermark and the assembled request hash-matches the
//! recorded `ModelCall` it precedes.
//!
//! # Consuming from ReAct
//!
//! [`AssemblingChatModel`] is the composition recipe: it runs the pipeline
//! over each call's `messages`/`tools` and forwards the assembly to the inner
//! model. The journaled `ModelCall` input *is* the assembled request, so the
//! evidence wrapper sits **inside** the assembler: recording mode builds
//! `AssemblingChatModel { inner: RecordingChatModel(real_model, journal,
//! parent) }`, replay mode `AssemblingChatModel { inner:
//! ReplayingChatModel(sentinel, source.clone(), journal, parent) }`, and the
//! summarizer slot is wrapped per mode the same way (parented to
//! [`CONTEXT_PIPELINE_PARENT`]). `create_react_agent(model, tools)` receives
//! the assembler; `react.rs` never knows.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Result, RustyError};
use crate::llm::{ChatMessage, ChatModel, ChatResponse, TokenChunk};
use crate::memory::{
    estimated_tokens, BudgetOverflow, ContextBudget, JournaledMemory, MemoryQuery, MemoryRecord,
    TOKEN_BYTES_PER_ESTIMATE,
};
use crate::record::{Effect, PayloadRef};
use crate::tool_select::{
    SelectionFeatures, ToolManifest, ToolOutcomeStats, ToolSelectionPolicy, ToolShortlist,
};

fn invalid(message: impl Into<String>) -> RustyError {
    // Context assembly failures are configuration errors: the policy, the
    // inputs, or their composition is wrong — the invalid-update class
    // covers contract validation without growing the error taxonomy.
    RustyError::InvalidUpdate(message.into())
}

/// The static, documented causal parent of pipeline-internal effects (the
/// compaction summarization call, the pipeline's memory reads). The
/// `ChatModel` seam carries no `PARENT_EVENT_KEY`, so pipeline effects
/// journal under this reserved marker naming the pipeline as their causal
/// origin; the true ordering is recovered from journal sequence numbers.
pub const CONTEXT_PIPELINE_PARENT: &str = "rusty:context_pipeline";

/// The only [`ContextPolicy::schema_version`] this module assembles under.
pub const CONTEXT_POLICY_SCHEMA_VERSION: &str = "context-policy-v1";

/// The [`SectionManifest`] format version, recorded inside every manifest.
pub const MANIFEST_FORMAT_VERSION: &str = "context-manifest-v1";

/// The reserved `name` of the manifest message — the sole carrier of the
/// section manifest inside the journaled `ModelCall` input.
pub const MANIFEST_MESSAGE_NAME: &str = "rusty.context_manifest";

/// The built-in counter's id ([`TokenizerPin::counter`] default): the
/// shipped bytes-per-token estimate plus the declared margin.
pub const ESTIMATED_COUNTER_ID: &str = "estimated";

/// The marker prefix on the generated summary message: a compacted history
/// section starts with a system message whose content begins with this line,
/// so the model (and the auditor) sees that the span is generated, not
/// verbatim. Wording pinned here and by the golden assembly.
pub const SUMMARY_MARKER: &str = "[context: generated summary replacing history messages 1..=";

/// The six sections in canonical assembly order.
pub const SECTION_ORDER: [SectionKind; 6] = [
    SectionKind::Identity,
    SectionKind::Task,
    SectionKind::Skills,
    SectionKind::Tools,
    SectionKind::Memory,
    SectionKind::History,
];

// --------------------------------------------------------------------- //
// Token accounting: the seam, the estimate as floor
// --------------------------------------------------------------------- //

/// The token-counting seam: how the pipeline measures message cost.
///
/// Implementations must be local and pure — a bundled tokenizer table, never
/// a call: an assembly that calls out to count itself is a replay hazard.
/// The policy pins which counter applies ([`TokenizerPin`]), so assembly
/// stays deterministic under a pinned policy, and the manifest records
/// [`TokenCounter::id`] so an auditor reads the accounting the assembly
/// actually applied.
pub trait TokenCounter: Send + Sync {
    /// The counter's stable identifier, journaled in the section manifest.
    fn id(&self) -> &str;

    /// The estimated (or provider-precise) token cost of `messages` for
    /// `model_id`.
    fn count(&self, messages: &[ChatMessage], model_id: &str) -> u32;
}

/// The shipped floor: serialized bytes ÷ [`TOKEN_BYTES_PER_ESTIMATE`], plus
/// the declared safety margin — the same accounting
/// [`crate::memory::estimated_tokens`] applies to memory records, extended to
/// whole messages. Deterministic, local, and always legal: the baseline every
/// provider-precise counter is measured against.
#[derive(Debug, Clone, Copy)]
pub struct EstimatedTokenCounter {
    margin_percent: u32,
}

impl EstimatedTokenCounter {
    /// The estimate with safety margin `margin_percent` (percent).
    pub fn new(margin_percent: u32) -> Self {
        Self { margin_percent }
    }
}

impl TokenCounter for EstimatedTokenCounter {
    fn id(&self) -> &str {
        ESTIMATED_COUNTER_ID
    }

    fn count(&self, messages: &[ChatMessage], model_id: &str) -> u32 {
        let _ = model_id; // the estimate is model-agnostic; the margin is the hedge
        let bytes: u64 = messages
            .iter()
            .map(|m| serde_json::to_vec(m).map(|v| v.len() as u64).unwrap_or(0))
            .sum();
        estimated_tokens(bytes, self.margin_percent)
    }
}

/// The largest byte length whose estimate under `margin_percent` stays within
/// `tokens` — the truncation target for text sections. Conservative (the
/// estimate's integer division rounds the fit down, never up).
fn byte_budget_for_tokens(tokens: u32, margin_percent: u32) -> usize {
    let bytes = (tokens as u128) * 100 * (TOKEN_BYTES_PER_ESTIMATE as u128)
        / (100 + margin_percent as u128);
    bytes.min(usize::MAX as u128) as usize
}

/// Truncate `text` to `max_bytes` on a char boundary.
fn truncate_to_byte_budget(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

// --------------------------------------------------------------------- //
// The policy
// --------------------------------------------------------------------- //

/// One section of the assembly, in canonical order. Closed enum — the
/// pipeline matches exhaustively; the order is declared ([`SECTION_ORDER`]),
/// never incidental.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    /// System prompt and agent manifest summary; pinned at admission.
    Identity,
    /// The current task/instruction.
    Task,
    /// Tier-1 metadata of shortlisted skills, tier-2 bodies of selected ones.
    Skills,
    /// The shortlisted tool schemas (the `tools` argument, not messages).
    Tools,
    /// Governed recall through [`JournaledMemory`].
    Memory,
    /// The verbatim `messages` channel, compacted when triggered.
    History,
}

impl SectionKind {
    /// The wire name (`identity` / `task` / `skills` / `tools` / `memory` /
    /// `history`).
    pub fn as_str(&self) -> &'static str {
        match self {
            SectionKind::Identity => "identity",
            SectionKind::Task => "task",
            SectionKind::Skills => "skills",
            SectionKind::Tools => "tools",
            SectionKind::Memory => "memory",
            SectionKind::History => "history",
        }
    }

    /// The overflow rule when the policy declares none: identity and task
    /// fail (a truncated instruction is a silent behavior change — a
    /// configuration error, not a truncation); every other section truncates.
    fn default_overflow(&self) -> BudgetOverflow {
        match self {
            SectionKind::Identity | SectionKind::Task => BudgetOverflow::Fail,
            _ => BudgetOverflow::Truncate,
        }
    }
}

/// Per-section policy: the budget and what to do when the content does not
/// fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionPolicy {
    /// The section's budget, in the pinned counter's tokens.
    pub budget_tokens: u32,

    /// The overflow rule; absent from the wire while unset, resolving to the
    /// section kind's default ([`SectionKind::default_overflow`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<BudgetOverflow>,
}

impl SectionPolicy {
    /// A section budget with the kind's default overflow rule.
    pub fn new(budget_tokens: u32) -> Self {
        Self {
            budget_tokens,
            overflow: None,
        }
    }

    /// Builder-style: declare the overflow rule explicitly.
    pub fn with_overflow(mut self, overflow: BudgetOverflow) -> Self {
        self.overflow = Some(overflow);
        self
    }

    fn resolved_overflow(&self, kind: SectionKind) -> BudgetOverflow {
        self.overflow.unwrap_or_else(|| kind.default_overflow())
    }
}

/// Sparse-wire predicate for [`ToolsSectionPolicy::selection`]: the default
/// selection policy serializes as absence, so a policy that never tuned
/// selection keeps its pre-selection wire shape byte-for-byte.
fn is_default_selection_policy(policy: &ToolSelectionPolicy) -> bool {
    *policy == ToolSelectionPolicy::default()
}

/// The tools section's policy: the budget, the overflow rule, and the
/// shortlist policy ([`ToolSelectionPolicy`]: cutoff, k, feature weights).
/// Selection *policy* is assembly policy — it lives here, not per tool. When
/// the assembly is handed manifests ([`ContextInputs::tool_manifests`]), the
/// pipeline runs [`crate::tool_select::shortlist`] itself under this policy
/// and records the full selection outcome in the section manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsSectionPolicy {
    /// The section's budget, in the pinned counter's tokens.
    pub budget_tokens: u32,

    /// The overflow rule; absent from the wire while unset, resolving to the
    /// kind's default (truncate — the shortlist already made the cut).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<BudgetOverflow>,

    /// The shortlist policy; absent from the wire while it equals
    /// [`ToolSelectionPolicy::default`].
    #[serde(default, skip_serializing_if = "is_default_selection_policy")]
    pub selection: ToolSelectionPolicy,
}

impl ToolsSectionPolicy {
    /// A section budget with the kind's default overflow rule and the default
    /// selection policy.
    pub fn new(budget_tokens: u32) -> Self {
        Self {
            budget_tokens,
            overflow: None,
            selection: ToolSelectionPolicy::default(),
        }
    }

    /// Builder-style: declare the overflow rule explicitly.
    pub fn with_overflow(mut self, overflow: BudgetOverflow) -> Self {
        self.overflow = Some(overflow);
        self
    }

    /// Builder-style: the shortlist policy (cutoff, k, feature weights).
    pub fn with_selection(mut self, selection: ToolSelectionPolicy) -> Self {
        self.selection = selection;
        self
    }

    fn resolved_overflow(&self) -> BudgetOverflow {
        self.overflow
            .unwrap_or_else(|| SectionKind::Tools.default_overflow())
    }
}

/// Sparse-wire predicate for [`MemorySectionPolicy::query`]: an empty query
/// (match-everything, modulo the two shipped defaults) serializes as absence.
fn memory_query_is_empty(query: &MemoryQuery) -> bool {
    *query == MemoryQuery::default()
}

/// The memory section's policy: the budget plus the policy-pinned base query
/// the per-assembly journaled read runs (a run narrows from here through the
/// query itself; it never widens past the policy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySectionPolicy {
    /// The section's budget, in the pinned counter's tokens.
    pub budget_tokens: u32,

    /// The overflow rule (default truncate — the base rank already made the
    /// cut; packing keeps the highest-ranked records that fit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<BudgetOverflow>,

    /// The base query every assembly's journaled read starts from.
    #[serde(default, skip_serializing_if = "memory_query_is_empty")]
    pub query: MemoryQuery,
}

impl MemorySectionPolicy {
    fn resolved_overflow(&self) -> BudgetOverflow {
        self.overflow
            .unwrap_or_else(|| SectionKind::Memory.default_overflow())
    }
}

/// The compaction policy: when the history section compacts, how much stays
/// verbatim, the summary's bound, and the summarizer's pinned prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionPolicy {
    /// The history section's estimated-token cost above which compaction
    /// fires.
    pub trigger_tokens: u32,

    /// How many trailing history messages stay verbatim; everything older is
    /// summarized.
    pub keep_recent_messages: usize,

    /// The generated summary's hard bound, in the pinned counter's tokens;
    /// an over-long summary is truncated to fit and the manifest says so.
    pub summary_max_tokens: u32,

    /// The summarizer's system prompt — policy-pinned, so the behavioral
    /// influence of the compaction wording versions with everything else.
    pub prompt: String,
}

/// Which counter the pipeline applies. The built-in floor is
/// [`ESTIMATED_COUNTER_ID`]; a provider-precise counter names itself here and
/// is supplied at pipeline construction — the pin and the instance must
/// agree, or construction fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizerPin {
    /// The counter id ([`TokenCounter::id`]).
    pub counter: String,
}

impl Default for TokenizerPin {
    fn default() -> Self {
        Self {
            counter: ESTIMATED_COUNTER_ID.to_owned(),
        }
    }
}

/// The versioned assembly policy: section layouts and budgets, the tokenizer
/// pin, the compaction trigger. Carried through the candidate pipeline as
/// [`crate::learn::CandidateContent::ContextPolicy`] — `Value`-bodied there
/// while the schema moves, parsed fail-closed here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPolicy {
    /// The policy schema version; must equal
    /// [`CONTEXT_POLICY_SCHEMA_VERSION`].
    pub schema_version: String,

    /// The total budget the assembly composes against (the shipped type:
    /// estimated-token accounting with the declared margin).
    pub budget: ContextBudget,

    /// Which counter applies (default: the shipped estimate).
    #[serde(default)]
    pub tokenizer: TokenizerPin,

    /// The identity section; absent = the section is not assembled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<SectionPolicy>,

    /// The task section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<SectionPolicy>,

    /// The skills section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SectionPolicy>,

    /// The tools section (budget + the shortlist policy the pipeline runs
    /// when handed manifests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsSectionPolicy>,

    /// The memory section (budget + base query).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySectionPolicy>,

    /// The history section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<SectionPolicy>,

    /// The compaction policy; absent = history is never compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionPolicy>,
}

impl ContextPolicy {
    /// Parse a policy from its candidate-carried `Value` form, fail-closed:
    /// an unknown schema version or a malformed body is a configuration
    /// error, never a guess.
    pub fn from_value(value: &Value) -> Result<Self> {
        let policy: Self = serde_json::from_value(value.clone()).map_err(|e| {
            invalid(format!(
                "context policy does not parse: {e} — the candidate body must be a \
                 {CONTEXT_POLICY_SCHEMA_VERSION} policy"
            ))
        })?;
        if policy.schema_version != CONTEXT_POLICY_SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported context policy schema version `{}` (this runtime assembles \
                 `{CONTEXT_POLICY_SCHEMA_VERSION}`) — a policy from a different schema version \
                 is a different contract",
                policy.schema_version
            )));
        }
        Ok(policy)
    }

    /// The policy in its candidate-carried `Value` form.
    pub fn to_value(&self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }
}

// --------------------------------------------------------------------- //
// Assembly inputs
// --------------------------------------------------------------------- //

/// One skill as the skills section carries it: the tier-1 metadata every
/// shortlisted skill shows, plus the tier-2 body when the skill is selected.
/// The name/revision/content-hash pin is what the manifest journals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSectionEntry {
    /// The skill's name.
    pub name: String,
    /// The selected revision.
    pub revision: String,
    /// The package content address (the skill plane's own digest).
    pub content_hash: String,
    /// Tier-1 metadata (the when-to-use summary).
    pub metadata: String,
    /// The tier-2 body, when the skill is selected into context.
    pub body: Option<String>,
}

/// What one assembly runs over. Everything here is a pure input: the pipeline
/// reads no clocks and no stores beyond the [`JournaledMemory`] handle handed
/// to [`ContextPipeline::assemble`].
#[derive(Debug, Clone, Default)]
pub struct ContextInputs {
    /// The identity text (system prompt + manifest summary), pinned at
    /// admission. Required when the policy enables the identity section.
    pub identity: Option<String>,

    /// The current task/instruction. Required when the policy enables the
    /// task section.
    pub task: Option<String>,

    /// The shortlisted skills (selection is the skills plane's; the pipeline
    /// budgets and journals what it is handed).
    pub skills: Vec<SkillSectionEntry>,

    /// The shortlisted tool schemas, exactly as passed to
    /// [`ChatModel::chat`] (selection is the tool plane's). This is the
    /// fallback path: when [`ContextInputs::tool_manifests`] is empty the
    /// pipeline budget-packs these schemas as handed, and the section
    /// manifest records only what the budget kept.
    pub tools: Vec<Value>,

    /// The governed tools path: selection manifests for the registry's
    /// tools ([`crate::tool_select::manifests_for_registry`]). When
    /// non-empty the pipeline runs the shortlist itself under the tools
    /// section's [`ToolSelectionPolicy`] — scoring against
    /// [`ContextInputs::task_tags`], [`ContextInputs::tool_outcomes`], and
    /// [`ContextInputs::effect_ceiling`] — packs the selected schemas
    /// against the section budget, and records the full ranking and
    /// exclusions in the section manifest. `tools` is then ignored for the
    /// section (manifests are authoritative).
    pub tool_manifests: Vec<ToolManifest>,

    /// The task's capability tags, matched against manifest tags by the
    /// shortlist. Meaningful only on the manifests path.
    pub task_tags: Vec<String>,

    /// The per-tool journaled outcome snapshot the shortlist scores against,
    /// keyed by tool name. Meaningful only on the manifests path.
    pub tool_outcomes: BTreeMap<String, ToolOutcomeStats>,

    /// The run's effect ceiling: manifests above it are excluded before
    /// scoring. `None` resolves to [`Effect::NonIdempotent`] (admits all).
    /// Meaningful only on the manifests path.
    pub effect_ceiling: Option<Effect>,

    /// The verbatim `messages` channel. Never mutated: compaction substitutes
    /// the summary in the assembled section only.
    pub history: Vec<ChatMessage>,
}

// --------------------------------------------------------------------- //
// The manifest and the assembly
// --------------------------------------------------------------------- //

/// The policy pin the manifest carries: the policy's name, plus the resolved
/// candidate id and content hash when the pipeline was built from a promoted
/// candidate (the design's pin rule: `context:*` surfaces bind through the
/// generic pointer rule, and the pin is the journaled manifest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyPin {
    /// The policy's name (the `context:{name}` surface's name part).
    pub name: String,

    /// The candidate the policy was resolved from, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,

    /// The candidate's content hash, when resolved from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

/// Sparse-wire predicate: `false` serializes as absence.
fn is_false(value: &bool) -> bool {
    !*value
}

/// What the history section's compaction did, when it fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionReport {
    /// How many leading history messages the summary replaced.
    pub watermark: usize,

    /// The summary message's token cost.
    pub summary_tokens: u32,

    /// `true` when the summarizer's output exceeded
    /// [`CompactionPolicy::summary_max_tokens`] and was truncated to fit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub summary_truncated: bool,
}

/// One section's outcome: what it carried, at what cost. `ids` names the
/// content the section packed — memory content addresses, tool names, skill
/// `name@revision:hash` pins — so the journal answers "what did the model
/// see" without re-running anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionReport {
    /// The section.
    pub kind: SectionKind,

    /// The section's declared budget (before total-budget absorption).
    pub budget_tokens: u32,

    /// The budget the section actually packed against, when total-budget
    /// absorption shrank it below the declared budget — absent while equal
    /// to `budget_tokens`, so the audit closes: `used_tokens` is always
    /// accounted against `effective_budget_tokens.unwrap_or(budget_tokens)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_budget_tokens: Option<u32>,

    /// The token cost the assembly actually applied (per-item accounting for
    /// multi-item sections — the module docs' rule).
    pub used_tokens: u32,

    /// `true` when the section's content was cut short (overflow truncate or
    /// total-budget absorption).
    pub truncated: bool,

    /// The packed content's identifiers, in carried order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,

    /// The compaction outcome, when the history section compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionReport>,

    /// The full selection outcome, when the tools section ran the governed
    /// shortlist: the selected top-k plus the complete ranking and the
    /// exclusions — the audit trail for why the model saw exactly these
    /// tools, recorded even when the section budget then cut the tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortlist: Option<ToolShortlist>,
}

/// The section manifest: what the assembly carried, under which policy and
/// counter, at what cost. Rides inside the journaled `ModelCall` input as the
/// reserved manifest message ([`MANIFEST_MESSAGE_NAME`]) — model-visible
/// context, budgeted as its own accounting line
/// ([`SectionManifest::manifest_tokens`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionManifest {
    /// The manifest format version ([`MANIFEST_FORMAT_VERSION`]).
    pub format: String,

    /// The policy pin the assembly ran under.
    pub policy: PolicyPin,

    /// The counter that ran ([`TokenCounter::id`]).
    pub counter: String,

    /// The total budget the assembly composed against.
    pub budget_tokens: u32,

    /// The manifest message's own token cost — off the top of the budget
    /// before sections pack.
    pub manifest_tokens: u32,

    /// Per-section outcomes, in canonical order; only enabled sections.
    pub sections: Vec<SectionReport>,
}

/// The result of one assembly: the exact message list and tool schemas
/// handed to [`ChatModel::chat`], plus the structured manifest (whose message
/// rendering is inside `messages`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextAssembly {
    /// The assembled messages, in canonical section order — identity, the
    /// manifest message, task, skills, memory, history.
    pub messages: Vec<ChatMessage>,

    /// The tool schemas for the `tools` argument (possibly truncated by the
    /// tools section's overflow rule).
    pub tools: Vec<Value>,

    /// The structured manifest (also embedded in `messages`).
    pub manifest: SectionManifest,
}

// --------------------------------------------------------------------- //
// Frozen three-tier prompt assembly (EP-02-S09)
// --------------------------------------------------------------------- //

/// The three directive tiers that compose the frozen system prefix.
///
/// Assembled once at session start and held byte-identical for the session's
/// life so provider prefix caching, resume, and the review fork are
/// deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectiveTiers {
    /// Identity and standing guidance — the agent's self-concept and normative
    /// instructions that rarely change.
    pub stable: String,

    /// Workspace snapshot — the current project state, file tree, and active
    /// context that changes at human pace.
    pub context: String,

    /// Skills index, memory snapshot, user profile — the fastest-moving tier,
    /// captured at session start and refreshed only at new sessions.
    pub volatile: String,
}

/// One tier's forensic record: byte length and SHA-256 at assembly time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierRecord {
    /// Tier name (`stable`, `context`, `volatile`, or `whole`).
    pub kind: String,

    /// Byte length of the tier text in the concatenated prefix.
    pub bytes: usize,

    /// SHA-256 of the tier text, lowercase hex.
    pub sha256: String,
}

/// The durably recorded frozen-prefix assembly: per-tier records plus the
/// whole-prefix hash. Stored beside the session so resume on another node
/// reproduces the exact prefix without re-rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenPrefixRecord {
    /// Per-tier length-and-hash records, in concatenation order.
    pub tiers: Vec<TierRecord>,

    /// SHA-256 of the entire concatenated prefix, lowercase hex.
    pub whole_prefix_sha256: String,
}

/// The frozen prefix: concatenated tier text plus its verification record.
///
/// Created by [`ContextPipeline::assemble_frozen_prefix`] at session start
/// and verified by [`FrozenPrefix::verify`] before every provider dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenPrefix {
    /// The concatenated three-tier text that becomes the session's system
    /// prompt.
    pub text: String,

    /// The forensic record stored beside the session.
    pub record: FrozenPrefixRecord,
}

impl FrozenPrefix {
    /// Verify that `actual_prefix` is byte-identical to the prefix recorded at
    /// assembly time. On mismatch, return [`RustyError::FrozenTierViolation`]
    /// naming the first divergent tier.
    pub fn verify(&self, actual_prefix: &str) -> Result<()> {
        let actual_hash = crate::record::sha256_hex(actual_prefix.as_bytes());
        if actual_hash == self.record.whole_prefix_sha256 {
            return Ok(());
        }

        // Walk tiers in order to name the first divergent one.
        let mut offset = 0usize;
        for tier in &self.record.tiers {
            let end = (offset + tier.bytes).min(actual_prefix.len());
            let tier_text = &actual_prefix[offset..end];
            let actual_tier_hash = crate::record::sha256_hex(tier_text.as_bytes());
            if actual_tier_hash != tier.sha256 {
                return Err(RustyError::FrozenTierViolation {
                    tier: tier.kind.clone(),
                    expected_hash: tier.sha256.clone(),
                    actual_hash,
                });
            }
            offset += tier.bytes;
        }

        // Individual tiers matched but the whole did not — padding or boundary
        // drift outside the recorded tiers.
        Err(RustyError::FrozenTierViolation {
            tier: "whole".to_owned(),
            expected_hash: self.record.whole_prefix_sha256.clone(),
            actual_hash,
        })
    }
}

// --------------------------------------------------------------------- //
// Section rendering
// --------------------------------------------------------------------- //

/// Render one memory record as its manifest/body line. Inline content renders
/// as canonical JSON; an artifact reference renders as its address — the
/// pipeline renders what it can see, and the address is the honest stand-in
/// for bytes it cannot resolve.
fn memory_line(record: &MemoryRecord) -> String {
    let kind = serde_json::to_value(record.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{:?}", record.kind).to_lowercase());
    let scope = record.scope.as_address();
    let confidence = serde_json::to_string(&serde_json::json!(record.confidence))
        .unwrap_or_else(|_| "null".to_owned());
    let content = match &record.content {
        PayloadRef::Inline(value) => serde_json::to_string(value).unwrap_or_default(),
        PayloadRef::Artifact(reference) => format!("<artifact sha256:{}>", reference.sha256),
    };
    format!(
        "- [{}] ({}, {}, confidence {confidence}): {content}",
        record.memory_id, kind, scope
    )
}

/// Render the skills section body: one line per shortlisted skill, then the
/// tier-2 bodies of the selected ones. Deterministic given ordered entries.
fn skills_body(skills: &[SkillSectionEntry]) -> String {
    let mut out = String::from("# Skills");
    for skill in skills {
        out.push_str(&format!(
            "\n- {} (revision {}, {}): {}",
            skill.name, skill.revision, skill.content_hash, skill.metadata
        ));
    }
    for skill in skills {
        if let Some(body) = &skill.body {
            out.push_str(&format!("\n\n## Skill: {}\n{body}", skill.name));
        }
    }
    out
}

/// Render the memory section body from the packed records.
fn memory_body(records: &[MemoryRecord]) -> String {
    let mut out = String::from("# Memory");
    for record in records {
        out.push('\n');
        out.push_str(&memory_line(record));
    }
    out
}

/// The canonical compaction input rendering: the compacted prefix as one
/// compact-JSON message per line, deterministic by construction.
fn compaction_rendering(prefix: &[ChatMessage]) -> String {
    prefix
        .iter()
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The generated summary message, marked as generated ([`SUMMARY_MARKER`]).
fn summary_message(watermark: usize, summary: &str) -> ChatMessage {
    ChatMessage::system(format!("{SUMMARY_MARKER}{watermark}]\n{summary}"))
}

/// A tool schema's name (`function.name`), for the manifest's tool ids.
fn tool_name(schema: &Value) -> Result<String> {
    schema
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            invalid(
                "tool schema carries no `function.name` — the pipeline cannot journal a tool \
                 it cannot name",
            )
        })
}

/// The canonical schema one manifest contributes to the `tools` argument —
/// the same shape [`crate::tool::ToolRegistry::schemas`] renders, so the
/// governed shortlist path and the registry path hand the model
/// byte-identical schemas.
fn manifest_schema(manifest: &ToolManifest) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": manifest.name.as_str(),
            "description": manifest.description.as_str(),
            "parameters": manifest.parameters_schema.clone(),
        }
    })
}

// --------------------------------------------------------------------- //
// The pipeline
// --------------------------------------------------------------------- //

/// The deterministic context assembly pipeline: one [`ContextPolicy`], one
/// pinned [`TokenCounter`], an optional summarizer slot for compaction.
///
/// The summarizer slot is the per-mode wiring the design fixes: the
/// application wraps the summarizer exactly as it wraps the run's own model
/// (recording / replaying over the run's journal and shared `ReplaySource`,
/// or bare) and hands it in at construction — the mode switch is
/// construction-time knowledge the `ChatModel` seam cannot recover.
#[derive(Clone)]
pub struct ContextPipeline {
    policy: ContextPolicy,
    counter: Arc<dyn TokenCounter>,
    model_id: String,
    policy_pin: PolicyPin,
    summarizer: Option<Arc<dyn ChatModel>>,
}

impl ContextPipeline {
    /// A pipeline under `policy`, counting with the policy-pinned built-in
    /// estimate. Fails when the policy pins a counter other than
    /// [`ESTIMATED_COUNTER_ID`] — supply it through
    /// [`ContextPipeline::with_token_counter`].
    pub fn new(policy: ContextPolicy) -> Result<Self> {
        let margin = policy.budget.margin_percent;
        let pipeline = Self {
            policy,
            counter: Arc::new(EstimatedTokenCounter::new(margin)),
            model_id: String::new(),
            policy_pin: PolicyPin {
                name: "inline".to_owned(),
                candidate_id: None,
                content_hash: None,
            },
            summarizer: None,
        };
        pipeline.check_counter_pin()?;
        Ok(pipeline)
    }

    /// Builder-style: a provider-precise counter. Its [`TokenCounter::id`]
    /// must equal the policy's pin — a policy that pins one counter and runs
    /// another is not the pinned policy.
    pub fn with_token_counter(mut self, counter: Arc<dyn TokenCounter>) -> Result<Self> {
        self.counter = counter;
        self.check_counter_pin()?;
        Ok(self)
    }

    fn check_counter_pin(&self) -> Result<()> {
        if self.counter.id() != self.policy.tokenizer.counter {
            return Err(invalid(format!(
                "context policy pins counter `{}` but the pipeline was given `{}` — the pin \
                 is what makes assembly deterministic under a promoted policy",
                self.policy.tokenizer.counter,
                self.counter.id()
            )));
        }
        Ok(())
    }

    /// Builder-style: the model id handed to the counter.
    pub fn for_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
        self
    }

    /// Builder-style: the policy pin the manifest carries (the resolved
    /// candidate id and content hash, when the policy came through the gate).
    pub fn with_policy_pin(
        mut self,
        name: impl Into<String>,
        candidate_id: Option<String>,
        content_hash: Option<String>,
    ) -> Self {
        self.policy_pin = PolicyPin {
            name: name.into(),
            candidate_id,
            content_hash,
        };
        self
    }

    /// Builder-style: the compaction summarizer slot (per-mode wrapped by the
    /// application — see the module docs). Required when the policy declares
    /// compaction; an assembly whose trigger fires without a summarizer fails
    /// rather than silently dropping history.
    pub fn with_summarizer(mut self, summarizer: Arc<dyn ChatModel>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// The policy this pipeline assembles under.
    pub fn policy(&self) -> &ContextPolicy {
        &self.policy
    }
    /// Assemble the frozen three-tier prefix from `tiers`.
    ///
    /// Concatenates stable + context + volatile, records each tier's byte
    /// length and SHA-256 plus the whole-prefix hash. Deterministic: equal
    /// `DirectiveTiers` produce byte-identical prefixes and records.
    pub fn assemble_frozen_prefix(&self, tiers: &DirectiveTiers) -> Result<FrozenPrefix> {
        let stable_hash = crate::record::sha256_hex(tiers.stable.as_bytes());
        let context_hash = crate::record::sha256_hex(tiers.context.as_bytes());
        let volatile_hash = crate::record::sha256_hex(tiers.volatile.as_bytes());

        let text = format!("{}{}{}", tiers.stable, tiers.context, tiers.volatile);
        let whole_hash = crate::record::sha256_hex(text.as_bytes());

        let record = FrozenPrefixRecord {
            tiers: vec![
                TierRecord {
                    kind: "stable".to_owned(),
                    bytes: tiers.stable.len(),
                    sha256: stable_hash,
                },
                TierRecord {
                    kind: "context".to_owned(),
                    bytes: tiers.context.len(),
                    sha256: context_hash,
                },
                TierRecord {
                    kind: "volatile".to_owned(),
                    bytes: tiers.volatile.len(),
                    sha256: volatile_hash,
                },
            ],
            whole_prefix_sha256: whole_hash,
        };

        Ok(FrozenPrefix { text, record })
    }

    /// Count one item (a message, or a tool schema wrapped as a synthetic
    /// system message — the documented per-item accounting, uniform across
    /// counters).
    fn count_item(&self, message: &ChatMessage) -> u32 {
        self.counter
            .count(std::slice::from_ref(message), &self.model_id)
    }

    fn count_schema(&self, schema: &Value) -> u32 {
        self.count_item(&ChatMessage::system(schema.to_string()))
    }

    /// Fit a single-message text section to `budget_tokens`: exact when it
    /// fits; truncation (by the estimate's byte rule, verified against the
    /// pinned counter) or a configuration error per the overflow rule.
    fn fit_text_section(
        &self,
        kind: SectionKind,
        text: &str,
        budget_tokens: u32,
        overflow: BudgetOverflow,
    ) -> Result<(ChatMessage, u32, bool)> {
        let message = ChatMessage::system(text);
        let cost = self.count_item(&message);
        if cost <= budget_tokens {
            return Ok((message, cost, false));
        }
        match overflow {
            BudgetOverflow::Fail => Err(invalid(format!(
                "the {} section does not fit its budget: an estimated {cost} tokens against \
                 {budget_tokens} declared — a {} that does not fit is a configuration error, \
                 not a truncation",
                kind.as_str(),
                kind.as_str()
            ))),
            BudgetOverflow::Truncate => {
                // Halve the byte budget until the pinned counter agrees the
                // message fits. The estimate's byte rule is the starting
                // point; the loop keeps the rule honest for any counter.
                // Deterministic: pure function of text, budget, counter.
                let mut bytes =
                    byte_budget_for_tokens(budget_tokens, self.policy.budget.margin_percent);
                let mut fitted = truncate_to_byte_budget(text, bytes);
                for _ in 0..32 {
                    let message = ChatMessage::system(fitted.clone());
                    let cost = self.count_item(&message);
                    if cost <= budget_tokens {
                        return Ok((message, cost, true));
                    }
                    bytes /= 2;
                    fitted = truncate_to_byte_budget(text, bytes);
                }
                let message = ChatMessage::system(fitted);
                let cost = self.count_item(&message);
                if cost > budget_tokens {
                    return Err(invalid(format!(
                        "the {} section cannot be truncated into its budget of {budget_tokens} \
                         tokens — the section framing alone costs {cost}",
                        kind.as_str()
                    )));
                }
                Ok((message, cost, true))
            }
        }
    }

    /// Run the compaction decision and, when triggered, the summarization
    /// call. Returns the assembled history items: the summary message plus
    /// the verbatim tail, or the verbatim history untouched.
    async fn compact_history(
        &self,
        history: &[ChatMessage],
        compaction: &CompactionPolicy,
    ) -> Result<(Vec<ChatMessage>, Option<CompactionReport>)> {
        let history_tokens: u32 = history.iter().map(|m| self.count_item(m)).sum();
        if history_tokens <= compaction.trigger_tokens
            || history.len() <= compaction.keep_recent_messages
        {
            return Ok((history.to_vec(), None));
        }
        let summarizer = self.summarizer.as_ref().ok_or_else(|| {
            invalid(
                "the compaction trigger fired but the pipeline has no summarizer — the \
                 per-mode summarizer slot (recording / replaying over the run's journal, or \
                 bare) is construction-time wiring; a policy that declares compaction without \
                 one would silently drop history",
            )
        })?;
        let watermark = history.len() - compaction.keep_recent_messages;
        let prefix = &history[..watermark];
        let request = vec![
            ChatMessage::system(compaction.prompt.clone()),
            ChatMessage::user(compaction_rendering(prefix)),
        ];
        // The summarizer slot carries the journaling: recording mode wrapped
        // it in RecordingChatModel (parent CONTEXT_PIPELINE_PARENT), replay
        // mode in ReplayingChatModel over the run's shared ReplaySource.
        let response = summarizer.chat(&request, &[]).await?;
        let text = response.message.content.clone().unwrap_or_default();
        let mut truncated = false;
        let mut summary = text;
        // Enforce the summary bound against the pinned counter: truncate by
        // the estimate's byte rule, then verify and halve until the rendered
        // summary message (marker included) fits. Deterministic: a pure
        // function of the summary text, the bound, and the counter.
        if self.count_item(&summary_message(watermark, &summary)) > compaction.summary_max_tokens {
            truncated = true;
            let mut bytes = byte_budget_for_tokens(
                compaction.summary_max_tokens,
                self.policy.budget.margin_percent,
            );
            for _ in 0..32 {
                summary = truncate_to_byte_budget(&summary, bytes);
                if self.count_item(&summary_message(watermark, &summary))
                    <= compaction.summary_max_tokens
                {
                    break;
                }
                bytes /= 2;
            }
        }
        let message = summary_message(watermark, &summary);
        let summary_tokens = self.count_item(&message);
        // Post-check, mirroring `fit_text_section`: the halving loop bounds
        // iterations, so an unenforceable bound (the marker framing alone
        // exceeds it) must fail loud rather than report a manifest whose
        // summary_tokens silently violate the policy.
        if summary_tokens > compaction.summary_max_tokens {
            return Err(invalid(format!(
                "the compaction summary bound of {} tokens is unenforceable: the \
                 generated-marker framing alone costs an estimated {summary_tokens} — raise \
                 `summary_max_tokens`; an unenforceable bound is a configuration error, not \
                 a truncation",
                compaction.summary_max_tokens
            )));
        }
        let mut items = vec![message];
        items.extend_from_slice(&history[watermark..]);
        Ok((
            items,
            Some(CompactionReport {
                watermark,
                summary_tokens,
                summary_truncated: truncated,
            }),
        ))
    }

    /// Pack the history items (summary first when compacted) against
    /// `budget_tokens`, newest-first. The summary is never dropped: it is the
    /// only carrier of the compacted span, so a summary that does not fit is
    /// a configuration error.
    fn pack_history(
        &self,
        items: &[ChatMessage],
        budget_tokens: u32,
        compaction: &Option<CompactionReport>,
    ) -> Result<(Vec<ChatMessage>, u32, bool)> {
        let (summary, tail) = match compaction {
            Some(_report) => {
                let cost = self.count_item(&items[0]);
                if cost > budget_tokens {
                    return Err(invalid(format!(
                        "the compaction summary costs an estimated {cost} tokens against a \
                         history budget of {budget_tokens} — raise the history budget or \
                         lower `summary_max_tokens`; dropping the summary would silently lose \
                         the compacted span"
                    )));
                }
                (Some((items[0].clone(), cost)), &items[1..])
            }
            None => (None, items),
        };
        let mut used = summary.as_ref().map_or(0, |(_, cost)| *cost);
        let mut kept: Vec<ChatMessage> = Vec::new();
        let mut truncated = false;
        for message in tail.iter().rev() {
            let cost = self.count_item(message);
            if used.saturating_add(cost) > budget_tokens {
                truncated = true;
                break;
            }
            used = used.saturating_add(cost);
            kept.push(message.clone());
        }
        kept.reverse();
        let mut packed = Vec::new();
        if let Some((message, _)) = summary {
            packed.push(message);
        }
        packed.extend(kept);
        Ok((packed, used, truncated))
    }

    /// Assemble `inputs` under the pinned policy: sections in canonical
    /// order, per-section budgets, the manifest message budgeted off the top
    /// of the total. Pure over its inputs — equal inputs and equal policy
    /// produce a byte-equal assembly.
    ///
    /// `memory` is the journaled memory handle (live store or replay source,
    /// the application's per-mode wiring); required when the policy enables
    /// the memory section. The pipeline issues at most one journaled read
    /// per assembly, under the section's declared budget; total-budget
    /// absorption re-packs the already-read records outside the journaled
    /// seam, deterministically.
    pub async fn assemble(
        &self,
        inputs: &ContextInputs,
        memory: Option<&JournaledMemory>,
    ) -> Result<ContextAssembly> {
        let policy = &self.policy;

        // ---- inputs the enabled sections require ----
        if policy.identity.is_some() && inputs.identity.is_none() {
            return Err(invalid(
                "the policy enables the identity section but the inputs carry no identity \
                 text — identity is pinned at admission; a run without it is a wiring bug",
            ));
        }
        if policy.task.is_some() && inputs.task.is_none() {
            return Err(invalid(
                "the policy enables the task section but the inputs carry no task text",
            ));
        }
        if policy.memory.is_some() && memory.is_none() {
            return Err(invalid(
                "the policy enables the memory section but no JournaledMemory handle was \
                 supplied — the memory section is queried per assembly through the journaled \
                 seam, never from a raw store",
            ));
        }

        // ---- memory: the one journaled read, at the declared budget ----
        let memory_records: Vec<MemoryRecord> = match (&policy.memory, memory) {
            (Some(section), Some(handle)) => {
                let budget = ContextBudget::new(section.budget_tokens)
                    .with_margin_percent(policy.budget.margin_percent)
                    .with_overflow(section.resolved_overflow());
                let assembly = handle
                    .read(
                        &section.query,
                        &budget,
                        Some(CONTEXT_PIPELINE_PARENT.to_owned()),
                    )
                    .await?;
                assembly.records
            }
            _ => Vec::new(),
        };

        // ---- history: compaction decision (pure) + summarization (journaled
        // through the slot) ----
        let (history_items, compaction_report) = match (&policy.history, &policy.compaction) {
            (Some(_), Some(compaction)) => {
                self.compact_history(&inputs.history, compaction).await?
            }
            (Some(_), None) => (inputs.history.clone(), None),
            (None, _) => (Vec::new(), None),
        };

        // ---- pack sections; the manifest comes off the top of the total ----
        let mut history_budget = policy.history.as_ref().map_or(0, |s| s.budget_tokens);
        let mut memory_budget = policy.memory.as_ref().map_or(0, |s| s.budget_tokens);

        // Total-budget absorption: shrink the truncatable sections (history
        // first, then memory) until the manifest plus all sections fit the
        // total. Monotone — budgets only shrink — so it terminates.
        for _ in 0..16 {
            let packed = self.pack_sections(
                inputs,
                &memory_records,
                &history_items,
                &compaction_report,
                history_budget,
                memory_budget,
            )?;
            let (manifest, manifest_message) = self.render_manifest(&packed, &compaction_report)?;
            let used_total: u32 = manifest
                .manifest_tokens
                .saturating_add(manifest.sections.iter().map(|s| s.used_tokens).sum());
            if used_total <= policy.budget.max_tokens {
                return Ok(self.finish(packed, manifest, manifest_message, inputs));
            }
            let overflow = used_total - policy.budget.max_tokens;
            if policy.history.is_some() && !history_items.is_empty() && history_budget > 0 {
                history_budget = history_budget.saturating_sub(overflow);
                continue;
            }
            if policy.memory.is_some() && !memory_records.is_empty() && memory_budget > 0 {
                memory_budget = memory_budget.saturating_sub(overflow);
                continue;
            }
            return Err(invalid(format!(
                "the assembly exceeds the total budget: {used_total} estimated tokens against \
                 {} declared, with the truncatable sections already absorbed — the policy's \
                 budget split does not compose",
                policy.budget.max_tokens
            )));
        }
        Err(invalid(
            "total-budget absorption did not converge — budgets shrink monotonically, so this \
             is unreachable; if it fires, the policy is pathological",
        ))
    }

    /// One packing pass over all enabled sections at the given effective
    /// budgets for history and memory. Section outcomes only; the manifest
    /// message is rendered from them by [`ContextPipeline::render_manifest`].
    #[allow(clippy::too_many_arguments)]
    fn pack_sections(
        &self,
        inputs: &ContextInputs,
        memory_records: &[MemoryRecord],
        history_items: &[ChatMessage],
        compaction_report: &Option<CompactionReport>,
        history_budget: u32,
        memory_budget: u32,
    ) -> Result<PackedSections> {
        let policy = &self.policy;
        let mut packed = PackedSections::default();

        if let Some(section) = &policy.identity {
            let text = inputs.identity.as_deref().unwrap_or_default();
            let (message, cost, truncated) = self.fit_text_section(
                SectionKind::Identity,
                text,
                section.budget_tokens,
                section.resolved_overflow(SectionKind::Identity),
            )?;
            packed.identity = Some(PackedSection::new(
                message,
                cost,
                truncated,
                section.budget_tokens,
            ));
        }

        if let Some(section) = &policy.task {
            let text = inputs.task.as_deref().unwrap_or_default();
            let (message, cost, truncated) = self.fit_text_section(
                SectionKind::Task,
                text,
                section.budget_tokens,
                section.resolved_overflow(SectionKind::Task),
            )?;
            packed.task = Some(PackedSection::new(
                message,
                cost,
                truncated,
                section.budget_tokens,
            ));
        }

        if let Some(section) = &policy.skills {
            if !inputs.skills.is_empty() {
                let body = skills_body(&inputs.skills);
                let (message, cost, truncated) = self.fit_text_section(
                    SectionKind::Skills,
                    &body,
                    section.budget_tokens,
                    section.resolved_overflow(SectionKind::Skills),
                )?;
                let ids = inputs
                    .skills
                    .iter()
                    .map(|s| format!("{}@{}:{}", s.name, s.revision, s.content_hash))
                    .collect();
                packed.skills = Some(
                    PackedSection::new(message, cost, truncated, section.budget_tokens)
                        .with_ids(ids),
                );
            }
        }

        if let Some(section) = &policy.tools {
            if !inputs.tool_manifests.is_empty() {
                // The governed path: the pipeline runs the shortlist itself
                // under the section's selection policy, then budget-packs
                // the selected schemas. The section manifest records the
                // full selection outcome — the complete ranking and the
                // exclusions, not just the cut the budget applied.
                let features = SelectionFeatures {
                    task_tags: inputs.task_tags.clone(),
                    effect_ceiling: inputs.effect_ceiling.unwrap_or(Effect::NonIdempotent),
                    outcomes: inputs.tool_outcomes.clone(),
                };
                let shortlist = crate::tool_select::shortlist(
                    &features,
                    &inputs.tool_manifests,
                    &section.selection,
                );
                let by_name: BTreeMap<&str, &ToolManifest> = inputs
                    .tool_manifests
                    .iter()
                    .map(|m| (m.name.as_str(), m))
                    .collect();
                let mut used: u32 = 0;
                let mut kept: Vec<Value> = Vec::new();
                let mut ids: Vec<String> = Vec::new();
                let mut truncated = false;
                for ranked in &shortlist.selected {
                    let manifest = by_name.get(ranked.name.as_str()).ok_or_else(|| {
                        invalid(format!(
                            "the shortlist selected `{}`, which is not among the input \
                             manifests — selection must draw from the handed set",
                            ranked.name
                        ))
                    })?;
                    let schema = manifest_schema(manifest);
                    let cost = self.count_schema(&schema);
                    if used.saturating_add(cost) > section.budget_tokens {
                        match section.resolved_overflow() {
                            BudgetOverflow::Truncate => {
                                truncated = true;
                                break;
                            }
                            BudgetOverflow::Fail => {
                                return Err(invalid(format!(
                                    "the tools section does not fit its budget: schema `{}` \
                                     costs an estimated {cost} tokens with {used} of {} already \
                                     used — the shortlist is too wide for the declared budget",
                                    ranked.name, section.budget_tokens
                                )));
                            }
                        }
                    }
                    used = used.saturating_add(cost);
                    kept.push(schema);
                    ids.push(ranked.name.clone());
                }
                packed.tools = Some(
                    PackedSection::new(kept, used, truncated, section.budget_tokens)
                        .with_ids(ids)
                        .with_shortlist(shortlist),
                );
            } else {
                // The fallback path: pre-shortlisted schemas, budget-packed
                // as handed (selection was the tool plane's).
                let mut used: u32 = 0;
                let mut kept: Vec<Value> = Vec::new();
                let mut truncated = false;
                for schema in &inputs.tools {
                    let cost = self.count_schema(schema);
                    if used.saturating_add(cost) > section.budget_tokens {
                        match section.resolved_overflow() {
                            BudgetOverflow::Truncate => {
                                truncated = true;
                                break;
                            }
                            BudgetOverflow::Fail => {
                                let name = tool_name(schema).unwrap_or_else(|_| "<unnamed>".into());
                                return Err(invalid(format!(
                                    "the tools section does not fit its budget: schema `{name}` \
                                     costs an estimated {cost} tokens with {used} of {} already \
                                     used — the shortlist is too wide for the declared budget",
                                    section.budget_tokens
                                )));
                            }
                        }
                    }
                    used = used.saturating_add(cost);
                    kept.push(schema.clone());
                }
                let mut ids = Vec::new();
                for schema in &kept {
                    ids.push(tool_name(schema)?);
                }
                packed.tools = Some(
                    PackedSection::new(kept, used, truncated, section.budget_tokens).with_ids(ids),
                );
            }
        }

        if let Some(section) = &policy.memory {
            if !memory_records.is_empty() {
                // Re-pack the journaled read's records against the effective
                // budget, rendered-line by rendered-line, highest rank first
                // (the base rank's order is the pack order). Outside the
                // journaled seam and deterministic: the journaled MemoryRead
                // already pinned the candidate set.
                let header_cost = self.count_item(&ChatMessage::system("# Memory"));
                let mut used = header_cost;
                let mut kept: Vec<MemoryRecord> = Vec::new();
                let mut truncated = false;
                for record in memory_records {
                    let line_cost = self.count_item(&ChatMessage::system(memory_line(record)));
                    if used.saturating_add(line_cost) > memory_budget {
                        match section.resolved_overflow() {
                            BudgetOverflow::Truncate => {
                                truncated = true;
                                break;
                            }
                            BudgetOverflow::Fail => {
                                return Err(invalid(format!(
                                    "the memory section does not fit its budget: record `{}` \
                                     costs an estimated {line_cost} tokens with {used} of \
                                     {memory_budget} already used",
                                    record.memory_id
                                )));
                            }
                        }
                    }
                    used = used.saturating_add(line_cost);
                    kept.push(record.clone());
                }
                // When nothing fits — the absorbed budget cannot carry even
                // one record — the section is dropped rather than packed as
                // a bare "# Memory" header: a header-only section spends
                // tokens on no content and guarantees the next absorption
                // iteration errors. Mirrors `pack_history`'s behavior at
                // budget 0.
                if !kept.is_empty() {
                    let message = ChatMessage::system(memory_body(&kept));
                    let ids = kept.iter().map(|r| r.memory_id.clone()).collect();
                    packed.memory = Some(
                        PackedSection::new(message, used, truncated, section.budget_tokens)
                            .with_effective_budget(memory_budget)
                            .with_ids(ids),
                    );
                }
            }
        }

        if let Some(section) = &policy.history {
            if !history_items.is_empty() {
                let (messages, used, truncated) =
                    self.pack_history(history_items, history_budget, compaction_report)?;
                // Budget 0 without a compaction summary packs nothing — the
                // section is dropped rather than carried empty.
                if !messages.is_empty() {
                    packed.history = Some(
                        PackedSection::new(messages, used, truncated, section.budget_tokens)
                            .with_effective_budget(history_budget),
                    );
                }
            }
        }

        Ok(packed)
    }

    /// Render the manifest message from a packing pass, fixing the manifest's
    /// own token line by iteration (the count depends on the rendered
    /// manifest, which carries the count). The count is a non-decreasing step
    /// function of the value, so iterating upward from zero converges.
    fn render_manifest(
        &self,
        packed: &PackedSections,
        compaction: &Option<CompactionReport>,
    ) -> Result<(SectionManifest, ChatMessage)> {
        let mut manifest_tokens = 0u32;
        for _ in 0..8 {
            let manifest = SectionManifest {
                format: MANIFEST_FORMAT_VERSION.to_owned(),
                policy: self.policy_pin.clone(),
                counter: self.counter.id().to_owned(),
                budget_tokens: self.policy.budget.max_tokens,
                manifest_tokens,
                sections: packed.section_reports(compaction),
            };
            let mut message = ChatMessage::system(format!(
                "{MANIFEST_FORMAT_VERSION}\n{}",
                serde_json::to_string(&manifest)?
            ));
            message.name = Some(MANIFEST_MESSAGE_NAME.to_owned());
            let cost = self.count_item(&message);
            if cost == manifest_tokens {
                return Ok((manifest, message));
            }
            manifest_tokens = cost;
        }
        Err(invalid(
            "the manifest's token line did not converge — the count is a step function of \
             the digits, so this is unreachable; if it fires, the counter is pathological",
        ))
    }

    /// Assemble the final message list: identity, the manifest message, task,
    /// skills, memory, history — the canonical section order, with the
    /// manifest riding directly behind identity.
    fn finish(
        &self,
        packed: PackedSections,
        manifest: SectionManifest,
        manifest_message: ChatMessage,
        inputs: &ContextInputs,
    ) -> ContextAssembly {
        let mut messages = Vec::new();
        if let Some(section) = &packed.identity {
            messages.push(section.content.clone());
        }
        messages.push(manifest_message);
        if let Some(section) = &packed.task {
            messages.push(section.content.clone());
        }
        if let Some(section) = &packed.skills {
            messages.push(section.content.clone());
        }
        if let Some(section) = &packed.memory {
            messages.push(section.content.clone());
        }
        if let Some(section) = &packed.history {
            messages.extend(section.content.clone());
        }
        let tools = packed
            .tools
            .as_ref()
            .map(|section| section.content.clone())
            .unwrap_or_else(|| inputs.tools.clone());
        ContextAssembly {
            messages,
            tools,
            manifest,
        }
    }
}

/// One packed section's outcome: the content, the accounting the assembly
/// applied, the declared budget (before total absorption), the effective
/// budget the pack ran against, and the packed content's identifiers for
/// the manifest.
struct PackedSection<T> {
    content: T,
    used: u32,
    truncated: bool,
    budget: u32,
    effective_budget: u32,
    ids: Vec<String>,
    shortlist: Option<ToolShortlist>,
}

impl<T> PackedSection<T> {
    fn new(content: T, used: u32, truncated: bool, budget: u32) -> Self {
        Self {
            content,
            used,
            truncated,
            budget,
            effective_budget: budget,
            ids: Vec::new(),
            shortlist: None,
        }
    }

    fn with_ids(mut self, ids: Vec<String>) -> Self {
        self.ids = ids;
        self
    }

    /// The budget the pack actually ran against (after total-budget
    /// absorption); the manifest records it when it differs from declared.
    fn with_effective_budget(mut self, effective_budget: u32) -> Self {
        self.effective_budget = effective_budget;
        self
    }

    /// The governed shortlist outcome the tools section records.
    fn with_shortlist(mut self, shortlist: ToolShortlist) -> Self {
        self.shortlist = Some(shortlist);
        self
    }

    fn report(&self, kind: SectionKind, compaction: Option<CompactionReport>) -> SectionReport {
        SectionReport {
            kind,
            budget_tokens: self.budget,
            effective_budget_tokens: (self.effective_budget != self.budget)
                .then_some(self.effective_budget),
            used_tokens: self.used,
            truncated: self.truncated,
            ids: self.ids.clone(),
            compaction,
            shortlist: self.shortlist.clone(),
        }
    }
}

/// One packing pass's intermediate state: per-section packed content plus
/// cost, before the manifest message exists.
#[derive(Default)]
struct PackedSections {
    identity: Option<PackedSection<ChatMessage>>,
    task: Option<PackedSection<ChatMessage>>,
    skills: Option<PackedSection<ChatMessage>>,
    tools: Option<PackedSection<Vec<Value>>>,
    memory: Option<PackedSection<ChatMessage>>,
    history: Option<PackedSection<Vec<ChatMessage>>>,
}

impl PackedSections {
    /// The manifest's per-section reports, in canonical order ([`SECTION_ORDER`]).
    fn section_reports(&self, compaction: &Option<CompactionReport>) -> Vec<SectionReport> {
        let mut reports = Vec::new();
        if let Some(section) = &self.identity {
            reports.push(section.report(SectionKind::Identity, None));
        }
        if let Some(section) = &self.task {
            reports.push(section.report(SectionKind::Task, None));
        }
        if let Some(section) = &self.skills {
            reports.push(section.report(SectionKind::Skills, None));
        }
        if let Some(section) = &self.tools {
            reports.push(section.report(SectionKind::Tools, None));
        }
        if let Some(section) = &self.memory {
            reports.push(section.report(SectionKind::Memory, None));
        }
        if let Some(section) = &self.history {
            reports.push(section.report(SectionKind::History, compaction.clone()));
        }
        reports
    }
}

// --------------------------------------------------------------------- //
// The ReAct composition: AssemblingChatModel
// --------------------------------------------------------------------- //

/// A [`ChatModel`] wrapper that runs the context pipeline over every call and
/// forwards the assembly to the inner model — the pattern
/// [`crate::replay::RecordingChatModel`] establishes, composed so
/// `create_react_agent(model, tools)` (and its recording/replaying variants)
/// receive the wrapper and `react.rs` never knows.
///
/// Construction-time inputs are the pinned-at-admission half of the assembly:
/// identity, task, shortlisted skills, the governed tool manifests (with the
/// task's selection features), and the journaled memory handle. The per-call
/// half — the `messages` history — arrives through [`ChatModel::chat`]; when
/// manifests are set they supersede the per-call `tools` argument (selection
/// is the pipeline's, under the policy). The summarizer slot lives on the
/// pipeline ([`ContextPipeline::with_summarizer`]), wrapped per mode by the
/// application (the module docs' wiring recipe).
pub struct AssemblingChatModel {
    inner: Arc<dyn ChatModel>,
    pipeline: ContextPipeline,
    identity: Option<String>,
    task: Option<String>,
    skills: Vec<SkillSectionEntry>,
    tool_manifests: Vec<ToolManifest>,
    task_tags: Vec<String>,
    tool_outcomes: BTreeMap<String, ToolOutcomeStats>,
    effect_ceiling: Option<Effect>,
    frozen_prefix: Option<FrozenPrefix>,
    memory: Option<JournaledMemory>,
}

impl AssemblingChatModel {
    /// An assembling wrapper around `inner`, running `pipeline` per call.
    pub fn new(inner: Arc<dyn ChatModel>, pipeline: ContextPipeline) -> Self {
        Self {
            inner,
            pipeline,
            identity: None,
            task: None,
            skills: Vec::new(),
            tool_manifests: Vec::new(),
            task_tags: Vec::new(),
            tool_outcomes: BTreeMap::new(),
            effect_ceiling: None,
            memory: None,
            frozen_prefix: None,
        }
    }

    /// Builder-style: the pinned identity text.
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Builder-style: the current task text.
    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Builder-style: the shortlisted skills.
    pub fn with_skills(mut self, skills: Vec<SkillSectionEntry>) -> Self {
        self.skills = skills;
        self
    }

    /// Builder-style: the governed tool manifests
    /// ([`crate::tool_select::manifests_for_registry`]). When set, the
    /// pipeline runs the shortlist per call under the tools section's
    /// [`ToolSelectionPolicy`] and these manifests supersede the per-call
    /// `tools` argument.
    pub fn with_tool_manifests(mut self, manifests: Vec<ToolManifest>) -> Self {
        self.tool_manifests = manifests;
        self
    }

    /// Builder-style: the task's capability tags the shortlist scores
    /// against.
    pub fn with_task_tags(mut self, tags: Vec<String>) -> Self {
        self.task_tags = tags;
        self
    }

    /// Builder-style: the per-tool journaled outcome snapshot the shortlist
    /// scores against, keyed by tool name.
    pub fn with_tool_outcomes(mut self, outcomes: BTreeMap<String, ToolOutcomeStats>) -> Self {
        self.tool_outcomes = outcomes;
        self
    }

    /// Builder-style: the run's effect ceiling (manifests above it are
    /// excluded before scoring; default [`Effect::NonIdempotent`]).
    pub fn with_effect_ceiling(mut self, ceiling: Effect) -> Self {
        self.effect_ceiling = Some(ceiling);
        self
    }

    /// Builder-style: the journaled memory handle (per-mode wired by the
    /// application, like the summarizer slot).
    pub fn with_memory(mut self, memory: JournaledMemory) -> Self {
        self.memory = Some(memory);
        self
    }
    /// Builder-style: the frozen three-tier prefix assembled at session
    /// start. When set, the prefix is prepended as the first system message
    /// on every call and verified before dispatch.
    pub fn with_frozen_prefix(mut self, prefix: FrozenPrefix) -> Self {
        self.frozen_prefix = Some(prefix);
        self
    }

    /// The pipeline this wrapper assembles through.
    pub fn pipeline(&self) -> &ContextPipeline {
        &self.pipeline
    }

    async fn assemble(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ContextAssembly> {
        // When manifests are set the pipeline shortlists itself; the
        // per-call `tools` argument is superseded (documented on
        // `with_tool_manifests`). Otherwise the per-call schemas are the
        // fallback path.
        //
        // When a frozen prefix is present, the identity section is part of
        // the frozen system prompt and is not re-assembled per call.
        let inputs = ContextInputs {
            identity: if self.frozen_prefix.is_some() {
                None
            } else {
                self.identity.clone()
            },
            task: self.task.clone(),
            skills: self.skills.clone(),
            tools: if self.tool_manifests.is_empty() {
                tools.to_vec()
            } else {
                Vec::new()
            },
            tool_manifests: self.tool_manifests.clone(),
            task_tags: self.task_tags.clone(),
            tool_outcomes: self.tool_outcomes.clone(),
            effect_ceiling: self.effect_ceiling,
            history: messages.to_vec(),
        };
        let mut assembly = self
            .pipeline
            .assemble(&inputs, self.memory.as_ref())
            .await?;

        // Prepend the frozen prefix as the first system message. The
        // pipeline omits identity when frozen_prefix is set, so the
        // manifest rides at index 0; we insert the prefix before it.
        if let Some(prefix) = &self.frozen_prefix {
            assembly
                .messages
                .insert(0, ChatMessage::system(&prefix.text));
        }

        Ok(assembly)
    }
}

#[async_trait]
impl ChatModel for AssemblingChatModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        let assembly = self.assemble(messages, tools).await?;

        // Pre-dispatch frozen-prefix verification (EP-02-S09 AC 2).
        if let Some(prefix) = &self.frozen_prefix {
            let actual_prefix = assembly
                .messages
                .first()
                .and_then(|m| m.content.as_deref())
                .unwrap_or("");
            prefix.verify(actual_prefix)?;
        }

        self.inner.chat(&assembly.messages, &assembly.tools).await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> Result<ChatResponse> {
        let assembly = self.assemble(messages, tools).await?;

        // Pre-dispatch frozen-prefix verification (EP-02-S09 AC 2).
        if let Some(prefix) = &self.frozen_prefix {
            let actual_prefix = assembly
                .messages
                .first()
                .and_then(|m| m.content.as_deref())
                .unwrap_or("");
            prefix.verify(actual_prefix)?;
        }

        self.inner
            .chat_stream(&assembly.messages, &assembly.tools, on_token)
            .await
    }

    fn effect(&self) -> crate::record::Effect {
        self.inner.effect()
    }

    fn pricing(&self) -> Option<crate::llm::ModelPricing> {
        self.inner.pricing()
    }
}
