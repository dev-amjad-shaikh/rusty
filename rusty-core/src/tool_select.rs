//! Tool selection and validated calling (R0.13 agent core, wave 1b).
//!
//! A layer strictly above [`crate::tool::ToolRegistry`]: the registry stays
//! the executable truth; this module decides what the model is *shown* and
//! refuses malformed calls before dispatch. Three pieces:
//!
//! - **[`ToolManifest`]** — selection metadata for one tool: the derived half
//!   (name, description, schema, effect class) comes from the shipped
//!   [`crate::tool::ToolCapability`], never separately authored; the governed
//!   half is the operator overlay ([`ToolSelectionOverlay`]): capability
//!   tags, a when-to-use note, a cost/latency class, `parallel_safe` /
//!   `batchable` flags, and prerequisite tools. The overlay is the shape the
//!   `selection` member of a `tool_contract` candidate carries (wave 3).
//! - **Ranked shortlisting** ([`select`], [`shortlist`]) — structural scoring
//!   (tag overlap, prerequisite closure, effect-class ceiling, journaled
//!   outcome statistics) returning a deterministic top-k plus the full
//!   ranking, which the context pipeline records in its section manifest.
//!   No embeddings, per the release's vector decision.
//! - **[`ValidatingTool`]** — a [`crate::tool::Tool`] wrapper validating
//!   arguments against the tool's JSON schema *before* dispatch. On failure
//!   it returns the structured refusal payload — the reserved
//!   `ERROR: {"kind":"argument_validation","violations":[…]}` shape — instead
//!   of calling the tool. That payload is THE structured contract the
//!   call-outcome roll-up parses (design N10); every other failure is an
//!   opaque error string. No silent coercion, ever.
//!
//! # ReAct consumption (composition, no `react.rs` edits)
//!
//! 1. **Construction-time narrowing** — shortlist at admission, then build
//!    the run's registry via the shipped `ToolRegistry::restricted_to`; the
//!    executor's `TOOL_ALLOWLIST_KEY` mechanism narrows the rest per run.
//! 2. **Model wrapper** — the context pipeline rides an assembling
//!    `ChatModel` wrapper (`context.rs`'s seam, the `RecordingChatModel`
//!    pattern); `create_react_agent` receives the wrapper and never knows.
//! 3. **Tool wrappers** — wrap tools in [`ValidatingTool`] before
//!    registration ([`ValidatingTool::wrap_registry`]); the registry holds
//!    `Arc<dyn Tool>`, so wrapped tools are indistinguishable from native
//!    ones. A wrapper's refusal is a journaled `ToolCall` — attributable,
//!    replayable.
//! 4. **Middleware** — the shipped `MiddlewareChain` tool hooks carry ONLY
//!    the unjournaled half (observe, rewrite, reject without an evidence
//!    trail): a middleware rejection never reaches dispatch, so nothing
//!    records it (S4). Every enforcement an auditor must see — validation,
//!    argument gating — is a `Tool` wrapper, never middleware.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::effects::EffectRequest;
use crate::error::{Result, RustyError};
use crate::llm::ToolCall;
use crate::record::Effect;
use crate::tool::{Tool, ToolCapability, ToolRegistry};

/// The reserved `kind` of [`ValidatingTool`]'s structured refusal payload:
/// `ERROR: {"kind":"argument_validation","violations":[…]}`. The outcome
/// roll-up keys on this exact string; do not rename without a wave.
pub const ARGUMENT_VALIDATION_KIND: &str = "argument_validation";

/// The failure-isolation channel's reserved prefix. A refusal payload is
/// this prefix followed by the compact JSON body.
pub const ERROR_PREFIX: &str = "ERROR: ";

/// Registry size at which ranked shortlisting engages by default (design,
/// open question 5: the point where schema tokens measurably crowd the
/// context budget). Declared in the policy, never hard-coded into selection.
pub const DEFAULT_SHORTLIST_CUTOFF: usize = 20;

/// Maximum length of one overlay tag or one `when_to_use` note, in bytes.
pub const MAX_TAG_BYTES: usize = 64;
/// Bound on the `when_to_use` note; same discipline as tool descriptions.
pub const MAX_WHEN_TO_USE_BYTES: usize = 1024;

/// Relative cost/latency class of a tool call, declared by the overlay.
/// Ordering is the ladder `Low < Medium < High`; selection may narrow
/// against a run's budgets but never widen past them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// Local or near-free calls (compute, cache reads).
    Low,
    /// Ordinary network/tool calls. The derived default.
    Medium,
    /// Expensive or slow calls (paid APIs, long-running operations).
    High,
}

/// The operator-governed selection overlay for one tool.
///
/// This is the wire shape a `tool_contract` candidate's `selection` member
/// carries (wave 3): every member is optional, and unset members stay
/// absent from the wire, so old records keep deserializing (the established
/// evolution rule). Validation is fail-closed on shape; membership of
/// `prerequisites` is checked at manifest-assembly time, where the registry
/// is known.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectionOverlay {
    /// Structural capability tags matched against the task section.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// When-to-use note shown to selectors and reviewers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Declared cost/latency class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_class: Option<CostClass>,
    /// `true` when concurrent calls of this tool are safe. Overrides the
    /// derived default (`false` for write-class effects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_safe: Option<bool>,
    /// `true` when N calls of this tool collapse into one batch call —
    /// a tool-declared capability honored by the dispatching node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batchable: Option<bool>,
    /// Tools that must be callable for this tool to be useful; selection
    /// includes the transitive closure of a selected tool's prerequisites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
}

impl ToolSelectionOverlay {
    /// Fail-closed shape validation: tags and prerequisites are trimmed,
    /// non-empty, bounded, and duplicate-free; prerequisites must be
    /// well-formed tool names; notes are bounded and control-free.
    pub fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for tag in &self.tags {
            validate_label("tag", tag, MAX_TAG_BYTES)?;
            if !seen.insert(tag.as_str()) {
                return Err(RustyError::Tool(format!(
                    "tool selection overlay has duplicate tag `{tag}`"
                )));
            }
        }
        if let Some(note) = &self.when_to_use {
            if note.is_empty()
                || note != note.trim()
                || note.len() > MAX_WHEN_TO_USE_BYTES
                || note.chars().any(char::is_control)
            {
                return Err(RustyError::Tool(format!(
                    "when_to_use note must be non-empty, trimmed, control-free, and at most {MAX_WHEN_TO_USE_BYTES} bytes"
                )));
            }
        }
        let mut seen = BTreeSet::new();
        for prerequisite in &self.prerequisites {
            validate_tool_name(prerequisite)?;
            if !seen.insert(prerequisite.as_str()) {
                return Err(RustyError::Tool(format!(
                    "tool selection overlay lists prerequisite `{prerequisite}` twice"
                )));
            }
        }
        Ok(())
    }
}

/// The tool-name half of the shipped tool contract rules
/// (`tool.rs::validate_tool_contract` restated for names alone — the
/// overlay names tools it does not define).
fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(RustyError::Tool(format!(
            "tool name `{name}` must use 1..=128 ASCII letters, digits, `.`, `_`, `:`, or `-`"
        )));
    }
    Ok(())
}

/// A bounded overlay label: non-empty, trimmed, control-free.
fn validate_label(what: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return Err(RustyError::Tool(format!(
            "{what} `{value}` must be non-empty, trimmed, control-free, and at most {max} bytes"
        )));
    }
    Ok(())
}

/// Selection metadata for one tool: the derived executable contract plus
/// the resolved operator overlay. Assembled per assembly from the registry
/// and the promoted overlays; never stored — the registry and the
/// candidate pipeline are the stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    /// Stable tool name (derived from the tool).
    pub name: String,
    /// Model-facing description (derived).
    pub description: String,
    /// JSON Schema the tool accepts (derived).
    pub parameters_schema: Value,
    /// Runtime effect class (derived).
    pub effect: Effect,
    /// Capability tags (overlay; empty when undeclared).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// When-to-use note (overlay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Cost/latency class (overlay or [`CostClass::Medium`]).
    pub cost_class: CostClass,
    /// Concurrent calls are safe (overlay, or derived: `false` for
    /// write-class effects — `Compensatable`/`NonIdempotent`, the shipped
    /// conservatism).
    pub parallel_safe: bool,
    /// N calls collapse into one (overlay; default `false`).
    pub batchable: bool,
    /// Prerequisite tools (overlay; empty when undeclared).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
}

impl ToolManifest {
    /// Derive a manifest from the executable contract, with the floor
    /// overlay (no tags, no note, `Medium` cost, derived `parallel_safe`,
    /// not batchable, no prerequisites).
    pub fn from_capability(capability: &ToolCapability) -> Self {
        Self {
            name: capability.name.clone(),
            description: capability.description.clone(),
            parameters_schema: capability.parameters_schema.clone(),
            effect: capability.effect,
            tags: Vec::new(),
            when_to_use: None,
            cost_class: CostClass::Medium,
            parallel_safe: capability.effect.is_freely_repeatable(),
            batchable: false,
            prerequisites: Vec::new(),
        }
    }

    /// Apply a validated overlay over the derived defaults.
    pub fn with_overlay(mut self, overlay: &ToolSelectionOverlay) -> Result<Self> {
        overlay.validate()?;
        self.tags = overlay.tags.clone();
        self.when_to_use = overlay.when_to_use.clone();
        if let Some(cost_class) = overlay.cost_class {
            self.cost_class = cost_class;
        }
        if let Some(parallel_safe) = overlay.parallel_safe {
            self.parallel_safe = parallel_safe;
        }
        if let Some(batchable) = overlay.batchable {
            self.batchable = batchable;
        }
        self.prerequisites = overlay.prerequisites.clone();
        Ok(self)
    }
}

/// Derive manifests for every tool in `registry`, sorted by stable tool
/// name, applying `overlays` by tool name. An overlay naming an
/// unregistered tool fails closed — a configuration typo cannot silently
/// drop governed metadata (the `restricted_to` rule).
pub fn manifests_for_registry(
    registry: &ToolRegistry,
    overlays: &BTreeMap<String, ToolSelectionOverlay>,
) -> Result<Vec<ToolManifest>> {
    let capabilities = registry.capabilities()?;
    for name in overlays.keys() {
        if !registry.contains(name) {
            return Err(RustyError::Tool(format!(
                "tool selection overlay names `{name}`, which is not registered"
            )));
        }
    }
    capabilities
        .iter()
        .map(|capability| {
            let manifest = ToolManifest::from_capability(capability);
            match overlays.get(&capability.name) {
                Some(overlay) => manifest.with_overlay(overlay),
                None => Ok(manifest),
            }
        })
        .collect()
}

/// Per-tool journaled outcome statistics, as the wave-3 roll-up derives
/// them from `ToolCall` events. Wave 1 consumes the snapshot as one rank
/// input; a tool with no recorded history contributes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcomeStats {
    /// Total journaled calls.
    pub calls: u64,
    /// Calls whose outcome was a success.
    pub successes: u64,
    /// Calls refused by [`ValidatingTool`] (parsed from the structured
    /// contract).
    pub validation_failures: u64,
}

impl ToolOutcomeStats {
    /// Success rate in basis points (0..=10000); `None` with no evidence.
    pub fn success_bps(&self) -> Option<u32> {
        if self.calls == 0 {
            return None;
        }
        Some((self.successes.saturating_mul(10_000) / self.calls) as u32)
    }
}

/// The structural features one assembly scores manifests against. Built by
/// the context pipeline from its own declared sections and journaled with
/// the assembly — selection is a pure function of these features, the
/// manifests, and the policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionFeatures {
    /// Tags of the task section, matched against manifest tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_tags: Vec<String>,
    /// The run's effect ceiling: manifests above it are excluded.
    pub effect_ceiling: Effect,
    /// Per-tool outcome snapshot, keyed by tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outcomes: BTreeMap<String, ToolOutcomeStats>,
}

/// Feature weights for [`select`]. Integer arithmetic only — scores are
/// byte-reproducible on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionWeights {
    /// Points per task-tag overlap, scaled by 10000: with the default
    /// weights one matched tag ties a perfect outcome statistic (10000
    /// basis points at weight 1) — ties break by ascending name — and two
    /// matched tags outrank any outcome statistic.
    pub tag_match: u32,
    /// Points per success basis point from the outcome snapshot.
    pub outcome_success: u32,
}

impl Default for SelectionWeights {
    fn default() -> Self {
        Self {
            tag_match: 1,
            outcome_success: 1,
        }
    }
}

/// The selection policy: when shortlisting engages, how many tools it
/// returns, and how features weigh. Lives in the context policy's tools
/// section (selection *policy* is assembly policy, not per-tool metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectionPolicy {
    /// Registry size above which shortlisting engages; at or below it the
    /// shortlist is the identity — every eligible tool, ranked.
    pub cutoff: usize,
    /// The shortlist size once engaged.
    pub k: usize,
    /// Feature weights.
    pub weights: SelectionWeights,
}

impl Default for ToolSelectionPolicy {
    fn default() -> Self {
        Self {
            cutoff: DEFAULT_SHORTLIST_CUTOFF,
            k: 8,
            weights: SelectionWeights::default(),
        }
    }
}

/// One scored manifest in a ranking: the record the section manifest
/// carries so an auditor reads why the model saw exactly these tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedManifest {
    /// The tool.
    pub name: String,
    /// The total score under the pinned weights.
    pub score: u64,
    /// The task tags this manifest matched (sorted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_tags: Vec<String>,
}

/// Why a manifest was excluded before scoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExclusionReason {
    /// The manifest's effect class exceeds the run's ceiling.
    EffectAboveCeiling,
    /// Prerequisites absent from the eligible pool (transitively).
    UnsatisfiedPrerequisites {
        /// The missing tool names (sorted).
        missing: Vec<String>,
    },
}

/// A manifest excluded from scoring, with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedManifest {
    /// The tool.
    pub name: String,
    /// Why it never scored.
    pub reason: ExclusionReason,
}

/// The outcome of selection: the shortlist handed to the model plus the
/// full ranking and exclusions, recorded verbatim in the section manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolShortlist {
    /// The selected tools, in rank order (prerequisites ahead of their
    /// dependents). What the model is shown.
    pub selected: Vec<RankedManifest>,
    /// Every scored manifest, in rank order — including tools the cut
    /// dropped. The audit trail.
    pub ranking: Vec<RankedManifest>,
    /// Manifests excluded before scoring, by name order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ExcludedManifest>,
}

/// Score and shortlist `manifests` against `features`, returning the top
/// `k` with the full ranking. Deterministic: scores are integer arithmetic
/// over the inputs, ties break by ascending name, and the result does not
/// depend on the input slice's order.
///
/// Gates apply before scoring: a manifest above the effect ceiling is
/// excluded, and a manifest whose transitive prerequisites leave the
/// eligible pool is excluded. Selection then walks the ranking and takes
/// each tool **with its prerequisite closure**; a tool whose closure would
/// overflow `k` is skipped whole — the model never sees a tool it cannot
/// drive end to end.
pub fn select(features: &SelectionFeatures, manifests: &[ToolManifest], k: usize) -> ToolShortlist {
    select_weighted(features, manifests, k, &SelectionWeights::default())
}

/// The scoring core. `select` runs it under the default weights;
/// `shortlist` runs it under the pinned policy's.
fn select_weighted(
    features: &SelectionFeatures,
    manifests: &[ToolManifest],
    k: usize,
    weights: &SelectionWeights,
) -> ToolShortlist {
    // Gate 1: effect ceiling.
    let mut excluded: Vec<ExcludedManifest> = Vec::new();
    let mut eligible: Vec<&ToolManifest> = Vec::new();
    for manifest in manifests {
        if manifest.effect > features.effect_ceiling {
            excluded.push(ExcludedManifest {
                name: manifest.name.clone(),
                reason: ExclusionReason::EffectAboveCeiling,
            });
        } else {
            eligible.push(manifest);
        }
    }
    let eligible_names: BTreeSet<&str> = eligible.iter().map(|m| m.name.as_str()).collect();

    // Gate 2: transitive prerequisites must stay inside the eligible pool.
    let mut still_eligible: Vec<&ToolManifest> = Vec::new();
    for manifest in eligible {
        let mut missing = BTreeSet::new();
        let mut stack: Vec<&str> = manifest.prerequisites.iter().map(String::as_str).collect();
        let mut seen: BTreeSet<&str> = stack.iter().copied().collect();
        while let Some(prerequisite) = stack.pop() {
            if !eligible_names.contains(prerequisite) {
                missing.insert(prerequisite.to_owned());
                continue;
            }
            // Walk one hop deeper into the closure.
            if let Some(parent) = manifests.iter().find(|m| m.name == prerequisite) {
                for next in &parent.prerequisites {
                    if seen.insert(next.as_str()) {
                        stack.push(next.as_str());
                    }
                }
            }
        }
        if missing.is_empty() {
            still_eligible.push(manifest);
        } else {
            excluded.push(ExcludedManifest {
                name: manifest.name.clone(),
                reason: ExclusionReason::UnsatisfiedPrerequisites {
                    missing: missing.into_iter().collect(),
                },
            });
        }
    }

    // Score: tags dominate per unit (scaled by 10000), outcomes refine.
    let task_tags: BTreeSet<&str> = features.task_tags.iter().map(String::as_str).collect();
    let mut ranking: Vec<RankedManifest> = still_eligible
        .iter()
        .map(|manifest| {
            let mut matched: Vec<String> = manifest
                .tags
                .iter()
                .filter(|tag| task_tags.contains(tag.as_str()))
                .cloned()
                .collect();
            matched.sort();
            let tag_score = u64::from(weights.tag_match) * matched.len() as u64 * 10_000;
            let outcome_score = features
                .outcomes
                .get(&manifest.name)
                .and_then(ToolOutcomeStats::success_bps)
                .map(u64::from)
                .unwrap_or(0)
                * u64::from(weights.outcome_success);
            RankedManifest {
                name: manifest.name.clone(),
                score: tag_score + outcome_score,
                matched_tags: matched,
            }
        })
        .collect();
    ranking.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.name.cmp(&right.name))
    });

    // Greedy top-k with prerequisite closure, in rank order.
    let mut selected: Vec<RankedManifest> = Vec::new();
    let mut selected_names: BTreeSet<&str> = BTreeSet::new();
    let by_name: BTreeMap<&str, &ToolManifest> =
        manifests.iter().map(|m| (m.name.as_str(), m)).collect();
    let ranked_by_name: BTreeMap<&str, &RankedManifest> =
        ranking.iter().map(|r| (r.name.as_str(), r)).collect();
    for candidate in &ranking {
        if selected_names.contains(candidate.name.as_str()) {
            continue;
        }
        // Transitive closure of this candidate's prerequisites, in rank
        // order (already gate-checked inside the eligible pool).
        let mut closure: Vec<&str> = Vec::new();
        let mut stack: Vec<&str> = by_name
            .get(candidate.name.as_str())
            .map(|m| m.prerequisites.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let mut seen: BTreeSet<&str> = stack.iter().copied().collect();
        while let Some(prerequisite) = stack.pop() {
            if selected_names.contains(prerequisite) || closure.contains(&prerequisite) {
                continue;
            }
            closure.push(prerequisite);
            if let Some(parent) = by_name.get(prerequisite) {
                for next in &parent.prerequisites {
                    if seen.insert(next.as_str()) {
                        stack.push(next.as_str());
                    }
                }
            }
        }
        // Prerequisites rank ahead of dependents inside the closure, under
        // the main ranking's order: score descending, name ascending.
        closure.sort_by(|left, right| {
            let left_key = rank_key(&ranked_by_name, left);
            let right_key = rank_key(&ranked_by_name, right);
            right_key
                .0
                .cmp(&left_key.0)
                .then_with(|| left_key.1.cmp(right_key.1))
        });
        let needed = closure.len() + 1;
        if selected.len() + needed > k {
            continue;
        }
        for prerequisite in closure {
            if let Some(ranked) = ranked_by_name.get(prerequisite) {
                selected.push((*ranked).clone());
                selected_names.insert(prerequisite);
            }
        }
        selected.push(candidate.clone());
        selected_names.insert(candidate.name.as_str());
    }

    excluded.sort_by(|left, right| left.name.cmp(&right.name));
    ToolShortlist {
        selected,
        ranking,
        excluded,
    }
}

fn rank_key<'a>(
    ranked_by_name: &BTreeMap<&'a str, &'a RankedManifest>,
    name: &'a str,
) -> (u64, &'a str) {
    match ranked_by_name.get(name) {
        Some(ranked) => (ranked.score, ranked.name.as_str()),
        None => (0, name),
    }
}

/// Apply `policy`: at or below `policy.cutoff` the shortlist is the
/// identity (every eligible tool, ranked); above it, the top `policy.k`.
pub fn shortlist(
    features: &SelectionFeatures,
    manifests: &[ToolManifest],
    policy: &ToolSelectionPolicy,
) -> ToolShortlist {
    if manifests.len() <= policy.cutoff {
        select_weighted(features, manifests, manifests.len(), &policy.weights)
    } else {
        select_weighted(features, manifests, policy.k, &policy.weights)
    }
}

/// One argument-schema violation: where, which rule, and a deterministic
/// human-readable message. The element shape of the structured refusal
/// contract's `violations` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgumentViolation {
    /// JSON Pointer to the offending value (`""` is the arguments root).
    pub path: String,
    /// The schema rule violated (`type`, `required`, …), snake_case.
    pub rule: String,
    /// Deterministic detail (no addresses, no clocks).
    pub message: String,
}

/// Build the structured refusal payload: `ERROR: {"kind":"argument_validation","violations":[…]}`
/// — compact JSON, key order pinned by construction. Byte-exactness rests on
/// serde_json's default `BTreeMap`-backed object ordering; enabling the
/// `preserve_order` feature would flip it — the golden fails loud if it ever
/// does. This string is THE contract the outcome roll-up parses; the golden
/// pins it byte-for-byte.
pub fn argument_validation_refusal(violations: &[ArgumentViolation]) -> String {
    let body = json!({
        "kind": ARGUMENT_VALIDATION_KIND,
        "violations": violations,
    });
    let compact = serde_json::to_string(&body)
        .expect("serializing a refusal body cannot fail: it is a plain JSON value");
    format!("{ERROR_PREFIX}{compact}")
}

/// The parse half of the contract: recognize a tool message (or journaled
/// output payload) as a [`ValidatingTool`] refusal and decode its
/// violations. Returns `None` for every other `ERROR:` string — the
/// roll-up's opaque-error tier.
pub fn parse_argument_validation_refusal(content: &str) -> Option<Vec<ArgumentViolation>> {
    let body = content.strip_prefix(ERROR_PREFIX)?;
    let value: Value = serde_json::from_str(body).ok()?;
    if value.get("kind")?.as_str()? != ARGUMENT_VALIDATION_KIND {
        return None;
    }
    serde_json::from_value(value.get("violations")?.clone()).ok()
}

/// Validate `args` against a JSON Schema object, returning every violation
/// found (empty = valid).
///
/// The supported keyword subset, documented and complete: `type` (string
/// or list), `enum`, `const`, `properties`, `required`,
/// `additionalProperties` (bool or schema), `items` (single schema),
/// `minItems`/`maxItems`, `minLength`/`maxLength`, `minimum`/`maximum`.
/// Unlisted keywords are ignored rather than mis-enforced — a validator
/// that half-checks `pattern` is worse than one that says it does not.
pub fn validate_arguments(schema: &Value, args: &Value) -> Vec<ArgumentViolation> {
    let mut violations = Vec::new();
    validate_value(schema, args, "", &mut violations);
    violations
}

fn violation(path: &str, rule: &str, message: String) -> ArgumentViolation {
    ArgumentViolation {
        path: path.to_owned(),
        rule: rule.to_owned(),
        message,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        // JSON Schema: an integer is a number with zero fractional part.
        "integer" => {
            matches!(value, Value::Number(n) if n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0))
        }
        "number" => value.is_number(),
        other => json_type_name(value) == other,
    }
}

fn validate_value(schema: &Value, value: &Value, path: &str, out: &mut Vec<ArgumentViolation>) {
    if let Some(expected) = schema.get("type") {
        let matches = match expected {
            Value::String(one) => type_matches(one.as_str(), value),
            Value::Array(many) => many
                .iter()
                .filter_map(Value::as_str)
                .any(|one| type_matches(one, value)),
            _ => true,
        };
        if !matches {
            out.push(violation(
                path,
                "type",
                format!("expected {expected}, found {}", json_type_name(value)),
            ));
            // Further keyword checks are meaningless against a wrong type.
            return;
        }
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array) {
        if !variants.contains(value) {
            out.push(violation(
                path,
                "enum",
                "value is not one of the declared enum variants".to_owned(),
            ));
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != value {
            out.push(violation(
                path,
                "const",
                "value does not equal the declared const".to_owned(),
            ));
        }
    }
    match value {
        Value::Object(map) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        out.push(violation(
                            path,
                            "required",
                            format!("missing required property `{name}`"),
                        ));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (key, item) in map {
                let child_path = format!("{path}/{}", escape_pointer(key));
                match properties.and_then(|p| p.get(key)) {
                    Some(child_schema) => validate_value(child_schema, item, &child_path, out),
                    None => match additional {
                        Some(Value::Bool(false)) => out.push(violation(
                            path,
                            "additional_properties",
                            format!("unexpected property `{key}`"),
                        )),
                        Some(schema @ Value::Object(_)) => {
                            validate_value(schema, item, &child_path, out)
                        }
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    out.push(violation(
                        path,
                        "min_items",
                        format!(
                            "array has {} items, fewer than the minimum {min}",
                            items.len()
                        ),
                    ));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
                if (items.len() as u64) > max {
                    out.push(violation(
                        path,
                        "max_items",
                        format!(
                            "array has {} items, more than the maximum {max}",
                            items.len()
                        ),
                    ));
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate_value(item_schema, item, &format!("{path}/{index}"), out);
                }
            }
        }
        Value::String(text) => {
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
                if (text.chars().count() as u64) < min {
                    out.push(violation(
                        path,
                        "min_length",
                        format!(
                            "string length {} is below the minimum {min}",
                            text.chars().count()
                        ),
                    ));
                }
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
                if (text.chars().count() as u64) > max {
                    out.push(violation(
                        path,
                        "max_length",
                        format!(
                            "string length {} exceeds the maximum {max}",
                            text.chars().count()
                        ),
                    ));
                }
            }
        }
        Value::Number(number) => {
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
                if number.as_f64().is_some_and(|n| n < minimum) {
                    out.push(violation(
                        path,
                        "minimum",
                        format!("value {number} is below the minimum {minimum}"),
                    ));
                }
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
                if number.as_f64().is_some_and(|n| n > maximum) {
                    out.push(violation(
                        path,
                        "maximum",
                        format!("value {number} exceeds the maximum {maximum}"),
                    ));
                }
            }
        }
        _ => {}
    }
}

/// JSON Pointer escaping (`~` → `~0`, `/` → `~1`) for path segments.
fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// A [`Tool`] wrapper that validates arguments against the wrapped tool's
/// JSON schema before dispatch. On violation it does NOT call the tool:
/// it returns the structured refusal payload
/// (`ERROR: {"kind":"argument_validation","violations":[…]}`) as the tool's
/// result, so the failure-isolation channel carries the byte-exact
/// contract string the outcome roll-up parses and the model's next
/// iteration answers with a repaired call. Identity — name, description,
/// schema, effect class, effect kind, idempotency key, and
/// [`Tool::effect_request`] — delegates to the wrapped tool, so the
/// wrapper is transparent to the admission boundary (the `tool.rs` rule
/// for wrappers).
pub struct ValidatingTool {
    inner: Arc<dyn Tool>,
}

impl ValidatingTool {
    /// A validating wrapper around `inner`.
    pub fn new(inner: Arc<dyn Tool>) -> Self {
        Self { inner }
    }

    /// The wrapped tool.
    pub fn inner(&self) -> &Arc<dyn Tool> {
        &self.inner
    }

    /// Clone `registry` with every tool wrapped: the recipe-3 registration
    /// shape (`register_shared` over `Arc<dyn Tool>`). Compose with
    /// `restricted_to` for construction-time narrowing:
    /// `ValidatingTool::wrap_registry(&registry.restricted_to(&names)?)`.
    pub fn wrap_registry(registry: &ToolRegistry) -> ToolRegistry {
        let mut names: Vec<&str> = registry.names().collect();
        names.sort_unstable();
        let mut wrapped = ToolRegistry::new();
        for name in names {
            let tool = registry.get(name).expect("name came from the registry");
            wrapped.register_shared(Arc::new(Self::new(tool)));
        }
        wrapped
    }
}

#[async_trait]
impl Tool for ValidatingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn effect(&self) -> Effect {
        self.inner.effect()
    }

    fn effect_kind(&self) -> &str {
        self.inner.effect_kind()
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.inner.idempotency_key(args)
    }

    fn effect_request(&self, call: &ToolCall) -> EffectRequest {
        self.inner.effect_request(call)
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let violations = validate_arguments(&self.inner.parameters_schema(), &args);
        if !violations.is_empty() {
            return Ok(Value::String(argument_validation_refusal(&violations)));
        }
        self.inner.call(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Search;

    #[async_trait]
    impl Tool for Search {
        fn name(&self) -> &str {
            "web.search"
        }
        fn description(&self) -> &str {
            "Searches the web."
        }
        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50}
                },
                "required": ["query"],
                "additionalProperties": false
            })
        }
        fn effect(&self) -> Effect {
            Effect::ReadOnly
        }
        async fn call(&self, args: Value) -> Result<Value> {
            Ok(json!({"echo": args}))
        }
    }

    fn capability(name: &str, effect: Effect) -> ToolCapability {
        ToolCapability {
            name: name.to_owned(),
            description: format!("{name} tool."),
            parameters_schema: json!({"type": "object"}),
            effect,
        }
    }

    #[test]
    fn manifest_derives_defaults_from_effect_class() {
        let read = ToolManifest::from_capability(&capability("a", Effect::ReadOnly));
        assert!(read.parallel_safe);
        assert_eq!(read.cost_class, CostClass::Medium);
        assert!(!read.batchable);
        assert!(read.tags.is_empty());

        let write = ToolManifest::from_capability(&capability("b", Effect::NonIdempotent));
        assert!(!write.parallel_safe, "default false for NonIdempotent");

        let compensatable = ToolManifest::from_capability(&capability("c", Effect::Compensatable));
        assert!(!compensatable.parallel_safe);
    }

    #[test]
    fn overlay_validation_is_fail_closed() {
        let dup_tags = ToolSelectionOverlay {
            tags: vec!["web".into(), "web".into()],
            ..Default::default()
        };
        assert!(dup_tags.validate().is_err());

        let bad_prereq = ToolSelectionOverlay {
            prerequisites: vec!["not a name".into()],
            ..Default::default()
        };
        assert!(bad_prereq.validate().is_err());

        let untrimmed = ToolSelectionOverlay {
            when_to_use: Some(" padded".into()),
            ..Default::default()
        };
        assert!(untrimmed.validate().is_err());
    }

    #[test]
    fn overlay_application_overrides_defaults() {
        let overlay = ToolSelectionOverlay {
            tags: vec!["web".into()],
            when_to_use: Some("For open questions.".into()),
            cost_class: Some(CostClass::High),
            parallel_safe: Some(false),
            batchable: Some(true),
            prerequisites: vec!["http.get".into()],
        };
        let manifest = ToolManifest::from_capability(&capability("web.search", Effect::ReadOnly))
            .with_overlay(&overlay)
            .unwrap();
        assert_eq!(manifest.tags, ["web"]);
        assert_eq!(manifest.cost_class, CostClass::High);
        assert!(!manifest.parallel_safe);
        assert!(manifest.batchable);
        assert_eq!(manifest.prerequisites, ["http.get"]);
    }

    #[test]
    fn manifests_for_registry_rejects_unknown_overlay() {
        let mut registry = ToolRegistry::new();
        registry.register(Search);
        let mut overlays = BTreeMap::new();
        overlays.insert("ghost".to_owned(), ToolSelectionOverlay::default());
        assert!(manifests_for_registry(&registry, &overlays).is_err());
    }

    #[test]
    fn validate_arguments_covers_the_keyword_subset() {
        let schema = Search.parameters_schema();

        let valid = validate_arguments(&schema, &json!({"query": "rust", "limit": 5}));
        assert!(valid.is_empty(), "got: {valid:?}");

        let missing = validate_arguments(&schema, &json!({}));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].rule, "required");
        assert_eq!(missing[0].path, "");

        let wrong_type = validate_arguments(&schema, &json!({"query": "rust", "limit": "5"}));
        assert_eq!(wrong_type.len(), 1);
        assert_eq!(wrong_type[0].rule, "type");
        assert_eq!(wrong_type[0].path, "/limit");

        let extra = validate_arguments(&schema, &json!({"query": "rust", "bogus": 1}));
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].rule, "additional_properties");

        let bounds = validate_arguments(&schema, &json!({"query": "rust", "limit": 0}));
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].rule, "minimum");

        // Quoted numerics never coerce: "5" is a string, loudly.
        let quoted = validate_arguments(&schema, &json!({"query": "rust", "limit": "5"}));
        assert_eq!(quoted[0].message, "expected \"integer\", found string");
    }

    #[test]
    fn validate_arguments_walks_nested_paths() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "items": {"type": "object", "properties": {"sku": {"type": "string"}}, "required": ["sku"]}
                }
            },
            "required": ["items"]
        });
        let violations =
            validate_arguments(&schema, &json!({"items": [{"sku": "a"}, {"nope": 1}]}));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].path, "/items/1");
        assert_eq!(violations[0].rule, "required");

        let empty = validate_arguments(&schema, &json!({"items": []}));
        assert_eq!(empty[0].rule, "min_items");
    }

    fn manifest(name: &str, effect: Effect, tags: &[&str], prereqs: &[&str]) -> ToolManifest {
        ToolManifest::from_capability(&capability(name, effect))
            .with_overlay(&ToolSelectionOverlay {
                tags: tags.iter().map(|t| t.to_string()).collect(),
                prerequisites: prereqs.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            })
            .unwrap()
    }

    fn features(tags: &[&str], ceiling: Effect) -> SelectionFeatures {
        SelectionFeatures {
            task_tags: tags.iter().map(|t| t.to_string()).collect(),
            effect_ceiling: ceiling,
            outcomes: BTreeMap::new(),
        }
    }

    #[test]
    fn select_ranks_by_tag_overlap_then_name() {
        let manifests = vec![
            manifest("zeta", Effect::ReadOnly, &["web"], &[]),
            manifest("alpha", Effect::ReadOnly, &["web", "news"], &[]),
            manifest("mid", Effect::ReadOnly, &["news"], &[]),
        ];
        let outcome = select(
            &features(&["web", "news"], Effect::NonIdempotent),
            &manifests,
            3,
        );
        let order: Vec<&str> = outcome.ranking.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["alpha", "mid", "zeta"]);
        assert_eq!(outcome.ranking[0].matched_tags, ["news", "web"]);
        assert!(outcome.excluded.is_empty());
    }

    #[test]
    fn select_is_order_independent() {
        let manifests = vec![
            manifest("a", Effect::ReadOnly, &["web"], &[]),
            manifest("b", Effect::ReadOnly, &["web"], &[]),
            manifest("c", Effect::ReadOnly, &[], &[]),
        ];
        let mut reversed = manifests.clone();
        reversed.reverse();
        let forward = select(&features(&["web"], Effect::NonIdempotent), &manifests, 2);
        let backward = select(&features(&["web"], Effect::NonIdempotent), &reversed, 2);
        assert_eq!(forward, backward);
        let names: Vec<&str> = forward.selected.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn outcome_stats_refine_equal_tag_scores() {
        let mut feats = features(&["web"], Effect::NonIdempotent);
        feats.outcomes.insert(
            "reliable".to_owned(),
            ToolOutcomeStats {
                calls: 10,
                successes: 9,
                validation_failures: 0,
            },
        );
        feats.outcomes.insert(
            "flaky".to_owned(),
            ToolOutcomeStats {
                calls: 10,
                successes: 2,
                validation_failures: 3,
            },
        );
        let manifests = vec![
            manifest("flaky", Effect::ReadOnly, &["web"], &[]),
            manifest("reliable", Effect::ReadOnly, &["web"], &[]),
        ];
        let outcome = select(&feats, &manifests, 2);
        assert_eq!(outcome.ranking[0].name, "reliable");
        assert!(outcome.ranking[0].score > outcome.ranking[1].score);
    }

    #[test]
    fn effect_ceiling_excludes_before_scoring() {
        let manifests = vec![
            manifest("reader", Effect::ReadOnly, &["web"], &[]),
            manifest("sender", Effect::NonIdempotent, &["web"], &[]),
        ];
        let outcome = select(&features(&["web"], Effect::ReadOnly), &manifests, 5);
        assert_eq!(outcome.ranking.len(), 1);
        assert_eq!(outcome.excluded.len(), 1);
        assert_eq!(outcome.excluded[0].name, "sender");
        assert_eq!(
            outcome.excluded[0].reason,
            ExclusionReason::EffectAboveCeiling
        );
    }

    #[test]
    fn prerequisite_closure_gates_and_pulls() {
        let manifests = vec![
            manifest("fetch", Effect::ReadOnly, &["web"], &[]),
            manifest("summarize", Effect::ReadOnly, &["web"], &["fetch"]),
        ];
        // Closure pulls the prerequisite into the shortlist, ahead of its
        // dependent, consuming a slot.
        let outcome = select(&features(&["web"], Effect::NonIdempotent), &manifests, 1);
        let names: Vec<&str> = outcome.selected.iter().map(|r| r.name.as_str()).collect();
        // k=1 cannot fit summarize+fetch, and fetch needs nothing: the
        // closure rule picks the self-contained tool.
        assert_eq!(names, ["fetch"]);

        let outcome = select(&features(&["web"], Effect::NonIdempotent), &manifests, 2);
        let names: Vec<&str> = outcome.selected.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["fetch", "summarize"]);
    }

    #[test]
    fn missing_prerequisite_excludes() {
        let manifests = vec![manifest(
            "summarize",
            Effect::ReadOnly,
            &["web"],
            &["ghost"],
        )];
        let outcome = select(&features(&["web"], Effect::NonIdempotent), &manifests, 5);
        assert!(outcome.selected.is_empty());
        assert_eq!(
            outcome.excluded[0].reason,
            ExclusionReason::UnsatisfiedPrerequisites {
                missing: vec!["ghost".to_owned()]
            }
        );
    }

    #[test]
    fn shortlist_is_identity_below_cutoff() {
        let policy = ToolSelectionPolicy {
            cutoff: 3,
            k: 1,
            ..Default::default()
        };
        let manifests = vec![
            manifest("a", Effect::ReadOnly, &[], &[]),
            manifest("b", Effect::ReadOnly, &[], &[]),
        ];
        let outcome = shortlist(&features(&[], Effect::NonIdempotent), &manifests, &policy);
        assert_eq!(outcome.selected.len(), 2, "below cutoff: identity");
        let big: Vec<ToolManifest> = (0..5)
            .map(|i| manifest(&format!("tool{i}"), Effect::ReadOnly, &[], &[]))
            .collect();
        let outcome = shortlist(&features(&[], Effect::NonIdempotent), &big, &policy);
        assert_eq!(outcome.selected.len(), 1, "above cutoff: top-k");
    }

    #[tokio::test]
    async fn validating_tool_passes_valid_calls_through() {
        let tool = ValidatingTool::new(Arc::new(Search));
        let result = tool.call(json!({"query": "rust"})).await.unwrap();
        assert_eq!(result, json!({"echo": {"query": "rust"}}));
    }

    #[tokio::test]
    async fn validating_tool_refusal_is_byte_exact() {
        let tool = ValidatingTool::new(Arc::new(Search));
        let result = tool.call(json!({"limit": "5"})).await.unwrap();
        let Value::String(content) = result else {
            panic!("refusal is a string payload");
        };
        assert_eq!(
            content,
            "ERROR: {\"kind\":\"argument_validation\",\"violations\":[\
             {\"message\":\"missing required property `query`\",\"path\":\"\",\"rule\":\"required\"},\
             {\"message\":\"expected \\\"integer\\\", found string\",\"path\":\"/limit\",\"rule\":\"type\"}]}"
        );
        // The parse half round-trips the contract.
        let violations = parse_argument_validation_refusal(&content).unwrap();
        assert_eq!(violations.len(), 2);
        assert_eq!(violations[0].rule, "required");
        // Opaque error strings stay opaque.
        assert!(parse_argument_validation_refusal("ERROR: boom").is_none());
        assert!(
            parse_argument_validation_refusal("ERROR: {\"kind\":\"other\",\"violations\":[]}")
                .is_none()
        );
    }

    struct Keyed;

    #[async_trait]
    impl Tool for Keyed {
        fn name(&self) -> &str {
            "kv.put"
        }
        fn description(&self) -> &str {
            "Stores a value."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]})
        }
        fn effect(&self) -> Effect {
            Effect::Idempotent
        }
        fn effect_kind(&self) -> &str {
            "kv.write"
        }
        fn idempotency_key(&self, args: &Value) -> Option<String> {
            args.get("key").and_then(Value::as_str).map(str::to_owned)
        }
        async fn call(&self, args: Value) -> Result<Value> {
            Ok(args)
        }
    }

    #[test]
    fn validating_tool_delegates_identity_and_effect_request() {
        let inner = Arc::new(Keyed);
        let wrapped = ValidatingTool::new(inner.clone());
        assert_eq!(wrapped.name(), "kv.put");
        assert_eq!(wrapped.effect(), Effect::Idempotent);
        assert_eq!(wrapped.effect_kind(), "kv.write");
        assert_eq!(
            wrapped.idempotency_key(&json!({"key": "k1"})),
            Some("k1".to_owned())
        );
        let call = ToolCall::new("c1", "kv.put", json!({"key": "k1"}));
        assert_eq!(wrapped.effect_request(&call), inner.effect_request(&call));
    }

    #[tokio::test]
    async fn wrap_registry_wraps_every_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Search);
        registry.register(Keyed);
        let wrapped = ValidatingTool::wrap_registry(&registry);
        assert_eq!(wrapped.len(), 2);
        let search = wrapped.get("web.search").unwrap();
        let refusal = search.call(json!({})).await.unwrap();
        assert!(matches!(&refusal, Value::String(s) if s.starts_with(ERROR_PREFIX)));
    }
}
