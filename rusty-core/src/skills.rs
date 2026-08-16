//! Skill selection and governed activation: the run-integration plane over
//! the shipped [`crate::skill`] package registry (R0.13 agent core,
//! wave 4a).
//!
//! `skill.rs` ships versioned, content-addressed, scanned packages with
//! three disclosure tiers and a forward-only latest pointer — and says in
//! its module docs that selection, activation, and load journaling belong
//! to a run-integration slice. This module is that slice:
//!
//! - **[`SkillBinding`]** — the run-facing half of a skill: when-to-use
//!   metadata beyond the description (trigger tags matched structurally
//!   against the task section, a task-shape note, a cost class) and the
//!   *enforceable* tool set. The frontmatter's advisory `allowed-tools`
//!   becomes a declared set that narrows what a call may reach while the
//!   skill is active (S5). This is the wire shape the learn plane's
//!   `CandidateContent::Skill { name, content_hash, binding }` carries —
//!   the contract with the parallel stream that owns the `learn.rs`
//!   delta; extract it into a [`SkillPin`] to bind a candidate.
//! - **Structural shortlisting** ([`select_skills`]) — tier-1 catalog
//!   entries scored by trigger-tag overlap with the task and gated by
//!   declared tool availability after the run's narrowing, returning a
//!   deterministic top-k plus the full ranking and exclusions. No
//!   embeddings, per the release's vector decision.
//! - **Version-pointer resolution** ([`resolve_active_skill`]) — the
//!   learn plane's [`VersionPointer`] over the surface `skill:{name}` is
//!   the active authority: it chooses which *revision* the pipeline
//!   binds. [`crate::skill::SkillRegistry`]'s latest pointer stays
//!   authorship history and is never consulted for activation. A skill
//!   with nothing promoted does not bind — there is no silent
//!   latest-pointer fallback.
//! - **[`SkillGateTool`]** — the per-invocation gating [`Tool`] wrapper.
//!   It reads the assembly's active-skill set through a shared
//!   [`ActiveSkills`] handle (handed over at construction, updated by the
//!   assembly driver per assembly) and refuses a call outside the active
//!   skills' declared tool union with the structured payload
//!   `ERROR: {"kind":"skill_tool_gate",…}` — the second parsed kind of
//!   the outcome roll-up's two-tier contract (N10; the first is
//!   [`crate::tool_select::ARGUMENT_VALIDATION_KIND`]). The refusal
//!   returns as the tool's result, so it journals as an ordinary
//!   `ToolCall` under the `RecordingTool` pattern — evidenced,
//!   attributable, replayable. A skill can only narrow the run's tools,
//!   never widen them.
//!
//! # Evidence without a new event kind
//!
//! Skill selection and activation journal through the context pipeline's
//! existing carriers: the section manifest's `skills` report pins every
//! assembled skill as `name@revision:hash`, and the bodies ride inside
//! the journaled `ModelCall` input. A dedicated `SkillLoaded` event —
//! which would journal loads as first-class effects — is this design's
//! own proposal, deliberately deferred to a wave where `record.rs` is
//! unclaimed; this module adds no `RunEventKind` variants.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::context::SkillSectionEntry;
use crate::error::Result;
use crate::learn::{CandidateId, VersionPointer};
use crate::registry::pointer_admission;
use crate::skill::{SkillMetadata, SkillRegistry, SkillVersionSelector};
use crate::tool::Tool;
use crate::tool_select::{CostClass, ERROR_PREFIX, MAX_TAG_BYTES, MAX_WHEN_TO_USE_BYTES};

/// The surface prefix a skill's [`VersionPointer`] governs: `skill:{name}`.
pub const SKILL_SURFACE_PREFIX: &str = "skill:";

/// The reserved `kind` of [`SkillGateTool`]'s structured refusal payload:
/// `ERROR: {"kind":"skill_tool_gate",…}`. The outcome roll-up keys on this
/// exact string as its second parsed kind; do not rename without a wave.
pub const SKILL_TOOL_GATE_KIND: &str = "skill_tool_gate";

/// Catalog size at which ranked shortlisting engages by default. Declared
/// in the policy, never hard-coded into selection — same discipline as
/// [`crate::tool_select::DEFAULT_SHORTLIST_CUTOFF`].
pub const DEFAULT_SKILL_SHORTLIST_CUTOFF: usize = 20;

/// Default shortlist depth when the cutoff is exceeded.
pub const DEFAULT_SKILL_SHORTLIST_K: usize = 5;

/// The most trigger tags one binding may declare.
pub const MAX_TRIGGER_TAGS: usize = 16;

/// The most tools one binding may declare (the frontmatter's
/// [`crate::skill::MAX_ALLOWED_TOOLS`] bound applied to the enforceable
/// half).
pub const MAX_BINDING_TOOLS: usize = crate::skill::MAX_ALLOWED_TOOLS;

/// Every way the run-integration plane can refuse a binding, a resolution,
/// or a section assembly. Module-local, mirroring
/// [`crate::skill::SkillError`]: the refused rule is named in the type.
#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    /// A binding broke its shape rules.
    #[error("invalid skill binding: {reason}")]
    InvalidBinding {
        /// The rule it broke.
        reason: &'static str,
    },

    /// The pointer's candidate did not resolve to a skill pin.
    #[error(
        "skill candidate {candidate_id} did not resolve to a skill pin — the candidate \
         store must carry the `CandidateContent::Skill` content for the pointed candidate"
    )]
    CandidateNotPinned {
        /// The unresolvable candidate.
        candidate_id: String,
    },

    /// The pointer's surface does not name the pinned skill.
    #[error(
        "version pointer surface `{surface}` does not match the pinned skill `{skill}` \
         (expected `skill:{skill}`)"
    )]
    SurfaceMismatch {
        /// The pointer's (untagged) surface.
        surface: String,
        /// The skill the candidate pin names.
        skill: String,
    },

    /// The pinned content hash is not a registered revision of the skill.
    #[error(
        "skill `{name}` has no registered version at content hash {content_hash} — the learn \
         pointer names a version the skill registry does not hold"
    )]
    VersionNotRegistered {
        /// The skill name.
        name: String,
        /// The content hash the pin named.
        content_hash: String,
    },

    /// An active skill's pinned version could not supply its body for the
    /// section — a registry that accepted activation but not disclosure is
    /// inconsistent, and assembly fails closed rather than shipping a
    /// pin without its text.
    #[error("active skill `{name}` at {content_hash} could not be disclosed from the registry")]
    Undisclosable {
        /// The skill name.
        name: String,
        /// The pinned content hash.
        content_hash: String,
    },
}

// --------------------------------------------------------------------- //
// SkillBinding — the run-facing half of a skill
// --------------------------------------------------------------------- //

/// The run-facing metadata of a skill: when-to-use signals for structural
/// selection, and the declared tool set the gate enforces while the skill
/// is active.
///
/// This is the wire shape a `skill` candidate's `binding` member carries
/// (wave 4's `learn.rs` delta, owned by the parallel stream): every member
/// is optional on the wire, and unset members stay absent, so old records
/// keep deserializing (the established evolution rule). Validation is
/// fail-closed on shape; *availability* of declared tools is checked at
/// selection time, where the run's narrowed registry is known.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillBinding {
    /// Tags matched structurally against the task section's tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_tags: Vec<String>,

    /// When-to-use note beyond the description, when declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_shape: Option<String>,

    /// Relative cost class of running with this skill active, when
    /// declared (the tool plane's ladder; selection may narrow against a
    /// run's budgets, never widen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_class: Option<CostClass>,

    /// The declared tool set: while this skill is active, calls may reach
    /// only tools in the union of the active skills' sets (S5). Empty
    /// declares no narrowing contribution. Only ever narrows the run's
    /// tools — the gate admits a subset of what the run already has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// One member of the structured gate refusal's `active_skills` array: the
/// pin of a skill active at refusal time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSkillPin {
    /// The skill's name.
    pub name: String,
    /// The bound revision.
    pub revision: u64,
    /// The bound content hash.
    pub content_hash: String,
}

impl SkillBinding {
    /// Fail-closed shape validation: bounded, trimmed, control-free, and
    /// tool entries under the frontmatter's own charset rule.
    pub fn validate(&self) -> std::result::Result<(), SkillsError> {
        if self.trigger_tags.len() > MAX_TRIGGER_TAGS {
            return Err(SkillsError::InvalidBinding {
                reason: "too many trigger tags",
            });
        }
        for tag in &self.trigger_tags {
            if tag.is_empty() || tag.len() > MAX_TAG_BYTES || tag.chars().any(char::is_control) {
                return Err(SkillsError::InvalidBinding {
                    reason: "trigger tags must be non-empty, bounded, and control-free",
                });
            }
        }
        if let Some(note) = &self.task_shape {
            if note.is_empty()
                || note != note.trim()
                || note.len() > MAX_WHEN_TO_USE_BYTES
                || note.chars().any(char::is_control)
            {
                return Err(SkillsError::InvalidBinding {
                    reason: "task shape must be non-empty, trimmed, bounded, and control-free",
                });
            }
        }
        if self.tools.len() > MAX_BINDING_TOOLS {
            return Err(SkillsError::InvalidBinding {
                reason: "too many declared tools",
            });
        }
        for tool in &self.tools {
            if tool.is_empty()
                || tool.len() > crate::skill::MAX_FRONTMATTER_VALUE_BYTES
                || !tool
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
            {
                return Err(SkillsError::InvalidBinding {
                    reason: "declared tools must be bounded tool names (ASCII letters, digits, `.`, `_`, `:`, `-`)",
                });
            }
        }
        Ok(())
    }
}

/// What a promoted `skill` candidate pins, extracted: the skill name, the
/// package content address (the skill plane's own digest — candidate
/// identity and package identity are one hash), and the binding.
///
/// [`skill_pin`] is the extraction: one match arm over
/// `CandidateContent::Skill { name, content_hash, binding }`.
pub struct SkillPin {
    /// The skill name (the registry key).
    pub name: String,
    /// The package content address.
    pub content_hash: String,
    /// The run-facing binding.
    pub binding: SkillBinding,
}

/// The typed extraction [`SkillPin`]'s docs promise: one match arm over a
/// candidate's content, `None` for every non-skill candidate. A skill
/// candidate without a declared binding pins the default (empty) binding —
/// no trigger tags, no declared tools, nothing narrowed.
pub fn skill_pin(candidate: &crate::learn::Candidate) -> Option<SkillPin> {
    match &candidate.content {
        crate::learn::CandidateContent::Skill {
            name,
            content_hash,
            binding,
        } => Some(SkillPin {
            name: name.clone(),
            content_hash: content_hash.clone(),
            binding: binding.clone().unwrap_or_default(),
        }),
        _ => None,
    }
}

// --------------------------------------------------------------------- //
// Active-skill resolution: the learn pointer is the active authority
// --------------------------------------------------------------------- //

/// One active skill: the revision the learn pointer bound, with the
/// declared tool set the gate enforces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSkill {
    /// The skill's name.
    pub name: String,
    /// The bound revision (1-based, from the skill registry's history).
    pub revision: u64,
    /// The bound package content address.
    pub content_hash: String,
    /// The declared tool set from the binding (empty: narrows nothing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl ActiveSkill {
    /// The pin form the gate refusal carries.
    fn pin(&self) -> ActiveSkillPin {
        ActiveSkillPin {
            name: self.name.clone(),
            revision: self.revision,
            content_hash: self.content_hash.clone(),
        }
    }
}

/// The set of skills active for an assembly: which revisions bound, and
/// which tools they jointly declare. Name-sorted, so the gate's decisions
/// and the refusal payload are deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSkillSet {
    skills: Vec<ActiveSkill>,
}

impl ActiveSkillSet {
    /// Build from resolved skills; sorted by name, duplicate names
    /// converge onto the first resolution (a name has one pointer).
    pub fn new(mut skills: Vec<ActiveSkill>) -> Self {
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills.dedup_by(|a, b| a.name == b.name);
        Self { skills }
    }

    /// The active skills, name-sorted.
    pub fn skills(&self) -> &[ActiveSkill] {
        &self.skills
    }

    /// `true` when no skill is active.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// One active skill by name.
    pub fn find(&self, name: &str) -> Option<&ActiveSkill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The union of the active skills' declared tool sets, sorted.
    pub fn declared_tools(&self) -> BTreeSet<String> {
        self.skills
            .iter()
            .flat_map(|skill| skill.tools.iter().cloned())
            .collect()
    }

    /// The gate's admission rule: when no active skill declares any tool,
    /// nothing is narrowed and every call passes; otherwise a call passes
    /// exactly when its tool is in the declared union.
    pub fn permits(&self, tool: &str) -> bool {
        let declared = self.declared_tools();
        declared.is_empty() || declared.contains(tool)
    }
}

/// The shared handle the assembly driver and the gate wrappers both hold:
/// the driver replaces the set per assembly; wrappers read it at call
/// time, so narrowing is per-invocation state (S5), not the run's static
/// allowlist. Cheap to clone; every clone reads the same set.
#[derive(Clone, Default)]
pub struct ActiveSkills(Arc<RwLock<ActiveSkillSet>>);

impl ActiveSkills {
    /// A handle over an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the active set (the assembly driver, per assembly).
    pub fn set(&self, set: ActiveSkillSet) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = set;
    }

    /// The set as of this read.
    pub fn snapshot(&self) -> ActiveSkillSet {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl std::fmt::Debug for ActiveSkills {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveSkills")
            .field("skills", &self.snapshot().skills)
            .finish()
    }
}

/// Resolve one skill's active version through the learn plane's pointer:
/// the generic [`pointer_admission`] rule (canary draw, then full-traffic
/// active), the candidate's [`SkillPin`], and the skill registry's
/// content-addressed history. Returns `None` when the pointer serves
/// nothing — a skill with nothing promoted does not bind; the registry's
/// latest pointer is authorship history, never an activation fallback.
///
/// `pin` maps a candidate id to its skill pin — the caller's half of the
/// contract with the candidate store: [`skill_pin`] applied to the store's
/// candidate lookup.
pub fn resolve_active_skill(
    registry: &SkillRegistry,
    pointer: &VersionPointer,
    run_id: &str,
    pin: &dyn Fn(&CandidateId) -> Option<SkillPin>,
) -> std::result::Result<Option<ActiveSkill>, SkillsError> {
    let Some((candidate_id, _binding)) = pointer_admission(pointer, run_id) else {
        return Ok(None);
    };
    let pin = pin(&candidate_id).ok_or_else(|| SkillsError::CandidateNotPinned {
        candidate_id: candidate_id.as_str().to_owned(),
    })?;
    // The tagged surface is the draw's seed; the name check runs against
    // the untagged base (`skill:{name}`).
    let (base, _tag) = pointer.surface.split_tag();
    let expected = format!("{SKILL_SURFACE_PREFIX}{}", pin.name);
    if base.as_str() != expected {
        return Err(SkillsError::SurfaceMismatch {
            surface: base.as_str().to_owned(),
            skill: pin.name,
        });
    }
    let version = registry
        .get_version(
            &pin.name,
            SkillVersionSelector::ContentHash(pin.content_hash.clone()),
        )
        .ok_or_else(|| SkillsError::VersionNotRegistered {
            name: pin.name.clone(),
            content_hash: pin.content_hash.clone(),
        })?;
    Ok(Some(ActiveSkill {
        name: pin.name,
        revision: version.revision(),
        content_hash: pin.content_hash,
        tools: pin.binding.tools,
    }))
}

/// Resolve every pointer into the active set for one run. Pointers serve
/// in slice order; the set itself is name-sorted.
pub fn resolve_active_set(
    registry: &SkillRegistry,
    pointers: &[VersionPointer],
    run_id: &str,
    pin: &dyn Fn(&CandidateId) -> Option<SkillPin>,
) -> std::result::Result<ActiveSkillSet, SkillsError> {
    let mut skills = Vec::new();
    for pointer in pointers {
        if let Some(skill) = resolve_active_skill(registry, pointer, run_id, pin)? {
            skills.push(skill);
        }
    }
    Ok(ActiveSkillSet::new(skills))
}

// --------------------------------------------------------------------- //
// Structural shortlisting
// --------------------------------------------------------------------- //

/// The catalog view the shortlist scores: tier-1 metadata plus the
/// binding. Bodies never enter selection — the tier-2 load happens only
/// for skills the active set binds, at section-assembly time.
#[derive(Debug, Clone)]
pub struct SkillCatalogEntry {
    /// The tier-1 disclosure unit (name, description, revision, hash).
    pub metadata: SkillMetadata,
    /// The run-facing binding.
    pub binding: SkillBinding,
}

/// The features a selection runs against: the task's tags, and the tools
/// the run actually has after construction-time narrowing. `None` for
/// `available_tools` means the caller has no narrowing information — the
/// availability gate is skipped, never guessed.
#[derive(Debug, Clone, Default)]
pub struct SkillSelectionFeatures {
    /// The task section's capability tags.
    pub task_tags: Vec<String>,
    /// The run's tool names after narrowing, when known.
    pub available_tools: Option<BTreeSet<String>>,
}

/// The shortlist policy: when ranked selection engages and how deep it
/// cuts. Assembly policy — pinned by the application alongside the
/// context policy, versioned with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSelectionPolicy {
    /// Catalog size at which ranked shortlisting engages. Below it,
    /// selection is identity: every eligible skill is shortlisted, ranked.
    pub cutoff: usize,
    /// The shortlist depth above the cutoff.
    pub k: usize,
}

impl Default for SkillSelectionPolicy {
    fn default() -> Self {
        Self {
            cutoff: DEFAULT_SKILL_SHORTLIST_CUTOFF,
            k: DEFAULT_SKILL_SHORTLIST_K,
        }
    }
}

/// One skill in the ranking: its pin, its score, and the tier-1 line the
/// skills section renders for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedSkill {
    /// The skill's name.
    pub name: String,
    /// The catalog revision this entry names.
    pub revision: u64,
    /// The package content address.
    pub content_hash: String,
    /// The selection score (integer arithmetic; ties break by name).
    pub score: u64,
    /// The task tags this skill's binding matched (sorted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_tags: Vec<String>,
    /// The tier-1 metadata line the section carries.
    pub metadata: String,
}

/// Why a catalog entry was excluded before scoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SkillExclusionReason {
    /// The binding declares tools the narrowed run does not have — a
    /// skill whose instructions assume tools it cannot reach is excluded
    /// rather than shown and gated at call time.
    UnavailableTools {
        /// The missing tool names (sorted).
        missing: Vec<String>,
    },
}

/// A catalog entry excluded from scoring, with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedSkill {
    /// The skill's name.
    pub name: String,
    /// The catalog revision.
    pub revision: u64,
    /// The package content address.
    pub content_hash: String,
    /// Why it never scored.
    pub reason: SkillExclusionReason,
}

/// The outcome of skill selection: the shortlist the skills section
/// carries plus the full ranking and the exclusions — the audit trail for
/// why the model saw exactly these skills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSelection {
    /// The shortlisted skills, in rank order.
    pub selected: Vec<RankedSkill>,
    /// Every scored skill, in rank order — including entries the cut
    /// dropped.
    pub ranking: Vec<RankedSkill>,
    /// Entries excluded before scoring, by name order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ExcludedSkill>,
}

/// Render the tier-1 metadata line for one entry: the description, plus
/// the task-shape note when the binding declares one.
fn metadata_line(entry: &SkillCatalogEntry) -> String {
    match &entry.binding.task_shape {
        Some(note) => format!("{} — {note}", entry.metadata.description),
        None => entry.metadata.description.clone(),
    }
}

/// Score and shortlist `catalog` against `features` under `policy`.
///
/// Deterministic: the catalog is scored in name order, scores are integer
/// arithmetic over the inputs, and ties break by ascending name — the
/// result does not depend on the input slice's order. The availability
/// gate applies before scoring: an entry whose declared tools leave the
/// narrowed run's set is excluded, never half-shown. Below the cutoff the
/// shortlist is identity (every eligible entry, ranked); above it, the
/// top `k`.
pub fn select_skills(
    features: &SkillSelectionFeatures,
    catalog: &[SkillCatalogEntry],
    policy: &SkillSelectionPolicy,
) -> SkillSelection {
    // Gate: declared tool availability after the run's narrowing.
    let mut excluded: Vec<ExcludedSkill> = Vec::new();
    let mut eligible: Vec<&SkillCatalogEntry> = Vec::new();
    for entry in catalog {
        let missing: Vec<String> = match &features.available_tools {
            Some(available) => entry
                .binding
                .tools
                .iter()
                .filter(|tool| !available.contains(*tool))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        if missing.is_empty() {
            eligible.push(entry);
        } else {
            excluded.push(ExcludedSkill {
                name: entry.metadata.name.clone(),
                revision: entry.metadata.revision,
                content_hash: entry.metadata.content_hash.clone(),
                reason: SkillExclusionReason::UnavailableTools { missing },
            });
        }
    }
    excluded.sort_by(|a, b| a.name.cmp(&b.name));

    // Score: trigger-tag overlap, tie-broken by name. Name order is the
    // iteration order, so a stable descending sort is the whole tiebreak.
    eligible.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
    let task_tags: BTreeSet<&str> = features.task_tags.iter().map(String::as_str).collect();
    let mut ranking: Vec<RankedSkill> = eligible
        .iter()
        .map(|entry| {
            let mut matched: Vec<String> = entry
                .binding
                .trigger_tags
                .iter()
                .filter(|tag| task_tags.contains(tag.as_str()))
                .cloned()
                .collect();
            matched.sort();
            matched.dedup();
            RankedSkill {
                name: entry.metadata.name.clone(),
                revision: entry.metadata.revision,
                content_hash: entry.metadata.content_hash.clone(),
                score: matched.len() as u64 * 10_000,
                matched_tags: matched,
                metadata: metadata_line(entry),
            }
        })
        .collect();
    ranking.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));

    let selected = if catalog.len() > policy.cutoff {
        ranking.iter().take(policy.k).cloned().collect()
    } else {
        ranking.clone()
    };
    SkillSelection {
        selected,
        ranking,
        excluded,
    }
}

// --------------------------------------------------------------------- //
// Section assembly inputs (consuming context.rs's vocabulary exactly)
// --------------------------------------------------------------------- //

/// Build the skills section's entries: the shortlist as tier-1 metadata,
/// with tier-2 bodies attached for the skills the active set binds. An
/// active skill's entry pins the *bound* revision and content hash — the
/// learn pointer's choice — never the catalog's latest: the manifest's
/// `name@revision:hash` ids must name the version whose body the model
/// saw. Active skills absent from the shortlist still enter the section —
/// activation is the learn pointer's decision, not the shortlist's —
/// appended in name order after the ranked entries.
///
/// Output order is the section's carried order, which the pipeline's
/// manifest pins as `name@revision:hash` ids.
pub fn skill_section_entries(
    registry: &SkillRegistry,
    selection: &SkillSelection,
    active: &ActiveSkillSet,
) -> std::result::Result<Vec<SkillSectionEntry>, SkillsError> {
    let body_of = |name: &str, content_hash: &str| {
        registry
            .get_version(
                name,
                SkillVersionSelector::ContentHash(content_hash.to_owned()),
            )
            .map(|version| version.body().to_owned())
            .ok_or_else(|| SkillsError::Undisclosable {
                name: name.to_owned(),
                content_hash: content_hash.to_owned(),
            })
    };

    let mut entries = Vec::new();
    let mut carried: BTreeSet<&str> = BTreeSet::new();
    for ranked in &selection.selected {
        carried.insert(ranked.name.as_str());
        // An active skill's pin is the bound revision — the learn
        // pointer's choice, which the manifest must name: the body the
        // model saw is the pinned version's, never the catalog latest's.
        let (revision, content_hash, body) = match active.find(&ranked.name) {
            Some(active_skill) => (
                active_skill.revision.to_string(),
                active_skill.content_hash.clone(),
                Some(body_of(&active_skill.name, &active_skill.content_hash)?),
            ),
            None => (
                ranked.revision.to_string(),
                ranked.content_hash.clone(),
                None,
            ),
        };
        entries.push(SkillSectionEntry {
            name: ranked.name.clone(),
            revision,
            content_hash,
            metadata: ranked.metadata.clone(),
            body,
        });
    }
    for skill in active.skills() {
        if carried.contains(skill.name.as_str()) {
            continue;
        }
        let metadata = registry
            .get_version(
                &skill.name,
                SkillVersionSelector::ContentHash(skill.content_hash.clone()),
            )
            .map(|version| version.metadata().description)
            .ok_or_else(|| SkillsError::Undisclosable {
                name: skill.name.clone(),
                content_hash: skill.content_hash.clone(),
            })?;
        entries.push(SkillSectionEntry {
            name: skill.name.clone(),
            revision: skill.revision.to_string(),
            content_hash: skill.content_hash.clone(),
            metadata,
            body: Some(body_of(&skill.name, &skill.content_hash)?),
        });
    }
    Ok(entries)
}

// --------------------------------------------------------------------- //
// The gating Tool wrapper (S5): per-invocation active-skill narrowing
// --------------------------------------------------------------------- //

/// The decoded body of a [`SkillGateTool`] refusal: the tool refused, the
/// skills active at refusal time, and the declared union the call was
/// checked against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillToolGateRefusal {
    /// The tool whose call was refused.
    pub tool: String,
    /// The active skills' pins, name-sorted.
    pub active_skills: Vec<ActiveSkillPin>,
    /// The declared tool union the gate enforced (sorted).
    pub declared_tools: Vec<String>,
}

/// Build the structured gate refusal payload:
/// `ERROR: {"kind":"skill_tool_gate",…}` — compact JSON, key order pinned
/// by construction. The second kind of the outcome roll-up's two-tier
/// contract (N10), beside
/// [`crate::tool_select::argument_validation_refusal`]; the golden pins it
/// byte-for-byte.
pub fn skill_tool_gate_refusal(tool: &str, active: &ActiveSkillSet) -> String {
    let refusal = SkillToolGateRefusal {
        tool: tool.to_owned(),
        active_skills: active.skills().iter().map(ActiveSkill::pin).collect(),
        declared_tools: active.declared_tools().into_iter().collect(),
    };
    let body = json!({
        "kind": SKILL_TOOL_GATE_KIND,
        "tool": refusal.tool,
        "active_skills": refusal.active_skills,
        "declared_tools": refusal.declared_tools,
    });
    let compact = serde_json::to_string(&body)
        .expect("serializing a gate refusal body cannot fail: it is a plain JSON value");
    format!("{ERROR_PREFIX}{compact}")
}

/// The parse half of the contract: recognize a tool message (or journaled
/// output payload) as a [`SkillGateTool`] refusal and decode its body.
/// Returns `None` for every other `ERROR:` string — the roll-up's
/// opaque-error tier.
pub fn parse_skill_tool_gate_refusal(content: &str) -> Option<SkillToolGateRefusal> {
    let body = content.strip_prefix(ERROR_PREFIX)?;
    let value: Value = serde_json::from_str(body).ok()?;
    if value.get("kind")?.as_str()? != SKILL_TOOL_GATE_KIND {
        return None;
    }
    Some(SkillToolGateRefusal {
        tool: value.get("tool")?.as_str()?.to_owned(),
        active_skills: serde_json::from_value(value.get("active_skills")?.clone()).ok()?,
        declared_tools: serde_json::from_value(value.get("declared_tools")?.clone()).ok()?,
    })
}

/// A [`Tool`] wrapper that enforces active-skill tool narrowing (S5):
/// while one or more active skills declare tool sets, a call may reach
/// only the union of those sets. A refused call does NOT dispatch: the
/// wrapper returns the structured refusal payload as the tool's result,
/// so the failure-isolation channel carries the byte-exact contract
/// string and the refusal journals as an ordinary `ToolCall` under the
/// `RecordingTool` pattern — the evidenced-enforcement rule.
///
/// The active set is per-invocation state: the wrapper reads the shared
/// [`ActiveSkills`] handle at call time, and the assembly driver updates
/// the handle as each assembly binds skills. This is deliberately not the
/// run's static allowlist — the executor writes `TOOL_ALLOWLIST_KEY` once
/// per run, and dispatch under it succeeds regardless of which skill is
/// active this invocation.
///
/// Identity — name, description, schema, effect class, effect kind,
/// idempotency key, and [`Tool::effect_request`] — delegates to the
/// wrapped tool, so the wrapper is transparent to the admission boundary
/// (the `tool.rs` rule for wrappers).
pub struct SkillGateTool {
    inner: Arc<dyn Tool>,
    active: ActiveSkills,
}

impl SkillGateTool {
    /// A gating wrapper around `inner`, reading `active` at call time.
    pub fn new(inner: Arc<dyn Tool>, active: ActiveSkills) -> Self {
        Self { inner, active }
    }

    /// The wrapped tool.
    pub fn inner(&self) -> &Arc<dyn Tool> {
        &self.inner
    }

    /// The handle this wrapper reads.
    pub fn active(&self) -> &ActiveSkills {
        &self.active
    }

    /// Clone `registry` with every tool gated on `active` — the
    /// registration recipe beside
    /// [`crate::tool_select::ValidatingTool::wrap_registry`]; compose the
    /// two wrappers for validation AND skill narrowing over the narrowed
    /// registry.
    pub fn wrap_registry(
        registry: &crate::tool::ToolRegistry,
        active: ActiveSkills,
    ) -> crate::tool::ToolRegistry {
        let mut names: Vec<&str> = registry.names().collect();
        names.sort_unstable();
        let mut wrapped = crate::tool::ToolRegistry::new();
        for name in names {
            let tool = registry.get(name).expect("name came from the registry");
            wrapped.register_shared(Arc::new(Self::new(tool, active.clone())));
        }
        wrapped
    }
}

#[async_trait]
impl Tool for SkillGateTool {
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
        let active = self.active.snapshot();
        if !active.permits(self.inner.name()) {
            return Ok(Value::String(skill_tool_gate_refusal(
                self.inner.name(),
                &active,
            )));
        }
        self.inner.call(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{SkillPackage, SkillSource};

    fn skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
    }

    fn register(registry: &mut SkillRegistry, name: &str, body: &str) -> SkillMetadata {
        let package =
            SkillPackage::from_markdown(&skill_md(name, &format!("The {name} skill."), body))
                .expect("valid package");
        registry
            .register(
                package,
                SkillSource::LocalPath {
                    path: "/skills/test".to_owned(),
                },
                "tests",
            )
            .expect("registers")
            .version
            .metadata()
    }

    fn catalog_entry(metadata: SkillMetadata, tags: &[&str], tools: &[&str]) -> SkillCatalogEntry {
        SkillCatalogEntry {
            metadata,
            binding: SkillBinding {
                trigger_tags: tags.iter().map(|t| t.to_string()).collect(),
                tools: tools.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn sparse_binding_stays_absent_from_the_wire() {
        let binding = SkillBinding {
            trigger_tags: vec!["web".into()],
            ..Default::default()
        };
        let wire = serde_json::to_string(&binding).unwrap();
        assert_eq!(wire, "{\"trigger_tags\":[\"web\"]}");
        let parsed: SkillBinding = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed, binding);
    }

    #[test]
    fn binding_validation_is_fail_closed() {
        let base = SkillBinding::default();
        assert!(base.validate().is_ok());

        let mut bad = base.clone();
        bad.trigger_tags = vec![String::new()];
        assert!(bad.validate().is_err());

        let mut bad = base.clone();
        bad.task_shape = Some("  padded".into());
        assert!(bad.validate().is_err());

        let mut bad = base.clone();
        bad.tools = vec!["not a tool".into()];
        assert!(bad.validate().is_err());
    }

    #[test]
    fn selection_is_deterministic_regardless_of_input_order() {
        let mut registry = SkillRegistry::new();
        let a = register(&mut registry, "alpha", "Alpha body.");
        let b = register(&mut registry, "beta", "Beta body.");
        let c = register(&mut registry, "gamma", "Gamma body.");
        let catalog = vec![
            catalog_entry(a.clone(), &["web", "search"], &[]),
            catalog_entry(b.clone(), &["search"], &[]),
            catalog_entry(c.clone(), &[], &[]),
        ];
        let features = SkillSelectionFeatures {
            task_tags: vec!["search".into()],
            available_tools: None,
        };
        let policy = SkillSelectionPolicy { cutoff: 20, k: 5 };

        let first = select_skills(&features, &catalog, &policy);
        let mut shuffled = catalog.clone();
        shuffled.reverse();
        let second = select_skills(&features, &shuffled, &policy);
        assert_eq!(first, second);

        // beta and alpha matched `search` once each; name order breaks the
        // tie; gamma scored zero.
        let order: Vec<&str> = first.ranking.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, vec!["alpha", "beta", "gamma"]);
        assert_eq!(first.ranking[0].matched_tags, vec!["search".to_string()]);
    }

    #[test]
    fn cutoff_engages_the_top_k() {
        let mut registry = SkillRegistry::new();
        let catalog: Vec<SkillCatalogEntry> = (0..6)
            .map(|i| {
                let name = format!("skill-{i}");
                let metadata = register(&mut registry, &name, "Body.");
                catalog_entry(metadata, if i % 2 == 0 { &["even"] } else { &[] }, &[])
            })
            .collect();
        let features = SkillSelectionFeatures {
            task_tags: vec!["even".into()],
            available_tools: None,
        };
        let policy = SkillSelectionPolicy { cutoff: 3, k: 2 };
        let outcome = select_skills(&features, &catalog, &policy);
        assert_eq!(outcome.selected.len(), 2);
        assert_eq!(outcome.ranking.len(), 6);
        // Below the cutoff, selection is identity.
        let identity = select_skills(
            &features,
            &catalog[..3],
            &SkillSelectionPolicy { cutoff: 3, k: 2 },
        );
        assert_eq!(identity.selected.len(), 3);
    }

    #[test]
    fn unavailable_tools_exclude_before_scoring() {
        let mut registry = SkillRegistry::new();
        let metadata = register(&mut registry, "deployer", "Deploys.");
        let catalog = vec![catalog_entry(
            metadata,
            &["deploy"],
            &["cloud.deploy", "cloud.logs"],
        )];
        let features = SkillSelectionFeatures {
            task_tags: vec!["deploy".into()],
            available_tools: Some(BTreeSet::from(["cloud.deploy".to_string()])),
        };
        let outcome = select_skills(&features, &catalog, &SkillSelectionPolicy::default());
        assert!(outcome.selected.is_empty());
        assert_eq!(outcome.excluded.len(), 1);
        match &outcome.excluded[0].reason {
            SkillExclusionReason::UnavailableTools { missing } => {
                assert_eq!(missing, &vec!["cloud.logs".to_string()]);
            }
        }
        // No narrowing information: the gate is skipped, never guessed.
        let open = SkillSelectionFeatures {
            available_tools: None,
            ..features.clone()
        };
        let outcome = select_skills(&open, &catalog, &SkillSelectionPolicy::default());
        assert_eq!(outcome.selected.len(), 1);
    }

    #[test]
    fn active_set_permits_by_declared_union() {
        let set = ActiveSkillSet::new(vec![
            ActiveSkill {
                name: "alpha".into(),
                revision: 1,
                content_hash: "h1".into(),
                tools: vec!["web.search".into()],
            },
            ActiveSkill {
                name: "beta".into(),
                revision: 2,
                content_hash: "h2".into(),
                tools: vec!["http.get".into()],
            },
        ]);
        assert!(set.permits("web.search"));
        assert!(set.permits("http.get"));
        assert!(!set.permits("email.send"));

        // No declarations anywhere: nothing is narrowed.
        let open = ActiveSkillSet::new(vec![ActiveSkill {
            name: "alpha".into(),
            revision: 1,
            content_hash: "h1".into(),
            tools: vec![],
        }]);
        assert!(open.permits("anything.at.all"));

        // The empty set narrows nothing.
        assert!(ActiveSkillSet::default().permits("anything.at.all"));
    }

    #[test]
    fn section_entries_carry_bodies_only_for_active_skills() {
        let mut registry = SkillRegistry::new();
        let a = register(&mut registry, "alpha", "Alpha instructions.");
        let b = register(&mut registry, "beta", "Beta instructions.");
        let catalog = vec![
            catalog_entry(a.clone(), &["web"], &[]),
            catalog_entry(b.clone(), &[], &[]),
        ];
        let features = SkillSelectionFeatures {
            task_tags: vec!["web".into()],
            available_tools: None,
        };
        let selection = select_skills(&features, &catalog, &SkillSelectionPolicy::default());
        let active = ActiveSkillSet::new(vec![ActiveSkill {
            name: "beta".into(),
            revision: 1,
            content_hash: b.content_hash.clone(),
            tools: vec![],
        }]);
        let entries = skill_section_entries(&registry, &selection, &active).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].body.is_none(),
            "alpha is shortlisted, not active"
        );
        assert_eq!(entries[1].body.as_deref(), Some("Beta instructions.\n"));
        assert_eq!(entries[1].revision, "1");
    }

    #[test]
    fn active_skill_absent_from_the_shortlist_still_enters_the_section() {
        let mut registry = SkillRegistry::new();
        let a = register(&mut registry, "alpha", "Alpha instructions.");
        let b = register(&mut registry, "beta", "Beta instructions.");
        // Only alpha is shortlisted (beta scores below the cut).
        let catalog = vec![
            catalog_entry(a.clone(), &["web"], &[]),
            catalog_entry(b.clone(), &[], &[]),
        ];
        let features = SkillSelectionFeatures {
            task_tags: vec!["web".into()],
            available_tools: None,
        };
        let selection = select_skills(
            &features,
            &catalog,
            &SkillSelectionPolicy { cutoff: 1, k: 1 },
        );
        assert_eq!(selection.selected.len(), 1);
        let active = ActiveSkillSet::new(vec![ActiveSkill {
            name: "beta".into(),
            revision: 1,
            content_hash: b.content_hash.clone(),
            tools: vec![],
        }]);
        let entries = skill_section_entries(&registry, &selection, &active).unwrap();
        assert_eq!(entries.len(), 2, "the active skill joins the section");
        assert_eq!(entries[1].name, "beta");
        assert!(entries[1].body.is_some());
    }

    #[test]
    fn refusal_payload_round_trips() {
        let set = ActiveSkillSet::new(vec![ActiveSkill {
            name: "alpha".into(),
            revision: 3,
            content_hash: "abc123".into(),
            tools: vec!["web.search".into()],
        }]);
        let payload = skill_tool_gate_refusal("email.send", &set);
        assert!(payload.starts_with(ERROR_PREFIX));
        let parsed = parse_skill_tool_gate_refusal(&payload).unwrap();
        assert_eq!(parsed.tool, "email.send");
        assert_eq!(parsed.declared_tools, vec!["web.search".to_string()]);
        assert_eq!(parsed.active_skills[0].name, "alpha");

        // Other ERROR strings stay opaque — the roll-up's second tier.
        assert!(parse_skill_tool_gate_refusal("ERROR: boom").is_none());
        let argument_refusal = crate::tool_select::argument_validation_refusal(&[]);
        assert!(parse_skill_tool_gate_refusal(&argument_refusal).is_none());
    }

    #[test]
    fn pointer_resolution_binds_the_pinned_revision() {
        use crate::learn::{CandidateId, SurfaceKey, VersionPointer};

        let mut registry = SkillRegistry::new();
        let v1 = register(&mut registry, "alpha", "Revision one.");
        let v2 = register(&mut registry, "alpha", "Revision two, improved.");

        let pin_v2 = SkillPin {
            name: "alpha".into(),
            content_hash: v2.content_hash.clone(),
            binding: SkillBinding::default(),
        };
        let candidate: CandidateId = CandidateId::from("cand-alpha-2".to_string());
        let pin_fn = |id: &CandidateId| {
            (id == &candidate).then(|| SkillPin {
                name: pin_v2.name.clone(),
                content_hash: pin_v2.content_hash.clone(),
                binding: pin_v2.binding.clone(),
            })
        };

        // Nothing promoted: the skill does not bind — no latest-pointer
        // fallback.
        let pointer = VersionPointer::new(SurfaceKey::new("skill:alpha"));
        assert!(resolve_active_skill(&registry, &pointer, "run-1", &pin_fn)
            .unwrap()
            .is_none());

        // Promoted: the pinned revision binds, not the registry's latest
        // (they coincide here; the pin is what decides).
        let mut promoted = VersionPointer::new(SurfaceKey::new("skill:alpha"));
        promoted.active = Some(candidate.clone());
        let active = resolve_active_skill(&registry, &promoted, "run-1", &pin_fn)
            .unwrap()
            .unwrap();
        assert_eq!(active.revision, v2.revision);
        assert_eq!(active.content_hash, v2.content_hash);

        // A pin the registry does not hold fails closed.
        let ghost = SkillPin {
            name: "alpha".into(),
            content_hash: "0".repeat(64),
            binding: SkillBinding::default(),
        };
        let ghost_id = CandidateId::from("cand-ghost".to_string());
        let mut pointer = VersionPointer::new(SurfaceKey::new("skill:alpha"));
        pointer.active = Some(ghost_id.clone());
        let err = resolve_active_skill(&registry, &pointer, "run-1", &|id| {
            (id == &ghost_id).then(|| SkillPin {
                name: ghost.name.clone(),
                content_hash: ghost.content_hash.clone(),
                binding: ghost.binding.clone(),
            })
        })
        .unwrap_err();
        assert!(matches!(err, SkillsError::VersionNotRegistered { .. }));

        // A surface mismatch fails closed.
        let mut wrong = VersionPointer::new(SurfaceKey::new("skill:beta"));
        wrong.active = Some(candidate.clone());
        let err = resolve_active_skill(&registry, &wrong, "run-1", &pin_fn).unwrap_err();
        assert!(matches!(err, SkillsError::SurfaceMismatch { .. }));
        // The promote/rollback divergence case (two revisions, byte-exact
        // rebinding) lives in tests/skills_run.rs's
        // `promotion_and_rollback_change_the_bound_revision_byte_exactly`.
        let _ = v1;
    }
}
