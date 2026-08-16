//! Tool-outcome-learning integration tests (R0.13 agent core, wave 3).
//!
//! Six groups:
//!
//! - **Golden files** — the wire shapes of the `ToolOutcomeIndex`, the
//!   extended `tool_contract` candidate carrying the additive `selection`
//!   member, and the distilled argument guidance, pinned under
//!   `tests/golden/`. `UPDATE_GOLDEN=1` blesses an intentional change. The
//!   pre-wave `candidate_tool_contract.json` golden is round-tripped to
//!   prove the new member leaves the existing wire shape byte-identical.
//! - **The roll-up** — mixed synthetic journaled evidence (successes,
//!   opaque `Error`-status failures, structured validation refusals)
//!   produces correct per-tool `ToolOutcomeStats`, latency percentiles,
//!   argument-pattern outcomes, and violation clusters.
//! - **The N10 proof** — a dedicated test that refusals are counted by
//!   PARSING PAYLOADS: every event in its journal carries
//!   `EventStatus::Ok`, and the structured refusal still lands in
//!   `validation_failures`, never in `successes`.
//! - **Determinism and fail-loud** — the index rebuilds from the same
//!   journals byte-identically; a ToolCall with no journaled output is an
//!   inconsistent journal, not a silent skip.
//! - **The gate** — a learned selection overlay candidate passes
//!   `admit_promotion` (approval-ruled, like every registry kind), promotes
//!   through the journaled lifecycle, and rolls back byte-exactly.
//! - **End to end** — a scripted-model scenario: a baseline run over the
//!   identity shortlist journals validation refusals; the roll-up and
//!   distiller read them; a promoted overlay re-ranks the shortlist; the
//!   next run makes zero invalid calls at non-inferior completion.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    admit_promotion, candidate_effect_key, evaluation_effect_key, promotion_effect_id,
    promotion_effect_key, rollback_effect_key, Candidate, CandidateContent, CandidateEvaluation,
    CandidateRecord, CandidateStatus, EvaluationThresholds, EvaluationVerdict, EvidenceSpan,
    LearnError, PromotionAuthority, PromotionDecision, PromotionEnvelope, PromotionReceipt,
    PromotionRefusal, ReplaySummary, RollbackReceipt, VersionPointer,
};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::record::{Effect, EventStatus, RunEventKind};
use rusty_agent_runtime::replay::{tool_call_request, RecordingTool};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_runtime::tool_outcomes::{
    build_outcome_index, distill_argument_guidance, selection_candidate, ToolOutcomeIndex,
};
use rusty_agent_runtime::tool_select::{
    argument_validation_refusal, manifests_for_registry, parse_argument_validation_refusal,
    select, ArgumentViolation, SelectionFeatures, ToolSelectionOverlay, ValidatingTool,
};

// ---------- golden-file machinery (the tests/learn.rs discipline) ----------

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

const CLOCK_START_MS: u64 = 1_750_200_000_000;
const STAMP_MS: i64 = 1_750_200_010_000;

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn distiller() -> ProvenanceAuthor {
    ProvenanceAuthor::Distiller {
        name: "tool-outcome-distiller".into(),
    }
}

/// Journal one tool call in the `RecordingTool` shapes: success carries the
/// result verbatim at `Ok`; a tool error carries `{"error": …}` at `Error`;
/// a validation refusal carries the structured contract string at `Ok`
/// (N10 — the wrapper returns it as an ordinary `Ok` result).
fn journal_call(
    journal: &Journal,
    tool: &str,
    args: Value,
    output: Value,
    status: EventStatus,
    latency_ms: u64,
) {
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::ReadOnly)
            .node("tools")
            .parent("parent-event")
            .input(tool_call_request(tool, &args))
            .output(output)
            .status(status)
            .latency_ms(latency_ms),
    );
}

/// The mixed-evidence journal every roll-up test shares: successes, an
/// opaque `Error`-status failure, and structured refusals at `Ok`.
fn evidence_journal(run_id: &str) -> Journal {
    let journal = Journal::new(run_id, "thread-1", Clock::logical(CLOCK_START_MS, 1));
    // search.legacy: one success, one structured refusal, one opaque error.
    journal_call(
        &journal,
        "search.legacy",
        json!({"query": "rust"}),
        json!({"results": ["a"]}),
        EventStatus::Ok,
        12,
    );
    journal_call(
        &journal,
        "search.legacy",
        json!({"limit": "5"}),
        Value::String(argument_validation_refusal(&[
            ArgumentViolation {
                path: "".into(),
                rule: "required".into(),
                message: "missing required property `query`".into(),
            },
            ArgumentViolation {
                path: "/limit".into(),
                rule: "type".into(),
                message: "expected \"integer\", found string".into(),
            },
        ])),
        EventStatus::Ok,
        1,
    );
    journal_call(
        &journal,
        "search.legacy",
        json!({"query": "rust"}),
        json!({"error": "upstream timeout"}),
        EventStatus::Error,
        400,
    );
    // http.get: two successes, then the same refusal twice (a recurring
    // violation pattern — the distiller's input).
    journal_call(
        &journal,
        "http.get",
        json!({"url": "https://a.example"}),
        json!({"status": 200}),
        EventStatus::Ok,
        30,
    );
    journal_call(
        &journal,
        "http.get",
        json!({"url": "https://b.example"}),
        json!({"status": 200}),
        EventStatus::Ok,
        50,
    );
    for (args, latency) in [(json!({"url": 5}), 2), (json!({"url": 7}), 3)] {
        journal_call(
            &journal,
            "http.get",
            args,
            Value::String(argument_validation_refusal(&[ArgumentViolation {
                path: "/url".into(),
                rule: "type".into(),
                message: "expected \"string\", found integer".into(),
            }])),
            EventStatus::Ok,
            latency,
        );
    }
    journal
}

fn evidence_index() -> ToolOutcomeIndex {
    let journal = evidence_journal("run-a");
    let snapshot = journal.snapshot();
    build_outcome_index(&[&snapshot], ts(STAMP_MS)).unwrap()
}

// ---------- the roll-up ----------

#[test]
fn rollup_over_mixed_evidence_produces_correct_stats() {
    // Wave-3 exit criterion 1: mixed successes, opaque failures, and
    // structured validation refusals roll up into correct ToolOutcomeStats.
    let index = evidence_index();

    let legacy = index.rollup_for("search.legacy").unwrap();
    assert_eq!(legacy.stats.calls, 3);
    assert_eq!(legacy.stats.successes, 1);
    assert_eq!(legacy.stats.validation_failures, 1);
    assert_eq!(legacy.opaque_failures, 1);
    assert_eq!(legacy.stats.success_bps(), Some(3_333));
    assert_eq!(legacy.latencies_ms, [1, 12, 400]);
    assert_eq!(legacy.latency_percentile(50), Some(12));
    assert_eq!(legacy.latency_percentile(95), Some(400));

    let http = index.rollup_for("http.get").unwrap();
    assert_eq!(http.stats.calls, 4);
    assert_eq!(http.stats.successes, 2);
    assert_eq!(http.stats.validation_failures, 2);
    assert_eq!(http.opaque_failures, 0);
    assert_eq!(http.latency_percentile(50), Some(3));
    assert_eq!(http.latency_percentile(95), Some(50));

    // Violation clusters: the recurring http.get `/url` type failure, and
    // search.legacy's two distinct single occurrences.
    assert_eq!(http.violations.len(), 1);
    assert_eq!(http.violations[0].path, "/url");
    assert_eq!(http.violations[0].rule, "type");
    assert_eq!(http.violations[0].count, 2);
    assert_eq!(legacy.violations.len(), 2);
    assert_eq!(legacy.violations[0].path, "", "sorted by (path, rule)");
    assert_eq!(legacy.violations[1].path, "/limit");

    // Argument patterns group by shape, not values: the two `{"query":
    // "rust"}` calls (one success, one opaque failure) share one pattern.
    assert_eq!(legacy.patterns.len(), 2);
    let shared = legacy
        .patterns
        .values()
        .find(|p| p.calls == 2)
        .expect("the shared-shape pattern");
    assert_eq!(shared.successes, 1);
    assert_eq!(shared.opaque_failures, 1);
    let url_patterns: Vec<_> = http.patterns.values().collect();
    assert_eq!(url_patterns.len(), 2);
    let bad_url = url_patterns
        .iter()
        .find(|p| p.validation_failures == 2)
        .expect("the quoted-url pattern");
    assert_eq!(bad_url.success_bps(), Some(0));

    // The selection layer's input shape, directly consumable.
    let snapshot = index.selection_snapshot();
    assert_eq!(snapshot["search.legacy"].validation_failures, 1);
    assert_eq!(snapshot["http.get"].success_bps(), Some(5_000));
}

#[test]
fn refusals_are_counted_by_parsing_payloads_never_status() {
    // THE N10 proof: every event in this journal carries EventStatus::Ok —
    // a status-reading roll-up would see three successes. The structured
    // refusal is counted as a validation failure and the opaque `ERROR:`
    // string as an opaque failure because the roll-up PARSES PAYLOADS.
    let journal = Journal::new("run-n10", "thread-1", Clock::logical(CLOCK_START_MS, 1));
    journal_call(
        &journal,
        "search.legacy",
        json!({"limit": "5"}),
        Value::String(argument_validation_refusal(&[ArgumentViolation {
            path: "".into(),
            rule: "required".into(),
            message: "missing required property `query`".into(),
        }])),
        EventStatus::Ok,
        1,
    );
    journal_call(
        &journal,
        "search.legacy",
        json!({"query": "rust"}),
        Value::String("ERROR: upstream exploded".into()),
        EventStatus::Ok,
        9,
    );
    journal_call(
        &journal,
        "search.legacy",
        json!({"query": "rust"}),
        json!({"results": ["a"]}),
        EventStatus::Ok,
        11,
    );
    let snapshot = journal.snapshot();
    assert!(
        snapshot.events.iter().all(|e| e.status == EventStatus::Ok),
        "the proof's premise: nothing here journals as Error"
    );

    let index = build_outcome_index(&[&snapshot], ts(STAMP_MS)).unwrap();
    let stats = &index.rollup_for("search.legacy").unwrap().stats;
    assert_eq!(stats.calls, 3);
    assert_eq!(stats.successes, 1, "the refusal is not a success");
    assert_eq!(
        stats.validation_failures, 1,
        "counted via parse_argument_validation_refusal despite status Ok"
    );
    assert_eq!(
        index.rollup_for("search.legacy").unwrap().opaque_failures,
        1,
        "the unparseable ERROR string is the opaque tier"
    );
}

#[test]
fn the_index_rebuilds_from_journals_byte_identically() {
    // The derived-index discipline: equal inputs, byte-identical index.
    let journal_a = evidence_journal("run-a");
    let journal_b = evidence_journal("run-a");
    let first = build_outcome_index(&[&journal_a.snapshot()], ts(STAMP_MS)).unwrap();
    let second = build_outcome_index(&[&journal_b.snapshot()], ts(STAMP_MS)).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

#[test]
fn incomplete_journals_fail_loud_and_interrupted_calls_are_excluded() {
    // A ToolCall with no output payload is an inconsistent journal, not a
    // silent skip.
    let journal = Journal::new("run-bad", "thread-1", Clock::logical(CLOCK_START_MS, 1));
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::ReadOnly)
            .input(tool_call_request("search.legacy", &json!({"query": "rust"}))),
    );
    let snapshot = journal.snapshot();
    assert!(build_outcome_index(&[&snapshot], ts(STAMP_MS)).is_err());

    // A suspended run's in-flight call is not terminal evidence.
    let journal = Journal::new("run-suspended", "thread-1", Clock::logical(CLOCK_START_MS, 1));
    journal_call(
        &journal,
        "search.legacy",
        json!({"query": "rust"}),
        json!({"results": ["a"]}),
        EventStatus::Interrupted,
        5,
    );
    let snapshot = journal.snapshot();
    let index = build_outcome_index(&[&snapshot], ts(STAMP_MS)).unwrap();
    assert!(index.tools.is_empty(), "Interrupted is excluded");
}

// ---------- the argument-repair distiller ----------

#[test]
fn distiller_turns_recurring_violations_into_guidance() {
    let index = evidence_index();

    // The margin gate: at min 2 only the recurring pattern earns guidance.
    let guidance = distill_argument_guidance(&index, 2);
    assert_eq!(guidance.len(), 1);
    assert_eq!(guidance[0].tool, "http.get");
    assert_eq!(guidance[0].path, "/url");
    assert_eq!(guidance[0].rule, "type");
    assert_eq!(guidance[0].occurrences, 2);
    assert_eq!(
        guidance[0].guidance,
        "the value at `/url` has the wrong JSON type; supply the declared type — \
         quoted numerics never coerce"
    );

    // At min 1 every cluster speaks, ordered by tool, path, rule.
    let guidance = distill_argument_guidance(&index, 1);
    let keys: Vec<(&str, &str, &str)> = guidance
        .iter()
        .map(|g| (g.tool.as_str(), g.path.as_str(), g.rule.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("http.get", "/url", "type"),
            ("search.legacy", "", "required"),
            ("search.legacy", "/limit", "type"),
        ]
    );
    assert_eq!(
        guidance[1].guidance,
        "calls are missing a required property at the arguments root; include every \
         property the schema's `required` list names"
    );
}

// ---------- the gate: overlays promote and roll back byte-exactly ----------

const V2_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string","minLength":1}},"required":["query"],"additionalProperties":false}"#;

fn overlay_candidate(tags: &[&str], created_ms: i64) -> Candidate {
    selection_candidate(
        "search.v2",
        serde_json::from_str(V2_SCHEMA).unwrap(),
        ToolSelectionOverlay {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        },
        distiller(),
        EvidenceSpan {
            run_ids: vec!["run-a".into()],
            ..EvidenceSpan::default()
        },
        ts(created_ms),
    )
    .unwrap()
}

fn evaluation(candidate: &Candidate) -> CandidateEvaluation {
    CandidateEvaluation {
        candidate_id: candidate.candidate_id.clone(),
        dataset_version: "tools-v1".into(),
        replay: ReplaySummary {
            fixture_ids: vec!["run-a".into()],
            matched: 1,
            divergences: Vec::new(),
        },
        baseline_report: json!({
            "format_version": 1,
            "name": "tools@tools-v1",
            "dataset_version": "tools-v1",
            "summary": {"run_pass_rate": 0.5},
        }),
        candidate_report: json!({
            "format_version": 1,
            "name": "tools@tools-v1",
            "dataset_version": "tools-v1",
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
        evaluated_at: ts(1_750_200_003_000),
    }
}

#[test]
fn selection_overlay_promotes_through_the_gate_and_rolls_back_byte_exactly() {
    // Wave-3 exit criterion 2: a learned overlay candidate passes
    // admit_promotion through the envelope (approval-ruled, like every
    // registry kind), the lifecycle journals through the four existing
    // candidate kinds, and the promote A → B → rollback walk restores A
    // byte-exactly.
    let a = overlay_candidate(&["search"], 1_750_200_002_000);
    let b = overlay_candidate(&["search", "docs"], 1_750_200_002_500);
    assert_ne!(a.candidate_id, b.candidate_id);
    assert_eq!(a.surface().as_str(), "tool_contract:search.v2");
    match &a.content {
        CandidateContent::ToolContract { selection, .. } => {
            assert!(selection.is_some(), "the wave-3 member is carried");
        }
        other => panic!("expected a tool_contract candidate, got {other:?}"),
    }

    // The gate holds the family at approval; a scoped token admits.
    let envelope = PromotionEnvelope::r08_default();
    let refusal = admit_promotion(&envelope, &a, Some(&evaluation(&a)), None)
        .expect_err("no token, no promotion");
    assert!(matches!(
        refusal,
        LearnError::Refused(PromotionRefusal::RequiresApproval { .. })
    ));
    let approval = ApprovalToken::approve(promotion_effect_id(&a), "ops:test");
    let decision = admit_promotion(&envelope, &a, Some(&evaluation(&a)), Some(&approval))
        .expect("a scoped token admits");
    match decision.authority {
        PromotionAuthority::Approval { approved_by } => assert_eq!(approved_by, "ops:test"),
        other => panic!("expected approval authority, got {other:?}"),
    }

    // Every transition journaled through the existing candidate kinds,
    // parented to the transition that caused it.
    let journal = Journal::new("learn-run", "learn-thread", Clock::System);
    let created = journal.record(
        EventDraft::new(RunEventKind::CandidateCreated, Effect::Idempotent)
            .input(json!({
                "effect_key": candidate_effect_key(&a.candidate_id),
                "candidate_id": a.candidate_id,
            }))
            .output(serde_json::to_value(&a).unwrap()),
    );
    let evaluated = journal.record(
        EventDraft::new(RunEventKind::CandidateEvaluated, Effect::Idempotent)
            .input(json!({
                "effect_key": evaluation_effect_key(&a.candidate_id, "tools-v1"),
                "candidate_id": a.candidate_id,
            }))
            .output(serde_json::to_value(evaluation(&a)).unwrap())
            .parent(created.clone()),
    );
    let promote_a = PromotionReceipt {
        candidate_id: a.candidate_id.clone(),
        surface: a.surface(),
        previous: None,
        decision: PromotionDecision {
            authority: PromotionAuthority::Approval {
                approved_by: "ops:test".into(),
            },
            canary: None,
        },
        promoted_at: ts(1_750_200_004_000),
    };
    let promoted = journal.record(
        EventDraft::new(RunEventKind::CandidatePromoted, Effect::Idempotent)
            .input(json!({
                "effect_key": promotion_effect_key(&a.candidate_id),
                "candidate_id": a.candidate_id,
            }))
            .output(serde_json::to_value(&promote_a).unwrap())
            .parent(evaluated.clone()),
    );

    // Promote A → B, roll back, re-resolve: byte-exact.
    let mut store = std::collections::HashMap::new();
    store.insert(a.candidate_id.to_string(), serde_json::to_vec(&a).unwrap());
    store.insert(b.candidate_id.to_string(), serde_json::to_vec(&b).unwrap());
    let promote_b = PromotionReceipt {
        candidate_id: b.candidate_id.clone(),
        surface: a.surface(),
        previous: Some(a.candidate_id.clone()),
        decision: promote_a.decision.clone(),
        promoted_at: ts(1_750_200_005_000),
    };
    let pointer = VersionPointer::new(a.surface())
        .promoted(&promote_a)
        .promoted(&promote_b);
    assert_eq!(pointer.active, Some(b.candidate_id.clone()));

    let rollback = RollbackReceipt {
        surface: a.surface(),
        from: b.candidate_id.clone(),
        to: promote_b.previous.clone(),
        cause: "selection regression on the replay set".into(),
        rolled_back_at: ts(1_750_200_006_000),
    };
    let rolled = pointer.rolled_back(&rollback);
    let restored_id = rolled.active.expect("rollback re-points to A");
    assert_eq!(restored_id, a.candidate_id);
    journal.record(
        EventDraft::new(RunEventKind::CandidateRolledBack, Effect::Idempotent)
            .input(json!({
                "effect_key": rollback_effect_key(&a.surface(), &b.candidate_id),
                "candidate_id": b.candidate_id,
            }))
            .output(serde_json::to_value(&rollback).unwrap())
            .parent(promoted.clone()),
    );
    let kinds: Vec<RunEventKind> = journal.events().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RunEventKind::CandidateCreated,
            RunEventKind::CandidateEvaluated,
            RunEventKind::CandidatePromoted,
            RunEventKind::CandidateRolledBack,
        ],
        "every transition journaled through the existing candidate kinds"
    );

    // Re-resolution by id returns the exact bytes that served before B.
    let redistilled = overlay_candidate(&["search"], 1_750_200_002_000);
    assert_eq!(redistilled.candidate_id, restored_id);
    let restored_bytes = store.get(restored_id.as_str()).expect("the restored id resolves");
    assert_eq!(*restored_bytes, serde_json::to_vec(&redistilled).unwrap());
    let restored: Candidate = serde_json::from_slice(restored_bytes).unwrap();
    restored.verify_address().unwrap();

    // A's lifecycle record agrees it is promotable again.
    let mut record_a = CandidateRecord::new(a.clone());
    record_a.apply_evaluation(evaluation(&a)).unwrap();
    record_a.apply_promotion(promote_a).unwrap();
    assert_eq!(record_a.status, CandidateStatus::Promoted);
}

// ---------- end to end: a promoted overlay reduces invalid calls ----------

struct LegacySearch;

#[async_trait]
impl Tool for LegacySearch {
    fn name(&self) -> &str {
        "search.legacy"
    }
    fn description(&self) -> &str {
        "The old search endpoint."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::from_str(V2_SCHEMA).unwrap()
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"results": ["legacy"], "query": args.get("query").cloned().unwrap_or(Value::Null)}))
    }
}

struct V2Search;

#[async_trait]
impl Tool for V2Search {
    fn name(&self) -> &str {
        "search.v2"
    }
    fn description(&self) -> &str {
        "The current search endpoint."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::from_str(V2_SCHEMA).unwrap()
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"results": ["v2"], "query": args.get("query").cloned().unwrap_or(Value::Null)}))
    }
}

fn base_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(LegacySearch);
    registry.register(V2Search);
    registry
}

/// One scripted-model run: shortlist under `overlays`, take the top-ranked
/// tool, and call it through the shipped `ValidatingTool` + `RecordingTool`
/// stack with the model's habitual arguments — `search.legacy` gets the
/// planted defect (an integer `query`), `search.v2` a well-formed call. On
/// a refusal the script retries once, unchanged — the defect does not
/// self-repair — then completes with an answer either way. Returns the
/// journal and whether the run completed.
async fn scripted_run(run_id: &str, overlays: &BTreeMap<String, ToolSelectionOverlay>) -> (Journal, bool) {
    let journal = Journal::new(run_id, "thread-1", Clock::logical(CLOCK_START_MS, 1));
    let registry = base_registry();
    let manifests = manifests_for_registry(&registry, overlays).unwrap();
    let features = SelectionFeatures {
        task_tags: vec!["search".into()],
        effect_ceiling: Effect::ReadOnly,
        outcomes: BTreeMap::new(),
    };
    let shortlist = select(&features, &manifests, 10);
    let chosen = shortlist.selected[0].name.clone();

    let narrowed = registry.restricted_to(std::slice::from_ref(&chosen)).unwrap();
    let validated = ValidatingTool::wrap_registry(&narrowed);
    let tool = validated.get(&chosen).unwrap();
    let recording = RecordingTool::new(tool, journal.clone(), "parent-event").node("tools");

    // The habitual call, then one unchanged retry when refused.
    let habit = if chosen == "search.legacy" {
        json!({"query": 5})
    } else {
        json!({"query": "rust"})
    };
    let first = recording.call(habit.clone()).await.unwrap();
    if matches!(&first, Value::String(s) if parse_argument_validation_refusal(s).is_some()) {
        recording.call(habit).await.unwrap();
    }
    (journal, true)
}

/// The invalid + failed call count of a run's journal, through the roll-up
/// the learning loop itself uses.
fn invalid_and_failed(snapshot: &JournalSnapshot) -> u64 {
    let index = build_outcome_index(&[snapshot], ts(STAMP_MS)).unwrap();
    index
        .tools
        .values()
        .map(|r| r.stats.validation_failures + r.opaque_failures)
        .sum()
}

#[tokio::test]
async fn a_promoted_overlay_reduces_invalid_calls_at_non_inferior_completion() {
    // Wave-3 exit criterion 3, the design's proof shape end to end:
    // evidence → roll-up → distiller → candidate → gate → pointer → a new
    // run that measurably reduces invalid/failed calls at non-inferior
    // completion.

    // 1. The baseline run over the identity shortlist: no tags, no outcome
    //    history — the tie breaks by name and `search.legacy` ranks first,
    //    so the planted defect fires twice, both journaled.
    let (baseline, baseline_completed) = scripted_run("run-baseline", &BTreeMap::new()).await;
    let baseline_snapshot = baseline.snapshot();
    let baseline_index = build_outcome_index(&[&baseline_snapshot], ts(STAMP_MS)).unwrap();
    let legacy = baseline_index.rollup_for("search.legacy").unwrap();
    assert_eq!(legacy.stats.calls, 2);
    assert_eq!(legacy.stats.successes, 0);
    assert_eq!(
        legacy.stats.validation_failures, 2,
        "both defective calls journaled as structured refusals (status Ok, parsed)"
    );
    assert_eq!(invalid_and_failed(&baseline_snapshot), 2);
    assert!(baseline_completed);

    // 2. The distiller reads the roll-up: the recurring violation becomes
    //    reviewable guidance, and the learned overlay — tagging the
    //    reliable endpoint for the task's `search` tag — becomes a
    //    candidate carrying the evidence span.
    let guidance = distill_argument_guidance(&baseline_index, 1);
    assert_eq!(guidance.len(), 1);
    assert_eq!(guidance[0].tool, "search.legacy");
    let candidate = overlay_candidate(&["search"], 1_750_200_002_000);

    // 3. The gate: approval-ruled; the scoped token admits.
    let approval = ApprovalToken::approve(promotion_effect_id(&candidate), "ops:test");
    admit_promotion(
        &PromotionEnvelope::r08_default(),
        &candidate,
        Some(&evaluation(&candidate)),
        Some(&approval),
    )
    .expect("the learned overlay clears the envelope");

    // 4. The promoted run resolves its overlays from the promoted
    //    candidate's content — the pointer's artifact, not a side channel.
    let CandidateContent::ToolContract {
        tool, selection: Some(overlay), ..
    } = &candidate.content
    else {
        panic!("the candidate carries the overlay");
    };
    let mut overlays = BTreeMap::new();
    overlays.insert(tool.clone(), overlay.clone());
    let (promoted, promoted_completed) = scripted_run("run-promoted", &overlays).await;
    let promoted_snapshot = promoted.snapshot();

    // 5. The measurement: the overlay's tag outranks the defective tool,
    //    the script drives `search.v2`, and invalid + failed calls drop
    //    from 2 to 0 while completion holds.
    let promoted_index = build_outcome_index(&[&promoted_snapshot], ts(STAMP_MS)).unwrap();
    assert_eq!(invalid_and_failed(&baseline_snapshot), 2);
    assert_eq!(invalid_and_failed(&promoted_snapshot), 0);
    let v2 = promoted_index.rollup_for("search.v2").unwrap();
    assert_eq!(v2.stats.calls, 1);
    assert_eq!(v2.stats.success_bps(), Some(10_000));
    assert_eq!(promoted_completed, baseline_completed, "non-inferior completion");
}

// ---------- goldens ----------

#[test]
fn golden_tool_outcome_index_shape() {
    assert_golden("tool_outcome_index.json", &evidence_index());
}

#[test]
fn golden_selection_candidate_shape() {
    assert_golden(
        "candidate_tool_contract_selection.json",
        &overlay_candidate(&["search"], 1_750_200_002_000),
    );
}

#[test]
fn golden_argument_guidance_shape() {
    assert_golden(
        "argument_guidance.json",
        &distill_argument_guidance(&evidence_index(), 1),
    );
}

#[test]
fn pre_wave_tool_contract_wire_shape_is_byte_identical() {
    // The M2 additivity proof at the wire level: the pre-wave golden parses
    // under the extended contract (the `selection` member defaults to
    // absent), keeps its content address, and re-serializes to the same
    // bytes.
    let path = golden_path("candidate_tool_contract.json");
    let expected = std::fs::read_to_string(&path).unwrap();
    let candidate: Candidate = serde_json::from_str(&expected).unwrap();
    candidate.verify_address().unwrap();
    match &candidate.content {
        CandidateContent::ToolContract { selection, .. } => {
            assert!(selection.is_none(), "the pre-wave shape carries no selection");
        }
        other => panic!("expected a tool_contract candidate, got {other:?}"),
    }
    let rendered = format!("{}\n", serde_json::to_string_pretty(&candidate).unwrap());
    assert_eq!(
        rendered, expected,
        "the pre-wave tool_contract golden must round-trip byte-identically"
    );
}
