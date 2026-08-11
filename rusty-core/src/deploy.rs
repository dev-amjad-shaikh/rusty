//! The deployment control plane's core contracts (R0.12 Operations
//! Plane, wave 3): revisions, environments, and the per-environment
//! deployment pointer, plus the journaled payloads of every control-plane
//! act and the environment-scoped secret records.
//!
//! The governing decision, from the design: the heavy machinery is
//! reused, so the new entities stay small. A [`DeploymentRevision`] is
//! the [`crate::learn::Candidate`] discipline turned toward deployments —
//! immutable, content-addressed ([`derive_revision_id`], the
//! [`crate::learn::derive_candidate_id`] rule), authored (`human:{id}`
//! attribution) — carrying the graph/assistant identity, the
//! `graph_hash` the checkpoint header already computes, and a **frozen
//! pin set**: the registry surfaces the revision binds, resolved to
//! candidate ids at revision *creation* from a declared source
//! environment. The freeze is deliberate and priced in the design's
//! honest edges: a revision evaluated against a recorded dataset must be
//! the same thing the gate evaluated when it canaries, and pins resolved
//! at admission would make that evidence a moving target. The weight —
//! every registry promotion that should reach prod requires a new
//! revision — is the price of evaluable deployments, paid knowingly.
//!
//! An [`Environment`] is R0.11's tag promoted to a first-class record:
//! a name (the `dev` / `staging` / `prod` convention, deployment-declared
//! rather than enumerated), the gate and approval declarations that will
//! govern promotions into it (wired in wave 4; declared here so the rule
//! in force is data, not code), and creation metadata. What an
//! environment is *not* — the R0.11 tag discipline applied one level up:
//! not a separate process, not an isolated store, not a trust boundary.
//! Environments are logical surfaces over one deployment's stores and one
//! binary's admission path.
//!
//! The [`DeploymentPointer`] is the R0.8
//! [`crate::learn::VersionPointer`] shape applied to revisions: one
//! pointer per environment under the surface key `deployment:{env}`, an
//! `active` full-traffic revision, and a `canary` binding one revision to
//! a declared fraction of new runs. Promotion moves `active` and clears
//! any canary — a full promotion supersedes the experiment it graduated
//! from. Rollback re-points `active` to the previously serving revision:
//! byte-exact, because the restored revision is the immutable record that
//! served before, not a reconstruction. Canary admission is the seeded
//! draw ([`deployment_admission`]) over the pointer surface — the draw
//! machinery reused verbatim; only the surface it draws over is new.
//!
//! Environment-scoped secrets are custody, not brokerage (R0.11's line,
//! re-argued by the environment dimension): named, tenant-scoped,
//! environment-tagged values stored as ciphertext envelopes on both
//! backends (the broker's construction — the store side only ever holds
//! [`StoredEnvSecret`]), journaled as metadata — never bytes — on the
//! deployment evidence chain. A request outside the holder's environment
//! is a typed, journaled [`EnvSecretDenial`]: scoping is enforcement, not
//! convention.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::broker::SealedCredential;
use crate::error::Result;
use crate::learn::{canary_admits, CanaryBinding, CandidateId, EnvironmentTag, SurfaceKey};
use crate::memory::ProvenanceAuthor;
use crate::record::sha256_hex;
use crate::registry::PointerBinding;

// --------------------------------------------------------------------- //
// The deployment revision
// --------------------------------------------------------------------- //

/// A content-addressed revision identity: lowercase hex SHA-256 over the
/// revision's canonical content ([`derive_revision_id`]). Transparent
/// newtype so the type system — not convention — keeps revision ids
/// distinct from candidate ids, memory addresses, and other digest
/// strings (the [`CandidateId`] precedent).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(String);

impl RevisionId {
    /// The hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RevisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for RevisionId {
    /// Wrap a digest the caller already trusts — rehydration from storage,
    /// a rollback's `to`. Minting ids from content is
    /// [`derive_revision_id`]'s job alone; nothing here re-verifies.
    fn from(digest: String) -> Self {
        Self(digest)
    }
}

/// One frozen pin: a registry surface bound to the candidate id that
/// served it in the revision's source environment at creation. The pin
/// names a *candidate* — content-addressed, immutable — so the pin set
/// can never drift beneath a recorded revision: what the gate evaluated
/// is what canaries, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryPin {
    /// The untagged registry surface (`prompt:system`,
    /// `model_settings:primary`, …). The environment the pin was resolved
    /// from travels once on the revision (`source_environment`), not on
    /// every pin.
    pub surface: SurfaceKey,

    /// The candidate the source environment's pointer served at creation.
    pub candidate_id: CandidateId,
}

/// What a revision binds: the content the address covers.
///
/// Struct serialization is field-ordered, so the canonical form is stable
/// across map backends — the same discipline
/// [`crate::learn::derive_candidate_id`] relies on. Attribution is not
/// identity (the [`crate::learn::Candidate`] rule): the author and the
/// creation instant travel on the record, outside the address, so two
/// registrations of the same declaration converge on one id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionContent {
    /// The registered graph this revision serves (the `GraphRegistry`
    /// name — the run's admission target).
    pub graph: String,

    /// The graph's topology hash — exactly what
    /// [`crate::graph::Graph::topology_hash`] computes and the checkpoint
    /// header stamps. Code identity is the R0.7/R0.11 story, unchanged;
    /// the revision records it so admission can refuse a build the
    /// revision no longer describes.
    pub graph_hash: String,

    /// The assistant identity the revision binds, when the deployment
    /// serves through one. Absent when the revision binds the graph
    /// directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,

    /// The environment the pin set was resolved from at creation. A
    /// revision-refresh is a new revision resolved again from its source
    /// — the staleness of the freeze becomes a journaled act, never
    /// silent drift.
    pub source_environment: EnvironmentTag,

    /// The frozen pin set, sorted by surface (the canonical order — a
    /// set, so order is derivation, not declaration). May be empty: a
    /// graph that binds no registry artifacts pins nothing.
    pub pins: Vec<RegistryPin>,
}

/// The content address of a revision: `sha256` over the canonical
/// serialization of its content — the [`crate::learn::derive_candidate_id`]
/// discipline, so two registrations of the same declaration converge on
/// one id and a tampered revision fails its own address.
pub fn derive_revision_id(content: &RevisionContent) -> Result<RevisionId> {
    Ok(RevisionId(sha256_hex(&serde_json::to_vec(content)?)))
}

/// The digest a journaled [`DeploymentResolved`] names for the bound
/// revision's pin set: `sha256` over the canonical pin list. The
/// revision's own address already covers the pins — one derivation, two
/// homes — so the audit walk reads the pin-set commitment out of the
/// resolution event without fetching the revision.
pub fn pin_set_digest(pins: &[RegistryPin]) -> String {
    // Struct serialization is field-ordered (see `RevisionContent`), so
    // this digest is stable across map backends.
    let bytes = serde_json::to_vec(pins).expect("a pin list always serializes");
    sha256_hex(&bytes)
}

/// An immutable, content-addressed declaration of what may serve.
///
/// Immutable by construction: a changed declaration is a new id, and
/// there is no in-place update anywhere in the control plane. A registry
/// prompt promotion does not flow into an existing revision — the
/// revision-refresh path mints a new one addressing the delta, a
/// journaled act like any other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentRevision {
    /// The content address ([`derive_revision_id`]).
    pub revision_id: RevisionId,

    /// What the revision binds.
    pub content: RevisionContent,

    /// Who registered the revision (`human:{id}`, the registry commit
    /// discipline). Mandatory — journaled with registration, because a
    /// deployment act that cannot name its author is indistinguishable
    /// from an untracked edit.
    pub author: ProvenanceAuthor,

    /// When the revision was registered.
    pub created_at: DateTime<Utc>,
}

impl DeploymentRevision {
    /// Mint a revision over `content`, attributed to `author`. The pin
    /// set is canonicalized here — sorted by surface, duplicate surfaces
    /// refused — so equal declarations converge on one address however
    /// the caller ordered them.
    pub fn new(
        mut content: RevisionContent,
        author: ProvenanceAuthor,
        created_at: DateTime<Utc>,
    ) -> std::result::Result<Self, DeployError> {
        if content.graph.is_empty() {
            return Err(DeployError::InvalidRevision {
                reason: "empty graph identity — a revision exists to name what serves",
            });
        }
        content.pins.sort_by(|a, b| a.surface.cmp(&b.surface));
        for pair in content.pins.windows(2) {
            if pair[0].surface == pair[1].surface {
                return Err(DeployError::DuplicatePin {
                    surface: pair[0].surface.clone(),
                });
            }
        }
        let revision_id = derive_revision_id(&content)
            .map_err(|e| DeployError::UnaddressableContent(e.to_string()))?;
        Ok(Self {
            revision_id,
            content,
            author,
            created_at,
        })
    }

    /// Re-derive the content address and compare. The integrity check the
    /// store read path and admission run before anything serves or pins —
    /// a revision failing its own address is tampered evidence, refused
    /// rather than journaled.
    pub fn verify_address(&self) -> std::result::Result<(), DeployError> {
        let derived = derive_revision_id(&self.content)
            .map_err(|e| DeployError::UnaddressableContent(e.to_string()))?;
        if derived != self.revision_id {
            return Err(DeployError::AddressMismatch {
                declared: self.revision_id.clone(),
                derived,
            });
        }
        Ok(())
    }
}

// --------------------------------------------------------------------- //
// The environment record
// --------------------------------------------------------------------- //

/// The gate declaration governing promotions into an environment: the
/// gate policy's name and the dataset version the gate replays. Names
/// only — the gate seam itself wires in wave 4; what lands here is the
/// declaration, so the rule in force when a promotion happens is data an
/// audit reads, not code it infers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateDeclaration {
    /// The gate policy's name (the `rusty-eval` `GatePolicy` the control
    /// plane will evaluate through the core seam, wave 4).
    pub policy: String,

    /// The recorded dataset version the gate replays the revision
    /// against. Versioned because datasets are immutable per version:
    /// re-running the gate must mean re-reading the same evidence.
    pub dataset_version: String,
}

/// An environment: R0.11's promotion tag as a first-class record.
///
/// Logical, not physical — one deployment serves every environment from
/// one store and one binary's admission path; the record exists so the
/// control plane can name the surface, declare its promotion rule, and
/// answer "which revision serves here" from journaled data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    /// The environment's name — `dev` / `staging` / `prod` by
    /// deployment-declared convention, not an enum: the R0.11 tag set,
    /// promoted. Validated by [`EnvironmentTag`], so the pointer surface
    /// key (`deployment:{name}`) is always well-formed.
    pub name: EnvironmentTag,

    /// The gate declaration promotions into this environment must clear,
    /// when one is declared (enforced in wave 4; recorded from the
    /// start). Absent: promotion is an operator act with no gate — the
    /// `dev` floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateDeclaration>,

    /// Whether promotions into this environment require a human approval
    /// token scoped to the revision's promotion effect id (checked in
    /// wave 4; the `prod` stance). Recorded as data so the declaration is
    /// auditable before it is enforced.
    #[serde(default)]
    pub approval_required: bool,

    /// Who declared the environment (`human:{id}`).
    pub created_by: ProvenanceAuthor,

    /// When the environment was declared.
    pub created_at: DateTime<Utc>,
}

/// The pointer surface key for one environment: `deployment:{env}`. One
/// prefix for every deployment pointer, so pointer listings and the
/// canary draw's seed material read without string surgery.
pub fn deployment_surface(environment: &EnvironmentTag) -> SurfaceKey {
    SurfaceKey::new(format!("deployment:{environment}"))
}

// --------------------------------------------------------------------- //
// The deployment pointer
// --------------------------------------------------------------------- //

/// A canary binding on a deployment pointer: one revision bound to a
/// declared fraction of new runs while `active` serves the rest — the
/// [`crate::learn::CanaryBinding`] shape with a revision in the slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryDeployment {
    /// The canaried revision.
    pub revision_id: RevisionId,

    /// The fraction of new runs the revision serves, in `(0, 1]`.
    pub fraction: f64,
}

impl CanaryDeployment {
    /// Declare a canary. Fractions outside `(0, 1]` are refused: zero
    /// binds nothing (a cleared slot says that), and above one is not a
    /// fraction.
    pub fn new(revision_id: RevisionId, fraction: f64) -> std::result::Result<Self, DeployError> {
        if !(fraction > 0.0 && fraction <= 1.0) {
            return Err(DeployError::InvalidFraction { fraction });
        }
        Ok(Self {
            revision_id,
            fraction,
        })
    }
}

/// The serving revision of one environment: an immutable-pointer move
/// away from any revision that ever served — the R0.8
/// [`crate::learn::VersionPointer`] shape applied to revisions.
///
/// New runs bind the pointer at admission (the canary by seeded draw —
/// [`deployment_admission`]); in-flight runs keep the revision their
/// journaled resolution names, the same conservatism as checkpoint
/// pinning. There is no hot reload: a hot-reloaded deployment is a
/// silent behavioral rewrite, forbidden since R0.8.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentPointer {
    /// The surface this pointer governs (`deployment:{env}`).
    pub surface: SurfaceKey,

    /// The full-traffic revision (`None`: the environment serves
    /// nothing — there is no implicit "latest", because latest is a
    /// guess with a deploy's blast radius).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RevisionId>,

    /// The canary binding, when one is declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<CanaryDeployment>,
}

impl DeploymentPointer {
    /// A pointer with nothing serving.
    pub fn new(surface: SurfaceKey) -> Self {
        Self {
            surface,
            active: None,
            canary: None,
        }
    }

    /// The pointer after a promotion: `active` moves to the promoted
    /// revision and any canary clears — a full promotion supersedes the
    /// experiment it graduated from (the
    /// [`crate::learn::VersionPointer::promoted`] semantics).
    pub fn promoted(&self, promotion: &RevisionPromotion) -> DeploymentPointer {
        let mut next = self.clone();
        next.active = Some(promotion.revision_id.clone());
        next.canary = None;
        next
    }

    /// The pointer after a rollback: `active` re-points to the
    /// revision that served before — byte-exact, because revisions are
    /// content-addressed and immutable, so the restored revision is the
    /// one that served, not a reconstruction. A canary naming the rolled
    /// revision clears with it. The caller derives `to` from the
    /// journaled transition history before building the rollback — this
    /// is the move, not the check.
    pub fn rolled_back(&self, rollback: &RevisionRollback) -> DeploymentPointer {
        let mut next = self.clone();
        if self.active.as_ref() == Some(&rollback.from) {
            next.active = rollback.to.clone();
        }
        if self
            .canary
            .as_ref()
            .is_some_and(|canary| canary.revision_id == rollback.from)
        {
            next.canary = None;
        }
        next
    }
}

/// The revision an environment's pointer binds for `run_id`: the canary
/// when this run's seeded draw admits, else the full-traffic revision.
/// `None` when the pointer serves nothing — a pointer serving nothing
/// binds nothing, and admission refuses the run rather than guessing.
///
/// The draw reuses [`canary_admits`] verbatim: its seed material is the
/// bound id's string form, so wrapping the revision id as a one-off
/// [`CanaryBinding`] draws the identical seeded value the learn plane
/// would — a canary at staging and a canary at prod are independent
/// draws over the same run id (the surface is part of the seed), and a
/// recorded run re-derives its assignment from the journaled resolution
/// alone.
pub fn deployment_admission(
    pointer: &DeploymentPointer,
    run_id: &str,
) -> Option<(RevisionId, PointerBinding)> {
    if let Some(canary) = &pointer.canary {
        let binding = CanaryBinding {
            candidate_id: CandidateId::from(canary.revision_id.as_str().to_owned()),
            fraction: canary.fraction,
        };
        if canary_admits(&binding, &pointer.surface, run_id) {
            return Some((canary.revision_id.clone(), PointerBinding::Canary));
        }
    }
    pointer
        .active
        .clone()
        .map(|revision_id| (revision_id, PointerBinding::Active))
}

// --------------------------------------------------------------------- //
// The journaled control-plane acts
// --------------------------------------------------------------------- //

/// A revision was registered: the journaled payload of a
/// [`crate::record::RunEventKind::RevisionRegistered`] event on the
/// deployment evidence chain. The revision carries its own author and
/// content address; the tenant rides alongside because the chain is
/// deployment-wide (the [`crate::artifact::ArtifactRelease`] precedent —
/// audit metadata, not scoping).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionRegistration {
    /// The tenant the revision registered under.
    pub tenant: String,

    /// The registered revision.
    pub revision: DeploymentRevision,
}

/// An environment was declared: the journaled payload of a
/// [`crate::record::RunEventKind::EnvironmentDeclared`] event. The rule
/// in force journals with the declaration, so an audit reads the
/// declaration that governed a promotion, not a later edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentDeclaration {
    /// The tenant the environment was declared under.
    pub tenant: String,

    /// The declared environment.
    pub environment: Environment,
}

/// A revision was promoted into an environment: the journaled payload of
/// a [`crate::record::RunEventKind::RevisionPromoted`] event, and the
/// move [`DeploymentPointer::promoted`] applies. `previous` is recorded
/// because it is the rollback path's whole story: the transition history
/// on the chain is what a byte-exact rollback re-derives, never a
/// reconstruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionPromotion {
    /// The tenant the promotion governs.
    pub tenant: String,

    /// The environment promoted into.
    pub environment: EnvironmentTag,

    /// The revision that now serves full traffic.
    pub revision_id: RevisionId,

    /// The revision the promotion displaced (`None` when the environment
    /// served nothing before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<RevisionId>,

    /// Who promoted (`human:{id}` — a deployment act with a name on it).
    pub author: ProvenanceAuthor,

    /// When the promotion journaled.
    pub promoted_at: DateTime<Utc>,
}

/// A serving revision was rolled back: the journaled payload of a
/// [`crate::record::RunEventKind::RevisionRolledBack`] event, and the
/// move [`DeploymentPointer::rolled_back`] applies. Byte-exact by
/// construction: `to` is derived from the chain's transition history —
/// the immutable revision that served before — so the restored serving
/// state is a fact about what happened, not a guess at what should.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionRollback {
    /// The tenant the rollback governs.
    pub tenant: String,

    /// The environment rolling back.
    pub environment: EnvironmentTag,

    /// The revision that was serving.
    pub from: RevisionId,

    /// The previously serving revision, re-derived from the journaled
    /// transition history (`None`: the environment returns to serving
    /// nothing — the state before its first promotion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<RevisionId>,

    /// Why the rollback happened — the operator's note, the incident id.
    pub cause: String,

    /// Who rolled back (`human:{id}`).
    pub author: ProvenanceAuthor,

    /// When the rollback journaled.
    pub rolled_back_at: DateTime<Utc>,
}

/// The journaled admission resolution (R0.12 wave 3): the output of a
/// [`crate::record::RunEventKind::DeploymentResolved`] event, journaled
/// at admission into the run's own journal — the
/// [`crate::registry::ConfigResolution`] precedent lifted from
/// configuration to deployments.
///
/// The walk it closes: the run's signed receipt commits the journal head;
/// this event names the environment, the bound `revision_id`, and the
/// pointer slot; the revision addresses the frozen pin set; the pins name
/// candidates; the candidates carry their authors and promotions. Every
/// hop is signature-covered — this event sits inside the journal the
/// receipt's head signs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentResolved {
    /// The environment the run was admitted to.
    pub environment: EnvironmentTag,

    /// The revision the environment's pointer bound — reproducible at
    /// audit from this id alone (revisions are content-addressed: the id
    /// *is* the integrity check).
    pub revision_id: RevisionId,

    /// Which pointer slot admitted the revision (the full-traffic
    /// `active`, or the `canary` when this run's seeded draw admitted).
    pub pointer: PointerBinding,

    /// The digest of the bound revision's frozen pin set
    /// ([`pin_set_digest`]) — the pin-set commitment, readable without
    /// fetching the revision.
    pub pin_set_digest: String,
}

// --------------------------------------------------------------------- //
// Environment-scoped secrets
// --------------------------------------------------------------------- //

/// The longest an environment-secret name may be. Names ride in scoped
/// keys (`{name}@{environment}`), journal payloads, and store primary
/// keys, so a bound keeps a configuration typo from minting an unbounded
/// key.
pub const MAX_SECRET_NAME_LEN: usize = 128;

/// The scoped id of an environment secret: `{name}@{environment}` — the
/// scope is part of the secret's identity, so the staging database URL
/// and the prod database URL are two secrets that can never be
/// interchangeable. The sealed envelope authenticates this id as
/// associated data, so a ciphertext transplanted between scopes fails to
/// open.
pub fn scoped_secret_name(name: &str, environment: &EnvironmentTag) -> String {
    format!("{name}@{environment}")
}

/// The secret naming rules, enforced at set time: non-empty, bounded
/// ([`MAX_SECRET_NAME_LEN`]), no leading or trailing whitespace, no
/// control characters, no `@` (the scope separator — a name carrying it
/// would make scoped ids ambiguous), and no `/` (the tenant id-prefix
/// separator). The registry artifact naming rule, applied to secrets.
pub fn validate_secret_name(name: &str) -> std::result::Result<(), DeployError> {
    let refuse = |reason: &'static str| DeployError::InvalidSecretName {
        name: name.to_owned(),
        reason,
    };
    if name.is_empty() {
        return Err(refuse("empty — a secret exists to be named"));
    }
    if name.len() > MAX_SECRET_NAME_LEN {
        return Err(refuse("over 128 bytes"));
    }
    if name != name.trim() {
        return Err(refuse(
            "leading or trailing whitespace — visually identical names would be distinct \
             secrets, which is a misreview waiting to happen",
        ));
    }
    if name.chars().any(|c| c.is_control() || c == '@' || c == '/') {
        return Err(refuse(
            "carries a control character, `@`, or `/` — the scope separator and the tenant \
             separator are structural, and control characters have no business in a key",
        ));
    }
    Ok(())
}

/// The public record of one environment secret: metadata by
/// construction, safe to serve and journal. Rotation is replacement
/// beneath the stable scoped name — what a run's evidence pins is the
/// scoped name, not the value of the moment (the broker's "rotate a
/// credential without redeploying" argument applied to static material).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvSecretRecord {
    /// The secret's name within its scope.
    pub name: String,

    /// The environment the secret is scoped to. Enforcement, not
    /// convention: resolution serves `name@{environment}` and nothing
    /// else.
    pub environment: EnvironmentTag,

    /// Who set (or last rotated) the secret (`human:{id}`).
    pub set_by: ProvenanceAuthor,

    /// When the secret was first set.
    pub created_at: DateTime<Utc>,

    /// When the secret was last rotated beneath the stable scoped name.
    /// Absent when never rotated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<DateTime<Utc>>,
}

/// What both store backends hold for one environment secret: the
/// metadata record plus the sealed value. Plaintext on neither backend,
/// ever — a Postgres dump of this row contains no secret (the broker's
/// custody shape, verbatim).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEnvSecret {
    /// The metadata record (safe to serve and journal).
    pub record: EnvSecretRecord,

    /// The sealed value — the broker's envelope construction with the
    /// scoped secret id as associated data.
    pub envelope: SealedCredential,
}

/// A secret was set or rotated: the journaled payload of an
/// [`crate::record::RunEventKind::EnvSecretSet`] event on the deployment
/// evidence chain. Metadata only — the bytes live only in the sealed
/// envelope and can never appear here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvSecretAct {
    /// The tenant the secret serves.
    pub tenant: String,

    /// The secret's metadata after the act (`rotated_at` marks a
    /// rotation beneath the stable scoped name).
    pub record: EnvSecretRecord,
}

/// A secret was revoked by deletion: the journaled payload of an
/// [`crate::record::RunEventKind::EnvSecretRevoked`] event. Deletion is
/// the only revocation path — there is no disable flag to forget, and
/// the tombstone is the evidence the scope once held a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvSecretRevocation {
    /// The tenant the secret served.
    pub tenant: String,

    /// The revoked secret's name.
    pub name: String,

    /// The environment the secret was scoped to.
    pub environment: EnvironmentTag,

    /// Who revoked it (`human:{id}`).
    pub revoked_by: ProvenanceAuthor,

    /// When the revocation journaled.
    pub revoked_at: DateTime<Utc>,
}

/// A secret resolution was refused on scope: the journaled payload of an
/// [`crate::record::RunEventKind::EnvSecretDenied`] event — the
/// [`crate::capsule::CapsuleDenial`] discipline (attributable to a
/// declaration, not a stack trace) applied to environment scope. Names
/// the scope requested and the scope the holder holds, because the
/// difference between the two is exactly what a scope audit reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvSecretDenial {
    /// The tenant the request ran under.
    pub tenant: String,

    /// The secret's name.
    pub name: String,

    /// The environment scope the request asked for.
    pub requested_environment: EnvironmentTag,

    /// The environment scope the requester holds — the admission
    /// environment the resolution runs under.
    pub held_environment: EnvironmentTag,

    /// When the refusal journaled.
    pub denied_at: DateTime<Utc>,
}

// --------------------------------------------------------------------- //
// Errors
// --------------------------------------------------------------------- //

/// The deployment plane's typed refusals. A refused declaration, move,
/// or resolution changes nothing — the [`crate::learn::LearnError`]
/// discipline: refused operations are contract outcomes surfaced to the
/// caller, never silent no-ops.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum DeployError {
    /// A revision declaration outside the construction rules.
    #[error("invalid revision: {reason}")]
    InvalidRevision {
        /// The rule it broke.
        reason: &'static str,
    },

    /// A pin set naming one surface twice — a set, not a bag.
    #[error(
        "pin set names surface `{surface}` twice — a revision pins one candidate per surface; \
         a second naming is a configuration error, not a second pin"
    )]
    DuplicatePin {
        /// The doubly pinned surface.
        surface: SurfaceKey,
    },

    /// A revision's content could not be serialized for addressing —
    /// unreachable for well-formed content, surfaced rather than
    /// panicked (the `UndiffableContent` precedent).
    #[error("revision content could not be addressed: {0}")]
    UnaddressableContent(String),

    /// A stored revision fails its own content address.
    #[error(
        "revision declares id `{declared}` but its content addresses to `{derived}` — \
         tampered evidence; refused, never served"
    )]
    AddressMismatch {
        /// The id the record carried.
        declared: RevisionId,
        /// The id its content derives.
        derived: RevisionId,
    },

    /// A canary fraction outside `(0, 1]`.
    #[error(
        "canary fraction {fraction} is outside (0, 1] — zero binds nothing (a cleared slot \
         says that), and above one is not a fraction"
    )]
    InvalidFraction {
        /// The refused fraction.
        fraction: f64,
    },

    /// A secret name outside the naming rules (see
    /// [`validate_secret_name`]).
    #[error("invalid secret name {name:?}: {reason}")]
    InvalidSecretName {
        /// The refused name.
        name: String,
        /// The rule it broke.
        reason: &'static str,
    },
}

// --------------------------------------------------------------------- //
// Tests
// --------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::path::PathBuf;

    // ---------- golden-file machinery (the artifact.rs discipline) ----------
    //
    // Asserted here (unit tests beside the contracts) so the golden
    // fixtures under `tests/golden/` pin the new wire shapes without the
    // wave touching another test file. `UPDATE_GOLDEN=1` blesses an
    // intentional change — the diff is then the contract change under
    // review.

    fn golden_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
            .join(name)
    }

    fn assert_golden(name: &str, value: &impl Serialize) {
        let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
        let path = golden_path(name);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &rendered).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
        assert_eq!(
            rendered,
            expected,
            "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
             and review the diff",
            path.display()
        );
    }

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn author() -> ProvenanceAuthor {
        ProvenanceAuthor::Human {
            human_id: "amjad".into(),
        }
    }

    fn tag(name: &str) -> EnvironmentTag {
        EnvironmentTag::new(name).unwrap()
    }

    fn candidate_id(seed: char) -> CandidateId {
        CandidateId::from(seed.to_string().repeat(64))
    }

    fn revision(seed: char) -> DeploymentRevision {
        DeploymentRevision::new(
            RevisionContent {
                graph: "pipeline".into(),
                graph_hash: "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"
                    .into(),
                assistant: Some("support-agent".into()),
                source_environment: tag("staging"),
                pins: vec![
                    RegistryPin {
                        surface: SurfaceKey::new("model_settings:primary"),
                        candidate_id: candidate_id(seed),
                    },
                    RegistryPin {
                        surface: SurfaceKey::new("prompt:system"),
                        candidate_id: candidate_id('p'),
                    },
                ],
            },
            author(),
            ts(1_760_000_000_000),
        )
        .unwrap()
    }

    // ---------- construction and identity ----------

    #[test]
    fn content_addressing_converges_and_ignores_attribution() {
        // Identity is integrity: equal declarations converge on one id,
        // however they are ordered and whoever registers them — the pin
        // set sorts at construction, and the author lives outside the
        // address.
        let first = revision('a');
        let mut shuffled = first.content.clone();
        shuffled.pins.reverse();
        let second = DeploymentRevision::new(
            shuffled,
            ProvenanceAuthor::Human {
                human_id: "someone-else".into(),
            },
            ts(1_760_000_999_000),
        )
        .unwrap();
        assert_eq!(first.revision_id, second.revision_id);
        assert_ne!(first.author, second.author, "attribution is not identity");
        first.verify_address().unwrap();
    }

    #[test]
    fn a_changed_declaration_is_a_new_id() {
        let changed = DeploymentRevision::new(
            RevisionContent {
                graph_hash: "0".repeat(64),
                ..revision('a').content
            },
            author(),
            ts(1_760_000_000_000),
        )
        .unwrap();
        assert_ne!(changed.revision_id, revision('a').revision_id);
    }

    #[test]
    fn duplicate_pins_and_empty_graphs_are_refused() {
        let dup = DeploymentRevision::new(
            RevisionContent {
                pins: vec![
                    RegistryPin {
                        surface: SurfaceKey::new("prompt:system"),
                        candidate_id: candidate_id('a'),
                    },
                    RegistryPin {
                        surface: SurfaceKey::new("prompt:system"),
                        candidate_id: candidate_id('b'),
                    },
                ],
                ..revision('a').content
            },
            author(),
            ts(1_760_000_000_000),
        );
        assert!(
            matches!(dup, Err(DeployError::DuplicatePin { .. })),
            "one surface pins one candidate"
        );
        let empty = DeploymentRevision::new(
            RevisionContent {
                graph: String::new(),
                ..revision('a').content
            },
            author(),
            ts(1_760_000_000_000),
        );
        assert!(matches!(empty, Err(DeployError::InvalidRevision { .. })));
    }

    #[test]
    fn verify_address_refuses_tampering() {
        let mut tampered = revision('a');
        tampered.content.graph_hash = "f".repeat(64);
        assert!(matches!(
            tampered.verify_address(),
            Err(DeployError::AddressMismatch { .. })
        ));
    }

    // ---------- the pointer moves ----------

    #[test]
    fn promotion_moves_active_and_clears_the_canary() {
        let surface = deployment_surface(&tag("prod"));
        let mut pointer = DeploymentPointer::new(surface.clone());
        pointer.active = Some(RevisionId::from("a".repeat(64)));
        pointer.canary =
            Some(CanaryDeployment::new(RevisionId::from("b".repeat(64)), 0.1).unwrap());
        let promotion = RevisionPromotion {
            tenant: "default".into(),
            environment: tag("prod"),
            revision_id: RevisionId::from("b".repeat(64)),
            previous: pointer.active.clone(),
            author: author(),
            promoted_at: ts(1_760_000_100_000),
        };
        let moved = pointer.promoted(&promotion);
        assert_eq!(moved.active, Some(RevisionId::from("b".repeat(64))));
        assert_eq!(
            moved.canary, None,
            "a full promotion supersedes its experiment"
        );
    }

    #[test]
    fn rollback_repoints_byte_exact() {
        let surface = deployment_surface(&tag("prod"));
        let mut pointer = DeploymentPointer::new(surface);
        pointer.active = Some(RevisionId::from("b".repeat(64)));
        let rollback = RevisionRollback {
            tenant: "default".into(),
            environment: tag("prod"),
            from: RevisionId::from("b".repeat(64)),
            to: Some(RevisionId::from("a".repeat(64))),
            cause: "error rate".into(),
            author: author(),
            rolled_back_at: ts(1_760_000_200_000),
        };
        let moved = pointer.rolled_back(&rollback);
        assert_eq!(
            moved.active,
            Some(RevisionId::from("a".repeat(64))),
            "the restored revision is the one that served, not a reconstruction"
        );
        // Rolling back a revision the pointer does not serve moves nothing.
        let stray = pointer.rolled_back(&RevisionRollback {
            from: RevisionId::from("c".repeat(64)),
            ..rollback
        });
        assert_eq!(stray, pointer);
    }

    #[test]
    fn canary_fraction_bounds_are_enforced() {
        assert!(CanaryDeployment::new(RevisionId::from("a".repeat(64)), 0.0).is_err());
        assert!(CanaryDeployment::new(RevisionId::from("a".repeat(64)), 1.0).is_ok());
        assert!(CanaryDeployment::new(RevisionId::from("a".repeat(64)), 1.1).is_err());
    }

    #[test]
    fn the_canary_draw_is_seeded_and_reproducible() {
        // A recorded run re-derives its assignment from the pointer alone:
        // one draw over the same (surface, revision, run) triple, every
        // time; a different surface (staging vs prod) is an independent
        // draw.
        let revision = RevisionId::from("a".repeat(64));
        let mut pointer = DeploymentPointer::new(deployment_surface(&tag("staging")));
        pointer.active = Some(RevisionId::from("f".repeat(64)));
        pointer.canary = Some(CanaryDeployment::new(revision.clone(), 0.5).unwrap());
        let first = deployment_admission(&pointer, "run-1");
        let second = deployment_admission(&pointer, "run-1");
        assert_eq!(
            first, second,
            "the draw is a pure function of journaled facts"
        );
        let prod_pointer =
            DeploymentPointer::new(deployment_surface(&tag("prod"))).promoted(&RevisionPromotion {
                tenant: "default".into(),
                environment: tag("prod"),
                revision_id: RevisionId::from("f".repeat(64)),
                previous: None,
                author: author(),
                promoted_at: ts(1_760_000_000_000),
            });
        let mut prod_with_canary = prod_pointer;
        prod_with_canary.canary = Some(CanaryDeployment::new(revision, 0.5).unwrap());
        // Not asserting inequality (the draws could coincide); asserting
        // the mechanism: both answers come from their own surface's seed.
        assert!(deployment_admission(&prod_with_canary, "run-1").is_some());
        // A pointer serving nothing binds nothing.
        let empty = DeploymentPointer::new(deployment_surface(&tag("dev")));
        assert_eq!(deployment_admission(&empty, "run-1"), None);
    }

    #[test]
    fn secret_names_follow_the_registry_naming_rule() {
        assert!(validate_secret_name("database-url").is_ok());
        assert!(validate_secret_name("").is_err());
        assert!(
            validate_secret_name("db@prod").is_err(),
            "the scope separator is structural"
        );
        assert!(
            validate_secret_name("a/b").is_err(),
            "the tenant separator is structural"
        );
        assert_eq!(
            scoped_secret_name("database-url", &tag("staging")),
            "database-url@staging"
        );
    }

    // ---------- goldens ----------

    #[test]
    fn golden_deployment_event_kinds_shape() {
        // The plane's additive RunEventKind wire names (the
        // `artifact_event_kinds.json` discipline): pinned so no wire
        // shape lands unpinned — declared in wire order, appended after
        // wave 2's retention acts.
        use crate::record::RunEventKind;
        assert_golden(
            "deployment_event_kinds.json",
            &vec![
                RunEventKind::DeploymentResolved,
                RunEventKind::RevisionRegistered,
                RunEventKind::RevisionPromoted,
                RunEventKind::RevisionRolledBack,
                RunEventKind::EnvironmentDeclared,
                RunEventKind::EnvSecretSet,
                RunEventKind::EnvSecretRevoked,
                RunEventKind::EnvSecretDenied,
            ],
        );
    }

    #[test]
    fn golden_deployment_revision_shape() {
        assert_golden("deployment_revision.json", &revision('a'));
    }

    #[test]
    fn golden_environment_shape() {
        let environment = Environment {
            name: tag("prod"),
            gate: Some(GateDeclaration {
                policy: "r0.12-default".into(),
                dataset_version: "support-v3".into(),
            }),
            approval_required: true,
            created_by: author(),
            created_at: ts(1_760_000_000_000),
        };
        assert_golden("environment.json", &environment);
    }

    #[test]
    fn golden_deployment_pointer_shape() {
        // The full shape: both slots bound — active plus a 10% canary.
        let mut pointer = DeploymentPointer::new(deployment_surface(&tag("staging")));
        pointer.active = Some(RevisionId::from("a".repeat(64)));
        pointer.canary =
            Some(CanaryDeployment::new(RevisionId::from("b".repeat(64)), 0.1).unwrap());
        assert_golden("deployment_pointer.json", &pointer);
    }

    #[test]
    fn golden_deployment_resolved_shape() {
        // The journaled resolution: a run admitted to staging under the
        // canary slot, naming the bound revision and the pin-set digest.
        let resolved = DeploymentResolved {
            environment: tag("staging"),
            revision_id: revision('a').revision_id,
            pointer: PointerBinding::Canary,
            pin_set_digest: pin_set_digest(&revision('a').content.pins),
        };
        assert_golden("deployment_resolved.json", &resolved);
    }

    #[test]
    fn golden_revision_registration_shape() {
        assert_golden(
            "revision_registration.json",
            &RevisionRegistration {
                tenant: "default".into(),
                revision: revision('a'),
            },
        );
    }

    #[test]
    fn golden_environment_declaration_shape() {
        assert_golden(
            "environment_declaration.json",
            &EnvironmentDeclaration {
                tenant: "default".into(),
                environment: Environment {
                    name: tag("dev"),
                    gate: None,
                    approval_required: false,
                    created_by: author(),
                    created_at: ts(1_760_000_000_000),
                },
            },
        );
    }

    #[test]
    fn golden_revision_promotion_shape() {
        assert_golden(
            "revision_promotion.json",
            &RevisionPromotion {
                tenant: "default".into(),
                environment: tag("prod"),
                revision_id: revision('a').revision_id,
                previous: Some(RevisionId::from("f".repeat(64))),
                author: author(),
                promoted_at: ts(1_760_000_100_000),
            },
        );
    }

    #[test]
    fn golden_revision_rollback_shape() {
        assert_golden(
            "revision_rollback.json",
            &RevisionRollback {
                tenant: "default".into(),
                environment: tag("prod"),
                from: revision('a').revision_id,
                to: Some(RevisionId::from("f".repeat(64))),
                cause: "canary regression".into(),
                author: author(),
                rolled_back_at: ts(1_760_000_200_000),
            },
        );
    }

    #[test]
    fn golden_env_secret_record_shape() {
        assert_golden(
            "env_secret_record.json",
            &EnvSecretRecord {
                name: "database-url".into(),
                environment: tag("staging"),
                set_by: author(),
                created_at: ts(1_760_000_000_000),
                rotated_at: Some(ts(1_760_000_500_000)),
            },
        );
    }

    #[test]
    fn golden_stored_env_secret_shape() {
        // The custody shape: metadata plus the sealed envelope — hex
        // ciphertext fields, never bytes (fixed vectors pin the wire).
        assert_golden(
            "stored_env_secret.json",
            &StoredEnvSecret {
                record: EnvSecretRecord {
                    name: "database-url".into(),
                    environment: tag("prod"),
                    set_by: author(),
                    created_at: ts(1_760_000_000_000),
                    rotated_at: None,
                },
                envelope: SealedCredential {
                    format_version: 1,
                    key_id: "esk-0123456789abcdef".into(),
                    wrapped_data_key: "aa".repeat(48),
                    wrap_nonce: "bb".repeat(24),
                    nonce: "cc".repeat(24),
                    ciphertext: "dd".repeat(80),
                    sealed_at: ts(1_760_000_000_000),
                },
            },
        );
    }

    #[test]
    fn golden_env_secret_denial_shape() {
        assert_golden(
            "env_secret_denial.json",
            &EnvSecretDenial {
                tenant: "default".into(),
                name: "database-url".into(),
                requested_environment: tag("prod"),
                held_environment: tag("staging"),
                denied_at: ts(1_760_000_300_000),
            },
        );
    }
}
