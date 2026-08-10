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

use rusty_agent_runtime::durable::{retry_decision_event, ErrorClass, RetryDecision};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::GraphBuilder;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot, PARENT_EVENT_KEY};
use rusty_agent_runtime::learn::{
    admit_promotion, candidate_effect_key, detect_policy_drift, distill_retry_parameters,
    distill_timeout_parameters, evaluation_effect_key, promotion_effect_id, promotion_effect_key,
    rollback_effect_key, AutoPromotion, CanaryBinding, Candidate, CandidateContent,
    CandidateEvaluation, CandidateEvaluator, CandidateId, CandidateOverlay, CandidateRecord,
    CandidateStatus, DriftBaseline, DriftThresholds, EnvelopeRule, EvaluationRequest,
    EvaluationThresholds, EvaluationVerdict, EvidenceSpan, GrantDirection, LearnError,
    PromotionAuthority, PromotionDecision, PromotionEnvelope, PromotionReceipt, PromotionRefusal,
    ReplaySummary, RetryLearningConfig, RollbackReceipt, TimeoutLearningConfig,
    TwinCandidateEvaluator, VersionPointer,
};
use rusty_agent_runtime::memory::{
    ContextBudget, InMemoryMemoryStore, MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord,
    MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor, ScopeAddress, ValidityWindow,
};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{
    derive_policy_version, DecisionEvent, DecisionFamily, DecisionOutcome, DecisionRole, Effect,
    EventStatus, ExecutorPolicy, PolicyVersion, RunEventKind, RunManifest,
};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::twin::{FaultAnchor, FaultSchedule, InjectedFault};

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

// --------------------------------------------------------------------- //
// R0.10 wave 3: the learned retry/timeout families, end to end
//
// Three test groups:
//
// - **The distillers** — the retry learner's margin gate (the Wave 1
//   scar: a per-class table that does not beat the floor earns nothing),
//   the permanent-failure stance, and the timeout learner's empirical
//   abort-fraction fit with abstention on heavy tails (the second Wave 1
//   scar). Sparse evidence produces no candidate, never a confident one.
// - **The twin gate** — `TwinCandidateEvaluator` replays the evidence
//   span's fixtures head-to-head, floor against candidate on identical
//   seeds and fault schedules. A winning candidate clears the Auto
//   envelope's evidence bar; a candidate that truncates completions
//   regresses and the gate refuses it mechanically. Under the R0.8
//   default envelope the policy family's bar is the human's: no token,
//   no promotion — a scoped token admits, carrying the reviewer's name.
// - **Drift detection** — the acting version's journaled outcomes
//   against the promotion-time twin baseline: completion drops,
//   dead-letter growth, and p95 latency growth declare drift with
//   attributable reasons; sparse evidence declares nothing; shadow
//   decisions are the next candidate's evidence, never the acting
//   version's health.
// --------------------------------------------------------------------- //

/// One journaled retry decision for the learner and drift suites, built
/// through the family's own emission contract and then given a terminal
/// outcome when the scenario needs one (a `Retry` selection journals
/// `outcome: None` — the re-attempt has not happened yet).
#[allow(clippy::too_many_arguments)]
fn journaled_retry(
    seq: u64,
    class: ErrorClass,
    attempt: u32,
    max_attempts: u32,
    decision: &RetryDecision,
    outcome: Option<DecisionOutcome>,
    dependency_latency_ms: Option<u64>,
    policy_version: &PolicyVersion,
) -> DecisionEvent {
    let mut event = retry_decision_event(
        "run-evidence",
        "thread-evidence",
        seq,
        Effect::Idempotent,
        class,
        attempt,
        max_attempts,
        dependency_latency_ms,
        decision,
        policy_version,
        ts(1_750_000_010_000 + seq as i64 * 1_000),
    );
    event.outcome = outcome;
    event
}

/// `count` retry decisions for `class`, `successes` of them journaled
/// with a success outcome (the retry recovered the call).
fn retry_evidence(
    class: ErrorClass,
    retries: u64,
    successes: u64,
    failures: u64,
) -> Vec<DecisionEvent> {
    let mut events = Vec::new();
    let mut seq = 1u64;
    for i in 0..retries {
        let outcome = if i < successes {
            Some(DecisionOutcome::Success)
        } else {
            None
        };
        events.push(journaled_retry(
            seq,
            class,
            1,
            3,
            &RetryDecision::Retry { after_ms: 500 },
            outcome,
            None,
            &PolicyVersion::default(),
        ));
        seq += 1;
    }
    for _ in 0..failures {
        events.push(journaled_retry(
            seq,
            class,
            3,
            3,
            &RetryDecision::Dead,
            Some(DecisionOutcome::Failure),
            None,
            &PolicyVersion::default(),
        ));
        seq += 1;
    }
    events
}

/// A journaled run of tool completions for the timeout learner: `callee`
/// completed `latencies.len()` times with the given latencies.
fn completion_events(callee: &str, latencies: &[u64], start_seq: u64) -> JournalSnapshot {
    let journal = Journal::new(
        "run-latencies",
        "thread-latencies",
        Clock::logical(1_700_000_000_000, 10),
    );
    let step = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": [callee]})),
    );
    for (i, latency) in latencies.iter().enumerate() {
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
                .node(callee)
                .input(json!({"tool": callee, "arguments": {"i": i}}))
                .output(json!({"result": "ok"}))
                .latency_ms(*latency)
                .status(EventStatus::Ok)
                .parent(&step),
        );
    }
    let _ = start_seq;
    journal.snapshot()
}

/// The twin-gate winning fixture: one recorded 100 ms completion the
/// fault schedule rate-limits on its first two attempts. The recorded
/// world answers attempt three; the arms differ only in what the wait
/// cost — the wall-time win a shorter backoff earns at identical
/// completion.
fn rate_limited_snapshot() -> JournalSnapshot {
    let journal = Journal::new(
        "run-rate-limited",
        "thread-rate-limited",
        Clock::logical(1_700_000_000_000, 10),
    );
    let step = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": ["search"]})),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("search")
            .input(json!({"tool": "search", "arguments": {}}))
            .output(json!({"result": "ok"}))
            .latency_ms(100)
            .cost_usd(0.001)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
            .output(json!({"done": true}))
            .parent(&step),
    );
    journal.snapshot()
}

/// The twin-gate losing fixture: a flaky call (recorded error, re-served
/// on every attempt), a slow 10 s completion (the timeout family's
/// target), and a fast one.
fn slow_call_snapshot() -> JournalSnapshot {
    let journal = Journal::new(
        "run-slow-call",
        "thread-slow-call",
        Clock::logical(1_700_000_000_000, 10),
    );
    let step = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": ["flaky", "slow", "fast"]})),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("flaky")
            .input(json!({"tool": "flaky", "arguments": {}}))
            .output(json!({"error": "connection reset"}))
            .latency_ms(100)
            .status(EventStatus::Error)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("slow")
            .input(json!({"tool": "slow", "arguments": {}}))
            .output(json!({"result": "ok"}))
            .latency_ms(10_000)
            .cost_usd(0.002)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::Idempotent)
            .node("fast")
            .input(json!({"tool": "fast", "arguments": {}}))
            .output(json!({"result": "ok"}))
            .latency_ms(50)
            .parent(&step),
    );
    journal.record(
        EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
            .output(json!({"done": true}))
            .parent(&step),
    );
    journal.snapshot()
}

/// A `policy` candidate carrying `parameters` for `family`.
fn policy_candidate(family: DecisionFamily, parameters: Value) -> Candidate {
    Candidate::new(
        CandidateContent::Policy { family, parameters },
        distiller(),
        EvidenceSpan {
            run_ids: vec!["run-evidence".into()],
            ..EvidenceSpan::default()
        },
        ts(1_750_000_020_000),
    )
    .unwrap()
}

/// An envelope whose policy family auto-promotes past the evidence bar —
/// the mechanical gate the twin evaluation feeds. Every other family
/// keeps the approval rule.
fn auto_policy_envelope() -> PromotionEnvelope {
    PromotionEnvelope {
        envelope_version: "test-auto".into(),
        prompt: EnvelopeRule::Approval,
        policy: EnvelopeRule::Auto(AutoPromotion {
            dataset_version: None,
            min_improvement: 0.0,
            scopes: Vec::new(),
        }),
        memory_set: EnvelopeRule::Approval,
        tool_permission: EnvelopeRule::Approval,
    }
}

// ---------- the distillers ----------

#[test]
fn retry_learner_emits_a_per_class_entry_when_the_margin_clears() {
    // A class recovering on every other retry (p = 0.5): the floor's
    // schedule pays the full backoff ladder and charges the dead-letter
    // tail; a shorter base with a wider budget clears the 2 s margin.
    let events = retry_evidence(ErrorClass::Timeout, 10, 5, 0);
    let params = distill_retry_parameters(&events, &RetryLearningConfig::default());

    // The flat schedule stays the floor's — only the class the evidence
    // spoke for earns an entry.
    assert_eq!(params.base_delay_ms, 1_000);
    assert_eq!(params.max_delay_ms, 300_000);
    assert_eq!(params.max_attempts, 3);
    let entry = params
        .per_class
        .as_ref()
        .and_then(|table| table.get(&ErrorClass::Timeout))
        .expect("p = 0.5 clears the margin");
    assert_eq!(entry.base_delay_ms, 100, "the grid's shortest base wins");
    assert_eq!(
        entry.max_attempts, 5,
        "a wider budget buys back the dead-letter tail"
    );
}

#[test]
fn retry_learner_abstains_when_the_floor_is_already_optimal() {
    // p = 1.0: the first retry always recovers, so the whole decision is
    // the first draw's mean — the floor's 500 ms against the grid's best
    // 50 ms, a 450 ms fit that does not clear the 2 s margin. The Wave 1
    // scar is exactly this: a marginal per-class table demoting the
    // floor's jittered exponential. No margin, no entry.
    let events = retry_evidence(ErrorClass::Timeout, 10, 10, 0);
    let params = distill_retry_parameters(&events, &RetryLearningConfig::default());
    assert!(
        params.per_class.is_none(),
        "a fit inside the margin earns nothing: {params:?}"
    );
}

#[test]
fn retry_learner_marks_permanent_failure_classes_for_early_abort() {
    // Four retries observed, four terminal failures, not one success: the
    // permanent-failure stance — abort after the first failure — on the
    // floor's schedule. The evidence is the terminal outcomes, never the
    // absence of successes alone.
    let events = retry_evidence(ErrorClass::Unknown, 4, 0, 4);
    let params = distill_retry_parameters(&events, &RetryLearningConfig::default());
    let entry = params
        .per_class
        .as_ref()
        .and_then(|table| table.get(&ErrorClass::Unknown))
        .expect("terminal failures are evidence");
    assert_eq!(entry.max_attempts, 1, "abort after the first failure");
    assert_eq!(entry.base_delay_ms, 1_000, "the floor's schedule stands");
    assert_eq!(entry.max_delay_ms, 300_000);
}

#[test]
fn retry_learner_abstains_on_sparse_evidence() {
    let events = retry_evidence(ErrorClass::Timeout, 2, 2, 0);
    let params = distill_retry_parameters(&events, &RetryLearningConfig::default());
    assert!(
        params.per_class.is_none(),
        "two observations produce no confident policy"
    );
}

#[test]
fn timeout_learner_fits_the_smallest_rung_within_the_abort_tolerance() {
    // `search`: 19 completions at 800 ms and one at 4.5 s. The 1 s rung
    // would prematurely abort 5% of completions — over the 1% tolerance;
    // the 5 s rung aborts none. `embed` has ten completions — under the
    // 16-sample bar, no entry.
    let mut latencies = vec![800u64; 19];
    latencies.push(4_500);
    let snapshot = completion_events("search", &latencies, 0);
    let embed = completion_events("embed", &[100u64; 10], 0);
    let events: Vec<_> = snapshot
        .events
        .iter()
        .chain(embed.events.iter())
        .cloned()
        .collect();

    let params = distill_timeout_parameters(&events, &TimeoutLearningConfig::default());
    assert_eq!(
        params.default_millis, None,
        "the floor keeps unnamed callees"
    );
    assert_eq!(
        params.max_millis,
        Some(300_000),
        "the ladder's top pins the ceiling"
    );
    let table = params.per_callee.expect("search earns a bound");
    assert_eq!(table.get("search"), Some(&5_000));
    assert!(
        !table.contains_key("embed"),
        "sparse evidence earns nothing"
    );
}

#[test]
fn timeout_learner_abstains_on_heavy_tails() {
    // The Wave 1 scar, guarded: 19 completions at 1 s and one at 10
    // minutes. No rung below the ladder's top fits within the abort
    // tolerance — the tail is heavier than the ladder, and the learner
    // abstains rather than shipping a bound that fails non-inferiority.
    // The top rung itself is never emitted: a bound equal to it reclaims
    // nothing the floor does not.
    let mut latencies = vec![1_000u64; 19];
    latencies.push(600_000);
    let snapshot = completion_events("batch", &latencies, 0);
    let params = distill_timeout_parameters(&snapshot.events, &TimeoutLearningConfig::default());
    assert!(
        params.per_callee.is_none(),
        "a tail heavier than the ladder keeps the floor: {params:?}"
    );
    assert_eq!(params.max_millis, None);
}

#[test]
fn distilled_parameters_round_trip_through_the_candidate_gate() {
    // The learner's output IS the candidate's content: serialized into a
    // `policy` candidate, applied to the floor by the gate's own parse
    // path, and named by the registry's content-derived version.
    let events = retry_evidence(ErrorClass::Timeout, 10, 5, 0);
    let params = distill_retry_parameters(&events, &RetryLearningConfig::default());
    let candidate = policy_candidate(
        DecisionFamily::Retry,
        serde_json::to_value(&params).unwrap(),
    );
    let CandidateContent::Policy { family, parameters } = &candidate.content else {
        unreachable!()
    };
    let policy = ExecutorPolicy::static_v0()
        .with_family_parameters(*family, parameters.clone())
        .expect("distilled parameters apply to the floor");
    assert!(!policy.is_static_floor());
    let version = derive_policy_version(&policy).unwrap();
    assert_ne!(
        version,
        derive_policy_version(&ExecutorPolicy::static_v0()).unwrap(),
        "a learned body names a distinct version"
    );
}

// ---------- the twin gate ----------

/// The rate-limited fixture's evaluation: a 100 ms backoff candidate
/// against the fault schedule that rate-limits the recorded call's first
/// two attempts.
fn winning_evaluation() -> (Candidate, CandidateEvaluation) {
    let snapshot = rate_limited_snapshot();
    let effect_seq = snapshot
        .events
        .iter()
        .find(|event| event.kind == RunEventKind::ToolCall)
        .map(|event| event.seq)
        .expect("the fixture records the call");
    let faults = FaultSchedule::new(42)
        .with_injection(
            FaultAnchor::OnAttempt {
                effect_seq,
                attempt: 1,
            },
            InjectedFault::RateLimited { retry_after_ms: 50 },
        )
        .with_injection(
            FaultAnchor::OnAttempt {
                effect_seq,
                attempt: 2,
            },
            InjectedFault::RateLimited { retry_after_ms: 50 },
        );
    let evaluator = TwinCandidateEvaluator::new(42, distiller()).with_faults(faults);
    let candidate = policy_candidate(
        DecisionFamily::Retry,
        json!({"base_delay_ms": 100, "max_delay_ms": 30_000, "max_attempts": 3}),
    );
    let request = EvaluationRequest {
        dataset_version: "twin-v1".into(),
        target_metric: "wall_time_ms".into(),
        thresholds: EvaluationThresholds::default(),
        replay_evidence: vec![snapshot],
    };
    let evaluation = futures::executor::block_on(evaluator.evaluate(&candidate, &request))
        .expect("the twin prices the candidate");
    (candidate, evaluation)
}

#[test]
fn twin_gate_admits_a_candidate_that_wins_on_the_target_metric() {
    let (candidate, evaluation) = winning_evaluation();
    assert_eq!(evaluation.replay.matched, 1);
    assert!(evaluation.replay.divergences.is_empty());
    assert!(
        !evaluation.verdict.regressed,
        "identical completion — both arms recover on attempt three"
    );
    assert_eq!(evaluation.verdict.target_metric, "wall_time_ms");
    let delta = evaluation.verdict.delta.expect("the twin prices wall time");
    assert!(
        delta > 0.0,
        "the shorter backoff waits less at identical completion: delta {delta}"
    );
    // Both reports carry the aggregate the drift baseline later reads.
    assert!(evaluation.baseline_report.get("aggregate").is_some());
    assert!(evaluation.candidate_report.get("aggregate").is_some());

    // The mechanical gate: clean replay, no regression, improvement past
    // the bar — admitted on the envelope's own authority.
    let decision = admit_promotion(&auto_policy_envelope(), &candidate, Some(&evaluation), None)
        .expect("a winning candidate clears the evidence bar");
    assert!(matches!(
        decision.authority,
        PromotionAuthority::Envelope { .. }
    ));
}

#[test]
fn twin_gate_refuses_a_candidate_that_regresses_completion() {
    // A 5 s bound on a recorded 10 s completion truncates it on every
    // attempt — observed as Timeout, retried to the budget, dead-lettered.
    // The floor completes two of three items (flaky dead-letters either
    // way); the candidate completes one. Completion parity is breached,
    // the verdict regresses, and the Auto envelope refuses mechanically.
    let evaluator = TwinCandidateEvaluator::new(42, distiller());
    let candidate = policy_candidate(
        DecisionFamily::Timeout,
        json!({"max_millis": 300_000, "per_callee": {"slow": 5_000}}),
    );
    let request = EvaluationRequest {
        dataset_version: "twin-v1".into(),
        target_metric: "completion_rate".into(),
        thresholds: EvaluationThresholds::default(),
        replay_evidence: vec![slow_call_snapshot()],
    };
    let evaluation = futures::executor::block_on(evaluator.evaluate(&candidate, &request))
        .expect("the twin prices the candidate");
    assert!(
        evaluation.verdict.regressed,
        "truncating completions is the regression the gate exists to catch"
    );
    let delta = evaluation.verdict.delta.unwrap();
    assert!(delta < 0.0, "completion fell: delta {delta}");

    let refusal = admit_promotion(&auto_policy_envelope(), &candidate, Some(&evaluation), None)
        .expect_err("a regressed candidate never auto-promotes");
    assert!(
        matches!(
            refusal,
            LearnError::Refused(PromotionRefusal::EvaluationRegressed { .. })
        ),
        "{refusal}"
    );
}

#[test]
fn r08_default_leaves_the_policy_bar_to_a_human_approval() {
    // The release default: `policy` candidates promote only under a
    // scoped approval — the evidence bar is the reviewer's, exercised
    // against the twin's journaled reports, not the gate's arithmetic.
    let (candidate, evaluation) = winning_evaluation();
    let envelope = PromotionEnvelope::r08_default();

    let refusal = admit_promotion(&envelope, &candidate, Some(&evaluation), None)
        .expect_err("no token, no promotion");
    assert!(
        matches!(
            refusal,
            LearnError::Refused(PromotionRefusal::RequiresApproval { .. })
        ),
        "{refusal}"
    );

    let approval = ApprovalToken::approve(promotion_effect_id(&candidate), "ops:test");
    let decision = admit_promotion(&envelope, &candidate, Some(&evaluation), Some(&approval))
        .expect("a scoped token admits");
    match decision.authority {
        PromotionAuthority::Approval { approved_by } => assert_eq!(approved_by, "ops:test"),
        other => panic!("expected approval authority, got {other:?}"),
    }

    // The token is scoped, not transferable: a token minted for another
    // candidate's promotion effect does not admit this one.
    let other = policy_candidate(
        DecisionFamily::Retry,
        json!({"base_delay_ms": 250, "max_delay_ms": 30_000, "max_attempts": 3}),
    );
    let wrong = ApprovalToken::approve(promotion_effect_id(&other), "ops:test");
    let refusal = admit_promotion(&envelope, &candidate, Some(&evaluation), Some(&wrong))
        .expect_err("a token admits exactly one effect");
    assert!(
        matches!(
            refusal,
            LearnError::Refused(PromotionRefusal::ApprovalMismatch { .. })
        ),
        "{refusal}"
    );
}

// ---------- drift detection ----------

fn drift_version() -> PolicyVersion {
    PolicyVersion::new("policy-drift-test")
}

/// `terminal` decisions by the acting version: `successes` recovered,
/// `dead_lettered` spent their budget with the gates open, the rest
/// failed with the gates closed. Latency feature on every decision.
fn acting_outcomes(
    terminal: u64,
    successes: u64,
    dead_lettered: u64,
    latency_ms: u64,
) -> Vec<DecisionEvent> {
    let version = drift_version();
    let mut events = Vec::new();
    for seq in 1..=terminal {
        if seq <= successes {
            events.push(journaled_retry(
                seq,
                ErrorClass::Timeout,
                1,
                3,
                &RetryDecision::Retry { after_ms: 100 },
                Some(DecisionOutcome::Success),
                Some(latency_ms),
                &version,
            ));
        } else if seq <= successes + dead_lettered {
            // The budget spent with the gates open: attempt 3 of 3,
            // retryable class, repeatable effect — the dead-letter shape
            // the retry family journals (the legal set has collapsed).
            events.push(journaled_retry(
                seq,
                ErrorClass::Timeout,
                3,
                3,
                &RetryDecision::Dead,
                Some(DecisionOutcome::Failure),
                Some(latency_ms),
                &version,
            ));
        } else {
            // The gates closed: a non-retryable class fails immediately.
            events.push(journaled_retry(
                seq,
                ErrorClass::InvalidInput,
                1,
                3,
                &RetryDecision::Fail,
                Some(DecisionOutcome::Failure),
                Some(latency_ms),
                &version,
            ));
        }
    }
    events
}

#[test]
fn drift_monitor_stays_quiet_on_a_healthy_version() {
    let events = acting_outcomes(10, 10, 0, 100);
    let baseline = DriftBaseline {
        completion_rate: 1.0,
        dead_letter_rate: 0.0,
        latency_p95_ms: Some(100),
    };
    let report = detect_policy_drift(
        &events,
        &drift_version(),
        &baseline,
        &DriftThresholds::default(),
    );
    assert!(!report.drifted, "healthy: {:?}", report.reasons);
    assert!(report.reasons.is_empty());
    assert_eq!(report.decisions, 10);
    assert_eq!(report.terminal, 10);
    assert_eq!(report.completion_rate, 1.0);
}

#[test]
fn drift_monitor_declares_a_completion_drop() {
    let events = acting_outcomes(10, 6, 0, 100);
    let baseline = DriftBaseline {
        completion_rate: 1.0,
        dead_letter_rate: 0.0,
        latency_p95_ms: Some(100),
    };
    let report = detect_policy_drift(
        &events,
        &drift_version(),
        &baseline,
        &DriftThresholds::default(),
    );
    assert!(report.drifted);
    assert!(
        report
            .reasons
            .iter()
            .any(|reason| reason.contains("completion rate")),
        "{:?}",
        report.reasons
    );
}

#[test]
fn drift_monitor_declares_dead_letter_growth() {
    // Two of ten decisions spent their budget with the gates open. The
    // baseline carried a lower completion rate already, so only the
    // dead-letter signal breaches.
    let events = acting_outcomes(10, 8, 2, 100);
    let baseline = DriftBaseline {
        completion_rate: 0.75,
        dead_letter_rate: 0.0,
        latency_p95_ms: None,
    };
    let report = detect_policy_drift(
        &events,
        &drift_version(),
        &baseline,
        &DriftThresholds::default(),
    );
    assert!(report.drifted);
    assert_eq!(report.dead_letter_rate, 0.2);
    assert!(
        report
            .reasons
            .iter()
            .any(|reason| reason.contains("dead-letter")),
        "{:?}",
        report.reasons
    );
    assert!(
        !report
            .reasons
            .iter()
            .any(|reason| reason.contains("completion rate")),
        "completion held its baseline: {:?}",
        report.reasons
    );
}

#[test]
fn drift_monitor_declares_latency_growth() {
    let events = acting_outcomes(10, 10, 0, 200);
    let baseline = DriftBaseline {
        completion_rate: 1.0,
        dead_letter_rate: 0.0,
        latency_p95_ms: Some(100),
    };
    let report = detect_policy_drift(
        &events,
        &drift_version(),
        &baseline,
        &DriftThresholds::default(),
    );
    assert!(report.drifted);
    assert!(
        report.reasons.iter().any(|reason| reason.contains("p95")),
        "{:?}",
        report.reasons
    );
}

#[test]
fn drift_monitor_abstains_on_sparse_evidence() {
    // Every terminal decision failed — and still nothing is declared:
    // three outcomes are noise, not a verdict.
    let events = acting_outcomes(3, 0, 0, 100);
    let baseline = DriftBaseline {
        completion_rate: 1.0,
        dead_letter_rate: 0.0,
        latency_p95_ms: Some(100),
    };
    let report = detect_policy_drift(
        &events,
        &drift_version(),
        &baseline,
        &DriftThresholds::default(),
    );
    assert!(!report.drifted);
    assert!(
        report
            .reasons
            .iter()
            .any(|reason| reason.contains("insufficient evidence")),
        "{:?}",
        report.reasons
    );
}

#[test]
fn drift_monitor_ignores_shadow_and_other_version_decisions() {
    let mut events = acting_outcomes(10, 10, 0, 100);
    // Shadow decisions by the acting version — off-policy evidence for
    // the next candidate — and acting decisions by another version:
    // neither speaks for this version's health.
    for seq in 100..110 {
        let mut shadow = journaled_retry(
            seq,
            ErrorClass::InvalidInput,
            1,
            3,
            &RetryDecision::Fail,
            Some(DecisionOutcome::Failure),
            Some(5_000),
            &drift_version(),
        );
        shadow.role = Some(DecisionRole::Shadow);
        events.push(shadow);
    }
    for seq in 200..210 {
        events.push(journaled_retry(
            seq,
            ErrorClass::InvalidInput,
            1,
            3,
            &RetryDecision::Fail,
            Some(DecisionOutcome::Failure),
            Some(5_000),
            &PolicyVersion::new("policy-other"),
        ));
    }
    let baseline = DriftBaseline {
        completion_rate: 1.0,
        dead_letter_rate: 0.0,
        latency_p95_ms: Some(100),
    };
    let report = detect_policy_drift(
        &events,
        &drift_version(),
        &baseline,
        &DriftThresholds::default(),
    );
    assert!(!report.drifted, "{:?}", report.reasons);
    assert_eq!(report.decisions, 10, "only this version's acting decisions");
    assert_eq!(report.terminal, 10);
}

#[test]
fn drift_baseline_round_trips_from_a_twin_evaluation_report() {
    // The promotion-time baseline is read back off the evaluation's own
    // baseline report — the evidence that promoted the version is the
    // yardstick its health is measured against.
    let (_candidate, evaluation) = winning_evaluation();
    let baseline = DriftBaseline::from_twin_report(&evaluation.baseline_report)
        .expect("the twin report carries the aggregate");
    assert_eq!(
        baseline.completion_rate, 1.0,
        "the floor completes the fixture"
    );
    assert_eq!(baseline.dead_letter_rate, 0.0);
    assert!(baseline.latency_p95_ms.is_some());

    // A report with no aggregate — a non-twin evaluation, a hand-authored
    // report — names no baseline. Drift detection fails absent rather
    // than guessing.
    assert!(DriftBaseline::from_twin_report(&json!({"summary": {}})).is_none());
}
