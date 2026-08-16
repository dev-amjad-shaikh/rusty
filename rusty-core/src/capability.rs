//! Resolved capability sets: the immutable, content-addressed composition
//! one agent version declares and one run resolves at admission.
//!
//! A [`CapabilitySet`] names exact members — tool names today, plus
//! forward-compatible skill/connector references — and derives its identity
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
//! Skill and connector members are opaque references with kind tags. Their
//! planes land separately; the set records them verbatim today so the
//! content address already covers them, and validation against their
//! registries arrives with those planes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::tool::{ToolCapability, ToolRegistry};

/// The set id prefix: a capability set id is `cs-` followed by the
/// lowercase hex SHA-256 of the set's canonical member serialization —
/// the same digest convention every [`crate::record::RunManifest`] pin
/// follows.
pub const CAPABILITY_SET_ID_PREFIX: &str = "cs-";

/// Maximum length of one opaque skill/connector reference.
pub const MAX_CAPABILITY_REF_BYTES: usize = 256;

/// Which capability plane a [`CapabilityRef`] belongs to.
///
/// Tools are not referenced through this type: they are the execution
/// plane this crate already owns, so a set names them directly. Only the
/// planes this module does not interpret — skills and connectors — ride
/// as opaque references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRefKind {
    /// A versioned skill package (procedural knowledge; the skill plane
    /// owns interpretation).
    Skill,
    /// A connector instance or generation (the connector plane owns
    /// interpretation).
    Connector,
}

impl CapabilityRefKind {
    /// The stable wire tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityRefKind::Skill => "skill",
            CapabilityRefKind::Connector => "connector",
        }
    }
}

/// An opaque, kind-tagged reference to a skill or connector plane member.
///
/// The reference string is interpreted only by the owning plane; this
/// module checks shape (non-empty, trimmed, control-free, bounded) and
/// otherwise records it verbatim so the set's content address covers it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CapabilityRef {
    /// The plane that interprets `reference`.
    pub kind: CapabilityRefKind,
    /// Opaque plane-specific reference (a skill package id, a connector
    /// instance id).
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

    /// A connector plane reference.
    pub fn connector(reference: impl Into<String>) -> Result<Self> {
        Self::new(CapabilityRefKind::Connector, reference)
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
    /// Skill/connector references, sorted by (kind, reference).
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
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        CapabilitySetBody {
            tools: self.tools.clone(),
            refs: self.refs.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CapabilitySet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let body = CapabilitySetBody::deserialize(deserializer)?;
        Self::from_members(&body.tools, &body.refs).map_err(serde::de::Error::custom)
    }
}

impl CapabilitySet {
    /// Compose and validate a set against the executable catalog.
    ///
    /// Every tool member must appear in `catalog`; unknown names fail
    /// closed. Skill/connector references are shape-checked and recorded
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

    /// The skill/connector references, sorted by (kind, reference).
    pub fn refs(&self) -> &[CapabilityRef] {
        &self.refs
    }

    /// `true` when the set names no members at all — a deliberately
    /// tool-free agent.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.refs.is_empty()
    }

    /// Resolve the set into the exact tool allowlist the executor consumes
    /// ([`crate::executor::RunConfig::tool_allowlist`]). Skill/connector
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
    /// Skill/connector references have no registry to check against yet;
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
    format!("{CAPABILITY_SET_ID_PREFIX}{}", crate::record::sha256_hex(&bytes))
}
