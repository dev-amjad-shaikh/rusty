//! Trajectory-distiller and flagship-loop tests (R0.13 wave 4).
//!
//! Four test groups:
//!
//! - **Golden files** — the `skill` candidate's wire shape
//!   (`candidate_skill.json`) and the distilled package's rendered body
//!   (`distilled_skill.md`) are pinned byte-for-byte; a wording change in
//!   the distiller's rendering is a visible, reviewable diff, never silent
//!   drift. The additivity proof (`pre_wave_candidate_goldens_…`) pins the
//!   other half of the wave's contract delta: every pre-wave candidate
//!   golden parses under the extended [`CandidateKind`], keeps its content
//!   address, and re-serializes byte-identically.
//! - **Distiller contract** — determinism (equal evidence → equal package
//!   → equal candidate id), the evidence-integrity refusals (no
//!   trajectories, orphan corrections), and fail-closed construction
//!   through the skill plane's own parser and scanner: a draft that fails
//!   validation or earns a scan denial never becomes a candidate.
//! - **The gate** — a `skill` candidate sits at `Approval` under the
//!   shipped envelope (the semantic blast radius R0.8 priced for prompts);
//!   only an [`ApprovalToken`] scoped to the candidate's own promotion
//!   effect id admits it.
//! - **The release proof** — the flagship loop end to end: a scripted
//!   agent with a planted defect runs and journals; a human correction
//!   lands through the shipped correction loop; the distiller produces a
//!   candidate skill whose package passes the skill plane's validation and
//!   scan; evaluation (exact replay of the recorded evidence plus a
//!   scripted experiment) shows improvement with no regression; a scoped
//!   approval promotes it; a new run assembles with the promoted skill and
//!   the defect class disappears — with the attribution chain asserted by
//!   walking ids from the improved run's journal back to the correction;
//!   rollback re-points the skill and the defective run returns,
//!   byte-exact.
//!
//! The run drives follow the wave-1 pattern (`tests/context.rs`): the
//! evidence wrapper sits *inside* the assembler, so the journaled
//! `ModelCall` input is the assembled request — the skill body and the
//! manifest's pins are what the journal holds.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::context::{
    AssemblingChatModel, ContextPipeline, ContextPolicy, SectionKind, SectionManifest,
    SectionPolicy, SkillSectionEntry, TokenizerPin, CONTEXT_POLICY_SCHEMA_VERSION,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    admit_promotion, candidate_effect_key, evaluation_effect_key, promotion_effect_id,
    promotion_effect_key, rollback_effect_key, Candidate, CandidateContent, CandidateEvaluation,
    CandidateEvaluator, CandidateKind, EvaluationRequest, EvaluationThresholds, EvaluationVerdict,
    LearnError, PromotionAuthority, PromotionDecision, PromotionEnvelope, PromotionReceipt,
    PromotionRefusal, ReplaySummary, RollbackReceipt, VersionPointer,
};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::memory::{
    Candidacy, Correction, CorrectionTarget, InMemoryMemoryStore, MemoryKind, MemoryProvenance,
    MemoryRecord, MemoryScope, MemorySource, MemoryStore, ProvenanceAuthor, ScopeAddress,
    ValidityWindow,
};
use rusty_agent_runtime::record::{Effect, EventStatus, PayloadRef, RunEvent, RunEventKind};
use rusty_agent_runtime::replay::{
    ExactReplay, RecordingChatModel, RecordingTool, ReplayingChatModel, ReplayingTool,
};
use rusty_agent_runtime::skill::{SkillRegistry, SkillSource, SkillVersion};
use rusty_agent_runtime::skill_distill::{
    distill_skill, trajectory_steps, DistillRequest, DistilledSkill, SkillDistillError,
};
use rusty_agent_runtime::skills::{
    resolve_active_skill, select_skills, skill_pin, skill_section_entries, ActiveSkillSet,
    SkillBinding, SkillCatalogEntry, SkillSelectionFeatures, SkillSelectionPolicy,
};
use rusty_agent_runtime::tool::Tool;

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
    assert_text_golden(
        name,
        &format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
    );
}

/// The text twin of [`assert_golden`], for the distiller's rendered
/// `SKILL.md` body: a wording change must land as a golden diff.
fn assert_text_golden(name: &str, text: &str) {
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

// ---------- shared fixtures ----------

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn distiller() -> ProvenanceAuthor {
    ProvenanceAuthor::Distiller {
        name: "trajectory-distiller".into(),
    }
}

/// One journaled defective tool call, hand-recorded: the distiller fixture.
/// `seq` 0 is the super-step start, `seq` 1 the defective `issue_refund`
/// call (no `reason` argument — the planted defect class).
fn fixture_snapshot() -> JournalSnapshot {
    let journal = Journal::new(
        "run-defective",
        "thread-w4",
        Clock::logical(1_700_000_000_000, 10),
    );
    let step = journal.record(
        EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
            .input(json!({"step": 0, "active_nodes": ["tools"]})),
    );
    journal.record(
        EventDraft::new(RunEventKind::ToolCall, Effect::NonIdempotent)
            .node("tools")
            .input(json!({"tool": "issue_refund", "arguments": {"order_id": "o-1"}}))
            .output(json!({"refund": "r-1"}))
            .status(EventStatus::Ok)
            .parent(&step),
    );
    journal.snapshot()
}

/// The correction the fixture folds in: the defective call should have
/// carried an explicit reason.
fn fixture_correction() -> Correction {
    Correction {
        correction_id: "correction-1".into(),
        author: "amjad".into(),
        target: CorrectionTarget::RunEvent {
            run_id: "run-defective".into(),
            event_id: "run-defective:1".into(),
        },
        corrected: json!({
            "tool": "issue_refund",
            "arguments": {"order_id": "o-1", "reason": "customer request"},
        }),
        scope: ScopeAddress::new(MemoryScope::Agent, "support-1"),
        rationale: Some("Refunds require an explicit reason.".into()),
    }
}

fn fixture_request() -> DistillRequest {
    DistillRequest {
        name: "refund-with-reason".into(),
        description: "Issue refunds with an explicit reason, per the recorded correction.".into(),
        trajectories: vec![fixture_snapshot()],
        corrections: vec![fixture_correction()],
        binding: None,
        distilled_by: distiller(),
        created_at: ts(1_750_200_000_000),
    }
}

fn fixture_distilled() -> DistilledSkill {
    distill_skill(&fixture_request()).unwrap()
}

// ---------- golden files ----------

#[test]
fn golden_candidate_skill_shape() {
    assert_golden("candidate_skill.json", &fixture_distilled().candidate);
}

#[test]
fn golden_distilled_skill_body_shape() {
    assert_text_golden("distilled_skill.md", fixture_distilled().package.body());
}

#[test]
fn pre_wave_candidate_goldens_are_byte_identical() {
    // The wave's additivity proof at the wire level (the wave-3 M2
    // pattern): every pre-wave candidate golden parses under the extended
    // `CandidateKind` / `CandidateContent`, keeps its content address, and
    // re-serializes to the same bytes. The `skill` variant appends; nothing
    // about the shipped shapes moves.
    for name in [
        "candidate.json",
        "candidate_prompt.json",
        "candidate_tool_contract.json",
        "candidate_tool_contract_selection.json",
        "candidate_model_settings.json",
        "candidate_memory_configuration.json",
        "candidate_memory_configuration_maintenance.json",
        "candidate_middleware_composition.json",
        "candidate_context_policy.json",
    ] {
        let path = golden_path(name);
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
        let candidate: Candidate = serde_json::from_str(&expected).unwrap();
        candidate
            .verify_address()
            .unwrap_or_else(|e| panic!("golden `{name}` fails its own address: {e}"));
        let rendered = format!("{}\n", serde_json::to_string_pretty(&candidate).unwrap());
        assert_eq!(
            rendered, expected,
            "the pre-wave golden `{name}` must round-trip byte-identically"
        );
    }
}

// ---------- distiller contract ----------

#[test]
fn trajectory_extraction_reads_journaled_tool_calls_in_order() {
    let steps = trajectory_steps(&fixture_snapshot());
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].event_id, "run-defective:1");
    assert_eq!(steps[0].tool, "issue_refund");
    assert_eq!(steps[0].arguments, json!({"order_id": "o-1"}));
    assert!(steps[0].ok);
}

#[test]
fn distillation_is_deterministic_and_convergent() {
    let a = distill_skill(&fixture_request()).unwrap();
    let b = distill_skill(&fixture_request()).unwrap();
    assert_eq!(a.package, b.package, "equal evidence drafts equal packages");
    assert_eq!(
        a.candidate.candidate_id, b.candidate.candidate_id,
        "two distillations of the same change converge on one id"
    );
    a.candidate.verify_address().unwrap();

    // Evidence order and multiplicity are not identity: the same evidence
    // supplied reordered (or duplicated) converges on the same package and
    // candidate.
    let mut reordered = fixture_request();
    reordered.trajectories = vec![fixture_snapshot(), fixture_snapshot()];
    let c = distill_skill(&reordered).unwrap();
    assert_eq!(c.package.content_hash(), a.package.content_hash());
    assert_eq!(c.candidate.candidate_id, a.candidate.candidate_id);
}

#[test]
fn candidate_skill_contract_shape() {
    let distilled = fixture_distilled();
    let candidate = &distilled.candidate;
    assert_eq!(candidate.kind(), CandidateKind::Skill);
    assert_eq!(candidate.kind().as_str(), "skill");
    assert_eq!(candidate.surface().as_str(), "skill:refund-with-reason");
    match &candidate.content {
        CandidateContent::Skill {
            name,
            content_hash,
            binding,
        } => {
            assert_eq!(name, "refund-with-reason");
            assert_eq!(
                content_hash,
                &distilled.package.content_hash(),
                "candidate identity and package identity are one digest"
            );
            assert!(binding.is_none());
        }
        other => panic!("expected a skill candidate, got {other:?}"),
    }
    // The evidence span names exactly what the distiller read.
    assert_eq!(
        candidate.evidence.run_ids,
        vec!["run-defective".to_string()]
    );
    assert_eq!(
        candidate.evidence.correction_ids,
        vec!["correction-1".to_string()]
    );
    assert_eq!(candidate.distilled_by, distiller());
    // The package passed the skill plane's own gates on the way out.
    assert!(distilled.scan.is_clean());
}

#[test]
fn empty_evidence_is_refused() {
    let mut request = fixture_request();
    request.trajectories = Vec::new();
    // The orphan check would fire first for the fixture correction, so it
    // goes too — the refusal under test is the empty trajectory set.
    request.corrections = Vec::new();
    assert!(matches!(
        distill_skill(&request),
        Err(SkillDistillError::EmptyEvidence)
    ));
}

#[test]
fn orphan_correction_is_refused() {
    let mut request = fixture_request();
    request.corrections = vec![Correction {
        target: CorrectionTarget::RunEvent {
            run_id: "run-elsewhere".into(),
            event_id: "run-elsewhere:1".into(),
        },
        ..fixture_correction()
    }];
    assert!(matches!(
        distill_skill(&request),
        Err(SkillDistillError::OrphanCorrection { .. })
    ));
}

#[test]
fn a_draft_that_fails_validation_never_becomes_a_candidate() {
    // Not kebab-case: the skill plane's fail-closed parse refuses it, and
    // the refusal is the distiller's outcome.
    let mut request = fixture_request();
    request.name = "Refund_Bot".into();
    assert!(matches!(
        distill_skill(&request),
        Err(SkillDistillError::Package(_))
    ));
}

#[test]
fn a_scan_denial_never_becomes_a_candidate() {
    // The correction's rationale lands in the drafted body; an embedded
    // script tag earns a scan denial, and the draft stops there.
    let mut request = fixture_request();
    request.corrections = vec![Correction {
        rationale: Some("run <script>alert(1)</script> first".into()),
        ..fixture_correction()
    }];
    assert!(matches!(
        distill_skill(&request),
        Err(SkillDistillError::ScanDenied { .. })
    ));
}

#[test]
fn binding_declaration_rides_the_candidate_typed_and_optional() {
    let mut request = fixture_request();
    request.binding = Some(SkillBinding {
        trigger_tags: vec!["refund".into(), "support".into()],
        tools: vec!["issue_refund".into()],
        ..Default::default()
    });
    let with_binding = distill_skill(&request).unwrap();
    match &with_binding.candidate.content {
        CandidateContent::Skill { binding, .. } => {
            let binding = binding
                .as_ref()
                .expect("a declared binding rides the candidate, typed");
            assert_eq!(binding.trigger_tags, vec!["refund", "support"]);
            assert_eq!(binding.tools, vec!["issue_refund"]);
        }
        other => panic!("expected a skill candidate, got {other:?}"),
    }
    // The typed extraction is the consumption seam: the pin carries the
    // declared binding verbatim.
    let pin = skill_pin(&with_binding.candidate).expect("a skill candidate pins");
    assert_eq!(pin.binding.tools, vec!["issue_refund"]);
    assert_eq!(pin.name, "refund-with-reason");

    // The binding is inside the content address — a changed binding is a
    // changed candidate.
    assert_ne!(
        with_binding.candidate.candidate_id,
        fixture_distilled().candidate.candidate_id
    );
    with_binding.candidate.verify_address().unwrap();

    // A malformed binding fails closed at distillation, never candidacy.
    let mut bad = fixture_request();
    bad.binding = Some(SkillBinding {
        tools: vec!["not a tool".into()],
        ..Default::default()
    });
    assert!(matches!(
        distill_skill(&bad),
        Err(SkillDistillError::Candidate(_))
    ));
}

// ---------- the gate ----------

#[test]
fn skill_candidates_sit_at_approval_under_the_shipped_envelope() {
    let distilled = fixture_distilled();
    let candidate = &distilled.candidate;
    let evaluation = CandidateEvaluation {
        candidate_id: candidate.candidate_id.clone(),
        dataset_version: "refunds-v1".into(),
        replay: ReplaySummary {
            fixture_ids: vec!["run-defective".into()],
            matched: 1,
            divergences: Vec::new(),
        },
        baseline_report: json!({"format_version": 1, "summary": {"run_pass_rate": 0.0}}),
        candidate_report: json!({"format_version": 1, "summary": {"run_pass_rate": 1.0}}),
        verdict: EvaluationVerdict {
            regressed: false,
            target_metric: "run_pass_rate".into(),
            baseline: Some(0.0),
            candidate: Some(1.0),
            delta: Some(1.0),
        },
        thresholds: EvaluationThresholds::default(),
        evaluated_by: distiller(),
        evaluated_at: ts(1_750_200_001_000),
    };
    let envelope = PromotionEnvelope::r08_default();

    // Evidence alone never promotes a skill: no token, no promotion — and
    // the refusal names the exact effect id the approval must be scoped to.
    let required = promotion_effect_id(candidate);
    match admit_promotion(&envelope, candidate, Some(&evaluation), None) {
        Err(LearnError::Refused(PromotionRefusal::RequiresApproval { effect_id })) => {
            assert_eq!(effect_id, required);
        }
        other => panic!("expected a scoped-approval refusal, got {other:?}"),
    }

    // A token scoped to another candidate's promotion is not transferable.
    let other = distill_skill(&DistillRequest {
        name: "refund-with-note".into(),
        ..fixture_request()
    })
    .unwrap();
    let wrong = ApprovalToken::approve(promotion_effect_id(&other.candidate), "ops:amjad");
    assert!(matches!(
        admit_promotion(&envelope, candidate, Some(&evaluation), Some(&wrong)),
        Err(LearnError::Refused(
            PromotionRefusal::ApprovalMismatch { .. }
        ))
    ));

    // The scoped token admits, carrying the reviewer's name.
    let token = ApprovalToken::approve(required, "ops:amjad");
    let decision = admit_promotion(&envelope, candidate, Some(&evaluation), Some(&token)).unwrap();
    assert_eq!(
        decision.authority,
        PromotionAuthority::Approval {
            approved_by: "ops:amjad".into()
        }
    );
    assert!(decision.canary.is_none());
}

// ---------- the release proof ----------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const THREAD_ID: &str = "thread-w4";
const TASK: &str = "issue a refund for order o-1";
/// The marker the promoted skill's body carries (the correction's
/// rationale): its presence in the assembled context is what corrects the
/// scripted model's behavior.
const CORRECTION_MARKER: &str = "Refunds require an explicit reason.";

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

/// The scripted refund agent: one tool-calling turn, then the answer. The
/// planted defect: without the correction's marker anywhere in its context,
/// the model issues the refund with no `reason` argument. With the promoted
/// skill assembled into context (the marker is in its body), the model
/// issues the corrected call.
struct RefundModel {
    corrected: Mutex<Option<bool>>,
}

#[async_trait::async_trait]
impl ChatModel for RefundModel {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        _tools: &[Value],
    ) -> RuntimeResult<ChatResponse> {
        let message = {
            let mut corrected = self.corrected.lock().unwrap();
            match *corrected {
                None => {
                    let sees_marker = messages
                        .iter()
                        .filter_map(|message| message.content.as_deref())
                        .any(|content| content.contains(CORRECTION_MARKER));
                    *corrected = Some(sees_marker);
                    let arguments = if sees_marker {
                        json!({"order_id": "o-1", "reason": "customer request"})
                    } else {
                        json!({"order_id": "o-1"})
                    };
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "c1",
                        "issue_refund",
                        arguments,
                    )])
                }
                Some(true) => ChatMessage::assistant("refund r-1 issued for o-1, reason recorded"),
                Some(false) => ChatMessage::assistant("refund r-1 issued for o-1, no reason"),
            }
        };
        Ok(ChatResponse {
            message,
            model: Some("scripted-refund-1".into()),
            usage: None,
        })
    }
}

/// The refund tool (record mode).
struct RefundTool;

#[async_trait::async_trait]
impl Tool for RefundTool {
    fn name(&self) -> &str {
        "issue_refund"
    }
    fn description(&self) -> &str {
        "Issues a refund for an order."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "order_id": {"type": "string"},
                "reason": {"type": "string"},
            },
        })
    }
    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }
    async fn call(&self, args: Value) -> RuntimeResult<Value> {
        Ok(json!({"refund": "r-1", "arguments": args}))
    }
}

fn refund_schema() -> Value {
    RefundTool.parameters_schema()
}

/// The replay sentinels: identical identity to the record-mode halves
/// (schemas feed the model-call request hash), panic on call — exact
/// replay must never reach them, and the counters make "never called"
/// assertable rather than implied.
struct PanicTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "issue_refund"
    }
    fn description(&self) -> &str {
        "Issues a refund for an order."
    }
    fn parameters_schema(&self) -> Value {
        refund_schema()
    }
    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }
    async fn call(&self, _args: Value) -> RuntimeResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicTool was invoked")
    }
}

struct PanicModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChatModel for PanicModel {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> RuntimeResult<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicModel was invoked")
    }
}

/// The assembly policy the runs bind: skills and history sections only —
/// the minimal pipeline that carries a promoted skill into context.
fn skill_pipeline() -> ContextPipeline {
    ContextPipeline::new(ContextPolicy {
        schema_version: CONTEXT_POLICY_SCHEMA_VERSION.to_owned(),
        budget: rusty_agent_runtime::memory::ContextBudget::new(8192),
        tokenizer: TokenizerPin::default(),
        identity: None,
        task: None,
        skills: Some(SectionPolicy::new(1024)),
        tools: None,
        memory: None,
        history: Some(SectionPolicy::new(4096)),
        compaction: None,
    })
    .unwrap()
}

/// The run-facing form of one resolved skill version.
fn skill_entry(version: &SkillVersion) -> SkillSectionEntry {
    SkillSectionEntry {
        name: version.name().to_owned(),
        revision: version.revision().to_string(),
        content_hash: version.content_hash().to_owned(),
        metadata: version.metadata().description,
        body: Some(version.body().to_owned()),
    }
}

/// One assembled, journaled model call: the recording wrapper sits inside
/// the assembler (the wave-1 wiring), so the journaled `ModelCall` input
/// is the assembled request — manifest message and skill body included.
async fn assembled_call(
    journal: &Journal,
    model: Arc<dyn ChatModel>,
    parent: String,
    skills: &[SkillSectionEntry],
    history: &[ChatMessage],
) -> ChatResponse {
    let inner: Arc<dyn ChatModel> =
        Arc::new(RecordingChatModel::new(model, journal.clone(), parent));
    let assembling = AssemblingChatModel::new(inner, skill_pipeline()).with_skills(skills.to_vec());
    assembling
        .chat(history, &[refund_schema()])
        .await
        .expect("the scripted run completes")
}

/// Drive the scripted refund scenario and journal it: one model call
/// requesting the refund tool, the tool call, then the answering model
/// call. `skills` is what admission bound — the promoted skill after
/// promotion, empty before (and after rollback).
async fn drive_recording(run_id: &str, skills: Vec<SkillSectionEntry>) -> JournalSnapshot {
    let journal = Journal::new(run_id, THREAD_ID, logical_clock());
    let model: Arc<dyn ChatModel> = Arc::new(RefundModel {
        corrected: Mutex::new(None),
    });
    let first = assembled_call(
        &journal,
        model.clone(),
        format!("{run_id}:agent:0"),
        &skills,
        &[ChatMessage::user(TASK)],
    )
    .await;
    let call = first.message.tool_calls[0].clone();
    let tool = RecordingTool::new(
        Arc::new(RefundTool),
        journal.clone(),
        format!("{run_id}:tools:0"),
    );
    let result = tool.call(call.arguments.clone()).await.unwrap();
    let history = vec![
        ChatMessage::user(TASK),
        first.message.clone(),
        ChatMessage::tool_result(&call.id, result.to_string()),
    ];
    assembled_call(
        &journal,
        model,
        format!("{run_id}:agent:1"),
        &skills,
        &history,
    )
    .await;
    journal.snapshot()
}

/// The planted defect class, counted in a journal: `issue_refund` calls
/// journaled without the `reason` argument.
fn defect_count(snapshot: &JournalSnapshot) -> usize {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ToolCall)
        .filter(|event| match &event.input {
            Some(PayloadRef::Inline(value)) => {
                value.get("tool").and_then(Value::as_str) == Some("issue_refund")
                    && value
                        .get("arguments")
                        .and_then(|arguments| arguments.get("reason"))
                        .is_none()
            }
            _ => false,
        })
        .count()
}

/// The final assistant answer of a completed scripted run.
fn final_answer(snapshot: &JournalSnapshot) -> String {
    let last_model_call = snapshot
        .events
        .iter()
        .rev()
        .find(|event| event.kind == RunEventKind::ModelCall)
        .expect("a completed run journaled its model calls");
    match &last_model_call.output {
        Some(PayloadRef::Inline(value)) => value["message"]["content"]
            .as_str()
            .expect("the final model call carries an answer")
            .to_owned(),
        other => panic!("expected inline model-call output, got {other:?}"),
    }
}

/// The events of one kind, in journal order.
fn events_of_kind(snapshot: &JournalSnapshot, kind: RunEventKind) -> Vec<&RunEvent> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .collect()
}

/// The structured manifest out of a journaled `ModelCall` request: the
/// assembled messages' reserved manifest message (the wave-1 test's
/// reader).
fn manifest_of(request: &Value) -> SectionManifest {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .expect("model call request carries messages");
    let manifest_message = messages
        .iter()
        .find(|m| m.get("name").and_then(Value::as_str) == Some("rusty.context_manifest"))
        .expect("assembly carries the manifest message");
    let content = manifest_message
        .get("content")
        .and_then(Value::as_str)
        .unwrap();
    serde_json::from_str(content.strip_prefix("context-manifest-v1\n").unwrap()).unwrap()
}

/// The evaluation: the `CandidateEvaluator` seam's scripted implementation
/// for skill candidates. The replay half re-drives the recorded defective
/// run under panic sentinels over the run's own `ReplaySource` — the
/// recorded evidence must reproduce itself byte-for-byte before any
/// comparison is worth reading. The experiment half drives the scripted
/// scenario baseline (no skill) against candidate (the candidate's pinned
/// skill assembled into context), grading the defect class. The reports
/// mimic `rusty-eval`'s `ExperimentReport` summary shape; the verdict
/// computes the target metric's delta the way `compare()` does — the same
/// scripted-seam discipline wave 3's evaluator established (the
/// workspace's dependency direction keeps `rusty-eval` out of the runtime
/// crate).
#[derive(Debug)]
struct SkillEvaluator {
    /// The recorded defective run (the replay half's evidence).
    defective: JournalSnapshot,
    /// The run-facing resolution of the candidate's pinned skill version.
    skill: SkillSectionEntry,
}

#[async_trait::async_trait]
impl CandidateEvaluator for SkillEvaluator {
    async fn evaluate(
        &self,
        candidate: &Candidate,
        request: &EvaluationRequest,
    ) -> RuntimeResult<CandidateEvaluation> {
        // The evaluation applies the candidate's own pinned content, never
        // a lookalike: the bound skill's content hash must be the
        // candidate's.
        let CandidateContent::Skill { content_hash, .. } = &candidate.content else {
            return Err(rusty_agent_runtime::error::RustyError::InvalidUpdate(
                "the skill evaluator evaluates `skill` candidates only".into(),
            ));
        };
        assert_eq!(
            &self.skill.content_hash, content_hash,
            "the evaluated skill is the candidate's pinned revision"
        );

        // The replay half: re-drive the recorded run under panic
        // sentinels; every effect is served from the journal, and the
        // re-driven journal is byte-identical.
        let replay = ExactReplay::new(self.defective.clone())?;
        let rjournal = replay.fresh_journal(logical_clock());
        let source = replay.source();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let run_id = self.defective.run_id.clone();
        let inner: Arc<dyn ChatModel> = Arc::new(ReplayingChatModel::new(
            Arc::new(PanicModel {
                calls: model_calls.clone(),
            }),
            source.clone(),
            rjournal.clone(),
            format!("{run_id}:agent:0"),
        ));
        let assembling = AssemblingChatModel::new(inner, skill_pipeline());
        let first = assembling
            .chat(&[ChatMessage::user(TASK)], &[refund_schema()])
            .await?;
        let call = first.message.tool_calls[0].clone();
        let tool = ReplayingTool::new(
            Arc::new(PanicTool {
                calls: tool_calls.clone(),
            }),
            source.clone(),
            rjournal.clone(),
            format!("{run_id}:tools:0"),
        );
        let result = tool.call(call.arguments.clone()).await?;
        let history = vec![
            ChatMessage::user(TASK),
            first.message.clone(),
            ChatMessage::tool_result(&call.id, result.to_string()),
        ];
        let inner: Arc<dyn ChatModel> = Arc::new(ReplayingChatModel::new(
            Arc::new(PanicModel {
                calls: model_calls.clone(),
            }),
            source.clone(),
            rjournal.clone(),
            format!("{run_id}:agent:1"),
        ));
        AssemblingChatModel::new(inner, skill_pipeline())
            .chat(&history, &[refund_schema()])
            .await?;
        assert_eq!(model_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        assert!(
            source.is_exhausted(),
            "unserved effects: {:?}",
            source.remaining()
        );
        let replayed = rjournal.snapshot();
        assert_eq!(self.defective.events, replayed.events);
        assert_eq!(self.defective.head_hash, replayed.head_hash);

        // The experiment half: baseline (no skill) against candidate (the
        // pinned skill assembled) on the scripted scenario.
        let baseline = drive_recording("eval-skill-baseline", Vec::new()).await;
        let applied = drive_recording("eval-skill-candidate", vec![self.skill.clone()]).await;
        let rate = |snapshot: &JournalSnapshot| {
            if defect_count(snapshot) == 0 {
                1.0
            } else {
                0.0
            }
        };
        let (baseline_rate, candidate_rate) = (rate(&baseline), rate(&applied));
        let report = |pass_rate: f64| {
            json!({
                "format_version": 1,
                "name": format!("refunds@{}", request.dataset_version),
                "dataset_version": request.dataset_version,
                "summary": {"run_pass_rate": pass_rate},
            })
        };
        Ok(CandidateEvaluation {
            candidate_id: candidate.candidate_id.clone(),
            dataset_version: request.dataset_version.clone(),
            replay: ReplaySummary {
                fixture_ids: vec![self.defective.run_id.clone()],
                matched: 1,
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
            evaluated_at: ts(1_750_200_001_000),
        })
    }
}

/// The correction loop's derived record (R0.8 wave 2's shape): the
/// correction becomes an attributed example at its scope — agent scope, so
/// a candidate pending evaluation — journaled through the memory write
/// seam with the correction's attribution in its provenance.
fn correction_record(correction: &Correction) -> MemoryRecord {
    assert!(
        correction.is_candidate(),
        "agent scope is the candidate path"
    );
    MemoryRecord::new(
        MemoryKind::Example,
        correction.scope.clone(),
        MemoryProvenance {
            author: correction.author_as_provenance(),
            evidence: correction.evidence(),
            written_at: ts(1_750_200_000_500),
        },
        1.0,
        ValidityWindow::starting(ts(1_750_200_000_000)),
        ts(1_750_200_000_500),
        correction.corrected.clone(),
    )
    .unwrap()
    .with_candidacy(Candidacy::Pending)
}

#[tokio::test]
async fn release_proof_trajectory_to_promoted_skill_and_back() {
    // ---------------------------------------------------------------
    // 1. The defective run: a scripted agent with the planted defect
    //    (refund issued with no `reason`) runs and journals.
    // ---------------------------------------------------------------
    let defective = drive_recording("run-defective", Vec::new()).await;
    assert_eq!(
        defect_count(&defective),
        1,
        "the planted defect is present in the recorded run"
    );
    assert_eq!(
        final_answer(&defective),
        "refund r-1 issued for o-1, no reason"
    );
    let defective_call = events_of_kind(&defective, RunEventKind::ToolCall)
        .into_iter()
        .next()
        .expect("the defective run journaled its tool call")
        .id
        .clone();

    // ---------------------------------------------------------------
    // 2. The correction lands through the shipped correction loop: an
    //    attributed example record at agent scope (a candidate pending
    //    evaluation), journaled as a memory write into the learning
    //    journal the whole lifecycle shares.
    // ---------------------------------------------------------------
    let learn_journal = Journal::new("learn-run-w4", "learn-thread-w4", logical_clock());
    let store: Arc<dyn MemoryStore> = Arc::new(InMemoryMemoryStore::new());
    let memory = learn_journal.memory(MemorySource::Store(store));
    let correction = Correction {
        target: CorrectionTarget::RunEvent {
            run_id: defective.run_id.clone(),
            event_id: defective_call.clone(),
        },
        ..fixture_correction()
    };
    let record = correction_record(&correction);
    memory.write(&record, None).await.unwrap();
    let correction_write = events_of_kind(&learn_journal.snapshot(), RunEventKind::MemoryWrite)
        .into_iter()
        .next()
        .expect("the correction journaled as a memory write")
        .id
        .clone();

    // ---------------------------------------------------------------
    // 3. Distillation: the recorded trajectory plus the correction
    //    distill into a candidate skill whose package passes the skill
    //    plane's own validation and scan. Creation journals, parented to
    //    the correction's write.
    // ---------------------------------------------------------------
    let distilled = distill_skill(&DistillRequest {
        trajectories: vec![defective.clone()],
        corrections: vec![correction.clone()],
        ..fixture_request()
    })
    .unwrap();
    assert!(distilled.scan.is_clean());
    let candidate = distilled.candidate.clone();
    let mut registry = SkillRegistry::new();
    let registration = registry
        .register(
            distilled.package.clone(),
            SkillSource::Registry {
                name: "distilled".into(),
            },
            "trajectory-distiller",
        )
        .unwrap();
    assert!(!registration.already_registered);
    let created = learn_journal.record(
        EventDraft::new(RunEventKind::CandidateCreated, Effect::Idempotent)
            .input(json!({
                "effect_key": candidate_effect_key(&candidate.candidate_id),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(&candidate).unwrap())
            .parent(&correction_write),
    );

    // ---------------------------------------------------------------
    // 4. Evaluation through the seam: exact replay of the recorded
    //    evidence (byte-identical, zero outbound calls) plus the
    //    scripted experiment — the defect class disappears under the
    //    candidate.
    // ---------------------------------------------------------------
    let evaluator = SkillEvaluator {
        defective: defective.clone(),
        skill: skill_entry(&registration.version),
    };
    let request = EvaluationRequest {
        dataset_version: "refunds-v1".into(),
        target_metric: "run_pass_rate".into(),
        thresholds: EvaluationThresholds::default(),
        replay_evidence: vec![defective.clone()],
    };
    let evaluation = evaluator.evaluate(&candidate, &request).await.unwrap();
    // The replay half's load is carried inside `evaluate` by the sentinel
    // counts and the event/head-hash equality; here assert the summary's
    // substance, not its derived predicate.
    assert!(
        evaluation.replay.divergences.is_empty(),
        "no fixture diverged: the evidence reproduces itself"
    );
    assert_eq!(evaluation.replay.matched, 1);
    assert_eq!(evaluation.verdict.baseline, Some(0.0));
    assert_eq!(evaluation.verdict.candidate, Some(1.0));
    assert_eq!(evaluation.verdict.delta, Some(1.0));
    assert!(!evaluation.verdict.regressed);
    let evaluated = learn_journal.record(
        EventDraft::new(RunEventKind::CandidateEvaluated, Effect::Idempotent)
            .input(json!({
                "effect_key": evaluation_effect_key(&candidate.candidate_id, "refunds-v1"),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(&evaluation).unwrap())
            .parent(&created),
    );

    // ---------------------------------------------------------------
    // 5. Scoped-approval promotion through the shipped gate: the
    //    envelope holds `skill` at Approval; the token scoped to this
    //    candidate's promotion effect id admits it. The pointer moves on
    //    `skill:refund-with-reason`; re-registering the package at
    //    promotion is idempotent on the content address.
    // ---------------------------------------------------------------
    let envelope = PromotionEnvelope::r08_default();
    let refused = admit_promotion(&envelope, &candidate, Some(&evaluation), None);
    assert!(
        matches!(
            refused,
            Err(LearnError::Refused(
                PromotionRefusal::RequiresApproval { .. }
            ))
        ),
        "a skill never promotes on evidence alone: {refused:?}"
    );
    let token = ApprovalToken::approve(promotion_effect_id(&candidate), "ops:amjad");
    let decision = admit_promotion(&envelope, &candidate, Some(&evaluation), Some(&token)).unwrap();
    let receipt = PromotionReceipt {
        candidate_id: candidate.candidate_id.clone(),
        surface: candidate.surface(),
        previous: None,
        decision,
        promoted_at: ts(1_750_200_002_000),
    };
    let pointer = VersionPointer::new(candidate.surface()).promoted(&receipt);
    assert_eq!(pointer.active, Some(candidate.candidate_id.clone()));
    let promoted = learn_journal.record(
        EventDraft::new(RunEventKind::CandidatePromoted, Effect::Idempotent)
            .input(json!({
                "effect_key": promotion_effect_key(&candidate.candidate_id),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(&receipt).unwrap())
            .parent(&evaluated),
    );
    let re_registered = registry
        .register(
            distilled.package.clone(),
            SkillSource::Registry {
                name: "distilled".into(),
            },
            "trajectory-distiller",
        )
        .unwrap();
    assert!(
        re_registered.already_registered,
        "promotion re-registration is idempotent on the content address"
    );

    // ---------------------------------------------------------------
    // 6. The improved run: admission resolves the pointer THROUGH THE
    //    SKILLS PLANE'S REAL RESOLVER — `resolve_active_skill` over a
    //    candidate-store lookup (`skill_pin` is the extraction), then
    //    `skill_section_entries` assembles the section. The defect class
    //    disappears; the journaled model call's manifest pins the
    //    promoted revision.
    // ---------------------------------------------------------------
    assert_eq!(
        pointer.active,
        Some(candidate.candidate_id.clone()),
        "the pointer moved"
    );
    let candidates: std::collections::BTreeMap<_, _> =
        [(candidate.candidate_id.clone(), candidate.clone())]
            .into_iter()
            .collect();
    let pin = |id: &rusty_agent_runtime::learn::CandidateId| candidates.get(id).and_then(skill_pin);
    let resolved = resolve_active_skill(&registry, &pointer, "run-improved", &pin)
        .expect("the promoted candidate resolves")
        .expect("the promoted pointer binds the skill");
    let skill_name = resolved.name.clone();
    let pinned_hash = resolved.content_hash.clone();
    let active_set = ActiveSkillSet::new(vec![resolved]);
    let catalog = vec![SkillCatalogEntry {
        metadata: registry
            .get(&skill_name)
            .expect("the promoted skill is registered")
            .metadata(),
        binding: pin(&candidate.candidate_id)
            .expect("the pin extracts")
            .binding,
    }];
    let selection = select_skills(
        &SkillSelectionFeatures::default(),
        &catalog,
        &SkillSelectionPolicy::default(),
    );
    let entries = skill_section_entries(&registry, &selection, &active_set).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].body.is_some(),
        "the active skill's tier-2 body loads"
    );
    let improved = drive_recording("run-improved", entries).await;
    assert_eq!(
        defect_count(&improved),
        0,
        "the defect class disappears under the promoted skill"
    );
    assert_eq!(
        final_answer(&improved),
        "refund r-1 issued for o-1, reason recorded"
    );
    let improved_call = events_of_kind(&improved, RunEventKind::ModelCall)
        .into_iter()
        .next()
        .expect("the improved run journaled its model calls");
    let assembled = match &improved_call.input {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline model-call request, got {other:?}"),
    };
    let manifest = manifest_of(&assembled);
    let skills_section = manifest
        .sections
        .iter()
        .find(|section| section.kind == SectionKind::Skills)
        .expect("the improved run assembled a skills section");
    assert_eq!(
        skills_section.ids,
        vec![format!("{skill_name}@1:{pinned_hash}")],
        "the manifest pins the promoted skill revision"
    );

    // ---------------------------------------------------------------
    // 7. The attribution chain, asserted by walking ids: the improved
    //    run's journal names the promoted revision; the promotion names
    //    the candidate and its approver; the evaluation names the
    //    candidate; the candidate's evidence span names the correction
    //    and the recorded run; creation hangs off the correction's
    //    journaled write; the correction names the defective event.
    // ---------------------------------------------------------------
    assert_eq!(pinned_hash, distilled.package.content_hash());
    let lifecycle = learn_journal.snapshot();
    let write_event = &lifecycle.events[0];
    let created_event = &lifecycle.events[1];
    let evaluated_event = &lifecycle.events[2];
    let promoted_event = &lifecycle.events[3];
    assert_eq!(write_event.kind, RunEventKind::MemoryWrite);
    assert_eq!(created_event.id, created);
    assert_eq!(created_event.kind, RunEventKind::CandidateCreated);
    assert_eq!(
        created_event.parent.as_deref(),
        Some(correction_write.as_str())
    );
    let created_output = match &created_event.output {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline candidate payload, got {other:?}"),
    };
    assert_eq!(
        created_output["candidate_id"].as_str().unwrap(),
        candidate.candidate_id.as_str()
    );
    assert_eq!(evaluated_event.parent.as_deref(), Some(created.as_str()));
    let evaluated_output = match &evaluated_event.output {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline evaluation payload, got {other:?}"),
    };
    assert_eq!(
        evaluated_output["candidate_id"].as_str().unwrap(),
        candidate.candidate_id.as_str()
    );
    assert_eq!(promoted_event.parent.as_deref(), Some(evaluated.as_str()));
    let promoted_output = match &promoted_event.output {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline promotion payload, got {other:?}"),
    };
    assert_eq!(
        promoted_output["candidate_id"].as_str().unwrap(),
        candidate.candidate_id.as_str()
    );
    assert_eq!(
        promoted_output["surface"].as_str().unwrap(),
        "skill:refund-with-reason"
    );
    assert_eq!(
        promoted_output["decision"]["authority"]["approved_by"]
            .as_str()
            .unwrap(),
        "ops:amjad"
    );
    // The correction's journaled write carries its attribution, and its
    // target is the defective run's defective call.
    let write_output = match &write_event.output {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected an inline memory record, got {other:?}"),
    };
    assert_eq!(
        write_output["provenance"]["evidence"]["correction_id"]
            .as_str()
            .unwrap(),
        "correction-1"
    );
    assert_eq!(
        correction.attribution(),
        "human:amjad via correction:correction-1"
    );
    let CorrectionTarget::RunEvent { run_id, event_id } = &correction.target else {
        panic!("the correction targets a journaled run event");
    };
    assert_eq!(run_id, &defective.run_id);
    assert_eq!(event_id, &defective_call);
    assert_eq!(candidate.evidence.run_ids, vec![defective.run_id.clone()]);
    assert_eq!(
        candidate.evidence.correction_ids,
        vec!["correction-1".to_string()]
    );

    // ---------------------------------------------------------------
    // 8. Rollback: the pointer re-points to the static floor (no skill),
    //    the rollback journals off the promotion, and the defective run
    //    returns byte-exact — same run id, same clock, no skill bound.
    // ---------------------------------------------------------------
    let rollback = RollbackReceipt {
        surface: candidate.surface(),
        from: candidate.candidate_id.clone(),
        to: None,
        cause: "operator review: skill text pending re-review".into(),
        rolled_back_at: ts(1_750_200_003_000),
    };
    let pointer = pointer.rolled_back(&rollback);
    assert!(pointer.active.is_none(), "the static version serves again");
    learn_journal.record(
        EventDraft::new(RunEventKind::CandidateRolledBack, Effect::Idempotent)
            .input(json!({
                "effect_key": rollback_effect_key(&candidate.surface(), &candidate.candidate_id),
                "candidate_id": candidate.candidate_id,
            }))
            .output(serde_json::to_value(&rollback).unwrap())
            .parent(&promoted),
    );
    let restored = drive_recording("run-defective", Vec::new()).await;
    assert_eq!(
        defect_count(&restored),
        1,
        "the defect behavior returns once the skill is rolled back"
    );
    assert_eq!(
        serde_json::to_string(&restored).unwrap(),
        serde_json::to_string(&defective).unwrap(),
        "the rolled-back run is byte-identical to the recorded defective run"
    );
}

#[test]
fn rollback_restores_the_prior_skill_candidate_byte_exactly() {
    // Byte-exactness at the pointer level (the wave-3 contract test's
    // pattern, on the skill surface): promote skill A then skill B, roll
    // back, and the re-pointed candidate's stored bytes are byte-identical
    // to A's — candidates are content-addressed and immutable, so the
    // pointer's `previous` IS the version that served.
    let a = fixture_distilled().candidate;
    let b = distill_skill(&DistillRequest {
        name: "refund-with-reason".into(),
        description: "Issue refunds with an explicit reason and a courtesy note.".into(),
        ..fixture_request()
    })
    .unwrap()
    .candidate;
    assert_ne!(a.candidate_id, b.candidate_id);
    assert_eq!(a.surface(), b.surface());

    let mut store = std::collections::HashMap::new();
    store.insert(a.candidate_id.to_string(), serde_json::to_vec(&a).unwrap());
    store.insert(b.candidate_id.to_string(), serde_json::to_vec(&b).unwrap());

    let promote_a = PromotionReceipt {
        candidate_id: a.candidate_id.clone(),
        surface: a.surface(),
        previous: None,
        decision: PromotionDecision {
            authority: PromotionAuthority::Approval {
                approved_by: "ops:amjad".into(),
            },
            canary: None,
        },
        promoted_at: ts(1_750_200_004_000),
    };
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

    let rolled = pointer.rolled_back(&RollbackReceipt {
        surface: a.surface(),
        from: b.candidate_id.clone(),
        to: promote_b.previous.clone(),
        cause: "drift monitor: refund failure-rate uptick on refunds@v1".into(),
        rolled_back_at: ts(1_750_200_006_000),
    });
    let restored_id = rolled.active.expect("rollback re-points to A");
    assert_eq!(restored_id, a.candidate_id);

    // Re-resolution returns the exact bytes that served before B, and a
    // fresh distillation of A's evidence re-derives the same id.
    let redistilled = fixture_distilled().candidate;
    assert_eq!(redistilled.candidate_id, restored_id);
    let restored_bytes = store
        .get(restored_id.as_str())
        .expect("the restored id resolves in the store");
    assert_eq!(*restored_bytes, serde_json::to_vec(&redistilled).unwrap());
    let restored: Candidate = serde_json::from_slice(restored_bytes).unwrap();
    restored.verify_address().unwrap();
}
