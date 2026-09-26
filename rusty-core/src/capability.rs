//! Resolved capability sets: the immutable, content-addressed composition
//! one agent version declares and one run resolves at admission.
//!
//! A [`CapabilitySet`] names exact members — tool names today, plus
//! forward-compatible skill references — and derives its identity
//! (the set id) from the canonical serialization of those members, so two
//! compositions are the same set if and only if they name the same members.
//! The empty set is legitimate: it describes a deliberately tool-free
//! agent.
//!
//! The run-admission contract has three parts:
//!
//! - **Composition** ([`CapabilitySet::compose`]) validates every tool
//!   member against the graph's executable catalog and fails closed on
//!   unknown or duplicate names — a configuration typo can never silently
//!   broaden or ambiguously describe what a run may call.
//! - **Resolution** ([`CapabilitySet::resolve_allowlist`]) produces the
//!   exact `tool_allowlist` vector the executor already consumes; the set
//!   id pins into [`crate::record::RunManifest`] alongside the prompt,
//!   tool-schema, and model pins.
//! - **Replay** ([`CapabilitySet::replay_guard`]) re-resolves the pinned
//!   set against the current registry: a member the registry no longer
//!   contains fails with a typed [`RustyError::Replay`] instead of
//!   silently widening or narrowing the replayed run.
//!
//! Skill members are opaque references with kind tags. Their plane lands
//! separately; the set records them verbatim today so the content address
//! already covers them, and validation against the skill registry arrives
//! with that plane.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::sync::Arc;

use crate::error::{Result, RustyError};
use crate::record::{ApprovalRequest, Effect};
use crate::tool::approval::{ask_detail, ApprovalAnswerer, ApprovalGate};
use crate::tool::{GuardDenial, GuardedCall, ToolCapability, ToolGuard, ToolRegistry};

/// The set id prefix: a capability set id is `cs-` followed by the
/// lowercase hex SHA-256 of the set's canonical member serialization —
/// the same digest convention every [`crate::record::RunManifest`] pin
/// follows.
pub const CAPABILITY_SET_ID_PREFIX: &str = "cs-";

/// Maximum length of one opaque skill reference.
pub const MAX_CAPABILITY_REF_BYTES: usize = 256;

/// Which capability plane a [`CapabilityRef`] belongs to.
///
/// Tools are not referenced through this type: they are the execution
/// plane this crate already owns, so a set names them directly. Only the
/// planes this module does not interpret — skills today — ride as opaque
/// references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRefKind {
    /// A versioned skill package (procedural knowledge; the skill plane
    /// owns interpretation).
    Skill,
}

impl CapabilityRefKind {
    /// The stable wire tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityRefKind::Skill => "skill",
        }
    }
}

/// An opaque, kind-tagged reference to a skill plane member.
///
/// The reference string is interpreted only by the owning plane; this
/// module checks shape (non-empty, trimmed, control-free, bounded) and
/// otherwise records it verbatim so the set's content address covers it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CapabilityRef {
    /// The plane that interprets `reference`.
    pub kind: CapabilityRefKind,
    /// Opaque plane-specific reference (a skill package id).
    pub reference: String,
}

impl CapabilityRef {
    /// A reference of `kind` to `reference`, shape-checked.
    pub fn new(kind: CapabilityRefKind, reference: impl Into<String>) -> Result<Self> {
        let reference = reference.into();
        if reference.is_empty()
            || reference != reference.trim()
            || reference.len() > MAX_CAPABILITY_REF_BYTES
            || reference.chars().any(char::is_control)
        {
            return Err(RustyError::Tool(format!(
                "{} reference must be non-empty, trimmed, control-free, and at most {MAX_CAPABILITY_REF_BYTES} bytes",
                kind.as_str()
            )));
        }
        Ok(Self { kind, reference })
    }

    /// A skill plane reference.
    pub fn skill(reference: impl Into<String>) -> Result<Self> {
        Self::new(CapabilityRefKind::Skill, reference)
    }
}

/// The immutable, content-addressed capability composition of one agent
/// version, resolved exactly for one run.
///
/// Members are normalized at construction (sorted, duplicates refused), so
/// the set id is the only identity a set needs: it is computed, never
/// claimed. Serialization carries the members only — the id is recomputed
/// on deserialization, so a tampered or stale address cannot survive a
/// round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitySet {
    /// Exact tool names, sorted.
    tools: Vec<String>,
    /// Skill references, sorted by (kind, reference).
    refs: Vec<CapabilityRef>,
    /// `cs-` + SHA-256 of the canonical member serialization.
    id: String,
}

/// The wire shape: members only. The id is derived, never transported.
#[derive(Serialize, Deserialize)]
struct CapabilitySetBody {
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    refs: Vec<CapabilityRef>,
}

impl Serialize for CapabilitySet {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        CapabilitySetBody {
            tools: self.tools.clone(),
            refs: self.refs.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CapabilitySet {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let body = CapabilitySetBody::deserialize(deserializer)?;
        Self::from_members(&body.tools, &body.refs).map_err(serde::de::Error::custom)
    }
}

impl CapabilitySet {
    /// Compose and validate a set against the executable catalog.
    ///
    /// Every tool member must appear in `catalog`; unknown names fail
    /// closed. Skill references are shape-checked and recorded
    /// verbatim — their planes validate them when those planes land. The
    /// empty composition is legitimate (a tool-free agent).
    pub fn compose(
        tools: &[String],
        refs: &[CapabilityRef],
        catalog: &[ToolCapability],
    ) -> Result<Self> {
        let set = Self::from_members(tools, refs)?;
        set.validate_against_catalog(catalog)?;
        Ok(set)
    }

    /// Normalize members and compute the content address, without catalog
    /// validation. Catalog checks are a separate step
    /// ([`CapabilitySet::validate_against`],
    /// [`CapabilitySet::validate_against_catalog`]) so stored sets can be
    /// read back and re-validated against *today's* registry — the replay
    /// contract depends on that distinction.
    pub fn from_members(tools: &[String], refs: &[CapabilityRef]) -> Result<Self> {
        let mut tools = tools.to_vec();
        tools.sort_unstable();
        for pair in tools.windows(2) {
            if pair[0] == pair[1] {
                return Err(RustyError::Tool(format!(
                    "capability set contains duplicate tool `{}`",
                    pair[0]
                )));
            }
        }
        let mut refs = refs.to_vec();
        refs.sort();
        for pair in refs.windows(2) {
            if pair[0] == pair[1] {
                return Err(RustyError::Tool(format!(
                    "capability set contains duplicate {} reference `{}`",
                    pair[0].kind.as_str(),
                    pair[0].reference
                )));
            }
        }
        let id = set_id(&tools, &refs);
        Ok(Self { tools, refs, id })
    }

    /// The set's content address (`cs-` + lowercase hex SHA-256).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The exact tool members, sorted.
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// The skill references, sorted by (kind, reference).
    pub fn refs(&self) -> &[CapabilityRef] {
        &self.refs
    }

    /// `true` when the set names no members at all — a deliberately
    /// tool-free agent.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.refs.is_empty()
    }

    /// Resolve the set into the exact tool allowlist the executor consumes
    /// ([`crate::executor::RunConfig::tool_allowlist`]). Skill
    /// members never widen the tool plane.
    pub fn resolve_allowlist(&self) -> Vec<String> {
        self.tools.clone()
    }

    /// Validate every tool member against the live registry, failing closed
    /// on a name the registry does not contain. Admission-time entry point
    /// when the caller holds the executable registry rather than its
    /// derived catalog.
    pub fn validate_against(&self, registry: &ToolRegistry) -> Result<()> {
        for name in &self.tools {
            if !registry.contains(name) {
                return Err(RustyError::Tool(format!(
                    "capability set `{}` names tool `{name}`, which is not registered",
                    self.id
                )));
            }
        }
        Ok(())
    }

    /// Validate every tool member against the derived catalog (the same
    /// check as [`CapabilitySet::validate_against`], for callers that hold
    /// the advertised catalog rather than the registry).
    pub fn validate_against_catalog(&self, catalog: &[ToolCapability]) -> Result<()> {
        for name in &self.tools {
            if !catalog.iter().any(|tool| &tool.name == name) {
                return Err(RustyError::Tool(format!(
                    "capability set `{}` names tool `{name}`, which the catalog does not advertise",
                    self.id
                )));
            }
        }
        Ok(())
    }

    /// Resolve the set a delegated (child) run starts from: the exact
    /// intersection of the parent's resolved set and the child's declared
    /// set.
    ///
    /// Inheritance can narrow but never widen: a member the parent does
    /// not hold is dropped, even when the child declares it. The resolved
    /// set is content-addressed anew — its id honestly differs from the
    /// child's declared id whenever inheritance cut something, which is
    /// exactly the evidence a child run's manifest should pin.
    pub fn intersect_for_child(parent: &CapabilitySet, declared: &CapabilitySet) -> CapabilitySet {
        let tools: Vec<String> = declared
            .tools
            .iter()
            .filter(|name| parent.tools.binary_search(name).is_ok())
            .cloned()
            .collect();
        let refs: Vec<CapabilityRef> = declared
            .refs
            .iter()
            .filter(|reference| parent.refs.binary_search(reference).is_ok())
            .cloned()
            .collect();
        Self::from_members(&tools, &refs).expect("the intersection of two valid sets is valid")
    }

    /// The replay binding: re-resolve a pinned set against the current
    /// registry before a replayed run is allowed to proceed.
    ///
    /// Replay must reproduce the same set id, which it can only do by
    /// resolving the same members. A registry that no longer contains a
    /// member makes that impossible, so the replay fails with a typed
    /// [`RustyError::Replay`] naming the missing member — it never
    /// silently narrows (drops the member) or widens (ignores the set).
    /// Skill references have no registry to check against yet;
    /// the contract extends to them when their planes land.
    pub fn replay_guard(&self, registry: &ToolRegistry) -> Result<()> {
        for name in &self.tools {
            if !registry.contains(name) {
                return Err(self.replay_refusal(name));
            }
        }
        Ok(())
    }

    /// The replay binding against the derived catalog — the same contract
    /// as [`CapabilitySet::replay_guard`] for callers (the server's replay
    /// endpoint) that hold the advertised catalog rather than the live
    /// registry.
    pub fn replay_guard_catalog(&self, catalog: &[ToolCapability]) -> Result<()> {
        for name in &self.tools {
            if !catalog.iter().any(|tool| &tool.name == name) {
                return Err(self.replay_refusal(name));
            }
        }
        Ok(())
    }

    /// The typed refusal both replay guards share.
    fn replay_refusal(&self, name: &str) -> RustyError {
        RustyError::Replay(format!(
            "capability set `{}` names tool `{name}`, which the current registry no \
             longer contains; replay refused rather than resolving a different set",
            self.id
        ))
    }
}

/// The content address of a member list: `cs-` + SHA-256 over the
/// canonical `serde_json` serialization of `{"refs": …, "tools": …}` —
/// the same canonicalization ([`crate::record::canonicalize_value`]) every
/// manifest pin relies on, so object key order can never fork an address.
fn set_id(tools: &[String], refs: &[CapabilityRef]) -> String {
    let body = serde_json::json!({
        "refs": refs,
        "tools": tools,
    });
    let canonical: Value = crate::record::canonicalize_value(&body);
    let bytes = serde_json::to_vec(&canonical).expect("a serde_json::Value always serializes");
    format!(
        "{CAPABILITY_SET_ID_PREFIX}{}",
        crate::record::sha256_hex(&bytes)
    )
}

// ---------- permission presets (parity wave) ----------

/// The builtin CLI tool's stable name
/// ([`crate::tool::builtins::cli::CliTool`]). Named here rather than reached
/// for: the preset plane stays data over the tool plane — the CLI mode guard
/// matches the name, it does not import the tool.
pub const CLI_TOOL_NAME: &str = "run_cli";

/// A named, closed permission preset (parity wave): one word that bundles a
/// guard set, an approval posture, and a CLI policy mode into a permission
/// stance an embedder can apply to a run.
///
/// A preset is *data resolving into the existing seams* — the deny-only
/// guard layer ([`crate::tool::ToolGuard`]), the closed approval vocabulary
/// ([`crate::record::ApprovalDecision`]), and the builtin CLI tool's own policy — never a
/// parallel permission system. The variants form a restrictiveness ladder
/// (declaration order, made mechanical by the `Ord` derive), so composition
/// is the intersection, never the union: [`PermissionPreset::intersect`] is
/// `min`, and two presets can only narrow what either would permit alone.
///
/// Wire names are stable snake_case (`"read_only"`, `"workspace_ask"`,
/// `"workspace"`, `"full_access"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPreset {
    /// Everything above [`Effect::ReadOnly`] is unreachable, and the CLI
    /// tool is disabled outright.
    ReadOnly,

    /// Workspace writes proceed; irreversible effects
    /// ([`Effect::is_freely_repeatable`] is `false`) ask in the closed
    /// approval vocabulary — and with no answerer wired, every ask denies
    /// (fail closed). The CLI tool runs argv spawns only.
    WorkspaceAsk,

    /// Workspace writes and irreversible effects proceed without an ask
    /// (auto-allow-once). The CLI tool runs argv spawns only.
    Workspace,

    /// No guards at all: every call the allowlist admitted may run, and the
    /// CLI tool may take shell payloads. The builtin's own containment — the
    /// jail, the output ceilings, the timeout — still applies; no preset
    /// lifts the tool's floor.
    FullAccess,
}

impl PermissionPreset {
    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionPreset::ReadOnly => "read_only",
            PermissionPreset::WorkspaceAsk => "workspace_ask",
            PermissionPreset::Workspace => "workspace",
            PermissionPreset::FullAccess => "full_access",
        }
    }

    /// The effect ceiling, when the preset has one: calls declaring a higher
    /// class than this are unreachable.
    pub fn effect_ceiling(self) -> Option<Effect> {
        match self {
            PermissionPreset::ReadOnly => Some(Effect::ReadOnly),
            _ => None,
        }
    }

    /// The CLI policy mode the preset permits (see [`CliPolicyMode`]).
    pub fn cli_mode(self) -> CliPolicyMode {
        match self {
            PermissionPreset::ReadOnly => CliPolicyMode::Disabled,
            PermissionPreset::WorkspaceAsk | PermissionPreset::Workspace => CliPolicyMode::Jailed,
            PermissionPreset::FullAccess => CliPolicyMode::Shell,
        }
    }

    /// How effects above the freely repeatable ladder rungs are decided (see
    /// [`ApprovalPosture`]).
    pub fn posture(self) -> ApprovalPosture {
        match self {
            PermissionPreset::ReadOnly => ApprovalPosture::AutoDeny,
            PermissionPreset::WorkspaceAsk => ApprovalPosture::Ask,
            PermissionPreset::Workspace | PermissionPreset::FullAccess => {
                ApprovalPosture::AllowOnce
            }
        }
    }

    /// The restrictive composition of two presets: the intersection, never
    /// the union. The ladder makes it one line — the more restrictive of the
    /// pair — so no combination of presets can widen access.
    pub fn intersect(self, other: PermissionPreset) -> PermissionPreset {
        self.min(other)
    }

    /// Resolve the preset into its concrete admission bundle with no ask
    /// answerer wired: under `workspace_ask` every ask denies (fail closed).
    /// Use [`PermissionPreset::resolve_with`] to wire the decision source.
    pub fn resolve(self) -> PresetResolution {
        self.resolve_with(None, None)
    }

    /// Resolve the preset into its concrete admission bundle: the guard set
    /// to register on the run, the CLI policy mode, and the approval
    /// posture. `answerer` is the decision source for the ask posture;
    /// `gate` journals each asked/decided pair it answers.
    pub fn resolve_with(
        self,
        answerer: Option<ApprovalAnswerer>,
        gate: Option<ApprovalGate>,
    ) -> PresetResolution {
        let mut guards: Vec<Arc<dyn ToolGuard>> = Vec::new();
        if let Some(ceiling) = self.effect_ceiling() {
            guards.push(Arc::new(EffectCeilingGuard {
                preset: self,
                ceiling,
            }));
        }
        let cli_mode = self.cli_mode();
        if cli_mode != CliPolicyMode::Shell {
            guards.push(Arc::new(CliModeGuard { mode: cli_mode }));
        }
        if self.posture() == ApprovalPosture::Ask {
            guards.push(Arc::new(PresetAskGuard {
                preset: self,
                answerer,
                gate,
            }));
        }
        PresetResolution {
            preset: self,
            cli_mode,
            posture: self.posture(),
            guards,
        }
    }
}

/// How far a preset lets the builtin CLI tool go (parity wave). Declaration
/// order is the permissiveness ladder, so [`PresetResolution::intersect`]
/// takes `min`.
///
/// A mode can only narrow what [`crate::tool::builtins::cli::CliPolicy`]
/// already enforces — the jail, the output ceilings, and the timeout apply
/// in every mode, `Shell` included. The preset layer chooses which
/// *invocation shapes* may reach the tool; the tool's own containment is
/// the floor no preset lifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliPolicyMode {
    /// `run_cli` is unreachable.
    Disabled,
    /// Only policies declared read-only may run (the tool must report
    /// [`Effect::ReadOnly`]).
    ReadOnly,
    /// Argv spawns only — a raw `command` shell payload is refused.
    Jailed,
    /// Shell payloads are permitted (still jailed, ceilinged, timed out).
    Shell,
}

impl CliPolicyMode {
    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            CliPolicyMode::Disabled => "disabled",
            CliPolicyMode::ReadOnly => "read_only",
            CliPolicyMode::Jailed => "jailed",
            CliPolicyMode::Shell => "shell",
        }
    }
}

/// Which effects require an ask versus auto-deny versus auto-allow-once
/// (parity wave). The posture applies above the freely repeatable ladder
/// ([`Effect::is_freely_repeatable`]); repeatable effects never ask under
/// any posture. Declaration order is the restrictiveness ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPosture {
    /// Nothing above the ceiling will ever be granted (the read-only
    /// stance: the ceiling guard has already made the ask unreachable).
    AutoDeny,
    /// Each occurrence asks in the closed approval vocabulary; with no
    /// answerer wired the ask denies — fail closed.
    Ask,
    /// Each occurrence proceeds without an ask (auto-allow-once).
    AllowOnce,
}

impl ApprovalPosture {
    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalPosture::AutoDeny => "auto_deny",
            ApprovalPosture::Ask => "ask",
            ApprovalPosture::AllowOnce => "allow_once",
        }
    }
}

/// A preset resolved into the admission seams (parity wave): the bundle of
/// data plus the materialized guard set. Not a parallel permission system —
/// the guards plug into [`crate::tool::ToolGuard`], the posture speaks the
/// closed [`crate::record::ApprovalDecision`] vocabulary, and the CLI mode narrows the
/// builtin tool's own policy.
#[derive(Debug)]
pub struct PresetResolution {
    preset: PermissionPreset,
    cli_mode: CliPolicyMode,
    posture: ApprovalPosture,
    guards: Vec<Arc<dyn ToolGuard>>,
}

impl PresetResolution {
    /// The most restrictive preset that contributed to this resolution.
    pub fn preset(&self) -> PermissionPreset {
        self.preset
    }

    /// The resolved CLI policy mode.
    pub fn cli_mode(&self) -> CliPolicyMode {
        self.cli_mode
    }

    /// The resolved approval posture.
    pub fn posture(&self) -> ApprovalPosture {
        self.posture
    }

    /// The materialized guard set, ready for
    /// [`crate::executor::RunConfig::with_tool_guards`].
    pub fn guards(&self) -> &[Arc<dyn ToolGuard>] {
        &self.guards
    }

    /// Consume the resolution into its guard set.
    pub fn into_guards(self) -> Vec<Arc<dyn ToolGuard>> {
        self.guards
    }

    /// The restrictive composition of two resolutions: the union of the
    /// guard sets — guards compose as any-denial-denies, so the union can
    /// only narrow — and the more restrictive preset, CLI mode, and posture
    /// of the pair.
    pub fn intersect(left: PresetResolution, right: PresetResolution) -> PresetResolution {
        let mut guards = left.guards;
        guards.extend(right.guards);
        PresetResolution {
            preset: left.preset.min(right.preset),
            cli_mode: left.cli_mode.min(right.cli_mode),
            posture: left.posture.min(right.posture),
            guards,
        }
    }
}

/// Run wiring for presets (parity wave): apply a resolved preset's guard
/// set to a [`crate::executor::RunConfig`] through the existing
/// [`crate::executor::RunConfig::with_tool_guards`] seam.
///
/// An extension trait rather than a `RunConfig` field: the guard seam is
/// deliberately the whole surface a preset needs, and preset guards
/// *append* to any guards the config already carries — which the seam's
/// monotonicity (deny-only, every guard evaluated) turns into the
/// restrictive composition for free.
pub trait RunConfigPresetExt {
    /// Apply `preset`'s guard set to the run config. Under `workspace_ask`
    /// every ask denies (fail closed); use
    /// [`RunConfigPresetExt::with_permission_preset_answered`] to wire a
    /// decision source.
    fn with_permission_preset(self, preset: PermissionPreset) -> Self;

    /// Apply `preset`'s guard set with the ask answerer (and optionally the
    /// journaling gate) for the ask posture.
    fn with_permission_preset_answered(
        self,
        preset: PermissionPreset,
        answerer: ApprovalAnswerer,
        gate: Option<ApprovalGate>,
    ) -> Self;
}

impl RunConfigPresetExt for crate::executor::RunConfig {
    fn with_permission_preset(mut self, preset: PermissionPreset) -> Self {
        self.tool_guards.extend(preset.resolve().into_guards());
        self
    }

    fn with_permission_preset_answered(
        mut self,
        preset: PermissionPreset,
        answerer: ApprovalAnswerer,
        gate: Option<ApprovalGate>,
    ) -> Self {
        self.tool_guards
            .extend(preset.resolve_with(Some(answerer), gate).into_guards());
        self
    }
}

/// Denies calls whose declared effect class exceeds the preset's ceiling.
#[derive(Debug)]
struct EffectCeilingGuard {
    preset: PermissionPreset,
    ceiling: Effect,
}

impl ToolGuard for EffectCeilingGuard {
    fn name(&self) -> &str {
        "preset_effect_ceiling"
    }

    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        (call.effect > self.ceiling).then(|| {
            GuardDenial::new(
                self.name(),
                format!(
                    "preset `{}` admits effects up to `{}`; `{}` declares `{}`",
                    self.preset.as_str(),
                    effect_name(self.ceiling),
                    call.tool,
                    effect_name(call.effect),
                ),
            )
        })
    }
}

/// Narrows the builtin CLI tool to the preset's mode. Other tools are not
/// this guard's business.
#[derive(Debug)]
struct CliModeGuard {
    mode: CliPolicyMode,
}

impl ToolGuard for CliModeGuard {
    fn name(&self) -> &str {
        "preset_cli_mode"
    }

    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        if call.tool != CLI_TOOL_NAME {
            return None;
        }
        let reason = match self.mode {
            CliPolicyMode::Disabled => {
                Some("preset cli mode `disabled` refuses `run_cli` outright".to_owned())
            }
            CliPolicyMode::ReadOnly if call.effect != Effect::ReadOnly => Some(format!(
                "preset cli mode `read_only` admits only read-only cli policies; this `run_cli` declares `{}`",
                effect_name(call.effect),
            )),
            CliPolicyMode::Jailed if call.arguments.get("command").is_some() => Some(
                "preset cli mode `jailed` refuses shell payloads; spawn argv directly".to_owned(),
            ),
            _ => None,
        };
        reason.map(|reason| GuardDenial::new(self.name(), reason))
    }
}

/// The ask stance: freely repeatable effects proceed; anything above asks
/// in the closed approval vocabulary. With no answerer wired the ask
/// denies — fail closed by construction.
struct PresetAskGuard {
    preset: PermissionPreset,
    answerer: Option<ApprovalAnswerer>,
    gate: Option<ApprovalGate>,
}

impl std::fmt::Debug for PresetAskGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresetAskGuard")
            .field("preset", &self.preset)
            .field("answerer", &self.answerer.is_some())
            .field("gate", &self.gate.is_some())
            .finish()
    }
}

impl PresetAskGuard {
    fn deny(&self, reason: String) -> Option<GuardDenial> {
        Some(GuardDenial::new(self.name(), reason))
    }
}

impl ToolGuard for PresetAskGuard {
    fn name(&self) -> &str {
        "preset_ask"
    }

    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        if call.effect.is_freely_repeatable() {
            return None;
        }
        let request = ApprovalRequest {
            kind: call.tool.to_owned(),
            effect_id: None,
            detail: Some(ask_detail([
                ("preset", Value::from(self.preset.as_str())),
                ("arguments", call.arguments.clone()),
                ("scope", Value::from(call.scope)),
            ])),
        };
        let Some(answerer) = &self.answerer else {
            return self.deny(format!(
                "preset `{}` requires approval for `{}` effects; no answerer is wired, so the ask denies (fail closed)",
                self.preset.as_str(),
                effect_name(call.effect),
            ));
        };
        let decision = answerer(&request);
        if let Some(gate) = &self.gate {
            gate.decide(&request, decision.clone());
        }
        if decision.grants() {
            None
        } else {
            self.deny(format!(
                "preset `{}` ask for `{}` was not granted: {}",
                self.preset.as_str(),
                call.tool,
                crate::tool::approval::decision_summary(&decision),
            ))
        }
    }
}

/// The wire name of an effect class, for guard reasons.
fn effect_name(effect: Effect) -> &'static str {
    match effect {
        Effect::Pure => "pure",
        Effect::ReadOnly => "read_only",
        Effect::Idempotent => "idempotent",
        Effect::Compensatable => "compensatable",
        Effect::NonIdempotent => "non_idempotent",
    }
}
