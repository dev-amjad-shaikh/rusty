//! The post-turn background review fork (EP-07-S04): a `side`-stamped
//! session that replays each substantive turn off the user's latency path
//! and persists what it taught, without ever contaminating the live
//! transcript.
//!
//! The module is the fork's contract vocabulary; the scheduler that fires
//! it at turn end is the server's wiring. One plan, one dispatch surface,
//! one ledger write path:
//!
//! 1. **Substance** ([`assess_substance`]): a turn earns a review when it
//!    ran a tool, its assistant output crossed the configured length
//!    threshold, or the user invoked an explicit learn command. Anything
//!    else is churn the fork never spends on.
//! 2. **The replay plan** ([`plan_review`]): the fork inherits the parent's
//!    provider binding and frozen prompt prefix — the parent's
//!    [`RunManifest::prompts`] digests pin the prefix verbatim, and
//!    [`prompt_prefix_hash`] folds them into the one hash a later audit
//!    joins on. When the review model is the parent's model the fork
//!    replays the turn's transcript projection
//!    ([`ReplayMaterial::Projection`]) so the provider serves it from warm
//!    cache; routed to a cheaper auxiliary model whose cache key
//!    necessarily differs, it replays a compact digest instead
//!    ([`ReplayMaterial::Digest`], built by [`compact_digest`] — a
//!    deterministic projection, never a model call).
//! 3. **Confinement** ([`fork_dispatch_registry`]): the fork's toolset is
//!    composition, not prompt instruction — the parent's registry is
//!    narrowed to the review allowlist (memory tools from EP-06-S03 plus
//!    the session's skill tools) by construction, and every survivor is
//!    wrapped in [`ReviewBoundaryTool`], the guard-layer backup that
//!    refuses any out-of-allowlist or post-cancellation call before the
//!    tool launches (the same registration recipe as
//!    [`crate::tool_select::ValidatingTool::wrap_registry`] and
//!    [`crate::skills::SkillGateTool::wrap_registry`]). The fork cannot
//!    send messages, schedule work, or escalate: those tools are absent
//!    from its registry and refused by the boundary.
//! 4. **Cancellation** (the same boundary): when a new live turn starts on
//!    the parent session, the scheduler cancels the fork's
//!    `CancellationToken`; the boundary refuses the fork's next tool
//!    launch, and mutations the fork already committed remain valid ledger
//!    entries — cancellation never corrupts the ledger, uncommitted work
//!    is simply lost.
//! 5. **Provenance** ([`ReviewForkPlan::stamp`]): every provider call the
//!    fork makes carries `traffic: side` and component attribution
//!    `review_fork` with the parent thread as sub-id, so fork traffic is
//!    structurally excluded from memory promotion and the training
//!    main-line. The stamp rides the run via
//!    [`crate::executor::RunConfig::with_turn_stamp`]; the ReAct
//!    dispatcher re-attributes the per-call boundary and preserves the
//!    named component (EP-07-S12).
//! 6. **The ledger write** ([`commit_skill_write`]): a concluded review
//!    lands its skill mutation through the ordinary registry with fork
//!    attribution, obeying the patch-before-create order (EP-07-S03): a
//!    create over an existing name and a patch against thin air are both
//!    typed refusals, a create demands its `CreateNew` justification, and
//!    a fresh write enters `Trial` — unpromoted by construction, no gate
//!    has run.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::Result;
use crate::llm::{ChatMessage, Role};
use crate::memory_tools::maintenance_names;
use crate::record::{RunManifest, sha256_hex};
use crate::skill::{SkillPackage, SkillPromotionStatus, SkillRegistry, SkillSource};
use crate::skill_editorial::{EditorialProvenance, PatchPreference};
use crate::tool::{Tool, ToolRegistry};
use crate::tool_select::{ERROR_PREFIX, ToolPredicate, filtered};
use rusty_api::{ComponentAttribution, TrafficClass, TurnBoundary, TurnStamp};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The component attribution every fork provider call carries
/// (`contracts:turn-stamp`; named in the stamp vocabulary itself).
pub const REVIEW_FORK_COMPONENT: &str = "review_fork";

/// The default assistant-output length above which a turn counts as
/// substantive (AC1's second leg).
pub const DEFAULT_SUBSTANTIVE_ASSISTANT_CHARS: usize = 2_000;

/// The per-message content budget in a compact digest (AC2).
pub const DEFAULT_DIGEST_MESSAGE_CHARS: usize = 120;

/// The structured refusal kind the boundary returns, parsed from the
/// `ERROR: {"kind": …}` envelope beside `skill_tool_gate`.
pub const REVIEW_BOUNDARY_KIND: &str = "review_fork_boundary";

// ---------------------------------------------------------------------------
// AC1: the substance rule
// ---------------------------------------------------------------------------

/// Why a completed turn earned a review fork (AC1). The order is the
/// precedence the assessment reports in: an explicit ask outranks
/// behavioral heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Substance {
    /// The user invoked an explicit learn command.
    ExplicitLearn,
    /// The turn ran at least one tool call.
    ToolCalls,
    /// The turn's assistant output exceeded the configured length threshold.
    LongAssistantOutput,
}

impl Substance {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitLearn => "explicit_learn",
            Self::ToolCalls => "tool_calls",
            Self::LongAssistantOutput => "long_assistant_output",
        }
    }
}

/// The substance rule's one dial: the assistant-output length threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubstanceRule {
    /// Assistant text at or above this many characters makes the turn
    /// substantive.
    pub assistant_chars: usize,
}

impl Default for SubstanceRule {
    fn default() -> Self {
        Self {
            assistant_chars: DEFAULT_SUBSTANTIVE_ASSISTANT_CHARS,
        }
    }
}

/// Assess a completed turn's transcript projection: `Some(substance)` when
/// the turn earned a review fork, `None` when the turn is churn the fork
/// never spends on.
pub fn assess_substance(
    projection: &[ChatMessage],
    learn_invoked: bool,
    rule: &SubstanceRule,
) -> Option<Substance> {
    if learn_invoked {
        return Some(Substance::ExplicitLearn);
    }
    if projection.iter().any(ChatMessage::has_tool_calls) {
        return Some(Substance::ToolCalls);
    }
    let assistant_chars: usize = projection
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter_map(|message| message.content.as_deref())
        .map(str::len)
        .sum();
    (assistant_chars >= rule.assistant_chars).then_some(Substance::LongAssistantOutput)
}

// ---------------------------------------------------------------------------
// AC1/AC2: the replay plan
// ---------------------------------------------------------------------------

/// What the scheduler knows about the completed parent turn: identity, the
/// pinned model and manifest, the turn's transcript projection, and whether
/// the user asked to learn explicitly.
#[derive(Debug, Clone)]
pub struct ParentTurn {
    /// The parent session's thread id.
    pub thread_id: String,
    /// The parent session's stamp identity.
    pub session_id: Uuid,
    /// The completed turn's id.
    pub turn_id: Uuid,
    /// The parent's pinned model (`RunManifest::model` — provider-precise,
    /// never an alias).
    pub model: String,
    /// The parent's run manifest; `prompts` pins the frozen prompt prefix
    /// the fork inherits.
    pub manifest: RunManifest,
    /// The turn's transcript projection (EP-01-S03's `derive_messages`
    /// output for the turn), user turn through final assistant message.
    pub projection: Vec<ChatMessage>,
    /// Whether the user invoked an explicit learn command this turn.
    pub learn_invoked: bool,
}

/// What the fork replays (AC1/AC2).
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayMaterial {
    /// The full transcript projection — the review model is the parent's
    /// model, so the provider serves the replay from warm cache.
    Projection(Vec<ChatMessage>),
    /// A compact digest of the turn — the review routed to an auxiliary
    /// model whose cache key differs, so warm-prefix replay buys nothing.
    Digest(Vec<ChatMessage>),
}

impl ReplayMaterial {
    /// The replayed messages, whichever material the plan chose.
    pub fn messages(&self) -> &[ChatMessage] {
        match self {
            Self::Projection(messages) | Self::Digest(messages) => messages,
        }
    }

    /// `true` when the replay rides the parent's warm cache prefix.
    pub fn is_warm(&self) -> bool {
        matches!(self, Self::Projection(_))
    }
}

/// The fork's charter: everything the background session needs, decided
/// before it starts. Data, not a runner — the consumer builds the graph
/// over [`fork_dispatch_registry`] and drives it with the executor.
#[derive(Debug, Clone)]
pub struct ReviewForkPlan {
    /// The fork session's thread id (`{parent}/review/{turn}`), namespaced
    /// under the parent so an audit walks from fork to turn directly.
    pub fork_thread_id: String,
    /// The parent thread the fork reviews.
    pub parent_thread_id: String,
    /// Why the turn earned the fork.
    pub substance: Substance,
    /// The review model: the parent's pinned model on the warm path, the
    /// configured auxiliary otherwise.
    pub model: String,
    /// The one hash naming the parent's frozen prompt prefix
    /// ([`prompt_prefix_hash`] over the parent's manifest).
    pub prompt_prefix_hash: String,
    /// The fork run's manifest: the parent's prompt digests carried
    /// verbatim (the frozen prefix is inherited, not re-assembled), the
    /// model pinned to the review model.
    pub manifest: RunManifest,
    /// The replay material.
    pub replay: ReplayMaterial,
    /// The stamp every fork provider call carries: `traffic: side`,
    /// component `review_fork`, the parent thread as sub-id.
    pub stamp: TurnStamp,
}

/// The fork's configuration: the substance rule and the optional auxiliary
/// review model.
#[derive(Debug, Clone, Default)]
pub struct ReviewForkConfig {
    /// The substance rule.
    pub substance: SubstanceRule,
    /// The auxiliary review model. `None` reviews with the parent's pinned
    /// model — the warm-cache path.
    pub review_model: Option<String>,
    /// The per-message content budget for digests.
    pub digest_message_chars: Option<usize>,
}

impl ReviewForkConfig {
    /// A config that reviews on the parent's model with default dials.
    pub fn warm() -> Self {
        Self::default()
    }

    /// Route reviews to an auxiliary model (the digest path).
    pub fn with_review_model(mut self, model: impl Into<String>) -> Self {
        self.review_model = Some(model.into());
        self
    }
}

/// The one hash naming a manifest's frozen prompt prefix: SHA-256 over the
/// canonical prompt-digest map. `RunManifest::prompts` is a `BTreeMap`, so
/// equal prefixes hash equal regardless of insertion order.
pub fn prompt_prefix_hash(manifest: &RunManifest) -> String {
    let canonical =
        serde_json::to_vec(&manifest.prompts).expect("serializing a string map cannot fail");
    sha256_hex(&canonical)
}

/// The compact digest (AC2): one system frame plus one line per turn
/// message — role, tool-call names, content truncated to the budget. Pure
/// projection: a digest must never itself cost a model call.
pub fn compact_digest(projection: &[ChatMessage], message_chars: usize) -> Vec<ChatMessage> {
    let mut lines = Vec::with_capacity(projection.len());
    for message in projection {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let mut line = role.to_owned();
        if !message.tool_calls.is_empty() {
            let names: Vec<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            line.push_str(&format!(" (calls: {})", names.join(", ")));
        }
        if let Some(content) = message.content.as_deref() {
            let truncated: String = content.chars().take(message_chars).collect();
            line.push_str(": ");
            line.push_str(&truncated);
            if content.chars().count() > message_chars {
                line.push('…');
            }
        }
        lines.push(line);
    }
    vec![
        ChatMessage::system(
            "You are the post-turn review fork. The following is a compact digest of one \
             completed turn; persist what it teaches through your memory and skill tools. \
             You cannot message the user, schedule work, or escalate.",
        ),
        ChatMessage::user(lines.join("\n")),
    ]
}

/// Decide whether a completed turn earns a review fork, and plan it.
/// `None` when the turn is not substantive — no fork, no spend.
pub fn plan_review(parent: &ParentTurn, config: &ReviewForkConfig) -> Option<ReviewForkPlan> {
    let substance = assess_substance(&parent.projection, parent.learn_invoked, &config.substance)?;
    let (model, replay) = match &config.review_model {
        // The warm path: the review model is the parent's model, so the
        // full projection replays off the provider's cache.
        None => (
            parent.model.clone(),
            ReplayMaterial::Projection(parent.projection.clone()),
        ),
        // The auxiliary path: a different model's cache key differs by
        // construction, so the fork replays a compact digest.
        Some(auxiliary) if *auxiliary == parent.model => (
            parent.model.clone(),
            ReplayMaterial::Projection(parent.projection.clone()),
        ),
        Some(auxiliary) => (
            auxiliary.clone(),
            ReplayMaterial::Digest(compact_digest(
                &parent.projection,
                config
                    .digest_message_chars
                    .unwrap_or(DEFAULT_DIGEST_MESSAGE_CHARS),
            )),
        ),
    };
    let mut manifest = parent.manifest.clone();
    manifest.model = Some(model.clone());
    Some(ReviewForkPlan {
        fork_thread_id: format!("{}/review/{}", parent.thread_id, parent.turn_id),
        parent_thread_id: parent.thread_id.clone(),
        substance,
        model,
        prompt_prefix_hash: prompt_prefix_hash(&parent.manifest),
        manifest,
        replay,
        stamp: TurnStamp {
            session_id: parent.session_id,
            traffic: TrafficClass::Side,
            turn_id: parent.turn_id,
            turn_boundary: TurnBoundary::Start,
            issued_by: ComponentAttribution {
                component: REVIEW_FORK_COMPONENT.to_owned(),
                sub_id: Some(parent.thread_id.clone()),
            },
        },
    })
}

// ---------------------------------------------------------------------------
// AC3/AC4: confinement and cancellation — the fork's dispatch surface
// ---------------------------------------------------------------------------

/// The review allowlist: the memory maintenance tools (EP-06-S03) plus the
/// session's skill tools — everything the fork may dispatch, and nothing
/// else. Sorted and deduplicated.
pub fn review_tool_allowlist(skill_tools: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names = maintenance_names();
    names.extend(skill_tools);
    names.sort();
    names.dedup();
    names
}

/// The confinement half: the parent's registry narrowed to the review
/// allowlist by construction ([`filtered`], EP-05-S04), so message sends,
/// scheduling, and escalation are absent from the fork's toolset — not
/// instructed away, unreachable.
pub fn confined_review_registry(
    parent: &ToolRegistry,
    skill_tools: impl IntoIterator<Item = String>,
) -> ToolRegistry {
    filtered(
        ToolPredicate::ByName {
            names: review_tool_allowlist(skill_tools),
        },
        parent,
    )
}

/// The guard-layer backup over the confined registry: a wrapping tool that
/// refuses out-of-allowlist and post-cancellation calls *before* the inner
/// tool launches (AC3's `pre_execute` enforcement mapped onto the tool
/// contract; the registration recipe beside
/// [`crate::tool_select::ValidatingTool::wrap_registry`]). The refusal is
/// model-visible — the fork is a model session, and the denial must be
/// legible to it.
pub struct ReviewBoundaryTool {
    inner: Arc<dyn Tool>,
    allowed: Arc<BTreeSet<String>>,
    cancellation: CancellationToken,
}

impl fmt::Debug for ReviewBoundaryTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReviewBoundaryTool")
            .field("tool", &self.inner.name())
            .finish()
    }
}

impl ReviewBoundaryTool {
    /// Wrap one tool.
    pub fn new(
        inner: Arc<dyn Tool>,
        allowed: Arc<BTreeSet<String>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            inner,
            allowed,
            cancellation,
        }
    }

    /// Clone `registry` with every tool boundary-wrapped over the same
    /// allowlist and cancellation token.
    pub fn wrap_registry(
        registry: &ToolRegistry,
        allowed: Arc<BTreeSet<String>>,
        cancellation: CancellationToken,
    ) -> ToolRegistry {
        let mut names: Vec<&str> = registry.names().collect();
        names.sort_unstable();
        let mut wrapped = ToolRegistry::new();
        for name in names {
            let tool = registry.get(name).expect("name came from the registry");
            wrapped.register_shared(Arc::new(Self::new(
                tool,
                Arc::clone(&allowed),
                cancellation.clone(),
            )));
        }
        wrapped
    }

    /// The refusal the boundary returns, in the codebase's structured
    /// envelope (`ERROR: {"kind": …}`), key order pinned.
    fn refusal(&self, reason: &str) -> String {
        let body = json!({
            "kind": REVIEW_BOUNDARY_KIND,
            "tool": self.inner.name(),
            "reason": reason,
        });
        let compact = serde_json::to_string(&body)
            .expect("serializing a boundary refusal cannot fail: it is a plain JSON value");
        format!("{ERROR_PREFIX}{compact}")
    }
}

#[async_trait::async_trait]
impl Tool for ReviewBoundaryTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn effect(&self) -> crate::record::Effect {
        self.inner.effect()
    }

    fn effect_kind(&self) -> &str {
        self.inner.effect_kind()
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.inner.idempotency_key(args)
    }

    fn effect_request(&self, call: &crate::llm::ToolCall) -> crate::effects::EffectRequest {
        self.inner.effect_request(call)
    }

    async fn call(&self, args: Value) -> Result<Value> {
        // Cancellation first: a cancelled fork launches nothing further,
        // allowlisted or not (AC4).
        if self.cancellation.is_cancelled() {
            return Ok(Value::String(self.refusal(
                "the review fork was cancelled: live traffic resumed on the parent session",
            )));
        }
        if !self.allowed.contains(self.inner.name()) {
            return Ok(Value::String(self.refusal(
                "outside the review fork's confinement: only memory and skill tools pass",
            )));
        }
        self.inner.call(args).await
    }
}

/// Assemble the fork's whole dispatch surface: the parent's registry
/// narrowed to the review allowlist by construction, each survivor wrapped
/// in the cancellation boundary. Drive it with the prebuilt ReAct agent
/// under `RunConfig::with_turn_stamp(plan.stamp)` /
/// `with_manifest(plan.manifest)` / `with_cancellation(token)`.
pub fn fork_dispatch_registry(
    parent: &ToolRegistry,
    skill_tools: impl IntoIterator<Item = String>,
    cancellation: CancellationToken,
) -> ToolRegistry {
    let allowlist = review_tool_allowlist(skill_tools);
    let confined = filtered(
        ToolPredicate::ByName {
            names: allowlist.clone(),
        },
        parent,
    );
    ReviewBoundaryTool::wrap_registry(
        &confined,
        Arc::new(allowlist.into_iter().collect()),
        cancellation,
    )
}

// ---------------------------------------------------------------------------
// AC6: the ledger write
// ---------------------------------------------------------------------------

/// A concluded review's skill mutation: the package to land and the
/// editorial decision that licenses it (EP-07-S03). The skill's name comes
/// from the package itself.
#[derive(Debug, Clone)]
pub struct ForkSkillWrite {
    /// The candidate package.
    pub package: SkillPackage,
    /// The rung the review landed on; `CreateNew` carries its
    /// justification, patch rungs carry none.
    pub editorial: EditorialProvenance,
}

/// The evidence a landed fork write returns.
#[derive(Debug, Clone, PartialEq)]
pub struct ForkSkillCommit {
    /// The skill's name.
    pub name: String,
    /// The revision the write landed as (1 for a create).
    pub revision: u64,
    /// The content address of the landed version.
    pub content_hash: String,
    /// The rung the write obeyed.
    pub rung: PatchPreference,
    /// Always `Trial` for a fresh write: unpromoted by construction, no
    /// gate has run.
    pub status: SkillPromotionStatus,
    /// The fork's attribution on the registration.
    pub author: String,
}

/// A fork write refusal, each variant naming the violated order.
#[derive(Debug)]
pub enum ForkWriteError {
    /// Patch-before-create: the name already exists — patch it, do not
    /// create beside it (EP-07-S03).
    CreateOverExisting {
        /// The existing skill's name.
        name: String,
    },
    /// A patch rung against a name nothing registers: there is nothing to
    /// patch.
    PatchWithoutTarget {
        /// The missing skill's name.
        name: String,
    },
    /// The editorial provenance failed its own invariant.
    Editorial(crate::skill_editorial::EditorialError),
    /// The registry refused the write.
    Registry(crate::skill::SkillError),
}

impl fmt::Display for ForkWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateOverExisting { name } => write!(
                f,
                "patch-before-create: `{name}` already exists — patch the loaded skill, do not \
                 create beside it"
            ),
            Self::PatchWithoutTarget { name } => {
                write!(
                    f,
                    "patch rung against `{name}`, but nothing registers that name"
                )
            }
            Self::Editorial(error) => write!(f, "editorial provenance invalid: {error}"),
            Self::Registry(error) => write!(f, "registry refused the write: {error}"),
        }
    }
}

impl std::error::Error for ForkWriteError {}

/// Land a concluded review's skill mutation through the ordinary registry
/// with fork attribution (AC6). The patch-before-create order is enforced
/// at the write point: the rung and the registry state must agree, a
/// `CreateNew` carries its justification (structurally mandatory, outside
/// the content address), and the write enters `Trial` — a fresh
/// registration is unpromoted by construction.
pub fn commit_skill_write(
    registry: &mut SkillRegistry,
    write: ForkSkillWrite,
    fork_id: &str,
) -> std::result::Result<ForkSkillCommit, ForkWriteError> {
    write
        .editorial
        .validate()
        .map_err(ForkWriteError::Editorial)?;
    let name = write.package.name().to_owned();
    let exists = registry.contains(&name);
    match (write.editorial.rung, exists) {
        (PatchPreference::CreateNew, true) => {
            return Err(ForkWriteError::CreateOverExisting { name });
        }
        (PatchPreference::CreateNew, false) => {}
        (_, false) => return Err(ForkWriteError::PatchWithoutTarget { name }),
        (_, true) => {}
    }
    let registration = registry
        .register(write.package, SkillSource::Learned, fork_id)
        .map_err(ForkWriteError::Registry)?;
    Ok(ForkSkillCommit {
        name,
        revision: registration.version.revision(),
        content_hash: registration.version.content_hash().to_owned(),
        rung: write.editorial.rung,
        status: SkillPromotionStatus::Trial,
        author: fork_id.to_owned(),
    })
}
