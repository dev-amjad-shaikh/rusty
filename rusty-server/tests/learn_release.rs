//! The R0.8 wave-4 release proof: one defect walks the whole learning
//! loop end to end against the *real* evaluator — `rusty-eval`'s
//! experiment runner driving the real executor, not wave 3's scripted
//! `FixedEvaluator`.
//!
//! The story (one long test, chapters marked in the body, the
//! `crash_recovery.rs` convention):
//!
//! 1. **The defect.** A human-written calibration record teaches the
//!    served calculator agent string-typed arguments (`{"a": "2"}`). The
//!    production graph reads it through the memory seam (unjournaled HTTP
//!    — server-run nodes hold no journal handle, the wave-2 boundary) and
//!    answers "what is 2 + 3?" with `the answer is 0`.
//! 2. **The evidence.** The defect behavior is recorded into a fixture
//!    journal *with journaled memory reads and a recording tool* — a
//!    self-contained replay artifact that survives later store changes
//!    (promotion rewrites what the live namespace serves; the fixture
//!    carries its own reads). A human correction against the defect run's
//!    journaled `node_input` event derives an `example` record.
//! 3. **The evaluation.** `EvalCandidateEvaluator` runs the versioned
//!    dataset twice: baseline against the live namespace (the defect
//!    reproduces: pass rate 0), candidate against the candidate overlay
//!    (the calibration superseded, the example served: pass rate 1). The
//!    causal lever is the memory lens, so experiment reads are
//!    *unjournaled in-process* — exact replay serves journaled effects,
//!    and journaled candidate reads would be replayed to the baseline
//!    verbatim, pinning delta at 0 forever (the eval crate's "the
//!    replayed world cannot differ" rule). Separately, the recorded
//!    fixture is re-driven under exact replay — the journaled-reads
//!    wiring, where parity *is* the claim.
//! 4. **The promotion.** `memory_set` at agent scope with a strictly
//!    positive delta and a clean replay is in the R0.8 default envelope:
//!    auto-promotion, no approval token. The version pointer moves and
//!    the *same question* now yields `the answer is 5`.
//! 5. **The chain.** Every hop is auditable: the good run's journaled
//!    memory read serves the example; the example names the correction
//!    and the defect run; the served candidate is byte-identical to the
//!    posted one; the lifecycle events are journaled into the defect
//!    run's own journal with the verdict the gate read.
//! 6. **The rollback.** Byte-exact: the pointer clears, and the final
//!    run reproduces the defect run's output and node-output events
//!    exactly.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tower::ServiceExt;

use rusty_agent_runtime::prelude::*;
use rusty_eval::dataset::{Dataset, EvalCase, Expectation, StatePredicate};
use rusty_eval::experiment::PreparedRun;
use rusty_server::{
    router, DirectoryDatasetSource, EvalCandidateEvaluator, EvaluationAgent, GraphRegistry,
    ServerConfig,
};

// --------------------------------------------------------------------- //
// Harness (the corrections.rs / learn_gate.rs conventions)
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of the test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-learn-release-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// A fixed timestamp, so candidates minted here are deterministic.
fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The run's journaled events (Flight Recorder).
async fn events_of(app: &Router, run_id: &str) -> Vec<Value> {
    let (status, v) = call(app, "GET", &format!("/runs/{run_id}/events"), None).await;
    assert_eq!(status, StatusCode::OK, "events failed: {v}");
    v["events"].as_array().unwrap().clone()
}

/// Register an agent declaring the `private` state scope (the manifest
/// gate for agent-scoped memory).
async fn register_agent(app: &Router, agent_id: &str) {
    let (status, v) = call(
        app,
        "POST",
        "/agents",
        Some(json!({
            "agent_id": agent_id,
            "manifest": {
                "agent_kind": "researcher",
                "manifest_version": "researcher/1.0.0",
                "accepts": {"calculate": {"kind": "application/json"}},
                "scopes": ["private"],
                "budget": {"max_tokens": 100000},
            },
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "agent registration failed: {v}"
    );
}

// --------------------------------------------------------------------- //
// The story's constants and payloads
// --------------------------------------------------------------------- //

/// The agent scope the calibration and the example live at.
const AGENT_SCOPE: &str = "support-1";
/// The calibration record's lookup key.
const CALIBRATION_KEY: &str = "calc-args";
/// The one question every run answers.
const QUESTION: &str = "what is 2 + 3?";
/// The correction id (attribution rides it).
const CORRECTION_ID: &str = "corr-release-1";

/// The defective calibration: string-typed arguments — the calculator's
/// lenient parse reads them as zero.
fn defect_args() -> Value {
    json!({"op": "add", "a": "2", "b": "3"})
}

/// The corrected arguments: typed numbers.
fn corrected_args() -> Value {
    json!({"op": "add", "a": 2, "b": 3})
}

// The recorded fixture's identity and determinism anchors. The fixture
// is recorded *after* the calibration write and *before* the correction,
// so its journaled reads capture the defect-era namespace.
const FIXTURE_RUN_ID: &str = "fixture-calc-defect-1";
const FIXTURE_THREAD_ID: &str = "fixture-calc-thread-1";
const FIXTURE_START_MS: u64 = 1_750_000_000_000;
const FIXTURE_TICK_MS: u64 = 5;
const FIXTURE_RNG_SEED: u64 = 42;

// --------------------------------------------------------------------- //
// The late router: nodes and test-side stores reach the server through
// HTTP, but the router only exists after it is built with the evaluator
// — which itself needs the wiring. A set-once cell breaks the cycle.
// --------------------------------------------------------------------- //

#[derive(Clone, Default)]
struct LateRouter(Arc<RwLock<Option<Router>>>);

impl LateRouter {
    fn set(&self, router: Router) {
        *self.0.write().unwrap() = Some(router);
    }

    fn get(&self) -> Router {
        self.0
            .read()
            .unwrap()
            .clone()
            .expect("late router used before set")
    }
}

impl std::fmt::Debug for LateRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LateRouter")
            .field("set", &self.0.read().unwrap().is_some())
            .finish()
    }
}

/// The test's `call`, routed through the late handle and mapping
/// transport failures into the runtime's error type (nodes and the
/// test-side store both speak `rusty_agent_runtime::Result`).
async fn node_call(
    late: &LateRouter,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value)> {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = late
        .get()
        .oneshot(builder.body(body).unwrap())
        .await
        .map_err(|e| RustyError::Node(format!("memory http call `{method} {uri}`: {e}")))?;
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| RustyError::Node(format!("memory http body `{method} {uri}`: {e}")))?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    Ok((status, value))
}

// --------------------------------------------------------------------- //
// The calculator: honest arithmetic, a lenient argument parse (the
// defect's lever — string args parse to zero, the classic "the tool
// never failed, it just answered a different question" calibration bug)
// --------------------------------------------------------------------- //

#[derive(Debug)]
struct CalculatorTool;

#[async_trait::async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Add or multiply two numbers."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["add", "mul"]},
                "a": {},
                "b": {},
            },
            "required": ["op", "a", "b"],
        })
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let op = args["op"].as_str().unwrap_or("add");
        // The lenient parse: non-numeric operands become zero instead of
        // an error — exactly why the defective calibration is silent.
        let a = args["a"].as_f64().unwrap_or(0.0);
        let b = args["b"].as_f64().unwrap_or(0.0);
        match op {
            "add" => Ok(json!(a + b)),
            "mul" => Ok(json!(a * b)),
            other => Err(RustyError::Tool(format!("unknown op `{other}`"))),
        }
    }
}

// --------------------------------------------------------------------- //
// The read lens and the tool wiring: one node, three honest wirings
// --------------------------------------------------------------------- //

/// How the node reads memory.
#[derive(Clone)]
enum ReadLens {
    /// Production wiring: unjournaled reads over HTTP against the live
    /// namespace (server-run nodes get no journal handle — the wave-2
    /// boundary; the server journals memory reads at its own seam).
    Http(LateRouter),
    /// Evaluation wiring: unjournaled reads against an in-process store —
    /// the live namespace for the baseline, the candidate overlay for the
    /// candidate. The overlay is the comparison's causal lever, so these
    /// reads must NOT be journaled: exact replay serves journaled reads
    /// verbatim, and replayed candidate reads would hand the baseline the
    /// candidate's behavior (delta pinned at 0, the gate unreachable).
    Store(Arc<dyn MemoryStore>),
    /// Fixture wiring: journaled reads — the recorded run is replay
    /// evidence that must keep reproducing the defect after promotion
    /// changes what the live namespace serves.
    Journaled(JournaledMemory),
}

impl std::fmt::Debug for ReadLens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variant = match self {
            ReadLens::Http(_) => "Http",
            ReadLens::Store(_) => "Store",
            ReadLens::Journaled(_) => "Journaled",
        };
        f.write_str(variant)
    }
}

/// How the node calls the calculator.
#[derive(Clone)]
enum ToolWiring {
    /// A plain call — production and experiment runs (nothing to serve;
    /// the run's evidence is its state updates).
    Direct,
    /// The flight-recording wrapper — the fixture run.
    Recording(Journal),
    /// The replay wrapper — the fixture's re-drive (the tool is never
    /// invoked; the recorded output is served).
    Replaying(ReplaySource, Journal),
}

impl std::fmt::Debug for ToolWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variant = match self {
            ToolWiring::Direct => "Direct",
            ToolWiring::Recording(_) => "Recording",
            ToolWiring::Replaying(_, _) => "Replaying",
        };
        f.write_str(variant)
    }
}

/// The calibration lookup: one keyed record at the agent scope.
fn calibration_query() -> MemoryQuery {
    MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::Agent, AGENT_SCOPE)),
        key: Some(CALIBRATION_KEY.to_owned()),
        ..MemoryQuery::default()
    }
}

/// The example lookup: correction-derived examples at the agent scope.
fn examples_query() -> MemoryQuery {
    MemoryQuery {
        scope: Some(ScopeAddress::new(MemoryScope::Agent, AGENT_SCOPE)),
        kinds: vec![MemoryKind::Example],
        ..MemoryQuery::default()
    }
}

/// The record's content, inlined (this test never spills payloads).
fn inline_content(record: &MemoryRecord) -> Result<&Value> {
    match &record.content {
        PayloadRef::Inline(value) => Ok(value),
        PayloadRef::Artifact(_) => Err(RustyError::Node(
            "artifact-spilled memory content in a test that inlines everything".to_owned(),
        )),
    }
}

/// Read records through the lens (one query, no budget on the wire; the
/// journaled path declares the budget the journaled request carries).
async fn read_records(
    lens: &ReadLens,
    query: MemoryQuery,
    parent: Option<String>,
) -> Result<Vec<MemoryRecord>> {
    match lens {
        ReadLens::Http(late) => {
            let (status, body) = node_call(
                late,
                "POST",
                "/memory/query",
                Some(serde_json::to_value(&query)?),
            )
            .await?;
            if status != StatusCode::OK {
                return Err(RustyError::Node(format!(
                    "memory query failed: {status} {body}"
                )));
            }
            Ok(serde_json::from_value(body["records"].clone())?)
        }
        ReadLens::Store(store) => store.query(&query, Utc::now()).await,
        ReadLens::Journaled(journaled) => {
            let assembly = journaled
                .read(&query, &ContextBudget::new(16_000), parent)
                .await?;
            Ok(assembly.records)
        }
    }
}

/// Call the calculator through the wiring.
async fn call_calculator(
    wiring: &ToolWiring,
    args: Value,
    parent: Option<String>,
) -> Result<Value> {
    let calculator: Arc<dyn Tool> = Arc::new(CalculatorTool);
    match wiring {
        ToolWiring::Direct => calculator.call(args).await,
        ToolWiring::Recording(journal) => {
            let parent = parent.ok_or_else(|| {
                RustyError::Node("a recording tool needs the node's parent event".to_owned())
            })?;
            RecordingTool::new(calculator, journal.clone(), parent)
                .node("tool:calculator")
                .call(args)
                .await
        }
        ToolWiring::Replaying(source, journal) => {
            let parent = parent.ok_or_else(|| {
                RustyError::Node("a replaying tool needs the node's parent event".to_owned())
            })?;
            ReplayingTool::new(calculator, source.clone(), journal.clone(), parent)
                .call(args)
                .await
        }
    }
}

/// The one-node agent: read the calibration (or, when it is superseded,
/// the correction's example), call the calculator, phrase the answer.
async fn calc_node(ctx: &NodeContext, lens: &ReadLens, wiring: &ToolWiring) -> Result<NodeOutput> {
    // A floor under wall latency: experiment runs are wall-timed, and a
    // sub-millisecond baseline makes the comparison's latency ratio
    // degenerate (baseline p95 of 0 breaches on *any* candidate time,
    // regardless of threshold — compare.rs's None branch). The gate's
    // signal in this story is the pass-rate delta; the sleep just keeps
    // the latency half well-defined. Journaled latencies are read
    // through the run's clock, so under the fixture's logical clock this
    // real-time sleep leaves no trace — replay parity is untouched.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let question = ctx
        .state()
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or(QUESTION)
        .to_owned();
    let parent = ctx
        .config()
        .extra
        .get(PARENT_EVENT_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned);

    // Read 1: the calibration. One read on a hit — the defect-era run
    // and its fixture record exactly one memory read, keeping the
    // replay's exactly-once serving trivially auditable.
    let calibration = read_records(lens, calibration_query(), parent.clone()).await?;
    let (args, source) = if let Some(record) = calibration.first() {
        (inline_content(record)?["args"].clone(), "calibration")
    } else {
        // Read 2: the correction's example for this question.
        let examples = read_records(lens, examples_query(), parent.clone()).await?;
        let mut found = None;
        for record in &examples {
            let content = inline_content(record)?;
            if content["input"]["question"].as_str() == Some(question.as_str()) {
                found = Some(content["corrected"]["tool_args"].clone());
                break;
            }
        }
        match found {
            Some(args) => (args, "example"),
            None => (defect_args(), "default"),
        }
    };

    let result = call_calculator(wiring, args.clone(), parent).await?;
    let answer = format!("the answer is {}", result.as_f64().unwrap_or(0.0));
    let mut updates = std::collections::HashMap::new();
    updates.insert("tool_args".to_owned(), args);
    updates.insert("answer".to_owned(), json!(answer));
    updates.insert("args_source".to_owned(), json!(source));
    Ok(NodeOutput::updates(updates))
}

/// The single-node graph over the given wiring.
fn calc_graph(lens: ReadLens, wiring: ToolWiring) -> (Graph, StateSpec) {
    let spec = StateSpec::new()
        .channel("question", Reducer::Overwrite)
        .channel("tool_args", Reducer::Overwrite)
        .channel("answer", Reducer::Overwrite)
        .channel("args_source", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("agent", move |ctx: NodeContext| {
        let lens = lens.clone();
        let wiring = wiring.clone();
        async move { calc_node(&ctx, &lens, &wiring).await }
    });
    builder.set_entry_point("agent");
    (builder.compile().unwrap(), spec)
}

// --------------------------------------------------------------------- //
// The test-side memory store: a read-only `MemoryStore` over the
// server's own memory routes, so the evaluator's baseline reads exactly
// what production serves.
// --------------------------------------------------------------------- //

#[derive(Clone)]
struct HttpMemoryStore {
    late: LateRouter,
}

impl std::fmt::Debug for HttpMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMemoryStore").finish()
    }
}

#[async_trait::async_trait]
impl MemoryStore for HttpMemoryStore {
    async fn put(&self, _record: &MemoryRecord) -> Result<bool> {
        Err(RustyError::InvalidUpdate(
            "HttpMemoryStore is read-only; writes go through the server's memory routes".to_owned(),
        ))
    }

    async fn get(&self, memory_id: &str) -> Result<Option<MemoryRecord>> {
        let (status, body) =
            node_call(&self.late, "GET", &format!("/memory/{memory_id}"), None).await?;
        match status {
            StatusCode::OK => Ok(Some(serde_json::from_value(body)?)),
            StatusCode::NOT_FOUND => Ok(None),
            other => Err(RustyError::Node(format!(
                "memory get `{memory_id}` failed: {other} {body}"
            ))),
        }
    }

    async fn all(&self) -> Result<Vec<MemoryRecord>> {
        let (status, body) = node_call(
            &self.late,
            "POST",
            "/memory/query",
            Some(json!({"include_superseded": true, "include_expired": true})),
        )
        .await?;
        if status != StatusCode::OK {
            return Err(RustyError::Node(format!(
                "memory all failed: {status} {body}"
            )));
        }
        Ok(serde_json::from_value(body["records"].clone())?)
    }

    async fn remove(&self, _memory_id: &str) -> Result<bool> {
        Err(RustyError::InvalidUpdate(
            "HttpMemoryStore is read-only; forgetting goes through the server's routes".to_owned(),
        ))
    }
}

// --------------------------------------------------------------------- //
// The evaluation agent: the same graph the server serves, prepared for
// the experiment (unjournaled reads against the side's lens) and for the
// fixture re-drive (journaled reads served by the replay).
// --------------------------------------------------------------------- //

struct CalcEvaluationAgent;

impl std::fmt::Debug for CalcEvaluationAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalcEvaluationAgent").finish()
    }
}

#[async_trait::async_trait]
impl EvaluationAgent for CalcEvaluationAgent {
    fn prepare(
        &self,
        _case: &EvalCase,
        _journal: &Journal,
        memory: Arc<dyn MemoryStore>,
    ) -> Result<PreparedRun> {
        // The experiment wiring: unjournaled reads against this side's
        // lens (baseline: the live namespace; candidate: the overlay),
        // direct tool calls. The lens is the only difference between the
        // two sides — the honest "same agent, different memory" claim.
        let (graph, spec) = calc_graph(ReadLens::Store(memory), ToolWiring::Direct);
        Ok(PreparedRun::new(graph, spec))
    }

    async fn redrive(
        &self,
        replay: &ExactReplay,
        _candidate: &Candidate,
        _memory: Arc<dyn MemoryStore>,
    ) -> Result<JournalSnapshot> {
        // The fixture wiring, mirrored: journaled reads served by the
        // recorded run's own memory-read events, tool outputs served by
        // the recorded tool call. The candidate memory is deliberately
        // unused — a replay serves recorded effects, so a candidate
        // cannot alter a replayed run; the experiment half is where the
        // candidate acts.
        let journal = replay.fresh_journal(Clock::logical(FIXTURE_START_MS, FIXTURE_TICK_MS));
        let lens = ReadLens::Journaled(journal.memory(MemorySource::Replay(
            MemoryReplaySource::new(replay.snapshot()),
        )));
        let wiring = ToolWiring::Replaying(replay.source(), journal.clone());
        let (graph, spec) = calc_graph(lens, wiring);
        let outcome = replay
            .run(
                &graph,
                &spec,
                fixture_initial_state(),
                ReplayParams::new(journal, RngSource::seeded(FIXTURE_RNG_SEED)),
            )
            .await?;
        Ok(outcome.journal)
    }
}

/// The fixture's initial state — the same single-channel state the
/// recorded `node_input` event carries.
fn fixture_initial_state() -> State {
    let mut state = State::new();
    state.insert("question", json!(QUESTION));
    state
}

/// Record the defect behavior into a self-contained fixture journal:
/// journaled memory reads against the live (defect-era) namespace, a
/// recording tool, a logical clock and a seeded RNG — everything exact
/// replay needs to reproduce the run after the store moves on.
async fn record_fixture(late: &LateRouter) -> JournalSnapshot {
    let journal = Journal::new(
        FIXTURE_RUN_ID,
        FIXTURE_THREAD_ID,
        Clock::logical(FIXTURE_START_MS, FIXTURE_TICK_MS),
    );
    let memory: Arc<dyn MemoryStore> = Arc::new(HttpMemoryStore { late: late.clone() });
    let lens = ReadLens::Journaled(journal.memory(MemorySource::Store(memory)));
    let wiring = ToolWiring::Recording(journal.clone());
    let (graph, spec) = calc_graph(lens, wiring);
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            fixture_initial_state(),
            RunConfig::new(FIXTURE_THREAD_ID)
                .with_journal(journal.clone())
                .with_rng(RngSource::seeded(FIXTURE_RNG_SEED)),
        )
        .await
        .expect("fixture run failed");
    let ExecutionOutcome::Done(state) = outcome else {
        panic!("the fixture run must complete, got {outcome:?}");
    };
    assert_eq!(
        state.get("answer").and_then(Value::as_str),
        Some("the answer is 0"),
        "the fixture records the defect behavior"
    );
    journal.snapshot()
}

// --------------------------------------------------------------------- //
// The release proof
// --------------------------------------------------------------------- //

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_release_proof_end_to_end() {
    let store = temp_store();
    let dataset_dir = std::env::temp_dir().join(format!(
        "rusty-server-learn-release-datasets-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dataset_dir).unwrap();

    // The versioned dataset: one case, the question, and the state
    // predicates the corrected behavior satisfies (no tool trajectory —
    // the experiment wiring calls the tool directly, so the evidence is
    // the run's final state).
    let case = EvalCase {
        id: "add-two-numbers".to_owned(),
        input: json!({"question": QUESTION}),
        expect: Expectation {
            state: vec![
                StatePredicate {
                    pointer: "/answer".to_owned(),
                    expected: json!("the answer is 5"),
                },
                StatePredicate {
                    pointer: "/args_source".to_owned(),
                    expected: json!("example"),
                },
            ],
            ..Expectation::default()
        },
        tags: Vec::new(),
    };
    Dataset::new("calc-agent", "v2", vec![case])
        .unwrap()
        .save(dataset_dir.join("calc-agent@v2.jsonl"))
        .unwrap();

    // The app: the evaluator is wired before the router exists, the
    // production graph reads through the late handle, and the handle is
    // set once the router is built.
    let late = LateRouter::default();
    let evaluator = EvalCandidateEvaluator::new(
        Arc::new(HttpMemoryStore { late: late.clone() }),
        Arc::new(DirectoryDatasetSource::new(
            dataset_dir.clone(),
            "calc-agent",
        )),
        Arc::new(CalcEvaluationAgent),
        1,
        ProvenanceAuthor::Distiller {
            name: "release-evaluator".to_owned(),
        },
    );
    let mut registry = GraphRegistry::new();
    let (graph, spec) = calc_graph(ReadLens::Http(late.clone()), ToolWiring::Direct);
    registry.register("calc_agent", graph, spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_candidate_evaluator(Arc::new(evaluator));
    let app = router(registry, config);
    late.set(app.clone());

    // -- Chapter 1: the defect --------------------------------------- //
    // The calibration teaches string-typed arguments; the served agent
    // reads it and silently computes with zeros.
    register_agent(&app, AGENT_SCOPE).await;
    let (status, v) = call(
        &app,
        "POST",
        "/memory",
        Some(json!({
            "kind": "fact",
            "scope": {"scope": "agent", "id": AGENT_SCOPE},
            "key": CALIBRATION_KEY,
            "content": {"args_style": "string", "args": defect_args()},
            "author": {"type": "human", "human_id": "amjad"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "calibration write failed: {v}");
    let calibration_id = v["record"]["memory_id"].as_str().unwrap().to_owned();

    let run_calc = |input: Value| {
        let app = app.clone();
        async move {
            let (status, v) = call(
                &app,
                "POST",
                "/threads",
                Some(json!({"graph": "calc_agent"})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "thread failed: {v}");
            let thread_id = v["thread_id"].as_str().unwrap().to_owned();
            let (status, v) = call(
                &app,
                "POST",
                &format!("/threads/{thread_id}/runs/wait"),
                Some(json!({"input": input})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "run failed: {v}");
            v
        }
    };

    let defect_run = run_calc(json!({"question": QUESTION})).await;
    assert_eq!(
        defect_run["output"]["answer"],
        json!("the answer is 0"),
        "the defect: string args parse to zero"
    );
    assert_eq!(defect_run["output"]["args_source"], json!("calibration"));
    let defect_run_id = defect_run["run_id"].as_str().unwrap().to_owned();

    // -- Chapter 2: the evidence ------------------------------------- //
    // The fixture records the defect era (pre-correction namespace);
    // the correction targets the defect run's journaled node input.
    let fixture = record_fixture(&late).await;

    let defect_events = events_of(&app, &defect_run_id).await;
    let node_input_id = defect_events
        .iter()
        .find(|e| e["kind"] == json!("node_input"))
        .and_then(|e| e["id"].as_str())
        .expect("the defect run journals its node input")
        .to_owned();
    let (status, v) = call(
        &app,
        "POST",
        "/memory/corrections",
        Some(json!({
            "correction_id": CORRECTION_ID,
            "author": "amjad",
            "target": {"type": "run_event", "run_id": defect_run_id, "event_id": node_input_id},
            "corrected": {"tool_args": corrected_args()},
            "scope": {"scope": "agent", "id": AGENT_SCOPE},
            "rationale": "the calculator takes numbers, not strings",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "correction failed: {v}");
    assert_eq!(
        v["attribution"],
        json!(format!("human:amjad via correction:{CORRECTION_ID}"))
    );
    assert_eq!(v["candidate"], json!(true));
    let example_id = v["example_id"].as_str().unwrap().to_owned();

    // -- Chapter 3: the candidate and the real evaluation ------------- //
    let (status, example_body) = call(&app, "GET", &format!("/memory/{example_id}"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "example read failed: {example_body}"
    );
    let example_record: MemoryRecord = serde_json::from_value(example_body).unwrap();
    let candidate = Candidate::new(
        CandidateContent::MemorySet {
            scope: ScopeAddress::new(MemoryScope::Agent, AGENT_SCOPE),
            adds: vec![example_record],
            supersedes: vec![calibration_id.clone()],
        },
        ProvenanceAuthor::Distiller {
            name: "correction-loop".to_owned(),
        },
        EvidenceSpan {
            run_ids: vec![defect_run_id.clone()],
            correction_ids: vec![CORRECTION_ID.to_owned()],
            memory_ids: vec![example_id.clone(), calibration_id.clone()],
        },
        ts(1_750_000_100_000),
    )
    .unwrap();

    let (status, v) = call(
        &app,
        "POST",
        "/learn/candidates",
        Some(json!({"candidate": serde_json::to_value(&candidate).unwrap(), "run_id": defect_run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "candidate create failed: {v}");
    let candidate_id = v["candidate_id"].as_str().unwrap().to_owned();

    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/evaluate"),
        Some(json!({
            "request": {
                "dataset_version": "v2",
                "target_metric": "run_pass_rate",
                // The latency bar is generous: the gate's signal in this
                // story is the pass-rate delta, not jitter between two
                // in-process runs.
                "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1000.0},
                "replay_evidence": [serde_json::to_value(&fixture).unwrap()],
            },
            "run_id": defect_run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "evaluate failed: {v}");
    let evaluation = &v["evaluation"];
    assert_eq!(evaluation["dataset_version"], json!("v2"));
    assert_eq!(evaluation["verdict"]["regressed"], json!(false));
    assert_eq!(evaluation["verdict"]["baseline"], json!(0.0));
    assert_eq!(evaluation["verdict"]["candidate"], json!(1.0));
    assert_eq!(evaluation["verdict"]["delta"], json!(1.0));
    assert_eq!(evaluation["replay"]["matched"], json!(1));
    assert_eq!(evaluation["replay"]["fixture_ids"], json!([FIXTURE_RUN_ID]));
    assert!(
        evaluation["replay"]["divergences"].is_null()
            || evaluation["replay"]["divergences"] == json!([]),
        "the fixture re-drives byte-identically: {}",
        evaluation["replay"]
    );

    // -- Chapter 4: the promotion ------------------------------------ //
    // memory_set at agent scope, strictly positive delta, clean replay:
    // in-envelope, no approval token.
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/promote"),
        Some(json!({"run_id": defect_run_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {v}");
    assert_eq!(v["pointer"]["active"], json!(candidate_id));

    let good_run = run_calc(json!({"question": QUESTION})).await;
    assert_eq!(
        good_run["output"]["answer"],
        json!("the answer is 5"),
        "the same question, corrected"
    );
    assert_eq!(good_run["output"]["args_source"], json!("example"));
    assert_eq!(good_run["output"]["tool_args"], corrected_args());
    let good_run_id = good_run["run_id"].as_str().unwrap().to_owned();

    // -- Chapter 5: the evidence chain -------------------------------- //
    // (a) The good run's journaled memory read serves the example.
    let (status, v) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({
            "scope": {"scope": "agent", "id": AGENT_SCOPE},
            "kinds": ["example"],
            "budget": serde_json::to_value(ContextBudget::new(4096)).unwrap(),
            "run_id": good_run_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "journaled query failed: {v}");
    assert!(
        v["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["memory_id"] == json!(example_id)),
        "the overlay serves the example: {v}"
    );
    let good_events = events_of(&app, &good_run_id).await;
    let memory_read = good_events
        .iter()
        .find(|e| e["kind"] == json!("memory_read"))
        .expect("the query journals a memory_read into the run");
    assert!(
        memory_read["output"]["value"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["memory_id"] == json!(example_id)),
        "the journaled read names the example"
    );

    // (b) The example names the correction and the defect run.
    let (status, example_body) = call(&app, "GET", &format!("/memory/{example_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let example: MemoryRecord = serde_json::from_value(example_body).unwrap();
    assert_eq!(
        example.provenance.author,
        ProvenanceAuthor::Human {
            human_id: "amjad".to_owned()
        }
    );
    let evidence = &example.provenance.evidence;
    assert_eq!(evidence.correction_id.as_deref(), Some(CORRECTION_ID));
    assert_eq!(evidence.run_id.as_deref(), Some(defect_run_id.as_str()));

    // (c) The served candidate is byte-identical to the posted one.
    let (status, served) = call(
        &app,
        "GET",
        &format!("/learn/candidates/{candidate_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET candidate failed: {served}");
    assert_eq!(served["status"], json!("promoted"));
    assert_eq!(
        serde_json::to_vec(&served["candidate"]).unwrap(),
        serde_json::to_vec(&serde_json::to_value(&candidate).unwrap()).unwrap(),
        "content-addressed: the served candidate is the posted candidate"
    );

    // (d) The lifecycle is journaled into the defect run, with the
    // verdict the gate read.
    let defect_events = events_of(&app, &defect_run_id).await;
    for kind in [
        "candidate_created",
        "candidate_evaluated",
        "candidate_promoted",
    ] {
        assert!(
            defect_events.iter().any(|e| e["kind"] == json!(kind)),
            "the defect run journals {kind}"
        );
    }
    let evaluated = defect_events
        .iter()
        .find(|e| e["kind"] == json!("candidate_evaluated"))
        .unwrap();
    assert_eq!(
        evaluated["output"]["value"]["verdict"]["delta"],
        json!(1.0),
        "the journaled evaluation carries the verdict the gate read"
    );

    // -- Chapter 6: the rollback, byte-exact -------------------------- //
    let (status, v) = call(
        &app,
        "POST",
        &format!("/learn/candidates/{candidate_id}/rollback"),
        Some(json!({"run_id": defect_run_id, "cause": "operator: regression monitor"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {v}");

    let restored_run = run_calc(json!({"question": QUESTION})).await;
    assert_eq!(
        serde_json::to_vec(&restored_run["output"]).unwrap(),
        serde_json::to_vec(&defect_run["output"]).unwrap(),
        "the restored run's output is byte-identical to the defect run's"
    );
    let node_outputs = |events: &[Value]| {
        events
            .iter()
            .filter(|e| e["kind"] == json!("node_output"))
            .map(|e| e["output"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        node_outputs(&events_of(&app, restored_run["run_id"].as_str().unwrap()).await),
        node_outputs(&events_of(&app, &defect_run_id).await),
        "the node-output payloads are deep-equal across the defect and restored runs"
    );

    let _ = std::fs::remove_dir_all(store);
    let _ = std::fs::remove_dir_all(dataset_dir);
}
