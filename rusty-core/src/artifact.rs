//! The run artifact plane (R0.12 Operations Plane, wave 1): the records
//! that make a run's binary outputs operable — content-addressed,
//! lineage-carrying, retainable — plus the journaled commitment that
//! binds each one back into the run's evidence.
//!
//! Two artifact concepts share one word in this codebase, and the type
//! system keeps them apart. Registry artifacts
//! ([`crate::registry::ArtifactRecord`]) are human-authored *configuration*:
//! a commit is a candidate, governance is the learn pipeline, the bytes
//! are small and diffed. [`RunArtifact`] records are run-produced
//! *outputs*: generated files, images, audio, exported datasets. A run
//! artifact is not a candidate, has no promotion lifecycle, and is never
//! diffed; its governance is lineage, permissions, and retention. The
//! server surface is `/artifacts`, deliberately distinct from
//! `/registry/artifacts`, so the route grammar itself says which concept
//! a caller addresses.
//!
//! The record is deliberately thin because the bytes and the evidence
//! already have homes:
//!
//! - **Identity is integrity.** `artifact_id` is the lowercase hex
//!   SHA-256 of the bytes — exactly the [`crate::record::ArtifactRef`]
//!   rule, so a run artifact's address is the same digest the journal
//!   already stamps for spilled payloads. Two runs producing
//!   byte-identical outputs share one object, and a read re-hashes
//!   before it serves: a store that cannot prove its bytes is
//!   corruption, not data.
//! - **Lineage is what makes the plane evidence.** The journal already
//!   records the producing effect with its class, latency, and causal
//!   parentage, but nothing connected that effect to the file that
//!   outlived the run. [`ArtifactLineage`] is that join: the producing
//!   run id, the deterministic [`EffectId`] of the producing effect
//!   (re-derivable at audit through
//!   [`derive_effect_id`](crate::effects::derive_effect_id) — no new
//!   identity minting), and the journal event id whose output carried
//!   the reference. Recorded at commit, never edited.
//! - **The journal carries commitments, never bytes.** One additive
//!   [`RunEventKind::ArtifactCommitted`](crate::record::RunEventKind::ArtifactCommitted)
//!   event, payload [`ArtifactCommitment`], sits in the run's own
//!   journal, so the signed receipt's head covers it transitively: the
//!   audit walk is signed receipt → journal head → `ArtifactCommitted` →
//!   `EffectId` → the effect's journaled record → the bytes behind the
//!   address. Run artifacts are arbitrary bytes, potentially hundreds of
//!   megabytes; they do not belong in snapshots, so the journal records
//!   the reference and the commitment and the plane carries the rest.
//!
//! Wave 1 shipped the base record and the commit path; wave 2 grows the
//! plane's governance, all additively:
//!
//! - **Version accumulation** ([`append_artifact_version`]): a commit
//!   naming an already-taken name joins its sequence — the `versions`
//!   slot the wire shape carried from the first record, so accumulation
//!   is an append, never a shape change. Each version stays its own
//!   record under its own address; older versions serve by address with
//!   the sequence prefix they were committed under.
//! - **Previews** ([`derive_preview`]): derived on read, *never* stored
//!   — the `RegistryDiff` precedent. A stored preview is a second,
//!   divergent account of the same bytes; a kind that cannot be derived
//!   is an honest empty answer, not a placeholder.
//! - **The retention acts** ([`ArtifactPrune`], [`ArtifactRelease`],
//!   [`ArtifactUnavailability`]): the sweeper's journaled intention
//!   (before any byte moves — a crash mid-sweep leaves intentions
//!   auditable), the operator's release — the *only* path that prunes a
//!   receipt-pinned or `pinned` address, because shortening evidence
//!   retention is a governance decision with a name on it, never
//!   housekeeping — and the typed miss, journaled so "the record exists,
//!   the bytes do not" reads differently from "no such artifact" in
//!   exactly the way a retention audit needs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::effects::EffectId;
use crate::record::ArtifactRef;
use crate::registry::MAX_ARTIFACT_NAME_LEN;

// --------------------------------------------------------------------- //
// Media and retention — the closed enums
// --------------------------------------------------------------------- //

/// The artifact's media class: a closed, additively-evolved enum that
/// drives preview eligibility (Wave 2). The producer's declared media
/// *type* string travels beside it
/// ([`RunArtifact::media_type`]), preserved verbatim because the runtime
/// cannot certify a producer's claim, only record it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A generic file (text, JSON, binary without a richer class).
    File,
    /// A still image.
    Image,
    /// An audio recording.
    Audio,
    /// An exported dataset (rows, tables, archives of records).
    Data,
}

/// How long the bytes behind an artifact live. Declared at commit and
/// journaled with it; a change that would shorten live retention is a
/// governance act (Wave 2's journaled release), never housekeeping.
///
/// The default is [`RetentionPolicy::ReceiptBound`]: a signed receipt
/// commits to a journal head whose events name content addresses, and
/// pruning bytes an unexpired receipt still names would falsify the
/// receipt's *usefulness* — the chain verifies over events and survives,
/// but "the run produced `sha256:abc…`" is cold comfort when `abc…` was
/// swept. Retain-at-least-as-long-as-the-receipt is the stance that
/// keeps evidence whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum RetentionPolicy {
    /// Retain until an operator explicitly releases the pin (a journaled
    /// act, Wave 2).
    Pinned,
    /// Retain for at least this many days from commit.
    Days {
        /// The retention window, in whole days.
        days: u32,
    },
    /// Retain at least as long as any signed receipt whose journal
    /// references the address (the default).
    #[default]
    ReceiptBound,
}

// --------------------------------------------------------------------- //
// Versions and lineage
// --------------------------------------------------------------------- //

/// One entry in a named artifact's version sequence: the
/// [`crate::registry::ArtifactCommit`] discipline (append-only,
/// content-addressed) applied to bytes. The current version is the
/// last; older versions are addresses, retained under the same policy.
///
/// Wave 1 commits write exactly one entry — the committed object itself
/// — so the slot exists on the wire from the first record and version
/// accumulation (Wave 2) lands as appends, not a shape change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactVersion {
    /// The version's content address (lowercase hex SHA-256 of its
    /// bytes).
    pub sha256: String,

    /// The version's size in bytes.
    pub bytes: u64,

    /// When this version joined the sequence.
    pub committed_at: DateTime<Utc>,
}

/// The join that makes the plane evidence: where these bytes came from.
/// Recorded at commit, never edited — a later act (a retention release,
/// a sweep) journals its own event rather than rewriting the lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactLineage {
    /// The run that produced the artifact.
    pub run_id: String,

    /// The deterministic [`EffectId`] of the producing effect —
    /// re-derivable at audit from the journaled scope, kind, input hash,
    /// and key, so the lineage anchor needs no new identity minting.
    pub effect_id: EffectId,

    /// The journal event id whose output carried the reference to these
    /// bytes (the node output that spilled, or the event the declaring
    /// node parents its commitment to).
    pub event_id: String,
}

// --------------------------------------------------------------------- //
// The record
// --------------------------------------------------------------------- //

/// A run-produced output made operable: the plane's only persisted
/// entity.
///
/// Keyed by content address (`artifact_id`), tenant-scoped at the
/// metadata layer. Bytes are deliberately *not* tenant-namespaced —
/// content addressing makes byte storage global (two tenants producing
/// identical bytes share one object) — and the metadata layer is the
/// only path that lists or resolves, so a shared address grants no
/// cross-tenant read path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunArtifact {
    /// The content address: lowercase hex SHA-256 of the bytes, exactly
    /// the [`ArtifactRef`] rule. Identity is integrity.
    pub artifact_id: String,

    /// The logical name (`weekly-report`) versions accumulate under, or
    /// `None` for an address-only artifact: produced, referenced,
    /// retained, expired. The naming rules are the registry's
    /// ([`MAX_ARTIFACT_NAME_LEN`], no `@`, no `/`, no control
    /// characters), for the same reason: names ride in keys that tenant
    /// prefixes and environment tags already punctuate. Absent from the
    /// wire when unnamed — an address-only artifact carries no
    /// placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The media class (drives preview eligibility, Wave 2).
    pub media_kind: MediaKind,

    /// The producer-declared media type (`image/png`,
    /// `text/csv`), preserved verbatim: the runtime records the claim,
    /// it cannot certify it. Absent when the producer declared none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    /// Where these bytes came from (run, effect, event). Recorded at
    /// commit, never edited.
    pub lineage: ArtifactLineage,

    /// The name's version sequence, oldest first, append-only — present
    /// exactly for named artifacts. Wave 1 writes one entry (the
    /// committed object itself); Wave 2's accumulation appends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<ArtifactVersion>,

    /// The declared retention. Journaled with the commit; shortening it
    /// later is a governance act, not housekeeping.
    pub retention: RetentionPolicy,

    /// When the artifact was committed.
    pub created_at: DateTime<Utc>,
}

/// The journaled half of a commit: the output payload of one
/// [`RunEventKind::ArtifactCommitted`](crate::record::RunEventKind::ArtifactCommitted)
/// event in the producing run's own journal.
///
/// The event is the commitment, and it is what the signed receipt's head
/// covers — the bytes never enter the journal, so the payload names
/// everything an auditor needs to walk from the receipt to the bytes:
/// the address, the name and version index when named, the media kind,
/// the byte count, the producing [`EffectId`], and the declared
/// retention. Built together with the [`RunArtifact`] record by
/// [`commit_artifact`]: one derivation, two homes, so the journaled
/// commitment and the stored record can never diverge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactCommitment {
    /// The content address (the record's `artifact_id`).
    pub artifact_id: String,

    /// The logical name, when the artifact is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The version index this commit appended — present exactly when
    /// the artifact is named (the sequence position of the new head; `0`
    /// for the first version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,

    /// The media class.
    pub media_kind: MediaKind,

    /// The byte count (the [`ArtifactRef::bytes`] rule).
    pub bytes: u64,

    /// The producing effect's deterministic id.
    pub effect_id: EffectId,

    /// The retention declared at commit.
    pub retention: RetentionPolicy,
}

// --------------------------------------------------------------------- //
// The commit constructor
// --------------------------------------------------------------------- //

/// What a commit declares: everything the record and the journaled
/// commitment need, from whichever source the bytes arrived (an
/// SDK-declared output, or a journaled [`crate::record::PayloadRef::Artifact`]
/// the producing node opted into). Both paths build through
/// [`commit_artifact`], which is what makes "both write the same record
/// shape and journal the same event" structural rather than
/// conventional.
#[derive(Debug, Clone)]
pub struct CommitDeclaration {
    /// The content address and size of the committed bytes, as the byte
    /// store minted them.
    pub reference: ArtifactRef,

    /// The logical name versions accumulate under, when named.
    pub name: Option<String>,

    /// The media class.
    pub media_kind: MediaKind,

    /// The producer-declared media type string, when asserted.
    pub media_type: Option<String>,

    /// The producing run, effect, and journal event.
    pub lineage: ArtifactLineage,

    /// The declared retention (defaults to
    /// [`RetentionPolicy::ReceiptBound`] at the call sites, which is why
    /// this is not an `Option`).
    pub retention: RetentionPolicy,

    /// When the commit happened.
    pub committed_at: DateTime<Utc>,
}

/// Build the record and the journaled commitment for one commit — the
/// single constructor both commit paths share. Wave 1 writes the base
/// version sequence (one entry, index `0`); the append path that grows
/// it is Wave 2's.
///
/// Refusals are typed ([`ArtifactError`]) and change nothing: the
/// naming rules keep names route-addressable and unambiguous under
/// tagging and tenant prefixing, and the address rule keeps identity
/// honest — a commit over a malformed address would mint a record the
/// byte store can never verify.
pub fn commit_artifact(
    declaration: CommitDeclaration,
) -> Result<(RunArtifact, ArtifactCommitment), ArtifactError> {
    validate_artifact_address(&declaration.reference.sha256)?;
    if let Some(name) = &declaration.name {
        validate_artifact_name(name)?;
    }
    let version = declaration.name.as_ref().map(|_| ArtifactVersion {
        sha256: declaration.reference.sha256.clone(),
        bytes: declaration.reference.bytes,
        committed_at: declaration.committed_at,
    });
    let record = RunArtifact {
        artifact_id: declaration.reference.sha256.clone(),
        name: declaration.name.clone(),
        media_kind: declaration.media_kind,
        media_type: declaration.media_type.clone(),
        lineage: declaration.lineage,
        versions: version.into_iter().collect(),
        retention: declaration.retention,
        created_at: declaration.committed_at,
    };
    let commitment = ArtifactCommitment {
        artifact_id: declaration.reference.sha256,
        name: declaration.name,
        version: record
            .versions
            .len()
            .checked_sub(1)
            .map(|index| index as u64),
        media_kind: declaration.media_kind,
        bytes: declaration.reference.bytes,
        effect_id: record.lineage.effect_id.clone(),
        retention: declaration.retention,
    };
    Ok((record, commitment))
}

/// Build the record and the journaled commitment for a commit that
/// joins an existing name's version sequence (wave 2): the new head
/// record carries the prior sequence plus the new entry, and the
/// commitment names the appended index. Wave 1 answered this commit
/// with `409`; the sequence the wire shape carried from the first
/// record is what makes accumulation an append, not a migration.
///
/// Each version remains its own record under its own address — the
/// prior head's file is never edited (records are written once), so an
/// older version keeps serving by address with the sequence prefix it
/// was committed under. The name index re-points at the new head; the
/// history route reads the head's full sequence.
///
/// The same refusals as [`commit_artifact`] apply, plus
/// [`ArtifactError::VersionMismatch`] when the declaration does not
/// belong to `head`'s sequence — a version joins the sequence its name
/// owns, never another's.
pub fn append_artifact_version(
    head: &RunArtifact,
    declaration: CommitDeclaration,
) -> Result<(RunArtifact, ArtifactCommitment), ArtifactError> {
    validate_artifact_address(&declaration.reference.sha256)?;
    let Some(head_name) = &head.name else {
        return Err(ArtifactError::VersionMismatch {
            reason: "the sequence head is unnamed — versions accumulate under a name; an \
                     address-only artifact has no sequence to join",
        });
    };
    match &declaration.name {
        Some(name) if name == head_name => validate_artifact_name(name)?,
        _ => {
            return Err(ArtifactError::VersionMismatch {
                reason: "the declaration's name is not the sequence head's — a version \
                         joins the sequence its name owns",
            })
        }
    }
    let mut versions = head.versions.clone();
    versions.push(ArtifactVersion {
        sha256: declaration.reference.sha256.clone(),
        bytes: declaration.reference.bytes,
        committed_at: declaration.committed_at,
    });
    let record = RunArtifact {
        artifact_id: declaration.reference.sha256.clone(),
        name: head.name.clone(),
        media_kind: declaration.media_kind,
        media_type: declaration.media_type.clone(),
        lineage: declaration.lineage,
        versions,
        retention: declaration.retention,
        created_at: declaration.committed_at,
    };
    let commitment = ArtifactCommitment {
        artifact_id: declaration.reference.sha256,
        name: head.name.clone(),
        version: record
            .versions
            .len()
            .checked_sub(1)
            .map(|index| index as u64),
        media_kind: declaration.media_kind,
        bytes: declaration.reference.bytes,
        effect_id: record.lineage.effect_id.clone(),
        retention: declaration.retention,
    };
    Ok((record, commitment))
}

/// The artifact naming rules, enforced at commit: non-empty, bounded
/// ([`MAX_ARTIFACT_NAME_LEN`]), no leading or trailing whitespace, no
/// control characters, no `@` (the environment-tag separator), and no
/// `/` (the tenant id-prefix separator). The registry's rules verbatim
/// ([`crate::registry::ArtifactRecord::new`]), re-stated here because a
/// run artifact's name rides the same keys: tenant prefixes and
/// environment tags punctuate both, and a name that escapes one grammar
/// escapes the other.
fn validate_artifact_name(name: &str) -> Result<(), ArtifactError> {
    let refuse = |reason: &'static str| ArtifactError::InvalidName {
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

/// The address rule: 64 lowercase hex characters — the exact shape
/// [`crate::record::sha256_hex`] mints. Anything else is not a content
/// address this plane can verify, so it is refused at commit rather than
/// discovered at read.
fn validate_artifact_address(address: &str) -> Result<(), ArtifactError> {
    if address.len() == 64
        && address
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Ok(());
    }
    Err(ArtifactError::InvalidAddress {
        address: address.to_owned(),
        reason: "not 64 lowercase hex characters — the `sha256_hex` shape; identity is \
                 integrity, and an address outside the rule can never re-hash to itself",
    })
}

// --------------------------------------------------------------------- //
// The retention acts (wave 2): the payloads the deployment's artifact
// evidence chain journals
// --------------------------------------------------------------------- //

/// Why the sweeper pruned an address. The cause is part of the evidence:
/// a retention audit reads *which* rule fired, not just that bytes left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneCause {
    /// A `days(n)` policy elapsed and no verified signed receipt covered
    /// the address.
    Expired,
    /// A `receipt_bound` policy found no receipt to be bound to.
    Unbound,
    /// Every remaining pin on the address was a released one.
    Released,
}

/// The journaled half of a sweep prune: the output payload of one
/// [`RunEventKind::ArtifactPruned`](crate::record::RunEventKind::ArtifactPruned)
/// event on the deployment's artifact evidence chain. Journaled *before*
/// the bytes are deleted, so a crash mid-sweep leaves the intention
/// auditable and the bytes recoverable — the scan-and-prune step can
/// never leave a deletion nothing recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactPrune {
    /// The pruned content address.
    pub artifact_id: String,

    /// The logical name, when the pruned record was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Which retention rule fired.
    pub cause: PruneCause,

    /// When the sweep journaled the intention.
    pub swept_at: DateTime<Utc>,
}

/// The journaled half of the retention-release act: the output payload
/// of one
/// [`RunEventKind::ArtifactRetentionReleased`](crate::record::RunEventKind::ArtifactRetentionReleased)
/// event on the deployment's artifact evidence chain. The release is the
/// *only* path that prunes an address a live signed receipt covers or a
/// `pinned` policy holds — shortening evidence retention is a governance
/// decision with a name on it, so the act carries the operator's
/// identity and journals before any byte moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRelease {
    /// The released content address.
    pub artifact_id: String,

    /// The tenant whose record held the pin (the chain is
    /// deployment-wide; the tenant is audit metadata, not scoping).
    pub tenant: String,

    /// The logical name, when the released record was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The operator identity that released the pin (`human:{id}`, the
    /// registry commit discipline).
    pub released_by: String,

    /// The operator's stated reason, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// When the release journaled.
    pub released_at: DateTime<Utc>,
}

/// Which read surface observed the miss — the audit reads whether the
/// bytes themselves were asked for or a derivation was attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailabilitySurface {
    /// The byte read (`GET /artifacts/{id}/bytes`) — the surface an
    /// exact replay's byte fetch fails closed on.
    Bytes,
    /// The preview derivation (`GET /artifacts/{id}/preview`).
    Preview,
}

/// The journaled half of the typed miss: the output payload of one
/// [`RunEventKind::ArtifactUnavailable`](crate::record::RunEventKind::ArtifactUnavailable)
/// event on the deployment's artifact evidence chain. The record exists,
/// the bytes do not — that difference is exactly what a retention audit
/// needs, so the miss is typed (`410 artifact_unavailable`, never 404)
/// and journaled. Journaling lands on the deployment chain rather than
/// the producing run's journal so the miss evidence never rewrites a
/// journal a signed receipt already covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactUnavailability {
    /// The missed content address.
    pub artifact_id: String,

    /// The tenant whose record was read.
    pub tenant: String,

    /// The logical name, when the record was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The read surface that observed the miss.
    pub surface: UnavailabilitySurface,

    /// When the miss was observed.
    pub observed_at: DateTime<Utc>,
}

// --------------------------------------------------------------------- //
// Previews (wave 2): derived on read, never stored
// --------------------------------------------------------------------- //

/// The byte bound on a text or JSON preview's source window (4 KB — the
/// journal's `INLINE_PAYLOAD_MAX_BYTES` discipline: a preview is an
/// operator-scale read, so its derivation is bounded the same way the
/// journal bounds inline payloads).
pub const PREVIEW_TEXT_MAX_BYTES: usize = 4096;

/// The longest edge a derived thumbnail may have. Integer-factor nearest
/// sampling keeps the derivation cheap and exact — a thumbnail is an
/// operator's glance, not a rendering pipeline.
pub const PREVIEW_THUMBNAIL_MAX_EDGE: u32 = 64;

/// The bucket count of a derived waveform's peak envelope.
pub const PREVIEW_WAVEFORM_BUCKETS: usize = 64;

/// What a read derived from the bytes — the `RegistryDiff` precedent
/// applied to media: computed on read, *never* stored. A stored preview
/// would be a second, divergent account of the same bytes; derivation
/// keeps one source of truth, and a kind that cannot be derived answers
/// [`ArtifactPreview::Empty`] — an honest empty, not a placeholder.
///
/// The derivations are deliberately dependency-free, which bounds what
/// they cover, stated per kind:
///
/// - `file` / `data`: a bounded UTF-8 window ([`ArtifactPreview::Text`]),
///   or the parsed document when the whole payload fits the window and
///   is JSON ([`ArtifactPreview::Json`]). Binary bytes are not text —
///   they answer `Empty`.
/// - `image`: a real downscaled thumbnail ([`ArtifactPreview::Image`])
///   for the formats decodable without a codec dependency — uncompressed
///   BMP (24/32-bit `BI_RGB`) and binary PNM (P6/P5). Compressed formats
///   (PNG, JPEG, GIF, …) answer `Empty`: a codec dependency is the
///   measured-need seam the design's preview question reserves, not
///   something to half-parse.
/// - `audio`: waveform metadata ([`ArtifactPreview::Audio`]) for RIFF/WAVE
///   PCM (8/16-bit) — duration, rate, channels, and a bounded peak
///   envelope. Anything else answers `Empty`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactPreview {
    /// A bounded UTF-8 window over the source bytes.
    Text {
        /// The window's text (lossless: only whole characters).
        text: String,
        /// Whether the source extends past the window.
        truncated: bool,
        /// The source size in bytes.
        source_bytes: u64,
    },

    /// The parsed JSON document (only when it fits the window whole — a
    /// preview is bounded, so a larger document degrades to `Text`,
    /// never to a partial parse).
    Json {
        /// The parsed document.
        value: serde_json::Value,
        /// The source size in bytes.
        source_bytes: u64,
    },

    /// A downscaled thumbnail, re-encoded as a binary PPM (P6) and
    /// carried hex-encoded (the repo's dependency-free byte-on-JSON
    /// codec — the commit path's `bytes_hex` convention).
    Image {
        /// The decoded source format (`bmp` or `pnm`).
        format: String,
        /// The source dimensions.
        width: u32,
        /// The source dimensions.
        height: u32,
        /// The thumbnail dimensions (integer-factor downscale, nearest
        /// sampling).
        thumb_width: u32,
        /// The thumbnail dimensions.
        thumb_height: u32,
        /// The thumbnail as a P6 PPM, hex-encoded.
        pixels_ppm_hex: String,
    },

    /// Waveform metadata for RIFF/WAVE PCM: the envelope an operator
    /// glances at, derived — the audio bytes themselves are never
    /// re-encoded.
    Audio {
        /// The decoded container (`wav`).
        format: String,
        /// The clip length in whole milliseconds.
        duration_ms: u64,
        /// The sample rate in Hz.
        sample_rate: u32,
        /// The channel count.
        channels: u16,
        /// The frame count (samples per channel).
        frames: u64,
        /// The peak envelope: [`PREVIEW_WAVEFORM_BUCKETS`] buckets, each
        /// the loudest absolute sample in its window on the 16-bit scale
        /// (`0..=65535`), max across channels.
        peaks: Vec<u16>,
    },

    /// The honest empty answer: this media kind (or these bytes, under
    /// the dependency-free derivations above) yields no preview. Carries
    /// the reason so the answer is attributable, never a placeholder.
    Empty {
        /// Why nothing was derivable.
        reason: String,
    },
}

/// Derive the preview for `media_kind` over `bytes` — the single
/// derivation the preview route serves, so the answer is a pure function
/// of the bytes and can never drift from them.
pub fn derive_preview(media_kind: MediaKind, bytes: &[u8]) -> ArtifactPreview {
    let source_bytes = bytes.len() as u64;
    match media_kind {
        MediaKind::File | MediaKind::Data => derive_text_preview(bytes, source_bytes),
        MediaKind::Image => derive_image_preview(bytes),
        MediaKind::Audio => derive_audio_preview(bytes),
    }
}

/// The text/JSON rule: a whole document that fits the window and parses
/// as JSON is `Json`; any valid UTF-8 window is `Text`; interior-invalid
/// bytes are binary, and binary is not text.
fn derive_text_preview(bytes: &[u8], source_bytes: u64) -> ArtifactPreview {
    if bytes.len() <= PREVIEW_TEXT_MAX_BYTES {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
            return ArtifactPreview::Json {
                value,
                source_bytes,
            };
        }
    }
    let window = &bytes[..bytes.len().min(PREVIEW_TEXT_MAX_BYTES)];
    let truncated = bytes.len() > PREVIEW_TEXT_MAX_BYTES;
    match std::str::from_utf8(window) {
        Ok(text) => ArtifactPreview::Text {
            text: text.to_owned(),
            truncated,
            source_bytes,
        },
        Err(error) if truncated && error.error_len().is_none() => {
            // The window clipped a multi-byte character at its end: serve
            // up to the boundary — the window stays lossless.
            let text = std::str::from_utf8(&window[..error.valid_up_to()])
                .expect("valid_up_to is a char boundary");
            ArtifactPreview::Text {
                text: text.to_owned(),
                truncated: true,
                source_bytes,
            }
        }
        Err(_) => ArtifactPreview::Empty {
            reason: "the bytes are not valid UTF-8 — binary content yields no text \
                     preview, and a placeholder would pretend otherwise"
                .to_owned(),
        },
    }
}

/// The image rule: decode what is decodable without a codec dependency,
/// downscale by an integer factor with nearest sampling, re-encode as
/// P6. Anything else is the honest empty.
fn derive_image_preview(bytes: &[u8]) -> ArtifactPreview {
    let decoded = decode_bmp(bytes)
        .map(|(width, height, rgb)| ("bmp", width, height, rgb))
        .or_else(|| decode_pnm(bytes).map(|(width, height, rgb)| ("pnm", width, height, rgb)));
    let Some((format, width, height, rgb)) = decoded else {
        return ArtifactPreview::Empty {
            reason: "only uncompressed BMP (24/32-bit) and binary PNM (P6/P5) decode \
                     without a codec dependency — a compressed format is the measured-need \
                     seam, not something to half-parse"
                .to_owned(),
        };
    };
    let factor = (width.max(height))
        .div_ceil(PREVIEW_THUMBNAIL_MAX_EDGE)
        .max(1);
    let thumb_width = (width / factor).max(1);
    let thumb_height = (height / factor).max(1);
    let mut ppm = format!("P6\n{thumb_width} {thumb_height}\n255\n").into_bytes();
    for y in 0..thumb_height {
        for x in 0..thumb_width {
            let source = ((y * factor) * width + (x * factor)) as usize * 3;
            ppm.extend_from_slice(&rgb[source..source + 3]);
        }
    }
    ArtifactPreview::Image {
        format: format.to_owned(),
        width,
        height,
        thumb_width,
        thumb_height,
        pixels_ppm_hex: crate::broker::hex_encode(&ppm),
    }
}

/// A decoded raster: width, height, and tightly packed RGB bytes.
type Raster = (u32, u32, Vec<u8>);

fn read_u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Decode an uncompressed Windows BMP (`BM`, `BI_RGB`, 24 or 32 bits per
/// pixel) into RGB. Rows are bottom-up unless the height is negative;
/// 24-bit rows pad to a 4-byte boundary. Anything richer (compression,
/// palettes, bitfields) answers `None` — partial decoding guesses, and
/// guessing is not derivation.
fn decode_bmp(bytes: &[u8]) -> Option<Raster> {
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return None;
    }
    let data_offset = read_u32_le(bytes, 10)? as usize;
    let dib_size = read_u32_le(bytes, 14)? as usize;
    if dib_size < 40 {
        return None;
    }
    let width = read_u32_le(bytes, 18)?;
    let raw_height = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    let bpp = read_u16_le(bytes, 28)?;
    let compression = read_u32_le(bytes, 30)?;
    if compression != 0 || width == 0 || height == 0 || !matches!(bpp, 24 | 32) {
        return None;
    }
    let pixel_bytes = (bpp / 8) as usize;
    let stride = if bpp == 24 {
        (width as usize * 3).div_ceil(4) * 4
    } else {
        width as usize * 4
    };
    let needed = data_offset.checked_add(stride.checked_mul(height as usize)?)?;
    if bytes.len() < needed {
        return None;
    }
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for row in 0..height as usize {
        let source_row = if top_down {
            row
        } else {
            height as usize - 1 - row
        };
        let base = data_offset + source_row * stride;
        for x in 0..width as usize {
            let at = base + x * pixel_bytes;
            // BMP stores BGR(A); the preview speaks RGB.
            rgb.push(bytes[at + 2]);
            rgb.push(bytes[at + 1]);
            rgb.push(bytes[at]);
        }
    }
    Some((width, height, rgb))
}

/// Decode a binary PNM (P6 color, P5 grayscale; maxval 255) into RGB.
/// The header is whitespace-separated ASCII; exactly one whitespace byte
/// follows the maxval. Comments (`# …`) are honored between tokens, per
/// the format. Anything else (ASCII PNM, 16-bit maxval) answers `None`.
fn decode_pnm(bytes: &[u8]) -> Option<Raster> {
    let color = match bytes.first()? {
        b'P' => match bytes.get(1)? {
            b'6' => true,
            b'5' => false,
            _ => return None,
        },
        _ => return None,
    };
    let mut cursor = 2;
    let mut tokens = [0u32; 3];
    for token in &mut tokens {
        // Skip whitespace and comments before each token.
        loop {
            match bytes.get(cursor)? {
                b'#' => {
                    while bytes.get(cursor).is_some_and(|b| *b != b'\n') {
                        cursor += 1;
                    }
                }
                b if b.is_ascii_whitespace() => cursor += 1,
                _ => break,
            }
        }
        let start = cursor;
        while bytes.get(cursor).is_some_and(|b| b.is_ascii_digit()) {
            cursor += 1;
        }
        *token = std::str::from_utf8(bytes.get(start..cursor)?)
            .ok()?
            .parse()
            .ok()?;
    }
    let [width, height, maxval] = tokens;
    if width == 0 || height == 0 || maxval != 255 {
        return None;
    }
    // Exactly one whitespace byte separates the header from the raster.
    if !bytes.get(cursor)?.is_ascii_whitespace() {
        return None;
    }
    cursor += 1;
    let pixels = width as usize * height as usize;
    let mut rgb = Vec::with_capacity(pixels * 3);
    if color {
        let data = bytes.get(cursor..cursor + pixels * 3)?;
        rgb.extend_from_slice(data);
    } else {
        let data = bytes.get(cursor..cursor + pixels)?;
        for gray in data {
            rgb.extend_from_slice(&[*gray, *gray, *gray]);
        }
    }
    Some((width, height, rgb))
}

/// The audio rule: RIFF/WAVE PCM (8 or 16 bits) yields duration, rate,
/// channels, frames, and a bounded peak envelope. Any other container or
/// encoding is the honest empty.
fn derive_audio_preview(bytes: &[u8]) -> ArtifactPreview {
    match decode_wav(bytes) {
        Some((sample_rate, channels, frames, peaks)) => ArtifactPreview::Audio {
            format: "wav".to_owned(),
            duration_ms: frames.saturating_mul(1000) / u64::from(sample_rate.max(1)),
            sample_rate,
            channels,
            frames,
            peaks,
        },
        None => ArtifactPreview::Empty {
            reason: "only RIFF/WAVE PCM (8/16-bit) decodes without a codec dependency — \
                     a compressed or foreign container yields no waveform metadata"
                .to_owned(),
        },
    }
}

/// Parse a RIFF/WAVE PCM stream into its format fields and a peak
/// envelope of [`PREVIEW_WAVEFORM_BUCKETS`] buckets (the loudest absolute
/// sample per window, max across channels, on the 16-bit scale).
/// Chunked layout is honored (unknown chunks are skipped on their padded
/// length); non-PCM encodings, unusual bit depths, and truncated bodies
/// answer `None`.
fn decode_wav(bytes: &[u8]) -> Option<(u32, u16, u64, Vec<u16>)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut cursor = 12;
    let mut format: Option<(u16, u32, u16, u16)> = None; // (channels, rate, bits, align)
    let mut data: Option<&[u8]> = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = read_u32_le(bytes, cursor + 4)? as usize;
        let body = bytes.get(cursor + 8..cursor + 8 + size)?;
        match id {
            b"fmt " => {
                if size < 16 {
                    return None;
                }
                let audio_format = read_u16_le(body, 0)?;
                let channels = read_u16_le(body, 2)?;
                let sample_rate = read_u32_le(body, 4)?;
                let block_align = read_u16_le(body, 12)?;
                let bits = read_u16_le(body, 14)?;
                if audio_format != 1 || channels == 0 || !matches!(bits, 8 | 16) {
                    return None;
                }
                format = Some((channels, sample_rate, bits, block_align));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks pad to an even length.
        cursor += 8 + size + (size % 2);
    }
    let (channels, sample_rate, bits, block_align) = format?;
    let data = data?;
    let align = usize::from(block_align).max(usize::from(channels) * usize::from(bits / 8));
    let frames = (data.len() / align) as u64;
    if frames == 0 {
        return None;
    }
    let mut peaks = vec![0u16; PREVIEW_WAVEFORM_BUCKETS];
    for frame in 0..frames {
        let bucket = ((frame * PREVIEW_WAVEFORM_BUCKETS as u64) / frames) as usize;
        let base = frame as usize * align;
        for channel in 0..usize::from(channels) {
            let at = base + channel * usize::from(bits / 8);
            let magnitude = match bits {
                // 8-bit PCM is unsigned, midpoint 128: the magnitude is
                // the absolute offset from the midpoint, on the 16-bit
                // scale.
                8 => u16::from(data[at].abs_diff(128)) * 256,
                _ => {
                    let sample = i16::from_le_bytes([data[at], data[at + 1]]);
                    (sample as i32).unsigned_abs().min(u16::MAX as u32) as u16
                }
            };
            peaks[bucket] = peaks[bucket].max(magnitude);
        }
    }
    Some((sample_rate, channels, frames, peaks))
}

/// The artifact plane's typed refusals. A refused commit changes
/// nothing — the [`crate::registry::RegistryError`] discipline: refused
/// operations are contract outcomes surfaced to the caller, never
/// silent no-ops.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ArtifactError {
    /// An artifact name outside the naming rules (see
    /// [`commit_artifact`]).
    #[error("invalid artifact name {name:?}: {reason}")]
    InvalidName {
        /// The refused name.
        name: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A content address outside the `sha256_hex` shape.
    #[error("invalid artifact address {address:?}: {reason}")]
    InvalidAddress {
        /// The refused address.
        address: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A version append against a sequence the declaration does not
    /// belong to (see [`append_artifact_version`]) — an unnamed head, or
    /// a name that is not the head's. Refused, never redirected: a
    /// version joins the sequence its name owns.
    #[error("version append refused: {reason}")]
    VersionMismatch {
        /// The rule the append broke.
        reason: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Clock, EventDraft, Journal};
    use crate::record::{sha256_hex, Effect, RunEventKind};
    use serde::Serialize;
    use serde_json::json;
    use std::path::PathBuf;

    // ---------- golden-file machinery (the tests/registry.rs discipline) ----------
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

    fn effect_id() -> EffectId {
        crate::effects::derive_effect_id(
            "run-9",
            "render_report",
            &sha256_hex(b"weekly"),
            Some("render:9"),
        )
    }

    fn lineage() -> ArtifactLineage {
        ArtifactLineage {
            run_id: "run-9".into(),
            effect_id: effect_id(),
            event_id: "run-9:7".into(),
        }
    }

    fn named_commitment_pair() -> (RunArtifact, ArtifactCommitment) {
        commit_artifact(CommitDeclaration {
            reference: ArtifactRef {
                sha256: "a".repeat(64),
                bytes: 41_312,
            },
            name: Some("weekly-report".into()),
            media_kind: MediaKind::Image,
            media_type: Some("image/png".into()),
            lineage: lineage(),
            retention: RetentionPolicy::Days { days: 30 },
            committed_at: ts(1_760_000_000_000),
        })
        .unwrap()
    }

    // ---------- goldens ----------

    #[test]
    fn golden_artifact_event_kinds_shape() {
        // The plane's additive RunEventKind wire names (the
        // `registry_event_kinds.json` discipline): pinned so no wire
        // shape lands unpinned. Wave 2 appends the retention acts after
        // `artifact_committed`, per the additive evolution rule every
        // variant since R0.6 followed — declared in wire order.
        assert_golden(
            "artifact_event_kinds.json",
            &vec![
                RunEventKind::ArtifactCommitted,
                RunEventKind::ArtifactPruned,
                RunEventKind::ArtifactRetentionReleased,
                RunEventKind::ArtifactUnavailable,
            ],
        );
    }

    #[test]
    fn golden_run_artifact_shape() {
        // The full record: named, media-typed, day-retained, one base
        // version, lineage attached.
        let (record, _) = named_commitment_pair();
        assert_golden("run_artifact.json", &record);
    }

    #[test]
    fn golden_run_artifact_unnamed_shape() {
        // The sparse wire: unnamed, no declared media type, the default
        // retention — `name`, `media_type`, and `versions` are absent
        // (not null), so the address-only artifact carries no
        // placeholders.
        let (record, _) = commit_artifact(CommitDeclaration {
            reference: ArtifactRef {
                sha256: "b".repeat(64),
                bytes: 973,
            },
            name: None,
            media_kind: MediaKind::File,
            media_type: None,
            lineage: lineage(),
            retention: RetentionPolicy::default(),
            committed_at: ts(1_760_000_000_000),
        })
        .unwrap();
        assert_golden("run_artifact_unnamed.json", &record);
    }

    #[test]
    fn golden_artifact_commitment_shape() {
        // The journaled payload the `artifact_committed` event carries.
        let (_, commitment) = named_commitment_pair();
        assert_golden("artifact_commitment.json", &commitment);
    }

    #[test]
    fn golden_retention_policy_shape() {
        // All three variants in declaration order: the policy names and
        // the `days` payload are the contract.
        assert_golden(
            "retention_policy.json",
            &vec![
                RetentionPolicy::Pinned,
                RetentionPolicy::Days { days: 30 },
                RetentionPolicy::ReceiptBound,
            ],
        );
    }

    // ---------- the commit constructor ----------

    #[test]
    fn commit_builds_record_and_payload_from_one_derivation() {
        let (record, commitment) = named_commitment_pair();
        // Two homes, one derivation: the journaled commitment and the
        // stored record agree on every shared field by construction.
        assert_eq!(record.artifact_id, commitment.artifact_id);
        assert_eq!(record.name, commitment.name);
        assert_eq!(record.media_kind, commitment.media_kind);
        assert_eq!(record.retention, commitment.retention);
        assert_eq!(record.lineage.effect_id, commitment.effect_id);
        assert_eq!(record.versions.len(), 1);
        assert_eq!(record.versions[0].sha256, record.artifact_id);
        assert_eq!(record.versions[0].bytes, commitment.bytes);
        assert_eq!(commitment.version, Some(0));
    }

    #[test]
    fn commit_refuses_the_naming_rules() {
        for name in ["", "  padded", "has@tag", "tenant/escape", "ctl\tchar"] {
            let declaration = CommitDeclaration {
                reference: ArtifactRef {
                    sha256: "a".repeat(64),
                    bytes: 1,
                },
                name: Some(name.into()),
                media_kind: MediaKind::File,
                media_type: None,
                lineage: lineage(),
                retention: RetentionPolicy::default(),
                committed_at: ts(1_760_000_000_000),
            };
            assert!(
                matches!(
                    commit_artifact(declaration),
                    Err(ArtifactError::InvalidName { .. })
                ),
                "name {name:?} must be refused"
            );
        }
    }

    #[test]
    fn commit_refuses_a_malformed_address() {
        for address in ["", "abc", &"A".repeat(64), &"z".repeat(64)] {
            let declaration = CommitDeclaration {
                reference: ArtifactRef {
                    sha256: address.to_string(),
                    bytes: 1,
                },
                name: None,
                media_kind: MediaKind::File,
                media_type: None,
                lineage: lineage(),
                retention: RetentionPolicy::default(),
                committed_at: ts(1_760_000_000_000),
            };
            assert!(
                matches!(
                    commit_artifact(declaration),
                    Err(ArtifactError::InvalidAddress { .. })
                ),
                "address {address:?} must be refused"
            );
        }
    }

    #[test]
    fn sparse_wire_omits_absent_slots() {
        // Additive-evolution insurance: the unnamed record's optional
        // slots are absent on the wire (not null), and a payload without
        // them deserializes with the defaults the record was built from.
        let (record, _) = commit_artifact(CommitDeclaration {
            reference: ArtifactRef {
                sha256: "c".repeat(64),
                bytes: 7,
            },
            name: None,
            media_kind: MediaKind::Data,
            media_type: None,
            lineage: lineage(),
            retention: RetentionPolicy::Pinned,
            committed_at: ts(1_760_000_000_000),
        })
        .unwrap();
        let wire = serde_json::to_value(&record).unwrap();
        assert!(wire.get("name").is_none());
        assert!(wire.get("media_type").is_none());
        assert!(wire.get("versions").is_none());
        let back: RunArtifact = serde_json::from_value(wire).unwrap();
        assert_eq!(back, record);
    }

    // ---------- the journaled half ----------

    #[test]
    fn artifact_committed_journals_and_reverifies() {
        // The commitment event sits in the run's journal under the head
        // hash: snapshot, re-verify, and the payload resolves back into
        // the typed commitment — the audit walk's first hop.
        let (_, commitment) = named_commitment_pair();
        let journal = Journal::new("run-9", "thread-1", Clock::System);
        journal.record(
            EventDraft::new(RunEventKind::ArtifactCommitted, Effect::Pure)
                .output(serde_json::to_value(&commitment).unwrap())
                .parent("run-9:7"),
        );
        let snapshot = journal.snapshot();
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let parsed = serde_json::from_slice(&bytes).unwrap();
        let rebuilt = Journal::from_snapshot(parsed, Clock::System).unwrap();
        let event = &rebuilt.events()[0];
        assert_eq!(event.kind, RunEventKind::ArtifactCommitted);
        assert_eq!(event.parent.as_deref(), Some("run-9:7"));
        let resolved: ArtifactCommitment =
            serde_json::from_value(rebuilt.resolve(event.output.as_ref().unwrap()).unwrap())
                .unwrap();
        assert_eq!(resolved, commitment);
    }

    #[test]
    fn artifact_committed_payload_survives_a_mixed_journal() {
        // A journal holding pre-R0.12 events beside the new variant
        // round-trips whole: the additive variant changes nothing the
        // older events deserialize from.
        let (_, commitment) = named_commitment_pair();
        let journal = Journal::new("run-9", "thread-1", Clock::System);
        journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        journal.record(
            EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
                .node("render")
                .output(json!({"report": "weekly"})),
        );
        journal.record(
            EventDraft::new(RunEventKind::ArtifactCommitted, Effect::Pure)
                .output(serde_json::to_value(&commitment).unwrap()),
        );
        let snapshot = journal.snapshot();
        let rebuilt = Journal::from_snapshot(
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap(),
            Clock::System,
        )
        .unwrap();
        assert_eq!(rebuilt.len(), 3);
        assert_eq!(rebuilt.events()[2].kind, RunEventKind::ArtifactCommitted);
    }

    // ---------- wave 2: version accumulation ----------

    /// Build a three-version sequence by appending twice onto the named
    /// base commit; returns the newest head and its commitment.
    fn versioned_head() -> (RunArtifact, ArtifactCommitment) {
        let (base, _) = named_commitment_pair();
        let (v2, _) = append_artifact_version(
            &base,
            CommitDeclaration {
                reference: ArtifactRef {
                    sha256: "d".repeat(64),
                    bytes: 41_990,
                },
                name: Some("weekly-report".into()),
                media_kind: MediaKind::Image,
                media_type: Some("image/png".into()),
                lineage: lineage(),
                retention: RetentionPolicy::Days { days: 30 },
                committed_at: ts(1_760_100_000_000),
            },
        )
        .unwrap();
        append_artifact_version(
            &v2,
            CommitDeclaration {
                reference: ArtifactRef {
                    sha256: "e".repeat(64),
                    bytes: 42_104,
                },
                name: Some("weekly-report".into()),
                media_kind: MediaKind::Image,
                media_type: Some("image/png".into()),
                lineage: lineage(),
                retention: RetentionPolicy::Days { days: 30 },
                committed_at: ts(1_760_200_000_000),
            },
        )
        .unwrap()
    }

    #[test]
    fn golden_run_artifact_versioned_shape() {
        // The accumulated record: three entries, oldest first, each its
        // own address — the append-only `ArtifactCommit` discipline over
        // bytes, pinned on the wire.
        let (head, _) = versioned_head();
        assert_golden("run_artifact_versioned.json", &head);
    }

    #[test]
    fn golden_artifact_prune_shape() {
        // The sweeper's journaled intention: address, name, cause,
        // instant — all three causes in declaration order.
        assert_golden(
            "artifact_prune.json",
            &vec![
                ArtifactPrune {
                    artifact_id: "a".repeat(64),
                    name: Some("weekly-report".into()),
                    cause: PruneCause::Expired,
                    swept_at: ts(1_760_300_000_000),
                },
                ArtifactPrune {
                    artifact_id: "b".repeat(64),
                    name: None,
                    cause: PruneCause::Unbound,
                    swept_at: ts(1_760_300_000_000),
                },
                ArtifactPrune {
                    artifact_id: "c".repeat(64),
                    name: None,
                    cause: PruneCause::Released,
                    swept_at: ts(1_760_300_000_000),
                },
            ],
        );
    }

    #[test]
    fn golden_artifact_release_shape() {
        // The operator's journaled release: attributed, optional reason
        // present here; the sparse wire (no reason, unnamed) is covered
        // by `retention_acts_omit_absent_slots`.
        assert_golden(
            "artifact_release.json",
            &ArtifactRelease {
                artifact_id: "a".repeat(64),
                tenant: "acme".into(),
                name: Some("weekly-report".into()),
                released_by: "human:amjad".into(),
                reason: Some("evidence window closed by counsel".into()),
                released_at: ts(1_760_300_000_000),
            },
        );
    }

    #[test]
    fn golden_artifact_unavailability_shape() {
        // The journaled miss: both surfaces, so the audit reads which
        // read failed closed.
        assert_golden(
            "artifact_unavailability.json",
            &vec![
                ArtifactUnavailability {
                    artifact_id: "a".repeat(64),
                    tenant: "acme".into(),
                    name: Some("weekly-report".into()),
                    surface: UnavailabilitySurface::Bytes,
                    observed_at: ts(1_760_300_000_000),
                },
                ArtifactUnavailability {
                    artifact_id: "a".repeat(64),
                    tenant: "acme".into(),
                    name: Some("weekly-report".into()),
                    surface: UnavailabilitySurface::Preview,
                    observed_at: ts(1_760_300_100_000),
                },
            ],
        );
    }

    #[test]
    fn version_append_accumulates_and_indexes_the_new_head() {
        let (head, commitment) = versioned_head();
        assert_eq!(head.versions.len(), 3);
        assert_eq!(head.versions[0].sha256, "a".repeat(64));
        assert_eq!(head.versions[1].sha256, "d".repeat(64));
        assert_eq!(head.versions[2].sha256, "e".repeat(64));
        // The current version is the last, and it is the record itself.
        assert_eq!(head.artifact_id, head.versions[2].sha256);
        assert_eq!(commitment.version, Some(2));
        assert_eq!(commitment.name.as_deref(), Some("weekly-report"));
        // The append keeps the name, mints fresh lineage/retention from
        // the declaration, and never edits the base record.
        assert_eq!(head.name.as_deref(), Some("weekly-report"));
    }

    #[test]
    fn version_append_refuses_a_foreign_or_unnamed_sequence() {
        let (base, _) = named_commitment_pair();
        let declaration = |name: Option<&str>| CommitDeclaration {
            reference: ArtifactRef {
                sha256: "d".repeat(64),
                bytes: 1,
            },
            name: name.map(str::to_owned),
            media_kind: MediaKind::File,
            media_type: None,
            lineage: lineage(),
            retention: RetentionPolicy::default(),
            committed_at: ts(1_760_100_000_000),
        };
        // A different name does not join this sequence.
        assert!(matches!(
            append_artifact_version(&base, declaration(Some("monthly-report"))),
            Err(ArtifactError::VersionMismatch { .. })
        ));
        // An unnamed declaration has no sequence to join.
        assert!(matches!(
            append_artifact_version(&base, declaration(None)),
            Err(ArtifactError::VersionMismatch { .. })
        ));
        // An unnamed head has no sequence at all.
        let (unnamed, _) = commit_artifact(CommitDeclaration {
            reference: ArtifactRef {
                sha256: "b".repeat(64),
                bytes: 1,
            },
            name: None,
            media_kind: MediaKind::File,
            media_type: None,
            lineage: lineage(),
            retention: RetentionPolicy::default(),
            committed_at: ts(1_760_000_000_000),
        })
        .unwrap();
        assert!(matches!(
            append_artifact_version(&unnamed, declaration(Some("weekly-report"))),
            Err(ArtifactError::VersionMismatch { .. })
        ));
        // The naming and address rules still hold on the append path.
        assert!(matches!(
            append_artifact_version(&base, declaration(Some("tenant/escape"))),
            Err(ArtifactError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn retention_acts_omit_absent_slots() {
        // The sparse wire: unnamed, reasonless acts carry no
        // placeholders, and round-trip whole.
        let release = ArtifactRelease {
            artifact_id: "f".repeat(64),
            tenant: "default".into(),
            name: None,
            released_by: "human:amjad".into(),
            reason: None,
            released_at: ts(1_760_300_000_000),
        };
        let wire = serde_json::to_value(&release).unwrap();
        assert!(wire.get("name").is_none());
        assert!(wire.get("reason").is_none());
        let back: ArtifactRelease = serde_json::from_value(wire).unwrap();
        assert_eq!(back, release);
    }

    // ---------- wave 2: previews ----------

    /// A 4×2 uncompressed 24-bit BMP, rows bottom-up: the top row red,
    /// the bottom row blue (in display order).
    fn tiny_bmp() -> Vec<u8> {
        let stride = 12; // (4 px * 3 B) padded to a 4-byte boundary
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(54u32 + 2 * stride as u32).to_le_bytes()); // file size
        bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bmp.extend_from_slice(&54u32.to_le_bytes()); // data offset
        bmp.extend_from_slice(&40u32.to_le_bytes()); // DIB size
        bmp.extend_from_slice(&4u32.to_le_bytes()); // width
        bmp.extend_from_slice(&2i32.to_le_bytes()); // height (bottom-up)
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // bpp
        bmp.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        bmp.extend_from_slice(&(2 * stride as u32).to_le_bytes()); // image size
        bmp.extend_from_slice(&0u32.to_le_bytes()); // x ppm
        bmp.extend_from_slice(&0u32.to_le_bytes()); // y ppm
        bmp.extend_from_slice(&0u32.to_le_bytes()); // palette colors
        bmp.extend_from_slice(&0u32.to_le_bytes()); // important colors
                                                    // Stored bottom row first: blue (BGR).
        for _ in 0..4 {
            bmp.extend_from_slice(&[255, 0, 0]);
        }
        bmp.extend_from_slice(&[0; 0]); // stride is exactly 12 here
                                        // Stored top row: red (BGR).
        for _ in 0..4 {
            bmp.extend_from_slice(&[0, 0, 255]);
        }
        bmp
    }

    /// A 16-sample mono 8-bit PCM WAV at 8000 Hz, ramping 0..255.
    fn tiny_wav() -> Vec<u8> {
        let samples: Vec<u8> = (0..16).map(|i| i * 16).collect();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&8000u32.to_le_bytes()); // rate
        wav.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&1u16.to_le_bytes()); // block align
        wav.extend_from_slice(&8u16.to_le_bytes()); // bits
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(&samples);
        wav
    }

    #[test]
    fn golden_artifact_preview_shapes() {
        // Every derivable variant plus the honest empty, built from real
        // fixtures so the pinned shapes are derivations, not inventions.
        let text = derive_preview(MediaKind::File, b"plain export, no structure");
        let json_preview = derive_preview(MediaKind::Data, br#"{"rows":[1,2,3]}"#);
        let image = derive_preview(MediaKind::Image, &tiny_bmp());
        let audio = derive_preview(MediaKind::Audio, &tiny_wav());
        let empty = derive_preview(MediaKind::File, &[0xff, 0x00, 0xfe, 0x01]);
        assert_golden(
            "artifact_preview.json",
            &vec![text, json_preview, image, audio, empty],
        );
    }

    #[test]
    fn text_preview_bounds_the_window_and_json_stays_whole() {
        // A small JSON document derives as parsed JSON.
        let preview = derive_preview(MediaKind::Data, br#"{"ok":true}"#);
        assert!(matches!(preview, ArtifactPreview::Json { .. }));
        // A document past the window degrades to truncated text, never a
        // partial parse.
        let big = format!("{{\"data\":\"{}\"}}", "x".repeat(PREVIEW_TEXT_MAX_BYTES));
        let preview = derive_preview(MediaKind::Data, big.as_bytes());
        match preview {
            ArtifactPreview::Text {
                truncated,
                text,
                source_bytes,
            } => {
                assert!(truncated);
                assert!(text.len() <= PREVIEW_TEXT_MAX_BYTES);
                assert_eq!(source_bytes, big.len() as u64);
            }
            other => panic!("expected truncated text, got {other:?}"),
        }
        // A window that would clip a multi-byte character serves up to
        // the boundary, lossless.
        let mut bytes = vec![b'a'; PREVIEW_TEXT_MAX_BYTES - 1];
        bytes.extend_from_slice("é".as_bytes()); // 2 bytes, straddles the cap
        let preview = derive_preview(MediaKind::File, &bytes);
        match preview {
            ArtifactPreview::Text {
                truncated, text, ..
            } => {
                assert!(truncated);
                assert_eq!(text.len(), PREVIEW_TEXT_MAX_BYTES - 1);
            }
            other => panic!("expected boundary-clipped text, got {other:?}"),
        }
        // Interior-invalid bytes are binary: the honest empty.
        assert!(matches!(
            derive_preview(MediaKind::File, &[b'a', 0xff, b'b']),
            ArtifactPreview::Empty { .. }
        ));
    }

    #[test]
    fn image_preview_decodes_downscales_and_refuses_the_undecodable() {
        let preview = derive_preview(MediaKind::Image, &tiny_bmp());
        match preview {
            ArtifactPreview::Image {
                format,
                width,
                height,
                thumb_width,
                thumb_height,
                pixels_ppm_hex,
            } => {
                assert_eq!(format, "bmp");
                assert_eq!((width, height), (4, 2));
                assert_eq!((thumb_width, thumb_height), (4, 2)); // under the cap: factor 1
                let ppm = crate::broker::hex_decode(&pixels_ppm_hex).unwrap();
                assert_eq!(&ppm[0..2], b"P6");
                // The display-order top row is red (RGB), the bottom blue.
                let header_len = "P6\n4 2\n255\n".len();
                assert_eq!(&ppm[header_len..header_len + 3], &[255, 0, 0]);
                assert_eq!(&ppm[ppm.len() - 3..], &[0, 0, 255]);
            }
            other => panic!("expected a derived thumbnail, got {other:?}"),
        }
        // A P5 grayscale PNM derives too (channels replicate).
        let mut pnm = b"P5\n2 1\n255\n".to_vec();
        pnm.extend_from_slice(&[17, 240]);
        match derive_preview(MediaKind::Image, &pnm) {
            ArtifactPreview::Image {
                format,
                pixels_ppm_hex,
                ..
            } => {
                assert_eq!(format, "pnm");
                let ppm = crate::broker::hex_decode(&pixels_ppm_hex).unwrap();
                assert!(ppm.windows(3).any(|w| w == [17, 17, 17]));
            }
            other => panic!("expected a derived pnm thumbnail, got {other:?}"),
        }
        // A PNG is the honest empty: compressed formats need a codec the
        // runtime deliberately does not carry.
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
        assert!(matches!(
            derive_preview(MediaKind::Image, &png),
            ArtifactPreview::Empty { .. }
        ));
    }

    #[test]
    fn audio_preview_derives_waveform_metadata_for_pcm_only() {
        let preview = derive_preview(MediaKind::Audio, &tiny_wav());
        match preview {
            ArtifactPreview::Audio {
                format,
                duration_ms,
                sample_rate,
                channels,
                frames,
                peaks,
            } => {
                assert_eq!(format, "wav");
                assert_eq!(duration_ms, 2); // 16 frames at 8000 Hz
                assert_eq!(sample_rate, 8000);
                assert_eq!(channels, 1);
                assert_eq!(frames, 16);
                assert_eq!(peaks.len(), PREVIEW_WAVEFORM_BUCKETS);
                // Sixteen frames over sixty-four buckets touch every
                // fourth bucket. The 8-bit magnitude is the absolute
                // offset from midpoint 128, so the ramp's ends are the
                // loudest: frame 0 (sample 0, offset 128) at bucket 0.
                assert_eq!(peaks[0], 128 * 256);
                assert_eq!(peaks.iter().max().unwrap(), &(128 * 256));
                // The quiet middle (sample 128, offset 0) lands at
                // frame 8 → bucket 32.
                assert_eq!(peaks[32], 0);
            }
            other => panic!("expected waveform metadata, got {other:?}"),
        }
        // A truncated header and a foreign container are the honest empty.
        assert!(matches!(
            derive_preview(MediaKind::Audio, b"RIFF"),
            ArtifactPreview::Empty { .. }
        ));
        assert!(matches!(
            derive_preview(MediaKind::Audio, b"OggS........"),
            ArtifactPreview::Empty { .. }
        ));
    }
}
