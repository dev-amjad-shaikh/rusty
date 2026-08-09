//! Learning-candidate integration tests (R0.8 Rusty Learn, wave 3).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of `Candidate` (a
//!   `memory_set` and a `prompt` specimen), `PromotionEnvelope`,
//!   `CandidateEvaluation`, `PromotionReceipt`, `RollbackReceipt`, and
//!   `VersionPointer` are pinned against checked-in JSON under
//!   `tests/golden/`. Any accidental contract drift fails here. To bless
//!   an intentional contract change, re-run with `UPDATE_GOLDEN=1` and
//!   review the diff. The four new `RunEventKind` variants' wire names
//!   are pinned in `learn_event_kinds.json` (the `memory_event_kinds.json`
//!   pattern; the exhaustive `run_event_kind.json` list is owned by
//!   `tests/agents.rs`, outside this stream's file scope).
//! - **The evaluation seam, end to end with scripted nodes** — a
//!   scripted graph reads memory through the journaled seam; the
//!   scripted evaluator runs baseline and candidate (applied via
//!   `CandidateOverlay`) through the real executor and produces the
//!   journaled payload: replay summary, report pair, dataset version,
//!   verdict. No LLMs — this wave proves the machinery, wave 4 drives it
//!   with `rusty-eval`.
//! - **The journaled lifecycle** — created → evaluated → promoted →
//!   rolled back, journaled into one run's journal with causal
//!   parentage: every transition is in the journal (the wave's third
//!   exit criterion, at the core level).
//! - **Byte-exact rollback at the contract level** — promote A then B,
//!   roll back, and the re-pointed candidate's serialization is
//!   byte-identical to A's: content addressing makes the restored
//!   version the version that served, not a reconstruction.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::GraphBuilder;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, PARENT_EVENT_KEY};
use rusty_agent_runtime::learn::{
    admit_promotion, candidate_effect_key, evaluation_effect_key, promotion_effect_key,
    rollback_effect_key, AutoPromotion, CanaryBinding, Candidate, CandidateContent,
    CandidateEvaluation, CandidateEvaluator, CandidateId, CandidateOverlay, CandidateRecord,
    CandidateStatus, EnvelopeRule, EvaluationRequest, EvaluationThresholds, EvaluationVerdict,
    EvidenceSpan, GrantDirection, LearnError, PromotionAuthority, PromotionDecision,
    PromotionEnvelope, PromotionReceipt, ReplaySummary, RollbackReceipt, VersionPointer,
};
use rusty_agent_runtime::memory::{
    ContextBudget, InMemoryMemoryStore, MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord,
    MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor, ScopeAddress, ValidityWindow,
};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{Effect, RunEventKind, RunManifest};
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

fn distiller() -> ProvenanceAuthor {
    ProvenanceAuthor::Distiller {
        name: "correction-loop".into(),
    }
}

fn record(content: Value) -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "support-1"),
        MemoryProvenance {
            author: ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            evidence: rusty_agent_runtime::memory::MemoryEvidence {
                correction_id: Some("correction-9".into()),
                ..Default::default()
            },
            written_at: ts(1_750_000_001_000),
        },
        1.0,
        ValidityWindow::starting(ts(1_750_000_000_000)),
        ts(1_750_000_001_000),
        content,
    )
    .unwrap()
    .with_key("tone")
    .with_candidacy(rusty_agent_runtime::memory::Candidacy::Pending)
}

fn memory_set_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::MemorySet {
            scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
            adds: vec![record(json!({"tone": "warm", "greeting": "hello"}))],
            supersedes: vec!["a".repeat(64)],
        },
        distiller(),
        EvidenceSpan {
            run_ids: vec!["run-abc".into()],
            correction_ids: vec!["correction-9".into()],
            memory_ids: vec!["b".repeat(64)],
        },
        ts(1_750_000_002_000),
    )
    .unwrap()
}

fn prompt_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::Prompt {
            name: "system".into(),
            prompt: "You are a careful support agent. Answer tersely.".into(),
        },
        distiller(),
        EvidenceSpan {
            run_ids: vec!["run-abc".into()],
            ..EvidenceSpan::default()
        },
        ts(1_750_000_002_000),
    )
    .unwrap()
}

fn evaluation(candidate: &Candidate) -> CandidateEvaluation {
    CandidateEvaluation {
        candidate_id: candidate.candidate_id.clone(),
        dataset_version: "support-v3".into(),
        replay: ReplaySummary {
            fixture_ids: vec!["run-abc".into()],
            matched: 1,
            divergences: Vec::new(),
        },
        baseline_report: json!({
            "format_version": 1,
            "name": "support@support-v3",
            "dataset_version": "support-v3",
            "summary": {"run_pass_rate": 0.5},
        }),
        candidate_report: json!({
            "format_version": 1,
            "name": "support@support-v3",
            "dataset_version": "support-v3",
            "summary": {"run_pass_rate": 1.0},
        }),
        verdict: EvaluationVerdict {
            regressed: false,
            target_metric: "run_pass_rate".into(),
            baseline: Some(0.5),
            candidate: Some(1.0),
            delta: Some(0.5),
        },
        thresholds: EvaluationThresholds::default(),
        evaluated_by: distiller(),
        evaluated_at: ts(1_750_000_003_000),
    }
}

fn promotion_receipt(candidate: &Candidate) -> PromotionReceipt {
    PromotionReceipt {
        candidate_id: candidate.candidate_id.clone(),
        surface: candidate.surface(),
        previous: Some(CandidateId::from("c".repeat(64))),
        decision: PromotionDecision {
            authority: PromotionAuthority::Envelope {
                envelope_version: "r0.8-default".into(),
            },
            canary: None,
        },
        promoted_at: ts(1_750_000_004_000),
    }
}

// ---------- golden files ----------

#[test]
fn golden_candidate_memory_set_shape() {
    assert_golden("candidate.json", &memory_set_candidate());
}

#[test]
fn golden_candidate_prompt_shape() {
    assert_golden("candidate_prompt.json", &prompt_candidate());
}

#[test]
fn golden_promotion_envelope_shape() {
    // Every rule mode exercised: auto with a named dataset version and
    // scope restriction, plain approval, and a canary.
    let envelope = PromotionEnvelope {
        envelope_version: "acme-2026-03".into(),
        prompt: EnvelopeRule::Approval,
        policy: EnvelopeRule::Canary {
            fraction: 0.1,
            auto: AutoPromotion {
                dataset_version: Some("retry-v2".into()),
                min_improvement: 0.02,
                scopes: Vec::new(),
            },
        },
        memory_set: EnvelopeRule::Auto(AutoPromotion {
            dataset_version: Some("support-v3".into()),
            min_improvement: 0.0,
            scopes: vec![MemoryScope::Run, MemoryScope::Agent],
        }),
        tool_permission: EnvelopeRule::Approval,
    };
    assert_golden("promotion_envelope.json", &envelope);
}

#[test]
fn golden_candidate_evaluation_shape() {
    assert_golden(
        "candidate_evaluation.json",
        &evaluation(&memory_set_candidate()),
    );
}

#[test]
fn golden_promotion_receipt_shape() {
    assert_golden(
        "promotion_receipt.json",
        &promotion_receipt(&memory_set_candidate()),
    );
}

#[test]
fn golden_rollback_receipt_shape() {
    let candidate = memory_set_candidate();
    assert_golden(
        "rollback_receipt.json",
        &RollbackReceipt {
            surface: candidate.surface(),
            from: candidate.candidate_id.clone(),
            to: Some(CandidateId::from("c".repeat(64))),
            cause: "drift monitor: pass-rate drop on support@v3 (runs run-201..run-210)".into(),
            rolled_back_at: ts(1_750_000_005_000),
        },
    );
}

#[test]
fn golden_version_pointer_shape() {
    let candidate = memory_set_candidate();
    let pointer = VersionPointer {
        surface: candidate.surface(),
        active: Some(candidate.candidate_id.clone()),
        canary: Some(CanaryBinding {
            candidate_id: CandidateId::from("d".repeat(64)),
            fraction: 0.1,
        }),
    };
    assert_golden("version_pointer.json", &pointer);
}

#[test]
fn golden_learn_event_kinds_shape() {
    // The additive wave-3 variants' wire names, in declaration order (they
    // append after `memory_forget` — the same additive evolution rule every
    // variant since R0.6's `effect_receipt` followed). The exhaustive
    // `run_event_kind.json` list is owned by `tests/agents.rs` (outside this
    // stream's file scope); the names are pinned here so no wire shape
    // lands unpinned.
    assert_golden(
        "learn_event_kinds.json",
        &vec![
            RunEventKind::CandidateCreated,
            RunEventKind::CandidateEvaluated,
            RunEventKind::CandidatePromoted,
            RunEventKind::CandidateRolledBack,
        ],
    );
}

// ---------- contract behavior ----------

#[test]
fn content_address_converges_and_tampering_fails() {
    let a = memory_set_candidate();
    let b = memory_set_candidate();
    assert_eq!(
        a.candidate_id, b.candidate_id,
        "two distillations of the same change converge on one id"
    );
    a.verify_address().unwrap();
    let mut tampered = a.clone();
    tampered.candidate_id = CandidateId::from("e".repeat(64));
    assert!(matches!(
        tampered.verify_address(),
        Err(LearnError::AddressMismatch { .. })
    ));
}

#[test]
fn prompt_candidate_applies_to_the_manifest_pin() {
    let candidate = prompt_candidate();
    let baseline = RunManifest::new().pin_prompt("system", "You are terse.");
    let mut applied = baseline.clone();
    candidate.apply_to_manifest(&mut applied).unwrap();
    // The substituted pin is exactly what `pin_prompt` would record for
    // the candidate's text — the candidate and the manifest speak one
    // content address.
    let repinned =
        RunManifest::new().pin_prompt("system", "You are a careful support agent. Answer tersely.");
    assert_eq!(applied, repinned);
    assert_ne!(applied, baseline);
    // A non-prompt candidate refuses the manifest surface.
    assert!(memory_set_candidate()
        .apply_to_manifest(&mut applied)
        .is_err());
}

#[test]
fn tool_permission_content_is_typed_and_surfaced() {
    let candidate = Candidate::new(
        CandidateContent::ToolPermission {
            tool: "shell".into(),
            direction: GrantDirection::Narrow,
        },
        distiller(),
        EvidenceSpan::default(),
        ts(1_750_000_002_000),
    )
    .unwrap();
    assert_eq!(candidate.surface().as_str(), "tool:shell");
    assert!(candidate.verify_address().is_ok());
}

// ---------- the evaluation seam, end to end with scripted nodes ----------

/// A scripted evaluator: builds the recital graph over the base store
/// (baseline) and over the candidate overlay (candidate), drives both
/// through the real executor with the journaled memory seam, and grades
/// pass/fail on the recited answer. Reports mimic the `ExperimentReport`
/// summary shape the release proof will emit through `rusty-eval`; the
/// verdict computes the target metric's delta the way `compare()` will.
#[derive(Debug)]
struct ScriptedEvaluator {
    store: Arc<InMemoryMemoryStore>,
    expected: Value,
}

/// The scripted proof graph: one node reads the `tone` fact at agent
/// scope through the journaled memory seam and answers with it. The
/// behavior under test is entirely memory-driven, so a `memory_set`
/// candidate changes what the run does with no code change — which is
/// the point of the pipeline.
async fn run_recital(store: Arc<dyn MemoryStore>, run_id: &str) -> RuntimeResult<Value> {
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);
    let journal = Journal::new(run_id, "eval-thread", Clock::System);
    let memory = journal.memory(MemorySource::Store(store));
    let mut builder = GraphBuilder::new();
    builder.add_node("recite", move |ctx: NodeContext| {
        let memory = memory.clone();
        async move {
            let parent = ctx
                .config()
                .extra
                .get(PARENT_EVENT_KEY)
                .and_then(|v| v.as_str().map(str::to_owned));
            let query = MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::Agent, "support-1")),
                key: Some("tone".into()),
                ..MemoryQuery::default()
            };
            let assembly = memory
                .read(&query, &ContextBudget::new(4096), parent)
                .await?;
            let answer = assembly
                .records
                .first()
                .map(|r| match &r.content {
                    rusty_agent_runtime::record::PayloadRef::Inline(v) => v.clone(),
                    other => json!({ "unexpected": format!("{other:?}") }),
                })
                .unwrap_or(Value::Null);
            Ok(NodeOutput::update("answer", answer))
        }
    });
    builder.set_entry_point("recite");
    let graph = builder.compile()?;
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("eval-thread").with_journal(journal.clone()),
        )
        .await?;
    match outcome {
        ExecutionOutcome::Done(state) => Ok(state.to_value()["answer"].clone()),
        other => Err(rusty_agent_runtime::error::RustyError::Graph(format!(
            "scripted evaluation run did not complete: {other:?}"
        ))),
    }
}

#[async_trait::async_trait]
impl CandidateEvaluator for ScriptedEvaluator {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        request: &EvaluationRequest,
    ) -> RuntimeResult<CandidateEvaluation> {
        let baseline = run_recital(self.store.clone(), "eval:baseline").await?;
        let overlay: Arc<dyn MemoryStore> =
            Arc::new(CandidateOverlay::new(self.store.clone(), candidate)?);
        let applied = run_recital(overlay, "eval:candidate").await?;

        let baseline_pass = baseline == self.expected;
        let candidate_pass = applied == self.expected;
        let (baseline_rate, candidate_rate) = (
            if baseline_pass { 1.0 } else { 0.0 },
            if candidate_pass { 1.0 } else { 0.0 },
        );
        let report = |rate: f64| {
            json!({
                "format_version": 1,
                "name": format!("scripted@{}", request.dataset_version),
                "dataset_version": request.dataset_version,
                "summary": {"run_pass_rate": rate},
            })
        };
        Ok(CandidateEvaluation {
            candidate_id: candidate.candidate_id.clone(),
            dataset_version: request.dataset_version.clone(),
            replay: ReplaySummary {
                fixture_ids: request
                    .replay_evidence
                    .iter()
                    .map(|snapshot| snapshot.run_id.clone())
                    .collect(),
                matched: request.replay_evidence.len(),
                divergences: Vec::new(),
            },
            baseline_report: report(baseline_rate),
            candidate_report: report(candidate_rate),
            verdict: EvaluationVerdict {
                regressed: candidate_rate < baseline_rate,
                target_metric: request.target_metric.clone(),
                baseline: Some(baseline_rate),
                candidate: Some(candidate_rate),
                delta: Some(candidate_rate - baseline_rate),
            },
            thresholds: request.thresholds,
            evaluated_by: distiller(),
            evaluated_at: ts(1_750_000_003_000),
        })
    }
}

#[tokio::test]
async fn scripted_evaluation_composes_through_the_real_executor() {
    let store = Arc::new(InMemoryMemoryStore::new());
    // The defect: the agent recites a flat tone. The correction-derived
    // candidate teaches the warm tone — the release proof's shape, with
    // scripted nodes standing in for the LLM.
    let flat = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, "support-1"),
        MemoryProvenance {
            author: ProvenanceAuthor::System,
            evidence: Default::default(),
            written_at: ts(1_000),
        },
        1.0,
        ValidityWindow::starting(ts(500)),
        ts(1_000),
        json!({"tone": "flat"}),
    )
    .unwrap()
    .with_key("tone");
    store.put(&flat).await.unwrap();

    let warm = record(json!({"tone": "warm"}));
    let candidate = Candidate::new(
        CandidateContent::MemorySet {
            scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
            adds: vec![warm],
            supersedes: Vec::new(),
        },
        distiller(),
        EvidenceSpan::default(),
        ts(1_750_000_002_000),
    )
    .unwrap();

    let evaluator = ScriptedEvaluator {
        store: store.clone(),
        expected: json!({"tone": "warm"}),
    };
    let request = EvaluationRequest {
        dataset_version: "support-v3".into(),
        target_metric: "run_pass_rate".into(),
        thresholds: EvaluationThresholds::default(),
        replay_evidence: Vec::new(),
    };
    let evaluation = evaluator.evaluate(&candidate, &request).await.unwrap();

    // The candidate applied through the overlay passes where the baseline
    // fails; the verdict shows improvement with no regression.
    assert_eq!(evaluation.verdict.baseline, Some(0.0));
    assert_eq!(evaluation.verdict.candidate, Some(1.0));
    assert_eq!(evaluation.verdict.delta, Some(1.0));
    assert!(!evaluation.verdict.regressed);
    assert!(evaluation.replay.is_clean());

    // And the verdict clears the R0.8 default envelope — memory_set at
    // agent scope with a clean comparison auto-promotes.
    let decision = admit_promotion(
        &PromotionEnvelope::r08_default(),
        &candidate,
        Some(&evaluation),
        None,
    )
    .unwrap();
    assert_eq!(
        decision.authority,
        PromotionAuthority::Envelope {
            envelope_version: "r0.8-default".into()
        }
    );
}

// ---------- the journaled lifecycle + byte-exact rollback ----------

#[test]
fn lifecycle_transitions_journal_with_causal_parentage() {
    // The wave's third exit criterion at the core level: every transition
    // is in the journal, parented to the transition that caused it.
    let journal = Journal::new("learn-run", "learn-thread", Clock::System);
    let candidate = memory_set_candidate();

    let created = journal.record(
        EventDraft::new(RunEventKind::CandidateCreated, Effect::Idempotent)
            .input(json!({
                "effect_key": candidate_effect_key(&candidate.candidate_id),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(&candidate).unwrap()),
    );
    let evaluated = journal.record(
        EventDraft::new(RunEventKind::CandidateEvaluated, Effect::Idempotent)
            .input(json!({
                "effect_key": evaluation_effect_key(&candidate.candidate_id, "support-v3"),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(evaluation(&candidate)).unwrap())
            .parent(created.clone()),
    );
    let promoted = journal.record(
        EventDraft::new(RunEventKind::CandidatePromoted, Effect::Idempotent)
            .input(json!({
                "effect_key": promotion_effect_key(&candidate.candidate_id),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(promotion_receipt(&candidate)).unwrap())
            .parent(evaluated.clone()),
    );
    let rolled_back = journal.record(
        EventDraft::new(RunEventKind::CandidateRolledBack, Effect::Idempotent)
            .input(json!({
                "effect_key": rollback_effect_key(&candidate.surface(), &candidate.candidate_id),
                "candidate_id": candidate.candidate_id,
            }))
            .output(
                serde_json::to_value(RollbackReceipt {
                    surface: candidate.surface(),
                    from: candidate.candidate_id.clone(),
                    to: promotion_receipt(&candidate).previous.clone(),
                    cause: "drift monitor: pass-rate drop on support@v3".into(),
                    rolled_back_at: ts(1_750_000_005_000),
                })
                .unwrap(),
            )
            .parent(promoted.clone()),
    );

    let events = journal.events();
    let kinds: Vec<RunEventKind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RunEventKind::CandidateCreated,
            RunEventKind::CandidateEvaluated,
            RunEventKind::CandidatePromoted,
            RunEventKind::CandidateRolledBack,
        ]
    );
    assert_eq!(events[1].parent.as_deref(), Some(created.as_str()));
    assert_eq!(events[2].parent.as_deref(), Some(evaluated.as_str()));
    assert_eq!(events[3].parent.as_deref(), Some(promoted.as_str()));
    assert_eq!(events[3].id, rolled_back);
    // The head hash binds the lifecycle: a tampered snapshot fails to load.
    let mut snapshot = journal.snapshot();
    snapshot.events[2].status = rusty_agent_runtime::record::EventStatus::Error;
    assert!(Journal::from_snapshot(snapshot, Clock::System).is_err());
}

#[test]
fn rollback_restores_the_prior_candidate_byte_exactly() {
    // The wave's second exit criterion at the contract level: promote A →
    // B, roll back, re-resolve, and the restored version is byte-identical
    // to A — candidates are content-addressed and immutable, so the
    // pointer's `previous` IS the version that served.
    let a = memory_set_candidate();
    let b = Candidate::new(
        CandidateContent::MemorySet {
            scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
            adds: vec![record(json!({"tone": "colder", "greeting": "hi"}))],
            supersedes: vec!["a".repeat(64)],
        },
        distiller(),
        EvidenceSpan {
            run_ids: vec!["run-def".into()],
            correction_ids: vec!["correction-10".into()],
            memory_ids: Vec::new(),
        },
        ts(1_750_000_003_500),
    )
    .unwrap();
    assert_ne!(
        a.candidate_id, b.candidate_id,
        "distinct changes, distinct ids"
    );

    // The candidate store the pointer resolves against: id → bytes,
    // written once at creation and never mutated (candidates are
    // immutable; a changed candidate is a new id).
    let mut store = std::collections::HashMap::new();
    store.insert(a.candidate_id.to_string(), serde_json::to_vec(&a).unwrap());
    store.insert(b.candidate_id.to_string(), serde_json::to_vec(&b).unwrap());

    let promote_a = PromotionReceipt {
        candidate_id: a.candidate_id.clone(),
        surface: a.surface(),
        previous: None,
        decision: PromotionDecision {
            authority: PromotionAuthority::Envelope {
                envelope_version: "r0.8-default".into(),
            },
            canary: None,
        },
        promoted_at: ts(1_750_000_004_000),
    };
    let promote_b = PromotionReceipt {
        candidate_id: b.candidate_id.clone(),
        surface: a.surface(),
        previous: Some(a.candidate_id.clone()),
        decision: promote_a.decision.clone(),
        promoted_at: ts(1_750_000_005_000),
    };
    let pointer = VersionPointer::new(a.surface())
        .promoted(&promote_a)
        .promoted(&promote_b);
    assert_eq!(pointer.active, Some(b.candidate_id.clone()));

    let rolled = pointer.rolled_back(&RollbackReceipt {
        surface: a.surface(),
        from: b.candidate_id.clone(),
        to: promote_b.previous.clone(),
        cause: "drift monitor: pass-rate drop on support@v3".into(),
        rolled_back_at: ts(1_750_000_006_000),
    });
    let restored_id = rolled.active.expect("rollback re-points to A");
    assert_eq!(restored_id, a.candidate_id);

    // Re-resolution by id returns the exact bytes that served before B:
    // a freshly distilled instance of A's content serializes identically
    // (content addressing, not reconstruction), and the stored bytes
    // still verify against their address.
    let redistilled = memory_set_candidate();
    assert_eq!(redistilled.candidate_id, restored_id);
    let restored_bytes = store
        .get(restored_id.as_str())
        .expect("the restored id resolves in the store");
    assert_eq!(*restored_bytes, serde_json::to_vec(&redistilled).unwrap());
    let restored: Candidate = serde_json::from_slice(restored_bytes).unwrap();
    restored.verify_address().unwrap();

    // And the lifecycle record of A agrees it is promotable again (its
    // state machine sits at `promoted` until rolled back itself).
    let mut record_a = CandidateRecord::new(a.clone());
    record_a.apply_evaluation(evaluation(&a)).unwrap();
    record_a.apply_promotion(promote_a).unwrap();
    assert_eq!(record_a.status, CandidateStatus::Promoted);
}

#[test]
fn candidate_serde_roundtrip_is_shape_stable() {
    let candidate = memory_set_candidate();
    let wire = serde_json::to_string(&candidate).unwrap();
    let back: Candidate = serde_json::from_str(&wire).unwrap();
    assert_eq!(candidate, back);
    // Content-addressed: the round-tripped candidate re-derives its id.
    back.verify_address().unwrap();
}
