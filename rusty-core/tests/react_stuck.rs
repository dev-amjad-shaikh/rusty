//! No progress is a fault the loop sees for itself: the same tool call with
//! the same arguments runs once per turn; the second request is refused
//! with a notice in the result's place; the third ends the run. Each
//! episode is a typed repair record when the run carries a ledger, and the
//! next turn starts clean.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::react::{
    create_react_agent, MESSAGES_CHANNEL, NO_NEW_FACT_NOTICE, REPEATED_CALL_NOTICE,
};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::repair::{
    InMemoryRepairLedger, RepairComponent, RepairLedger, RepairLedgerHandle, RepairOutcome,
    RepairQuery, RepairTrigger,
};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
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
            model: Some("scripted".into()),
            usage: None,
        })
    }
}

struct CountingEcho {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountingEcho {
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

fn echo_call(id: &str) -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "echo", json!({"text": "hello"}))])
}

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn state(messages: Vec<ChatMessage>) -> State {
    State::from_value(json!({ MESSAGES_CHANNEL: messages })).unwrap()
}

async fn run(
    script: Vec<ChatMessage>,
    initial: State,
) -> (
    RustyResult<ExecutionOutcome>,
    Arc<AtomicUsize>,
    Arc<InMemoryRepairLedger>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(CountingEcho {
        calls: Arc::clone(&calls),
    });
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(script.into()),
    });
    let graph = create_react_agent(model, tools).unwrap();
    let ledger = Arc::new(InMemoryRepairLedger::new());
    let handle = RepairLedgerHandle(Arc::clone(&ledger) as Arc<dyn RepairLedger>);
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = executor
        .run(
            &graph,
            &spec(),
            initial,
            RunConfig::new("t-stuck").with_repair_ledger(handle),
        )
        .await;
    (outcome, calls, ledger)
}

fn stuck_records(ledger: &InMemoryRepairLedger) -> Vec<RepairOutcome> {
    ledger
        .query(&RepairQuery::default())
        .unwrap()
        .into_iter()
        .filter(|r| {
            r.component == RepairComponent::StuckTurnDetector
                && matches!(r.trigger, RepairTrigger::StuckTurn { .. })
        })
        .map(|r| r.outcome)
        .collect()
}

#[tokio::test]
async fn the_second_identical_call_is_refused_with_a_notice_and_the_run_goes_on() {
    let (outcome, calls, ledger) = run(
        vec![
            echo_call("c1"),
            echo_call("c2"),
            ChatMessage::assistant("done"),
        ],
        state(vec![ChatMessage::user("say hello")]),
    )
    .await;
    let state = match outcome.unwrap() {
        ExecutionOutcome::Done(state) => state,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool ran once");
    let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    let notices: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(REPEATED_CALL_NOTICE))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].tool_call_id.as_deref(),
        Some("c2"),
        "the notice answers the refused call"
    );
    assert_eq!(messages.last().unwrap().content.as_deref(), Some("done"));
    assert_eq!(stuck_records(&ledger), vec![RepairOutcome::Repaired]);
}

#[tokio::test]
async fn the_third_identical_call_ends_the_run() {
    let (outcome, calls, ledger) = run(
        vec![
            echo_call("c1"),
            echo_call("c2"),
            echo_call("c3"),
            ChatMessage::assistant("never"),
        ],
        state(vec![ChatMessage::user("say hello")]),
    )
    .await;
    let error = outcome.expect_err("the run stops").to_string();
    assert!(error.contains("no progress"), "{error}");
    assert!(error.contains("echo requested 3 times"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The ledger lists newest first: the refusal, then the stop.
    let outcomes = stuck_records(&ledger);
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes.contains(&RepairOutcome::Repaired) && outcomes.contains(&RepairOutcome::Failed)
    );
}

#[tokio::test]
async fn the_same_call_in_a_new_turn_is_a_new_question() {
    // The thread already holds a turn that made this call; the person asks
    // again, and the call runs again.
    let (outcome, calls, ledger) = run(
        vec![echo_call("c9"), ChatMessage::assistant("done again")],
        state(vec![
            ChatMessage::user("say hello"),
            echo_call("c1"),
            ChatMessage::tool_result("c1", "hello"),
            ChatMessage::assistant("done"),
            ChatMessage::user("say it again"),
        ]),
    )
    .await;
    assert!(matches!(outcome.unwrap(), ExecutionOutcome::Done(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(stuck_records(&ledger).is_empty());
}

#[tokio::test]
async fn different_arguments_are_progress() {
    let (outcome, calls, ledger) = run(
        vec![
            echo_call("c1"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c2",
                "echo",
                json!({"text": "world"}),
            )]),
            ChatMessage::assistant("done"),
        ],
        state(vec![ChatMessage::user("say two things")]),
    )
    .await;
    assert!(matches!(outcome.unwrap(), ExecutionOutcome::Done(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(stuck_records(&ledger).is_empty());
}

// ---------- pairing repair: every call gets an answer ----------

use rusty_agent_runtime::react::{
    repair_unpaired_tool_calls, UNKNOWN_OUTCOME_NOTICE, UNRECORDED_READ_NOTICE,
};

fn write_call(id: &str) -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "send", json!({"to": "x"}))])
}

#[test]
fn an_unanswered_call_is_answered_right_after_its_batch_by_effect() {
    let mut messages = vec![
        ChatMessage::user("go"),
        echo_call("r1"),
        ChatMessage::user("still there?"),
        write_call("w1"),
    ];
    let repaired = repair_unpaired_tool_calls(&mut messages, |tool| tool == "echo");
    assert_eq!(repaired.len(), 2);
    assert_eq!(
        (repaired[0].tool_call_id.as_str(), repaired[0].repeatable),
        ("r1", true)
    );
    assert_eq!(
        (repaired[1].tool_call_id.as_str(), repaired[1].repeatable),
        ("w1", false)
    );
    // The read's notice sits between its batch and the user's next words;
    // the write's closes the thread.
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("r1"));
    assert_eq!(messages[2].content.as_deref(), Some(UNRECORDED_READ_NOTICE));
    assert_eq!(messages[3].content.as_deref(), Some("still there?"));
    assert_eq!(messages[5].tool_call_id.as_deref(), Some("w1"));
    assert_eq!(messages[5].content.as_deref(), Some(UNKNOWN_OUTCOME_NOTICE));
}

#[test]
fn a_partly_answered_batch_gets_only_what_it_lacks_and_a_paired_thread_is_untouched() {
    let mut messages = vec![
        ChatMessage::user("two"),
        ChatMessage::assistant_tool_calls(vec![
            ToolCall::new("a", "echo", json!({"text": "1"})),
            ToolCall::new("b", "send", json!({"to": "y"})),
        ]),
        ChatMessage::tool_result("a", "1"),
        ChatMessage::assistant("half done"),
    ];
    let repaired = repair_unpaired_tool_calls(&mut messages, |tool| tool == "echo");
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired[0].tool_call_id, "b");
    assert!(!repaired[0].repeatable);
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("b"));
    assert_eq!(messages[3].content.as_deref(), Some(UNKNOWN_OUTCOME_NOTICE));
    assert_eq!(messages[4].content.as_deref(), Some("half done"));

    let mut paired = vec![
        ChatMessage::user("one"),
        echo_call("c1"),
        ChatMessage::tool_result("c1", "hello"),
        ChatMessage::assistant("done"),
    ];
    let before = paired.clone();
    assert!(repair_unpaired_tool_calls(&mut paired, |_| true).is_empty());
    assert_eq!(paired, before);
}

#[test]
fn a_tool_the_catalog_forgot_is_treated_as_a_write() {
    let mut messages = vec![ChatMessage::user("x"), echo_call("c1")];
    let repaired = repair_unpaired_tool_calls(&mut messages, |_| false);
    assert!(!repaired[0].repeatable);
    assert_eq!(messages[2].content.as_deref(), Some(UNKNOWN_OUTCOME_NOTICE));
}

/// A search that answers the same empty page whatever it is asked, and a
/// lookup that answers something new each time.
struct SameAgain {
    calls: Arc<AtomicUsize>,
    novel: bool,
}
#[async_trait::async_trait]
impl Tool for SameAgain {
    fn name(&self) -> &str {
        if self.novel {
            "lookup"
        } else {
            "search"
        }
    }
    fn description(&self) -> &str {
        "Searches."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> RustyResult<Value> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.novel {
            Ok(json!({"result": [{"n": n, "q": args.get("q").cloned().unwrap_or(Value::Null)}]}))
        } else {
            Ok(json!({"result": []}))
        }
    }
}

fn search(id: &str, q: &str, novel: bool) -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
        id,
        if novel { "lookup" } else { "search" },
        json!({"q": q}),
    )])
}

async fn run_searches(
    script: Vec<ChatMessage>,
    novel: bool,
) -> (RustyResult<ExecutionOutcome>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(SameAgain {
        calls: Arc::clone(&calls),
        novel,
    });
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(script.into()),
    });
    let graph = create_react_agent(model, tools).unwrap();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = executor
        .run(
            &graph,
            &spec(),
            state(vec![ChatMessage::user("find the fog machine incident")]),
            RunConfig::new("t-nothing-new"),
        )
        .await;
    (outcome, calls)
}

#[tokio::test]
async fn reads_that_answer_nothing_new_are_refused_after_three_and_end_the_run_after_one_more() {
    // Different queries, each answered with the same empty page. An empty
    // answer is nothing new from the first (row 93: the desk once spent 25
    // steps on empty recalls because the first empty page counted as a
    // fact) — three reads run, the fourth is refused with the notice;
    // asked once more, the run stops as no progress.
    let (outcome, calls) = run_searches(
        vec![
            search("c1", "fog machine", false),
            search("c2", "unicorn stables", false),
            search("c3", "fog", false),
            search("c4", "stables", false),
            search("c5", "machine", false),
        ],
        false,
    )
    .await;
    let error = outcome.unwrap_err().to_string();
    assert!(
        error.contains("no progress") && error.contains("answered nothing new"),
        "{error}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "three reads ran; the fourth was refused, the fifth ended the run"
    );
}

#[tokio::test]
async fn a_refused_read_can_still_end_well_by_answering() {
    let (outcome, calls) = run_searches(
        vec![search("c1", "fog machine", false), search("c2", "unicorn", false), search("c3", "fog", false), search("c4", "stables", false), ChatMessage::assistant("There is no such incident; say which system it was reported in and I will look there.")],
        false,
    )
    .await;
    let state = match outcome.unwrap() {
        ExecutionOutcome::Done(state) => state,
        other => panic!("expected Done, got {other:?}"),
    };
    let messages: Vec<ChatMessage> = state.get_as(MESSAGES_CHANNEL).unwrap().unwrap();
    let notices = messages
        .iter()
        .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(NO_NEW_FACT_NOTICE))
        .count();
    assert_eq!(notices, 1, "the fourth read got the notice");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn reads_that_answer_something_new_are_never_refused() {
    let (outcome, calls) = run_searches(
        vec![
            search("c1", "a", true),
            search("c2", "b", true),
            search("c3", "c", true),
            search("c4", "d", true),
            search("c5", "e", true),
            ChatMessage::assistant("done"),
        ],
        true,
    )
    .await;
    assert!(matches!(outcome.unwrap(), ExecutionOutcome::Done(_)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "every read answered something new and ran"
    );
}

/// A read the system has since contradicted may be taken again.
///
/// The shape is the one a changed schema forces: read the schema, list
/// under it, and be refused because the fields moved. Re-reading the schema
/// is the repair — the first read's answer is exactly what the refusal
/// called wrong — so the loop must let it through. Asking again with
/// nothing failed since is a repeat, and refused.
struct CountingSchema {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountingSchema {
    fn name(&self) -> &str {
        "schema"
    }
    fn description(&self) -> &str {
        "The fields this version has."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"fields": ["id", "posted_on"]}))
    }
}

/// Lists one page, then refuses: the fields it was given are last
/// version's.
struct ShiftingList {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for ShiftingList {
    fn name(&self) -> &str {
        "list"
    }
    fn description(&self) -> &str {
        "Entries, a page at a time."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"cursor": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(json!({"entries": 25, "next_cursor": "e25"}));
        }
        Err(RustyError::Tool(
            "`date` is not a field of schema v2".into(),
        ))
    }
}

fn schema_call(id: &str) -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "schema", json!({}))])
}

fn list_call(id: &str, cursor: Value) -> ChatMessage {
    ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, "list", json!({"cursor": cursor}))])
}

/// Runs the script against the shifting system, answering with how many
/// times the schema was actually read.
async fn run_shifting(script: Vec<ChatMessage>) -> (State, usize) {
    let schema_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(CountingSchema {
        calls: Arc::clone(&schema_calls),
    });
    tools.register(ShiftingList {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        script: Mutex::new(script.into()),
    });
    let graph = create_react_agent(model, tools).unwrap();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = executor
        .run(
            &graph,
            &spec(),
            state(vec![ChatMessage::user("count the entries")]),
            RunConfig::new("t-shift"),
        )
        .await;
    match outcome.unwrap() {
        ExecutionOutcome::Done(state) => (state, schema_calls.load(Ordering::SeqCst)),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn messages_of(state: &State) -> Vec<ChatMessage> {
    state.get_as(MESSAGES_CHANNEL).unwrap().unwrap()
}

#[tokio::test]
async fn a_read_the_system_contradicted_may_be_taken_again() {
    let (state, schema_reads) = run_shifting(vec![
        schema_call("s1"),
        list_call("l1", Value::Null),
        list_call("l2", json!("e25")),
        schema_call("s2"),
        ChatMessage::assistant("60 entries"),
    ])
    .await;
    assert_eq!(
        schema_reads, 2,
        "the re-read ran: the refusal called the first answer wrong"
    );
    let messages = messages_of(&state);
    assert!(
        !messages
            .iter()
            .any(|m| m.content.as_deref() == Some(REPEATED_CALL_NOTICE)),
        "nothing was refused as a repeat"
    );
    assert_eq!(
        messages.last().unwrap().content.as_deref(),
        Some("60 entries")
    );
}

#[tokio::test]
async fn a_re_read_with_nothing_failed_since_is_still_a_repeat() {
    let (state, schema_reads) = run_shifting(vec![
        schema_call("s1"),
        list_call("l1", Value::Null),
        list_call("l2", json!("e25")),
        schema_call("s2"),
        schema_call("s3"),
        ChatMessage::assistant("60 entries"),
    ])
    .await;
    assert_eq!(schema_reads, 2, "the third ask read nothing");
    let refused: Vec<ChatMessage> = messages_of(&state)
        .into_iter()
        .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(REPEATED_CALL_NOTICE))
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "a second ask with nothing failed since is a repeat"
    );
    assert_eq!(refused[0].tool_call_id.as_deref(), Some("s3"));
}
