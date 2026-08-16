//! Tool-call outcome learning (R0.13 agent core, wave 3): the derived
//! outcome roll-up over journaled `ToolCall` events, the argument-repair
//! distiller, and the candidate constructor that promotes learned selection
//! overlays through the shipped `tool_contract` gate.
//!
//! The design doc is `docs/agent-core-design.md` ("Tool selection and
//! calling optimization", wave 3). The discipline is wave 2's derived-index
//! discipline applied to tool evidence: pure functions over completed
//! journals, deterministic, rebuildable byte-identically; nothing here
//! mutates a journal, adds an event kind, or re-opens a shipped contract.
//!
//! # The two-tier failure contract (N10)
//!
//! A journaled [`RunEventKind::ToolCall`]'s outcome is classified by
//! **payload, never by status alone**:
//!
//! - **Structured refusals** — [`crate::tool_select::ValidatingTool`]
//!   returns its violation payload as an ordinary `Ok` result, so a refusal
//!   journals with [`EventStatus::Ok`] and the
//!   `ERROR: {"kind":"argument_validation","violations":[…]}` string as its
//!   output. The roll-up recognizes it with the shipped
//!   [`parse_argument_validation_refusal`] — the one structured contract.
//! - **Opaque failures** — every other failure: a tool error journaled
//!   with [`EventStatus::Error`] (output `{"error": …}`), or an
//!   `ERROR: …`-prefixed string that is not the structured kind (the
//!   failure-isolation channel's shape). Classed by nothing more than its
//!   tool and its prefix — one writer of structure, never parsed free-form
//!   prose.
//! - **Successes** — everything else with [`EventStatus::Ok`].
//!
//! `Interrupted` tool calls are excluded: a suspended run is not terminal
//! evidence of tool quality (the utility index's rule, restated).
//!
//! # What the roll-up produces
//!
//! [`build_outcome_index`] rolls completed runs' journals into a
//! [`ToolOutcomeIndex`]: per tool, the [`ToolOutcomeStats`] the selection
//! layer consumes ([`ToolOutcomeIndex::selection_snapshot`] feeds
//! [`crate::tool_select::SelectionFeatures::outcomes`] directly), the opaque
//! failure count, latency samples with nearest-rank percentiles, per
//! argument-pattern-digest outcomes ([`argument_pattern_digest`] — structure,
//! never values), and the violation clusters the argument-repair distiller
//! reads ([`distill_argument_guidance`]: recurring `(path, rule)` patterns →
//! deterministic, human-reviewable guidance).
//!
//! # The learning half
//!
//! A learned overlay becomes a candidate through [`selection_candidate`]:
//! `CandidateContent::ToolContract { tool, schema, selection: Some(overlay) }`
//! — the additive optional `selection` member wave 3 lands on the shipped
//! shape (absent from the wire while unset, so pre-wave artifacts keep their
//! content addresses byte-for-byte). The candidate flows through the shipped
//! machinery unchanged: evaluation, [`crate::learn::admit_promotion`], the
//! journaled pointer move, byte-exact rollback. The selection *policy*
//! (cutoff, `k`, weights) is assembly policy and promotes as the tools
//! section of a `context` candidate — never per-tool, per the design's home
//! decision.
//!
//! **Server seam (later wave).** The durable roll-up task kind and the
//! derived-index storage on both backends are queued behind this wave (the
//! design doc's `rusty-server` coordination entry). This module ships the
//! pure roll-up, the distiller, and the candidate constructor only; a server
//! task calling [`build_outcome_index`] on a schedule and persisting the
//! index is the whole remaining half.
//!
//! Golden-file tests under `tests/golden/` pin every wire shape this module
//! adds; any accidental drift fails CI.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::journal::JournalSnapshot;
use crate::learn::{Candidate, CandidateContent, EvidenceSpan};
use crate::memory::ProvenanceAuthor;
use crate::record::{sha256_hex, EventStatus, PayloadRef, RunEvent};
use crate::tool_select::{
    parse_argument_validation_refusal, ArgumentViolation, ToolOutcomeStats, ToolSelectionOverlay,
    ERROR_PREFIX,
};

fn invalid(message: impl Into<String>) -> RustyError {
    // Roll-up and distiller failures are inconsistent-journal or contract
    // validation errors; the invalid-update class covers them without
    // growing the error taxonomy (the memory module's convention).
    RustyError::InvalidUpdate(message.into())
}

// --------------------------------------------------------------------- //
// The argument-pattern digest
// --------------------------------------------------------------------- //

/// A digest of an argument set's *structure* — object keys and JSON types,
/// recursively, never values — so calls group by shape: `{"query": "rust"}`
/// and `{"query": "cargo"}` share one pattern; `{"query": 5}` is another.
/// `sha256` over the canonical shape serialization (object keys sort
/// deterministically), the one hashing primitive the journal heads and
/// content addresses already share.
pub fn argument_pattern_digest(arguments: &Value) -> String {
    let shape = argument_shape(arguments);
    let bytes = serde_json::to_vec(&shape)
        .expect("serializing an argument shape cannot fail: it is a plain JSON value");
    sha256_hex(&bytes)
}

fn argument_shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), argument_shape(item)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(argument_shape).collect())
        }
        Value::Null => Value::String("null".to_owned()),
        Value::Bool(_) => Value::String("boolean".to_owned()),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            Value::String("integer".to_owned())
        }
        Value::Number(_) => Value::String("number".to_owned()),
        Value::String(_) => Value::String("string".to_owned()),
    }
}

// --------------------------------------------------------------------- //
// The outcome index: derived, rebuildable, never on the tool
// --------------------------------------------------------------------- //

/// The derived tool-outcome index: per-tool roll-ups over completed runs'
/// journals, stamped with the instant the roll-up read as-of. **Derived,
/// never stored on the tool contract** — the index is a disposable
/// projection: rebuilding it from the same journals and stamp reproduces it
/// byte-identically (the checkpoint/artifact discipline applied to an
/// index, wave 2's rule for the utility index restated here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcomeIndex {
    /// The roll-up's as-of stamp — the pin a consumer (and an auditor)
    /// reads to know which snapshot moved selection.
    pub stamp: DateTime<Utc>,

    /// Per-tool roll-ups, keyed by tool name (a `BTreeMap`: the serialized
    /// shape is deterministic).
    pub tools: BTreeMap<String, ToolRollup>,
}

impl ToolOutcomeIndex {
    /// The selection layer's input: per-tool [`ToolOutcomeStats`] for every
    /// tool with recorded history — exactly the shape
    /// [`crate::tool_select::SelectionFeatures::outcomes`] consumes. A tool
    /// with no recorded history appears nowhere and contributes nothing
    /// (the wave-1 rule).
    pub fn selection_snapshot(&self) -> BTreeMap<String, ToolOutcomeStats> {
        self.tools
            .iter()
            .map(|(tool, rollup)| (tool.clone(), rollup.stats.clone()))
            .collect()
    }

    /// One tool's roll-up, when the index has seen it.
    pub fn rollup_for(&self, tool: &str) -> Option<&ToolRollup> {
        self.tools.get(tool)
    }
}

/// One tool's rolled-up outcomes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolRollup {
    /// The wave-1 consumption shape: total calls, successes, and
    /// validation failures (parsed from the structured contract).
    pub stats: ToolOutcomeStats,

    /// Failures outside the structured contract — tool errors and
    /// unparseable `ERROR:` strings, counted and otherwise opaque.
    pub opaque_failures: u64,

    /// Journaled latencies, sorted ascending (raw samples, the utility
    /// index's raw-counts discipline: the index stores honest evidence and
    /// the summarization lives in exactly one accessor).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latencies_ms: Vec<u64>,

    /// Per argument-pattern-digest outcomes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub patterns: BTreeMap<String, PatternOutcome>,

    /// The recurring violation patterns, sorted by `(path, rule)` — the
    /// argument-repair distiller's input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<ViolationCluster>,
}

impl ToolRollup {
    /// The nearest-rank latency percentile (`p` in `1..=100`), in
    /// milliseconds; `None` when no latency was journaled. Integer
    /// arithmetic, byte-reproducible everywhere.
    pub fn latency_percentile(&self, p: u32) -> Option<u64> {
        let n = self.latencies_ms.len() as u64;
        if n == 0 {
            return None;
        }
        let rank = (u64::from(p.clamp(1, 100)) * n).div_ceil(100);
        Some(self.latencies_ms[(rank - 1) as usize])
    }
}

/// One argument pattern's outcomes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternOutcome {
    /// Total journaled calls with this argument shape.
    pub calls: u64,
    /// Calls whose outcome was a success.
    pub successes: u64,
    /// Calls refused by argument validation.
    pub validation_failures: u64,
    /// Calls that failed opaquely.
    pub opaque_failures: u64,
}

impl PatternOutcome {
    /// Success rate in basis points (0..=10000); `None` with no evidence.
    pub fn success_bps(&self) -> Option<u32> {
        if self.calls == 0 {
            return None;
        }
        Some((self.successes.saturating_mul(10_000) / self.calls) as u32)
    }
}

/// A recurring violation pattern: one `(path, rule)` pair, its count, and
/// a representative message (the first journaled one — evidence order is
/// the determinism contract, see [`build_outcome_index`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViolationCluster {
    /// JSON Pointer to the offending value (`""` is the arguments root).
    pub path: String,
    /// The schema rule violated, snake_case.
    pub rule: String,
    /// Journaled occurrences.
    pub count: u64,
    /// A representative violation message.
    pub example_message: String,
}

/// The per-event classification (N10): payload first, status second.
enum CallOutcome {
    Success,
    ValidationFailure(Vec<ArgumentViolation>),
    OpaqueFailure,
}

/// Roll completed runs' journals into the tool-outcome index at `stamp`.
///
/// Pure over its inputs; equal inputs produce a byte-identical index.
/// Determinism contract: runs are read in slice order and events in
/// journal sequence order, so the first-seen violation message a
/// [`ViolationCluster`] carries is stable. A `ToolCall` event whose input
/// or output payload is missing, unresolvable, or not the shipped
/// [`crate::replay::tool_call_request`] shape is an inconsistent journal
/// and fails loud — the [`build_utility_index`](crate::memory_tiers::build_utility_index)
/// rule.
pub fn build_outcome_index(runs: &[&JournalSnapshot], stamp: DateTime<Utc>) -> Result<ToolOutcomeIndex> {
    let mut tools: BTreeMap<String, ToolRollup> = BTreeMap::new();
    for snapshot in runs {
        for event in &snapshot.events {
            if event.kind != crate::record::RunEventKind::ToolCall {
                continue;
            }
            if event.status == EventStatus::Interrupted {
                // A suspended call is not terminal evidence.
                continue;
            }
            let (tool, arguments) = tool_call_parts(snapshot, event)?;
            let output = resolve_payload(snapshot, event.output.as_ref(), event.seq, "output")?;
            let outcome = classify(event.status, &output);

            let rollup = tools.entry(tool).or_default();
            rollup.stats.calls += 1;
            let pattern = rollup
                .patterns
                .entry(argument_pattern_digest(&arguments))
                .or_default();
            pattern.calls += 1;
            match outcome {
                CallOutcome::Success => {
                    rollup.stats.successes += 1;
                    pattern.successes += 1;
                }
                CallOutcome::OpaqueFailure => {
                    rollup.opaque_failures += 1;
                    pattern.opaque_failures += 1;
                }
                CallOutcome::ValidationFailure(violations) => {
                    rollup.stats.validation_failures += 1;
                    pattern.validation_failures += 1;
                    for violation in violations {
                        let cluster = rollup
                            .violations
                            .iter_mut()
                            .find(|c| c.path == violation.path && c.rule == violation.rule);
                        match cluster {
                            Some(cluster) => cluster.count += 1,
                            None => rollup.violations.push(ViolationCluster {
                                path: violation.path,
                                rule: violation.rule,
                                count: 1,
                                example_message: violation.message,
                            }),
                        }
                    }
                }
            }
            if let Some(latency) = event.latency_ms {
                rollup.latencies_ms.push(latency);
            }
        }
    }
    for rollup in tools.values_mut() {
        rollup.latencies_ms.sort_unstable();
        rollup.violations.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.rule.cmp(&b.rule)));
    }
    Ok(ToolOutcomeIndex { stamp, tools })
}

/// Classify one journaled call: the structured refusal contract is
/// recognized by **payload parsing** — refusals journal with
/// [`EventStatus::Ok`], so status alone cannot see them. Every other
/// failure is an opaque error string or an `Error` status.
fn classify(status: EventStatus, output: &Value) -> CallOutcome {
    if let Some(text) = output.as_str() {
        if let Some(violations) = parse_argument_validation_refusal(text) {
            return CallOutcome::ValidationFailure(violations);
        }
        if text.starts_with(ERROR_PREFIX) {
            return CallOutcome::OpaqueFailure;
        }
    }
    match status {
        EventStatus::Error => CallOutcome::OpaqueFailure,
        _ => CallOutcome::Success,
    }
}

/// Resolve a journaled payload through the snapshot's artifact map, failing
/// loud on an absent or dangling reference (the journal-is-inconsistent
/// rule, shared with the utility index's `assembly_ids`).
fn resolve_payload(
    snapshot: &JournalSnapshot,
    payload: Option<&PayloadRef>,
    seq: u64,
    which: &str,
) -> Result<Value> {
    let payload = payload.ok_or_else(|| {
        invalid(format!(
            "journaled tool call at seq {seq} carries no {which} payload — the journal is \
             inconsistent"
        ))
    })?;
    match payload {
        PayloadRef::Inline(value) => Ok(value.clone()),
        PayloadRef::Artifact(reference) => snapshot
            .artifacts
            .get(&reference.sha256)
            .cloned()
            .ok_or_else(|| {
                invalid(format!(
                    "journaled tool call at seq {seq} references artifact {}, which the \
                     snapshot does not hold — the journal is inconsistent",
                    reference.sha256
                ))
            }),
    }
}

/// The tool name and arguments of a journaled call, parsed from the shipped
/// [`crate::replay::tool_call_request`] input shape.
fn tool_call_parts(snapshot: &JournalSnapshot, event: &RunEvent) -> Result<(String, Value)> {
    let input = resolve_payload(snapshot, event.input.as_ref(), event.seq, "input")?;
    let tool = input
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            invalid(format!(
                "journaled tool call at seq {} has no `tool` string in its input payload — \
                 not the shipped tool_call_request shape",
                event.seq
            ))
        })?
        .to_owned();
    let arguments = input.get("arguments").cloned().unwrap_or(Value::Null);
    Ok((tool, arguments))
}

// --------------------------------------------------------------------- //
// The argument-repair distiller
// --------------------------------------------------------------------- //

/// One piece of argument-repair guidance: a recurring violation pattern on
/// one tool, distilled into a deterministic, human-reviewable note. Feeds
/// correction examples, prompt candidates, or a `when_to_use` note on a
/// selection overlay — what an application builds from it is application
/// code (the R0.8 boundary); the distiller owns the derivation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgumentGuidance {
    /// The tool whose calls fail this way.
    pub tool: String,
    /// JSON Pointer to the offending value (`""` is the arguments root).
    pub path: String,
    /// The schema rule violated, snake_case.
    pub rule: String,
    /// Journaled occurrences the guidance rests on.
    pub occurrences: u64,
    /// The distilled guidance: a pure function of the rule and the path.
    pub guidance: String,
}

/// Distill recurring violation patterns into argument-repair guidance: one
/// [`ArgumentGuidance`] per `(tool, path, rule)` cluster with at least
/// `min_occurrences` journaled occurrences, ordered by tool, then path,
/// then rule. Sparse evidence produces no guidance — never a confident
/// guess (the wave-2 margin-gate discipline).
pub fn distill_argument_guidance(
    index: &ToolOutcomeIndex,
    min_occurrences: u64,
) -> Vec<ArgumentGuidance> {
    let mut guidance = Vec::new();
    for (tool, rollup) in &index.tools {
        for cluster in &rollup.violations {
            if cluster.count < min_occurrences {
                continue;
            }
            guidance.push(ArgumentGuidance {
                tool: tool.clone(),
                path: cluster.path.clone(),
                rule: cluster.rule.clone(),
                occurrences: cluster.count,
                guidance: guidance_text(&cluster.rule, &cluster.path),
            });
        }
    }
    guidance
}

/// The deterministic guidance text per rule. No clocks, no addresses — a
/// pure function, so the same cluster always distills to the same words.
fn guidance_text(rule: &str, path: &str) -> String {
    let target = if path.is_empty() {
        "the arguments root".to_owned()
    } else {
        format!("`{path}`")
    };
    match rule {
        "required" => {
            format!("calls are missing a required property at {target}; include every property the schema's `required` list names")
        }
        "type" => {
            format!("the value at {target} has the wrong JSON type; supply the declared type — quoted numerics never coerce")
        }
        "enum" => format!("the value at {target} must be one of the declared enum variants"),
        "const" => format!("the value at {target} must equal the declared const"),
        "additional_properties" => {
            format!("calls carry a property the schema does not declare at {target}; send only declared properties")
        }
        "min_items" => format!("the array at {target} is below the declared minimum length"),
        "max_items" => format!("the array at {target} exceeds the declared maximum length"),
        "min_length" => format!("the string at {target} is below the declared minimum length"),
        "max_length" => format!("the string at {target} exceeds the declared maximum length"),
        "minimum" => format!("the number at {target} is below the declared minimum"),
        "maximum" => format!("the number at {target} exceeds the declared maximum"),
        other => {
            format!("the value at {target} violates the `{other}` schema rule; re-read the tool's schema")
        }
    }
}

// --------------------------------------------------------------------- //
// The candidate constructor: learned overlays through the shipped gate
// --------------------------------------------------------------------- //

/// Build a `tool_contract` candidate carrying a learned selection overlay —
/// the wave-3 additive `selection` member on the shipped
/// `CandidateContent::ToolContract` shape. The overlay is validated
/// fail-closed at distillation (a draft that fails validation never becomes
/// a candidate, the skill-plane rule); the schema the candidate carries is
/// the tool's current declared schema, so the candidate speaks the same
/// content address the manifest pin digests. Evaluation, admission, the
/// pointer move, and rollback are the shipped machinery — nothing here
/// bypasses the gate.
pub fn selection_candidate(
    tool: &str,
    schema: Value,
    overlay: ToolSelectionOverlay,
    distilled_by: ProvenanceAuthor,
    evidence: EvidenceSpan,
    created_at: DateTime<Utc>,
) -> Result<Candidate> {
    overlay.validate()?;
    Candidate::new(
        CandidateContent::ToolContract {
            tool: tool.to_owned(),
            schema,
            selection: Some(overlay),
        },
        distilled_by,
        evidence,
        created_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pattern_digest_groups_by_shape_not_values() {
        let a = argument_pattern_digest(&json!({"query": "rust", "limit": 5}));
        let b = argument_pattern_digest(&json!({"query": "cargo", "limit": 9}));
        assert_eq!(a, b, "same shape, different values: one pattern");
        let c = argument_pattern_digest(&json!({"query": 5, "limit": 9}));
        assert_ne!(a, c, "a quoted-numeric flip is a different pattern");
        let d = argument_pattern_digest(&json!({"limit": 5, "query": "rust"}));
        assert_eq!(a, d, "object key order does not move the digest");
    }

    #[test]
    fn classify_reads_payloads_never_status_alone() {
        let refusal = crate::tool_select::argument_validation_refusal(&[ArgumentViolation {
            path: "".into(),
            rule: "required".into(),
            message: "missing required property `query`".into(),
        }]);
        // The N10 proof in miniature: status Ok, and still a validation
        // failure.
        assert!(matches!(
            classify(EventStatus::Ok, &Value::String(refusal)),
            CallOutcome::ValidationFailure(_)
        ));
        assert!(matches!(
            classify(EventStatus::Ok, &Value::String("ERROR: boom".into())),
            CallOutcome::OpaqueFailure
        ));
        assert!(matches!(
            classify(EventStatus::Error, &json!({"error": "timeout"})),
            CallOutcome::OpaqueFailure
        ));
        assert!(matches!(
            classify(EventStatus::Ok, &json!({"results": []})),
            CallOutcome::Success
        ));
        assert!(matches!(
            classify(EventStatus::Ok, &Value::String("plain result".into())),
            CallOutcome::Success
        ));
    }

    #[test]
    fn nearest_rank_percentiles() {
        let mut rollup = ToolRollup::default();
        assert_eq!(rollup.latency_percentile(95), None);
        rollup.latencies_ms = vec![10, 20, 30, 40];
        assert_eq!(rollup.latency_percentile(50), Some(20));
        assert_eq!(rollup.latency_percentile(95), Some(40));
        assert_eq!(rollup.latency_percentile(100), Some(40));
        assert_eq!(rollup.latency_percentile(1), Some(10));
    }
}
