//! Simulation integration tests (EP-12-S07).
//!
//! - **AC 1** — scenario-schema validation and scripted-message injection.
//! - **AC 2** — simulation runs as an ordinary session (real kernel, real log).
//! - **AC 3** — transcript, trace, and termination cause are case artifacts.
//! - **AC 4** — steering via the real inbox, drain semantics from the log.
//! - **AC 5** — scripted-simulator determinism across repetitions.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use rusty_agent_runtime::error::{Result as RuntimeResult, RustyError};
use rusty_agent_runtime::inbox::{ConsumptionPoint, Inbox, InboxConsumption, InboxKind};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent_with_recording, MESSAGES_CHANNEL};
use rusty_agent_runtime::record::RunEventKind;
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use serde_json::{json, Value};

use rusty_eval::{
    run_simulation, BehaviorRule, SimulationScenario, SteeringTool, TerminationCause,
    TerminationCriteria, Trigger, UserAction,
};

// ---------- helpers ----------

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

/// A scripted model: pops one canned response per `chat` call.
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> RuntimeResult<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: None,
            usage: None,
        })
    }
}

struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its input."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn call(&self, args: Value) -> RuntimeResult<Value> {
        Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
    }
}

fn echo_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    registry
}

fn output_payload(event: &rusty_agent_runtime::record::RunEvent) -> Value {
    match event.output.as_ref().expect("event carries an output") {
        rusty_agent_runtime::record::PayloadRef::Inline(value) => value.clone(),
        other => panic!("expected an inline payload, got {other:?}"),
    }
}

// ---------- AC 1: scenario schema validation ----------

#[test]
fn scenario_schema_round_trip() {
    let scenario = SimulationScenario {
        persona: "A frustrated customer".into(),
        goal: "Get a refund".into(),
        opening_message: "I want my money back".into(),
        max_turns: 5,
        script: vec![
            "The product arrived broken".into(),
            "I have the receipt".into(),
        ],
        behavior_rules: vec![BehaviorRule {
            trigger: Trigger::ResponseContains("refund".into()),
            action: UserAction::Say("Thank you".into()),
        }],
        termination: TerminationCriteria::GoalSatisfied {
            response_contains: "refund approved".into(),
        },
    };

    let json = serde_json::to_string(&scenario).unwrap();
    let parsed: SimulationScenario = serde_json::from_str(&json).unwrap();
    assert_eq!(scenario, parsed);
}

#[test]
fn scenario_invalid_json_rejected() {
    let bad =
        r#"{"persona": "x", "goal": "y", "opening_message": "z", "max_turns": "not_a_number"}"#;
    let result: Result<SimulationScenario, _> = serde_json::from_str(bad);
    assert!(result.is_err());
}

// ---------- AC 2: simulation runs as ordinary session ----------

#[tokio::test]
async fn simulation_runs_through_real_kernel_with_real_log() {
    let scenario = SimulationScenario {
        persona: "Test user".into(),
        goal: "Complete two turns".into(),
        opening_message: "Hello".into(),
        max_turns: 5,
        script: vec!["Follow-up one".into(), "Follow-up two".into()],
        behavior_rules: vec![],
        termination: TerminationCriteria::ScriptedEnding,
    };

    let model = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant("Response one"),
        ChatMessage::assistant("Response two"),
        ChatMessage::assistant("Response three"),
    ]));

    let result = run_simulation(&scenario, model, Inbox::new(), |sim_model, journal| {
        let graph = create_react_agent_with_recording(sim_model, echo_registry(), journal.clone())?;
        let initial_state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user(&scenario.opening_message)).unwrap()]
        }))?;
        Ok((graph, spec(), initial_state))
    })
    .await
    .unwrap();

    // The conversation has the opening + 3 assistant responses.
    let texts: Vec<&str> = result
        .transcript
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect();
    assert_eq!(
        texts,
        [
            "Hello",
            "Response one",
            "Follow-up one",
            "Response two",
            "Follow-up two",
            "Response three"
        ]
    );

    // Evidence was produced.
    assert!(result.evidence.status.is_done());
    assert_eq!(result.turn_count, 2);
    assert_eq!(result.termination, TerminationCause::ScriptedEnding);
}

// ---------- AC 3: transcript, trace, termination cause ----------

#[tokio::test]
async fn simulation_produces_eval_compatible_artifacts() {
    let scenario = SimulationScenario {
        persona: "Test user".into(),
        goal: "Reach goal".into(),
        opening_message: "Start".into(),
        max_turns: 3,
        script: vec!["Step 1".into()],
        behavior_rules: vec![],
        termination: TerminationCriteria::GoalSatisfied {
            response_contains: "done".into(),
        },
    };

    let model = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant("not done"),
        ChatMessage::assistant("all done"),
    ]));

    let result = run_simulation(&scenario, model, Inbox::new(), |sim_model, journal| {
        let graph = create_react_agent_with_recording(sim_model, echo_registry(), journal.clone())?;
        let initial_state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user(&scenario.opening_message)).unwrap()]
        }))?;
        Ok((graph, spec(), initial_state))
    })
    .await
    .unwrap();

    // Termination cause is goal-satisfied.
    assert_eq!(result.termination, TerminationCause::GoalSatisfied);

    // Evidence carries the full transcript in final_state.
    let transcript: Vec<ChatMessage> = serde_json::from_value(
        result
            .evidence
            .final_state
            .get("messages")
            .cloned()
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    assert!(transcript.len() >= 4);

    // Journal snapshot is available on the result.
    assert!(!result.journal.events.is_empty(), "journal has events");
}

// ---------- AC 4: steering via real inbox ----------

#[tokio::test]
async fn steering_scenario_delivers_via_inbox_and_journals_drain() {
    let inbox = Inbox::new();
    let scenario = SimulationScenario {
        persona: "Steering tester".into(),
        goal: "Test mid-turn steering".into(),
        opening_message: "Call the echo tool".into(),
        max_turns: 5,
        script: vec![],
        behavior_rules: vec![],
        termination: TerminationCriteria::ScriptedEnding,
    };

    let model = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c1",
            "steer",
            json!({"text": "go"}),
        )]),
        ChatMessage::assistant("steered answer"),
    ]));

    let mut tools = echo_registry();
    tools.register(SteeringTool::new(inbox.clone(), "actually, metric units"));

    let result = run_simulation(&scenario, model, inbox, |sim_model, journal| {
        let graph = create_react_agent_with_recording(sim_model, tools.clone(), journal.clone())?;
        let initial_state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user(&scenario.opening_message)).unwrap()]
        }))?;
        Ok((graph, spec(), initial_state))
    })
    .await
    .unwrap();

    // The steering message reached the assistant.
    let texts: Vec<&str> = result
        .transcript
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect();
    assert!(
        texts.contains(&"actually, metric units"),
        "steering message in transcript: {texts:?}"
    );

    // The journal records inbox intake and consumption.
    let snapshot = result.journal;
    let intakes: Vec<_> = snapshot
        .events
        .iter()
        .filter(|e| e.kind == RunEventKind::InboxIntake)
        .collect();
    assert!(!intakes.is_empty(), "inbox intake journaled");

    let consumptions: Vec<_> = snapshot
        .events
        .iter()
        .filter(|e| e.kind == RunEventKind::InboxConsumed)
        .collect();
    assert!(!consumptions.is_empty(), "inbox consumption journaled");

    // Verify consumption point is a step boundary (mid-turn steering).
    let consumption: InboxConsumption =
        serde_json::from_value(output_payload(consumptions[0])).unwrap();
    assert_eq!(consumption.point, ConsumptionPoint::StepBoundary);
    assert_eq!(consumption.messages.len(), 1);
    assert_eq!(consumption.messages[0].kind, InboxKind::Steering);
    assert_eq!(
        consumption.messages[0].content,
        json!("actually, metric units")
    );
}

// ---------- AC 5: scripted-simulator determinism ----------

#[tokio::test]
async fn scripted_simulation_is_deterministic_across_repetitions() {
    let scenario = SimulationScenario {
        persona: "Determinism checker".into(),
        goal: "Same result every time".into(),
        opening_message: "Hello".into(),
        max_turns: 3,
        script: vec!["Turn 1".into(), "Turn 2".into()],
        behavior_rules: vec![],
        termination: TerminationCriteria::ScriptedEnding,
    };

    let mut results = Vec::new();
    for _ in 0..5 {
        let model = Arc::new(ScriptedModel::new(vec![
            ChatMessage::assistant("A"),
            ChatMessage::assistant("B"),
            ChatMessage::assistant("C"),
        ]));

        let result = run_simulation(&scenario, model, Inbox::new(), |sim_model, journal| {
            let graph = create_react_agent_with_recording(sim_model, echo_registry(), journal.clone())?;
            let initial_state = State::from_value(json!({
                MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user(&scenario.opening_message)).unwrap()]
            }))?;
            Ok((graph, spec(), initial_state))
        })
        .await
        .unwrap();

        results.push(result);
    }

    // All repetitions produce identical transcripts.
    let first = &results[0];
    for result in &results[1..] {
        assert_eq!(
            result.transcript, first.transcript,
            "transcript mismatch across repetitions"
        );
        assert_eq!(result.turn_count, first.turn_count);
        assert_eq!(result.termination, first.termination);
        assert_eq!(result.evidence.status, first.evidence.status);
    }
}

// ---------- additional: max_turns termination ----------

#[tokio::test]
async fn max_turns_terminates_early() {
    let scenario = SimulationScenario {
        persona: "Early stop".into(),
        goal: "Stop at 2 turns".into(),
        opening_message: "Go".into(),
        max_turns: 2,
        script: vec!["One".into(), "Two".into(), "Three".into()],
        behavior_rules: vec![],
        termination: TerminationCriteria::MaxTurns(2),
    };

    let model = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant("R1"),
        ChatMessage::assistant("R2"),
        ChatMessage::assistant("R3"),
    ]));

    let result = run_simulation(&scenario, model, Inbox::new(), |sim_model, journal| {
        let graph = create_react_agent_with_recording(sim_model, echo_registry(), journal.clone())?;
        let initial_state = State::from_value(json!({
            MESSAGES_CHANNEL: [serde_json::to_value(ChatMessage::user(&scenario.opening_message)).unwrap()]
        }))?;
        Ok((graph, spec(), initial_state))
    })
    .await
    .unwrap();

    assert!(
        result.turn_count <= 2,
        "turn_count={}, transcript={:?}",
        result.turn_count,
        result.transcript
    );
}
