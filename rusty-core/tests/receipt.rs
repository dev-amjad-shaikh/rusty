//! Signed run receipts integration tests (R0.9, wave 3).
//!
//! Three test groups:
//!
//! - **Golden files** — the serialized shape of a full specimen
//!   `RunReceipt`, its exact canonical form (the byte string the
//!   signature covers and a transparency log would witness), the
//!   content-addressed key id, the wave's new `RunEventKind` wire name,
//!   and the journaled `SigningKeyRotation` payload, all pinned against
//!   checked-in JSON under `tests/golden/`. To bless an intentional
//!   contract change, re-run with `UPDATE_GOLDEN=1` and review the diff.
//! - **The exit criteria** — a receipt verifies against the run's
//!   exported `JournalSnapshot`; flipping one byte in any journaled
//!   event fails verification naming the journal head; a rotated key
//!   fails old receipts by signer id while the old key still verifies
//!   them (the key-history contract, keyed here at core level).
//! - **Component-named failures** — a tampered manifest digest, effect
//!   ledger, capsule map, policy list, or denials ledger each fails
//!   verification naming that component; a wrong key and a broken
//!   signature each fail by name. Verification never answers a bare
//!   `false`.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::capsule::{CapabilityGrant, CapsuleDenial, CapsuleId, CapsuleResolution};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot};
use rusty_agent_runtime::receipt::{
    mint_receipt, verify_receipt, ReceiptRejection, RunReceipt, SigningKey, SigningKeyRotation,
    RECEIPT_FORMAT_VERSION,
};
use rusty_agent_runtime::record::{
    sha256_hex, CapsuleVersion, Effect, EffectReceipt, PolicyVersion, RunEventKind, RunManifest,
};

// ---------- golden-file machinery ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    assert_golden_text(name, &rendered);
}

/// Assert raw `text` equals the golden file's content exactly — for the
/// canonical form, whose byte-for-byte identity is the contract (no
/// re-printing allowed).
fn assert_golden_text(name: &str, text: &str) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, text).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        text,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- the specimen ----------

/// The fixed signing key every golden uses: Ed25519 is deterministic, so
/// one fixed key pins both the signature and the key id.
fn specimen_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// A deterministic journal (logical clock, fixed run id) exercising every
/// journal-derived receipt component: one effect receipt, one capsule
/// resolution under a Cedar policy version, one scoped denial.
fn specimen_journal() -> Journal {
    let journal = Journal::new("run-1", "thread-1", Clock::logical(1_700_000_000_000, 5));
    let start = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure).input(json!({"step": 0})),
    );
    let node_input = journal.record(
        EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
            .node("agent")
            .input(json!({"messages": []}))
            .parent(start),
    );
    journal.record_effect_receipt(
        &EffectReceipt {
            provider: "stripe".into(),
            provider_id: "ch_3PKd".into(),
            idempotency_key: "run-1:charge:0".into(),
            task_id: None,
            effect_id: Some("ab".repeat(32)),
        },
        Some(node_input.clone()),
    );
    journal.record(
        EventDraft::new(RunEventKind::CapsuleResolved, Effect::ReadOnly)
            .output(
                serde_json::to_value(CapsuleResolution {
                    name: "researcher".into(),
                    version: CapsuleVersion::new("1.4.0"),
                    capsule_id: CapsuleId::from("cd".repeat(32)),
                    build_digest: "ef".repeat(32),
                    policy_version: Some("cedar-0123456789ab".into()),
                    overlays: None,
                    effective_grants: None,
                    clamped_budget: None,
                })
                .unwrap(),
            )
            .parent(node_input.clone()),
    );
    journal.record(
        EventDraft::new(RunEventKind::CapsuleDenied, Effect::Pure)
            .output(
                serde_json::to_value(CapsuleDenial::scoped(
                    CapsuleId::from("cd".repeat(32)),
                    CapabilityGrant::Network {
                        hosts: vec!["evil.example".into()],
                        protocols: vec!["https".into()],
                        methods: vec!["GET".into()],
                    },
                    "fetch GET https://evil.example/probe names no granted network scope",
                ))
                .unwrap(),
            )
            .parent(node_input),
    );
    journal
}

fn specimen_manifest() -> RunManifest {
    RunManifest::new()
        .pin_prompt("system", "You are a careful research agent.")
        .pin_tool_schema(
            "search",
            &json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        )
        .pin_model("gpt-5.2-2026-06-01", &json!({"temperature": 0, "seed": 42}))
        .with_memory_schema("memory-v1")
        .pin_capsule("researcher", CapsuleVersion::new("1.4.0"))
}

fn specimen_receipt() -> RunReceipt {
    mint_receipt(
        &specimen_journal().snapshot(),
        Some(specimen_manifest()),
        Some(PolicyVersion::new("policy-0123456789ab")),
        &specimen_key(),
    )
    .unwrap()
}

// ---------- golden shapes ----------

#[test]
fn golden_run_receipt_shape() {
    assert_golden("run_receipt.json", &specimen_receipt());
}

#[test]
fn golden_run_receipt_canonical_form() {
    // The exact byte string the signature covers — the form a transparency
    // log would witness. Pinned raw: no re-printing, no reformatting.
    let canonical = specimen_receipt().canonical_bytes().unwrap();
    let text = format!("{}\n", String::from_utf8(canonical).unwrap());
    assert_golden_text("run_receipt_canonical.json", &text);
}

#[test]
fn golden_receipt_key_shape() {
    // The content-addressed key id: public key hex plus its derived id.
    let key = specimen_key();
    assert_golden(
        "receipt_key.json",
        &json!({
            "public_key": key.public_key().to_hex(),
            "key_id": key.key_id(),
        }),
    );
}

#[test]
fn golden_receipt_event_kinds_shape() {
    // The wave's new RunEventKind wire name (the `capsule_event_kinds.json`
    // pattern; the exhaustive pre-R0.9 list is owned by `tests/agents.rs`).
    assert_golden(
        "receipt_event_kinds.json",
        &vec![RunEventKind::SigningKeyRotated],
    );
}

#[test]
fn golden_signing_key_rotation_shape() {
    // The journaled rotation payload: a genesis rotation (no previous key)
    // plus its successor, in one golden so both shapes are pinned.
    let at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1_800_000_000_000).unwrap();
    let genesis = SigningKeyRotation {
        previous_key_id: None,
        new_key_id: "aa".repeat(32),
        public_key: "bb".repeat(32),
        rotated_at: at,
    };
    let rotation = SigningKeyRotation {
        previous_key_id: Some("aa".repeat(32)),
        new_key_id: "cc".repeat(32),
        public_key: "dd".repeat(32),
        rotated_at: at,
    };
    assert_golden("signing_key_rotation.json", &vec![genesis, rotation]);
}

// ---------- the exit criteria ----------

#[test]
fn receipt_verifies_against_the_exported_snapshot() {
    let snapshot = specimen_journal().snapshot();
    let receipt = specimen_receipt();
    let verified = verify_receipt(&snapshot, &receipt, &specimen_key().public_key()).unwrap();
    assert_eq!(verified.run_id, "run-1");
    assert_eq!(verified.journal_head, receipt.journal_head);
    assert_eq!(verified.manifest_digest, receipt.manifest_digest);
    assert_eq!(verified.capsules.len(), 1);
    assert_eq!(verified.effect_receipts, 1);
    assert_eq!(
        verified.executor_policy,
        Some(PolicyVersion::new("policy-0123456789ab"))
    );
    assert_eq!(verified.capsule_policies, vec!["cedar-0123456789ab"]);
    assert_eq!(verified.denials.len(), 1);
    assert_eq!(verified.signer, receipt.signer);
}

#[test]
fn flipping_one_byte_in_any_journaled_event_fails_at_the_head() {
    let receipt = specimen_receipt();
    let key = specimen_key();
    let event_count = specimen_journal().len();
    for index in 0..event_count {
        let mut tampered: JournalSnapshot =
            serde_json::from_value(serde_json::to_value(specimen_journal().snapshot()).unwrap())
                .unwrap();
        // One byte, structurally: the event's recorded payload changes
        // while the snapshot's claimed head stays as exported.
        tampered.events[index].latency_ms = Some(
            tampered.events[index]
                .latency_ms
                .map(|ms| ms + 1)
                .unwrap_or(1),
        );
        let rejection = verify_receipt(&tampered, &receipt, &key.public_key()).unwrap_err();
        assert_eq!(
            rejection.component(),
            "journal_head",
            "event {index}: expected a journal-head rejection, got {rejection}"
        );
        let detail = rejection.to_string();
        assert!(
            detail.contains("recompute"),
            "event {index}: the rejection names the recomputed head: {detail}"
        );
    }
}

#[test]
fn rotated_key_fails_old_receipts_by_signer_id_and_the_old_key_still_verifies() {
    let snapshot = specimen_journal().snapshot();
    let old_key = specimen_key();
    let receipt = mint_receipt(
        &snapshot,
        Some(specimen_manifest()),
        Some(PolicyVersion::new("policy-0123456789ab")),
        &old_key,
    )
    .unwrap();

    // Rotation mints a new keypair; the receipt's signer still names the
    // old key id, and key ids are content addresses — offering the new
    // key is a definitive signer mismatch, not a guess.
    let new_key = SigningKey::from_bytes(&[9u8; 32]);
    let rejection = verify_receipt(&snapshot, &receipt, &new_key.public_key()).unwrap_err();
    assert_eq!(rejection.component(), "signer_key_id");

    // The old key — kept in the deployment's key history — still verifies.
    verify_receipt(&snapshot, &receipt, &old_key.public_key()).unwrap();
}

// ---------- component-named failures ----------

#[test]
fn tampered_manifest_digest_fails_naming_the_manifest() {
    let snapshot = specimen_journal().snapshot();
    let mut receipt = specimen_receipt();
    // Flip one hex digit in a pinned prompt hash; the committed digest no
    // longer re-hashes from the carried manifest.
    receipt
        .manifest
        .as_mut()
        .unwrap()
        .prompts
        .insert("system".into(), "0".repeat(64));
    let rejection = verify_receipt(&snapshot, &receipt, &specimen_key().public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::ManifestDigest { .. }),
        "{rejection}"
    );
    assert_eq!(rejection.component(), "manifest_digest");

    // Dropping the commitment while the manifest stays is the same named
    // failure from the other side.
    let mut receipt = specimen_receipt();
    receipt.manifest_digest = None;
    let rejection = verify_receipt(&snapshot, &receipt, &specimen_key().public_key()).unwrap_err();
    assert_eq!(rejection.component(), "manifest_digest");
}

#[test]
fn tampered_effect_ledger_fails_naming_the_ledger() {
    let snapshot = specimen_journal().snapshot();
    let mut receipt = specimen_receipt();
    receipt.effects[0] = "0".repeat(64);
    let rejection = verify_receipt(&snapshot, &receipt, &specimen_key().public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::EffectLedger(_)),
        "{rejection}"
    );
    assert_eq!(rejection.component(), "effect_ledger");

    // A ledger of the wrong length names the count divergence.
    let mut receipt = specimen_receipt();
    receipt.effects.push("1".repeat(64));
    let rejection = verify_receipt(&snapshot, &receipt, &specimen_key().public_key()).unwrap_err();
    assert_eq!(rejection.component(), "effect_ledger");
    assert!(rejection.to_string().contains("holds 2 entries"));
}

#[test]
fn tampered_capsule_map_and_policies_fail_by_name() {
    let snapshot = specimen_journal().snapshot();
    let key = specimen_key();

    let mut receipt = specimen_receipt();
    receipt
        .capsules
        .insert("researcher".into(), CapsuleId::from("0".repeat(32)));
    let rejection = verify_receipt(&snapshot, &receipt, &key.public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::CapsuleResolutions(_)),
        "{rejection}"
    );
    assert_eq!(rejection.component(), "capsule_resolutions");

    let mut receipt = specimen_receipt();
    receipt.capsule_policies = vec!["cedar-forged".into()];
    let rejection = verify_receipt(&snapshot, &receipt, &key.public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::CapsulePolicies(_)),
        "{rejection}"
    );
    assert_eq!(rejection.component(), "capsule_policies");
}

#[test]
fn tampered_denials_ledger_fails_naming_the_denials() {
    let snapshot = specimen_journal().snapshot();
    let mut receipt = specimen_receipt();
    // Erasing a denial is the forgery that matters: a run that attempted
    // forbidden access must keep saying so.
    receipt.denials.clear();
    let rejection = verify_receipt(&snapshot, &receipt, &specimen_key().public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::DenialsLedger(_)),
        "{rejection}"
    );
    assert_eq!(rejection.component(), "denials_ledger");
}

#[test]
fn broken_signature_and_wrong_run_fail_by_name() {
    let snapshot = specimen_journal().snapshot();
    let key = specimen_key();

    // A signature that does not cover the canonical statement.
    let mut receipt = specimen_receipt();
    receipt.signature = receipt.signature.replacen('a', "b", 1);
    let rejection = verify_receipt(&snapshot, &receipt, &key.public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::Signature(_)),
        "{rejection}"
    );
    assert_eq!(rejection.component(), "signature");

    // The receipt attests a different run than the snapshot records.
    let mut receipt = specimen_receipt();
    receipt.run_id = "run-2".into();
    let rejection = verify_receipt(&snapshot, &receipt, &key.public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::RunId { .. }),
        "{rejection}"
    );

    // A receipt from the future gets a clean refusal, not a panic.
    let mut receipt = specimen_receipt();
    receipt.format_version = RECEIPT_FORMAT_VERSION + 1;
    let rejection = verify_receipt(&snapshot, &receipt, &key.public_key()).unwrap_err();
    assert!(
        matches!(rejection, ReceiptRejection::FormatVersion { .. }),
        "{rejection}"
    );
}

#[test]
fn minting_refuses_tampered_evidence() {
    // Never sign evidence that fails its own chain check: a snapshot whose
    // events were edited without recomputing the head is rejected at mint.
    let mut snapshot = specimen_journal().snapshot();
    snapshot.events[0].status = rusty_agent_runtime::record::EventStatus::Error;
    let result = mint_receipt(&snapshot, None, None, &specimen_key());
    assert!(result.is_err());
}

#[test]
fn receipt_serde_roundtrip_and_sparse_wire_shape() {
    let receipt = specimen_receipt();
    let back: RunReceipt = serde_json::from_str(&serde_json::to_string(&receipt).unwrap()).unwrap();
    assert_eq!(receipt, back);

    // A receipt over an empty journal (no manifest, no capsules, no
    // effects, no denials) stays sparse on the wire — the additive rule
    // every contract here follows.
    let empty = Journal::new("run-empty", "thread-1", Clock::logical(1, 1));
    let receipt = mint_receipt(&empty.snapshot(), None, None, &specimen_key()).unwrap();
    let value = serde_json::to_value(&receipt).unwrap();
    for field in [
        "manifest_digest",
        "manifest",
        "capsules",
        "effects",
        "executor_policy",
        "capsule_policies",
        "denials",
    ] {
        assert!(value.get(field).is_none(), "{field} must stay absent");
    }
    // And it verifies: an empty ledger is a statement too.
    let verified =
        verify_receipt(&empty.snapshot(), &receipt, &specimen_key().public_key()).unwrap();
    assert_eq!(verified.effect_receipts, 0);
    assert!(verified.denials.is_empty());
}

#[test]
fn key_id_golden_is_a_published_digest() {
    // The key id derivation is pinned independently: sha256 of the public
    // key bytes, computed here through the one shared primitive.
    let key = specimen_key();
    assert_eq!(
        key.key_id(),
        sha256_hex(&key.public_key().to_bytes()),
        "key ids are content addresses of the public key"
    );
}
