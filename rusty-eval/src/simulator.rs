//! Simulated users for multi-turn eval scenarios.
//!
//! A simulation drives a declared persona through a multi-turn conversation
//! with a candidate agent, using the real kernel and inbox injection. The
//! conversation is logged as an ordinary session that eval assertions and
//! scorers evaluate.
//!
//! Two modes:
//!
//! - **Scripted** — every user turn is pre-declared; the run is deterministic
//!   and CI-safe.
//! - **Live** — a simulator model decides each turn from the conversation
//!   history (not yet implemented; blocked on `traffic: side` session stamping).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rusty_agent_runtime::error::{Result as RuntimeResult, RustyError};
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::graph::Graph;
use rusty_agent_runtime::inbox::Inbox;
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role};
use rusty_agent_runtime::state::{State, StateSpec};

use crate::error::{EvalError, Result};
use crate::evidence::{RunEvidence, RunStatus};

/// A declarative multi-turn simulation scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationScenario {
    /// The simulated user's persona description.
    pub persona: String,

    /// The goal the simulated user wants to achieve.
    pub goal: String,

    /// The opening message that starts the conversation.
    pub opening_message: String,

    /// Maximum number of turns before forced termination.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,

    /// For scripted mode: the sequence of user messages after the opening.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub script: Vec<String>,

    /// Per-turn behavior rules (evaluated in order; for live mode).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub behavior_rules: Vec<BehaviorRule>,

    /// When to stop the simulation.
    #[serde(default)]
    pub termination: TerminationCriteria,
}

fn default_max_turns() -> usize {
    10
}

/// A behavior rule: when `trigger` fires, perform `action`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorRule {
    pub trigger: Trigger,
    pub action: UserAction,
}

/// When a behavior rule fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Fire on every turn.
    Always,
    /// Fire when the assistant's response contains this substring.
    ResponseContains(String),
    /// Fire at a specific 1-based turn number.
    AtTurn(usize),
}

/// What the simulated user does when a rule fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAction {
    /// Send this message as a follow-up (next turn).
    Say(String),
    /// Send this message as steering (mid-turn, next step boundary).
    Steer(String),
    /// End the simulation.
    End,
}

/// When to terminate a simulation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationCriteria {
    /// Stop after this many turns.
    MaxTurns(usize),
    /// Stop when the assistant's response contains this substring.
    GoalSatisfied { response_contains: String },
    /// Stop when the scripted messages are exhausted.
    #[default]
    ScriptedEnding,
    /// Stop when any criterion is met.
    AnyOf(Vec<TerminationCriteria>),
}

impl TerminationCriteria {
    fn check(&self, turn: usize, last_response: Option<&str>) -> bool {
        match self {
            TerminationCriteria::MaxTurns(n) => turn >= *n,
            TerminationCriteria::GoalSatisfied { response_contains } => {
                last_response.is_some_and(|r| r.contains(response_contains))
            }
            TerminationCriteria::ScriptedEnding => false,
            TerminationCriteria::AnyOf(criteria) => {
                criteria.iter().any(|c| c.check(turn, last_response))
            }
        }
    }
}

/// The result of running a simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResult {
    /// Distilled run evidence (same shape as experiment runs).
    pub evidence: RunEvidence,

    /// The full conversation transcript.
    pub transcript: Vec<ChatMessage>,

    /// How many user turns the simulation ran (opening message excluded).
    pub turn_count: usize,

    /// Why the simulation terminated.
    pub termination: TerminationCause,

    /// The complete journal snapshot (events, artifacts, head hash).
    pub journal: JournalSnapshot,
}

/// Why a simulation terminated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationCause {
    /// Reached the maximum turn limit.
    MaxTurns,
    /// Goal was satisfied.
    GoalSatisfied,
    /// Scripted messages exhausted.
    ScriptedEnding,
    /// Simulation was cancelled (e.g., by a behavior rule).
    Cancelled,
    /// The run failed with an error.
    RunFailed { error: String },
}

/// A simulating model wrapper that injects scripted user messages via the
/// inbox after each assistant response.
struct SimulatingModel {
    inner: Arc<dyn ChatModel>,
    script: Mutex<VecDeque<String>>,
    turn: AtomicUsize,
    max_turns: usize,
    inbox: Inbox,
}

impl SimulatingModel {
    fn new(inner: Arc<dyn ChatModel>, script: Vec<String>, max_turns: usize, inbox: Inbox) -> Self {
        Self {
            inner,
            script: Mutex::new(script.into()),
            turn: AtomicUsize::new(0),
            max_turns,
            inbox,
        }
    }
}

#[async_trait]
impl ChatModel for SimulatingModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> RuntimeResult<ChatResponse> {
        let turn = self.turn.fetch_add(1, Ordering::SeqCst) + 1;

        let response = self.inner.chat(messages, tools).await?;

        if turn >= self.max_turns {
            return Ok(response);
        }

        let next_msg = {
            let mut script = self.script.lock().unwrap();
            script.pop_front()
        };

        if let Some(msg) = next_msg {
            self.inbox
                .followup(json!(msg))
                .map_err(|e| RustyError::Graph(format!("simulation follow-up failed: {e}")))?;
        }

        Ok(response)
    }
}

/// Run a scripted simulation scenario against an agent.
///
/// `prepare` receives the wrapped model and journal, and returns the graph,
/// state spec, and initial state for the run.
pub async fn run_simulation<F>(
    scenario: &SimulationScenario,
    inner_model: Arc<dyn ChatModel>,
    inbox: Inbox,
    prepare: F,
) -> Result<SimulationResult>
where
    F: FnOnce(Arc<dyn ChatModel>, &Journal) -> RuntimeResult<(Graph, StateSpec, State)>,
{
    let journal = Journal::new("sim", "sim-thread", Clock::System);

    let sim_model = Arc::new(SimulatingModel::new(
        inner_model,
        scenario.script.clone(),
        scenario.max_turns,
        inbox.clone(),
    ));

    let (graph, spec, initial_state) = prepare(sim_model, &journal)
        .map_err(|e| EvalError::AgentBuild(format!("simulation prepare failed: {e}")))?;

    let executor = Executor::new();
    let started = std::time::Instant::now();

    let outcome = executor
        .run(
            &graph,
            &spec,
            initial_state,
            RunConfig::new("sim-thread")
                .with_journal(journal.clone())
                .with_inbox(inbox),
        )
        .await;

    let latency_ms = started.elapsed().as_millis() as u64;

    let (status, final_state, termination) = match outcome {
        Ok(ExecutionOutcome::Done(state)) => {
            let state_value = state.to_value();
            let transcript = extract_transcript(&state_value);
            let turn_count = count_turns(&transcript);
            let last_response = last_assistant_response(&transcript);

            let termination = if scenario
                .termination
                .check(turn_count, last_response.as_deref())
            {
                TerminationCause::GoalSatisfied
            } else {
                TerminationCause::ScriptedEnding
            };

            (RunStatus::Done, state_value, termination)
        }
        Ok(ExecutionOutcome::Interrupted { state, .. }) => {
            let state_value = state.to_value();
            (
                RunStatus::Interrupted,
                state_value,
                TerminationCause::Cancelled,
            )
        }
        Err(error) => {
            let msg = error.to_string();
            (
                RunStatus::Failed { error: msg.clone() },
                Value::Null,
                TerminationCause::RunFailed { error: msg },
            )
        }
    };

    let evidence = RunEvidence::from_journal(&journal, status, final_state.clone(), latency_ms);
    let transcript = extract_transcript(&final_state);
    let turn_count = count_turns(&transcript);

    Ok(SimulationResult {
        evidence,
        transcript,
        turn_count,
        termination,
        journal: journal.snapshot(),
    })
}

fn extract_transcript(state_value: &Value) -> Vec<ChatMessage> {
    state_value
        .get("messages")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn count_turns(transcript: &[ChatMessage]) -> usize {
    transcript
        .iter()
        .filter(|m| m.role == Role::User)
        .count()
        .saturating_sub(1)
}

fn last_assistant_response(transcript: &[ChatMessage]) -> Option<String> {
    transcript
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.has_tool_calls())
        .and_then(|m| m.content.clone())
}

/// A tool that steers the run from inside the tools node — a mid-turn arrival
/// with deterministic timing. Used in steering scenarios (AC 4).
pub struct SteeringTool {
    inbox: Inbox,
    content: String,
}

impl SteeringTool {
    /// Build a tool that sends `content` to the inbox when called.
    pub fn new(inbox: Inbox, content: impl Into<String>) -> Self {
        Self {
            inbox,
            content: content.into(),
        }
    }
}

#[async_trait]
impl rusty_agent_runtime::tool::Tool for SteeringTool {
    fn name(&self) -> &str {
        "steer"
    }

    fn description(&self) -> &str {
        "Steers the run with a mid-turn message."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }

    async fn call(&self, _args: Value) -> RuntimeResult<Value> {
        self.inbox
            .steer(json!(&self.content))
            .map_err(|e| RustyError::Graph(format!("steering tool failed: {e}")))?;
        Ok(json!(null))
    }
}
