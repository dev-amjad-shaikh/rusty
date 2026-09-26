//! Signed run receipts (R0.9 Rusty Capsules, wave 3): an Ed25519-signed
//! statement over evidence the Flight Recorder already keeps, and the
//! verification API that re-walks that evidence.
//!
//! A [`RunReceipt`] signs:
//!
//! - the run id and the **journal head** ([`JournalRef`]) — the chained
//!   SHA-256 every checkpoint stamps; signing the head signs every event
//!   in the chain transitively, which is what hash chains are for;
//! - the **run manifest** ([`RunManifest`]) — prompt, tool-schema, model,
//!   and memory-schema digests — carried in full for the auditor and
//!   committed to by `manifest_digest`, plus the **resolved capsule
//!   content addresses** ([`CapsuleId`]) the journaled resolutions bound
//!   the run's pins to;
//! - the **effect ledger** — one digest per journaled
//!   [`EffectReceipt`] over provider,
//!   provider id, idempotency key, and effect id, so the receipt covers
//!   what the run *did to the world*, not just what it computed;
//! - the **policy versions** — the executor [`PolicyVersion`] the run's
//!   checkpoint header pinned, plus the Cedar policy versions capsules
//!   were admitted under;
//! - the **denials ledger** — the [`RunEventKind::CapsuleDenied`] event
//!   ids. A receipt over a run that attempted forbidden access says so;
//!   the visibility of the denial survives into the signed statement;
//! - `signer` (a content-addressed key id) and `signature` over the
//!   canonical serialization of all of the above.
//!
//! # What a receipt proves, and what it does not
//!
//! A verified receipt proves which code, capsule builds, memory schema,
//! policies, and permissions produced this run's actions — what Rusty
//! received, authorized, and executed, with the denials attached. It does
//! **not** prove that an external LLM's answer was truthful, that a tool's
//! provider behaved honestly, or that a remote agent did what it claimed:
//! those are claims about systems whose journals Rusty does not hold, and
//! a signature over Rusty's evidence cannot witness them. The receipt is a
//! statement about *this runtime's* conduct.
//!
//! # Cryptographic honesty
//!
//! Ed25519 over locally held keys gives **integrity and origin against a
//! key the operator holds**: a receipt that verifies was minted by whoever
//! controlled the signing key, over exactly the evidence named. It gives
//! nothing more — no non-repudiation against the operator (they hold the
//! key and can mint any statement), no transparency (a withheld receipt
//! leaves no trace), no remote attestation (the key proves nothing about
//! the machine it lives on). KMS integration, transparency-log witnessing,
//! and attestation are R1.0+; the canonical form
//! ([`RunReceipt::canonical_bytes`]) is the exact byte string a
//! transparency log would witness, so that integration lands additively.
//!
//! # Verification reuses the journal's digests
//!
//! [`verify_receipt`] recomputes the head over the snapshot's event chain
//! with the journal's own chain step (see
//! [`crate::journal::Journal::from_snapshot`]), so a receipt's head and
//! the journal's head are the same bytes by construction — the signature
//! check is the only new machinery, not a new evidence pipeline. Every
//! failure is a typed [`ReceiptRejection`] naming the component whose
//! digest mismatched; verification never answers a bare `false`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::capsule::{CapsuleId, CapsuleResolution};
use crate::error::RustyError;
use crate::journal::JournalSnapshot;
use crate::record::{
    canonicalize_value, sha256_hex, EffectReceipt, JournalRef, PayloadRef, PolicyVersion,
    RunEventKind, RunManifest,
};

/// The receipt envelope version. Bump only on a breaking change to the
/// signed statement; additive evolution uses serde defaults so receipts
/// written by earlier builds keep verifying.
pub const RECEIPT_FORMAT_VERSION: u32 = 1;

// --------------------------------------------------------------------- //
// Keys
// --------------------------------------------------------------------- //

/// A signing key for minting receipts: Ed25519 secret key material, held
/// locally by the operator's server. The secret never enters the store
/// abstraction — the server persists it as a `0600` file under
/// `{store_path}/keys/` and keeps only the public half in the key history,
/// so a database backend can never hold what it must not leak.
pub struct SigningKey(ed25519_dalek::SigningKey);

/// A verification key: the public half of a [`SigningKey`]. Key ids are
/// content addresses of this key ([`derive_key_id`]), so a key id on a
/// receipt commits to exactly one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(ed25519_dalek::VerifyingKey);

impl SigningKey {
    /// A fresh keypair drawn from OS entropy (two v4 UUID draws — 32 bytes
    /// of `getrandom` entropy, the same source the runtime's id minting
    /// already trusts; `ed25519-dalek` carries no RNG feature here).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self::from_bytes(&bytes)
    }

    /// Wrap 32 bytes of secret key material (the stored secret-file form).
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(bytes))
    }

    /// The 32 secret bytes — what the server's `0600` secret file holds.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Rehydrate from the hex form ([`SigningKey::to_hex`]).
    pub fn from_hex(hex: &str) -> Option<Self> {
        let bytes = hex_decode(hex)?;
        let bytes: [u8; 32] = bytes.try_into().ok()?;
        Some(Self::from_bytes(&bytes))
    }

    /// Lowercase hex of the secret bytes.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0.to_bytes())
    }

    /// The public half.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// This key's content-addressed id ([`derive_key_id`]).
    pub fn key_id(&self) -> String {
        derive_key_id(&self.public_key())
    }

    /// Sign `message`, returning the lowercase-hex signature. Ed25519 is
    /// deterministic: the same key over the same bytes mints the same
    /// signature, which is what makes receipt goldens stable.
    pub fn sign_hex(&self, message: &[u8]) -> String {
        use ed25519_dalek::Signer as _;
        hex_encode(&self.0.sign(message).to_bytes())
    }
}

// The secret key's Debug prints the key id — a content address of the
// public half — never the secret material.
impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SigningKey(key_id: {})", self.key_id())
    }
}

impl Clone for SigningKey {
    fn clone(&self) -> Self {
        Self::from_bytes(&self.0.to_bytes())
    }
}

impl PublicKey {
    /// Wrap 32 bytes of public key material. Fails on bytes that are not a
    /// valid compressed Edwards point — a malformed key is rejected here,
    /// not at first verification.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, RustyError> {
        ed25519_dalek::VerifyingKey::from_bytes(bytes)
            .map(Self)
            .map_err(|e| invalid_key(format!("invalid Ed25519 public key: {e}")))
    }

    /// The 32 public bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Rehydrate from the hex form ([`PublicKey::to_hex`]). `None` on
    /// malformed hex or wrong length; an invalid point is an error.
    pub fn from_hex(hex: &str) -> Result<Self, RustyError> {
        let bytes = hex_decode(hex)
            .ok_or_else(|| invalid_key(format!("public key is not valid hex: `{hex}`")))?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            invalid_key(format!(
                "public key must be 32 bytes, got {}",
                hex.len() / 2
            ))
        })?;
        Self::from_bytes(&bytes)
    }

    /// Lowercase hex of the public bytes — the form the key history holds.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0.to_bytes())
    }

    /// This key's content-addressed id ([`derive_key_id`]).
    pub fn key_id(&self) -> String {
        derive_key_id(self)
    }
}

/// The content address of a public key: `sha256` over its 32 bytes, the
/// one hashing primitive shared with artifact references, journal heads,
/// and capsule ids. A receipt's `signer` naming this id commits to exactly
/// one key, which is what makes "receipts signed after the compromise date
/// are suspect" answerable from the key history alone.
pub fn derive_key_id(public_key: &PublicKey) -> String {
    sha256_hex(&public_key.0.to_bytes())
}

fn invalid_key(detail: String) -> RustyError {
    RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        detail,
    )))
}

/// Lowercase hex of `bytes` (the [`sha256_hex`] formatting rule, for key
/// material and signatures rather than digests).
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decode lowercase or uppercase hex; `None` on odd length or non-hex
/// input. Local to receipts: the crate hashes often but decodes hex only
/// here, at the key/signature boundary.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

// --------------------------------------------------------------------- //
// The receipt
// --------------------------------------------------------------------- //

/// The signed statement over one run's evidence (R0.9 wave 3). Serde-
/// versioned (`format_version`), golden-pinned
/// (`rusty-core/tests/golden/run_receipt.json`), and additive like every
/// other contract here: new fields arrive with serde defaults so receipts
/// written by earlier builds keep verifying.
///
/// Two fields are evidence carried for the auditor rather than digests
/// recomputed from the journal: `manifest` (which lives in checkpoint
/// headers, not in the event chain) and `executor_policy` (likewise).
/// Everything else — the head, the resolved capsules, the effect and
/// denials ledgers, the Cedar policy versions — is recomputed from the
/// journaled events at verification time, and `manifest_digest` commits
/// the carried manifest into the signed statement so tampering with either
/// half is caught by name ([`ReceiptRejection::ManifestDigest`]) before
/// the signature is even checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReceipt {
    /// Receipt envelope version; [`RECEIPT_FORMAT_VERSION`] for anything
    /// minted now.
    pub format_version: u32,

    /// The run this receipt attests.
    pub run_id: String,

    /// The journal head the signature covers: event count plus chained
    /// SHA-256. Signing the head signs every event transitively;
    /// verification recomputes it from the snapshot's event chain.
    pub journal_head: JournalRef,

    /// SHA-256 of the canonical serialization of `manifest` — the compact
    /// commitment the canonical form covers. `None` exactly when the run
    /// pinned no manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,

    /// The run manifest the server pinned into the run's checkpoint
    /// headers: prompt hashes, tool schema hashes, model + parameters
    /// digest, memory schema. Carried in full because the journal does not
    /// hold it — the manifest is checkpoint-header evidence, and the
    /// receipt is where it joins the signed statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RunManifest>,

    /// The capsule content addresses the run's pins resolved to (pin name
    /// → [`CapsuleId`]), recomputed from the journaled
    /// [`RunEventKind::CapsuleResolved`] events. When a pin resolved more
    /// than once the latest resolution wins — the run's current binding,
    /// the same rule mint and verify share.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capsules: BTreeMap<String, CapsuleId>,

    /// The effect ledger: one digest per journaled
    /// [`EffectReceipt`], in journal order,
    /// over provider, provider id, idempotency key, and effect id — the
    /// four fields the design names. `task_id` is deliberately outside the
    /// digest: task linkage is dispatch bookkeeping, not effect identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,

    /// The executor policy version the run's checkpoint header pinned.
    /// Carried evidence (like `manifest`): the header lives outside the
    /// journal, so the signature covers it and no recompute can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_policy: Option<PolicyVersion>,

    /// The Cedar policy versions capsules were admitted under, distinct,
    /// in first-use order — recomputed from the journaled resolutions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capsule_policies: Vec<String>,

    /// The denials ledger: the ids of the run's
    /// [`RunEventKind::CapsuleDenied`] events, in journal order. Empty for
    /// a run that never attempted forbidden access — which is itself a
    /// statement the signature covers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denials: Vec<String>,

    /// The signer's content-addressed key id ([`derive_key_id`]).
    /// Verification resolves the public key by this id from the
    /// deployment's key history, which is what keeps receipts signed
    /// before a rotation verifiable.
    pub signer: String,

    /// Lowercase-hex Ed25519 signature over [`RunReceipt::canonical_bytes`].
    pub signature: String,
}

/// The signed statement, exactly as the canonical form covers it: every
/// receipt field except `signature`. Borrowed, so canonicalization never
/// clones the receipt.
#[derive(Serialize)]
struct ReceiptStatement<'a> {
    format_version: u32,
    run_id: &'a str,
    journal_head: &'a JournalRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_digest: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: &'a Option<RunManifest>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    capsules: &'a BTreeMap<String, CapsuleId>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    effects: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    executor_policy: &'a Option<PolicyVersion>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    capsule_policies: &'a [String],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    denials: &'a [String],
    signer: &'a str,
}

impl RunReceipt {
    fn statement(&self) -> ReceiptStatement<'_> {
        ReceiptStatement {
            format_version: self.format_version,
            run_id: &self.run_id,
            journal_head: &self.journal_head,
            manifest_digest: &self.manifest_digest,
            manifest: &self.manifest,
            capsules: &self.capsules,
            effects: &self.effects,
            executor_policy: &self.executor_policy,
            capsule_policies: &self.capsule_policies,
            denials: &self.denials,
            signer: &self.signer,
        }
    }

    /// The canonical serialization the signature covers: the statement as
    /// JSON with every object map key-sorted recursively — the same
    /// canonicalization rule the journal's head chain applies
    /// (`canonicalize_value`), so receipt digests and journal digests
    /// are computed under one rule and can never drift apart across
    /// serde_json map backends. These exact bytes are what a transparency
    /// log would witness; the integration lands additively because the
    /// form is already frozen.
    pub fn canonical_bytes(&self) -> crate::error::Result<Vec<u8>> {
        let value = serde_json::to_value(self.statement())?;
        Ok(serde_json::to_vec(&canonicalize_value(&value))?)
    }
}

/// The journaled record of a signing-key rotation (R0.9 wave 3): the
/// output payload of a [`RunEventKind::SigningKeyRotated`] event in the
/// deployment's receipts journal. Journaling the new key id is what makes
/// "which key signed what, from when" a chained fact rather than a
/// registry note; the public half travels with it so the lineage is
/// self-describing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SigningKeyRotation {
    /// The key id that signed until now. `None` on genesis — the first
    /// key a deployment ever generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_key_id: Option<String>,

    /// The new key id (content address of `public_key`).
    pub new_key_id: String,

    /// The new public key, lowercase hex.
    pub public_key: String,

    /// When the rotation happened, read from the server's clock.
    pub rotated_at: DateTime<Utc>,
}

// --------------------------------------------------------------------- //
// Minting
// --------------------------------------------------------------------- //

/// Mint and sign a receipt over `snapshot`.
///
/// `manifest` and `executor_policy` come from the run's checkpoint header
/// (the journal does not hold them); everything else is derived from the
/// journaled events by the same component-derivation walk verification
/// uses, so mint and verify cannot drift apart.
///
/// Minting refuses evidence that fails its own integrity check — a
/// snapshot whose event chain does not recompute to its claimed head is
/// rejected before anything is signed, because a signature over
/// unverifiable evidence would attest nothing. Externalized snapshots
/// ([`JournalSnapshot::artifact_refs`]) are refused for the same reason
/// verification rejects them: the ledger digests must cover payloads this
/// process can resolve.
pub fn mint_receipt(
    snapshot: &JournalSnapshot,
    manifest: Option<RunManifest>,
    executor_policy: Option<PolicyVersion>,
    signing_key: &SigningKey,
) -> crate::error::Result<RunReceipt> {
    let journal_head = verified_head(snapshot)?;
    let components = derive_components(snapshot)?;
    let manifest_digest = manifest_digest(manifest.as_ref())?;
    let mut receipt = RunReceipt {
        format_version: RECEIPT_FORMAT_VERSION,
        run_id: snapshot.run_id.clone(),
        journal_head,
        manifest_digest,
        manifest,
        capsules: components.capsules,
        effects: components.effects,
        executor_policy,
        capsule_policies: components.capsule_policies,
        denials: components.denials,
        signer: signing_key.key_id(),
        // Filled below; the canonical form never covers it.
        signature: String::new(),
    };
    let canonical = receipt.canonical_bytes()?;
    receipt.signature = signing_key.sign_hex(&canonical);
    Ok(receipt)
}

/// SHA-256 of the canonical serialization of `manifest` — the commitment
/// [`RunReceipt::manifest_digest`] carries. The same canonical-digest
/// convention the manifest's own pins use (object keys sort
/// deterministically, so equal manifests commit equal).
pub fn manifest_digest(manifest: Option<&RunManifest>) -> crate::error::Result<Option<String>> {
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    let value = serde_json::to_value(manifest)?;
    Ok(Some(sha256_hex(&serde_json::to_vec(&canonicalize_value(
        &value,
    ))?)))
}

/// The head the snapshot's event chain recomputes to, checked against the
/// snapshot's claim — the integrity gate mint and verify share. Uses the
/// journal's own chain step, so this can never disagree with
/// [`crate::journal::Journal::from_snapshot`].
fn verified_head(snapshot: &JournalSnapshot) -> crate::error::Result<JournalRef> {
    if !snapshot.artifact_refs.is_empty() {
        return Err(RustyError::Serialization(serde_json::Error::io(
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "journal snapshot references an external artifact store; \
                 receipts v1 mint and verify over embedded snapshots",
            ),
        )));
    }
    let recomputed = crate::journal::recompute_head_hash(&snapshot.events)?;
    if recomputed != snapshot.head_hash {
        return Err(RustyError::Serialization(serde_json::Error::io(
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "journal snapshot head hash mismatch: events recompute to \
                     {recomputed}, snapshot claims {}",
                    snapshot.head_hash
                ),
            ),
        )));
    }
    Ok(JournalRef {
        events: snapshot.events.len() as u64,
        sha256: recomputed,
    })
}

/// The journal-derived components of a receipt, recomputed identically at
/// mint and at verify — one walk, so the two can never drift.
struct ReceiptComponents {
    capsules: BTreeMap<String, CapsuleId>,
    effects: Vec<String>,
    capsule_policies: Vec<String>,
    denials: Vec<String>,
}

/// Walk the snapshot's events, deriving the resolved-capsule map, the
/// effect ledger, the Cedar policy versions, and the denials ledger.
///
/// Malformed payloads (a hand-edited journal whose head was recomputed to
/// match) are skipped rather than fatal — the same tolerance rule
/// [`JournalSnapshot::find_effect_receipt`] documents. The skip cannot
/// hide tampering: any byte flip in a journaled event changes the head,
/// which fails verification first.
fn derive_components(snapshot: &JournalSnapshot) -> crate::error::Result<ReceiptComponents> {
    let mut capsules = BTreeMap::new();
    let mut effects = Vec::new();
    let mut capsule_policies = Vec::new();
    let mut denials = Vec::new();
    for event in &snapshot.events {
        match event.kind {
            RunEventKind::EffectReceipt => {
                let Some(value) = resolve_output(snapshot, event) else {
                    continue;
                };
                if let Ok(receipt) = serde_json::from_value::<EffectReceipt>(value) {
                    effects.push(effect_ledger_digest(&receipt)?);
                }
            }
            RunEventKind::CapsuleResolved => {
                let Some(value) = resolve_output(snapshot, event) else {
                    continue;
                };
                if let Ok(resolution) = serde_json::from_value::<CapsuleResolution>(value) {
                    // Latest resolution wins: the run's current binding.
                    capsules.insert(resolution.name, resolution.capsule_id);
                    if let Some(version) = resolution.policy_version {
                        if !capsule_policies.contains(&version) {
                            capsule_policies.push(version);
                        }
                    }
                }
            }
            RunEventKind::CapsuleDenied => denials.push(event.id.clone()),
            _ => {}
        }
    }
    Ok(ReceiptComponents {
        capsules,
        effects,
        capsule_policies,
        denials,
    })
}

/// Resolve an event's output payload, looking through the snapshot's
/// artifact map — the [`JournalSnapshot::find_effect_receipt`] rule.
fn resolve_output(
    snapshot: &JournalSnapshot,
    event: &crate::record::RunEvent,
) -> Option<serde_json::Value> {
    match event.output.as_ref()? {
        PayloadRef::Inline(value) => Some(value.clone()),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256).cloned(),
    }
}

/// One effect-ledger entry, exactly as digested: the four fields the
/// design names, in declaration order. A typed struct (no free-form maps),
/// so its serialization is canonical under either serde_json map backend —
/// the same rule the journal chain relies on for struct fields.
#[derive(Serialize)]
struct EffectLedgerEntry<'a> {
    provider: &'a str,
    provider_id: &'a str,
    idempotency_key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    effect_id: Option<&'a str>,
}

/// The ledger digest of one journaled effect receipt.
fn effect_ledger_digest(receipt: &EffectReceipt) -> crate::error::Result<String> {
    let entry = EffectLedgerEntry {
        provider: &receipt.provider,
        provider_id: &receipt.provider_id,
        idempotency_key: &receipt.idempotency_key,
        effect_id: receipt.effect_id.as_deref(),
    };
    Ok(sha256_hex(&serde_json::to_vec(&entry)?))
}

// --------------------------------------------------------------------- //
// Verification
// ---------------------------------------------------------------------//

/// A verification failure, typed by the component whose digest mismatched.
/// The Display text names the claimed and recomputed values; [`ReceiptRejection::component`]
/// gives the stable machine-readable name the server's `POST
/// /receipts/verify` reports. Verification never answers a bare `false`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReceiptRejection {
    /// The receipt was written by a newer (or older, breaking) format.
    #[error("unsupported receipt format version {claimed} (this build verifies {supported})")]
    FormatVersion {
        /// The version the receipt claims.
        claimed: u32,
        /// The version this build verifies.
        supported: u32,
    },

    /// The snapshot spills payloads to an external artifact store; v1
    /// verifies embedded snapshots only.
    #[error(
        "the snapshot references an external artifact store; receipts v1 \
             verifies embedded snapshots only"
    )]
    ExternalArtifacts,

    /// The snapshot's own event chain does not recompute to its claimed
    /// head — tampered evidence, caught before any receipt comparison.
    #[error(
        "journal head mismatch: the snapshot's events recompute to {recomputed}, \
             the snapshot claims {claimed}"
    )]
    SnapshotHead {
        /// The head the snapshot carries.
        claimed: String,
        /// The head the event chain recomputes to.
        recomputed: String,
    },

    /// The receipt names a different run than the snapshot records.
    #[error("run id mismatch: the receipt attests `{claimed}`, the snapshot records `{snapshot}`")]
    RunId {
        /// The run id in the receipt.
        claimed: String,
        /// The run id in the snapshot.
        snapshot: String,
    },

    /// The receipt's signed head is not the head the snapshot's chain
    /// recomputes to — the receipt covers a different journal state.
    #[error(
        "journal head mismatch: the receipt signs {claimed}, the snapshot's events \
             recompute to {recomputed}"
    )]
    JournalHead {
        /// The head the receipt signs.
        claimed: String,
        /// The head the event chain recomputes to.
        recomputed: String,
    },

    /// The receipt's signed event count disagrees with the snapshot.
    #[error(
        "journal length mismatch: the receipt signs {claimed} events, the snapshot holds {actual}"
    )]
    JournalLength {
        /// The event count the receipt signs.
        claimed: u64,
        /// The event count in the snapshot.
        actual: u64,
    },

    /// The carried manifest does not re-hash to the committed digest.
    #[error(
        "manifest digest mismatch: the receipt commits {claimed:?}, the carried manifest \
             re-hashes to {recomputed:?}"
    )]
    ManifestDigest {
        /// The digest the receipt commits.
        claimed: Option<String>,
        /// The digest the carried manifest re-hashes to.
        recomputed: Option<String>,
    },

    /// The resolved-capsule map disagrees with the journaled resolutions.
    #[error("capsule resolution mismatch: {0}")]
    CapsuleResolutions(String),

    /// The effect ledger disagrees with the journaled effect receipts.
    #[error("effect ledger mismatch: {0}")]
    EffectLedger(String),

    /// The Cedar policy versions disagree with the journaled resolutions.
    #[error("capsule policy mismatch: {0}")]
    CapsulePolicies(String),

    /// The denials ledger disagrees with the journaled denials.
    #[error("denials ledger mismatch: {0}")]
    DenialsLedger(String),

    /// The public key offered for verification is not the receipt's
    /// signer — the key id is a content address, so this is a definitive
    /// answer, not a guess.
    #[error(
        "signer mismatch: the receipt names signer {claimed}, the offered key derives {derived}"
    )]
    SignerKeyId {
        /// The key id the receipt names.
        claimed: String,
        /// The key id the offered public key derives to.
        derived: String,
    },

    /// The signature does not verify over the canonical bytes — either it
    /// is malformed or the signed statement was altered.
    #[error("signature invalid: {0}")]
    Signature(String),
}

impl ReceiptRejection {
    /// The stable machine-readable component name (the server's verify
    /// endpoint reports it): which part of the evidence failed.
    pub fn component(&self) -> &'static str {
        match self {
            ReceiptRejection::FormatVersion { .. } => "format_version",
            ReceiptRejection::ExternalArtifacts => "external_artifacts",
            ReceiptRejection::SnapshotHead { .. } => "journal_head",
            ReceiptRejection::RunId { .. } => "run_id",
            ReceiptRejection::JournalHead { .. } => "journal_head",
            ReceiptRejection::JournalLength { .. } => "journal_head",
            ReceiptRejection::ManifestDigest { .. } => "manifest_digest",
            ReceiptRejection::CapsuleResolutions(_) => "capsule_resolutions",
            ReceiptRejection::EffectLedger(_) => "effect_ledger",
            ReceiptRejection::CapsulePolicies(_) => "capsule_policies",
            ReceiptRejection::DenialsLedger(_) => "denials_ledger",
            ReceiptRejection::SignerKeyId { .. } => "signer_key_id",
            ReceiptRejection::Signature(_) => "signature",
        }
    }
}

/// The typed summary of a successfully verified receipt: what was proven.
/// Every field was either recomputed from the snapshot during verification
/// or covered by the signature over the canonical form — nothing here is
/// taken on trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifiedRun {
    /// The verified run.
    pub run_id: String,

    /// The verified journal head (recomputed from the snapshot's chain).
    pub journal_head: JournalRef,

    /// The verified manifest commitment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<String>,

    /// The verified resolved-capsule map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capsules: BTreeMap<String, CapsuleId>,

    /// How many journaled effect receipts the verified ledger covers.
    pub effect_receipts: usize,

    /// The executor policy version the receipt attests (carried,
    /// signature-covered evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_policy: Option<PolicyVersion>,

    /// The verified Cedar policy versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capsule_policies: Vec<String>,

    /// The verified denials ledger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denials: Vec<String>,

    /// The verified signer's key id.
    pub signer: String,
}

/// Verify a receipt against an exported journal snapshot and a public key:
/// recompute the head over the snapshot's event chain, recompute the
/// manifest and ledger digests, check the signature.
///
/// The order is evidence-first, signature-last: the snapshot's own chain
/// is re-verified before any comparison (tampered evidence is named as
/// such, never compared against), each journal-derived component is
/// recomputed and compared by name, and only then does the signature
/// check run — so a failure always says *which* digest mismatched, and a
/// signature failure means the signed statement itself was altered.
///
/// The manifest and executor policy are the two components no journal can
/// recompute (they live in checkpoint headers); the carried manifest is
/// re-hashed against its committed digest, and both are covered by the
/// signature. Everything else is proven from the snapshot alone.
pub fn verify_receipt(
    snapshot: &JournalSnapshot,
    receipt: &RunReceipt,
    public_key: &PublicKey,
) -> Result<VerifiedRun, ReceiptRejection> {
    if receipt.format_version != RECEIPT_FORMAT_VERSION {
        return Err(ReceiptRejection::FormatVersion {
            claimed: receipt.format_version,
            supported: RECEIPT_FORMAT_VERSION,
        });
    }
    if !snapshot.artifact_refs.is_empty() {
        return Err(ReceiptRejection::ExternalArtifacts);
    }
    let recomputed_head = crate::journal::recompute_head_hash(&snapshot.events).map_err(|e| {
        ReceiptRejection::Signature(format!("the snapshot's events cannot be hashed: {e}"))
    })?;
    if recomputed_head != snapshot.head_hash {
        return Err(ReceiptRejection::SnapshotHead {
            claimed: snapshot.head_hash.clone(),
            recomputed: recomputed_head,
        });
    }
    if receipt.run_id != snapshot.run_id {
        return Err(ReceiptRejection::RunId {
            claimed: receipt.run_id.clone(),
            snapshot: snapshot.run_id.clone(),
        });
    }
    if receipt.journal_head.sha256 != recomputed_head {
        return Err(ReceiptRejection::JournalHead {
            claimed: receipt.journal_head.sha256.clone(),
            recomputed: recomputed_head,
        });
    }
    if receipt.journal_head.events != snapshot.events.len() as u64 {
        return Err(ReceiptRejection::JournalLength {
            claimed: receipt.journal_head.events,
            actual: snapshot.events.len() as u64,
        });
    }
    let recomputed_manifest = manifest_digest(receipt.manifest.as_ref())
        .map_err(|e| ReceiptRejection::Signature(format!("the manifest cannot be hashed: {e}")))?;
    if recomputed_manifest != receipt.manifest_digest {
        return Err(ReceiptRejection::ManifestDigest {
            claimed: receipt.manifest_digest.clone(),
            recomputed: recomputed_manifest,
        });
    }
    let components = derive_components(snapshot)
        .map_err(|e| ReceiptRejection::Signature(format!("the ledger cannot be digested: {e}")))?;
    if components.capsules != receipt.capsules {
        return Err(ReceiptRejection::CapsuleResolutions(ledger_diff(
            "capsules",
            &receipt.capsules,
            &components.capsules,
        )));
    }
    if components.effects != receipt.effects {
        return Err(ReceiptRejection::EffectLedger(sequence_diff(
            "effects",
            &receipt.effects,
            &components.effects,
        )));
    }
    if components.capsule_policies != receipt.capsule_policies {
        return Err(ReceiptRejection::CapsulePolicies(sequence_diff(
            "capsule policies",
            &receipt.capsule_policies,
            &components.capsule_policies,
        )));
    }
    if components.denials != receipt.denials {
        return Err(ReceiptRejection::DenialsLedger(sequence_diff(
            "denials",
            &receipt.denials,
            &components.denials,
        )));
    }
    let derived_key_id = public_key.key_id();
    if derived_key_id != receipt.signer {
        return Err(ReceiptRejection::SignerKeyId {
            claimed: receipt.signer.clone(),
            derived: derived_key_id,
        });
    }
    let canonical = receipt
        .canonical_bytes()
        .map_err(|e| ReceiptRejection::Signature(format!("the statement cannot be hashed: {e}")))?;
    let signature_bytes = hex_decode(&receipt.signature)
        .and_then(|bytes| <[u8; 64]>::try_from(bytes.as_slice()).ok())
        .ok_or_else(|| {
            ReceiptRejection::Signature("the signature is not 64 bytes of hex".to_string())
        })?;
    let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);
    // `verify_strict`: canonical-S and torsion-safe — one valid encoding
    // per signature, the posture a witnessed statement wants.
    public_key
        .0
        .verify_strict(&canonical, &signature)
        .map_err(|_| {
            ReceiptRejection::Signature(
                "the signature does not verify over the canonical statement".to_string(),
            )
        })?;
    Ok(VerifiedRun {
        run_id: receipt.run_id.clone(),
        journal_head: receipt.journal_head.clone(),
        manifest_digest: receipt.manifest_digest.clone(),
        capsules: components.capsules,
        effect_receipts: components.effects.len(),
        executor_policy: receipt.executor_policy.clone(),
        capsule_policies: components.capsule_policies,
        denials: components.denials,
        signer: receipt.signer.clone(),
    })
}

/// Verify a receipt's authenticity and its head *as a prefix of a
/// journal that may have grown since the mint* (R0.12 wave 2 — the
/// retention sweeper's coverage check).
///
/// [`verify_receipt`] answers "does this receipt attest this exact
/// journal?"; this answers the retention question "does this receipt
/// still pin the addresses its covered events name?". A receipt is a
/// statement about the events under its head, and the journal's
/// append-only hash chain makes that statement durable: recomputing the
/// head over the current journal's first `journal_head.events` events
/// and matching it against the signed head proves the covered prefix is
/// byte-identical to what was signed, however much the journal grew
/// since. Ledger components (effects, capsules, denials) are whole-
/// journal derivations and are deliberately *not* recomputed here — the
/// coverage question is about the prefix, not the tail.
///
/// The fail-closed rule matches [`verify_receipt`]'s: authenticity
/// (signer key id plus the strict signature over the canonical
/// statement), run identity, and the prefix head are all checked, and
/// any mismatch is a typed rejection — a sweeper that cannot verify a
/// receipt's coverage of an address must assume the coverage stands.
pub fn verify_receipt_prefix(
    snapshot: &JournalSnapshot,
    receipt: &RunReceipt,
    public_key: &PublicKey,
) -> Result<(), ReceiptRejection> {
    if receipt.format_version != RECEIPT_FORMAT_VERSION {
        return Err(ReceiptRejection::FormatVersion {
            claimed: receipt.format_version,
            supported: RECEIPT_FORMAT_VERSION,
        });
    }
    if receipt.run_id != snapshot.run_id {
        return Err(ReceiptRejection::RunId {
            claimed: receipt.run_id.clone(),
            snapshot: snapshot.run_id.clone(),
        });
    }
    let covered = receipt.journal_head.events as usize;
    if covered > snapshot.events.len() {
        return Err(ReceiptRejection::JournalLength {
            claimed: receipt.journal_head.events,
            actual: snapshot.events.len() as u64,
        });
    }
    let recomputed =
        crate::journal::recompute_head_hash(&snapshot.events[..covered]).map_err(|e| {
            ReceiptRejection::Signature(format!("the covered events cannot be hashed: {e}"))
        })?;
    if recomputed != receipt.journal_head.sha256 {
        return Err(ReceiptRejection::JournalHead {
            claimed: receipt.journal_head.sha256.clone(),
            recomputed,
        });
    }
    let derived_key_id = public_key.key_id();
    if derived_key_id != receipt.signer {
        return Err(ReceiptRejection::SignerKeyId {
            claimed: receipt.signer.clone(),
            derived: derived_key_id,
        });
    }
    let canonical = receipt
        .canonical_bytes()
        .map_err(|e| ReceiptRejection::Signature(format!("the statement cannot be hashed: {e}")))?;
    let signature_bytes = hex_decode(&receipt.signature)
        .and_then(|bytes| <[u8; 64]>::try_from(bytes.as_slice()).ok())
        .ok_or_else(|| {
            ReceiptRejection::Signature("the signature is not 64 bytes of hex".to_string())
        })?;
    let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);
    // `verify_strict`, as in `verify_receipt`: one valid encoding per
    // signature, the posture a witnessed statement wants.
    public_key
        .0
        .verify_strict(&canonical, &signature)
        .map_err(|_| {
            ReceiptRejection::Signature(
                "the signature does not verify over the canonical statement".to_string(),
            )
        })?;
    Ok(())
}

/// The first divergence between a claimed and recomputed sequence, phrased
/// for the rejection message.
fn sequence_diff(what: &str, claimed: &[String], recomputed: &[String]) -> String {
    if claimed.len() != recomputed.len() {
        return format!(
            "the receipt's {what} ledger holds {} entries, the journal recomputes {}",
            claimed.len(),
            recomputed.len()
        );
    }
    for (index, (claimed, recomputed)) in claimed.iter().zip(recomputed).enumerate() {
        if claimed != recomputed {
            return format!(
                "the receipt's {what} entry {index} is `{claimed}`, the journal recomputes `{recomputed}`"
            );
        }
    }
    format!("the receipt's {what} ledger disagrees with the journal")
}

/// The divergence between a claimed and recomputed capsule map, phrased
/// for the rejection message.
fn ledger_diff(
    what: &str,
    claimed: &BTreeMap<String, CapsuleId>,
    recomputed: &BTreeMap<String, CapsuleId>,
) -> String {
    for (name, id) in claimed {
        match recomputed.get(name) {
            None => {
                return format!(
                    "the receipt resolves pin `{name}` to `{id}`, the journal holds no resolution"
                )
            }
            Some(recomputed) if recomputed != id => {
                return format!(
                    "the receipt resolves pin `{name}` to `{id}`, the journal recomputes `{recomputed}`"
                )
            }
            _ => {}
        }
    }
    for name in recomputed.keys() {
        if !claimed.contains_key(name) {
            return format!(
                "the journal resolves pin `{name}`, the receipt's {what} ledger omits it"
            );
        }
    }
    format!("the receipt's {what} ledger disagrees with the journal")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_rejects_malformed() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x10]), "00ff10");
        assert_eq!(hex_decode("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(hex_decode("00FF10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(hex_decode("0ff"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn key_id_is_a_content_address() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = key.public_key();
        assert_eq!(key.key_id(), derive_key_id(&public));
        assert_eq!(key.key_id().len(), 64);
        // Different secret, different id — the id commits to the key.
        let other = SigningKey::from_bytes(&[8u8; 32]);
        assert_ne!(key.key_id(), other.key_id());
        // Hex round-trip preserves the key (and thus the id).
        let rehydrated = PublicKey::from_hex(&public.to_hex()).unwrap();
        assert_eq!(rehydrated, public);
        let secret = SigningKey::from_hex(&key.to_hex()).unwrap();
        assert_eq!(secret.key_id(), key.key_id());
    }

    #[test]
    fn generated_keys_sign_and_verify() {
        let key = SigningKey::generate();
        let signature = key.sign_hex(b"canonical bytes");
        let bytes = hex_decode(&signature).unwrap();
        let signature =
            ed25519_dalek::Signature::from_bytes(&<[u8; 64]>::try_from(bytes.as_slice()).unwrap());
        key.public_key()
            .0
            .verify_strict(b"canonical bytes", &signature)
            .unwrap();
    }

    #[test]
    fn prefix_verification_survives_journal_growth() {
        use crate::journal::{Clock, EventDraft, Journal};
        use crate::record::{Effect, RunEventKind};

        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let journal = Journal::new("run-prefix", "thread-prefix", Clock::System);
        journal.record(
            EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
                .output(serde_json::json!({"step": 0})),
        );
        journal.record(
            EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
                .output(serde_json::json!({"value": 1})),
        );
        let minted = journal.snapshot();
        let receipt = mint_receipt(&minted, None, None, &signing).unwrap();
        // Whole-journal verification agrees at mint time.
        verify_receipt(&minted, &receipt, &signing.public_key()).unwrap();

        // The journal grows after the mint — post-mint commits must not
        // stale the coverage the receipt attests.
        journal.record(
            EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                .output(serde_json::json!({"step": 0})),
        );
        let grown = journal.snapshot();
        verify_receipt_prefix(&grown, &receipt, &signing.public_key()).unwrap();
        // The whole-journal check *does* reject the grown journal — the
        // prefix check is the sweeper's answer, not a loosening.
        assert!(matches!(
            verify_receipt(&grown, &receipt, &signing.public_key()),
            Err(ReceiptRejection::JournalHead { .. })
        ));

        // A tampered covered event changes the recomputed prefix head.
        let mut tampered = grown.clone();
        tampered.events[0].seq = 99;
        assert!(matches!(
            verify_receipt_prefix(&tampered, &receipt, &signing.public_key()),
            Err(ReceiptRejection::JournalHead { .. })
        ));
        // The wrong key never verifies, however the journal stands.
        let other = SigningKey::from_bytes(&[10u8; 32]);
        assert!(matches!(
            verify_receipt_prefix(&grown, &receipt, &other.public_key()),
            Err(ReceiptRejection::SignerKeyId { .. })
        ));
    }
}
