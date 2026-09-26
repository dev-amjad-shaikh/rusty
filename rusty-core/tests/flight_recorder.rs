//! Flight Recorder integration tests (R0.5).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of `RunEvent`, `DecisionEvent`,
//!   `Effect`, and `CheckpointHeader` are pinned against checked-in JSON
//!   under `tests/golden/`. Any accidental contract drift fails here. To
//!   bless an intentional contract change, re-run with `UPDATE_GOLDEN=1`
//!   and review the diff.
//! - **Back-compat** — a pre-R0.5 checkpoint JSON (no header, no journal
//!   reference) still deserializes.
//! - **Journal integration** — a small graph (mock `ChatModel` + a tool)
//!   produces the expected ordered, causally linked, effect-classified
//!   journal.
//! - **Determinism** — the same graph driven twice with the same seeded
//!   RNG and logical clock yields byte-identical journal snapshots.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Map, Value};

use rusty_agent_runtime::checkpoint::{Checkpoint, Checkpointer, InMemoryCheckpointer};
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::{Graph, GraphBuilder};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal, RngSource, PARENT_EVENT_KEY};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Usage};
use rusty_agent_runtime::node::{NodeContext, NodeOutput};
use rusty_agent_runtime::record::{
    ArtifactRef, CapsuleVersion, CheckpointHeader, DecisionAction, DecisionEvent, DecisionFamily,
    DecisionOutcome, Effect, EffectReceipt, EventStatus, PayloadRef, PolicyVersion, RunEvent,
    RunEventKind, RunManifest, CURRENT_FORMAT_VERSION,
};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
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

fn fixed_time() -> DateTime<Utc> {
    DateTime::from_timestamp_millis(1_750_000_000_000).unwrap()
}

fn sample_run_event() -> RunEvent {
    RunEvent {
        id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:4".into(),
        run_id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d".into(),
        thread_id: "thread-42".into(),
        node_id: Some("agent".into()),
        seq: 4,
        kind: RunEventKind::ModelCall,
        effect: Effect::NonIdempotent,
        input: Some(PayloadRef::inline(json!({
            "messages": [{"role": "user", "content": "ping"}],
            "tools": [],
        }))),
        output: Some(PayloadRef::Artifact(ArtifactRef {
            sha256: "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7".into(),
            bytes: 8192,
        })),
        latency_ms: Some(137),
        tokens: Some(Usage {
            prompt_tokens: 128,
            completion_tokens: 32,
            total_tokens: 160,
            cached_tokens: None,
            reasoning_tokens: None,
        }),
        cost_usd: Some(0.00042),
        status: EventStatus::Ok,
        parent: Some("019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:1".into()),
        recorded_at: fixed_time(),
    }
}

fn sample_decision_event() -> DecisionEvent {
    DecisionEvent {
        id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:d0".into(),
        run_id: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d".into(),
        thread_id: "thread-42".into(),
        seq: 0,
        family: DecisionFamily::Retry,
        features: Map::from_iter([
            ("failure_class".to_owned(), json!("timeout")),
            ("attempt".to_owned(), json!(1)),
            ("p95_latency_ms".to_owned(), json!(840)),
        ]),
        legal_actions: vec![DecisionAction::Retry { attempt: 2 }, DecisionAction::Abort],
        selected: DecisionAction::Retry { attempt: 2 },
        propensity: 0.8,
        policy_version: PolicyVersion::new("static-v0"),
        role: None,
        outcome: Some(DecisionOutcome::Success),
        decided_at: fixed_time(),
    }
}

#[test]
fn golden_run_event_shape() {
    assert_golden("run_event.json", &sample_run_event());
}

#[test]
fn golden_decision_event_shape() {
    assert_golden("decision_event.json", &sample_decision_event());
}

#[test]
fn golden_effect_shape() {
    // All variants in declaration order: the variant names are the contract.
    assert_golden(
        "effect.json",
        &vec![
            Effect::Pure,
            Effect::ReadOnly,
            Effect::Idempotent,
            Effect::Compensatable,
            Effect::NonIdempotent,
        ],
    );
}

#[test]
fn golden_checkpoint_header_shape() {
    assert_golden(
        "checkpoint_header.json",
        &CheckpointHeader {
            format_version: CURRENT_FORMAT_VERSION,
            graph_version: "react-v3".into(),
            graph_hash: "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".into(),
            policy_version: PolicyVersion::new("static-v0"),
            logical_clock: 1_750_000_000_000,
            // R0.7: no manifest pinned — the serialized shape must stay
            // byte-identical to the R0.5 golden (additive evolution).
            manifest: None,
            // R0.13: no inbox attached — absent from the wire for the same
            // additive discipline.
            inbox: None,
        },
    );
}

/// The R0.7 versioned run manifest, fully pinned: prompts, tool schemas,
/// model + parameters, memory schema, and capsule versions.
fn sample_run_manifest() -> RunManifest {
    RunManifest::new()
        .pin_prompt("system", "You are a careful research agent.")
        .pin_tool_schema(
            "search",
            &json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        )
        .pin_model(
            "gpt-5.2-2026-06-01",
            &json!({"temperature": 0, "seed": 42, "max_tokens": 512}),
        )
        .with_memory_schema("memory-v1")
        .pin_capsule("researcher", CapsuleVersion::new("1.4.0"))
}

#[test]
fn golden_run_manifest_shape() {
    assert_golden("run_manifest.json", &sample_run_manifest());
}

#[test]
fn golden_run_manifest_middleware_shape() {
    // R0.11 wave 4's one additive manifest field: a bound middleware
    // composition pins as the canonical-JSON digest of its ordered layer
    // list under `middleware`. Old manifests are byte-stable — the field
    // is absent, not null, when no composition bound (pinned by
    // `run_manifest.json` staying untouched).
    let manifest = sample_run_manifest().pin_middleware(&json!([
        {"layer": "request_logger"},
        {"layer": "tool_call_blocklist", "config": {"blocked": ["shell", "fs_write"]}},
    ]));
    assert_golden("run_manifest_middleware.json", &manifest);
}

#[test]
fn golden_checkpoint_header_with_manifest_shape() {
    // The extended header: the R0.5 fields unchanged, plus the pinned
    // manifest. This is the wire shape R0.7 checkpoints carry.
    assert_golden(
        "checkpoint_header_manifest.json",
        &CheckpointHeader {
            format_version: CURRENT_FORMAT_VERSION,
            graph_version: "react-v3".into(),
            graph_hash: "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".into(),
            policy_version: PolicyVersion::new("static-v0"),
            logical_clock: 1_750_000_000_000,
            manifest: Some(sample_run_manifest()),
            inbox: None,
        },
    );
}

/// Old headers deserialize into the extended header: the R0.5 golden file
/// doubles as the pre-R0.7 fixture.
#[test]
fn r05_golden_header_deserializes_with_no_manifest() {
    let bytes = std::fs::read_to_string(golden_path("checkpoint_header.json")).unwrap();
    let header: CheckpointHeader = serde_json::from_str(&bytes)
        .expect("the R0.5 golden header must deserialize into the extended header");
    assert_eq!(header.manifest, None);
    assert_eq!(header.format_version, CURRENT_FORMAT_VERSION);
}

/// The receipt an `Idempotent` effect's recipient journals (R0.6 wave 2b):
/// the payload shape exact replay's receipt lookup matches on.
fn sample_effect_receipt() -> EffectReceipt {
    EffectReceipt {
        provider: "stripe".into(),
        provider_id: "ch_3PKdY2eZvKYlo2C0".into(),
        idempotency_key: "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:charge:7".into(),
        task_id: Some("task-019157c5".into()),
        // R0.7 effect id unset: the receipt golden must stay byte-identical
        // to the R0.6 shape (additive evolution).
        effect_id: None,
    }
}

#[test]
fn golden_effect_receipt_shape() {
    assert_golden("effect_receipt.json", &sample_effect_receipt());
}

#[test]
fn effect_receipt_without_task_id_omits_the_field() {
    // Additive evolution: the optional linkage is absent (not null) on the
    // wire when unset, so pre-receipt consumers see no shape change.
    let receipt = EffectReceipt {
        task_id: None,
        ..sample_effect_receipt()
    };
    let value = serde_json::to_value(&receipt).unwrap();
    assert!(value.get("task_id").is_none());
    let back: EffectReceipt = serde_json::from_value(value).unwrap();
    assert_eq!(back, receipt);
}

// ---------- checkpoint back-compat ----------

/// A checkpoint exactly as R0.4 wrote it: no `header`, no `journal_ref`.
const OLD_SHAPE_CHECKPOINT_JSON: &str = r#"{
  "id": "b3d2c1a0-1111-4222-8333-444455556666",
  "thread_id": "legacy-thread",
  "step": 3,
  "state": {"messages": [{"role": "user", "content": "hi"}]},
  "next_nodes": ["agent"],
  "created_at": "2026-08-05T12:00:00Z"
}"#;

#[test]
fn pre_r05_checkpoint_without_header_still_loads() {
    let checkpoint: Checkpoint = serde_json::from_str(OLD_SHAPE_CHECKPOINT_JSON)
        .expect("old-shape checkpoint must deserialize");

    assert_eq!(checkpoint.id, "b3d2c1a0-1111-4222-8333-444455556666");
    assert_eq!(checkpoint.thread_id, "legacy-thread");
    assert_eq!(checkpoint.step, 3);
    assert_eq!(checkpoint.next_nodes, vec!["agent".to_string()]);

    // The serde defaults fill in the R0.5 provenance: current format
    // version, unversioned graph, static policy, logical clock zero — and
    // no journal reference.
    assert_eq!(checkpoint.header, CheckpointHeader::default());
    assert_eq!(checkpoint.header.format_version, CURRENT_FORMAT_VERSION);
    assert_eq!(checkpoint.header.policy_version, PolicyVersion::default());
    assert!(checkpoint.journal_ref.is_none());

    // And it re-serializes into the new shape without losing anything.
    let back: Checkpoint =
        serde_json::from_str(&serde_json::to_string(&checkpoint).unwrap()).unwrap();
    assert_eq!(back.id, checkpoint.id);
    assert_eq!(back.header, checkpoint.header);
}

// ---------- shared fixtures: mock model + tool ----------

/// A mock chat model: no network, fixed response and usage.
struct MockModel;

#[async_trait::async_trait]
impl ChatModel for MockModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("pong"),
            model: Some("mock-model-v1".into()),
            usage: Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 3,
                total_tokens: 15,
                cached_tokens: None,
                reasoning_tokens: None,
            }),
        })
    }
}

/// A trivial lookup tool.
struct EchoTool;

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
        // An honest declaration: this tool only reads its arguments.
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

/// The causal parent handed to the current invocation by the executor.
fn parent_event(ctx: &NodeContext) -> String {
    ctx.config()
        .extra
        .get(PARENT_EVENT_KEY)
        .and_then(Value::as_str)
        .expect("executor must set the parent event key")
        .to_owned()
}

/// A two-node linear graph (`agent` calls the mock model, `tools` calls the
/// echo tool); both nodes journal their own calls into `journal`.
fn model_tool_graph(journal: &Journal) -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();

    let agent_journal = journal.clone();
    builder.add_node("agent", move |ctx: NodeContext| {
        let journal = agent_journal.clone();
        async move {
            let parent = parent_event(&ctx);
            let started = journal.clock().now();
            let response = MockModel.chat(&[ChatMessage::user("ping")], &[]).await?;
            let latency_ms = (journal.clock().now() - started).num_milliseconds().max(0) as u64;
            journal.record(
                EventDraft::new(RunEventKind::ModelCall, MockModel.effect())
                    .node("agent")
                    .input(json!({"messages": [{"role": "user", "content": "ping"}], "tools": []}))
                    .output(json!({
                        "content": response.message.content,
                        "model": response.model,
                    }))
                    .latency_ms(latency_ms)
                    .tokens(response.usage.unwrap_or_default())
                    .parent(parent),
            );
            Ok(NodeOutput::update("log", json!("agent")))
        }
    });

    let tools_journal = journal.clone();
    builder.add_node("tools", move |ctx: NodeContext| {
        let journal = tools_journal.clone();
        async move {
            let parent = parent_event(&ctx);
            let tool = EchoTool;
            let args = json!({"text": "hello"});
            let result = tool.call(args.clone()).await?;
            journal.record(
                EventDraft::new(RunEventKind::ToolCall, tool.effect())
                    .node("tools")
                    .input(args)
                    .output(result)
                    .parent(parent),
            );
            Ok(NodeOutput::update("log", json!("tools")))
        }
    });

    builder.set_entry_point("agent");
    builder.add_edge("agent", "tools");
    (builder.compile().unwrap(), spec)
}

// ---------- journal integration ----------

#[tokio::test]
async fn journal_records_ordered_causally_linked_classified_events() {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());

    let journal = Journal::new("run-int", "t-int", Clock::System);
    let (graph, spec) = model_tool_graph(&journal);

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-int").with_journal(journal.clone()),
        )
        .await
        .unwrap();
    match outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(state.get("log"), Some(&json!(["agent", "tools"])));
        }
        other => panic!("expected Done, got {other:?}"),
    }

    // Executor::journal() exposes the run's journal.
    let exposed = executor.journal().expect("journal must be set after a run");
    assert_eq!(exposed.head_hash(), journal.head_hash());

    let events = journal.events();
    let kinds: Vec<RunEventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RunEventKind::SuperStepStart, // step 0: agent
            RunEventKind::NodeInput,
            RunEventKind::ModelCall,
            RunEventKind::NodeOutput,
            RunEventKind::SuperStepEnd,
            RunEventKind::RoutingDecision,
            RunEventKind::CheckpointWritten,
            RunEventKind::SuperStepStart, // step 1: tools
            RunEventKind::NodeInput,
            RunEventKind::ToolCall,
            RunEventKind::NodeOutput,
            RunEventKind::SuperStepEnd,
            RunEventKind::RoutingDecision,
            RunEventKind::CheckpointWritten,
        ]
    );

    // Sequence numbers are the total order; ids are `{run_id}:{seq}`.
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64);
        assert_eq!(event.id, format!("run-int:{i}"));
        assert_eq!(event.run_id, "run-int");
        assert_eq!(event.thread_id, "t-int");
    }

    // Causal links, step 0 (indices per the kind sequence above).
    let (step_start, node_in, model, node_out, step_end, route, ckpt) = (
        &events[0], &events[1], &events[2], &events[3], &events[4], &events[5], &events[6],
    );
    assert_eq!(step_start.parent, None);
    assert_eq!(node_in.parent.as_deref(), Some(step_start.id.as_str()));
    // The model call's parent is the invocation that made it — delivered to
    // the node via NodeConfig::extra.
    assert_eq!(model.parent.as_deref(), Some(node_in.id.as_str()));
    assert_eq!(node_out.parent.as_deref(), Some(node_in.id.as_str()));
    assert_eq!(step_end.parent.as_deref(), Some(node_out.id.as_str()));
    assert_eq!(route.parent.as_deref(), Some(step_end.id.as_str()));
    assert_eq!(ckpt.parent.as_deref(), Some(route.id.as_str()));
    // Step 1 chains off step 0's routing decision.
    assert_eq!(events[7].parent.as_deref(), Some(route.id.as_str()));
    assert_eq!(events[8].parent.as_deref(), Some(events[7].id.as_str()));
    assert_eq!(events[9].parent.as_deref(), Some(events[8].id.as_str()));

    // Effect classifications: declared defaults and the tool's override.
    assert_eq!(node_in.effect, Effect::Pure); // closure node default
    assert_eq!(model.effect, Effect::NonIdempotent); // ChatModel default
    assert_eq!(events[9].effect, Effect::ReadOnly); // EchoTool override
    assert_eq!(ckpt.effect, Effect::Idempotent);
    assert_eq!(route.effect, Effect::Pure);

    // Model identity and token usage travel on the model-call event.
    assert_eq!(
        model.tokens,
        Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 3,
            total_tokens: 15,
            cached_tokens: None,
            reasoning_tokens: None,
        })
    );
    assert_eq!(model.node_id.as_deref(), Some("agent"));
    assert_eq!(events[9].node_id.as_deref(), Some("tools"));
    assert!(model.latency_ms.is_some());
    assert_eq!(model.status, EventStatus::Ok);

    // Checkpoints carry the frozen provenance header and a journal
    // reference binding state to evidence.
    let history = checkpointer.list("t-int").await.unwrap();
    assert_eq!(history.len(), 2);
    for checkpoint in &history {
        assert_eq!(checkpoint.header.format_version, CURRENT_FORMAT_VERSION);
        assert_eq!(checkpoint.header.graph_hash, graph.topology_hash());
        assert_eq!(checkpoint.header.graph_version, "unversioned");
        assert_eq!(checkpoint.header.policy_version, PolicyVersion::default());
        let journal_ref = checkpoint
            .journal_ref
            .as_ref()
            .expect("checkpoints must reference the journal head");
        assert!(journal_ref.events > 0);
        assert_eq!(journal_ref.sha256.len(), 64);
    }
    // The second checkpoint references a strictly larger journal than the
    // first (evidence grew between boundaries).
    assert!(
        history[1].journal_ref.as_ref().unwrap().events
            > history[0].journal_ref.as_ref().unwrap().events
    );
}

#[tokio::test]
async fn run_manifest_is_stamped_into_every_checkpoint_header() {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());

    let journal = Journal::new("run-manifest", "t-manifest", Clock::System);
    let (graph, spec) = model_tool_graph(&journal);
    let manifest = sample_run_manifest();

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-manifest")
                .with_journal(journal)
                .with_manifest(manifest.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));

    // Every boundary checkpoint of the run carries the pinned manifest,
    // alongside the R0.5 provenance it extends.
    let history = checkpointer.list("t-manifest").await.unwrap();
    assert!(!history.is_empty());
    for checkpoint in &history {
        assert_eq!(checkpoint.header.manifest.as_ref(), Some(&manifest));
        assert_eq!(checkpoint.header.format_version, CURRENT_FORMAT_VERSION);
        assert_eq!(checkpoint.header.graph_hash, graph.topology_hash());
    }

    // A run that pins nothing stamps no manifest — and its serialized
    // checkpoints carry no `manifest` key at all (no churn for old shapes).
    let plain_journal = Journal::new("run-plain", "t-plain", Clock::System);
    let (plain_graph, plain_spec) = model_tool_graph(&plain_journal);
    executor
        .run(
            &plain_graph,
            &plain_spec,
            State::new(),
            RunConfig::new("t-plain").with_journal(plain_journal),
        )
        .await
        .unwrap();
    let plain = checkpointer.get_latest("t-plain").await.unwrap().unwrap();
    assert_eq!(plain.header.manifest, None);
    let wire = serde_json::to_value(&plain.header).unwrap();
    assert!(wire.get("manifest").is_none());
}

#[tokio::test]
async fn interrupt_and_resume_are_journaled() {
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());
    let spec = StateSpec::new().channel("answer", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("gate", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("gate");
    let graph = builder.compile().unwrap();

    let journal = Journal::new("run-hitl", "t-hitl", Clock::System);
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-hitl").with_journal(journal.clone()),
        )
        .await
        .unwrap();
    assert!(outcome.is_interrupted());

    let kinds: Vec<RunEventKind> = journal.events().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RunEventKind::SuperStepStart,
            RunEventKind::NodeInput,
            RunEventKind::Interrupt,
            RunEventKind::CheckpointWritten,
        ]
    );
    let interrupt = &journal.events()[2];
    assert_eq!(interrupt.status, EventStatus::Interrupted);
    assert_eq!(interrupt.node_id.as_deref(), Some("gate"));
    assert_eq!(
        journal.resolve(interrupt.input.as_ref().unwrap()),
        Some(json!({"question": "approve?"}))
    );
    // The suspension checkpoint references a journal that includes the
    // interrupt event.
    let stored = checkpointer.get_latest("t-hitl").await.unwrap().unwrap();
    assert_eq!(stored.journal_ref.as_ref().unwrap().events, 3);

    // Resume: a fresh journal records the resume event as the causal root
    // of the continued run.
    let resume_journal = Journal::new("run-hitl-resume", "t-hitl", Clock::System);
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-hitl")
                .with_resume(json!(true))
                .with_journal(resume_journal.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));
    let events = resume_journal.events();
    assert_eq!(events[0].kind, RunEventKind::Resume);
    assert_eq!(events[1].kind, RunEventKind::SuperStepStart);
    assert_eq!(events[1].parent.as_deref(), Some(events[0].id.as_str()));
}

// ---------- determinism ----------

/// Drive the model/tool graph once with a seeded RNG and logical clock;
/// return the serialized journal snapshot.
fn run_seeded() -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let checkpointer = Arc::new(InMemoryCheckpointer::new());
        let executor = Executor::with_checkpointer(checkpointer);
        let journal = Journal::new("run-det", "t-det", Clock::logical(1_700_000_000_000, 10));
        let (graph, spec) = model_tool_graph(&journal);

        let outcome = executor
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-det")
                    .with_journal(journal.clone())
                    .with_rng(RngSource::seeded(7)),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Done(_)));
        serde_json::to_string_pretty(&journal.snapshot()).unwrap()
    })
}

#[test]
fn seeded_clock_and_rng_make_journal_snapshots_identical() {
    let first = run_seeded();
    let second = run_seeded();
    assert_eq!(
        first, second,
        "same graph + same seed + same logical clock must reproduce the journal exactly"
    );
}
