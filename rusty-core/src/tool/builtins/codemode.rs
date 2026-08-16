//! Code mode: model-authored programs as bounded data, dispatched through
//! the real tool pipeline.
//!
//! A code-mode harness lets the model orchestrate several tool calls in one
//! dispatch by writing a small program. Rusty takes no script-engine
//! dependency, and its composition philosophy is data-over-code (the
//! composer plane's recipes are data), so the program here is *JSON*: a
//! [`CodeProgram`] is a list of steps — each step names a tool and its
//! arguments, and a bounded `parallel` block fans independent steps out —
//! interpreted by [`CodeModeTool`] against the run's own
//! [`crate::tool::ToolExecutor`]. There is no evaluation semantics beyond
//! sequencing, fan-out, and reference splicing: no loops, no conditionals,
//! no recursion (a program may not name `run_code` itself).
//!
//! # References
//!
//! A step's argument values may embed `{"$step": "<id>", "path": "<JSON
//! pointer>"}` to splice a value out of an *earlier, completed* step's
//! result (`{"$step": "search", "path": "/results/0/excerpt"}`; an empty
//! path splices the whole result). Resolution is fail-closed: an unknown
//! step, a forward reference, a reference to a failed step, or a pointer
//! that does not resolve fails the program and names the referencing step.
//! A malformed or unresolvable reference is an authoring error, so
//! `on_error` never tolerates it.
//!
//! # Admission and evidence
//!
//! Every step dispatches through [`crate::tool::ToolExecutor::execute_one`]
//! — the same path the ReAct loop's batch dispatch uses — so allowlist
//! restriction, middleware, deny-only guards, and the effect boundary judge
//! each sub-call on its own merits: a read-only run can `run_code` only
//! read-only steps, and a program that smuggles a non-allowlisted tool is
//! refused at that step, in the open.
//!
//! With [`CodeModeTool::with_evidence`] attached, every sub-call is
//! journaled as a [`RunEventKind::ToolCall`] event in the canonical
//! [`crate::replay::tool_call_request`] shape — the *resolved* arguments,
//! the verbatim result or the error, the sub-tool's own declared effect —
//! parented to the supplied causal anchor. Hand the tool the same anchor
//! the harness parents the `run_code` call itself to (in the prebuilt ReAct
//! agent, the tools-node invocation's [`crate::journal::PARENT_EVENT_KEY`]
//! node-input event id). The [`Tool`] contract gives a tool body no access
//! to its own journaled event id, so sub-calls hang off the invocation
//! anchor alongside the parent call rather than beneath it; during exact
//! replay the outer replaying wrapper serves the recorded `run_code` result
//! and the interpreter never runs, so sub-calls need no serving path of
//! their own.
//!
//! # Effect honesty
//!
//! [`Tool::effect`] is per-tool, not per-call, so `run_code` declares the
//! *ceiling* over the effects of the tools its executor can reach — the
//! maximum on the taxonomy's declaration order, computed once at
//! construction from the configured registry (which is why the executor
//! handed to [`CodeModeTool::new`] should already be the run-restricted
//! one). One correction applies: an all-keyed-write ceiling would be
//! [`Effect::Idempotent`], but a program is an unkeyed tuple of calls —
//! there is no stable idempotency key for "the program", and declaring
//! `Idempotent` while returning `None` from [`Tool::idempotency_key`] is a
//! combination the admission boundary refuses — so the ceiling steps up to
//! [`Effect::NonIdempotent`]. The declaration may be stricter than the
//! truth, never weaker; the per-step truth is enforced by admission at each
//! sub-call regardless.
//!
//! # Bounds
//!
//! - [`MAX_PROGRAM_BYTES`] on the serialized program.
//! - [`MAX_PROGRAM_STEPS`] steps in total (parallel members count).
//! - [`MAX_PARALLEL_FANOUT`] members per `parallel` block.
//! - [`MAX_TOLERATED_FAILURES`] steps may fail under `on_error:
//!   "continue"` before the program fails.
//! - [`MAX_STEP_RESULT_BYTES`] per step result; larger results are clamped
//!   before they can flow into references or the program result.
//!
//! # Wiring
//!
//! Build the sub-dispatch executor first, then register the tool over it;
//! the executor the tool holds defines exactly what programs can reach:
//!
//! ```ignore
//! let mut registry = ToolRegistry::new();
//! registry.register(CalculatorTool);
//! // Restrict to the run's allowlist before this point when the run is
//! // scoped; guards, middleware, and effect admission attach the same way
//! // they do on the run's own executor.
//! let sub = ToolExecutor::new(registry.clone());
//! registry.register(CodeModeTool::new(sub));
//! let executor = ToolExecutor::new(registry);
//! ```

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::Tool;
use crate::error::{Result, RustyError};
use crate::journal::{EventDraft, Journal};
use crate::llm::ToolCall;
use crate::record::{Effect, EventStatus, RunEventKind};
use crate::tool::{ToolExecutor, ToolRegistry};

/// The reserved tool name code mode dispatches under.
pub const RUN_CODE_TOOL_NAME: &str = "run_code";

/// The object key marking an argument value as a reference into an earlier
/// step's result: `{"$step": "<id>", "path": "<JSON pointer>"}`.
pub const STEP_REFERENCE_KEY: &str = "$step";

/// Maximum serialized size of one program.
pub const MAX_PROGRAM_BYTES: usize = 16 * 1024;
/// Maximum number of steps one program may carry, parallel members
/// included.
pub const MAX_PROGRAM_STEPS: usize = 16;
/// Maximum number of members one `parallel` block may fan out.
pub const MAX_PARALLEL_FANOUT: usize = 8;
/// Maximum number of step failures one program may tolerate under
/// `on_error: "continue"`.
pub const MAX_TOLERATED_FAILURES: usize = 4;
/// Maximum length of a step id.
pub const MAX_STEP_ID_BYTES: usize = 64;
/// Maximum length of a reference's JSON pointer.
pub const MAX_REFERENCE_PATH_BYTES: usize = 256;
/// Maximum serialized size of one step's result before it is clamped.
pub const MAX_STEP_RESULT_BYTES: usize = 64 * 1024;

/// What a step's failure does to the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepErrorPolicy {
    /// The program fails, naming the step.
    #[default]
    Fail,
    /// The failure is recorded in the program result's `failures` map and
    /// the program continues, within [`MAX_TOLERATED_FAILURES`].
    Continue,
}

/// One program step: a tool call whose arguments may splice earlier
/// results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramStep {
    /// The step's unique id within the program — the key its result is
    /// stored and referenced under.
    pub id: String,
    /// The registered tool to dispatch (never `run_code`).
    pub tool: String,
    /// The call arguments; defaults to `{}`.
    #[serde(default = "empty_arguments")]
    pub arguments: Value,
    /// What this step's failure does to the program.
    #[serde(default)]
    pub on_error: StepErrorPolicy,
}

fn empty_arguments() -> Value {
    Value::Object(Map::new())
}

/// A bounded block of independent steps dispatched concurrently and joined
/// before the program continues.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParallelBlock {
    /// The block's members.
    pub parallel: Vec<ProgramStep>,
}

/// One program entry: a single step or a parallel block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProgramItem {
    /// A sequential step.
    Step(ProgramStep),
    /// A concurrent block.
    Parallel(ParallelBlock),
}

/// A code-mode program: the closed, bounded data shape the model authors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeProgram {
    /// The program's entries, executed in order.
    pub steps: Vec<ProgramItem>,
}

impl CodeProgram {
    /// Validate the static shape: bounds, step identities, argument
    /// containers, and the no-recursion rule. References between steps
    /// resolve at execution, when the completed-results table exists.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            return Err(RustyError::Tool(
                "run_code program must contain at least one step".into(),
            ));
        }
        if self.each_step().count() > MAX_PROGRAM_STEPS {
            return Err(RustyError::Tool(format!(
                "run_code program carries more than {MAX_PROGRAM_STEPS} steps"
            )));
        }
        for item in &self.steps {
            if let ProgramItem::Parallel(block) = item {
                if block.parallel.is_empty() || block.parallel.len() > MAX_PARALLEL_FANOUT {
                    return Err(RustyError::Tool(format!(
                        "run_code parallel blocks carry 1..={MAX_PARALLEL_FANOUT} steps"
                    )));
                }
            }
        }
        let mut seen = HashSet::new();
        for step in self.each_step() {
            if step.id.is_empty()
                || step.id.len() > MAX_STEP_ID_BYTES
                || !step
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
            {
                return Err(RustyError::Tool(format!(
                    "run_code step id `{}` must use 1..={MAX_STEP_ID_BYTES} ASCII letters, digits, `.`, `_`, `:`, or `-`",
                    step.id
                )));
            }
            if !seen.insert(step.id.as_str()) {
                return Err(RustyError::Tool(format!(
                    "run_code step id `{}` appears more than once",
                    step.id
                )));
            }
            if step.tool == RUN_CODE_TOOL_NAME {
                return Err(RustyError::Tool(format!(
                    "run_code step `{}` names `{RUN_CODE_TOOL_NAME}`: programs are not recursive — unfold the sub-program into steps",
                    step.id
                )));
            }
            if !step.arguments.is_object() {
                return Err(RustyError::Tool(format!(
                    "run_code step `{}` arguments must be a JSON object",
                    step.id
                )));
            }
        }
        Ok(())
    }

    /// Every step in the program, sequential and parallel members alike.
    fn each_step(&self) -> impl Iterator<Item = &ProgramStep> {
        self.steps.iter().flat_map(|item| match item {
            ProgramItem::Step(step) => std::slice::from_ref(step).iter(),
            ProgramItem::Parallel(block) => block.parallel.iter(),
        })
    }
}

/// The parsing envelope: `steps` items are decoded one at a time so a
/// malformed entry is reported with its position.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramEnvelope {
    steps: Vec<Value>,
}

/// The evidence handle [`CodeModeTool::with_evidence`] attaches: the run's
/// journal plus the causal anchor sub-call events parent to.
#[derive(Debug, Clone)]
struct CodeModeEvidence {
    journal: Journal,
    parent: String,
}

/// `run_code`: interpret a bounded JSON program against the run's own tool
/// executor.
///
/// See the module documentation for the program shape, the admission and
/// evidence contract, and the effect-ceiling rule.
#[derive(Debug, Clone)]
pub struct CodeModeTool {
    executor: ToolExecutor,
    evidence: Option<CodeModeEvidence>,
    effect: Effect,
    description: String,
    /// Program-execution counter, the occurrence discriminator in sub-call
    /// ids: two programs running the same step ids must not collide on an
    /// approval's occurrence identity.
    executions: Arc<AtomicU64>,
}

impl CodeModeTool {
    /// A code-mode tool dispatching sub-calls through `executor`.
    ///
    /// The executor defines the entire sub-dispatch surface — registry,
    /// middleware, guards, effect admission — so hand over the
    /// run-restricted one; the declared effect ceiling is computed from its
    /// registry here.
    pub fn new(executor: ToolExecutor) -> Self {
        Self {
            effect: effect_ceiling(executor.registry()),
            executor,
            evidence: None,
            description: describe(),
            executions: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Attach the evidence handle: every sub-call is journaled as a
    /// [`RunEventKind::ToolCall`] event parented to `parent`, the causal
    /// anchor of the `run_code` invocation (in the prebuilt ReAct agent,
    /// the tools node's [`crate::journal::PARENT_EVENT_KEY`] node-input
    /// event id). Without this handle the program still runs — admission
    /// never depends on evidence — but sub-calls leave no journal events.
    pub fn with_evidence(mut self, journal: Journal, parent: impl Into<String>) -> Self {
        self.evidence = Some(CodeModeEvidence {
            journal,
            parent: parent.into(),
        });
        self
    }

    /// The sub-dispatch executor.
    pub fn executor(&self) -> &ToolExecutor {
        &self.executor
    }

    async fn run_program(&self, program: &CodeProgram) -> Result<Value> {
        let execution = self.executions.fetch_add(1, Ordering::Relaxed);
        let mut completed = Map::new();
        let mut failures = Map::new();
        let mut tolerated = 0usize;
        for item in &program.steps {
            match item {
                ProgramItem::Step(step) => {
                    let arguments = resolve_arguments(step, &completed)?;
                    match self.dispatch_step(execution, step, arguments).await {
                        Ok(value) => {
                            completed.insert(step.id.clone(), clamp_result(value));
                        }
                        Err(error) => {
                            note_failure(step, &error, &mut tolerated, &mut failures)?;
                        }
                    }
                }
                ProgramItem::Parallel(block) => {
                    // Every member's arguments resolve before any member
                    // dispatches: a block with an unresolvable reference
                    // fails without spending a single sub-call.
                    let resolved = block
                        .parallel
                        .iter()
                        .map(|step| resolve_arguments(step, &completed).map(|args| (step, args)))
                        .collect::<Result<Vec<_>>>()?;
                    let outcomes = futures::future::join_all(
                        resolved
                            .iter()
                            .map(|(step, arguments)| {
                                self.dispatch_step(execution, step, arguments.clone())
                            })
                            .collect::<Vec<_>>(),
                    )
                    .await;
                    // A block always joins: members already dispatched run to
                    // completion, and the failure policy applies afterwards.
                    for ((step, _), outcome) in resolved.iter().zip(outcomes) {
                        match outcome {
                            Ok(value) => {
                                completed.insert(step.id.clone(), clamp_result(value));
                            }
                            Err(error) => {
                                note_failure(step, &error, &mut tolerated, &mut failures)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(json!({
            "results": Value::Object(completed),
            "failures": Value::Object(failures),
        }))
    }

    /// Dispatch one resolved step through the executor's full admission
    /// pipeline, journaling the sub-call when evidence is attached.
    async fn dispatch_step(
        &self,
        execution: u64,
        step: &ProgramStep,
        arguments: Value,
    ) -> Result<Value> {
        let call = ToolCall::new(
            format!("run_code:{execution}:{}", step.id),
            step.tool.as_str(),
            arguments,
        );
        let started = self
            .evidence
            .as_ref()
            .map(|evidence| evidence.journal.clock().now());
        let result = self.executor.execute_one(&call).await;
        if let Some(evidence) = &self.evidence {
            // An unregistered tool has no declared effect; record the
            // attempt under the restrictive default. The refusal itself
            // names the tool.
            let effect = self
                .executor
                .registry()
                .get(&step.tool)
                .map(|tool| tool.effect())
                .unwrap_or(Effect::NonIdempotent);
            let mut draft = EventDraft::new(RunEventKind::ToolCall, effect)
                .input(crate::replay::tool_call_request(
                    &step.tool,
                    &call.arguments,
                ))
                .parent(evidence.parent.clone());
            if let Some(started) = started {
                let latency = (evidence.journal.clock().now() - started)
                    .num_milliseconds()
                    .max(0) as u64;
                draft = draft.latency_ms(latency);
            }
            match &result {
                Ok(value) => {
                    evidence.journal.record(draft.output(value.clone()));
                }
                Err(error) => {
                    evidence.journal.record(
                        draft
                            .status(EventStatus::Error)
                            .output(json!({ "error": error.to_string() })),
                    );
                }
            }
        }
        result
    }
}

#[async_trait]
impl Tool for CodeModeTool {
    fn name(&self) -> &str {
        RUN_CODE_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        program_schema()
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    // `effect_kind` stays the tool name and `idempotency_key` stays `None`:
    // a program is an unkeyed tuple of calls, so there is no stable key to
    // derive — see the module documentation's effect-honesty note.

    async fn call(&self, args: Value) -> Result<Value> {
        let bytes = serde_json::to_vec(&args).map_err(|error| {
            RustyError::Tool(format!("run_code program did not serialize: {error}"))
        })?;
        if bytes.len() > MAX_PROGRAM_BYTES {
            return Err(RustyError::Tool(format!(
                "run_code program exceeds the {MAX_PROGRAM_BYTES} byte ceiling"
            )));
        }
        let envelope: ProgramEnvelope = serde_json::from_value(args)
            .map_err(|error| RustyError::Tool(format!("run_code program is malformed: {error}")))?;
        let mut items = Vec::with_capacity(envelope.steps.len());
        for (index, item) in envelope.steps.into_iter().enumerate() {
            // Route on the `parallel` key so each shape is decoded against
            // its own closed struct and a typo surfaces as a field-level
            // error rather than an opaque untagged-enum mismatch.
            let parsed = if item.get("parallel").is_some() {
                serde_json::from_value::<ParallelBlock>(item).map(ProgramItem::Parallel)
            } else {
                serde_json::from_value::<ProgramStep>(item).map(ProgramItem::Step)
            };
            items.push(parsed.map_err(|error| {
                RustyError::Tool(format!("run_code step {} is malformed: {error}", index + 1))
            })?);
        }
        let program = CodeProgram { steps: items };
        program.validate()?;
        self.run_program(&program).await
    }
}

/// The declared effect of a code-mode tool: the ceiling over the effects
/// of the tools its executor can reach, `run_code` itself excluded (the
/// ceiling describes what *steps* may do; the interpreter's own declaration
/// must not poison it). An [`Effect::Idempotent`] ceiling steps up to
/// [`Effect::NonIdempotent`] — a program carries no idempotency key, and
/// the declaration may be stricter than the truth, never weaker.
fn effect_ceiling(registry: &ToolRegistry) -> Effect {
    let ceiling = registry
        .names()
        .filter(|name| *name != RUN_CODE_TOOL_NAME)
        .filter_map(|name| registry.get(name))
        .map(|tool| tool.effect())
        .max()
        .unwrap_or(Effect::Pure);
    match ceiling {
        Effect::Idempotent => Effect::NonIdempotent,
        other => other,
    }
}

/// Record a step failure: tolerated within budget under
/// [`StepErrorPolicy::Continue`], fatal to the program otherwise.
fn note_failure(
    step: &ProgramStep,
    error: &RustyError,
    tolerated: &mut usize,
    failures: &mut Map<String, Value>,
) -> Result<()> {
    if step.on_error != StepErrorPolicy::Continue {
        return Err(RustyError::Tool(format!(
            "run_code program failed at step `{}`: {error}",
            step.id
        )));
    }
    *tolerated += 1;
    if *tolerated > MAX_TOLERATED_FAILURES {
        return Err(RustyError::Tool(format!(
            "run_code program failed at step `{}`: the tolerated-failure budget ({MAX_TOLERATED_FAILURES}) is exhausted: {error}",
            step.id
        )));
    }
    failures.insert(step.id.clone(), Value::String(error.to_string()));
    Ok(())
}

/// Splice the completed-results table into a step's arguments.
fn resolve_arguments(step: &ProgramStep, completed: &Map<String, Value>) -> Result<Value> {
    resolve_value(&step.id, &step.arguments, completed)
}

fn resolve_value(step: &str, value: &Value, completed: &Map<String, Value>) -> Result<Value> {
    match value {
        Value::Object(map) if map.contains_key(STEP_REFERENCE_KEY) => {
            if map.len() != 2 {
                return Err(RustyError::Tool(format!(
                    "run_code step `{step}` reference must carry exactly `{STEP_REFERENCE_KEY}` and `path`"
                )));
            }
            let reference = map
                .get(STEP_REFERENCE_KEY)
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RustyError::Tool(format!(
                        "run_code step `{step}` reference `{STEP_REFERENCE_KEY}` must be a step id string"
                    ))
                })?;
            let path = map.get("path").and_then(Value::as_str).ok_or_else(|| {
                RustyError::Tool(format!(
                    "run_code step `{step}` reference `path` must be a JSON pointer string"
                ))
            })?;
            if path.len() > MAX_REFERENCE_PATH_BYTES || (!path.is_empty() && !path.starts_with('/'))
            {
                return Err(RustyError::Tool(format!(
                    "run_code step `{step}` reference path must be a JSON pointer (`\"\"` or `/...`) of at most {MAX_REFERENCE_PATH_BYTES} bytes"
                )));
            }
            let target = completed.get(reference).ok_or_else(|| {
                RustyError::Tool(format!(
                    "run_code step `{step}` references step `{reference}`, which has not completed — unknown, later, and failed steps cannot be referenced"
                ))
            })?;
            target.pointer(path).cloned().ok_or_else(|| {
                RustyError::Tool(format!(
                    "run_code step `{step}` reference `{path}` does not resolve in step `{reference}`'s result"
                ))
            })
        }
        Value::Object(map) => {
            let mut resolved = Map::with_capacity(map.len());
            for (key, value) in map {
                resolved.insert(key.clone(), resolve_value(step, value, completed)?);
            }
            Ok(Value::Object(resolved))
        }
        Value::Array(items) => items
            .iter()
            .map(|item| resolve_value(step, item, completed))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Clamp one step's result to [`MAX_STEP_RESULT_BYTES`] before it can flow
/// into references or the program result. Strings truncate at a char
/// boundary with an ellipsis (the house excerpt discipline); any other
/// oversized value is replaced by a marker that says what happened.
fn clamp_result(value: Value) -> Value {
    let size = serde_json::to_vec(&value)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if size <= MAX_STEP_RESULT_BYTES {
        return value;
    }
    match value {
        Value::String(text) => {
            // 4 bytes of headroom: the ellipsis itself takes 3.
            let mut kept = String::new();
            for ch in text.chars() {
                if kept.len() + ch.len_utf8() > MAX_STEP_RESULT_BYTES - 4 {
                    break;
                }
                kept.push(ch);
            }
            kept.push('…');
            Value::String(kept)
        }
        _ => json!({
            "truncated": true,
            "original_bytes": size,
            "note": "step result exceeded the per-step result ceiling; narrow the producing call and re-run",
        }),
    }
}

/// The model-facing description: the program shape, the reference syntax,
/// the failure policy, and the bounds, in one line (tool descriptions may
/// not carry control characters).
fn describe() -> String {
    format!(
        "Author and run a small program as JSON data: {{\"steps\": [...]}} where each entry is a step {{\"id\": \"<unique id>\", \"tool\": \"<tool name>\", \"arguments\": {{...}}, optional \"on_error\": \"continue\"}} or a parallel block {{\"parallel\": [<steps>]}} of independent steps fanned out together. Any argument value may be a reference {{\"$step\": \"<earlier step id>\", \"path\": \"<JSON pointer such as /results/0/excerpt; empty splices the whole result>\"}} splicing a value out of a completed earlier step's result; references to unknown, later, or failed steps fail the program. Each step dispatches through the run's normal tool pipeline, so allowlists, guards, and effect admission judge every sub-call. A failing step fails the program and names itself unless it declared \"on_error\": \"continue\" (at most {MAX_TOLERATED_FAILURES} tolerated). Bounds: {MAX_PROGRAM_STEPS} steps total, {MAX_PARALLEL_FANOUT} per parallel block, {MAX_PROGRAM_BYTES} program bytes, no recursion — run_code may not name itself."
    )
}

/// The advertised parameter schema: the program shape, closed.
fn program_schema() -> Value {
    let step = json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "minLength": 1, "maxLength": MAX_STEP_ID_BYTES},
            "tool": {"type": "string", "description": "A registered tool name — never `run_code` itself."},
            "arguments": {"type": "object", "description": "The tool call arguments. Any value may be a reference object {\"$step\": \"<earlier step id>\", \"path\": \"<JSON pointer>\"} splicing from that step's result."},
            "on_error": {"type": "string", "enum": ["fail", "continue"], "default": "fail"}
        },
        "required": ["id", "tool"],
        "additionalProperties": false
    });
    json!({
        "type": "object",
        "properties": {
            "steps": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "anyOf": [
                        step.clone(),
                        {
                            "type": "object",
                            "properties": {
                                "parallel": {"type": "array", "minItems": 1, "maxItems": MAX_PARALLEL_FANOUT, "items": step}
                            },
                            "required": ["parallel"],
                            "additionalProperties": false
                        }
                    ]
                }
            }
        },
        "required": ["steps"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::EffectAdmissionContext;
    use crate::journal::Clock;
    use crate::record::PayloadRef;
    use crate::replay::RecordingTool;
    use crate::tool::builtins::{CalculatorTool, KnowledgeSearchTool, TextInspectorTool};
    use crate::tool::{GuardDenial, GuardedCall, ToolGuard};
    use std::sync::Mutex;

    /// A configurable test tool that records the arguments it was called
    /// with and answers (or fails) with a canned outcome.
    struct Stub {
        name: &'static str,
        effect: Effect,
        outcome: std::result::Result<Value, String>,
        calls: Mutex<Vec<Value>>,
    }

    impl Stub {
        fn ok(name: &'static str, effect: Effect, result: Value) -> Arc<Self> {
            Arc::new(Self {
                name,
                effect,
                outcome: Ok(result),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn err(name: &'static str, effect: Effect, message: &str) -> Arc<Self> {
            Arc::new(Self {
                name,
                effect,
                outcome: Err(message.to_owned()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Value> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "A test stub."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect(&self) -> Effect {
            self.effect
        }
        async fn call(&self, args: Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(args);
            self.outcome.clone().map_err(RustyError::Tool)
        }
    }

    struct PanicTool;

    #[async_trait]
    impl Tool for PanicTool {
        fn name(&self) -> &str {
            "panic"
        }
        fn description(&self) -> &str {
            "Always panics."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            panic!("kaboom");
        }
    }

    /// A guard that denies one named tool.
    #[derive(Debug)]
    struct DenyTool(&'static str);

    impl ToolGuard for DenyTool {
        fn name(&self) -> &str {
            "deny-tool"
        }
        fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
            (call.tool == self.0).then(|| {
                GuardDenial::new(
                    "deny-tool",
                    format!("`{}` is off limits in this run", self.0),
                )
            })
        }
    }

    fn registry_of(tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register_shared(tool);
        }
        registry
    }

    fn code_mode(registry: ToolRegistry) -> CodeModeTool {
        CodeModeTool::new(ToolExecutor::new(registry))
    }

    fn test_journal() -> Journal {
        Journal::new("run", "thread", Clock::logical(0, 1))
    }

    fn calculator_step(id: &str, operation: &str, left: Value, right: Value) -> Value {
        json!({"id": id, "tool": "calculator", "arguments": {"operation": operation, "left": left, "right": right}})
    }

    #[tokio::test]
    async fn sequential_steps_resolve_placeholders() {
        let produce = Stub::ok("produce", Effect::Pure, json!({"value": 41}));
        let greet = Stub::ok("greet", Effect::Pure, json!("hello world"));
        let tool = code_mode(registry_of(vec![
            produce.clone(),
            greet.clone(),
            Arc::new(CalculatorTool),
            Arc::new(TextInspectorTool),
        ]));

        let result = tool
            .call(json!({"steps": [
                {"id": "produce", "tool": "produce"},
                calculator_step("sum", "add", json!({"$step": "produce", "path": "/value"}), json!(1)),
                {"id": "greet", "tool": "greet"},
                // An empty path splices the whole result.
                {"id": "count", "tool": "inspect_text", "arguments": {"text": {"$step": "greet", "path": ""}}}
            ]}))
            .await
            .unwrap();

        assert_eq!(result["results"]["sum"]["result"], json!(42.0));
        assert_eq!(result["results"]["count"]["words"], json!(2));
        assert_eq!(result["failures"], json!({}));
        // A step without `arguments` dispatches with the empty object.
        assert_eq!(greet.calls(), vec![json!({})]);
    }

    #[tokio::test]
    async fn parallel_blocks_fan_out_and_join() {
        let tool = code_mode(registry_of(vec![Arc::new(CalculatorTool)]));

        let result = tool
            .call(json!({"steps": [
                {"parallel": [
                    calculator_step("a", "add", json!(1), json!(2)),
                    calculator_step("b", "multiply", json!(3), json!(4)),
                    calculator_step("c", "subtract", json!(10), json!(5))
                ]},
                calculator_step(
                    "total",
                    "add",
                    json!({"$step": "a", "path": "/result"}),
                    json!({"$step": "b", "path": "/result"})
                )
            ]}))
            .await
            .unwrap();

        assert_eq!(result["results"]["a"]["result"], json!(3.0));
        assert_eq!(result["results"]["b"]["result"], json!(12.0));
        assert_eq!(result["results"]["c"]["result"], json!(5.0));
        assert_eq!(result["results"]["total"]["result"], json!(15.0));
    }

    #[tokio::test]
    async fn smuggled_tool_is_refused_at_the_step() {
        let writer = Stub::ok("write_file", Effect::NonIdempotent, json!({"ok": true}));
        let full = registry_of(vec![Arc::new(CalculatorTool), writer.clone()]);
        let restricted = full.restricted_to(&["calculator".to_string()]).unwrap();
        let journal = test_journal();
        let tool = CodeModeTool::new(ToolExecutor::new(restricted))
            .with_evidence(journal.clone(), "run:3");

        let error = tool
            .call(json!({"steps": [
                calculator_step("calc", "add", json!(1), json!(1)),
                {"id": "write", "tool": "write_file", "arguments": {"path": "x"}}
            ]}))
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("step `write`"), "got: {message}");
        assert!(
            message.contains("unknown tool `write_file`"),
            "got: {message}"
        );
        // The smuggled tool was never invoked.
        assert!(writer.calls().is_empty());
        // The journal shows the honest refusal: one clean sub-call, one
        // refused sub-call with the error recorded, both parented.
        let events = journal.events();
        let calls: Vec<_> = events
            .iter()
            .filter(|event| event.kind == RunEventKind::ToolCall)
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].status, EventStatus::Ok);
        assert_eq!(calls[1].status, EventStatus::Error);
        match &calls[1].output {
            Some(PayloadRef::Inline(output)) => assert!(
                output["error"].as_str().unwrap().contains("unknown tool"),
                "got: {output}"
            ),
            other => panic!("expected an inline refusal payload, got {other:?}"),
        }
        assert!(calls
            .iter()
            .all(|event| event.parent.as_deref() == Some("run:3")));
    }

    #[tokio::test]
    async fn effect_admission_judges_each_sub_call() {
        let writer = Stub::ok("charge_card", Effect::NonIdempotent, json!({"ok": true}));
        let executor =
            ToolExecutor::new(registry_of(vec![Arc::new(CalculatorTool), writer.clone()]))
                .with_effect_admission(EffectAdmissionContext::new("scope"));
        let tool = CodeModeTool::new(executor);

        // A read-only run can run_code read-only steps: every sub-call
        // passes the boundary on its own declared class.
        tool.call(json!({"steps": [calculator_step("c", "add", json!(2), json!(2))]}))
            .await
            .unwrap();

        // An irreversible step without an approval is refused at that
        // step — the program cannot launder it through run_code.
        let error = tool
            .call(json!({"steps": [{"id": "charge", "tool": "charge_card"}]}))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("step `charge`"), "got: {message}");
        assert!(
            message.contains("effect admission denied"),
            "got: {message}"
        );
        assert!(writer.calls().is_empty());
    }

    #[tokio::test]
    async fn guard_denials_propagate_and_are_journaled() {
        let journal = test_journal();
        let executor = ToolExecutor::new(registry_of(vec![
            Arc::new(CalculatorTool),
            Arc::new(TextInspectorTool),
        ]))
        .with_tool_guards(vec![Arc::new(DenyTool("inspect_text"))])
        .with_guard_journal(journal.clone(), "run:5");
        let tool = CodeModeTool::new(executor).with_evidence(journal.clone(), "run:5");

        let error = tool
            .call(json!({"steps": [
                calculator_step("calc", "add", json!(1), json!(2)),
                {"id": "read", "tool": "inspect_text", "arguments": {"text": "hello"}}
            ]}))
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("tool guard denied `inspect_text`"),
            "got: {message}"
        );
        assert!(message.contains("deny-tool"), "got: {message}");

        let events = journal.events();
        let denials: Vec<_> = events
            .iter()
            .filter(|event| event.kind == RunEventKind::ToolCallDenied)
            .collect();
        assert_eq!(denials.len(), 1);
        assert_eq!(denials[0].parent.as_deref(), Some("run:5"));
        let refused: Vec<_> = events
            .iter()
            .filter(|event| {
                event.kind == RunEventKind::ToolCall && event.status == EventStatus::Error
            })
            .collect();
        assert_eq!(refused.len(), 1);
        match &refused[0].output {
            Some(PayloadRef::Inline(output)) => assert!(
                output["error"].as_str().unwrap().contains("guard"),
                "got: {output}"
            ),
            other => panic!("expected an inline refusal payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn program_bounds_hold() {
        let tool = code_mode(registry_of(vec![Arc::new(CalculatorTool)]));
        let step = |id: String| calculator_step(&id, "add", json!(1), json!(1));

        let too_many: Vec<Value> = (0..=MAX_PROGRAM_STEPS)
            .map(|index| step(format!("s{index}")))
            .collect();
        let error = tool.call(json!({"steps": too_many})).await.unwrap_err();
        assert!(
            error.to_string().contains("more than 16 steps"),
            "got: {error}"
        );

        let fan_out: Vec<Value> = (0..=MAX_PARALLEL_FANOUT)
            .map(|index| step(format!("p{index}")))
            .collect();
        let error = tool
            .call(json!({"steps": [{"parallel": fan_out}]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("parallel blocks carry 1..=8"),
            "got: {error}"
        );

        let oversized = json!({"steps": [{
            "id": "x",
            "tool": "calculator",
            "arguments": {"operation": "add", "left": 1, "right": 1, "pad": "x".repeat(32 * 1024)}
        }]});
        let error = tool.call(oversized).await.unwrap_err();
        assert!(error.to_string().contains("byte ceiling"), "got: {error}");

        let error = tool
            .call(json!({"steps": [step("dup".to_string()), step("dup".to_string())]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("appears more than once"),
            "got: {error}"
        );

        let error = tool
            .call(json!({"steps": [step("bad id".to_string())]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("must use 1..=64"),
            "got: {error}"
        );

        let error = tool
            .call(json!({"steps": [{"id": "x", "tool": "run_code"}]}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not recursive"), "got: {error}");

        let error = tool.call(json!({"steps": []})).await.unwrap_err();
        assert!(
            error.to_string().contains("at least one step"),
            "got: {error}"
        );

        let error = tool
            .call(json!({"steps": [{"id": "x", "tool": "calculator", "arguments": [1]}]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("must be a JSON object"),
            "got: {error}"
        );

        let error = tool.call(json!({"stepz": []})).await.unwrap_err();
        assert!(error.to_string().contains("malformed"), "got: {error}");

        let error = tool
            .call(json!({"steps": [{"id": "x", "toolz": "calculator"}]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("step 1 is malformed"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn fail_fast_halts_the_program() {
        let before = Stub::ok("before", Effect::Pure, json!(1));
        let failing = Stub::err("failing", Effect::Pure, "boom");
        let after = Stub::ok("after", Effect::Pure, json!(2));
        let tool = code_mode(registry_of(vec![
            before.clone(),
            failing.clone(),
            after.clone(),
        ]));

        let error = tool
            .call(json!({"steps": [
                {"id": "a", "tool": "before"},
                {"id": "f", "tool": "failing"},
                {"id": "b", "tool": "after"}
            ]}))
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("failed at step `f`"), "got: {message}");
        assert!(message.contains("boom"), "got: {message}");
        assert_eq!(before.calls().len(), 1);
        assert!(after.calls().is_empty());
    }

    #[tokio::test]
    async fn tolerated_failures_are_recorded_and_bounded() {
        let failing = Stub::err("failing", Effect::Pure, "boom");
        let tool = code_mode(registry_of(vec![
            Stub::ok("before", Effect::Pure, json!(1)),
            failing.clone(),
            Stub::ok("after", Effect::Pure, json!(2)),
        ]));

        let result = tool
            .call(json!({"steps": [
                {"id": "a", "tool": "before"},
                {"id": "f", "tool": "failing", "on_error": "continue"},
                {"id": "b", "tool": "after"}
            ]}))
            .await
            .unwrap();
        assert_eq!(result["results"]["a"], json!(1));
        assert_eq!(result["results"]["b"], json!(2));
        assert!(result["results"].get("f").is_none());
        assert!(result["failures"]["f"].as_str().unwrap().contains("boom"));

        // A tolerated failure inside a parallel block follows the same rule.
        let result = tool
            .call(json!({"steps": [{"parallel": [
                {"id": "ok", "tool": "before"},
                {"id": "bad", "tool": "failing", "on_error": "continue"}
            ]}]}))
            .await
            .unwrap();
        assert_eq!(result["results"]["ok"], json!(1));
        assert!(result["failures"]["bad"].as_str().unwrap().contains("boom"));

        // The budget is per program: the fifth tolerated failure fails the
        // program, naming the step that crossed it.
        let steps: Vec<Value> = (0..=MAX_TOLERATED_FAILURES)
            .map(|index| json!({"id": format!("f{index}"), "tool": "failing", "on_error": "continue"}))
            .collect();
        let calls_before = failing.calls().len();
        let error = tool.call(json!({"steps": steps})).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("step `f4`"), "got: {message}");
        assert!(
            message.contains("tolerated-failure budget"),
            "got: {message}"
        );
        assert_eq!(failing.calls().len() - calls_before, 5);
    }

    #[tokio::test]
    async fn unresolvable_references_fail_closed() {
        let produce = Stub::ok("produce", Effect::Pure, json!({"value": 1}));
        let failing = Stub::err("failing", Effect::Pure, "boom");
        let tool = code_mode(registry_of(vec![
            produce.clone(),
            failing.clone(),
            Arc::new(CalculatorTool),
        ]));
        let reference = |step: &str, path: &str| json!({"$step": step, "path": path});

        // A forward reference — even under `on_error: "continue"`, since an
        // unresolvable reference is an authoring error, not a tool failure.
        let error = tool
            .call(json!({"steps": [
                {"id": "s1", "tool": "calculator", "on_error": "continue",
                 "arguments": {"operation": "add", "left": reference("s2", "/value"), "right": 1}},
                {"id": "s2", "tool": "produce"}
            ]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("`s1` references step `s2`"),
            "got: {error}"
        );

        // A pointer that does not resolve.
        let error = tool
            .call(json!({"steps": [
                {"id": "p", "tool": "produce"},
                {"id": "c", "tool": "calculator",
                 "arguments": {"operation": "add", "left": reference("p", "/missing"), "right": 1}}
            ]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("does not resolve"),
            "got: {error}"
        );

        // A reference to a tolerated-failed step has nothing to resolve
        // against.
        let error = tool
            .call(json!({"steps": [
                {"id": "f", "tool": "failing", "on_error": "continue"},
                {"id": "g", "tool": "calculator",
                 "arguments": {"operation": "add", "left": reference("f", ""), "right": 1}}
            ]}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("`g` references step `f`"),
            "got: {error}"
        );

        // Malformed reference shapes.
        for bad in [
            json!({"$step": "p"}),
            json!({"$step": "p", "path": "/value", "note": 1}),
            json!({"$step": 1, "path": "/value"}),
            json!({"$step": "p", "path": "value"}),
        ] {
            let error = tool
                .call(json!({"steps": [
                    {"id": "p", "tool": "produce"},
                    {"id": "c", "tool": "calculator",
                     "arguments": {"operation": "add", "left": bad, "right": 1}}
                ]}))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("reference"), "got: {error}");
        }
    }

    #[test]
    fn effect_ceiling_is_the_maximum_step_class() {
        // An all-pure surface declares Pure.
        let tool = code_mode(registry_of(vec![Arc::new(CalculatorTool)]));
        assert_eq!(tool.effect(), Effect::Pure);

        // A read-only surface raises the ceiling to ReadOnly.
        let tool = code_mode(registry_of(vec![
            Arc::new(CalculatorTool),
            Arc::new(KnowledgeSearchTool::new(Vec::new()).unwrap()),
        ]));
        assert_eq!(tool.effect(), Effect::ReadOnly);

        // A keyed-write surface would ceiling at Idempotent, but a program
        // carries no idempotency key, so the declaration steps up.
        let tool = code_mode(registry_of(vec![Stub::ok(
            "put",
            Effect::Idempotent,
            json!(true),
        )]));
        assert_eq!(tool.effect(), Effect::NonIdempotent);

        // An irreversible surface dominates.
        let tool = code_mode(registry_of(vec![
            Arc::new(CalculatorTool),
            Stub::ok("send", Effect::NonIdempotent, json!(true)),
        ]));
        assert_eq!(tool.effect(), Effect::NonIdempotent);

        // run_code's own declaration never poisons the ceiling: a tool
        // whose sub-surface is pure declares Pure even when the registry it
        // was built from holds another run_code over an irreversible one.
        let inner = code_mode(registry_of(vec![Stub::ok(
            "send",
            Effect::NonIdempotent,
            json!(true),
        )]));
        assert_eq!(inner.effect(), Effect::NonIdempotent);
        let outer = code_mode(registry_of(vec![Arc::new(CalculatorTool), Arc::new(inner)]));
        assert_eq!(outer.effect(), Effect::Pure);

        // Programs are not keyable, so no idempotency key is ever declared.
        assert_eq!(outer.idempotency_key(&json!({})), None);
    }

    #[tokio::test]
    async fn sub_calls_are_journaled_with_the_parent_anchor() {
        let produce = Stub::ok("produce", Effect::Pure, json!({"value": 41}));
        let journal = test_journal();
        let tool = code_mode(registry_of(vec![produce.clone(), Arc::new(CalculatorTool)]))
            .with_evidence(journal.clone(), "run:9");
        // Wrap the way the ReAct tools node wraps every dispatched tool:
        // the wrapper journals the parent run_code call, the interpreter
        // journals each sub-call beneath the same invocation anchor.
        let wrapped = RecordingTool::new(Arc::new(tool), journal.clone(), "run:9");

        wrapped
            .call(json!({"steps": [
                {"id": "produce", "tool": "produce"},
                calculator_step("sum", "add", json!({"$step": "produce", "path": "/value"}), json!(1))
            ]}))
            .await
            .unwrap();
        let error = wrapped
            .call(json!({"steps": [{"id": "oops", "tool": "missing"}]}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("step `oops`"), "got: {error}");

        let events = journal.events();
        assert_eq!(events.len(), 5);
        assert!(events
            .iter()
            .all(|event| event.parent.as_deref() == Some("run:9")));

        // The sub-calls: canonical request shapes with the *resolved*
        // arguments, verbatim results, and each sub-tool's own effect.
        let sum = events
            .iter()
            .find(|event| {
                matches!(&event.input, Some(PayloadRef::Inline(input)) if input["tool"] == json!("calculator"))
            })
            .expect("the calculator sub-call is journaled");
        assert_eq!(sum.kind, RunEventKind::ToolCall);
        assert_eq!(sum.effect, Effect::Pure);
        match &sum.input {
            Some(PayloadRef::Inline(input)) => assert_eq!(
                input["arguments"],
                json!({"operation": "add", "left": 41, "right": 1})
            ),
            other => panic!("expected an inline request payload, got {other:?}"),
        }
        match &sum.output {
            Some(PayloadRef::Inline(output)) => assert_eq!(output["result"], json!(42.0)),
            other => panic!("expected an inline result payload, got {other:?}"),
        }

        // The failed sub-call and the failed parent call both record the
        // error honestly.
        let refused = events
            .iter()
            .find(|event| {
                matches!(&event.input, Some(PayloadRef::Inline(input)) if input["tool"] == json!("missing"))
            })
            .expect("the refused sub-call is journaled");
        assert_eq!(refused.status, EventStatus::Error);
        assert_eq!(refused.effect, Effect::NonIdempotent);

        // The parent run_code calls are journaled by the wrapper with the
        // program as their input.
        let parents: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(&event.input, Some(PayloadRef::Inline(input)) if input["tool"] == json!("run_code"))
            })
            .collect();
        assert_eq!(parents.len(), 2);
        assert_eq!(parents[0].status, EventStatus::Ok);
        assert_eq!(parents[1].status, EventStatus::Error);
    }

    #[tokio::test]
    async fn step_results_are_clamped() {
        let big_text = Stub::ok("big_text", Effect::Pure, json!("x".repeat(128 * 1024)));
        let big_object = Stub::ok(
            "big_object",
            Effect::Pure,
            json!({"blob": "x".repeat(128 * 1024)}),
        );
        let tool = code_mode(registry_of(vec![big_text.clone(), big_object.clone()]));

        let result = tool
            .call(json!({"steps": [
                {"id": "text", "tool": "big_text"},
                {"id": "object", "tool": "big_object"}
            ]}))
            .await
            .unwrap();

        let text = result["results"]["text"].as_str().unwrap();
        assert!(text.len() <= MAX_STEP_RESULT_BYTES);
        assert!(text.ends_with('…'));
        assert_eq!(result["results"]["object"]["truncated"], json!(true));
        let expected_bytes = serde_json::to_vec(&json!({"blob": "x".repeat(128 * 1024)}))
            .unwrap()
            .len();
        assert_eq!(
            result["results"]["object"]["original_bytes"],
            json!(expected_bytes)
        );
    }

    #[tokio::test]
    async fn a_panicking_step_fails_the_program_without_taking_it_down() {
        let tool = code_mode(registry_of(vec![
            Arc::new(PanicTool),
            Arc::new(CalculatorTool),
        ]));

        let error = tool
            .call(json!({"steps": [{"id": "p", "tool": "panic"}]}))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("step `p`"), "got: {message}");
        assert!(message.contains("panicked"), "got: {message}");

        // The interpreter survives and keeps dispatching.
        tool.call(json!({"steps": [calculator_step("c", "add", json!(1), json!(1))]}))
            .await
            .unwrap();
    }
}
