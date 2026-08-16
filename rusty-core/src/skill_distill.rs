//! The trajectory distiller (R0.13 wave 4): the reference implementation of
//! the flagship loop's distillation stage — successful run trajectories and
//! the R0.8 correction loop's attributed examples, read from journaled
//! evidence, distilled into a candidate [`SkillPackage`].
//!
//! The design doc is `docs/agent-core-design.md` ("The flagship loop:
//! trajectories into governed skills"). Its boundary rules govern everything
//! here:
//!
//! - **Application code, between runs.** The distiller runs outside any live
//!   run, over terminal evidence only (the observe-stage rule): completed
//!   runs' journals and the corrections recorded against them. It reads
//!   journals; it never writes one, and it never touches a store.
//! - **Deterministic.** No model call drafts anything here — the reference
//!   distiller is closed-form templating over the evidence, so equal inputs
//!   produce byte-equal packages, one content hash, and one candidate id.
//!   A smarter distiller (an LLM drafting prose) is an application-side
//!   replacement for this function, not a change to it.
//! - **Fail-closed through the skill plane's own validators.** The draft is
//!   constructed as text, then parsed by
//!   [`crate::skill::SkillPackage::from_markdown`] and scanned by
//!   [`crate::skill::scan_package`] — the same gates every other package
//!   author faces. A draft that fails validation or earns a scan denial
//!   **never becomes a candidate**: the error is the outcome.
//! - **Candidacy is the shipped contract.** The output candidate is the
//!   additive [`CandidateContent::Skill`] carrying `{ name, content_hash,
//!   binding }`, the content hash being the skill plane's own address, so
//!   candidate identity and package identity are one digest. Creation,
//!   evaluation, promotion, and rollback journal through the four existing
//!   candidate lifecycle event kinds; nothing here mints a journal event.
//!
//! What this module deliberately does not do: bind skills into runs (the
//! skills plane's shortlisting and gating), move pointers (the learn gate's
//! job, through [`crate::learn::admit_promotion`]), or evaluate (the
//! [`crate::learn::CandidateEvaluator`] seam's).

use chrono::{DateTime, Utc};
use serde_json::Value;
use thiserror::Error;

use crate::journal::JournalSnapshot;
use crate::learn::{Candidate, CandidateContent, EvidenceSpan};
use crate::memory::{Correction, CorrectionTarget, ProvenanceAuthor};
use crate::record::{EventStatus, PayloadRef, RunEventKind};
use crate::skill::{scan_package, ScanFinding, SkillError, SkillPackage};

/// Every way distillation can refuse to produce a candidate. Refusal is the
/// contract's center: a draft that cannot stand on its evidence, pass the
/// skill plane's parse, or clear its scan is an error, never a candidate
/// (fail-closed, the skill plane's own discipline).
#[derive(Debug, Error)]
pub enum SkillDistillError {
    /// No trajectories were supplied. A skill distilled from nothing is a
    /// guess; the reference distiller's input is journaled evidence.
    #[error(
        "no trajectories supplied: the reference distiller reads journaled evidence — \
         a skill distilled from nothing is a guess, and guesses do not become candidates"
    )]
    EmptyEvidence,

    /// A correction targets a journaled run event that is not in the
    /// supplied evidence. A candidate that cannot name its observations
    /// cannot be audited — the correction's target must be one of the
    /// trajectories being distilled.
    #[error(
        "correction `{correction_id}` targets `{run_id}:{event_id}`, which is not in the \
         supplied trajectories — the evidence span must contain what the correction corrects"
    )]
    OrphanCorrection {
        /// The correction whose target is missing.
        correction_id: String,
        /// The run the correction names.
        run_id: String,
        /// The journaled event the correction names.
        event_id: String,
    },

    /// The drafted package failed the skill plane's fail-closed parse
    /// ([`crate::skill::SkillPackage::from_markdown`]). The refusal travels
    /// verbatim: the skill plane's error already names the rule the draft
    /// broke.
    #[error("the drafted package failed validation: {0}")]
    Package(#[from] SkillError),

    /// The security scan reported denials; registration would fail closed,
    /// so candidacy fails first — a draft the scan denies never becomes a
    /// candidate.
    #[error("security scan denied the drafted package: {} denial(s)", denials.len())]
    ScanDenied {
        /// The denials that refused the draft.
        denials: Vec<ScanFinding>,
    },

    /// The candidate's content address could not be derived — unreachable
    /// for well-formed content, surfaced rather than panicked.
    #[error("candidate construction failed: {0}")]
    Candidate(String),
}

/// One journaled tool call as the distiller reads it: the trajectory's
/// atomic step. Extracted from [`RunEventKind::ToolCall`] events in journal
/// order — the canonical [`crate::replay::tool_call_request`] input shape
/// (`{"tool": …, "arguments": …}`) plus the journaled outcome status.
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryStep {
    /// The journaled event's id (`{run_id}:{seq}`).
    pub event_id: String,

    /// The tool the run called.
    pub tool: String,

    /// The model-supplied arguments, verbatim from the journaled request.
    pub arguments: Value,

    /// `true` when the call completed ([`EventStatus::Ok`]).
    pub ok: bool,
}

/// Extract one run's tool-call trajectory: every [`RunEventKind::ToolCall`]
/// event in `seq` order whose input carries the canonical request shape.
/// Events with a non-canonical input (a hand-journaled call outside the
/// recording wrappers' shape) are skipped — the trajectory is a reading of
/// the evidence, and a step the reader cannot name honestly is not a step;
/// the correction-integrity check in [`distill_skill`] is where unnamed
/// evidence fails closed.
pub fn trajectory_steps(snapshot: &JournalSnapshot) -> Vec<TrajectoryStep> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ToolCall)
        .filter_map(|event| {
            let input = match &event.input {
                Some(PayloadRef::Inline(value)) => value,
                _ => return None,
            };
            let tool = input.get("tool")?.as_str()?.to_owned();
            let arguments = input.get("arguments").cloned().unwrap_or(Value::Null);
            Some(TrajectoryStep {
                event_id: event.id.clone(),
                tool,
                arguments,
                ok: event.status == EventStatus::Ok,
            })
        })
        .collect()
}

/// What one distillation runs over: the evidence, the draft's identity, and
/// the candidate's attribution. Pure input — the distiller reads no clocks
/// and no stores; `created_at` arrives as a parameter so equal requests
/// converge on one candidate id (the learning plane's identity-is-integrity
/// rule).
#[derive(Debug, Clone)]
pub struct DistillRequest {
    /// The skill's name (kebab-case; validated by the skill plane's parse,
    /// fail-closed).
    pub name: String,

    /// The tier-1 discovery text. The distiller renders it verbatim; the
    /// skill plane's parse validates it.
    pub description: String,

    /// The completed runs whose journals the distiller reads (terminal
    /// evidence only — the caller selects completed runs; the distiller
    /// trusts the selection and names it in the evidence span).
    pub trajectories: Vec<JournalSnapshot>,

    /// The R0.8 corrections the draft folds in. A correction targeting a
    /// run event must name an event inside `trajectories` — the orphan
    /// check fails closed otherwise.
    pub corrections: Vec<Correction>,

    /// The run-facing binding declaration, when the distiller declares one
    /// — carried opaquely into [`CandidateContent::Skill`]'s `binding`
    /// member while the binding schema is the skills plane's.
    pub binding: Option<Value>,

    /// The distiller's identity (mandatory — journaled with creation).
    pub distilled_by: ProvenanceAuthor,

    /// The distillation instant (a parameter, not a clock read).
    pub created_at: DateTime<Utc>,
}

/// The distiller's output: the validated package and the candidate that
/// names it. Both halves travel together because promotion needs both — the
/// gate moves the pointer on the candidate, and registering the package is
/// what makes the candidate's content hash resolvable.
#[derive(Debug, Clone)]
pub struct DistilledSkill {
    /// The drafted package, validated and scanned (denial-free by
    /// construction — a denied draft is an error, not an output).
    pub package: SkillPackage,

    /// The journaled scan outcome (warnings travel as evidence; there are
    /// no denials by construction).
    pub scan: crate::skill::ScanReport,

    /// The candidate naming the package's content hash. Identity is the
    /// content's: two distillations of the same evidence converge on one
    /// candidate id.
    pub candidate: Candidate,
}

/// Render one trajectory step as its procedure line. Compact JSON is
/// canonical (object keys sort deterministically), so the line is a pure
/// function of the journaled step.
fn step_line(index: usize, step: &TrajectoryStep) -> String {
    let arguments = serde_json::to_string(&step.arguments).unwrap_or_else(|_| "null".to_owned());
    let outcome = if step.ok { "ok" } else { "error" };
    format!(
        "{index}. Call `{tool}` with `{arguments}` → {outcome} (`{event_id}`)",
        index = index,
        tool = step.tool,
        event_id = step.event_id,
    )
}

/// Render the draft `SKILL.md` from the evidence: frontmatter (name,
/// description, the trajectory's tool set as advisory `allowed-tools`),
/// then the body — one procedure section per trajectory run, then the
/// corrections the draft folds in, each with its attribution string. Fully
/// deterministic: trajectories render sorted by run id, steps in journal
/// order, corrections sorted by correction id, tools sorted and deduped.
fn render_skill_md(request: &DistillRequest) -> String {
    let mut trajectories: Vec<&JournalSnapshot> = request.trajectories.iter().collect();
    trajectories.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    // The same run's journal supplied twice is the same evidence twice:
    // dedupe by run id so evidence multiplicity never moves the draft (the
    // convergence rule — equal evidence distills to one candidate).
    trajectories.dedup_by(|a, b| a.run_id == b.run_id);
    let mut corrections: Vec<&Correction> = request.corrections.iter().collect();
    corrections.sort_by(|a, b| a.correction_id.cmp(&b.correction_id));

    let mut tools: Vec<String> = Vec::new();
    let mut bodies: Vec<(&str, Vec<TrajectoryStep>)> = Vec::new();
    for snapshot in trajectories {
        let steps = trajectory_steps(snapshot);
        for step in &steps {
            if !tools.contains(&step.tool) {
                tools.push(step.tool.clone());
            }
        }
        bodies.push((&snapshot.run_id, steps));
    }
    tools.sort();

    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", request.name));
    out.push_str(&format!("description: {}\n", request.description));
    if !tools.is_empty() {
        out.push_str(&format!("allowed-tools: {}\n", tools.join(", ")));
    }
    out.push_str("---\n");
    out.push_str(&format!("# {}\n\n{}\n", request.name, request.description));

    out.push_str("\n## Procedure\n");
    for (run_id, steps) in &bodies {
        out.push_str(&format!(
            "\nDistilled from run `{run_id}` ({} journaled tool call(s)):\n",
            steps.len()
        ));
        for (index, step) in steps.iter().enumerate() {
            out.push('\n');
            out.push_str(&step_line(index + 1, step));
        }
        out.push('\n');
    }

    if !corrections.is_empty() {
        out.push_str("\n## Corrections\n");
        for correction in corrections {
            let corrected =
                serde_json::to_string(&correction.corrected).unwrap_or_else(|_| "null".to_owned());
            out.push_str(&format!("\n- {}: {corrected}", correction.attribution(),));
            if let Some(rationale) = &correction.rationale {
                out.push_str(&format!(" — {rationale}"));
            }
            out.push('\n');
        }
    }
    out
}

/// The correction-integrity check: every correction targeting a journaled
/// run event must name a run and event inside the supplied trajectories.
/// Memory- and prompt-targeted corrections name their evidence differently
/// (content addresses, not run events) and pass here; their linkage rides
/// in the candidate's evidence span.
fn check_correction_targets(request: &DistillRequest) -> Result<(), SkillDistillError> {
    for correction in &request.corrections {
        if let CorrectionTarget::RunEvent { run_id, event_id } = &correction.target {
            let found = request.trajectories.iter().any(|snapshot| {
                snapshot.run_id == *run_id
                    && snapshot.events.iter().any(|event| event.id == *event_id)
            });
            if !found {
                return Err(SkillDistillError::OrphanCorrection {
                    correction_id: correction.correction_id.clone(),
                    run_id: run_id.clone(),
                    event_id: event_id.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Distill journaled trajectories and corrections into a candidate skill.
///
/// The pipeline, in order: integrity (correction targets must name the
/// evidence), draft (deterministic rendering), validate (the skill plane's
/// fail-closed parse), scan (denials refuse), candidacy (the package's
/// content hash becomes the candidate's). The candidate's evidence span
/// names exactly what was read — the trajectory run ids and the correction
/// ids, sorted and deduped, so the audit walk from candidate to evidence is
/// a lookup, not a search.
pub fn distill_skill(request: &DistillRequest) -> Result<DistilledSkill, SkillDistillError> {
    if request.trajectories.is_empty() {
        return Err(SkillDistillError::EmptyEvidence);
    }
    check_correction_targets(request)?;

    let package = SkillPackage::from_markdown(&render_skill_md(request))?;
    let scan = scan_package(&package);
    if scan.has_denials() {
        return Err(SkillDistillError::ScanDenied {
            denials: scan.denials().cloned().collect(),
        });
    }

    let mut run_ids: Vec<String> = request
        .trajectories
        .iter()
        .map(|snapshot| snapshot.run_id.clone())
        .collect();
    run_ids.sort();
    run_ids.dedup();
    let mut correction_ids: Vec<String> = request
        .corrections
        .iter()
        .map(|correction| correction.correction_id.clone())
        .collect();
    correction_ids.sort();
    correction_ids.dedup();

    let candidate = Candidate::new(
        CandidateContent::Skill {
            name: package.name().to_owned(),
            content_hash: package.content_hash(),
            binding: request.binding.clone(),
        },
        request.distilled_by.clone(),
        EvidenceSpan {
            run_ids,
            correction_ids,
            memory_ids: Vec::new(),
        },
        request.created_at,
    )
    .map_err(|error| SkillDistillError::Candidate(error.to_string()))?;

    Ok(DistilledSkill {
        package,
        scan,
        candidate,
    })
}
