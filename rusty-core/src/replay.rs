//! The replay engine: exact replay, branch diff, and portable fixtures.
//!
//! This module is the second work item of the R0.5 Flight Recorder. Wave 1
//! landed the evidence contracts ([`crate::record`]), the journal, and the
//! determinism seams ([`Clock`], [`RngSource`]); this module consumes them to
//! re-drive a recorded run **exactly**: model, tool, remote, and WASM calls
//! are served from the journal instead of executed, and the replayed run's
//! journal and final state must reproduce the recorded ones byte-for-byte.
//!
//! # The exact-replay contract
//!
//! Exact replay re-runs the *same* graph topology with the *same* determinism
//! seams (logical clock, seeded RNG) and swaps only the effect
//! implementations: wherever the recorded run talked to the world, the replay
//! talks to the journal. Node bodies still execute — `Pure` computation is
//! freely repeatable — but every outbound effect is matched against the
//! journaled record **by sequence and request hash** and answered with the
//! recorded response. Three failures are detected loudly:
//!
//! - **Divergence** — the replayed run issues a request whose canonical hash
//!   differs from the journaled request at the next unserved sequence
//!   position ([`RustyError::Replay`]).
//! - **Order violation** — effects arrive in a different order or kind than
//!   the journal recorded (exact replay serves effects in `seq` order only).
//! - **Exhaustion / shortfall** — the replayed run issues more effects than
//!   the journal holds, or finishes with recorded effects unserved
//!   (verification fails).
//!
//! Interrupts need no serving: with every effect answered deterministically
//! from the journal, node logic re-derives the same interrupt and the
//! executor journals it identically.
//!
//! # Zero outbound calls
//!
//! [`ReplayingChatModel`] and [`ReplayingTool`] wrap the real implementations
//! but **never invoke them** — there is no code path from `chat`/`call` to
//! the wrapped value. Tests prove it by replaying with panic-on-call
//! sentinels as the inner implementations. The wrappers exist so the *same
//! graph code* runs in record and replay mode; only the effect
//! implementations are swapped.
//!
//! # Determinism prerequisites
//!
//! Byte-identical replay requires the replay run to observe the same time and
//! id sequences as the recorded run: attach a fresh journal with the recorded
//! run's identity (`run_id`, `thread_id`) and a logical clock with the same
//! parameters ([`ExactReplay::fresh_journal`]), and seed the RNG with the
//! recorded run's seed ([`ReplayParams`]). [`ReplayFixture`] carries these
//! parameters in its metadata so CI replay is self-contained. The recording
//! and replaying wrappers perform the same number of journal-clock reads per
//! call, keeping the tick sequence aligned; per the journal's determinism
//! model, super-steps with several parallel nodes interleave clock reads by
//! schedule, so byte-identical replay is guaranteed for runs whose steps run
//! one node at a time.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::checkpoint::{Checkpoint, Checkpointer, InMemoryCheckpointer};
use crate::effects::EffectRequest;
use crate::error::{Result, RustyError};
use crate::executor::{ExecutionOutcome, Executor, RunConfig, DEFAULT_MAX_STEPS};
use crate::graph::Graph;
use crate::journal::{Clock, EventDraft, Journal, JournalSnapshot, RngSource};
use crate::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall, Usage};
use crate::record::{sha256_hex, Effect, EventStatus, PayloadRef, RunEvent, RunEventKind};
use crate::state::{State, StateSpec};
use crate::tool::Tool;

/// The current on-disk format version of [`ReplayFixture`].
///
/// Bump only on a breaking change to the fixture envelope; additive evolution
/// uses serde defaults so previously written fixtures keep deserializing.
pub const FIXTURE_FORMAT_VERSION: u32 = 1;

/// The effect kinds exact replay can serve from the journal: outbound calls
/// whose recorded responses replace live execution. Super-step boundaries,
/// node inputs/outputs, routing decisions, interrupts, and checkpoint writes
/// are *re-derived* by the executor, never served.
const SERVABLE_KINDS: [RunEventKind; 4] = [
    RunEventKind::ModelCall,
    RunEventKind::ToolCall,
    RunEventKind::RemoteCall,
    RunEventKind::WasmCall,
];

fn is_servable(kind: RunEventKind) -> bool {
    SERVABLE_KINDS.contains(&kind)
}

fn replay_error(message: impl Into<String>) -> RustyError {
    RustyError::Replay(message.into())
}

/// The canonical JSON shape a [`RecordingChatModel`] journals (and a
/// [`ReplayingChatModel`] matches) as a model call's **request**: the
/// messages and tool schemas exactly as passed to [`ChatModel::chat`].
///
/// The hash of this value's canonical serialization is the request identity
/// exact replay matches on — node code that journals model calls by hand must
/// use this shape for its journals to be replayable.
pub fn model_call_request(messages: &[ChatMessage], tools: &[Value]) -> Value {
    json!({ "messages": messages, "tools": tools })
}

/// The canonical JSON shape a [`RecordingChatModel`] journals (and a
/// [`ReplayingChatModel`] reconstructs a [`ChatResponse`] from) as a model
/// call's **response**: the assistant message, the reported model identity,
/// and token usage. Keys are always present (`null` when unreported) so the
/// shape is stable.
pub fn model_call_response(response: &ChatResponse) -> Value {
    json!({
        "message": response.message,
        "model": response.model,
        "usage": response.usage,
    })
}

/// The canonical JSON shape a [`RecordingTool`] journals (and a
/// [`ReplayingTool`] matches) as a tool call's **request**: the tool name and
/// the model-supplied arguments. The recorded **output** is the tool's result
/// value verbatim.
pub fn tool_call_request(name: &str, arguments: &Value) -> Value {
    json!({ "tool": name, "arguments": arguments })
}

/// Reconstruct a [`ChatResponse`] from a journaled [`model_call_response`]
/// payload.
fn chat_response_from_recorded(seq: u64, value: &Value) -> Result<ChatResponse> {
    let decode = |key: &str| -> Result<Value> {
        value.get(key).cloned().ok_or_else(|| {
            replay_error(format!(
                "recorded model call at seq {seq} is malformed: output payload has no `{key}` key"
            ))
        })
    };
    let message: ChatMessage = serde_json::from_value(decode("message")?)?;
    let model: Option<String> = serde_json::from_value(decode("model")?)?;
    let usage: Option<Usage> = serde_json::from_value(decode("usage")?)?;
    Ok(ChatResponse {
        message,
        model,
        usage,
    })
}

/// The recorded error text of a failed served effect (`{"error": …}` output
/// payloads, as written by the recording wrappers and the executor's
/// node-failure path).
fn recorded_error_text(served: &ServedEffect) -> String {
    served
        .output
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("<no recorded error payload>")
        .to_owned()
}

/// Verify the integrity of a journal snapshot: the chained head hash must
/// recompute from the events (as [`Journal::from_snapshot`] enforces), every
/// content-addressed artifact must hash to its own key, and every artifact
/// reference on an event must resolve. Anything less is not evidence a replay
/// may trust.
fn verify_snapshot(snapshot: &JournalSnapshot) -> Result<()> {
    // Head-hash re-verification (the clock is irrelevant to hashing).
    Journal::from_snapshot(clone_snapshot(snapshot), Clock::System)?;
    for (hash, value) in &snapshot.artifacts {
        let actual = sha256_hex(&serde_json::to_vec(value)?);
        if actual != *hash {
            return Err(replay_error(format!(
                "journal snapshot artifact integrity failure: artifact stored under {hash} \
                 hashes to {actual} — the snapshot was tampered with or corrupted"
            )));
        }
    }
    for event in &snapshot.events {
        for payload in [&event.input, &event.output].into_iter().flatten() {
            if let PayloadRef::Artifact(reference) = payload {
                if !snapshot.artifacts.contains_key(&reference.sha256) {
                    return Err(replay_error(format!(
                        "journal snapshot is truncated: event {} references artifact {} which \
                         is absent from the artifact map",
                        event.id, reference.sha256
                    )));
                }
            }
        }
    }
    Ok(())
}

// `JournalSnapshot` is `Clone`; the explicit helper documents that the clone
// exists only because `Journal::from_snapshot` consumes.
fn clone_snapshot(snapshot: &JournalSnapshot) -> JournalSnapshot {
    snapshot.clone()
}

fn resolve_in(snapshot: &JournalSnapshot, payload: &PayloadRef) -> Option<Value> {
    match payload {
        PayloadRef::Inline(value) => Some(value.clone()),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256).cloned(),
    }
}

fn resolve_opt(snapshot: &JournalSnapshot, payload: Option<&PayloadRef>) -> Option<Value> {
    payload.and_then(|payload| resolve_in(snapshot, payload))
}

/// Canonical-content hash of a request value: `sha256` of its `serde_json`
/// serialization (map keys sort deterministically, so equal values hash
/// equal). This is the same computation [`PayloadRef::content_hash`] applies
/// to inline payloads, which is what makes journaled requests matchable.
fn request_hash(request: &Value) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(request)?))
}

/// A recorded effect served during exact replay: the matching journaled event
/// plus its resolved input/output payloads (artifact references already
/// looked through).
#[derive(Debug, Clone)]
pub struct ServedEffect {
    /// The journaled event that matched the issued request.
    pub event: RunEvent,

    /// The event's resolved input payload (the journaled request).
    pub input: Option<Value>,

    /// The event's resolved output payload (the journaled response).
    pub output: Option<Value>,
}

impl ServedEffect {
    /// Re-journal this served effect into the replay run's journal under
    /// causal parent `parent`, returning the new event id.
    ///
    /// The replayed event reproduces the recorded one exactly — kind, node,
    /// effect class, payloads, latency, tokens, cost, status — while the
    /// journal assigns the replay run's own `seq` and `recorded_at` (which
    /// match the recorded run's when the determinism seams are aligned).
    pub fn rejournal(&self, journal: &Journal, parent: impl Into<String>) -> String {
        let mut draft = EventDraft::new(self.event.kind, self.event.effect)
            .status(self.event.status)
            .parent(parent);
        if let Some(node) = &self.event.node_id {
            draft = draft.node(node.clone());
        }
        if let Some(input) = &self.input {
            draft = draft.input(input.clone());
        }
        if let Some(output) = &self.output {
            draft = draft.output(output.clone());
        }
        if let Some(latency_ms) = self.event.latency_ms {
            draft = draft.latency_ms(latency_ms);
        }
        if let Some(tokens) = self.event.tokens {
            draft = draft.tokens(tokens);
        }
        if let Some(cost_usd) = self.event.cost_usd {
            draft = draft.cost_usd(cost_usd);
        }
        journal.record(draft)
    }
}

#[derive(Debug)]
struct ReplaySourceInner {
    /// The snapshot's servable events, in `seq` order.
    servable: Vec<RunEvent>,
    /// The snapshot's artifact map, for payload resolution.
    artifacts: BTreeMap<String, Value>,
    /// Position of the next unserved event. Strictly ordered: exact replay
    /// answers effects in journaled sequence order or not at all.
    cursor: usize,
}

/// The serving side of exact replay: an ordered cursor over a journaled run's
/// recorded effects.
///
/// Cheap to clone (shared cursor behind one `Arc`), so every replaying
/// wrapper in a graph can hold a handle. Matching is by **sequence + request
/// hash**: [`ReplaySource::serve`] takes the next unserved servable event in
/// `seq` order, requires its kind to match, and requires the issued request's
/// canonical hash to equal the journaled request's. Anything else fails
/// loudly with [`RustyError::Replay`] — a replay that has drifted from its
/// evidence must stop, not improvise.
#[derive(Debug, Clone)]
pub struct ReplaySource {
    inner: Arc<Mutex<ReplaySourceInner>>,
}

impl ReplaySource {
    /// A source over the servable events of `snapshot`. The snapshot is
    /// assumed already integrity-verified (via [`ExactReplay::new`] or
    /// [`ReplayFixture::import`]); unresolved artifact references simply fail
    /// to match.
    pub fn new(snapshot: &JournalSnapshot) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ReplaySourceInner {
                servable: snapshot
                    .events
                    .iter()
                    .filter(|event| is_servable(event.kind))
                    .cloned()
                    .collect(),
                artifacts: snapshot.artifacts.clone(),
                cursor: 0,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ReplaySourceInner> {
        // Poison means a serving path panicked mid-read; the cursor value is
        // plain data and stays coherent, so recovering is safe.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serve one outbound effect from the journal.
    ///
    /// `kind` must be a servable effect kind (model, tool, remote, or WASM
    /// call); `request` is the canonical request value (see
    /// [`model_call_request`] / [`tool_call_request`]). On success the cursor
    /// advances past the matched event and the returned [`ServedEffect`
    /// carries the recorded response. On any mismatch — wrong kind at the
    /// cursor, different request hash, no events left — a descriptive
    /// [`RustyError::Replay`] names the sequence position and both sides of
    /// the comparison.
    pub fn serve(&self, kind: RunEventKind, request: &Value) -> Result<ServedEffect> {
        if !is_servable(kind) {
            return Err(replay_error(format!(
                "{kind:?} is not a servable effect kind; exact replay serves only {SERVABLE_KINDS:?} \
                 — executor-derived events are reproduced by re-execution, not served"
            )));
        }
        let issued_hash = request_hash(request)?;
        let mut inner = self.lock();
        let Some(event) = inner.servable.get(inner.cursor) else {
            return Err(replay_error(format!(
                "replay exhaustion: the run issued a {kind:?} call (request hash {issued_hash}), \
                 but the journal holds no further recorded effects to serve — the replayed run \
                 is doing more work than the recorded one"
            )));
        };
        if event.kind != kind {
            return Err(replay_error(format!(
                "replay order violation at recorded seq {}: the run issued a {kind:?} call, but \
                 the next unserved recorded effect is {:?} — exact replay requires effects in \
                 journaled sequence order",
                event.seq, event.kind
            )));
        }
        let recorded_input = event
            .input
            .as_ref()
            .and_then(|payload| match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(reference) => inner.artifacts.get(&reference.sha256).cloned(),
            })
            .ok_or_else(|| {
                replay_error(format!(
                    "recorded {:?} at seq {} has no input payload to match against",
                    event.kind, event.seq
                ))
            })?;
        let recorded_hash = request_hash(&recorded_input)?;
        if recorded_hash != issued_hash {
            return Err(replay_error(format!(
                "replay divergence at recorded seq {} ({:?}): the run issued a request whose \
                 canonical hash is {issued_hash}, but the journaled request hashes to \
                 {recorded_hash} — the replayed run has diverged from its evidence",
                event.seq, event.kind
            )));
        }
        let served = ServedEffect {
            input: Some(recorded_input),
            output: event.output.as_ref().and_then(|payload| match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(reference) => inner.artifacts.get(&reference.sha256).cloned(),
            }),
            event: event.clone(),
        };
        inner.cursor += 1;
        Ok(served)
    }

    /// `true` when every recorded effect has been served. Verification treats
    /// a non-exhausted source as a replay that stopped short of its evidence.
    pub fn is_exhausted(&self) -> bool {
        let inner = self.lock();
        inner.cursor == inner.servable.len()
    }

    /// The `(seq, kind)` pairs still waiting to be served, in order. Empty
    /// when [`ReplaySource::is_exhausted`] is `true`.
    pub fn remaining(&self) -> Vec<(u64, RunEventKind)> {
        let inner = self.lock();
        inner.servable[inner.cursor..]
            .iter()
            .map(|event| (event.seq, event.kind))
            .collect()
    }
}

/// A [`ChatModel`] that records every call into the run's journal in the
/// canonical replay-compatible shape, then returns the real response.
///
/// Construct per node invocation (it is a cheap handle): the causal parent is
/// the invocation's node-input event id, delivered to node code via
/// [`crate::journal::PARENT_EVENT_KEY`]. This is the supported way to produce
/// journals that [`ReplayingChatModel`] can later replay — hand-rolled
/// journaling must reproduce the same canonical shapes to be replayable.
pub struct RecordingChatModel {
    inner: Arc<dyn ChatModel>,
    journal: Journal,
    parent: String,
    node_id: Option<String>,
}

impl RecordingChatModel {
    /// A recording wrapper around `inner`, journaling into `journal` with
    /// causal parent `parent` (the current invocation's node-input event id).
    pub fn new(inner: Arc<dyn ChatModel>, journal: Journal, parent: impl Into<String>) -> Self {
        Self {
            inner,
            journal,
            parent: parent.into(),
            node_id: None,
        }
    }

    /// Builder-style: the node this model call is about (recorded as the
    /// event's `node_id`).
    pub fn node(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }

    fn draft(&self) -> EventDraft {
        let mut draft = EventDraft::new(RunEventKind::ModelCall, self.inner.effect())
            .parent(self.parent.clone());
        if let Some(node) = &self.node_id {
            draft = draft.node(node.clone());
        }
        draft
    }
}

#[async_trait]
impl ChatModel for RecordingChatModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        let started = self.journal.clock().now();
        let result = self.inner.chat(messages, tools).await;
        let latency_ms = (self.journal.clock().now() - started)
            .num_milliseconds()
            .max(0) as u64;
        let draft = self
            .draft()
            .input(model_call_request(messages, tools))
            .latency_ms(latency_ms);
        match result {
            Ok(response) => {
                let mut draft = draft.output(model_call_response(&response));
                if let Some(usage) = response.usage {
                    draft = draft.tokens(usage);
                }
                self.journal.record(draft);
                Ok(response)
            }
            Err(error) => {
                self.journal.record(
                    draft
                        .status(EventStatus::Error)
                        .output(json!({ "error": error.to_string() })),
                );
                Err(error)
            }
        }
    }

    fn effect(&self) -> Effect {
        self.inner.effect()
    }
}

/// A [`ChatModel`] that answers every call from a recorded journal instead of
/// executing it.
///
/// The wrapped implementation is carried for its identity ([`ChatModel::effect`])
/// and is **never invoked**: there is no code path from `chat` to `inner`, so
/// replaying with a panic-on-call sentinel (or a client with no credentials
/// and no network) is safe. Each call is matched against the journal by
/// sequence + request hash and answered with the recorded response; the
/// served event is re-journaled into the replay run's journal so the
/// replayed evidence reproduces the recorded evidence byte-for-byte.
pub struct ReplayingChatModel {
    inner: Arc<dyn ChatModel>,
    source: ReplaySource,
    journal: Journal,
    parent: String,
}

impl ReplayingChatModel {
    /// A replaying wrapper around `inner` (never invoked), serving from
    /// `source` and re-journaling into `journal` with causal parent `parent`
    /// (the current invocation's node-input event id).
    pub fn new(
        inner: Arc<dyn ChatModel>,
        source: ReplaySource,
        journal: Journal,
        parent: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            source,
            journal,
            parent: parent.into(),
        }
    }
}

#[async_trait]
impl ChatModel for ReplayingChatModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        let served = self.source.serve(
            RunEventKind::ModelCall,
            &model_call_request(messages, tools),
        )?;
        // Clock-read parity with RecordingChatModel (two reads per call)
        // keeps the logical clock's tick sequence aligned with the recorded
        // run, so the replayed journal's timestamps reproduce it exactly.
        let _started = self.journal.clock().now();
        let _ended = self.journal.clock().now();
        served.rejournal(&self.journal, self.parent.clone());
        match served.event.status {
            EventStatus::Ok => {
                let output = served.output.as_ref().ok_or_else(|| {
                    replay_error(format!(
                        "recorded model call at seq {} succeeded but carries no output payload",
                        served.event.seq
                    ))
                })?;
                chat_response_from_recorded(served.event.seq, output)
            }
            EventStatus::Error => Err(RustyError::Llm(format!(
                "replayed model call at seq {} failed as recorded: {}",
                served.event.seq,
                recorded_error_text(&served)
            ))),
            EventStatus::Interrupted => Err(replay_error(format!(
                "recorded model call at seq {} has status `interrupted`, which model calls \
                 never produce — the journal is inconsistent",
                served.event.seq
            ))),
        }
    }

    fn effect(&self) -> Effect {
        self.inner.effect()
    }
}

/// A [`Tool`] that records every call into the run's journal in the canonical
/// replay-compatible shape, then returns the real result.
///
/// Construct per node invocation like [`RecordingChatModel`]; identity
/// (name, description, schema, effect class) delegates to the wrapped tool.
pub struct RecordingTool {
    inner: Arc<dyn Tool>,
    journal: Journal,
    parent: String,
    node_id: Option<String>,
}

impl RecordingTool {
    /// A recording wrapper around `inner`, journaling into `journal` with
    /// causal parent `parent` (the current invocation's node-input event id).
    pub fn new(inner: Arc<dyn Tool>, journal: Journal, parent: impl Into<String>) -> Self {
        Self {
            inner,
            journal,
            parent: parent.into(),
            node_id: None,
        }
    }

    /// Builder-style: the node this tool call is about (recorded as the
    /// event's `node_id`).
    pub fn node(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn effect(&self) -> Effect {
        self.inner.effect()
    }

    fn effect_kind(&self) -> &str {
        self.inner.effect_kind()
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.inner.idempotency_key(args)
    }

    fn effect_request(&self, call: &ToolCall) -> EffectRequest {
        self.inner.effect_request(call)
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let started = self.journal.clock().now();
        let result = self.inner.call(args.clone()).await;
        let latency_ms = (self.journal.clock().now() - started)
            .num_milliseconds()
            .max(0) as u64;
        let mut draft = EventDraft::new(RunEventKind::ToolCall, self.inner.effect())
            .input(tool_call_request(self.inner.name(), &args))
            .latency_ms(latency_ms)
            .parent(self.parent.clone());
        if let Some(node) = &self.node_id {
            draft = draft.node(node.clone());
        }
        match result {
            Ok(value) => {
                self.journal.record(draft.output(value.clone()));
                Ok(value)
            }
            Err(error) => {
                self.journal.record(
                    draft
                        .status(EventStatus::Error)
                        .output(json!({ "error": error.to_string() })),
                );
                Err(error)
            }
        }
    }
}

/// A [`Tool`] that answers every call from a recorded journal instead of
/// executing it.
///
/// Identity (name, description, schema, effect class) delegates to the
/// wrapped tool, which is **never invoked** — the replaying analogue of
/// [`RecordingTool`]. See [`ReplayingChatModel`] for the matching and
/// zero-outbound guarantees.
pub struct ReplayingTool {
    inner: Arc<dyn Tool>,
    source: ReplaySource,
    journal: Journal,
    parent: String,
}

impl ReplayingTool {
    /// A replaying wrapper around `inner` (never invoked), serving from
    /// `source` and re-journaling into `journal` with causal parent `parent`.
    pub fn new(
        inner: Arc<dyn Tool>,
        source: ReplaySource,
        journal: Journal,
        parent: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            source,
            journal,
            parent: parent.into(),
        }
    }
}

#[async_trait]
impl Tool for ReplayingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn effect(&self) -> Effect {
        self.inner.effect()
    }

    fn effect_kind(&self) -> &str {
        self.inner.effect_kind()
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.inner.idempotency_key(args)
    }

    fn effect_request(&self, call: &ToolCall) -> EffectRequest {
        self.inner.effect_request(call)
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let request = tool_call_request(self.inner.name(), &args);
        let served = self.source.serve(RunEventKind::ToolCall, &request)?;
        // Clock-read parity with RecordingTool; see ReplayingChatModel.
        let _started = self.journal.clock().now();
        let _ended = self.journal.clock().now();
        served.rejournal(&self.journal, self.parent.clone());
        match served.event.status {
            EventStatus::Ok => served.output.clone().ok_or_else(|| {
                replay_error(format!(
                    "recorded tool call at seq {} succeeded but carries no output payload",
                    served.event.seq
                ))
            }),
            EventStatus::Error => Err(RustyError::Tool(format!(
                "replayed tool call at seq {} failed as recorded: {}",
                served.event.seq,
                recorded_error_text(&served)
            ))),
            EventStatus::Interrupted => Err(replay_error(format!(
                "recorded tool call at seq {} has status `interrupted`, which tool calls \
                 never produce — the journal is inconsistent",
                served.event.seq
            ))),
        }
    }
}

/// An exact-replay session over one integrity-verified journal snapshot.
///
/// Construction re-verifies the snapshot (chained head hash, artifact
/// integrity, reference resolution), so a tampered or truncated journal is
/// rejected at the boundary. The typical flow:
///
/// 1. `let replay = ExactReplay::new(snapshot)?;`
/// 2. `let journal = replay.fresh_journal(clock)` — a fresh journal with the
///    recorded run's identity and a matching logical clock;
/// 3. build the same graph topology with [`ReplayingChatModel`] /
///    [`ReplayingTool`] wrappers sourcing from `replay.source()` and
///    re-journaling into that journal;
/// 4. `replay.run_and_verify(&graph, &spec, initial_state, params).await` —
///    drives the run and checks the replayed journal reproduces the recorded
///    one event-for-event.
#[derive(Debug)]
pub struct ExactReplay {
    snapshot: JournalSnapshot,
    source: ReplaySource,
}

impl ExactReplay {
    /// Start an exact-replay session over `snapshot`.
    ///
    /// Fails with [`RustyError::Replay`] when the snapshot fails integrity
    /// verification (tampered head hash, corrupted artifacts, dangling
    /// references), or with [`RustyError::Serialization`] when an event
    /// cannot be re-hashed. Exact replay of *resumed* runs (journals whose
    /// first event is a resume) is deferred: their evidence begins mid-run
    /// against checkpointed state the journal does not carry.
    pub fn new(snapshot: JournalSnapshot) -> Result<Self> {
        verify_snapshot(&snapshot)?;
        if snapshot
            .events
            .first()
            .is_some_and(|event| event.kind == RunEventKind::Resume)
        {
            return Err(replay_error(
                "exact replay of resumed runs is not supported: the journal begins with a \
                 resume event, whose pre-resume state lives in a checkpoint the journal does \
                 not carry — replay the original run's journal instead",
            ));
        }
        let source = ReplaySource::new(&snapshot);
        Ok(Self { snapshot, source })
    }

    /// The recorded snapshot this session replays.
    pub fn snapshot(&self) -> &JournalSnapshot {
        &self.snapshot
    }

    /// The serving cursor over the recorded effects, for building replaying
    /// wrappers. Shared: every wrapper in the replayed graph must serve from
    /// this one cursor.
    pub fn source(&self) -> ReplaySource {
        self.source.clone()
    }

    /// A fresh journal with the recorded run's identity (`run_id`,
    /// `thread_id`), timestamping from `clock`. For byte-identical replay,
    /// `clock` must be a logical clock with the recorded run's parameters
    /// (carried in [`ReplayFixture`] metadata).
    pub fn fresh_journal(&self, clock: Clock) -> Journal {
        Journal::new(
            self.snapshot.run_id.clone(),
            self.snapshot.thread_id.clone(),
            clock,
        )
    }

    /// Drive `graph` over `initial_state`, answering effects from the
    /// recorded journal.
    ///
    /// `params.journal` must carry the recorded run's identity (build it with
    /// [`ExactReplay::fresh_journal`]); a mismatched journal is rejected
    /// rather than silently producing evidence under the wrong run. The
    /// replayed run's outcome and its journal snapshot are returned for
    /// inspection or [`ExactReplay::verify`].
    pub async fn run(
        &self,
        graph: &Graph,
        spec: &StateSpec,
        initial_state: State,
        params: ReplayParams,
    ) -> Result<ReplayOutcome> {
        if params.journal.run_id() != self.snapshot.run_id
            || params.journal.thread_id() != self.snapshot.thread_id
        {
            return Err(replay_error(format!(
                "replay journal identity mismatch: the snapshot records run `{}` on thread \
                 `{}`, but the supplied journal is run `{}` on thread `{}` — build the replay \
                 journal with ExactReplay::fresh_journal",
                self.snapshot.run_id,
                self.snapshot.thread_id,
                params.journal.run_id(),
                params.journal.thread_id()
            )));
        }
        let executor = match &params.checkpointer {
            Some(checkpointer) => Executor::with_checkpointer(checkpointer.clone()),
            None => Executor::new(),
        };
        let config = RunConfig::new(self.snapshot.thread_id.clone())
            .with_max_steps(params.max_steps)
            .with_rng(params.rng.clone())
            .with_journal(params.journal.clone());
        let outcome = executor.run(graph, spec, initial_state, config).await?;
        Ok(ReplayOutcome {
            outcome,
            journal: params.journal.snapshot(),
        })
    }

    /// Verify a replayed journal against the recorded one: every recorded
    /// effect must have been served, and the two snapshots must agree on run
    /// identity, every event, every artifact, and the head hash. The first
    /// disagreement fails with a descriptive [`RustyError::Replay`].
    pub fn verify(&self, replayed: &JournalSnapshot) -> Result<()> {
        if !self.source.is_exhausted() {
            return Err(replay_error(format!(
                "exact replay left recorded effects unserved: {:?} — the replayed run stopped \
                 short of the journaled history",
                self.source.remaining()
            )));
        }
        let recorded = &self.snapshot;
        if recorded.run_id != replayed.run_id || recorded.thread_id != replayed.thread_id {
            return Err(replay_error(format!(
                "replay identity mismatch: recorded run `{}`/thread `{}` vs replayed \
                 `{}`/`{}`",
                recorded.run_id, recorded.thread_id, replayed.run_id, replayed.thread_id
            )));
        }
        if recorded.events.len() != replayed.events.len() {
            return Err(replay_error(format!(
                "replay event-count mismatch: the recorded journal holds {} events but the \
                 replayed journal holds {}",
                recorded.events.len(),
                replayed.events.len()
            )));
        }
        for (recorded_event, replayed_event) in recorded.events.iter().zip(&replayed.events) {
            if recorded_event != replayed_event {
                return Err(replay_error(format!(
                    "replay event mismatch at seq {}: recorded {} but replayed {}",
                    recorded_event.seq,
                    summarize_event(recorded_event),
                    summarize_event(replayed_event),
                )));
            }
        }
        if recorded.artifacts != replayed.artifacts {
            return Err(replay_error(
                "replay artifact mismatch: the replayed journal's artifact payloads differ \
                 from the recorded ones",
            ));
        }
        if recorded.head_hash != replayed.head_hash {
            return Err(replay_error(format!(
                "replay head-hash mismatch: recorded {} but replayed {} — event content \
                 agrees but the evidence chain does not (this should be unreachable; please \
                 report it)",
                recorded.head_hash, replayed.head_hash
            )));
        }
        Ok(())
    }

    /// [`ExactReplay::run`] plus [`ExactReplay::verify`]: the replay either
    /// reproduces the recorded evidence exactly or fails with the first
    /// disagreement.
    pub async fn run_and_verify(
        &self,
        graph: &Graph,
        spec: &StateSpec,
        initial_state: State,
        params: ReplayParams,
    ) -> Result<ReplayOutcome> {
        let replayed = self.run(graph, spec, initial_state, params).await?;
        self.verify(&replayed.journal)?;
        Ok(replayed)
    }
}

/// One-line event summary for verification errors (kind, node, status —
/// enough to locate the divergence without dumping payloads).
fn summarize_event(event: &RunEvent) -> String {
    format!(
        "{:?}(node={:?}, status={:?})",
        event.kind, event.node_id, event.status
    )
}

/// Per-run parameters of an exact replay: the fresh journal (recorded run's
/// identity plus matching logical clock), the RNG (the recorded run's seed
/// for byte-identical checkpoint ids), an optional checkpointer (present iff
/// the recorded run had one — checkpoint-written events are part of the
/// evidence), and the step limit.
pub struct ReplayParams {
    /// The replay run's journal; build with [`ExactReplay::fresh_journal`].
    pub journal: Journal,

    /// The replay run's randomness source. [`RngSource::seeded`] with the
    /// recorded run's seed reproduces checkpoint ids; anything else makes the
    /// replayed journal diverge at the first checkpoint-written event.
    pub rng: RngSource,

    /// Persistence for the replay run. Must be `Some` when the recorded run
    /// journaled checkpoint-written events, `None` when it did not.
    pub checkpointer: Option<Arc<dyn Checkpointer>>,

    /// Super-step limit, as in [`RunConfig::max_steps`].
    pub max_steps: usize,
}

impl ReplayParams {
    /// Parameters over `journal` and `rng` with no checkpointer and the
    /// default step limit.
    pub fn new(journal: Journal, rng: RngSource) -> Self {
        Self {
            journal,
            rng,
            checkpointer: None,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    /// Builder-style: persist replay checkpoints through `checkpointer`.
    pub fn with_checkpointer(mut self, checkpointer: Arc<dyn Checkpointer>) -> Self {
        self.checkpointer = Some(checkpointer);
        self
    }

    /// Builder-style: override the step limit.
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }
}

/// The result of an exact replay: the run's outcome and the replayed journal
/// (export form, comparable against the recorded snapshot).
#[derive(Debug)]
pub struct ReplayOutcome {
    /// How the replayed run ended. Exact replay of an interrupted recorded
    /// run ends in the same `Interrupted` outcome with the same payload.
    pub outcome: ExecutionOutcome,

    /// The replayed run's journal snapshot. After
    /// [`ExactReplay::run_and_verify`], this is event-for-event identical to
    /// the recorded snapshot.
    pub journal: JournalSnapshot,
}

/// Token and cost totals of one branch of a [`BranchDiff`]: the sum over all
/// of the branch's events (the `tokens` field is only ever set on model
/// calls, so this is the branch's model usage).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BranchTotals {
    /// Number of events in the branch.
    pub events: usize,

    /// Summed token usage across the branch's model calls.
    pub tokens: Usage,

    /// Summed recorded cost in USD across the branch's events.
    pub cost_usd: f64,
}

/// One channel's differing value at one super-step of a [`BranchDiff`].
/// `None` means the channel was absent from that branch's super-step-end
/// record at that step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelDiff {
    /// The state channel that differs.
    pub channel: String,

    /// The post-reducer value in the base branch, when present.
    pub base: Option<Value>,

    /// The post-reducer value in the branch, when present.
    pub branch: Option<Value>,
}

/// The state-channel differences of one super-step between two branches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepDiff {
    /// The super-step index (from the super-step-start event's input).
    pub step: u64,

    /// Channels whose post-reducer values differ at this step (presence or
    /// value), sorted by channel name.
    pub channels: Vec<ChannelDiff>,
}

/// The structural diff between two journal snapshots — typically two branches
/// forked from a common point: one replayed an existing history, the other
/// continued differently.
///
/// Events are compared **logically**: kind, node, sequence, effect class,
/// resolved input/output payloads, latency, tokens, cost, and status must
/// match, while identity and timing fields (`id`, `run_id`, `thread_id`,
/// `parent`, `recorded_at`) are excluded — two branches are separate runs,
/// so their identity-bearing fields legitimately differ even when their
/// evidence is the same. The first logically-unequal sequence position is the
/// divergence point; everything after it is reported as removed (base) or
/// added (branch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchDiff {
    /// The first sequence number where the branches' evidence differs, or
    /// `None` when they are logically identical.
    pub first_divergent_seq: Option<u64>,

    /// Events present in the branch at and after the divergence point (the
    /// branch's new work relative to the base).
    pub added: Vec<RunEvent>,

    /// Events present in the base at and after the divergence point (the work
    /// the branch replaced).
    pub removed: Vec<RunEvent>,

    /// State-channel differences at each super-step, in step order. Only
    /// steps with at least one differing channel appear.
    pub step_diffs: Vec<StepDiff>,

    /// Totals over the whole base journal.
    pub base_totals: BranchTotals,

    /// Totals over the whole branch journal.
    pub branch_totals: BranchTotals,
}

impl BranchDiff {
    /// Diff `branch` against `base`. When both snapshots share a common
    /// history (a fork), the shared prefix compares equal and the divergence
    /// point is where the fork's branches parted; `fork_seq`-style explicit
    /// fork points are unnecessary — the evidence carries the cut.
    pub fn between(base: &JournalSnapshot, branch: &JournalSnapshot) -> BranchDiff {
        let divergence = (0..base.events.len().max(branch.events.len()))
            .find(|&i| {
                match (base.events.get(i), branch.events.get(i)) {
                    (Some(a), Some(b)) => !logically_equal(a, base, b, branch),
                    // One side exhausted: divergence at the first missing seq.
                    _ => true,
                }
            })
            .map(|i| {
                base.events
                    .get(i)
                    .or_else(|| branch.events.get(i))
                    .map(|event| event.seq)
                    .unwrap_or(i as u64)
            });
        let (removed, added) = match divergence {
            Some(seq) => (
                base.events
                    .iter()
                    .filter(|event| event.seq >= seq)
                    .cloned()
                    .collect(),
                branch
                    .events
                    .iter()
                    .filter(|event| event.seq >= seq)
                    .cloned()
                    .collect(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        BranchDiff {
            first_divergent_seq: divergence,
            added,
            removed,
            step_diffs: diff_steps(base, branch),
            base_totals: totals(base),
            branch_totals: totals(branch),
        }
    }

    /// `true` when the two branches are logically identical (no divergence
    /// point, nothing added or removed).
    pub fn is_identical(&self) -> bool {
        self.first_divergent_seq.is_none()
    }
}

/// Logical evidence equality, excluding identity and timing fields (see
/// [`BranchDiff`]). Payloads compare by resolved value, so an inline payload
/// and an artifact reference carrying the same bytes compare equal.
fn logically_equal(
    a: &RunEvent,
    snapshot_a: &JournalSnapshot,
    b: &RunEvent,
    snapshot_b: &JournalSnapshot,
) -> bool {
    a.seq == b.seq
        && a.kind == b.kind
        && a.node_id == b.node_id
        && a.effect == b.effect
        && a.latency_ms == b.latency_ms
        && a.tokens == b.tokens
        && a.cost_usd == b.cost_usd
        && a.status == b.status
        && resolve_opt(snapshot_a, a.input.as_ref()) == resolve_opt(snapshot_b, b.input.as_ref())
        && resolve_opt(snapshot_a, a.output.as_ref()) == resolve_opt(snapshot_b, b.output.as_ref())
}

/// Sum token usage and cost over a snapshot's events.
fn totals(snapshot: &JournalSnapshot) -> BranchTotals {
    let mut totals = BranchTotals {
        events: snapshot.events.len(),
        ..BranchTotals::default()
    };
    for event in &snapshot.events {
        if let Some(usage) = event.tokens {
            totals.tokens.prompt_tokens = totals
                .tokens
                .prompt_tokens
                .saturating_add(usage.prompt_tokens);
            totals.tokens.completion_tokens = totals
                .tokens
                .completion_tokens
                .saturating_add(usage.completion_tokens);
            totals.tokens.total_tokens = totals
                .tokens
                .total_tokens
                .saturating_add(usage.total_tokens);
        }
        if let Some(cost) = event.cost_usd {
            totals.cost_usd += cost;
        }
    }
    totals
}

/// The post-reducer channel values of every super-step in a snapshot, keyed
/// by step index: super-step-start events supply the index, the following
/// super-step-end event supplies the channel values.
fn step_channel_values(snapshot: &JournalSnapshot) -> BTreeMap<u64, BTreeMap<String, Value>> {
    let mut steps = BTreeMap::new();
    let mut current_step: Option<u64> = None;
    for event in &snapshot.events {
        match event.kind {
            RunEventKind::SuperStepStart => {
                current_step = resolve_opt(snapshot, event.input.as_ref())
                    .and_then(|input| input.get("step").and_then(Value::as_u64));
            }
            RunEventKind::SuperStepEnd => {
                if let Some(step) = current_step.take() {
                    let channels = resolve_opt(snapshot, event.output.as_ref())
                        .and_then(|output| output.as_object().cloned())
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    steps.insert(step, channels);
                }
            }
            _ => {}
        }
    }
    steps
}

/// Per-step channel diffs between two snapshots (only steps with at least one
/// differing channel, channels sorted).
fn diff_steps(base: &JournalSnapshot, branch: &JournalSnapshot) -> Vec<StepDiff> {
    let base_steps = step_channel_values(base);
    let branch_steps = step_channel_values(branch);
    let mut diffs = Vec::new();
    for step in base_steps
        .keys()
        .chain(branch_steps.keys())
        .copied()
        .collect::<std::collections::BTreeSet<u64>>()
    {
        let empty = BTreeMap::new();
        let base_channels = base_steps.get(&step).unwrap_or(&empty);
        let branch_channels = branch_steps.get(&step).unwrap_or(&empty);
        let mut channels = Vec::new();
        for channel in base_channels
            .keys()
            .chain(branch_channels.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<String>>()
        {
            let base_value = base_channels.get(&channel).cloned();
            let branch_value = branch_channels.get(&channel).cloned();
            if base_value != branch_value {
                channels.push(ChannelDiff {
                    channel,
                    base: base_value,
                    branch: branch_value,
                });
            }
        }
        if !channels.is_empty() {
            diffs.push(StepDiff { step, channels });
        }
    }
    diffs
}

/// Determinism parameters of a recorded run, carried in [`FixtureMetadata`]
/// so [`ReplayFixture::replay_in_ci`] can reproduce the run byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalClockParams {
    /// The logical clock's start (epoch millis), as in [`Clock::logical`].
    pub start_ms: u64,

    /// The logical clock's per-read tick (millis), as in [`Clock::logical`].
    pub tick_ms: u64,
}

/// Provenance and determinism metadata of a [`ReplayFixture`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureMetadata {
    /// Human-readable fixture name (used in error messages).
    pub name: String,

    /// The recorded run's logical-clock parameters. Required for CI replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<LogicalClockParams>,

    /// The recorded run's RNG seed. Required for CI replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rng_seed: Option<u64>,
}

/// A portable, self-contained exact-replay bundle: everything CI (or another
/// machine, or a later wave's server endpoint) needs to re-drive a recorded
/// run and prove it reproduces.
///
/// A fixture carries the graph's topology hash (replay refuses a structurally
/// different graph), the full journal snapshot (integrity-verified on
/// import), the recorded final checkpoint (replay compares the outcome state
/// against it), and the determinism parameters the run was recorded with.
/// Serialization is plain JSON; [`ReplayFixture::export`] /
/// [`ReplayFixture::import`] are the wire boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFixture {
    /// Fixture envelope version; [`FIXTURE_FORMAT_VERSION`] for anything
    /// written now.
    pub format_version: u32,

    /// SHA-256 of the recorded graph's topology (see
    /// [`Graph::topology_hash`]). Checked before replay: a different topology
    /// cannot reproduce the journaled evidence.
    pub graph_hash: String,

    /// The application-declared graph version of the recorded run.
    pub graph_version: String,

    /// The recorded run's complete journal.
    pub journal: JournalSnapshot,

    /// The recorded run's final checkpoint, when it ran with a checkpointer.
    /// CI replay compares the replayed outcome state against it.
    pub final_checkpoint: Option<Checkpoint>,

    /// Provenance and determinism parameters.
    pub metadata: FixtureMetadata,
}

impl ReplayFixture {
    /// Capture a recorded run as a fixture. `graph` supplies the topology
    /// hash; `clock`/`rng_seed` are the determinism parameters the run was
    /// recorded with (both required for later CI replay).
    pub fn capture(
        name: impl Into<String>,
        graph: &Graph,
        graph_version: impl Into<String>,
        journal: JournalSnapshot,
        final_checkpoint: Option<Checkpoint>,
        clock: Option<LogicalClockParams>,
        rng_seed: Option<u64>,
    ) -> Self {
        Self {
            format_version: FIXTURE_FORMAT_VERSION,
            graph_hash: graph.topology_hash(),
            graph_version: graph_version.into(),
            journal,
            final_checkpoint,
            metadata: FixtureMetadata {
                name: name.into(),
                clock,
                rng_seed,
            },
        }
    }

    /// Serialize to pretty-printed JSON (the checked-in / on-disk form).
    pub fn export(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse a fixture from JSON, rejecting unsupported format versions and
    /// integrity failures (tampered or truncated journals) at the boundary.
    pub fn import(json: &str) -> Result<Self> {
        let fixture: Self = serde_json::from_str(json)?;
        if fixture.format_version != FIXTURE_FORMAT_VERSION {
            return Err(replay_error(format!(
                "unsupported fixture format version {} (this build supports \
                 {FIXTURE_FORMAT_VERSION}) — regenerate the fixture or upgrade the runtime",
                fixture.format_version
            )));
        }
        verify_snapshot(&fixture.journal)?;
        Ok(fixture)
    }

    /// An exact-replay session over the fixture's journal (integrity already
    /// verified by [`ReplayFixture::import`] or construction; re-verified
    /// here so a hand-assembled fixture is checked too).
    pub fn exact_replay(&self) -> Result<ExactReplay> {
        ExactReplay::new(self.journal.clone())
    }

    /// The replay parameters matching the fixture's determinism metadata: a
    /// fresh journal on the recorded identity and logical clock, the recorded
    /// RNG seed, and a fresh in-memory checkpointer (fixtures record runs
    /// that used one — the checkpoint-written events are part of the
    /// evidence).
    ///
    /// Fails with [`RustyError::Replay`] when the fixture lacks clock or seed
    /// metadata (a run recorded without determinism seams cannot be replayed
    /// byte-identically).
    pub fn replay_params(&self, replay: &ExactReplay) -> Result<ReplayParams> {
        let clock = self.metadata.clock.ok_or_else(|| {
            replay_error(format!(
                "fixture `{}` carries no logical-clock parameters; exact CI replay requires \
                 the recorded run's determinism seams",
                self.metadata.name
            ))
        })?;
        let seed = self.metadata.rng_seed.ok_or_else(|| {
            replay_error(format!(
                "fixture `{}` carries no RNG seed; exact CI replay requires the recorded \
                 run's determinism seams",
                self.metadata.name
            ))
        })?;
        let journal = replay.fresh_journal(Clock::logical(clock.start_ms, clock.tick_ms));
        Ok(ReplayParams::new(journal, RngSource::seeded(seed))
            .with_checkpointer(Arc::new(InMemoryCheckpointer::new())))
    }

    /// Replay the fixture end to end and verify everything: the graph's
    /// topology matches the recorded one, the run reproduces the journaled
    /// evidence event-for-event, and the outcome state matches the recorded
    /// final checkpoint.
    ///
    /// The caller supplies the replay session (`fixture.exact_replay()?`), a
    /// graph built with replaying wrappers over `replay.source()`, and the
    /// parameters from `fixture.replay_params(&replay)?` — the same journal
    /// handle the wrappers re-journal into. This is the `replay_in_ci` entry
    /// point: one call from recorded artifact to verified replay.
    pub async fn replay_in_ci(
        &self,
        replay: ExactReplay,
        graph: &Graph,
        spec: &StateSpec,
        initial_state: State,
        params: ReplayParams,
    ) -> Result<ReplayOutcome> {
        let actual_hash = graph.topology_hash();
        if actual_hash != self.graph_hash {
            return Err(replay_error(format!(
                "fixture `{}` was recorded against graph topology {}, but the graph under \
                 replay hashes to {actual_hash} — rebuild the recorded topology (same node \
                 names, edges, and entry point)",
                self.metadata.name, self.graph_hash
            )));
        }
        let replayed = replay
            .run_and_verify(graph, spec, initial_state, params)
            .await?;
        if let Some(final_checkpoint) = &self.final_checkpoint {
            if replayed.outcome.state() != &final_checkpoint.state {
                return Err(replay_error(format!(
                    "fixture `{}`: the replayed final state diverges from the recorded final \
                     checkpoint (journal evidence agreed; state did not)",
                    self.metadata.name
                )));
            }
        }
        Ok(replayed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use serde_json::json;

    fn snapshot_with(drafts: Vec<EventDraft>) -> JournalSnapshot {
        let journal = Journal::new("run-t", "thread-t", Clock::logical(1_000_000, 5));
        for draft in drafts {
            journal.record(draft);
        }
        journal.snapshot()
    }

    fn model_draft(request: Value, response: Value) -> EventDraft {
        EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
            .node("agent")
            .input(request)
            .output(response)
            .latency_ms(2)
            .tokens(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            })
    }

    #[test]
    fn canonical_shapes_have_stable_hashes() {
        let request = model_call_request(&[ChatMessage::user("ping")], &[]);
        assert_eq!(
            request,
            json!({"messages": [{"role": "user", "content": "ping"}], "tools": []})
        );
        let first = request_hash(&request).unwrap();
        let second = request_hash(&model_call_request(&[ChatMessage::user("ping")], &[])).unwrap();
        assert_eq!(first, second);
        // A different request hashes differently.
        let other = request_hash(&model_call_request(&[ChatMessage::user("pong")], &[])).unwrap();
        assert_ne!(first, other);

        let tool = tool_call_request("echo", &json!({"text": "hi"}));
        assert_eq!(tool, json!({"tool": "echo", "arguments": {"text": "hi"}}));
    }

    #[test]
    fn serve_matches_by_sequence_and_request_hash() {
        let snapshot = snapshot_with(vec![
            model_draft(
                model_call_request(&[ChatMessage::user("a")], &[]),
                json!({"r": 1}),
            ),
            model_draft(
                model_call_request(&[ChatMessage::user("b")], &[]),
                json!({"r": 2}),
            ),
        ]);
        let source = ReplaySource::new(&snapshot);
        assert!(!source.is_exhausted());

        // Wrong request at the cursor: divergence, and the cursor must not advance.
        let error = source
            .serve(
                RunEventKind::ModelCall,
                &model_call_request(&[ChatMessage::user("zzz")], &[]),
            )
            .unwrap_err();
        assert!(matches!(error, RustyError::Replay(_)));
        assert!(error.to_string().contains("divergence"));
        assert_eq!(source.remaining().len(), 2);

        // Wrong kind at the cursor: order violation.
        let error = source
            .serve(RunEventKind::ToolCall, &tool_call_request("t", &json!({})))
            .unwrap_err();
        assert!(error.to_string().contains("order violation"));

        // Correct sequence serves in order and exhausts.
        let first = source
            .serve(
                RunEventKind::ModelCall,
                &model_call_request(&[ChatMessage::user("a")], &[]),
            )
            .unwrap();
        assert_eq!(first.event.seq, 0);
        assert_eq!(first.output, Some(json!({"r": 1})));
        let second = source
            .serve(
                RunEventKind::ModelCall,
                &model_call_request(&[ChatMessage::user("b")], &[]),
            )
            .unwrap();
        assert_eq!(second.event.seq, 1);
        assert!(source.is_exhausted());

        // Serving past the end is a loud exhaustion error.
        let error = source
            .serve(
                RunEventKind::ModelCall,
                &model_call_request(&[ChatMessage::user("a")], &[]),
            )
            .unwrap_err();
        assert!(error.to_string().contains("exhaustion"));
    }

    #[test]
    fn serve_rejects_non_servable_kinds() {
        let snapshot = snapshot_with(vec![EventDraft::new(
            RunEventKind::SuperStepStart,
            Effect::Pure,
        )]);
        let error = ReplaySource::new(&snapshot)
            .serve(RunEventKind::NodeInput, &json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("not a servable"));
    }

    #[test]
    fn artifact_backed_requests_match_and_resolve() {
        let big_request =
            json!({"messages": [{"role": "user", "content": "x".repeat(9000)}], "tools": []});
        let big_response = json!({"message": {"role": "assistant", "content": "y".repeat(9000)}, "model": null, "usage": null});
        let snapshot = snapshot_with(vec![model_draft(big_request.clone(), big_response.clone())]);
        // The payloads were promoted to artifacts at record time.
        assert!(matches!(
            snapshot.events[0].input,
            Some(PayloadRef::Artifact(_))
        ));
        let served = ReplaySource::new(&snapshot)
            .serve(RunEventKind::ModelCall, &big_request)
            .unwrap();
        assert_eq!(served.output, Some(big_response));
    }

    #[test]
    fn tampered_snapshots_are_rejected_at_the_boundary() {
        let snapshot = snapshot_with(vec![model_draft(
            model_call_request(&[ChatMessage::user("a")], &[]),
            json!({"message": {"role": "assistant", "content": "r"}, "model": null, "usage": null}),
        )]);

        // Event tampering breaks the chained head hash.
        let mut tampered = snapshot.clone();
        tampered.events[0].status = EventStatus::Error;
        assert!(ExactReplay::new(tampered).is_err());

        // Artifact tampering breaks content addressing (the head hash chains
        // over events, so this needs the dedicated integrity check).
        let big = json!({"messages": [{"role": "user", "content": "x".repeat(9000)}], "tools": []});
        let artifact_snapshot = snapshot_with(vec![model_draft(big, json!({"ok": true}))]);
        let mut tampered = artifact_snapshot;
        *tampered.artifacts.values_mut().next().unwrap() = json!({"forged": true});
        let error = ExactReplay::new(tampered).unwrap_err();
        assert!(error.to_string().contains("integrity"));

        // A truncated snapshot (artifact removed) is rejected.
        let big = json!({"messages": [{"role": "user", "content": "x".repeat(9000)}], "tools": []});
        let truncated = snapshot_with(vec![model_draft(big, json!({"ok": true}))]);
        let mut truncated = truncated;
        truncated.artifacts.clear();
        assert!(ExactReplay::new(truncated).is_err());
    }

    #[test]
    fn resumed_run_journals_are_rejected() {
        let snapshot = snapshot_with(vec![EventDraft::new(RunEventKind::Resume, Effect::Pure)
            .input(json!({"checkpoint_id": "c"}))]);
        let error = ExactReplay::new(snapshot).unwrap_err();
        assert!(error.to_string().contains("resumed"));
    }

    #[test]
    fn rejournal_reproduces_the_recorded_event() {
        let recorded = snapshot_with(vec![model_draft(
            model_call_request(&[ChatMessage::user("a")], &[]),
            json!({"message": {"role": "assistant", "content": "r"}, "model": null, "usage": null}),
        )]);
        let source = ReplaySource::new(&recorded);
        let served = source
            .serve(
                RunEventKind::ModelCall,
                &model_call_request(&[ChatMessage::user("a")], &[]),
            )
            .unwrap();
        let replay_journal = Journal::new("run-t", "thread-t", Clock::logical(1_000_000, 5));
        // Same draft sequence: one record read per event on the same clock.
        served.rejournal(&replay_journal, "run-t:parent");
        let replayed = replay_journal.snapshot();
        assert_eq!(replayed.events.len(), 1);
        // Identical except for the causal parent we supplied.
        let mut expected = recorded.events[0].clone();
        expected.parent = Some("run-t:parent".into());
        assert_eq!(replayed.events[0], expected);
    }

    #[test]
    fn branch_diff_reports_divergence_steps_and_totals() {
        let shared = || {
            vec![
                EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
                    .input(json!({"step": 0, "active_nodes": ["a"]})),
                EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                    .output(json!({"log": ["a"]})),
            ]
        };
        let mut base_drafts = shared();
        base_drafts.push(
            EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
                .input(json!({"step": 1, "active_nodes": ["b"]})),
        );
        base_drafts.push(
            EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                .output(json!({"log": ["a", "b"]})),
        );
        base_drafts.push(
            model_draft(
                model_call_request(&[ChatMessage::user("q")], &[]),
                json!({"r": 1}),
            )
            .cost_usd(0.001),
        );
        let base = snapshot_with(base_drafts);

        let mut branch_drafts = shared();
        branch_drafts.push(
            EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
                .input(json!({"step": 1, "active_nodes": ["c"]})),
        );
        branch_drafts.push(
            EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                .output(json!({"log": ["a", "c"], "extra": 1})),
        );
        let branch = snapshot_with(branch_drafts);

        let diff = BranchDiff::between(&base, &branch);
        assert!(!diff.is_identical());
        assert_eq!(diff.first_divergent_seq, Some(2));
        assert_eq!(diff.removed.len(), 3);
        assert_eq!(diff.added.len(), 2);
        assert_eq!(diff.base_totals.events, 5);
        assert_eq!(diff.branch_totals.events, 4);
        assert_eq!(diff.base_totals.tokens.total_tokens, 15);
        assert_eq!(diff.branch_totals.tokens.total_tokens, 0);
        assert_eq!(diff.base_totals.cost_usd, 0.001);
        assert_eq!(diff.branch_totals.cost_usd, 0.0);

        // Step 0 is shared; step 1 differs on `log` and `extra`.
        assert_eq!(diff.step_diffs.len(), 1);
        let step = &diff.step_diffs[0];
        assert_eq!(step.step, 1);
        assert_eq!(step.channels.len(), 2);
        assert_eq!(step.channels[0].channel, "extra");
        assert_eq!(step.channels[0].base, None);
        assert_eq!(step.channels[0].branch, Some(json!(1)));
        assert_eq!(step.channels[1].channel, "log");
        assert_eq!(step.channels[1].base, Some(json!(["a", "b"])));
        assert_eq!(step.channels[1].branch, Some(json!(["a", "c"])));
    }

    #[test]
    fn branch_diff_of_identical_snapshots_is_empty() {
        // Two separate runs of the same drafts: identity fields (ids,
        // timestamps) match here too, but the point is the logical comparison.
        let snapshot = snapshot_with(vec![EventDraft::new(
            RunEventKind::SuperStepStart,
            Effect::Pure,
        )
        .input(json!({"step": 0, "active_nodes": ["a"]}))]);
        let diff = BranchDiff::between(&snapshot, &snapshot);
        assert!(diff.is_identical());
        assert!(diff.added.is_empty() && diff.removed.is_empty());
        assert!(diff.step_diffs.is_empty());
    }

    #[test]
    fn fixture_roundtrip_and_version_guard() {
        let journal = snapshot_with(vec![model_draft(
            model_call_request(&[ChatMessage::user("a")], &[]),
            json!({"message": {"role": "assistant", "content": "r"}, "model": null, "usage": null}),
        )]);
        let mut builder = crate::graph::GraphBuilder::new();
        builder.add_node("agent", |_ctx: crate::node::NodeContext| async {
            Ok(crate::node::NodeOutput::empty())
        });
        builder.set_entry_point("agent");
        let graph = builder.compile().unwrap();
        let fixture = ReplayFixture::capture(
            "roundtrip",
            &graph,
            "v1",
            journal,
            None,
            Some(LogicalClockParams {
                start_ms: 1_000_000,
                tick_ms: 5,
            }),
            Some(7),
        );
        let wire = fixture.export().unwrap();
        let imported = ReplayFixture::import(&wire).unwrap();
        assert_eq!(imported.export().unwrap(), wire);

        // An unsupported format version is rejected with a descriptive error.
        let mut value: Value = serde_json::from_str(&wire).unwrap();
        value["format_version"] = json!(99);
        let error = ReplayFixture::import(&value.to_string()).unwrap_err();
        assert!(error.to_string().contains("format version 99"));
    }
}
