//! Rewrite validation tests (EP-06-S09): loss-bounded, hash-checked curated
//! rewrites.
//!
//! Six test groups:
//!
//! - **Hash checking** — match pass, mismatch fail.
//! - **Loss bounds** — within bound pass, exceed bound fail, exceed bound
//!   with justifications pass.
//! - **Fact counting** — arrays, single-array objects, general objects,
//!   strings, scalars.
//! - **Diff and audit shape** — removed facts, diff summary, refusal reason.
//! - **Optimistic concurrency** — the pre-image hash is the content hash,
//!   not the memory id (which includes provenance).

use chrono::Utc;
use serde_json::{json, Value};

use rusty_agent_runtime::memory::{
    validate_rewrite, MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, ProvenanceAuthor,
    RewriteAudit, RewriteProposal, ScopeAddress, ValidityWindow,
};

fn provenance() -> MemoryProvenance {
    MemoryProvenance {
        author: ProvenanceAuthor::Human {
            human_id: "tester".into(),
        },
        evidence: Default::default(),
        written_at: Utc::now(),
    }
}

fn record(content: Value) -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "agent-1"),
        provenance(),
        1.0,
        ValidityWindow::starting(Utc::now()),
        Utc::now(),
        content,
    )
    .unwrap()
}

fn pre_image_hash(record: &MemoryRecord) -> String {
    record.content.content_hash().unwrap()
}

// ---------- hash checking ----------

#[test]
fn hash_match_passes() {
    let old = record(json!(["fact-a", "fact-b", "fact-c"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["fact-a", "fact-b", "fact-c", "fact-d"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(
        result.passed,
        "hash match should pass: {}",
        result.refusal_reason.unwrap_or_default()
    );
    assert_eq!(result.fact_count_before, 3);
    assert_eq!(result.fact_count_after, 4);
    assert!(result.refusal_reason.is_none());
}

#[test]
fn hash_mismatch_fails() {
    let old = record(json!(["fact-a", "fact-b"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: "0000000000000000000000000000000000000000000000000000000000000000"
            .into(),
        proposed_content: json!(["fact-a", "fact-b", "fact-c"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(!result.passed);
    assert_eq!(result.fact_count_before, 0);
    assert_eq!(result.fact_count_after, 0);
    let reason = result.refusal_reason.unwrap();
    assert!(
        reason.contains("hash mismatch"),
        "reason should mention hash mismatch: {reason}"
    );
    assert!(result.diff_summary.contains("hash mismatch"));
}

// ---------- loss bounds ----------

#[test]
fn loss_within_bound_passes() {
    // 19% drop (1 of 5) is within the 20% default bound.
    let old = record(json!(["a", "b", "c", "d", "e"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a", "b", "c", "d"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(
        result.passed,
        "19% drop should pass: {}",
        result.refusal_reason.unwrap_or_default()
    );
    assert_eq!(result.fact_count_before, 5);
    assert_eq!(result.fact_count_after, 4);
    assert!(result.removed_facts.contains(&"e".to_string()));
}

#[test]
fn loss_exceeds_bound_without_justification_fails() {
    // 25% drop (1 of 4) exceeds the 20% default bound, no justifications.
    let old = record(json!(["a", "b", "c", "d"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a", "b", "c"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(!result.passed);
    let reason = result.refusal_reason.as_ref().unwrap();
    assert!(
        reason.contains("loss bound exceeded"),
        "reason should mention loss bound: {reason}"
    );
    assert!(
        reason.contains("25.0%"),
        "reason should state the loss percentage: {reason}"
    );
    assert!(result.diff_summary.contains("exceeds bound"));
    assert_eq!(result.removed_facts, vec!["d"]);
}

#[test]
fn loss_exceeds_bound_with_justifications_passes() {
    // 25% drop (1 of 4) exceeds the 20% default bound, but a justification
    // is provided for the single removed fact.
    let old = record(json!(["a", "b", "c", "d"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a", "b", "c"]),
        justifications: vec!["d is superseded by new consolidated rule".into()],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(
        result.passed,
        "justified 25% drop should pass: {}",
        result.refusal_reason.unwrap_or_default()
    );
    assert!(result.refusal_reason.is_none());
    assert_eq!(result.removed_facts, vec!["d"]);
}

#[test]
fn loss_exceeds_bound_with_insufficient_justifications_fails() {
    // Two facts removed, only one justification provided.
    let old = record(json!(["a", "b", "c", "d"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a", "b"]),
        justifications: vec!["c is stale".into()],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(!result.passed);
    let reason = result.refusal_reason.as_ref().unwrap();
    assert!(reason.contains("loss bound exceeded"), "reason: {reason}");
}

// ---------- fact counting shapes ----------

#[test]
fn count_facts_array() {
    let old = record(json!(["red", "green", "blue"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["red", "green"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert_eq!(result.fact_count_before, 3);
    assert_eq!(result.fact_count_after, 2);
}

#[test]
fn count_facts_single_array_object() {
    // The common block shape: {"facts": [...]}
    let old = record(json!({"facts": ["x", "y", "z"]}));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!({"facts": ["x", "y"]}),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert_eq!(result.fact_count_before, 3);
    assert_eq!(result.fact_count_after, 2);
}

#[test]
fn count_facts_general_object() {
    // General object: each key-value pair is a fact.
    let old = record(json!({"a": 1, "b": 2}));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!({"a": 1}),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert_eq!(result.fact_count_before, 2);
    assert_eq!(result.fact_count_after, 1);
}

#[test]
fn count_facts_string_lines() {
    let old = record(json!("line one\nline two\nline three"));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!("line one\nline two"),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert_eq!(result.fact_count_before, 3);
    assert_eq!(result.fact_count_after, 2);
}

#[test]
fn count_facts_scalar() {
    let old = record(json!(42));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(42),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert_eq!(result.fact_count_before, 1);
    assert_eq!(result.fact_count_after, 1);
    assert!(result.passed);
}

// ---------- diff and audit shape ----------

#[test]
fn diff_summary_includes_counts_and_percentage() {
    let old = record(json!(["a", "b", "c", "d"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a", "b"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(
        result.diff_summary.contains("4 → 2"),
        "diff: {}",
        result.diff_summary
    );
    assert!(
        result.diff_summary.contains("50.0% loss"),
        "diff: {}",
        result.diff_summary
    );
}

#[test]
fn audit_round_trips() {
    let old = record(json!(["a", "b", "c"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a", "b"]),
        justifications: vec!["c is stale".into()],
        loss_bound_fraction: 0.20,
    };
    let validation = validate_rewrite(&old, &proposal).unwrap();
    let audit = RewriteAudit {
        pre_image_memory_id: old.memory_id.clone(),
        post_image_memory_id: None,
        pre_image: json!(["a", "b", "c"]),
        post_image: None,
        diff_summary: validation.diff_summary.clone(),
        source_citations: vec!["source-1".into(), "source-2".into()],
        acting_session_id: "consolidation-run-7".into(),
        validation,
        audited_at: Utc::now(),
    };
    let json = serde_json::to_string(&audit).unwrap();
    let round: RewriteAudit = serde_json::from_str(&json).unwrap();
    assert_eq!(round.pre_image_memory_id, old.memory_id);
    assert!(round.validation.passed);
    assert_eq!(round.source_citations.len(), 2);
}

// ---------- edge cases ----------

#[test]
fn empty_pre_image_no_loss() {
    let old = record(json!([]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a"]),
        justifications: vec![],
        loss_bound_fraction: 0.20,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(result.passed);
    assert_eq!(result.fact_count_before, 0);
    assert_eq!(result.fact_count_after, 1);
}

#[test]
fn zero_loss_bound_rejects_any_drop() {
    let old = record(json!(["a", "b"]));
    let proposal = RewriteProposal {
        expected_pre_image_hash: pre_image_hash(&old),
        proposed_content: json!(["a"]),
        justifications: vec!["b removed".into()],
        loss_bound_fraction: 0.0,
    };
    let result = validate_rewrite(&old, &proposal).unwrap();
    assert!(result.passed, "justified drop under zero bound should pass");
}
