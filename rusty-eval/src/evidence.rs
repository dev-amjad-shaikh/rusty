//! What a run left behind: the evidence assertions evaluate against.
//!
//! [`RunEvidence`] is a distilled, self-contained view of one run — the
//! ordered tool-call trajectory, the final state, wall latency, total cost,
//! and token usage — extracted from the run's Flight Recorder journal
//! ([`rusty_agent_runtime::journal::Journal`]). It is deliberately plain
//! data: serializable, clonable, and free of runtime handles, so it can
//! travel into reports, judges, and tests fabricated by hand.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rusty_agent_runtime::journal::Journal;
use rusty_agent_runtime::record::{EventStatus, RunEventKind};

/// How a run terminated, from the evaluator's point of view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunStatus {
    /// Routing reached the end; `final_state` is the terminal state.
    Done,
    /// A node interrupted (human-in-the-loop). `final_state` is the
    /// suspension-point state. An interrupted run has not finished its work;
    /// case runs count it as not passed.
    Interrupted,
    /// The executor returned an error. `final_state` is null — whatever
    /// partial state existed is not trustworthy evidence.
    Failed {
        /// The executor's error message.
        error: String,
    },
}

impl RunStatus {
    /// `true` only for [`RunStatus::Done`].
    pub fn is_done(&self) -> bool {
        matches!(self, RunStatus::Done)
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Done => f.write_str("done"),
            RunStatus::Interrupted => f.write_str("interrupted"),
            RunStatus::Failed { error } => write!(f, "failed: {error}"),
        }
    }
}

/// One journaled tool call, in trajectory order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Journal sequence number — the run's total order.
    pub seq: u64,

    /// Tool name (the `tool` field of the canonical tool-call request
    /// payload; falls back to the recording node's id).
    pub name: String,

    /// The call's arguments (`Null` when the journaled payload carried none).
    pub arguments: Value,

    /// Measured latency of the call in milliseconds, when journaled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,

    /// Journaled cost of the call in USD, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,

    /// How the call ended.
    pub status: EventStatus,
}

/// The distilled evidence of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvidence {
    /// How the run terminated.
    pub status: RunStatus,

    /// The ordered tool-call trajectory (journal `tool_call` events in
    /// sequence order).
    pub tool_calls: Vec<ToolCallRecord>,

    /// The run's final (or suspension-point) state as JSON. `Null` for
    /// failed runs.
    pub final_state: Value,

    /// Wall latency of the whole run in milliseconds, measured by the
    /// experiment runner around `Executor::run`.
    pub latency_ms: u64,

    /// Total journaled cost in USD: the sum of `cost_usd` over every event
    /// in the journal (model calls carry cost too, not only tool calls).
    /// Events without cost contribute zero.
    pub cost_usd: f64,

    /// Total tokens reported by model calls in the run.
    pub total_tokens: u64,
}

impl RunEvidence {
    /// Distill a journal plus run outcome into evidence.
    ///
    /// Tool calls are read from `tool_call` journal events in `seq` order.
    /// The canonical payload shape is `{"tool": name, "arguments": {...}}`
    /// (what the runtime's recording wrappers journal); events whose payload
    /// lacks a `tool` field fall back to the recording node's id, then to
    /// `"unknown"` — every journaled call stays visible in the trajectory.
    pub fn from_journal(
        journal: &Journal,
        status: RunStatus,
        final_state: Value,
        latency_ms: u64,
    ) -> Self {
        let events = journal.events();
        let mut tool_calls = Vec::new();
        let mut cost_usd = 0.0_f64;
        let mut total_tokens = 0_u64;

        for event in &events {
            if let Some(cost) = event.cost_usd {
                cost_usd += cost;
            }
            if let Some(usage) = &event.tokens {
                total_tokens = total_tokens.saturating_add(usage.total_tokens);
            }
            if event.kind != RunEventKind::ToolCall {
                continue;
            }
            let payload = event
                .input
                .as_ref()
                .and_then(|reference| journal.resolve(reference));
            let (name, arguments) = match payload {
                Some(value) => {
                    let name = value
                        .get("tool")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| event.node_id.clone())
                        .unwrap_or_else(|| "unknown".to_owned());
                    let arguments = value.get("arguments").cloned().unwrap_or(Value::Null);
                    (name, arguments)
                }
                None => (
                    event
                        .node_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_owned()),
                    Value::Null,
                ),
            };
            tool_calls.push(ToolCallRecord {
                seq: event.seq,
                name,
                arguments,
                latency_ms: event.latency_ms,
                cost_usd: event.cost_usd,
                status: event.status,
            });
        }

        Self {
            status,
            tool_calls,
            final_state,
            latency_ms,
            cost_usd,
            total_tokens,
        }
    }

    /// The trajectory as bare tool names, in order.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tool_calls
            .iter()
            .map(|call| call.name.as_str())
            .collect()
    }
}
