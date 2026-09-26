//! Effect kernel v2 tests (R0.7 wave 1a).
//!
//! The marker traits' compile-fail behavior cannot be unit-tested directly
//! (and the workspace carries no trybuild), so these tests exercise the
//! enforcement surface at the API level: every admission helper's accept and
//! reject paths, the declaration-consistency check, deterministic effect-id
//! derivation through `TypedEffect`, and the receipt lookup that closes the
//! recovery loop.

use std::sync::Arc;

use serde_json::json;

use rusty_agent_runtime::effects::{
    admit_compensatable, admit_irreversible, admit_retry, admit_speculation, derive_effect_id,
    ApprovalToken, CompensatableEffect, CompensationRegistry, EffectViolation, IdempotentEffect,
    IrreversibleEffect, PureEffect, ReadOnlyEffect, TypedEffect, EFFECT_ID_DOMAIN,
};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::record::{sha256_hex, Effect, EffectReceipt};

// ---------- typed effect fixtures ----------

/// An irreversible effect: a card charge with no idempotency story.
struct ChargeCard {
    input: String,
    key: Option<String>,
}

impl ChargeCard {
    fn new(amount_cents: u64) -> Self {
        let input = json!({"amount_cents": amount_cents, "currency": "usd"});
        Self {
            input: sha256_hex(&serde_json::to_vec(&input).unwrap()),
            key: None,
        }
    }
}

impl TypedEffect for ChargeCard {
    const EFFECT: Effect = Effect::NonIdempotent;

    fn kind(&self) -> &str {
        "charge_card"
    }

    fn input_hash(&self) -> &str {
        &self.input
    }

    fn idempotency_key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

impl IrreversibleEffect for ChargeCard {}

/// A compensatable effect: a reservation a cancellation can undo.
struct ReserveSeat;

impl TypedEffect for ReserveSeat {
    const EFFECT: Effect = Effect::Compensatable;

    fn kind(&self) -> &str {
        "reserve_seat"
    }

    fn input_hash(&self) -> &str {
        "seat-input-hash"
    }
}

impl CompensatableEffect for ReserveSeat {}

/// An idempotent effect: an upsert under a stable key.
struct UpsertDocument {
    key: Option<String>,
}

impl TypedEffect for UpsertDocument {
    const EFFECT: Effect = Effect::Idempotent;

    fn kind(&self) -> &str {
        "upsert_document"
    }

    fn input_hash(&self) -> &str {
        "doc-input-hash"
    }

    fn idempotency_key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

impl IdempotentEffect for UpsertDocument {}

/// Pure work: deterministic ranking over its inputs.
struct RankResults;

impl TypedEffect for RankResults {
    const EFFECT: Effect = Effect::Pure;

    fn kind(&self) -> &str {
        "rank_results"
    }

    fn input_hash(&self) -> &str {
        "rank-input-hash"
    }
}

impl PureEffect for RankResults {}

/// A read-only lookup (no admission helper gates this class — retries are
/// unconstrained); present to pin the full marker ladder.
struct FetchPage;

impl TypedEffect for FetchPage {
    const EFFECT: Effect = Effect::ReadOnly;

    fn kind(&self) -> &str {
        "fetch_page"
    }

    fn input_hash(&self) -> &str {
        "fetch-input-hash"
    }
}

impl ReadOnlyEffect for FetchPage {}

/// A lying declaration: marked irreversible, declares pure. Compiles — the
/// marker traits carry no methods — and must be rejected at admission.
struct SneakyEffect;

impl TypedEffect for SneakyEffect {
    const EFFECT: Effect = Effect::Pure;

    fn kind(&self) -> &str {
        "sneaky"
    }

    fn input_hash(&self) -> &str {
        "sneaky-input-hash"
    }
}

impl IrreversibleEffect for SneakyEffect {}

// ---------- approval boundary ----------

#[test]
fn irreversible_effect_requires_a_scoped_approval() {
    let charge = ChargeCard::new(4_200);
    let scope = "run-7";

    // No token: rejected, naming the effect id the approval must cover.
    let violation = admit_irreversible(&charge, scope, None).unwrap_err();
    match violation {
        EffectViolation::MissingApproval { kind, effect_id } => {
            assert_eq!(kind, "charge_card");
            assert_eq!(effect_id, charge.effect_id(scope));
        }
        other => panic!("expected MissingApproval, got {other:?}"),
    }

    // A token minted for a *different* occurrence does not launder this one.
    let other = ChargeCard::new(9_900);
    let wrong = ApprovalToken::for_effect(&other, scope, "ops:amjad");
    let violation = admit_irreversible(&charge, scope, Some(&wrong)).unwrap_err();
    match violation {
        EffectViolation::ApprovalScopeMismatch {
            required,
            presented,
            ..
        } => {
            assert_eq!(required, charge.effect_id(scope));
            assert_eq!(presented, other.effect_id(scope));
        }
        other => panic!("expected ApprovalScopeMismatch, got {other:?}"),
    }

    // The scoped token admits exactly this occurrence — and the scope is
    // load-bearing: the same approval does not carry into another run.
    let approval = ApprovalToken::for_effect(&charge, scope, "ops:amjad");
    assert!(approval.admits(&charge.effect_id(scope)));
    assert_eq!(approval.approved_by(), "ops:amjad");
    admit_irreversible(&charge, scope, Some(&approval)).unwrap();
    assert!(admit_irreversible(&charge, "run-8", Some(&approval)).is_err());
}

#[test]
fn approval_token_is_evidence_shaped_serde() {
    let charge = ChargeCard::new(4_200);
    let token = ApprovalToken::for_effect(&charge, "run-7", "policy:auto-approve-v2");
    let back: ApprovalToken =
        serde_json::from_str(&serde_json::to_string(&token).unwrap()).unwrap();
    assert_eq!(token, back);
}

// ---------- compensation registration ----------

#[test]
fn compensatable_effect_requires_a_registered_handler() {
    let reservation = ReserveSeat;
    let mut registry = CompensationRegistry::new();

    // No handler registered: rejected — the undo path is a precondition.
    // (Matched rather than `unwrap_err`: the Ok payload is a handler, which
    // is not `Debug`.)
    let violation = match admit_compensatable(&reservation, &registry) {
        Err(violation) => violation,
        Ok(_) => panic!("admission without a registered handler must fail"),
    };
    assert_eq!(
        violation,
        EffectViolation::MissingCompensation {
            kind: "reserve_seat".into()
        }
    );

    // Register the rollback: admission returns the handler, and it runs.
    let handler: rusty_agent_runtime::effects::CompensationHandler =
        Arc::new(|output| Ok(json!({"cancelled": output.get("seat")})));
    assert!(registry.register("reserve_seat", handler).is_none());
    let admitted = admit_compensatable(&reservation, &registry).unwrap();
    let record = admitted(&json!({"seat": "14A"})).unwrap();
    assert_eq!(record, json!({"cancelled": "14A"}));

    // Re-registration replaces deliberately, returning the old handler.
    let replacement: rusty_agent_runtime::effects::CompensationHandler =
        Arc::new(|_| Ok(json!({"cancelled": "all"})));
    assert!(registry.register("reserve_seat", replacement).is_some());
}

// ---------- retry admission ----------

#[test]
fn idempotent_effect_retries_only_under_a_key() {
    // Without the key, the declaration is meaningless — rejected. This is
    // the R0.6 envelope convention ("the key is what the idempotency
    // declaration means at the wire") made checkable.
    let keyless = UpsertDocument { key: None };
    let violation = admit_retry(&keyless).unwrap_err();
    assert_eq!(
        violation,
        EffectViolation::MissingIdempotencyKey {
            kind: "upsert_document".into()
        }
    );

    let keyed = UpsertDocument {
        key: Some("run-7:doc:3".into()),
    };
    admit_retry(&keyed).unwrap();
}

// ---------- speculation ----------

#[test]
fn pure_effect_is_admitted_to_speculation() {
    admit_speculation(&RankResults).unwrap();
}

#[test]
fn read_only_effects_need_no_admission_gate() {
    // ReadOnly retries are unconstrained and speculation is unsafe (the
    // world may have changed), so the class intentionally has no admission
    // helper; pin its wire mapping so the ladder stays complete.
    let fetch = FetchPage;
    assert_eq!(FetchPage::EFFECT, Effect::ReadOnly);
    assert!(FetchPage::EFFECT.is_freely_repeatable());
    assert_eq!(fetch.kind(), "fetch_page");
}

// ---------- declaration consistency ----------

#[test]
fn mismatched_marker_and_declared_class_are_rejected() {
    // The sneaky type says Pure but wears the Irreversible marker: every
    // admission helper checks the agreement first.
    let violation = admit_irreversible(&SneakyEffect, "run-7", None).unwrap_err();
    assert_eq!(
        violation,
        EffectViolation::DeclarationMismatch {
            kind: "sneaky".into(),
            marker: Effect::NonIdempotent,
            declared: Effect::Pure,
        }
    );
}

// ---------- deterministic effect ids ----------

#[test]
fn typed_effect_ids_are_deterministic_and_scope_bound() {
    let charge = ChargeCard::new(4_200);

    // Same inputs, same scope: the same id — this is what makes "did this
    // effect already commit?" a lookup instead of a guess.
    assert_eq!(charge.effect_id("run-7"), charge.effect_id("run-7"));
    // The derivation the type performs is the public formula over its own
    // contract fields.
    assert_eq!(
        charge.effect_id("run-7"),
        derive_effect_id("run-7", "charge_card", charge.input_hash(), None)
    );
    // Different run scope: a different id, so one run's receipt can never
    // answer another run's recovery question.
    assert_ne!(charge.effect_id("run-7"), charge.effect_id("run-8"));

    // The domain prefix is versioned and load-bearing.
    assert!(EFFECT_ID_DOMAIN.ends_with("/v1"));
    let manual = sha256_hex(
        [
            EFFECT_ID_DOMAIN,
            "run-7",
            "charge_card",
            charge.input_hash(),
            "-",
        ]
        .join("\n")
        .as_bytes(),
    );
    assert_eq!(charge.effect_id("run-7").as_str(), manual);
}

#[test]
fn recovery_finds_the_committed_receipt_by_effect_id() {
    // The full loop: a charge executed under a key and journaled its receipt
    // with the derived effect id; after a crash, the re-driven run derives
    // the same id and finds the receipt instead of re-charging.
    let mut charge = ChargeCard::new(4_200);
    charge.key = Some("run-7:charge:1".into());
    let id = charge.effect_id("run-7");

    let journal = Journal::new("run-7", "thread-7", Clock::System);
    let receipt = EffectReceipt {
        provider: "stripe".into(),
        provider_id: "ch_committed".into(),
        idempotency_key: "run-7:charge:1".into(),
        task_id: Some("task-1".into()),
        effect_id: Some(id.as_str().to_owned()),
    };
    journal.record_effect_receipt(&receipt, None);

    let snapshot = journal.snapshot();
    assert_eq!(
        snapshot.find_effect_receipt_by_effect_id(&id),
        Some(receipt)
    );

    // A *different* occurrence (new key → new id) finds nothing and would
    // execute — the lookup never over-serves.
    let mut next = ChargeCard::new(4_200);
    next.key = Some("run-7:charge:2".into());
    assert_eq!(
        snapshot.find_effect_receipt_by_effect_id(&next.effect_id("run-7")),
        None
    );
}
