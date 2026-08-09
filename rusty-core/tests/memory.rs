//! Governed memory integration tests (R0.8 Rusty Learn, waves 1–2).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of `MemoryRecord`,
//!   `ScopeAddress`, `MemoryProvenance`, `ValidityWindow`, `MemoryQuery`,
//!   `ContextBudget`, and `MemoryAssembly` are pinned against checked-in
//!   JSON under `tests/golden/`. Any accidental contract drift fails here.
//!   To bless an intentional contract change, re-run with `UPDATE_GOLDEN=1`
//!   and review the diff. (The new `RunEventKind` variants' wire names are
//!   pinned in `memory_event_kinds.json`; the exhaustive
//!   `run_event_kind.json` list is owned by another test file.) Wave 2 adds
//!   the `Correction` contract (with its target enum), the
//!   `MemoryForgetTombstone`, and the `MemoryConflict` review item.
//! - **Retrieval semantics** — the structured filters, the deterministic
//!   assembly rank, and the token-bounded packing (bytes÷4 estimate with
//!   the declared margin) over the in-memory store.
//! - **The journaled seam** — reads journal `MemoryRead` with the resolved
//!   query and the assembly; equal store state and equal budget produce
//!   byte-equal assemblies.
//! - **Wave-1 exit: replay-serving** — an exact replay of a memory-reading
//!   run serves the journaled assembly byte-identically, proven end to end
//!   through `ExactReplay::run_and_verify`; and the two negative proofs:
//!   a divergent request fails loudly, a live store cannot impersonate the
//!   journaled assembly.
//! - **Wave 2: the correction loop and memory operations** — attribution
//!   validation, candidacy, same-key supersession semantics (a summary
//!   supersedes its sources), conflict detection (flags, never resolves),
//!   consolidation's summary invariants, and forget planning (the
//!   transitive dependent-summary walk).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::error::RustyError;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::{Graph, GraphBuilder};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource, PARENT_EVENT_KEY};
use rusty_agent_runtime::memory::{
    apply_query, assemble, consolidation_summary, detect_conflicts, estimated_tokens,
    memory_effect_key, memory_forget_effect_key, plan_forget, BudgetOverflow, Candidacy,
    ContextBudget, Correction, CorrectionTarget, ForgetReason, InMemoryMemoryStore, MemoryEvidence,
    MemoryForgetTombstone, MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord,
    MemoryReplaySource, MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor, ScopeAddress,
    ValidityWindow, DEFAULT_TOKEN_MARGIN_PERCENT, MEMORY_SCHEMA_VERSION, TOKEN_BYTES_PER_ESTIMATE,
};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{Effect, PayloadRef, RunEventKind};
use rusty_agent_runtime::replay::{ExactReplay, ReplayParams};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};

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

// ---------- shared fixtures ----------

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn full_provenance() -> MemoryProvenance {
    MemoryProvenance {
        author: ProvenanceAuthor::Agent {
            agent_id: "researcher-7".into(),
        },
        evidence: MemoryEvidence {
            run_id: Some("run-abc".into()),
            event_ids: vec!["run-abc:3".into(), "run-abc:4".into()],
            correction_id: Some("correction-9".into()),
            candidate_id: None,
            source_memory_ids: vec!["a".repeat(64), "b".repeat(64)],
        },
        written_at: ts(1_750_000_001_000),
    }
}

/// A record with every populated field exercised (the golden shape), inline
/// content.
fn full_record() -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        full_provenance(),
        0.95,
        ValidityWindow {
            valid_from: ts(1_750_000_000_000),
            valid_until: Some(ts(1_850_000_000_000)),
        },
        ts(1_750_000_001_000),
        json!({"timezone": "UTC+4", "source": "profile form"}),
    )
    .unwrap()
    .with_key("timezone")
    .with_tags(["profile", "locale"])
    .with_priority(5)
    .with_expires_at(ts(1_900_000_000_000))
    .with_supersedes("c".repeat(64))
}

// ---------- golden files ----------

#[test]
fn golden_memory_record_shape() {
    assert_golden("memory_record.json", &full_record());
}

#[test]
fn golden_memory_scope_shape() {
    // All five scopes in declaration order, each with a concrete id: the
    // taxonomy and the address shape are the contract.
    assert_golden(
        "memory_scope.json",
        &vec![
            ScopeAddress::new(MemoryScope::Run, "run-abc"),
            ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
            ScopeAddress::new(MemoryScope::Team, "team-blue"),
            ScopeAddress::new(MemoryScope::User, "user-7"),
            ScopeAddress::new(MemoryScope::Tenant, "acme"),
        ],
    );
}

#[test]
fn golden_memory_provenance_shape() {
    assert_golden("memory_provenance.json", &full_provenance());
}

#[test]
fn golden_validity_window_shape() {
    assert_golden(
        "validity_window.json",
        &ValidityWindow {
            valid_from: ts(1_750_000_000_000),
            valid_until: Some(ts(1_850_000_000_000)),
        },
    );
}

#[test]
fn golden_memory_query_shape() {
    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::Agent, "researcher-7")),
        kinds: vec![MemoryKind::Fact, MemoryKind::Preference],
        key: Some("timezone".into()),
        tags: vec!["profile".into()],
        valid_at: Some(ts(1_800_000_000_000)),
        min_confidence: Some(0.5),
        include_expired: true,
        include_superseded: true,
        authored_by: Some(ProvenanceAuthor::Human {
            human_id: "amjad".into(),
        }),
        candidates_only: false,
        as_of: Some(ts(1_800_000_000_000)),
    };
    assert_golden("memory_query.json", &query);
}

#[test]
fn golden_context_budget_shape() {
    let budget = ContextBudget::new(512)
        .with_margin_percent(25)
        .with_overflow(BudgetOverflow::Fail);
    assert_golden("context_budget.json", &budget);
}

#[test]
fn golden_memory_assembly_shape() {
    // Two records, one with artifact-referenced content (both payload forms
    // pinned), packed under a budget that fits both — the journaled
    // `MemoryRead` output shape.
    let big = MemoryRecord::new(
        MemoryKind::Summary,
        ScopeAddress::new(MemoryScope::Team, "team-blue"),
        MemoryProvenance {
            author: ProvenanceAuthor::Distiller {
                name: "consolidator-v1".into(),
            },
            evidence: MemoryEvidence {
                source_memory_ids: vec!["d".repeat(64)],
                ..MemoryEvidence::default()
            },
            written_at: ts(1_750_000_002_000),
        },
        0.8,
        ValidityWindow::starting(ts(1_750_000_000_000)),
        ts(1_750_000_002_000),
        json!({"summary": "s".repeat(4096)}),
    )
    .unwrap();
    let first = full_record();
    // Rank: priority 5 leads, then confidence/recency — the small record
    // packs first, the big artifact-referenced one second.
    let budget = ContextBudget::new(4096);
    let assembly = assemble(vec![big, first], &budget).unwrap();
    assert!(!assembly.truncated);
    assert_eq!(assembly.memory_ids.len(), 2);
    assert_golden("memory_assembly.json", &assembly);
}

#[test]
fn golden_memory_event_kinds_shape() {
    // The additive R0.8 variants' wire names: `memory_read` / `memory_write`
    // (wave 1) and `memory_forget` (wave 2's tombstone). The exhaustive
    // `run_event_kind.json` list is owned by `tests/agents.rs` (outside this
    // stream's file scope); the names are pinned here so no wire shape
    // lands unpinned. Corrections journal through `memory_write` — there is
    // no correction event kind, by design.
    assert_golden(
        "memory_event_kinds.json",
        &vec![
            RunEventKind::MemoryRead,
            RunEventKind::MemoryWrite,
            RunEventKind::MemoryForget,
        ],
    );
}

// ---------- wave 2: the correction loop and memory operations ----------

/// The golden correction: every populated field exercised, with the
/// run-event target (the richest variant).
fn full_correction() -> Correction {
    Correction {
        correction_id: "correction-9".into(),
        author: "amjad".into(),
        target: CorrectionTarget::RunEvent {
            run_id: "run-abc".into(),
            event_id: "run-abc:7".into(),
        },
        corrected: json!({"answer": "42", "unit": " AED"}),
        scope: ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
        rationale: Some("the run quoted the pre-2024 exchange rate".into()),
    }
}

#[test]
fn golden_correction_shape() {
    assert_golden("correction.json", &full_correction());
}

#[test]
fn golden_correction_target_shape() {
    // All three targets in declaration order: what a correction may correct
    // is the contract.
    assert_golden(
        "correction_target.json",
        &vec![
            CorrectionTarget::RunEvent {
                run_id: "run-abc".into(),
                event_id: "run-abc:7".into(),
            },
            CorrectionTarget::Memory {
                memory_id: "a".repeat(64),
            },
            CorrectionTarget::Prompt {
                prompt_hash: "b".repeat(64),
            },
        ],
    );
}

#[test]
fn golden_memory_forget_tombstone_shape() {
    // The tombstone carries metadata by construction — id, scope, reason,
    // dependent invalidations; there is no content field a serializer could
    // leak the forgotten bytes through.
    assert_golden(
        "memory_forget_tombstone.json",
        &MemoryForgetTombstone {
            memory_id: "a".repeat(64),
            scope: ScopeAddress::new(MemoryScope::User, "user-7"),
            reason: ForgetReason::ErasureRequest,
            invalidated: vec!["c".repeat(64), "d".repeat(64)],
        },
    );
}

#[test]
fn golden_memory_conflict_shape() {
    assert_golden(
        "memory_conflict.json",
        &rusty_agent_runtime::memory::MemoryConflict {
            scope: ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
            key: "timezone".into(),
            memory_ids: vec!["a".repeat(64), "b".repeat(64)],
            overlap: ValidityWindow {
                valid_from: ts(1_750_000_000_000),
                valid_until: Some(ts(1_850_000_000_000)),
            },
        },
    );
}

#[test]
fn correction_attribution_and_scope_path() {
    let correction = full_correction();
    assert_eq!(
        correction.attribution(),
        "human:amjad via correction:correction-9"
    );
    assert_eq!(
        correction.author_as_provenance(),
        ProvenanceAuthor::Human {
            human_id: "amjad".into()
        }
    );
    let evidence = correction.evidence();
    assert_eq!(evidence.correction_id.as_deref(), Some("correction-9"));
    assert_eq!(evidence.run_id.as_deref(), Some("run-abc"));
    assert_eq!(evidence.event_ids, vec!["run-abc:7".to_string()]);
    // Agent scope is candidacy; run scope adopts directly.
    assert!(correction.is_candidate());
    let run_scope = Correction {
        scope: ScopeAddress::new(MemoryScope::Run, "run-abc"),
        ..full_correction()
    };
    assert!(!run_scope.is_candidate());
}

#[test]
fn correction_without_an_author_is_rejected_at_deserialization() {
    // The load-bearing validation: a correction that cannot name its
    // corrector is indistinguishable from a prompt edit.
    let mut wire = serde_json::to_value(full_correction()).unwrap();
    wire["author"] = json!("   ");
    assert!(serde_json::from_value::<Correction>(wire).is_err());
    let mut wire = serde_json::to_value(full_correction()).unwrap();
    wire["correction_id"] = json!("");
    assert!(serde_json::from_value::<Correction>(wire).is_err());
    let mut wire = serde_json::to_value(full_correction()).unwrap();
    wire.as_object_mut().unwrap().remove("author");
    assert!(serde_json::from_value::<Correction>(wire).is_err());
}

#[test]
fn candidacy_marks_candidates_and_filters_them() {
    let candidate = record_with_candidacy(true);
    let adopted = record_with_candidacy(false);
    let universe = vec![candidate.clone(), adopted.clone()];
    let query = MemoryQuery {
        candidates_only: true,
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, ts(6_000)), vec![candidate]);
    // Candidacy stays absent from the wire when unset (additive field).
    let wire = serde_json::to_value(&adopted).unwrap();
    assert!(!wire.as_object().unwrap().contains_key("candidacy"));
}

/// A fact at user scope, marked pending-candidate (or not).
fn record_with_candidacy(candidate: bool) -> MemoryRecord {
    let record = seeded_record(
        MemoryKind::Fact,
        Some(if candidate { "cand" } else { "adopt" }),
        0.9,
        1_000,
    );
    if candidate {
        record.with_candidacy(Candidacy::Pending)
    } else {
        record
    }
}

// ---------- contract behavior ----------

#[test]
fn scope_address_and_author_string_forms() {
    assert_eq!(
        ScopeAddress::new(MemoryScope::Agent, "researcher-7").as_address(),
        "agent:researcher-7"
    );
    assert_eq!(
        ScopeAddress::new(MemoryScope::Run, "run-abc").as_address(),
        "run:run-abc"
    );
    assert_eq!(
        ProvenanceAuthor::Agent {
            agent_id: "researcher-7".into()
        }
        .as_id_string(),
        "agent:researcher-7"
    );
    assert_eq!(
        ProvenanceAuthor::Distiller {
            name: "consolidator-v1".into()
        }
        .as_id_string(),
        "distiller:consolidator-v1"
    );
    assert_eq!(ProvenanceAuthor::System.as_id_string(), "system");
    // The derived write key, exactly as the design spells it.
    assert_eq!(
        memory_effect_key(
            &ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
            "m-1"
        ),
        "memory:agent:researcher-7:m-1"
    );
}

#[test]
fn schema_version_matches_the_manifest_pin_vocabulary() {
    // The pin vocabulary RunManifest::with_memory_schema records.
    assert_eq!(MEMORY_SCHEMA_VERSION, "memory-v1");
    assert_eq!(TOKEN_BYTES_PER_ESTIMATE, 4);
    assert_eq!(DEFAULT_TOKEN_MARGIN_PERCENT, 20);
}

#[test]
fn serde_roundtrips_preserve_the_contracts() {
    let record = full_record();
    let back: MemoryRecord =
        serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
    assert_eq!(record, back);

    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
        min_confidence: Some(0.5),
        ..MemoryQuery::default()
    };
    let wire = serde_json::to_string(&query).unwrap();
    let back: MemoryQuery = serde_json::from_str(&wire).unwrap();
    assert_eq!(query, back);
    // Sparse stays sparse: unset filters are absent, not null.
    let wire: Value = serde_json::from_str(&wire).unwrap();
    let object = wire.as_object().unwrap();
    assert!(object.contains_key("scope"));
    assert!(object.contains_key("min_confidence"));
    for absent in [
        "kinds",
        "key",
        "tags",
        "valid_at",
        "include_expired",
        "include_superseded",
        "authored_by",
        "as_of",
    ] {
        assert!(!object.contains_key(absent), "`{absent}` must stay absent");
    }
}

#[test]
fn pre_r08_sparse_record_json_still_loads() {
    // The additive-evolution rule, exercised: a record carrying only the
    // mandatory fields (no key/tags/priority/expires/supersedes/embedding)
    // is the minimal wire shape and must keep deserializing.
    let minimal = json!({
        "memory_id": "e".repeat(64),
        "kind": "preference",
        "scope": {"scope": "tenant", "id": "acme"},
        "provenance": {
            "author": {"type": "system"},
            "written_at": "2025-09-21T12:00:00Z",
        },
        "confidence": 1.0,
        "validity": {"valid_from": "2025-09-21T12:00:00Z"},
        "created_at": "2025-09-21T12:00:00Z",
        "content": {"kind": "inline", "value": {"rule": "be brief"}},
    });
    let record: MemoryRecord = serde_json::from_value(minimal).unwrap();
    assert_eq!(record.kind, MemoryKind::Preference);
    assert_eq!(record.scope.scope, MemoryScope::Tenant);
    assert_eq!(record.priority, 0);
    assert!(record.tags.is_empty());
    assert!(record.embedding.is_none());
}

// ---------- retrieval semantics ----------

fn seeded_record(
    kind: MemoryKind,
    key: Option<&str>,
    confidence: f64,
    created_ms: i64,
) -> MemoryRecord {
    let mut record = MemoryRecord::new(
        kind,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        MemoryProvenance {
            author: ProvenanceAuthor::System,
            evidence: MemoryEvidence::default(),
            written_at: ts(created_ms),
        },
        confidence,
        ValidityWindow {
            valid_from: ts(0),
            valid_until: Some(ts(10_000)),
        },
        ts(created_ms),
        json!({"key": key, "confidence": confidence}),
    )
    .unwrap();
    record.key = key.map(str::to_owned);
    record
}

#[test]
fn structured_filters_each_apply() {
    let fact = seeded_record(MemoryKind::Fact, Some("timezone"), 0.9, 1_000);
    let preference = seeded_record(MemoryKind::Preference, Some("tone"), 0.4, 2_000);
    let example = seeded_record(MemoryKind::Example, None, 0.95, 3_000);
    // A record superseded by `fact`, and an expired one.
    let mut superseded = seeded_record(MemoryKind::Fact, Some("timezone"), 0.1, 500);
    superseded.memory_id = "0".repeat(64);
    let fact = fact.with_supersedes(superseded.memory_id.clone());
    let mut expired = seeded_record(MemoryKind::Fact, Some("old"), 0.99, 100);
    expired.expires_at = Some(ts(5_000));
    let universe = vec![
        fact.clone(),
        preference.clone(),
        example.clone(),
        superseded.clone(),
        expired.clone(),
    ];
    let now = ts(6_000);

    // Kind filter.
    let query = MemoryQuery {
        kinds: vec![MemoryKind::Preference],
        ..MemoryQuery::default()
    };
    assert_eq!(
        apply_query(&universe, &query, now),
        vec![preference.clone()]
    );

    // Key equality + minimum confidence.
    let query = MemoryQuery {
        key: Some("timezone".into()),
        min_confidence: Some(0.5),
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![fact.clone()]);

    // Superseded records are filtered by default, included on request.
    let query = MemoryQuery {
        key: Some("timezone".into()),
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![fact.clone()]);
    let query = MemoryQuery {
        key: Some("timezone".into()),
        include_superseded: true,
        ..MemoryQuery::default()
    };
    let mut hits = apply_query(&universe, &query, now);
    hits.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    let mut expected = vec![fact.clone(), superseded.clone()];
    expected.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));
    assert_eq!(hits, expected);

    // Expired records are filtered by default (now is past the TTL),
    // included on request.
    let query = MemoryQuery {
        key: Some("old".into()),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());
    let query = MemoryQuery {
        key: Some("old".into()),
        include_expired: true,
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![expired.clone()]);

    // Validity-at-time: the windows are [0, 10_000).
    let query = MemoryQuery {
        valid_at: Some(ts(20_000)),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());
    let query = MemoryQuery {
        valid_at: Some(ts(9_999)),
        kinds: vec![MemoryKind::Example],
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &query, now), vec![example.clone()]);

    // Author filter.
    let query = MemoryQuery {
        authored_by: Some(ProvenanceAuthor::Human {
            human_id: "amjad".into(),
        }),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());

    // Scope filter excludes other scopes' records.
    let query = MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::Agent, "user-7")),
        ..MemoryQuery::default()
    };
    assert!(apply_query(&universe, &query, now).is_empty());
}

#[test]
fn assembly_rank_and_budget_are_deterministic() {
    // Priority beats confidence beats recency; the content address breaks
    // the final tie.
    let low_priority_high_confidence = seeded_record(MemoryKind::Fact, None, 0.99, 3_000);
    let high_priority = seeded_record(MemoryKind::Fact, None, 0.1, 100).with_priority(10);
    let mid = seeded_record(MemoryKind::Fact, None, 0.5, 2_000);
    let records = vec![
        low_priority_high_confidence.clone(),
        mid.clone(),
        high_priority.clone(),
    ];
    let budget = ContextBudget::new(u32::MAX).with_margin_percent(0);
    let assembly = assemble(records.clone(), &budget).unwrap();
    assert_eq!(
        assembly.memory_ids,
        vec![
            high_priority.memory_id.clone(),
            low_priority_high_confidence.memory_id.clone(),
            mid.memory_id.clone(),
        ]
    );

    // Byte-equal assemblies from equal input, independent of input order.
    let mut reversed = records.clone();
    reversed.reverse();
    let again = assemble(reversed, &budget).unwrap();
    assert_eq!(
        serde_json::to_vec(&assembly).unwrap(),
        serde_json::to_vec(&again).unwrap()
    );

    // A budget packs a prefix and reports the accounting honestly.
    let each = estimated_tokens(high_priority.content_bytes(), 0);
    let budget = ContextBudget::new(each).with_margin_percent(0);
    let packed = assemble(records, &budget).unwrap();
    assert_eq!(packed.memory_ids, vec![high_priority.memory_id.clone()]);
    assert!(packed.truncated);
    assert_eq!(packed.token_accounting.used_tokens, each);
    assert_eq!(packed.token_accounting.budget_tokens, each);
    assert_eq!(packed.token_accounting.bytes_per_token, 4);

    // Fail overflow refuses the prefix.
    let budget = budget.with_overflow(BudgetOverflow::Fail);
    assert!(assemble(
        vec![high_priority, low_priority_high_confidence, mid],
        &budget
    )
    .is_err());
}

// ---------- the journaled seam ----------

const RUN_ID: &str = "memory-run-1";
const THREAD_ID: &str = "memory-thread-1";
const CLOCK_START_MS: u64 = 1_000_000;
const CLOCK_TICK_MS: u64 = 5;
const RNG_SEED: u64 = 42;

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

/// Three user-scope facts in an in-memory store: the live side of the seam.
async fn seeded_store() -> Arc<InMemoryMemoryStore> {
    let store = Arc::new(InMemoryMemoryStore::new());
    for (key, confidence, created_ms) in [
        ("timezone", 0.9, 1_000),
        ("tone", 0.7, 2_000),
        ("units", 0.8, 3_000),
    ] {
        store
            .put(&seeded_record(
                MemoryKind::Fact,
                Some(key),
                confidence,
                created_ms,
            ))
            .await
            .unwrap();
    }
    store
}

fn user_facts_query() -> MemoryQuery {
    MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
        kinds: vec![MemoryKind::Fact],
        min_confidence: Some(0.5),
        ..MemoryQuery::default()
    }
}

#[tokio::test]
async fn journaled_read_records_request_and_assembly() {
    let store = seeded_store().await;
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let memory = journal.memory(MemorySource::Store(store));
    let budget = ContextBudget::new(1_000);
    let assembly = memory
        .read(&user_facts_query(), &budget, Some("memory-run-1:1".into()))
        .await
        .unwrap();

    // The filtered set, ranked by confidence-then-recency here (no
    // priorities): units (0.8), timezone (0.9)? — confidence descends
    // first, so timezone (0.9) leads.
    assert_eq!(assembly.records.len(), 3);
    assert_eq!(
        assembly.records[0].key.as_deref(),
        Some("timezone"),
        "rank: confidence descends before recency"
    );

    let events = journal.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.kind, RunEventKind::MemoryRead);
    assert_eq!(event.effect, Effect::ReadOnly);
    assert_eq!(event.parent.as_deref(), Some("memory-run-1:1"));

    // The journaled request is the resolved query (as_of stamped through
    // the run's logical clock) plus the budget.
    let Some(PayloadRef::Inline(input)) = &event.input else {
        panic!("read request travels inline")
    };
    let expected_as_of = ts(CLOCK_START_MS as i64); // first clock read
    assert_eq!(
        input["query"]["as_of"],
        serde_json::to_value(expected_as_of).unwrap()
    );
    assert_eq!(input["budget"]["max_tokens"].as_u64().unwrap(), 1_000);
    // The journaled output is the assembly itself — ids and order.
    let Some(PayloadRef::Inline(output)) = &event.output else {
        panic!("assembly travels inline")
    };
    assert_eq!(
        output["memory_ids"],
        serde_json::to_value(&assembly.memory_ids).unwrap()
    );
    assert_eq!(
        output["token_accounting"]["bytes_per_token"]
            .as_u64()
            .unwrap(),
        4
    );
}

#[tokio::test]
async fn equal_state_and_budget_give_byte_equal_assemblies() {
    // The determinism property the design requires, stated as a test: two
    // reads over equal store state under equal budgets produce byte-equal
    // assemblies (timestamps live on the events, not the assembly).
    let store_a = seeded_store().await;
    let store_b = seeded_store().await;
    let journal_a = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let journal_b = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let memory_a = journal_a.memory(MemorySource::Store(store_a));
    let memory_b = journal_b.memory(MemorySource::Store(store_b));
    let budget = ContextBudget::new(1_000);
    let a = memory_a
        .read(&user_facts_query(), &budget, None)
        .await
        .unwrap();
    let b = memory_b
        .read(&user_facts_query(), &budget, None)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&a).unwrap(),
        serde_json::to_vec(&b).unwrap()
    );
}

// ---------- wave-1 exit: replay-serving ----------

/// Build the one-node memory-reading graph. The node reads its causal
/// parent from `PARENT_EVENT_KEY`, performs the journaled read through the
/// captured handle, and writes the assembly's ids into state — so the run's
/// *behavior* depends on the served content, not just its evidence.
fn memory_graph(source: MemorySource, journal: &Journal) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("memory_ids", Reducer::Overwrite);
    let memory = journal.memory(source);
    let mut builder = GraphBuilder::new();
    builder.add_node("reader", move |ctx: NodeContext| {
        let memory = memory.clone();
        async move {
            let parent = ctx
                .config()
                .extra
                .get(PARENT_EVENT_KEY)
                .and_then(Value::as_str)
                .map(str::to_owned);
            let assembly = memory
                .read(&user_facts_query(), &ContextBudget::new(1_000), parent)
                .await?;
            Ok(NodeOutput::update("memory_ids", json!(assembly.memory_ids)))
        }
    });
    builder.set_entry_point("reader");
    (builder.compile().unwrap(), spec)
}

async fn record_memory_run() -> JournalSnapshot {
    let store = seeded_store().await;
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let (graph, spec) = memory_graph(MemorySource::Store(store), &journal);
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state
                    .get("memory_ids")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(3),
                "the recorded run's behavior consumed the assembly"
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
    journal.snapshot()
}

/// The wave-1 exit property, end to end: an exact replay of a
/// memory-reading run serves the journaled assembly byte-identically.
#[tokio::test]
async fn exact_replay_serves_the_journaled_assembly_byte_identically() {
    let snapshot = record_memory_run().await;
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|e| e.kind == RunEventKind::MemoryRead)
            .count(),
        1,
        "the recorded run journaled exactly one memory read"
    );

    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let source = MemoryReplaySource::new(replay.snapshot());
    let (graph, spec) = memory_graph(MemorySource::Replay(source.clone()), &journal);
    let outcome = replay
        .run_and_verify(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();

    // Every recorded memory read was served — the store was never queried
    // (the replay source is the only answer the run got).
    assert!(source.is_exhausted());

    // Byte-identical evidence (run_and_verify asserted structural equality;
    // the serialized bytes are the claim this wave makes).
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&outcome.journal).unwrap()
    );

    // The served assembly specifically: byte-identical output payloads on
    // the two MemoryRead events.
    let assembly_of = |snapshot: &JournalSnapshot| {
        snapshot
            .events
            .iter()
            .find(|e| e.kind == RunEventKind::MemoryRead)
            .map(|e| serde_json::to_vec(&e.output).unwrap())
            .unwrap()
    };
    assert_eq!(assembly_of(&snapshot), assembly_of(&outcome.journal));

    // And the replayed run's behavior consumed the same content.
    match &outcome.outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(
                state
                    .get("memory_ids")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(3)
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// The divergence half of the property: a replayed run asking a different
/// question fails loudly instead of improvising an answer.
#[tokio::test]
async fn replay_refuses_a_divergent_request() {
    let snapshot = record_memory_run().await;
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let source = MemoryReplaySource::new(&snapshot);
    let memory = journal.memory(MemorySource::Replay(source));
    // A different query (higher confidence floor) hashes differently.
    let divergent = MemoryQuery {
        min_confidence: Some(0.99),
        ..user_facts_query()
    };
    let error = memory
        .read(&divergent, &ContextBudget::new(1_000), None)
        .await
        .unwrap_err();
    assert!(matches!(error, RustyError::Replay(_)));
    assert!(error.to_string().contains("divergence"));
}

/// A live store cannot impersonate the journaled assembly: replaying
/// against an empty store produces different evidence, and verification
/// catches it.
#[tokio::test]
async fn replay_verification_catches_a_re_queried_store() {
    let snapshot = record_memory_run().await;
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    // The replayed run reads from a live (empty) store instead of the
    // journal — the request matches, the assembly does not.
    let (graph, spec) = memory_graph(
        MemorySource::Store(Arc::new(InMemoryMemoryStore::new())),
        &journal,
    );
    let outcome = replay
        .run(
            &graph,
            &spec,
            State::new(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();
    let error = replay.verify(&outcome.journal).unwrap_err();
    assert!(matches!(error, RustyError::Replay(_)));
    assert!(error.to_string().contains("event mismatch"));
}

// ---------- wave 2: consolidation, conflict detection, forgetting ----------

/// A distiller-authored record at user scope (confidence must be declared
/// for non-human authors).
fn distiller_record(
    kind: MemoryKind,
    key: Option<&str>,
    content: Value,
    confidence: f64,
    validity: ValidityWindow,
    created_ms: i64,
) -> MemoryRecord {
    let record = MemoryRecord::new(
        kind,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        MemoryProvenance {
            author: ProvenanceAuthor::Distiller {
                name: "test-distiller".into(),
            },
            evidence: MemoryEvidence::default(),
            written_at: ts(created_ms),
        },
        confidence,
        validity,
        ts(created_ms),
        content,
    )
    .unwrap();
    match key {
        Some(key) => record.with_key(key),
        None => record,
    }
}

#[test]
fn a_summary_supersedes_its_sources_in_default_retrieval() {
    let a = seeded_record(MemoryKind::Fact, Some("a"), 0.9, 1_000);
    let b = seeded_record(MemoryKind::Fact, Some("b"), 0.8, 2_000);
    let summary = consolidation_summary(
        ScopeAddress::new(MemoryScope::User, "user-7"),
        "consolidator-v1",
        &[a.clone(), b.clone()],
        json!({"distilled": "both facts"}),
        ts(3_000),
    )
    .unwrap();
    let universe = vec![a.clone(), b.clone(), summary.clone()];

    // Default retrieval serves the summary alone: consolidation supersedes
    // the records it distills (the source-naming half of the superseded
    // set). The sources stay queryable as evidence.
    let all = MemoryQuery::default();
    assert_eq!(
        apply_query(&universe, &all, ts(6_000)),
        vec![summary.clone()]
    );
    let with_evidence = MemoryQuery {
        include_superseded: true,
        ..MemoryQuery::default()
    };
    assert_eq!(apply_query(&universe, &with_evidence, ts(6_000)).len(), 3);
}

#[test]
fn consolidation_summary_carries_the_runtime_owned_invariants() {
    let strong = seeded_record(MemoryKind::Fact, Some("a"), 0.9, 1_000);
    let weak = seeded_record(MemoryKind::Fact, Some("b"), 0.4, 2_000);
    let summary = consolidation_summary(
        ScopeAddress::new(MemoryScope::Agent, "researcher-7"),
        "consolidator-v1",
        &[strong.clone(), weak.clone()],
        json!({"distilled": true}),
        ts(3_000),
    )
    .unwrap();

    assert_eq!(summary.kind, MemoryKind::Summary);
    assert_eq!(
        summary.provenance.author,
        ProvenanceAuthor::Distiller {
            name: "consolidator-v1".into()
        }
    );
    // The sources are named, sorted — the naming that supersedes them and
    // that dependent-summary invalidation walks on forgetting.
    let mut expected_ids = vec![strong.memory_id.clone(), weak.memory_id.clone()];
    expected_ids.sort();
    assert_eq!(summary.provenance.evidence.source_memory_ids, expected_ids);
    // Confidence is the minimum of the sources': a summary is no stronger
    // than its weakest source.
    assert_eq!(summary.confidence, 0.4);
    // Validity spans the sources; the seeded windows close at 10_000.
    assert_eq!(summary.validity.valid_from, ts(0));
    assert_eq!(summary.validity.valid_until, Some(ts(10_000)));

    // An open-ended source makes the summary open-ended.
    let open = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        MemoryProvenance {
            author: ProvenanceAuthor::System,
            evidence: MemoryEvidence::default(),
            written_at: ts(500),
        },
        1.0,
        ValidityWindow::starting(ts(0)),
        ts(500),
        json!({"open": true}),
    )
    .unwrap();
    let summary = consolidation_summary(
        ScopeAddress::new(MemoryScope::User, "user-7"),
        "consolidator-v1",
        &[strong, open],
        json!({"distilled": true}),
        ts(3_000),
    )
    .unwrap();
    assert_eq!(summary.validity.valid_until, None);

    // A summary that names no sources is not a consolidation.
    assert!(consolidation_summary(
        ScopeAddress::new(MemoryScope::User, "user-7"),
        "consolidator-v1",
        &[],
        json!({}),
        ts(3_000),
    )
    .is_err());
}

#[test]
fn conflict_detection_flags_and_never_resolves() {
    let window = ValidityWindow {
        valid_from: ts(0),
        valid_until: Some(ts(10_000)),
    };
    let one = distiller_record(
        MemoryKind::Fact,
        Some("timezone"),
        json!({"tz": "UTC+4"}),
        0.9,
        window.clone(),
        1_000,
    );
    let two = distiller_record(
        MemoryKind::Fact,
        Some("timezone"),
        json!({"tz": "UTC+1"}),
        0.8,
        window.clone(),
        2_000,
    );
    // Same key, disjoint windows: no overlap, no conflict.
    let later = distiller_record(
        MemoryKind::Fact,
        Some("timezone"),
        json!({"tz": "UTC+4"}),
        0.9,
        ValidityWindow {
            valid_from: ts(10_000),
            valid_until: None,
        },
        3_000,
    );
    // Same key, overlapping window, *equal* content: agreement, not
    // conflict.
    let agreeing = distiller_record(
        MemoryKind::Fact,
        Some("timezone"),
        json!({"tz": "UTC+4"}),
        0.7,
        window.clone(),
        4_000,
    );
    // Unrelated record sharing nothing.
    let other = distiller_record(
        MemoryKind::Fact,
        Some("tone"),
        json!({"tone": "brief"}),
        0.9,
        window.clone(),
        5_000,
    );
    let universe = vec![
        one.clone(),
        two.clone(),
        later.clone(),
        agreeing.clone(),
        other.clone(),
    ];
    let conflicts = detect_conflicts(&universe, ts(6_000));
    // Two contradictory pairs: `one` vs `two`, and `two` vs `agreeing`
    // (`one` and `agreeing` assert equal content; `later`'s window is
    // disjoint; `other` shares no key).
    let mut expected_pairs = vec![
        {
            let mut pair = vec![one.memory_id.clone(), two.memory_id.clone()];
            pair.sort();
            pair
        },
        {
            let mut pair = vec![two.memory_id.clone(), agreeing.memory_id.clone()];
            pair.sort();
            pair
        },
    ];
    expected_pairs.sort();
    let flagged: Vec<Vec<String>> = conflicts
        .iter()
        .map(|conflict| conflict.memory_ids.clone())
        .collect();
    assert_eq!(flagged, expected_pairs);
    assert!(conflicts.iter().all(|conflict| conflict.key == "timezone"));
    assert!(conflicts.iter().all(|conflict| conflict.overlap == window));

    // Supersession is disciplined replacement, not conflict: when `two`
    // supersedes `one`, that pair drops out of the flags (and `one`, now
    // superseded, leaves the live set entirely) — the `two` vs `agreeing`
    // contradiction stands.
    let two = two.with_supersedes(one.memory_id.clone());
    let universe = vec![one, two.clone(), later, agreeing.clone(), other];
    let conflicts = detect_conflicts(&universe, ts(6_000));
    let mut expected_pair = vec![two.memory_id.clone(), agreeing.memory_id.clone()];
    expected_pair.sort();
    assert_eq!(
        conflicts
            .iter()
            .map(|conflict| conflict.memory_ids.clone())
            .collect::<Vec<_>>(),
        vec![expected_pair]
    );
}

#[test]
fn plan_forget_walks_dependent_summaries_transitively() {
    let a = seeded_record(MemoryKind::Fact, Some("a"), 0.9, 1_000);
    let b = seeded_record(MemoryKind::Fact, Some("b"), 0.8, 2_000);
    let c = seeded_record(MemoryKind::Fact, Some("c"), 0.7, 3_000);
    let summary = consolidation_summary(
        ScopeAddress::new(MemoryScope::User, "user-7"),
        "consolidator-v1",
        &[a.clone(), b.clone()],
        json!({"ab": true}),
        ts(4_000),
    )
    .unwrap();
    // A summary of the summary: the transitive case.
    let meta = consolidation_summary(
        ScopeAddress::new(MemoryScope::User, "user-7"),
        "consolidator-v1",
        &[summary.clone(), c.clone()],
        json!({"abc": true}),
        ts(5_000),
    )
    .unwrap();
    let universe = vec![
        a.clone(),
        b.clone(),
        c.clone(),
        summary.clone(),
        meta.clone(),
    ];

    // Forgetting `a` invalidates the summary that named it and,
    // transitively, the summary that named that summary. `c` stands alone.
    let plan = plan_forget(&universe, std::slice::from_ref(&a.memory_id));
    assert_eq!(plan.forgotten, vec![a.memory_id.clone()]);
    let mut expected = vec![summary.memory_id.clone(), meta.memory_id.clone()];
    expected.sort();
    assert_eq!(plan.invalidated, expected);

    // Forgetting both sources of one summary invalidates it once.
    let plan = plan_forget(&universe, &[a.memory_id.clone(), b.memory_id.clone()]);
    let mut expected_forgotten = vec![a.memory_id.clone(), b.memory_id.clone()];
    expected_forgotten.sort();
    assert_eq!(plan.forgotten, expected_forgotten);
    assert_eq!(plan.invalidated, expected);

    // Absent targets are skipped — the caller decides whether absence is
    // an error.
    let plan = plan_forget(&universe, &["f".repeat(64)]);
    assert!(plan.forgotten.is_empty() && plan.invalidated.is_empty());
}

#[tokio::test]
async fn in_memory_store_removes_records() {
    let store = InMemoryMemoryStore::new();
    let record = seeded_record(MemoryKind::Fact, Some("removable"), 0.9, 1_000);
    assert!(store.put(&record).await.unwrap());
    assert!(store.remove(&record.memory_id).await.unwrap());
    assert!(!store.remove(&record.memory_id).await.unwrap());
    assert!(store.get(&record.memory_id).await.unwrap().is_none());
}

#[test]
fn memory_forget_effect_key_is_the_derived_form() {
    assert_eq!(
        memory_forget_effect_key(&ScopeAddress::new(MemoryScope::User, "user-7"), "m-1"),
        "memory_forget:user:user-7:m-1"
    );
}
