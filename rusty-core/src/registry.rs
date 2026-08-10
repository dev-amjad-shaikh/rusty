//! The prompt/configuration registry (R0.11 Extension Plane, wave 1):
//! named artifacts with an owner and a commit history, plus the diff
//! views between committed versions.
//!
//! The governing decision, from the design: the registry is the R0.8
//! candidate pipeline turned toward human-authored configuration. A
//! commit **is** a [`Candidate`] — content-addressed, immutable, authored
//! (`human:{id}` attribution, the correction loop's discipline applied to
//! configuration) — and everything that makes a candidate governable
//! (the lifecycle, the envelope gate, the version pointer, the journaled
//! transitions) is reused unchanged from [`crate::learn`]. What this
//! module adds is exactly the two things the pipeline never had:
//!
//! - [`ArtifactRecord`] — the named, owned index over candidates. One
//!   artifact per production surface (`prompt:system`,
//!   `model_settings:primary`, …): the record carries the family, the
//!   owner, and the ordered commit sequence, and nothing else. It is an
//!   index over the candidate store, never a fork of it — a commit
//!   changes nothing about the candidate it names, and the candidate's
//!   own lifecycle (created → evaluated → promoted) journals through the
//!   learn plane exactly as before. Ownership is review routing and
//!   attribution, not an ACL: tenant isolation stays API keys, and
//!   fine-grained RBAC stays post-R1.0.
//! - [`diff_candidates`] — the derived view between two committed
//!   versions: a line diff for the prompt text, a structural diff over
//!   the canonical-JSON form for the JSON families (added / removed /
//!   changed leaves). Computed on read, **never stored**: the store
//!   holds immutable versions, and a stored diff would be a second,
//!   divergent account of the same change. Diffing over
//!   `canonicalize_value` inherits its determinism — a reordering of
//!   canonical JSON object keys is not a change (array order stays
//!   significant: a middleware composition's layer order *is* the
//!   artifact).
//!
//! Environment tags and promotion per tag compose the learn plane's
//! pointer machinery directly ([`SurfaceKey::tagged`]); they are not
//! re-implemented here.
//!
//! Wave 2 (admission resolution and pinning) adds the read half of that
//! machinery: [`ConfigResolution`], the journaled payload of a
//! [`RunEventKind::ConfigResolved`](crate::record::RunEventKind::ConfigResolved)
//! event, plus the two pure functions the server's admission path is
//! built from — [`pointer_admission`] (which candidate a pointer binds
//! for this run: the canary when the seeded draw admits, else the active
//! version) and [`resolution_pin`] (what the run's manifest pins for the
//! resolved candidate). The manifest pin itself is unchanged — digests,
//! recorded by the R0.7 `pin_prompt` / `pin_tool_schema` / `pin_model`
//! exactly as before; this event is the digest ↔ version join the
//! manifest alone cannot express, so the manifest's wire shape stays
//! frozen and pre-R0.11 manifests keep deserializing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::learn::{
    canary_admits, surface_for_kind, Candidate, CandidateId, CandidateKind, EnvironmentTag,
    SurfaceKey, VersionPointer,
};
use crate::memory::ProvenanceAuthor;
use crate::record::{canonicalize_value, sha256_hex};

// --------------------------------------------------------------------- //
// The artifact record
// --------------------------------------------------------------------- //

/// The longest an artifact name may be. Names live inside surface keys —
/// which ride in pointer files, receipts, and journaled payloads — so a
/// bound keeps a configuration typo from minting an unbounded key.
pub const MAX_ARTIFACT_NAME_LEN: usize = 128;

/// One entry in an artifact's commit sequence: a candidate id and the
/// instant it joined the history. Authorship is deliberately *not*
/// repeated here — the candidate carries its own `distilled_by`, and
/// repeating it would give one fact two homes. The sequence is
/// append-only: a commit, like the candidate it names, is immutable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCommit {
    /// The committed candidate (its content address).
    pub candidate_id: CandidateId,

    /// When the commit joined this artifact's history — which may be
    /// later than the candidate's own `created_at` (distillation and
    /// registry authorship are separate acts).
    pub committed_at: DateTime<Utc>,
}

/// A named, owned artifact: the registry's only new persisted entity.
///
/// The key is the untagged production surface — [`Candidate::surface`]
/// for any candidate of this family and name — so the artifact ↔
/// candidate join is structural, not conventional: a candidate can only
/// ever commit to the artifact its own surface names. Environment-tagged
/// pointers (`prompt:system@prod`) hang off the base surface through the
/// learn plane's pointer machinery; the artifact itself is never tagged,
/// because a tag names a promotion target, and history is shared across
/// targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    /// The registry key: the untagged surface (`prompt:system`,
    /// `policy:retry`, `memory:agent:support-1`, …).
    pub surface: SurfaceKey,

    /// The registry family — the candidate kind this artifact indexes.
    pub family: CandidateKind,

    /// Who owns the artifact: review routing and attribution, not an
    /// ACL. `human:{id}` is the registry's native attribution (operator-
    /// authored configuration), but any [`ProvenanceAuthor`] is legal —
    /// a distiller-owned artifact is how a learned family keeps its
    /// pipeline attribution.
    pub owner: ProvenanceAuthor,

    /// The commit sequence, oldest first, append-only. Absent from the
    /// wire when empty: a declared-but-uncommitted artifact carries no
    /// placeholder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<ArtifactCommit>,

    /// When the artifact was declared.
    pub created_at: DateTime<Utc>,
}

impl ArtifactRecord {
    /// Declare an artifact for `family` under `name`, owned by `owner`.
    /// The name is validated (see the module docs and
    /// [`RegistryError::InvalidName`]); the surface key derives from
    /// `(family, name)` by the same rule [`Candidate::surface`] applies,
    /// so the artifact and its candidates can never disagree about which
    /// surface they govern.
    pub fn new(
        family: CandidateKind,
        name: impl Into<String>,
        owner: ProvenanceAuthor,
        created_at: DateTime<Utc>,
    ) -> Result<Self, RegistryError> {
        let name = name.into();
        validate_artifact_name(&name)?;
        Ok(Self {
            surface: surface_for_kind(family, &name),
            family,
            owner,
            commits: Vec::new(),
            created_at,
        })
    }

    /// The artifact's name — the surface minus its family prefix
    /// (`prompt:system` → `system`; `memory:agent:support-1` →
    /// `agent:support-1`).
    pub fn name(&self) -> &str {
        let prefix = surface_for_kind(self.family, "");
        self.surface
            .as_str()
            .strip_prefix(prefix.as_str())
            .unwrap_or_else(|| self.surface.as_str())
    }

    /// The latest commit (`None` before the first).
    pub fn latest(&self) -> Option<&ArtifactCommit> {
        self.commits.last()
    }

    /// The commit naming `candidate_id`, when this artifact's history
    /// contains it.
    pub fn find_commit(&self, candidate_id: &CandidateId) -> Option<&ArtifactCommit> {
        self.commits
            .iter()
            .find(|commit| &commit.candidate_id == candidate_id)
    }

    /// Admit a candidate into the history: build the commit, checking the
    /// three rules that keep the index honest. The candidate's kind must
    /// be the artifact's family and its surface must be the artifact's —
    /// a `prompt:other` candidate has no business in `prompt:system`'s
    /// history — and the candidate must not already be committed (a
    /// re-committed candidate is the same fact, answered by the route as
    /// a convergence, so reaching this guard means a genuinely duplicate
    /// append).
    ///
    /// This is the check, not the write: the store appends under its
    /// compare-and-swap guard, so two concurrent commits cannot lose one
    /// another.
    pub fn admit_commit(
        &self,
        candidate: &Candidate,
        committed_at: DateTime<Utc>,
    ) -> Result<ArtifactCommit, RegistryError> {
        if candidate.kind() != self.family {
            return Err(RegistryError::FamilyMismatch {
                artifact: self.family,
                candidate: candidate.kind(),
            });
        }
        if candidate.surface() != self.surface {
            return Err(RegistryError::SurfaceMismatch {
                artifact: self.surface.clone(),
                candidate: candidate.surface(),
            });
        }
        if self.find_commit(&candidate.candidate_id).is_some() {
            return Err(RegistryError::DuplicateCommit {
                candidate_id: candidate.candidate_id.clone(),
            });
        }
        Ok(ArtifactCommit {
            candidate_id: candidate.candidate_id.clone(),
            committed_at,
        })
    }
}

/// The artifact naming rules, enforced at declaration: non-empty, bounded
/// ([`MAX_ARTIFACT_NAME_LEN`]), no leading or trailing whitespace, no
/// control characters, no `@` (the environment-tag separator — a name
/// carrying it would make tagged and untagged surfaces ambiguous), and no
/// `/` (the tenant id-prefix separator — a name carrying it would escape
/// the store's path-keyed tenancy and the default tenant's id grammar).
/// `:` is legal: memory-scope addresses (`agent:support-1`) are names.
fn validate_artifact_name(name: &str) -> Result<(), RegistryError> {
    let refuse = |reason: &'static str| RegistryError::InvalidName {
        name: name.to_owned(),
        reason,
    };
    if name.is_empty() {
        return Err(refuse("empty — an artifact exists to be named"));
    }
    if name.len() > MAX_ARTIFACT_NAME_LEN {
        return Err(refuse("over 128 bytes"));
    }
    if name != name.trim() {
        return Err(refuse(
            "leading or trailing whitespace — visually identical names would be distinct \
             artifacts, which is a misreview waiting to happen",
        ));
    }
    if name.chars().any(|c| c.is_control() || c == '@' || c == '/') {
        return Err(refuse(
            "carries a control character, `@`, or `/` — the tag separator and the tenant \
             separator are structural, and control characters have no business in a key",
        ));
    }
    Ok(())
}

// --------------------------------------------------------------------- //
// Diff views — computed on read, never stored
// --------------------------------------------------------------------- //

/// The derived view between two committed versions of one artifact. A
/// line diff for the prompt text ([`RegistryDiff::Text`]); a structural
/// diff over the canonical-JSON content for every JSON family
/// ([`RegistryDiff::Structural`]). The wire shape is pinned under
/// `tests/golden/`, like every contract in the release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum RegistryDiff {
    /// Line diff of two prompt texts, oldest change hunks first.
    Text {
        /// Every line of the view, in order: context lines carried once,
        /// removed lines (from the base) before their added replacements.
        lines: Vec<TextDiffLine>,
    },
    /// Structural diff over the canonical-JSON content: added, removed,
    /// and changed leaves, each sorted by path. A leaf is a scalar, or a
    /// whole subtree where one side lacks the path — a removed object
    /// reports as one removed leaf, not a forest of scalars.
    Structural {
        /// Leaves present in the target and absent in the base.
        added: Vec<LeafChange>,
        /// Leaves present in the base and absent in the target.
        removed: Vec<LeafChange>,
        /// Leaves present in both with different values (including type
        /// changes, reported whole).
        changed: Vec<LeafModification>,
    },
}

impl RegistryDiff {
    /// `true` when the two versions are content-equal — the honest answer
    /// for a no-op diff, with no placeholder hunks invented.
    pub fn is_empty(&self) -> bool {
        match self {
            RegistryDiff::Text { lines } => lines
                .iter()
                .all(|line| matches!(line, TextDiffLine::Context(_))),
            RegistryDiff::Structural {
                added,
                removed,
                changed,
            } => added.is_empty() && removed.is_empty() && changed.is_empty(),
        }
    }
}

/// One line of a [`RegistryDiff::Text`] view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "line", rename_all = "snake_case")]
pub enum TextDiffLine {
    /// A line both versions carry.
    Context(String),
    /// A line the base carries and the target does not.
    Removed(String),
    /// A line the target carries and the base does not.
    Added(String),
}

/// One added or removed leaf of a [`RegistryDiff::Structural`] view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeafChange {
    /// The leaf's path (`/parameters/temperature`, `/layers/0/config`;
    /// `/`-joined segments, array indices as numbers — diagnostic, not a
    /// parseable pointer).
    pub path: String,

    /// The leaf's value on the side that carries it.
    pub value: Value,
}

/// One changed leaf of a [`RegistryDiff::Structural`] view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeafModification {
    /// The leaf's path (same convention as [`LeafChange::path`]).
    pub path: String,

    /// The base version's value.
    pub from: Value,

    /// The target version's value.
    pub to: Value,
}

/// Compute the diff view from `from` (the base) to `to` (the target).
///
/// Both candidates must be the same kind — a diff across kinds compares
/// two different contracts, which is not a view but a category error.
/// Within one kind the function is lenient about surfaces: the artifact
/// route enforces membership (both versions committed to *this*
/// artifact); the pure function answers for any same-kind pair.
///
/// The prompt family diffs its text line by line (an LCS walk — removed
/// lines before their added replacements, deterministic by construction;
/// prompt texts are operator-scale, so the quadratic table is never the
/// bottleneck). Every other family diffs structurally over the
/// canonical-JSON serialization of its content: equal canonical forms
/// diff empty, so a reordered object is not a change and equal content
/// addresses are visibly equal.
pub fn diff_candidates(from: &Candidate, to: &Candidate) -> Result<RegistryDiff, RegistryError> {
    if from.kind() != to.kind() {
        return Err(RegistryError::DiffAcrossKinds {
            from: from.kind(),
            to: to.kind(),
        });
    }
    if let (
        crate::learn::CandidateContent::Prompt { prompt: base, .. },
        crate::learn::CandidateContent::Prompt { prompt: target, .. },
    ) = (&from.content, &to.content)
    {
        return Ok(RegistryDiff::Text {
            lines: diff_lines(base, target),
        });
    }
    let base = canonicalize_value(
        &serde_json::to_value(&from.content)
            .map_err(|e| RegistryError::UndiffableContent(e.to_string()))?,
    );
    let target = canonicalize_value(
        &serde_json::to_value(&to.content)
            .map_err(|e| RegistryError::UndiffableContent(e.to_string()))?,
    );
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    diff_values("", &base, &target, &mut added, &mut removed, &mut changed);
    Ok(RegistryDiff::Structural {
        added,
        removed,
        changed,
    })
}

/// The recursive walk behind the structural diff. Objects merge-walk by
/// key (the canonical form sorts keys, so the walk order — and therefore
/// the emitted leaf order — is deterministic); arrays pair by position
/// (order is significant: a middleware layer order *is* the artifact);
/// anything else compares whole.
fn diff_values(
    path: &str,
    base: &Value,
    target: &Value,
    added: &mut Vec<LeafChange>,
    removed: &mut Vec<LeafChange>,
    changed: &mut Vec<LeafModification>,
) {
    match (base, target) {
        (Value::Object(base_map), Value::Object(target_map)) => {
            for (key, base_value) in base_map {
                let child = format!("{path}/{key}");
                match target_map.get(key) {
                    Some(target_value) => {
                        diff_values(&child, base_value, target_value, added, removed, changed)
                    }
                    None => removed.push(LeafChange {
                        path: child,
                        value: base_value.clone(),
                    }),
                }
            }
            for (key, target_value) in target_map {
                if !base_map.contains_key(key) {
                    added.push(LeafChange {
                        path: format!("{path}/{key}"),
                        value: target_value.clone(),
                    });
                }
            }
        }
        (Value::Array(base_items), Value::Array(target_items)) => {
            let shared = base_items.len().min(target_items.len());
            for (index, (base_value, target_value)) in base_items
                .iter()
                .zip(target_items.iter())
                .enumerate()
                .take(shared)
            {
                diff_values(
                    &format!("{path}/{index}"),
                    base_value,
                    target_value,
                    added,
                    removed,
                    changed,
                );
            }
            for (index, base_value) in base_items.iter().enumerate().skip(shared) {
                removed.push(LeafChange {
                    path: format!("{path}/{index}"),
                    value: base_value.clone(),
                });
            }
            for (index, target_value) in target_items.iter().enumerate().skip(shared) {
                added.push(LeafChange {
                    path: format!("{path}/{index}"),
                    value: target_value.clone(),
                });
            }
        }
        _ => {
            if base != target {
                changed.push(LeafModification {
                    path: path.to_owned(),
                    from: base.clone(),
                    to: target.clone(),
                });
            }
        }
    }
}

/// The line diff behind the prompt view: a longest-common-subsequence
/// walk over `base` and `target`, emitting context lines once and, at
/// each change, the removed lines before the added ones. Ties break
/// toward removal, which keeps a changed block reading as "these lines
/// out, those lines in" rather than interleaving the two.
fn diff_lines(base: &str, target: &str) -> Vec<TextDiffLine> {
    let base: Vec<&str> = base.lines().collect();
    let target: Vec<&str> = target.lines().collect();
    // lcs[i][j] = the common-subsequence length of base[i..] and
    // target[j..], filled bottom-up.
    let mut lcs = vec![vec![0usize; target.len() + 1]; base.len() + 1];
    for i in (0..base.len()).rev() {
        for j in (0..target.len()).rev() {
            lcs[i][j] = if base[i] == target[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut lines = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < base.len() && j < target.len() {
        if base[i] == target[j] {
            lines.push(TextDiffLine::Context(base[i].to_owned()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            lines.push(TextDiffLine::Removed(base[i].to_owned()));
            i += 1;
        } else {
            lines.push(TextDiffLine::Added(target[j].to_owned()));
            j += 1;
        }
    }
    lines.extend(
        base[i..]
            .iter()
            .map(|line| TextDiffLine::Removed((*line).to_owned())),
    );
    lines.extend(
        target[j..]
            .iter()
            .map(|line| TextDiffLine::Added((*line).to_owned())),
    );
    lines
}

// --------------------------------------------------------------------- //
// Admission resolution (R0.11 Extension Plane, wave 2)
// --------------------------------------------------------------------- //

/// Which pointer slot admitted the candidate at resolution: the
/// full-traffic `active` version, or the `canary` when the seeded draw
/// ([`canary_admits`]) admitted this run. Journaled on every
/// [`ConfigResolution`] — the difference between "the run served the
/// promoted version" and "the run served the experiment" is exactly the
/// evidence an audit of a canary's blast radius reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerBinding {
    /// The pointer's full-traffic candidate.
    Active,
    /// The pointer's canary candidate, admitted by this run's seeded draw.
    Canary,
}

/// A journaled admission resolution (R0.11 wave 2): the answer to one
/// named registry artifact a run declared at submission, recorded as the
/// output of a
/// [`RunEventKind::ConfigResolved`](crate::record::RunEventKind::ConfigResolved)
/// event at admission. This is the link that lets a manifest's content
/// *digest* pin reach the *version* it resolved from — the
/// [`CapsuleResolution`](crate::capsule::CapsuleResolution) precedent
/// (version string → content address) applied to configuration
/// (content digest → candidate).
///
/// The walk it closes: the run's signed receipt commits the manifest
/// digest; the manifest pins `digest`; this event names the `surface`,
/// `tag`, and `candidate_id` the digest resolved from; the candidate
/// carries its author, evaluation, and promotion approver. Every hop is
/// signature-covered — the resolution event sits inside the journal the
/// receipt's head signs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigResolution {
    /// The artifact resolved: its registry key, the **untagged** surface
    /// (`prompt:system`). The tag travels separately so the two facts —
    /// which artifact, which environment — read without string surgery.
    pub surface: SurfaceKey,

    /// The environment tag the run resolved under (`prompt:system@prod`).
    /// Absent when the run resolved the untagged surface — either it
    /// declared no environment and the deployment declares no default
    /// tag, or (deliberately) the artifact is governed untagged. Absent
    /// is a fact, never a guess: the deployment's declared default tag is
    /// configuration, not something the run invents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<EnvironmentTag>,

    /// The candidate the pointer bound — the exact version the run
    /// pinned, reproducible at audit from this id alone (candidates are
    /// content-addressed: the id *is* the integrity check).
    pub candidate_id: CandidateId,

    /// Which pointer slot admitted the candidate.
    pub pointer: PointerBinding,

    /// The content digest the run's manifest pins for this artifact —
    /// exactly what `RunManifest::pin_prompt` (SHA-256 of the prompt
    /// text), `pin_tool_schema` (canonical-JSON digest of the schema),
    /// or `pin_model` (canonical-JSON digest of the parameters) records.
    /// The digest ↔ version join, journaled: equal digests under
    /// different candidate ids cannot exist (content addressing), so
    /// this field is the receipt walk's hinge.
    pub digest: String,

    /// The model identifier the manifest pins alongside the parameters
    /// digest — present exactly for `model_settings` resolutions (the
    /// manifest's `model` slot is an identifier, not a digest, and the
    /// walk is incomplete without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// The resolved layer order — present exactly for
    /// `middleware_composition` resolutions (R0.11 wave 4): the layer
    /// names in the order the run's chain instantiated them. Ordering is
    /// registry-aware in both directions — the artifact declares the
    /// order, and this is the journaled evidence of the order that
    /// actually served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<String>>,
}

/// The candidate a pointer binds for `run_id`: the canary when this
/// run's seeded draw admits, else the full-traffic version. `None` when
/// the pointer serves nothing — for registry artifacts there is no
/// static fallback (that floor belongs to learned policy, where the
/// pre-registry behavior is defined); an artifact with nothing promoted
/// is unresolvable, and admission refuses the run rather than guessing.
///
/// The draw reads `pointer.surface` — the *tagged* key when the
/// promotion was tagged — so the environment is part of the draw's seed
/// material: a canary at `staging` and a canary at `prod` are
/// independent draws over the same run id, and a recorded run
/// re-derives its assignment from the journaled resolution alone.
pub fn pointer_admission(
    pointer: &VersionPointer,
    run_id: &str,
) -> Option<(CandidateId, PointerBinding)> {
    if let Some(binding) = &pointer.canary {
        if canary_admits(binding, &pointer.surface, run_id) {
            return Some((binding.candidate_id.clone(), PointerBinding::Canary));
        }
    }
    pointer
        .active
        .clone()
        .map(|candidate_id| (candidate_id, PointerBinding::Active))
}

/// What the run's manifest pins for a resolved candidate: the content
/// digest (and, for model settings, the model identifier) the R0.7 pin
/// slots record. Computed from the candidate's content by the same rules
/// `RunManifest`'s pin functions apply, so the journaled `digest` and
/// the manifest's pin can never diverge — one derivation, two homes
/// (the header and the journal).
///
/// Four families resolve: prompts, tool contracts, and model settings
/// (the R0.7 digest slots), plus middleware compositions (R0.11 wave 4 —
/// the release's one additive manifest field, the `middleware` digest).
/// Every other kind is refused ([`RegistryError::UnresolvableKind`]) —
/// executor policies bind through the checkpoint header's
/// `policy_version`, tool permissions through the capsule machinery, and
/// memory configuration pins in its own wave; pretending otherwise would
/// fake coverage the manifest cannot express.
pub fn resolution_pin(candidate: &Candidate) -> Result<(String, Option<String>), RegistryError> {
    match &candidate.content {
        crate::learn::CandidateContent::Prompt { prompt, .. } => {
            Ok((sha256_hex(prompt.as_bytes()), None))
        }
        crate::learn::CandidateContent::ToolContract { schema, .. } => {
            let bytes = serde_json::to_vec(&canonicalize_value(schema))
                .map_err(|e| RegistryError::UndiffableContent(e.to_string()))?;
            Ok((sha256_hex(&bytes), None))
        }
        crate::learn::CandidateContent::ModelSettings {
            model, parameters, ..
        } => {
            let bytes = serde_json::to_vec(&canonicalize_value(parameters))
                .map_err(|e| RegistryError::UndiffableContent(e.to_string()))?;
            Ok((sha256_hex(&bytes), Some(model.clone())))
        }
        // The composition's pin is the canonical-JSON digest of its
        // ordered layer list — the same rule `RunManifest::pin_middleware`
        // applies, so the journaled digest and the header pin are one
        // derivation. Array order is significant: a middleware layer
        // order *is* the artifact, so the canonical form sorts object
        // keys but never arrays.
        crate::learn::CandidateContent::MiddlewareComposition { layers, .. } => {
            let value = serde_json::to_value(layers)
                .map_err(|e| RegistryError::UndiffableContent(e.to_string()))?;
            let bytes = serde_json::to_vec(&canonicalize_value(&value))
                .map_err(|e| RegistryError::UndiffableContent(e.to_string()))?;
            Ok((sha256_hex(&bytes), None))
        }
        _ => Err(RegistryError::UnresolvableKind {
            kind: candidate.kind(),
        }),
    }
}

// --------------------------------------------------------------------- //
// Errors
// --------------------------------------------------------------------- //

/// The registry's typed refusals. A refused declaration, commit, or diff
/// changes nothing — the [`crate::learn::LearnError`] discipline: refused
/// operations are contract outcomes surfaced to the caller, never silent
/// no-ops.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum RegistryError {
    /// An artifact name outside the naming rules (see
    /// [`ArtifactRecord::new`]).
    #[error("invalid artifact name {name:?}: {reason}")]
    InvalidName {
        /// The refused name.
        name: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A commit whose candidate is not the artifact's family.
    #[error(
        "candidate is `{candidate}` but artifact is `{artifact}` — a commit joins the \
         artifact of its own family, never another's"
    )]
    FamilyMismatch {
        /// The artifact's family.
        artifact: CandidateKind,
        /// The candidate's kind.
        candidate: CandidateKind,
    },

    /// A commit whose candidate surfaces somewhere else.
    #[error(
        "candidate surfaces at `{candidate}` but the artifact is `{artifact}` — a commit \
         joins the artifact its own surface names"
    )]
    SurfaceMismatch {
        /// The artifact's surface.
        artifact: SurfaceKey,
        /// The candidate's surface.
        candidate: SurfaceKey,
    },

    /// The candidate is already in this artifact's history.
    #[error(
        "candidate `{candidate_id}` is already committed to this artifact — a re-commit is \
         the same fact, converged at the route; a second distinct commit needs a distinct \
         candidate"
    )]
    DuplicateCommit {
        /// The doubly committed candidate.
        candidate_id: CandidateId,
    },

    /// A diff was requested against a candidate this artifact's history
    /// does not contain.
    #[error(
        "candidate `{candidate_id}` is not committed to this artifact — a diff views two \
         committed versions of one artifact, nothing wider"
    )]
    NotCommitted {
        /// The candidate outside the history.
        candidate_id: CandidateId,
    },

    /// A diff across kinds: two different contracts, not a view.
    #[error(
        "cannot diff `{from}` against `{to}` — a diff compares two versions of one \
         contract; across kinds it compares two different contracts"
    )]
    DiffAcrossKinds {
        /// The base candidate's kind.
        from: CandidateKind,
        /// The target candidate's kind.
        to: CandidateKind,
    },

    /// An admission resolution was requested for a kind with no
    /// content-digest slot in the run manifest (R0.11): prompts, tool
    /// contracts, model settings, and middleware compositions (wave 4)
    /// resolve; executor policies bind through the checkpoint header's
    /// `policy_version`, tool permissions through the capsule machinery,
    /// and memory configuration pins in its own wave.
    #[error(
        "`{kind}` candidates do not resolve into a run manifest at admission — prompts, tool \
         contracts, model settings, and middleware compositions bind; the other families pin \
         through their own surfaces (the checkpoint header, capsule manifests) or their own waves"
    )]
    UnresolvableKind {
        /// The kind refused.
        kind: CandidateKind,
    },

    /// A candidate's content could not be serialized for the structural
    /// diff — unreachable for well-formed content, surfaced rather than
    /// panicked (the `UnaddressableContent` precedent).
    #[error("candidate content could not be diffed: {0}")]
    UndiffableContent(String),
}
