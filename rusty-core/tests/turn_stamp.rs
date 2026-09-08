//! Turn-stamp registration in the prebuilt ReAct loop (EP-07-S12 AC1):
//! a run whose scheduler attached a stamp dispatches every provider call
//! — blocking and streaming — through the stamped seam, re-attributed
//! with the boundary the dispatcher honestly knows, and an unstamped run
//! behaves exactly as before.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, GraphEvent, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::prelude::{ComponentAttribution, TrafficClass, TurnBoundary, TurnStamp};
use rusty_agent_runtime::react::{
    MESSAGES_CHANNEL, create_react_agent_streaming, create_react_agent_with_recording,
};
use rusty_agent_runtime::record::RunEventKind;
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

const THREAD_ID: &str = "t-stamps";

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state() -> State {
    State::from_value(json!({
        MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user("say hello")).unwrap()]
    }))
    .unwrap()
}

/// Which entry point a dispatch used, and the stamp it carried.
#[derive(Debug, Clone)]
struct Observed {
    entry: &'static str,
    stamp: Option<TurnStamp>,
}

/// A scripted model recording how each dispatch arrived. A stamped run
/// must never touch the plain paths; an unstamped run must never touch
/// the stamped ones.
#[derive(Default)]
struct StampProbe {
    script: Mutex<VecDeque<ChatMessage>>,
    calls: Mutex<Vec<Observed>>,
}

impl StampProbe {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Observed> {
        self.calls.lock().unwrap().clone()
    }

    fn respond(&self, entry: &'static str, stamp: Option<&TurnStamp>) -> ChatResponse {
        self.calls.lock().unwrap().push(Observed {
            entry,
            stamp: stamp.cloned(),
        });
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ChatMessage::assistant("done"));
        ChatResponse {
            message,
            model: None,
            usage: None,
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for StampProbe {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        Ok(self.respond("chat", None))
    }

    async fn chat_stamped(
        &self,
        stamp: &TurnStamp,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> RustyResult<ChatResponse> {
        Ok(self.respond("chat_stamped", Some(stamp)))
    }

    async fn chat_stream(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
        _on_token: &mut (dyn FnMut(rusty_agent_runtime::llm::TokenChunk) + Send),
    ) -> RustyResult<ChatResponse> {
        Ok(self.respond("chat_stream", None))
    }

    async fn chat_stream_stamped(
        &self,
        stamp: &TurnStamp,
        _messages: &[ChatMessage],
        _tools: &[Value],
        _on_token: &mut (dyn FnMut(rusty_agent_runtime::llm::TokenChunk) + Send),
    ) -> RustyResult<ChatResponse> {
        Ok(self.respond("chat_stream_stamped", Some(stamp)))
    }
}

/// The echo tool: answers immediately with its input.
struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "echoes the input text"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }

    async fn call(&self, arguments: Value) -> RustyResult<Value> {
        Ok(arguments)
    }
}

fn echo_tools() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(EchoTool);
    tools
}

/// The scheduler's honest identity for the run.
fn material() -> TurnStamp {
    TurnStamp {
        session_id: uuid::Uuid::nil(),
        traffic: TrafficClass::Main,
        turn_id: uuid::Uuid::from_u128(0x5ea50),
        // The dispatcher re-attributes both fields below; the values here
        // are the scheduler's placeholders.
        turn_boundary: TurnBoundary::Start,
        issued_by: ComponentAttribution {
            component: String::new(),
            sub_id: None,
        },
    }
}

fn script() -> Vec<ChatMessage> {
    vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c1",
            "echo",
            json!({"text": "hello"}),
        )]),
        ChatMessage::assistant("the echo said: hello"),
    ]
}

#[tokio::test]
async fn a_stamped_run_dispatches_every_call_stamped() {
    let journal = Journal::new(
        "run-stamped",
        THREAD_ID,
        Clock::logical(1_700_000_000_000, 10),
    );
    let model = Arc::new(StampProbe::new(script()));
    let probe = Arc::clone(&model);
    let graph = create_react_agent_with_recording(model, echo_tools(), journal.clone()).unwrap();
    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal.clone())
                .with_turn_stamp(material()),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ExecutionOutcome::Done(_)),
        "the stamped session completes"
    );

    let calls = probe.calls();
    assert_eq!(calls.len(), 2, "tool loop drives two model calls");
    for call in &calls {
        assert_eq!(call.entry, "chat_stamped", "no plain dispatch may occur");
        let stamp = call.stamp.as_ref().expect("the stamp rides every call");
        assert_eq!(stamp.session_id, uuid::Uuid::nil());
        assert_eq!(stamp.turn_id, uuid::Uuid::from_u128(0x5ea50));
        assert_eq!(stamp.traffic, TrafficClass::Main);
        // The scheduler left the component unnamed; the dispatcher names
        // itself honestly.
        assert_eq!(stamp.issued_by.component, "react_agent");
    }
    // The boundary is the dispatcher's own knowledge: the run's first
    // dispatch starts the turn, the tool-loop iteration continues it.
    assert_eq!(
        calls[0].stamp.as_ref().unwrap().turn_boundary,
        TurnBoundary::Start
    );
    assert_eq!(
        calls[1].stamp.as_ref().unwrap().turn_boundary,
        TurnBoundary::Continuation
    );

    // AC6's accrual: every stamped dispatch journaled its RequestHeader
    // before dispatch — the labels a later training consumer harvests.
    let headers: Vec<TurnStamp> = journal
        .events()
        .into_iter()
        .filter(|event| event.kind == RunEventKind::RequestHeader)
        .map(|event| {
            let input = journal.resolve(event.input.as_ref().unwrap()).unwrap();
            serde_json::from_value(input).expect("the header input is a TurnStamp")
        })
        .collect();
    assert_eq!(headers.len(), 2, "one header per provider call");
    assert_eq!(&headers[0], calls[0].stamp.as_ref().unwrap());
    assert_eq!(&headers[1], calls[1].stamp.as_ref().unwrap());
}

#[tokio::test]
async fn an_unstamped_run_is_byte_identical_to_before() {
    let journal = Journal::new(
        "run-plain",
        THREAD_ID,
        Clock::logical(1_700_000_000_000, 10),
    );
    let model = Arc::new(StampProbe::new(script()));
    let probe = Arc::clone(&model);
    let graph = create_react_agent_with_recording(model, echo_tools(), journal.clone()).unwrap();
    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID).with_journal(journal),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));
    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(
            call.entry, "chat",
            "an unstamped run never touches the seam"
        );
        assert!(call.stamp.is_none());
    }
}

#[tokio::test]
async fn a_streaming_stamped_run_dispatches_through_the_stamped_stream() {
    let model = Arc::new(StampProbe::new(script()));
    let probe = Arc::clone(&model);
    let (token_tx, token_rx) = tokio::sync::mpsc::channel::<GraphEvent>(16);
    let graph = create_react_agent_streaming(model, echo_tools(), token_tx).unwrap();
    let journal = Journal::new(
        "run-stream",
        THREAD_ID,
        Clock::logical(1_700_000_000_000, 10),
    );
    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal)
                .with_turn_stamp(material()),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));
    // The probe emits no token deltas; the token sink is exercised only
    // for liveness of the streaming dispatch path.
    let _ = token_rx;

    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(
            call.entry, "chat_stream_stamped",
            "a stamped streaming dispatch uses the stamped stream seam"
        );
        assert_eq!(
            call.stamp.as_ref().unwrap().issued_by.component,
            "react_agent"
        );
    }
    assert_eq!(
        calls[0].stamp.as_ref().unwrap().turn_boundary,
        TurnBoundary::Start
    );
    assert_eq!(
        calls[1].stamp.as_ref().unwrap().turn_boundary,
        TurnBoundary::Continuation
    );
}

#[tokio::test]
async fn a_named_component_is_preserved() {
    let journal = Journal::new(
        "run-named",
        THREAD_ID,
        Clock::logical(1_700_000_000_000, 10),
    );
    let model = Arc::new(StampProbe::new(vec![ChatMessage::assistant("done")]));
    let probe = Arc::clone(&model);
    let graph = create_react_agent_with_recording(model, echo_tools(), journal.clone()).unwrap();
    let named = TurnStamp {
        issued_by: ComponentAttribution {
            component: "review_fork".to_string(),
            sub_id: Some("fork-7".to_string()),
        },
        traffic: TrafficClass::Side,
        ..material()
    };
    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(),
            RunConfig::new(THREAD_ID)
                .with_journal(journal)
                .with_turn_stamp(named),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));
    let calls = probe.calls();
    assert_eq!(calls.len(), 1);
    let stamp = calls[0].stamp.as_ref().unwrap();
    // A scheduler that names its component keeps its attribution — the
    // dispatcher only fills the blank; side traffic stays side.
    assert_eq!(stamp.issued_by.component, "review_fork");
    assert_eq!(stamp.issued_by.sub_id.as_deref(), Some("fork-7"));
    assert_eq!(stamp.traffic, TrafficClass::Side);
}
