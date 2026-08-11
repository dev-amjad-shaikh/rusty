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
//! Wave 1 ships the base record and the commit path. The records extend
//! additively: named-artifact version *accumulation* (the `versions`
//! sequence beyond its base entry), previews, the retention sweeper, and
//! the retention-release act are later waves — the wire shapes here carry
//! their slots from the first byte so those land as additions, never
//! migrations.

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
// Errors
// --------------------------------------------------------------------- //

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
        // The wave's additive RunEventKind wire name (the
        // `registry_event_kinds.json` discipline): pinned so no wire
        // shape lands unpinned, appended after `connection_needs_reauth`
        // per the additive evolution rule every variant since R0.6
        // followed.
        assert_golden(
            "artifact_event_kinds.json",
            &vec![RunEventKind::ArtifactCommitted],
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
}
