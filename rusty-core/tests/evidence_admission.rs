//! Evidence & admission wave integration tests.
//!
//! Five test groups:
//!
//! - **Golden pins** — the serialized shapes of the wave's new event kinds,
//!   `RunConfigDeclaration`, and the closed `ApprovalDecision` vocabulary are
//!   pinned against checked-in JSON under `tests/golden/`. To bless an
//!   intentional contract change, re-run with `UPDATE_GOLDEN=1` and review
//!   the diff. (The full-enum prefix pin in `agents.rs` stays untouched:
//!   this wave pins its own appended variants, the convention every wave
//!   follows.)
//! - **Run-config envelope** — a run that pins a manifest, an allowlist, or
//!   explicit versions journals `RunConfigDeclared` once at start with the
//!   resolved fields; a default run journals nothing (old journals keep
//!   replaying); exact replay serves the envelope from the log and `verify`
//!   names the first divergent field.
//! - **Deny-only tool guards** — a guard denies an allowlisted tool, the
//!   denial is journaled (`ToolCallDenied`), the tool body never runs, and
//!   the model sees the `ERROR:` tool message; registration order cannot
//!   hide a denial; exact replay re-derives the denial from the same guards
//!   instead of serving it.
//! - **Approval pairs** — the gate journals the asked/decided pair with the
//!   causal parents; the closed vocabulary grants only through
//!   `ApprovedOnce`; the composer publish gate journals every admission path
//!   (approved, mis-scoped, missing) and stays byte-identical without a
//!   gate.
//! - **Chunk capture** — `RecordingChatModel::chat_stream` optionally
//!   journals the streamed chunks; the default journals the pre-wave shape
//!   exactly.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::composer::{
    publish_effect_id, ComposeSkillTool, ComposerSession, PublishComposedSkillTool,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, TokenChunk, ToolCall};
use rusty_agent_runtime::react::{
    create_react_agent_replaying, create_react_agent_with_recording, MESSAGES_CHANNEL,
};
use rusty_agent_runtime::record::{
    ApprovalDecision, ApprovalRequest, Effect, PayloadRef, PolicyVersion, RunConfigDeclaration,
    RunEvent, RunEventKind, RunManifest,
};
use rusty_agent_runtime::replay::{ExactReplay, RecordingChatModel, ReplayParams};
use rusty_agent_runtime::skill::SkillRegistry;
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::approval::ApprovalGate;
use rusty_agent_runtime::tool::{GuardDenial, GuardedCall, Tool, ToolGuard, ToolRegistry};

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

#[test]
fn golden_evidence_admission_event_kinds() {
    // The wave's appended variants, in declaration order — appended only,
    // never renamed or reordered, so pre-wave journals keep deserializing.
    assert_golden(
        "evidence_admission_event_kinds.json",
        &vec![
            RunEventKind::RunConfigDeclared,
            RunEventKind::ToolCallDenied,
            RunEventKind::ApprovalAsked,
            RunEventKind::ApprovalDecided,
        ],
    );
}

#[test]
fn golden_run_config_declaration_shape() {
    let mut tool_schemas = BTreeMap::new();
    tool_schemas.insert(
        "echo".to_owned(),
        "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7".to_owned(),
    );
    let declaration = RunConfigDeclaration {
        graph_version: "graph-v3".into(),
        graph_hash: "ab8131f2e59c4b8dbf2b1808ddcf2b53cb75b4e2caad7d8ca2472740b13c1d2e".into(),
        policy_version: PolicyVersion::new(PolicyVersion::STATIC_V0),
        tool_allowlist: Some(vec!["echo".into(), "search".into()]),
        manifest: Some(RunManifest {
            model: Some("gpt-5.2-2026-06-01".into()),
            tool_schemas,
            ..RunManifest::default()
        }),
    };
    assert_golden("run_config_declaration.json", &declaration);
}

#[test]
fn golden_approval_decision_shape() {
    // The whole closed vocabulary: one granting variant, three denying
    // ones, each in its wire shape.
    assert_golden(
        "approval_decision.json",
        &vec![
            ApprovalDecision::ApprovedOnce {
                approved_by: "ops:ada".into(),
            },
            ApprovalDecision::Rejected {
                decided_by: "ops:ada".into(),
                reason: Some("out of policy".into()),
            },
            ApprovalDecision::Cancelled { reason: None },
            ApprovalDecision::Unavailable {
                reason: Some("no approval token presented".into()),
            },
        ],
    );
}

// ---------- determinism parameters shared by record and replay ----------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;
const RUN_ID: &str = "run-evidence";
const THREAD_ID: &str = "t-evidence";

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("say hello")).unwrap()]
    }))
    .unwrap()
}

// ---------- models and tools: scripted (record), panic sentinels (replay) ----------

/// A scripted model: pops one canned response per `chat` call.
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn react_script() -> Self {
        Self {
            script: Mutex::new(
                vec![
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "c1",
                        "echo",
                        json!({"text": "hello"}),
                    )]),
                    ChatMessage::assistant("the echo said: hello"),
                ]
                .into(),
            ),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted-react-1".into()),
            usage: None,
        })
    }
}

/// A model that panics if it is ever called. Exact replay must never reach
/// it — the counter makes "never called" assertable rather than implied.
struct PanicModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChatModel for PanicModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicModel was invoked")
    }
}

/// A lookup tool with an honest effect declaration and a call counter, so a
/// guard-blocked dispatch can prove the body never ran.
struct EchoTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

/// A tool that panics if it is ever called. Identity (name, description,
/// schema, effect class) is identical to `EchoTool`'s: tool schemas feed
/// the model-call request hash, so the replay registry must match the
/// recorded one byte-for-byte.
struct PanicTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicTool was invoked")
    }
}

fn tools(calls: Arc<AtomicUsize>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool { calls });
    registry
}

fn sentinel_tools(calls: Arc<AtomicUsize>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(PanicTool { calls });
    registry
}

// ---------- guards ----------

/// A guard that denies the `echo` tool, naming itself and its reason.
#[derive(Debug)]
struct DenyEcho;

impl ToolGuard for DenyEcho {
    fn name(&self) -> &str {
        "deny-echo"
    }
    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        (call.tool == "echo")
            .then(|| GuardDenial::new("deny-echo", "echo is blocked by policy"))
    }
}

/// A second, independently named guard that also denies `echo` — the
/// any-denial-denies composition test needs two denials to list.
#[derive(Debug)]
struct DenyEchoOps;

impl ToolGuard for DenyEchoOps {
    fn name(&self) -> &str {
        "deny-echo-ops"
    }
    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        (call.tool == "echo").then(|| GuardDenial::new("deny-echo-ops", "ops freeze on echo"))
    }
}

/// A guard that never denies — silence, not permission.
#[derive(Debug)]
struct SilentGuard;

impl ToolGuard for SilentGuard {
    fn name(&self) -> &str {
        "silent"
    }
    fn check(&self, _call: &GuardedCall<'_>) -> Option<GuardDenial> {
        None
    }
}

/// A guard that records what it was shown, so tests can assert the guard
/// sees the finalized call: resolved name, post-middleware arguments, the
/// tool's declared effect, and the run scope.
#[derive(Debug)]
struct SpyGuard {
    seen: Mutex<Vec<(String, Value, Effect, String)>>,
}

impl ToolGuard for SpyGuard {
    fn name(&self) -> &str {
        "spy"
    }
    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        self.seen.lock().unwrap().push((
            call.tool.to_owned(),
            call.arguments.clone(),
            call.effect,
            call.scope.to_owned(),
        ));
        None
    }
}

// ---------- journal inspection helpers ----------

/// The journaled events of one kind, in sequence order.
fn events_of_kind(snapshot: &JournalSnapshot, kind: RunEventKind) -> Vec<&RunEvent> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .collect()
}

/// An event's inline input payload.
fn inline_input(event: &RunEvent) -> Value {
    match event.input.as_ref() {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected inline input payload, got {other:?}"),
    }
}

/// An event's inline output payload.
fn inline_output(event: &RunEvent) -> Value {
    match event.output.as_ref() {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected inline output payload, got {other:?}"),
    }
}

/// Record one ReAct run (scripted model + counting echo tool) with the full
/// determinism seam set and the given run-config pins. Returns the journal
/// snapshot, the final state, and the graph's topology hash.
async fn record_run(
    config: impl FnOnce(RunConfig) -> RunConfig,
    guards: Vec<Arc<dyn ToolGuard>>,
) -> (JournalSnapshot, State, String, Arc<AtomicUsize>) {
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let echo_calls = Arc::new(AtomicUsize::new(0));
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel::react_script());
    let graph = create_react_agent_with_recording(model, tools(echo_calls.clone()), journal.clone())
        .unwrap();
    let topology_hash = graph.topology_hash();

    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            config(
                RunConfig::new(THREAD_ID)
                    .with_journal(journal.clone())
                    .with_rng(RngSource::seeded(RNG_SEED)),
            )
            .with_tool_guards(guards),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state, topology_hash, echo_calls),
        other => panic!("expected Done, got {other:?}"),
    }
}

// ---------- the run-config envelope ----------

#[tokio::test]
async fn declaration_journals_the_resolved_envelope_once_at_start() {
    let manifest = RunManifest {
        model: Some("scripted-react-1".into()),
        ..RunManifest::default()
    };
    let (snapshot, _state, topology_hash, _calls) = record_run(
        |config| {
            config
                .with_graph_version("evidence-graph-v1")
                .with_tool_allowlist(["echo"])
                .with_manifest(manifest)
        },
        Vec::new(),
    )
    .await;

    let declarations = events_of_kind(&snapshot, RunEventKind::RunConfigDeclared);
    assert_eq!(declarations.len(), 1, "the envelope is journaled once");
    let declaration = declarations[0];
    assert_eq!(declaration.effect, Effect::Pure);
    // The declaration precedes all run work: it is the first journaled
    // event of a fresh run.
    assert_eq!(declaration.seq, 0);
    assert_eq!(
        inline_output(declaration),
        json!({
            "graph_version": "evidence-graph-v1",
            "graph_hash": topology_hash,
            "policy_version": PolicyVersion::STATIC_V0,
            "tool_allowlist": ["echo"],
            "manifest": {"model": "scripted-react-1"},
        })
    );
}

#[tokio::test]
async fn a_run_that_declares_nothing_journals_no_declaration() {
    // The byte-compat half of the envelope: absent means undeclared, so
    // runs (and journals) from before the wave are untouched.
    let (snapshot, _state, _hash, echo_calls) = record_run(|config| config, Vec::new()).await;
    assert!(
        events_of_kind(&snapshot, RunEventKind::RunConfigDeclared).is_empty(),
        "a default run journals no declaration"
    );
    assert_eq!(echo_calls.load(Ordering::SeqCst), 1, "the tool really ran");
}

#[tokio::test]
async fn a_declared_run_replays_byte_identically_from_the_log() {
    let manifest = RunManifest {
        model: Some("scripted-react-1".into()),
        ..RunManifest::default()
    };
    let (snapshot, _state, _hash, _calls) = record_run(
        |config| {
            config
                .with_graph_version("evidence-graph-v1")
                .with_tool_allowlist(["echo"])
                .with_manifest(manifest)
        },
        Vec::new(),
    )
    .await;

    // Replay against sentinels with NO config pins supplied by the caller:
    // ExactReplay serves the envelope from the recorded declaration, so the
    // replayed run re-declares — and re-journals — it identically.
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let graph = create_react_agent_replaying(
        Arc::new(PanicModel {
            calls: model_calls.clone(),
        }),
        sentinel_tools(tool_calls.clone()),
        replay.source(),
        journal.clone(),
    )
    .unwrap();
    let replayed = replay
        .run_and_verify(
            &graph,
            &spec(),
            initial_state(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();

    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap(),
        "the replayed journal — declaration included — is byte-identical"
    );
}

#[tokio::test]
async fn verify_names_the_divergent_envelope_field() {
    let (snapshot, _state, _hash, _calls) = record_run(
        |config| config.with_graph_version("evidence-graph-v1"),
        Vec::new(),
    )
    .await;
    let replay = ExactReplay::new(snapshot).unwrap();

    // A real replay exhausts the source and passes verification…
    let journal = replay.fresh_journal(logical_clock());
    let graph = create_react_agent_replaying(
        Arc::new(PanicModel {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        sentinel_tools(Arc::new(AtomicUsize::new(0))),
        replay.source(),
        journal.clone(),
    )
    .unwrap();
    let replayed = replay
        .run_and_verify(
            &graph,
            &spec(),
            initial_state(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();

    // …then a replayed journal whose declaration drifted names the field.
    let mut tampered = replayed.journal.clone();
    for event in &mut tampered.events {
        if event.kind == RunEventKind::RunConfigDeclared {
            if let Some(PayloadRef::Inline(value)) = event.output.as_mut() {
                value["graph_version"] = json!("evidence-graph-v2");
            }
        }
    }
    let error = replay.verify(&tampered).unwrap_err().to_string();
    assert!(
        error.contains("replay envelope divergence: graph_version"),
        "the divergence names its field, got: {error}"
    );
    assert!(
        error.contains("evidence-graph-v1") && error.contains("evidence-graph-v2"),
        "both sides are named, got: {error}"
    );
}

#[tokio::test]
async fn verify_rejects_a_dropped_declaration_by_presence() {
    let (snapshot, _state, _hash, _calls) = record_run(
        |config| config.with_graph_version("evidence-graph-v1"),
        Vec::new(),
    )
    .await;
    let replay = ExactReplay::new(snapshot).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let graph = create_react_agent_replaying(
        Arc::new(PanicModel {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        sentinel_tools(Arc::new(AtomicUsize::new(0))),
        replay.source(),
        journal.clone(),
    )
    .unwrap();
    let replayed = replay
        .run_and_verify(
            &graph,
            &spec(),
            initial_state(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED)),
        )
        .await
        .unwrap();

    let mut tampered = replayed.journal.clone();
    tampered
        .events
        .retain(|event| event.kind != RunEventKind::RunConfigDeclared);
    let error = replay.verify(&tampered).unwrap_err().to_string();
    assert!(
        error.contains("the recorded run declared a configuration envelope, but the replayed run declared none"),
        "a dropped declaration is a presence divergence, got: {error}"
    );
}

// ---------- deny-only tool guards ----------

/// The transcript's tool messages (role `tool`), in order.
fn tool_messages(state: &State) -> Vec<ChatMessage> {
    let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    messages
        .into_iter()
        .filter(|message| message.role == rusty_agent_runtime::llm::Role::Tool)
        .collect()
}

#[tokio::test]
async fn a_guard_denial_blocks_dispatch_and_journals_the_evidence() {
    let (snapshot, state, _hash, echo_calls) =
        record_run(|config| config, vec![Arc::new(DenyEcho)]).await;

    // The tool body never ran — the denial blocks the dispatch itself.
    assert_eq!(echo_calls.load(Ordering::SeqCst), 0);
    // No tool effect was journaled: a denied call is not a call.
    assert!(events_of_kind(&snapshot, RunEventKind::ToolCall).is_empty());

    // The denial is evidence: one ToolCallDenied event naming the tool, the
    // guard, and the reason, carrying the denied request as its input.
    let denials = events_of_kind(&snapshot, RunEventKind::ToolCallDenied);
    assert_eq!(denials.len(), 1);
    let denial = denials[0];
    assert_eq!(denial.effect, Effect::Pure);
    assert_eq!(
        inline_input(denial),
        json!({"tool": "echo", "arguments": {"text": "hello"}})
    );
    assert_eq!(
        inline_output(denial),
        json!({
            "tool": "echo",
            "effect": "read_only",
            "scope": THREAD_ID,
            "denials": [{"guard": "deny-echo", "reason": "echo is blocked by policy"}],
        })
    );

    // The model observed the refusal as the tool result, and the run drove
    // on to the scripted final answer instead of failing.
    let tool_messages = tool_messages(&state);
    assert_eq!(tool_messages.len(), 1);
    let content = tool_messages[0].content.as_deref().unwrap();
    assert!(
        content.contains("tool guard denied `echo`: guard `deny-echo`: echo is blocked by policy"),
        "the model sees the attributable refusal, got: {content}"
    );
}

#[tokio::test]
async fn guard_registration_order_cannot_hide_a_denial() {
    // deny-then-silent and silent-then-deny both deny: the layer is
    // monotonic, so ordering is not a decision surface.
    for guards in [
        vec![
            Arc::new(DenyEcho) as Arc<dyn ToolGuard>,
            Arc::new(SilentGuard),
        ],
        vec![
            Arc::new(SilentGuard) as Arc<dyn ToolGuard>,
            Arc::new(DenyEcho),
        ],
    ] {
        let (snapshot, _state, _hash, echo_calls) = record_run(|config| config, guards).await;
        assert_eq!(echo_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            events_of_kind(&snapshot, RunEventKind::ToolCallDenied).len(),
            1
        );
    }

    // Two denying guards are both named, in registration order: every guard
    // is evaluated, no short-circuit.
    let (snapshot, _state, _hash, _calls) = record_run(
        |config| config,
        vec![Arc::new(DenyEcho), Arc::new(DenyEchoOps)],
    )
    .await;
    let denial = events_of_kind(&snapshot, RunEventKind::ToolCallDenied)[0];
    assert_eq!(
        inline_output(denial)["denials"],
        json!([
            {"guard": "deny-echo", "reason": "echo is blocked by policy"},
            {"guard": "deny-echo-ops", "reason": "ops freeze on echo"},
        ])
    );
}

#[tokio::test]
async fn a_guard_sees_the_finalized_call() {
    let spy = Arc::new(SpyGuard {
        seen: Mutex::new(Vec::new()),
    });
    let (snapshot, _state, _hash, echo_calls) =
        record_run(|config| config, vec![spy.clone()]).await;

    // A silent guard blocks nothing: the tool ran and journaled its call.
    assert_eq!(echo_calls.load(Ordering::SeqCst), 1);
    assert_eq!(events_of_kind(&snapshot, RunEventKind::ToolCall).len(), 1);

    // The guard saw the finalized call: resolved name, arguments, declared
    // effect class, and the run scope.
    let seen = spy.seen.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        &[(
            "echo".to_owned(),
            json!({"text": "hello"}),
            Effect::ReadOnly,
            THREAD_ID.to_owned()
        )]
    );
}

#[tokio::test]
async fn replay_re_derives_a_denial_from_the_same_guards() {
    let (snapshot, _state, _hash, echo_calls) =
        record_run(|config| config, vec![Arc::new(DenyEcho)]).await;
    assert_eq!(echo_calls.load(Ordering::SeqCst), 0);

    // Replay carries the same guards (code, like the graph and the tool
    // identities): the denial is re-derived at the boundary, never served,
    // and the replaying tool sentinel never fires.
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let graph = create_react_agent_replaying(
        Arc::new(PanicModel {
            calls: model_calls.clone(),
        }),
        sentinel_tools(tool_calls.clone()),
        replay.source(),
        journal.clone(),
    )
    .unwrap();
    let replayed = replay
        .run_and_verify(
            &graph,
            &spec(),
            initial_state(),
            ReplayParams::new(journal, RngSource::seeded(RNG_SEED))
                .with_tool_guards(vec![Arc::new(DenyEcho)]),
        )
        .await
        .unwrap();

    assert_eq!(model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        serde_json::to_string(&replayed.journal).unwrap(),
        "the replayed journal — denial included — is byte-identical"
    );
}

// ---------- the journaled approval gate ----------

#[test]
fn the_gate_journals_the_asked_decided_pair_with_causal_parents() {
    let journal = Journal::new("run-gate", "t-gate", logical_clock());
    let gate = ApprovalGate::for_turn(&journal, "run-gate:7");
    let request = ApprovalRequest {
        kind: "publish_composed_skill".into(),
        effect_id: Some("eff-123".into()),
        detail: Some(json!({"content_hash": "abc"})),
    };

    let decision = gate.decide(
        &request,
        ApprovalDecision::ApprovedOnce {
            approved_by: "ops:ada".into(),
        },
    );
    assert!(decision.grants(), "decide returns the decision unchanged");

    let events = journal.events();
    assert_eq!(events.len(), 2);
    let asked = &events[0];
    let decided = &events[1];

    assert_eq!(asked.kind, RunEventKind::ApprovalAsked);
    assert_eq!(asked.effect, Effect::Pure);
    assert_eq!(asked.parent.as_deref(), Some("run-gate:7"));
    assert_eq!(
        inline_input(asked),
        json!({
            "kind": "publish_composed_skill",
            "effect_id": "eff-123",
            "detail": {"content_hash": "abc"},
        })
    );

    assert_eq!(decided.kind, RunEventKind::ApprovalDecided);
    assert_eq!(decided.effect, Effect::Pure);
    // The decided event hangs off its ask: the pair reads as one causal unit.
    assert_eq!(decided.parent.as_deref(), Some(asked.id.as_str()));
    assert_eq!(
        inline_output(decided),
        json!({
            "kind": "publish_composed_skill",
            "effect_id": "eff-123",
            "decision": {"decision": "approved_once", "approved_by": "ops:ada"},
        })
    );
}

#[test]
fn a_run_level_gate_journals_without_a_causal_anchor() {
    let journal = Journal::new("run-gate", "t-gate", logical_clock());
    let gate = ApprovalGate::new(&journal);
    let request = ApprovalRequest {
        kind: "cli_exec".into(),
        effect_id: None,
        detail: None,
    };
    let decision = gate.decide(&request, ApprovalDecision::Cancelled { reason: None });
    assert!(!decision.grants());

    let events = journal.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].parent, None, "no anchor, no parent");
    // Sparse on the wire: absent optionals are omitted, never null.
    assert_eq!(inline_input(&events[0]), json!({"kind": "cli_exec"}));
    assert_eq!(
        inline_output(&events[1]),
        json!({"kind": "cli_exec", "decision": {"decision": "cancelled"}})
    );
}

#[test]
fn the_closed_vocabulary_grants_only_through_approved_once() {
    let decisions = [
        ApprovalDecision::ApprovedOnce {
            approved_by: "ops:ada".into(),
        },
        ApprovalDecision::Rejected {
            decided_by: "ops:ada".into(),
            reason: None,
        },
        ApprovalDecision::Cancelled { reason: None },
        ApprovalDecision::Unavailable { reason: None },
    ];
    assert!(decisions[0].grants());
    assert_eq!(decisions[0].approved_by(), Some("ops:ada"));
    for decision in &decisions[1..] {
        assert!(!decision.grants(), "{decision:?} must not grant");
        assert_eq!(decision.approved_by(), None);
    }
}

// ---------- the composer publish gate, journaled ----------

fn skill_args(name: &str, body: &str) -> Value {
    json!({
        "name": name,
        "description": format!("The {name} skill."),
        "body": body,
        "author": "agent:rusty"
    })
}

/// Mint the approval a publish of `hash` in `scope` requires.
fn approval_for(scope: &str, hash: &str, approved_by: &str) -> Value {
    let token = ApprovalToken::approve(publish_effect_id(scope, hash), approved_by);
    serde_json::to_value(&token).unwrap()
}

/// A composer session with a compose tool and a gated publish tool.
fn gated_composer(
    scope: &str,
    journal: &Journal,
) -> (
    Arc<ComposerSession>,
    ComposeSkillTool,
    PublishComposedSkillTool,
    Arc<Mutex<SkillRegistry>>,
) {
    let session = ComposerSession::new(scope);
    let registry = Arc::new(Mutex::new(SkillRegistry::new()));
    (
        Arc::clone(&session),
        ComposeSkillTool::new(Arc::clone(&session)),
        PublishComposedSkillTool::new(session, Arc::clone(&registry))
            .with_approval_gate(ApprovalGate::new(journal)),
        registry,
    )
}

async fn compose_draft(compose: &ComposeSkillTool) -> String {
    let receipt = compose
        .call(skill_args("triage-report", "# Triage\n\nClassify.\n"))
        .await
        .unwrap();
    assert_eq!(receipt["valid"], json!(true));
    receipt["content_hash"].as_str().unwrap().to_owned()
}

/// The journaled approval pair of `journal`, as (asked, decided).
fn approval_pair(journal: &Journal) -> (RunEvent, RunEvent) {
    let events = journal.events();
    let asked: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.kind == RunEventKind::ApprovalAsked)
        .collect();
    let decided: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.kind == RunEventKind::ApprovalDecided)
        .collect();
    assert_eq!(asked.len(), 1, "exactly one ask journaled");
    assert_eq!(decided.len(), 1, "exactly one decision journaled");
    assert_eq!(
        decided[0].parent.as_deref(),
        Some(asked[0].id.as_str()),
        "the decision hangs off its ask"
    );
    (asked[0].clone(), decided[0].clone())
}

#[tokio::test]
async fn a_gated_publish_journals_the_approved_pair() {
    let journal = Journal::new("run-publish", "t-publish", logical_clock());
    let (session, compose, publish, registry) = gated_composer("run-publish", &journal);
    let hash = compose_draft(&compose).await;

    let published = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), &hash, "ops:ada"),
        }))
        .await
        .unwrap();
    assert_eq!(published["name"], json!("triage-report"));
    assert!(registry.lock().unwrap().get("triage-report").is_some());

    let (asked, decided) = approval_pair(&journal);
    assert_eq!(
        inline_input(&asked),
        json!({
            "kind": "publish_composed_skill",
            "effect_id": publish_effect_id(session.scope(), &hash).as_str(),
            "detail": {"content_hash": hash},
        })
    );
    assert_eq!(
        inline_output(&decided),
        json!({
            "kind": "publish_composed_skill",
            "effect_id": publish_effect_id(session.scope(), &hash).as_str(),
            "decision": {"decision": "approved_once", "approved_by": "ops:ada"},
        })
    );
}

#[tokio::test]
async fn a_misscoped_token_journals_a_rejection_and_registers_nothing() {
    let journal = Journal::new("run-publish", "t-publish", logical_clock());
    let (session, compose, publish, registry) = gated_composer("run-publish", &journal);
    let hash = compose_draft(&compose).await;

    // A token minted against a DIFFERENT draft's effect id: approvals are
    // not transferable.
    let wrong = ApprovalToken::approve(
        publish_effect_id(session.scope(), &"f".repeat(64)),
        "ops:ada",
    );
    let error = publish
        .call(json!({
            "content_hash": hash,
            "approval": serde_json::to_value(&wrong).unwrap(),
        }))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("publish admission denied"),
        "the admission refusal is unchanged, got: {error}"
    );
    assert!(registry.lock().unwrap().get("triage-report").is_none());

    let (asked, decided) = approval_pair(&journal);
    assert_eq!(
        inline_input(&asked)["effect_id"],
        json!(publish_effect_id(session.scope(), &hash).as_str())
    );
    assert_eq!(
        inline_output(&decided)["decision"],
        json!({
            "decision": "rejected",
            "decided_by": "ops:ada",
            "reason": "the token is scoped to a different effect id — approvals are not transferable",
        })
    );
}

#[tokio::test]
async fn a_missing_token_journals_an_unavailable_decision() {
    let journal = Journal::new("run-publish", "t-publish", logical_clock());
    let (_session, compose, publish, registry) = gated_composer("run-publish", &journal);
    let hash = compose_draft(&compose).await;

    let error = publish
        .call(json!({"content_hash": hash}))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("`approval` must name the scoped approval token"),
        "the missing-token refusal is unchanged, got: {error}"
    );
    assert!(registry.lock().unwrap().get("triage-report").is_none());

    let (_asked, decided) = approval_pair(&journal);
    assert_eq!(
        inline_output(&decided)["decision"],
        json!({
            "decision": "unavailable",
            "reason": "no approval token presented",
        })
    );
}

#[tokio::test]
async fn an_unknown_draft_refuses_before_any_ask_is_journaled() {
    let journal = Journal::new("run-publish", "t-publish", logical_clock());
    let (_session, _compose, publish, _registry) = gated_composer("run-publish", &journal);

    let error = publish
        .call(json!({"content_hash": "f".repeat(64)}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown draft"), "got: {error}");
    assert!(
        journal.events().is_empty(),
        "there is nothing to approve for an unknown draft — no ask is journaled"
    );
}

#[tokio::test]
async fn a_publish_without_a_gate_keeps_the_legacy_behavior() {
    // No gate attached: the admission is byte-identical to before the wave
    // and journals nothing.
    let session = ComposerSession::new("run-publish");
    let registry = Arc::new(Mutex::new(SkillRegistry::new()));
    let compose = ComposeSkillTool::new(Arc::clone(&session));
    let publish = PublishComposedSkillTool::new(Arc::clone(&session), Arc::clone(&registry));
    let hash = compose_draft(&compose).await;

    let published = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), &hash, "ops:ada"),
        }))
        .await
        .unwrap();
    assert_eq!(published["approved_by"], json!("ops:ada"));
    assert!(registry.lock().unwrap().get("triage-report").is_some());
}

// ---------- streaming chunk capture ----------

/// A model with a real `chat_stream`: two deltas, the terminal one carrying
/// a raw provider chunk.
struct StreamingModel;

#[async_trait::async_trait]
impl ChatModel for StreamingModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("hello world"),
            model: Some("stream-1".into()),
            usage: None,
        })
    }
    async fn chat_stream(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> RustyResult<ChatResponse> {
        on_token(TokenChunk {
            delta: "hello ".into(),
            finish: false,
            raw: None,
        });
        on_token(TokenChunk {
            delta: "world".into(),
            finish: true,
            raw: Some(json!({"provider": "chunk"})),
        });
        self.chat(_messages, _tools).await
    }
}

#[tokio::test]
async fn chat_stream_capture_journals_the_chunks_and_forwards_the_deltas() {
    let journal = Journal::new("run-stream", "t-stream", logical_clock());
    let model = RecordingChatModel::new(
        Arc::new(StreamingModel),
        journal.clone(),
        "run-stream:1".to_owned(),
    )
    .with_chunk_capture(true);

    let mut deltas = Vec::new();
    let response = model
        .chat_stream(&[ChatMessage::user("hi")], &[], &mut |chunk| {
            deltas.push((chunk.delta.clone(), chunk.finish));
        })
        .await
        .unwrap();

    // The caller's stream is untouched: every delta forwarded, the final
    // response intact.
    assert_eq!(
        deltas,
        vec![("hello ".to_owned(), false), ("world".to_owned(), true)]
    );
    assert_eq!(response.message.content.as_deref(), Some("hello world"));

    let events = journal.events();
    assert_eq!(events.len(), 1);
    let call = &events[0];
    assert_eq!(call.kind, RunEventKind::ModelCall);
    let output = inline_output(call);
    assert_eq!(
        output["chunks"],
        json!([
            {"delta": "hello ", "finish": false},
            {"delta": "world", "finish": true, "raw": {"provider": "chunk"}},
        ]),
        "the journaled output carries the captured chunk stream"
    );
}

#[tokio::test]
async fn chat_stream_without_capture_journals_the_pre_wave_shape() {
    let journal = Journal::new("run-stream", "t-stream", logical_clock());
    let model = RecordingChatModel::new(
        Arc::new(StreamingModel),
        journal.clone(),
        "run-stream:1".to_owned(),
    );

    let mut deltas = Vec::new();
    model
        .chat_stream(&[ChatMessage::user("hi")], &[], &mut |chunk| {
            deltas.push(chunk.delta.clone());
        })
        .await
        .unwrap();
    assert_eq!(deltas, vec!["hello ".to_owned(), "world".to_owned()]);

    let events = journal.events();
    assert_eq!(events.len(), 1);
    let output = inline_output(&events[0]);
    assert!(
        output.get("chunks").is_none(),
        "capture off is byte-identical to the pre-wave shape: no chunks key"
    );
}
