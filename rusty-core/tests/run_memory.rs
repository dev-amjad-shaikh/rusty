//! A run reads its memory into every model call, and a tool knows the run
//! it acts in. With a policy that has a memory section and a memory source
//! on the run, the agent node reads the source through the journaled seam
//! and the model sees a `# Memory` section; a dispatched tool sees who the
//! run is for and which agent it is of through `current_run`; and the
//! recording replays byte for byte with the reads served from the log.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
use rusty_agent_runtime::context::ContextPolicy;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot, RngSource};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::memory::{
    InMemoryMemoryStore, MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, MemorySource,
    MemoryStore, ProvenanceAuthor, ScopeAddress, ValidityWindow,
};
use rusty_agent_runtime::react::{create_react_agent, create_react_agent_replaying, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::replay::{ExactReplay, ReplayParams};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{current_run, Tool, ToolRegistry};

const RNG_SEED: u64 = 11;
const THREAD: &str = "t-run-memory";

fn logical_clock() -> Clock {
    Clock::logical(1_700_000_000_000, 10)
}

type Seen = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
    seen: Seen,
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let message = self.script.lock().unwrap().pop_front().ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse { message, model: Some("scripted-memory".into()), usage: None })
    }
}

struct PanicModel(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ChatModel for PanicModel {
    async fn chat(&self, _: &[ChatMessage], _: &[Value]) -> RustyResult<ChatResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("replay reached a model")
    }
}

/// Answers with what `current_run` says about the run it acts in.
struct WhoAmI;

#[async_trait::async_trait]
impl Tool for WhoAmI {
    fn name(&self) -> &str {
        "whoami"
    }
    fn description(&self) -> &str {
        "Says who the run is for."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(match current_run() {
            Some(run) => json!({
                "person": run.person_id(),
                "agent": run.agent_id,
                "thread": run.thread_id,
                "has_journal": run.journal.is_some(),
            }),
            None => json!({"run": null}),
        })
    }
}

fn tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(WhoAmI);
    registry
}

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [
            serde_json::to_value(ChatMessage::system("You are Recall.")).unwrap(),
            serde_json::to_value(ChatMessage::user("who am I?")).unwrap(),
        ]
    }))
    .unwrap()
}

async fn store_with_a_preference() -> Arc<dyn MemoryStore> {
    let store = InMemoryMemoryStore::new();
    let now = chrono::Utc::now();
    let record = MemoryRecord::new(
        MemoryKind::Preference,
        ScopeAddress::new(MemoryScope::User, "bob"),
        MemoryProvenance { author: ProvenanceAuthor::Human { human_id: "bob".into() }, evidence: Default::default(), written_at: now },
        1.0,
        ValidityWindow { valid_from: now, valid_until: None },
        now,
        json!({"text": "I prefer metric units"}),
    )
    .unwrap();
    store.put(&record).await.unwrap();
    Arc::new(store)
}

fn model(seen: &Seen) -> Arc<dyn ChatModel> {
    Arc::new(ScriptedModel {
        script: Mutex::new(
            vec![
                ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "whoami", json!({}))]),
                ChatMessage::assistant("you are bob, in metric"),
            ]
            .into(),
        ),
        seen: Arc::clone(seen),
    })
}

async fn record_run() -> (JournalSnapshot, State, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let journal = Journal::new("run-memory", THREAD, logical_clock());
    let graph = create_react_agent(model(&seen), tools()).unwrap();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let policy = ContextPolicy::standard(8192).with_memory_section(512);
    let config = RunConfig::new(THREAD)
        .with_journal(journal.clone())
        .with_rng(RngSource::seeded(RNG_SEED))
        .with_context_policy(&policy)
        .with_attribution(json!({"principal_id": "bob", "name": "Bob", "kind": "user"}))
        .with_acting_for("bob")
        .with_agent_id("recall-1")
        .with_memory_source(MemorySource::Store(store_with_a_preference().await));
    match executor.run(&graph, &spec(), initial_state(), config).await.unwrap() {
        ExecutionOutcome::Done(state) => (journal.snapshot(), state, seen),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn the_model_reads_memory_and_a_tool_knows_its_run() {
    let (snapshot, state, seen) = record_run().await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let memory = seen[0]
        .iter()
        .find(|m| m.role == Role::System && m.content.as_deref().is_some_and(|c| c.starts_with("# Memory")))
        .expect("the first model call carries a # Memory section");
    let text = memory.content.as_deref().unwrap();
    assert!(text.contains("I prefer metric units") && text.contains("user:bob"), "{text}");
    // The situation: the date from the run's logical clock, and whom the
    // run is a conversation with — declared on the config, so the tool's
    // person and the model's "you" agree.
    let situation = seen[0]
        .iter()
        .find(|m| m.role == Role::System && m.content.as_deref().is_some_and(|c| c.starts_with("Today is ")))
        .expect("the first model call carries the situation");
    let situation = situation.content.as_deref().unwrap();
    assert!(situation.contains("2023-11-14"), "the logical clock's date: {situation}");
    assert!(situation.contains("This conversation is with bob"), "{situation}");

    let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    let tool_result = messages.iter().find(|m| m.role == Role::Tool).expect("the tool ran");
    let said = tool_result.content.as_deref().unwrap();
    assert!(said.contains("\"person\":\"bob\"") || said.contains("\"person\": \"bob\""), "{said}");
    assert!(said.contains("recall-1"), "{said}");
    assert!(said.contains("t-run-memory"), "{said}");
    assert!(said.contains("\"has_journal\":true") || said.contains("\"has_journal\": true"), "{said}");

    // The read is evidence: one MemoryRead event under the pipeline.
    assert!(snapshot.events.iter().any(|e| e.kind == rusty_agent_runtime::record::RunEventKind::MemoryRead));
}

#[tokio::test]
async fn a_run_that_read_memory_replays_from_the_log() {
    let (snapshot, recorded_state, _) = record_run().await;
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let journal = replay.fresh_journal(logical_clock());
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = create_react_agent_replaying(Arc::new(PanicModel(Arc::clone(&calls))), tools(), replay.source(), journal.clone()).unwrap();
    let params = ReplayParams::new(journal, RngSource::seeded(RNG_SEED)).with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let replayed = replay.run_and_verify(&graph, &spec(), initial_state(), params).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(serde_json::to_string(&snapshot).unwrap(), serde_json::to_string(&replayed.journal).unwrap());
    match &replayed.outcome {
        ExecutionOutcome::Done(state) => assert_eq!(state, &recorded_state),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn outside_a_run_a_tool_sees_no_run() {
    assert!(current_run().is_none());
}

#[tokio::test]
async fn a_declared_counterpart_wins_over_the_attribution() {
    // A run a schedule fired: the attribution is the person who created the
    // schedule, but the application declares nobody as the counterpart —
    // the tool sees no person.
    let ctx = rusty_agent_runtime::tool::RunContext {
        attribution: Some(json!({"principal_id": "bob", "name": "Bob", "kind": "user"})),
        ..Default::default()
    };
    assert_eq!(ctx.person_id(), Some("bob"), "no declaration: the attribution's user");
    use rusty_agent_runtime::tool::Counterpart;
    let declared = rusty_agent_runtime::tool::RunContext { counterpart: Some(Counterpart::Person("cy".into())), ..ctx.clone() };
    assert_eq!(declared.person_id(), Some("cy"), "a declared counterpart wins");
    let nobody = rusty_agent_runtime::tool::RunContext { counterpart: Some(Counterpart::Nobody), ..ctx.clone() };
    assert_eq!(nobody.person_id(), None, "a declared nobody is nobody, whoever the attribution names");
    assert_eq!(Counterpart::from_value(&Counterpart::Nobody.to_value()), Counterpart::Nobody);
    assert_eq!(Counterpart::from_value(&Counterpart::Person("bob".into()).to_value()), Counterpart::Person("bob".into()));
    assert!(!Counterpart::Nobody.to_value().is_null(), "nobody is never null on the wire");
    assert_eq!(Counterpart::from_value(&json!("bob")), Counterpart::Person("bob".into()));
}
