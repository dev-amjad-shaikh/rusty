//! Assignments: durable outcomes a person delegates to an agent and comes
//! back to. An assignment is a goal definition (the core [`Goal`]: title,
//! description, phase, revision, round budget) plus what makes it *this*
//! authorized piece of work: the tenant and owner, the agent entrusted, the
//! request verbatim, what success looks like, constraints, a deadline, the
//! agent's own working record (done, unresolved, next step, waiting for),
//! every round it ran (a bounded run each, with the actions it took and what
//! they cost), the owner's steers, and what wakes it next.
//!
//! Each round is an ordinary run — journaled, verified, in Observe — on a
//! fresh thread, with the record injected as a checkpoint ahead of the
//! agent's charter. The agent keeps its record through `assignment.progress`
//! (its own claim, kept apart from the facts: actions come from the journal).
//! When a round ends the driver decides: another round while the agent has a
//! next step and rounds remain; wait when it needs a decision, an answer or
//! an approval; done when it says so; blocked on an error, for a person.
//! A restart closes the round that was running from its persisted journal
//! and continues. Nothing here is a second reasoning loop: ReAct owns a
//! round; this only decides whether there is another.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use axum::extract::{Path, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::goals::{Goal, GoalPhase, GoalProvenance};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::runs::{MultitaskStrategy, RunPayload};
use crate::threads::ThreadRecord;

pub const DEFAULT_ROUNDS: u64 = 3;
/// The most tokens one round may spend when neither the delegation nor the
/// agent names a bound: enough for a real investigation, not enough to read
/// the same records all afternoon — a round that spends it ends with its
/// record, and the next round continues from there.
pub const DEFAULT_ROUND_TOKENS: u64 = 150_000;

fn default_round_tokens() -> u64 {
    DEFAULT_ROUND_TOKENS
}
const SUMMARY_CHARS: usize = 400;
/// How much of one tool result the record keeps — enough for the next
/// round to know what was seen, not the whole payload.
const RESULT_CHARS: usize = 240;
/// How many of the latest actions the checkpoint shows with their results.
const RECENT_ACTIONS: usize = 10;
/// The instruction the checkpoint ends with — the one promise the agent
/// makes to its owner every round.
pub const PROGRESS_INSTRUCTION: &str = "Record progress with assignment.progress as soon as you have a first finding (with its numbers), and again before you finish: what is done, what is unresolved, the next step; set waiting_for when you need something from a person (a decision, an answer) and complete=true when the request is fully achieved. A round has a token budget and ends when it is spent — what you have not recorded is lost to the next round.";

/// One bounded run of the assignment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Round {
    pub round: u64,
    pub run_id: String,
    pub thread_id: String,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// running | success | error | interrupted | cancelled | restart
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_run_id: Option<String>,
    /// The closing turn, when the round ended with an answer and no record:
    /// one more run on the same thread that asks the agent to record now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closing_run_id: Option<String>,
    /// The tool calls the round made, from its journal: `{tool, effect,
    /// status, connection?}`.
    #[serde(default)]
    pub actions: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The agent's working record — its claim, recorded by `assignment.progress`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub unresolved: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_run: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Steer {
    pub at: DateTime<Utc>,
    pub by: Value,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub assignment_id: String,
    pub tenant: String,
    /// The goal definition, the core plane's record.
    pub goal: Goal,
    pub owner: Value,
    pub assistant_id: String,
    pub assistant_name: String,
    /// The person's words, verbatim.
    pub request: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    pub max_rounds: u64,
    /// The most tokens one round may spend before it ends with its record.
    #[serde(default = "default_round_tokens")]
    pub max_tokens_per_round: u64,
    /// The world its rounds run in, when one was named at delegation:
    /// every round's calls to that system are answered there, and the
    /// world keeps what earlier rounds did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_name: Option<String>,
    /// Every world the rounds run in when the agent touches several
    /// systems (ids, `world` first) and their names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worlds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub world_names: Vec<String>,
    /// Words a person gave it — `campaign:F5` makes it a campaign variant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Tools the agent must not call in this work: stated once at
    /// delegation, held by the platform every round — an attempt is
    /// refused and journaled, so the record shows the constraint tested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_tools: Vec<String>,
    /// Where in a chain of agent-started work this sits, when an agent
    /// delegated it from a run: `{depth, run_id, agent_id}`. Its rounds
    /// carry it, so work they start counts the depth on. None when a
    /// person delegated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<Value>,
    /// working | waiting | blocked | paused | done | cancelled
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub progress: Progress,
    #[serde(default)]
    pub rounds: Vec<Round>,
    #[serde(default)]
    pub steers: Vec<Steer>,
    /// `{kind: continue | approval | person | none, run_id?}`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_wake: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Assignment {
    fn running_round(&self) -> Option<&Round> {
        self.rounds.iter().find(|r| r.status == "running")
    }
    fn rounds_used(&self) -> u64 {
        self.rounds.iter().filter(|r| r.status != "restart").count() as u64
    }
    fn set_state(&mut self, state: &str, reason: Option<String>, now: DateTime<Utc>) {
        self.state = state.to_owned();
        self.state_reason = reason;
        self.updated_at = now;
        // The goal definition follows: active while work goes on, paused,
        // blocked for a person, complete when done or cancelled.
        let phase = match state {
            "working" | "waiting" => GoalPhase::Active,
            "paused" => GoalPhase::Paused,
            "blocked" => GoalPhase::Blocked,
            _ => GoalPhase::Complete,
        };
        if self.goal.phase != phase {
            if let Ok(goal) = self
                .goal
                .transition(phase, format!("assignment {state}"), now)
            {
                self.goal = goal;
            }
        }
    }
}

/// What a round's end says to the driver.
#[derive(Debug, Clone)]
pub struct Wake {
    pub assignment_id: String,
    pub run_id: String,
    pub terminal: Value,
    /// The paused run this one resumed, when it did.
    pub approval_of: Option<String>,
    /// A closing turn, not a round of its own.
    pub closing: bool,
}

/// The plane: the wake channel from the run path to the driver, and the
/// index of running rounds by run id (for the progress door).
pub struct AssignmentPlane {
    wake: mpsc::UnboundedSender<Wake>,
    inbox: Mutex<Option<mpsc::UnboundedReceiver<Wake>>>,
    by_run: RwLock<HashMap<String, String>>,
}

impl std::fmt::Debug for AssignmentPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssignmentPlane").finish()
    }
}

impl AssignmentPlane {
    pub fn new() -> Arc<Self> {
        let (wake, rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            wake,
            inbox: Mutex::new(Some(rx)),
            by_run: RwLock::new(HashMap::new()),
        })
    }
    pub fn bind(&self, run_id: &str, assignment_id: &str) {
        if let Ok(mut b) = self.by_run.write() {
            b.insert(run_id.to_owned(), assignment_id.to_owned());
        }
    }
    pub fn assignment_of(&self, run_id: &str) -> Option<String> {
        self.by_run.read().ok()?.get(run_id).cloned()
    }
    /// Called from the run path as a run reaches its terminal state.
    pub fn note_run_end(&self, metadata: Option<&Value>, run_id: &str, terminal: &Value) {
        let Some(assignment_id) = metadata
            .and_then(|m| m.get("assignment_id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let approval_of = metadata
            .and_then(|m| m.get("approval_of"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let closing = metadata
            .and_then(|m| m.get("closing"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let _ = self.wake.send(Wake {
            assignment_id: assignment_id.to_owned(),
            run_id: run_id.to_owned(),
            terminal: terminal.clone(),
            approval_of,
            closing,
        });
    }
}

fn brief(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let cut: String = text.chars().take(chars).collect();
    format!("{}…", cut.trim_end())
}

/// The checkpoint a round starts with: the record, ahead of the charter.
pub fn checkpoint(a: &Assignment, round: u64) -> String {
    let list = |items: &[String], empty: &str| -> String {
        if items.is_empty() {
            format!("  {empty}")
        } else {
            items
                .iter()
                .map(|i| format!("  - {i}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let all: Vec<(u64, &Value)> = a
        .rounds
        .iter()
        .flat_map(|r| r.actions.iter().map(move |x| (r.round, x)))
        .collect();
    let skipped = all.len().saturating_sub(RECENT_ACTIONS);
    let mut actions: Vec<String> = all
        .iter()
        .skip(skipped)
        .map(|(round, x)| {
            let tool = x.get("tool").and_then(Value::as_str).unwrap_or("?");
            let status = x.get("status").and_then(Value::as_str).unwrap_or("?");
            let via = x
                .pointer("/connection/name")
                .and_then(Value::as_str)
                .map(|n| format!(" via {n}"))
                .unwrap_or_default();
            let args = x
                .get("arguments")
                .and_then(Value::as_str)
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            let result = x
                .get("result")
                .and_then(Value::as_str)
                .map(|s| format!(" → {s}"))
                .unwrap_or_default();
            format!("round {round}: {tool}{via}{args} — {status}{result}")
        })
        .collect();
    if skipped > 0 {
        actions.insert(0, format!("({skipped} earlier actions not shown)"));
    }
    let last_round_note = a
        .rounds
        .iter()
        .rev()
        .find(|r| r.status != "running")
        .filter(|r| {
            r.status != "success" || a.progress.by_run.as_deref() != Some(r.run_id.as_str())
        })
        .map(|r| {
            format!(
                "\nYour previous round ({}) ended {}{} — record progress early this time.",
                r.round,
                if r.status == "success" {
                    "without recording progress".to_owned()
                } else {
                    format!("in {}", r.status)
                },
                r.error
                    .as_deref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default();
    let steers: Vec<String> = a
        .steers
        .iter()
        .rev()
        .take(3)
        .map(|s| s.text.clone())
        .collect();
    let last_words = a
        .rounds
        .iter()
        .rev()
        .find(|r| r.status != "running")
        .and_then(|r| {
            r.summary
                .as_deref()
                .map(|s| format!("\nWhat you said at the end of round {}: {}", r.round, s))
        })
        .unwrap_or_default();
    format!(
        "ASSIGNMENT {} — round {round}; {} of {} counted rounds remain after this one. This is durable work a person delegated to you; each round is one bounded run of at most {} tokens, and the record below is what is known so far (the actions are facts from the journal; the rest is your own record).\n\
Request (verbatim): \"{}\"\n\
Success looks like: {}\n\
Constraints: {}{}\n\
Done so far:\n{}\n\
Unresolved:\n{}\n\
External actions already taken (do not repeat them):\n{}\n\
Next step you noted: {}\n\
Steer from the owner: {}{}{}\n\
{}",
        a.assignment_id,
        a.max_rounds.saturating_sub(a.rounds_used() + 1),
        a.max_rounds,
        a.max_tokens_per_round,
        a.request.replace('"', "'"),
        a.success
            .as_deref()
            .unwrap_or("not specified — use your judgment and say what you took it to mean"),
        a.constraints.as_deref().unwrap_or("none stated"),
        if a.forbidden_tools.is_empty() {
            String::new()
        } else {
            format!(
                " Tools you must not call, and the platform refuses: {}.",
                a.forbidden_tools.join(", ")
            )
        },
        list(&a.progress.done, "nothing yet"),
        list(&a.progress.unresolved, "nothing recorded"),
        list(&actions, "none"),
        a.progress
            .next_step
            .as_deref()
            .unwrap_or("none — start from the request"),
        if steers.is_empty() {
            "none".to_owned()
        } else {
            steers.join(" | ")
        },
        last_words,
        last_round_note,
        PROGRESS_INSTRUCTION,
    )
}

/// The tool calls one run made, from its journal, each with the connection
/// it went through: `{tool, effect, status, connection?}`.
async fn run_actions(state: &AppState, tenant: &TenantContext, run_id: &str) -> Vec<Value> {
    let Ok(evidence) = crate::routes::run_evidence(state, tenant, run_id).await else {
        return Vec::new();
    };
    let Some(snapshot) = evidence.journal else {
        return Vec::new();
    };
    let payload = |reference: &rusty_agent_runtime::record::PayloadRef| -> Option<Value> {
        match reference {
            rusty_agent_runtime::record::PayloadRef::Inline(value) => Some(value.clone()),
            rusty_agent_runtime::record::PayloadRef::Artifact(artifact) => {
                snapshot.artifacts.get(&artifact.sha256).cloned()
            }
        }
    };
    snapshot
        .events
        .iter()
        .filter(|e| e.kind == rusty_agent_runtime::record::RunEventKind::ToolCall)
        .filter_map(|e| {
            let input = e.input.as_ref().and_then(&payload)?;
            let tool = input.get("tool")?.as_str()?.to_owned();
            let connection = state.connection_tools.as_ref().and_then(|cell| cell.binding_at(&tool, e.recorded_at)).map(|b| json!({"instance_id": b.instance_id, "name": b.connection}));
            let arguments = input.get("arguments").map(|a| brief(&a.to_string(), 160));
            let output = e.output.as_ref().and_then(&payload);
            let failure = output.as_ref().and_then(|o| o.get("error")).and_then(Value::as_str).and_then(rusty_agent_runtime::tool::ToolFailure::parse);
            let result = match &failure {
                Some(f) => Some(brief(&format!("[{}] {} — next: {}", f.class, f.detail, f.next), RESULT_CHARS)),
                None => output.map(|o| brief(&o.to_string(), RESULT_CHARS)),
            };
            let mut action = json!({"tool": tool, "effect": e.effect, "status": e.status, "arguments": arguments, "result": result, "failure": failure.as_ref().map(|f| f.class.clone())});
            if let Some(c) = connection {
                action["connection"] = c;
            }
            Some(action)
        })
        .collect()
}

fn last_assistant_text(terminal: &Value) -> Option<String> {
    terminal
        .pointer("/output/messages")
        .and_then(Value::as_array)?
        .iter()
        .rev()
        .find(|m| {
            m.get("role").and_then(Value::as_str) == Some("assistant")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|c| !c.trim().is_empty())
        })
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .map(|c| brief(c.trim(), SUMMARY_CHARS))
}

async fn load(state: &AppState, tenant: &str, id: &str) -> Result<Assignment, ApiError> {
    state
        .server_store
        .get_assignment(id)
        .await
        .map_err(ApiError::internal)?
        .filter(|a| a.tenant == tenant)
        .ok_or_else(|| ApiError::not_found(format!("unknown assignment `{id}`")))
}

async fn keep(state: &AppState, a: &Assignment) {
    if let Err(error) = state.server_store.put_assignment(a).await {
        tracing::warn!(%error, assignment = %a.assignment_id, "assignment not kept");
    }
    tell_owner(state, a).await;
}

/// A state the owner should hear about is told once, as a notice apart
/// from the record: the record says what was done, the notice that the
/// owner was told — and, later, that they saw it.
async fn tell_owner(state: &AppState, a: &Assignment) {
    let (title, text) = match a.state.as_str() {
        "done" => (
            "Done",
            a.rounds
                .iter()
                .rev()
                .find_map(|r| r.summary.clone())
                .unwrap_or_else(|| "the request is achieved".to_owned()),
        ),
        "waiting" => (
            "Needs you",
            a.progress
                .waiting_for
                .clone()
                .or_else(|| a.state_reason.clone())
                .unwrap_or_else(|| "the work waits for you".to_owned()),
        ),
        "blocked" => (
            "Blocked",
            a.state_reason
                .clone()
                .unwrap_or_else(|| "the work cannot go on by itself".to_owned()),
        ),
        "cancelled" => (
            "Cancelled",
            a.state_reason
                .clone()
                .unwrap_or_else(|| "the work was cancelled".to_owned()),
        ),
        _ => return,
    };
    let key = format!(
        "assignment:{}:{}:{}",
        a.assignment_id,
        a.state,
        a.updated_at.to_rfc3339()
    );
    let about = json!({"kind": "assignment", "assignment_id": a.assignment_id, "state": a.state});
    let title = format!("{title}: {}", brief(&a.request, 72));
    let text: String = text.chars().take(600).collect();
    if crate::notices::tell(state, &a.tenant, &a.owner, &key, about, &title, &text)
        .await
        .is_none()
    {
        return;
    }
    tell_delegating_agent(state, a, &text).await;
}

/// An agent that delegated from a run has no inbox, and the run that
/// delegated is long over when the work ends. It is told through its own
/// memory: one note under the assignment's key, so a later state
/// supersedes the earlier, recalled at the start of its next run and by
/// the words that name the work. The entrusted agent is the author: the
/// finding is its claim, the delegating agent's to check.
async fn tell_delegating_agent(state: &AppState, a: &Assignment, said: &str) {
    use rusty_agent_runtime::memory::{
        MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord, MemoryScope, ProvenanceAuthor,
        ScopeAddress, ValidityWindow,
    };
    if a.owner.get("kind").and_then(Value::as_str) != Some("agent") {
        return;
    }
    let Some(agent) = a.owner.get("agent_id").and_then(Value::as_str) else {
        return;
    };
    let what = match a.state.as_str() {
        "done" => format!("is done — it reported: {said}"),
        "waiting" => format!("waits for a person: {said}"),
        "blocked" => format!("is blocked: {said}"),
        "cancelled" => format!("was cancelled: {said}"),
        _ => return,
    };
    let sentence = format!(
        "The work you delegated to {} (assignment {}: \"{}\") {what}",
        a.assistant_name,
        a.assignment_id,
        brief(&a.request, 120)
    );
    if rusty_agent_runtime::memory::looks_like_secret(&sentence).is_some() {
        return;
    }
    let now = Utc::now();
    let scope = ScopeAddress::new(MemoryScope::Agent, agent);
    let key = format!("assignment:{}", a.assignment_id);
    let content = json!({"text": sentence, "assignment_id": a.assignment_id, "state": a.state});
    let record = MemoryRecord::new(
        MemoryKind::Fact,
        scope.clone(),
        MemoryProvenance {
            author: ProvenanceAuthor::Agent {
                agent_id: a.assistant_id.clone(),
            },
            evidence: Default::default(),
            written_at: now,
        },
        0.8,
        ValidityWindow {
            valid_from: now,
            valid_until: None,
        },
        now,
        content.clone(),
    );
    let Ok(mut record) = record else { return };
    // The newest note under the key is the one recalled; the earlier state
    // stays readable by id, as a person's own restatement would.
    let earlier = state
        .server_store
        .query_memory(
            &a.tenant,
            &MemoryQuery {
                scope: Some(scope),
                key: Some(key.clone()),
                ..Default::default()
            },
            now,
        )
        .await
        .ok()
        .and_then(|live| {
            live.into_iter().max_by(|x, y| {
                x.created_at
                    .cmp(&y.created_at)
                    .then_with(|| x.memory_id.cmp(&y.memory_id))
            })
        });
    if let Some(prev) = earlier {
        record = record.with_supersedes(prev.memory_id);
    }
    let name = a.assistant_name.trim().to_lowercase();
    let mut tags = vec![
        "assignment".to_owned(),
        "trigger:delegated".to_owned(),
        "trigger:assignment".to_owned(),
    ];
    if !name.is_empty() {
        tags.push(format!("trigger:{name}"));
    }
    let record = record.with_key(key).with_priority(7).with_tags(tags);
    match state
        .server_store
        .put_memory(&a.tenant, &record, &content)
        .await
    {
        Ok(_) => {
            tracing::info!(assignment = %a.assignment_id, %agent, state = %a.state, "the delegating agent was told through its memory")
        }
        Err(error) => {
            tracing::warn!(assignment = %a.assignment_id, %error, "the delegating agent was not told")
        }
    }
}

/// The loop's own reason when it stopped a run for making no progress — a
/// call repeated with the same arguments, or reads that answered nothing
/// new — read out of the run's terminal message, which wraps it in the
/// node and super-step that raised it.
fn no_progress_reason(error: &str) -> Option<&str> {
    error.find("no progress:").map(|at| &error[at..])
}

/// Start the next round: a fresh thread, the checkpoint ahead of the
/// charter, the request (or "continue") as the message, the run scheduled.
async fn start_round(
    state: &Arc<AppState>,
    a: &mut Assignment,
    steer: Option<String>,
) -> Result<(), String> {
    let now = Utc::now();
    if let Some(deadline) = a.deadline {
        if now > deadline {
            a.set_state(
                "blocked",
                Some(format!("the deadline passed at {}", deadline.to_rfc3339())),
                now,
            );
            return Err("the deadline passed".to_owned());
        }
    }
    // The chain's token budget: an assignment an agent started sits in a
    // chain with a cap; spent, the next round waits for a person.
    if let Some(root) = a
        .chain
        .as_ref()
        .and_then(|c| c.get("root_run_id"))
        .and_then(Value::as_str)
    {
        let cap = a
            .chain
            .as_ref()
            .and_then(|c| c.get("max_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(crate::chain_spend::CHAIN_MAX_TOKENS_DEFAULT);
        let spend = crate::chain_spend::read(state.server_store.as_ref(), root).await;
        if crate::chain_spend::spent(&spend, cap) {
            let why = crate::chain_spend::words(&spend, cap);
            a.set_state("waiting", Some(why.clone()), now);
            a.next_wake = Some(json!({"kind": "person"}));
            return Err(why);
        }
    }
    // Rounds are numbered in order; the budget counts the ones that ended
    // by themselves (a round a restart cut short is not charged).
    let round = a.rounds.len() as u64 + 1;
    let internal_assistant = crate::auth::scope_id(&a.tenant, &a.assistant_id);
    let assistant = state
        .server_store
        .get_assistant(&internal_assistant)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("the agent `{}` is gone", a.assistant_id))?;
    let thread_id = uuid::Uuid::new_v4().to_string();
    let internal_thread_id = crate::auth::scope_id(&a.tenant, &thread_id);
    let record = ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: a.tenant.clone(),
        graph: assistant.graph.clone(),
        metadata: json!({"trigger": "assignment", "assignment_id": a.assignment_id, "round": round, "assistant_id": a.assistant_id}),
        forked_from: None,
        seed_length: None,
        created_at: now,
    };
    state
        .server_store
        .create_thread(&internal_thread_id, &record)
        .await
        .map_err(|e| format!("thread could not be created: {e}"))?;
    let message = if round == 1 {
        a.request.clone()
    } else {
        match &steer {
            Some(s) => format!("Continue the assignment from your record. The owner says: {s}"),
            None => "Continue the assignment from your record.".to_owned(),
        }
    };
    let charter = crate::routes::assistant_instructions(&assistant.config).unwrap_or_default();
    let instructions = format!("{}\n\n{}", checkpoint(a, round), charter)
        .trim()
        .to_owned();
    let mut payload = RunPayload {
        input: Some(json!({"messages": [{"role": "user", "content": message}]})),
        metadata: Some(
            json!({"channel": "assignment", "assignment_id": a.assignment_id, "round": round, "on_behalf_of": a.owner, "created_by": a.owner, "chain": a.chain}),
        ),
        assistant_id: Some(a.assistant_id.clone()),
        ..RunPayload::default()
    };
    payload
        .config
        .get_or_insert_with(Default::default)
        .instructions = Some(instructions);
    crate::routes::apply_assistant_defaults(
        state,
        &a.tenant,
        &internal_thread_id,
        &assistant,
        &mut payload,
    )
    .await;
    if let Some(world) = &a.world {
        let config = payload.config.get_or_insert_with(Default::default);
        config.world = Some(world.clone());
        if a.worlds.len() > 1 {
            config.worlds = Some(a.worlds.clone());
        }
    }
    hold_constraints(a, &mut payload);
    grant_progress_door(&mut payload);
    // The round's bound: the assignment's tokens per round, or the agent's
    // own budget when that is tighter. A round that spends it ends with
    // its record and the next round continues from there.
    let budget = payload
        .config
        .get_or_insert_with(Default::default)
        .budget
        .get_or_insert_with(Default::default);
    budget.max_tokens = Some(budget.max_tokens.map_or(a.max_tokens_per_round, |own| {
        own.min(a.max_tokens_per_round)
    }));
    let scheduled = crate::runs::schedule(
        &state.run_deps,
        &internal_thread_id,
        &thread_id,
        &assistant.graph,
        payload,
        MultitaskStrategy::Enqueue,
    )
    .await
    .map_err(|e| format!("the round was not accepted: {e:?}"))?;
    if let Some(plane) = &state.run_deps.assignments {
        plane.bind(&scheduled.run_id, &a.assignment_id);
    }
    a.rounds.push(Round {
        round,
        run_id: scheduled.run_id.clone(),
        thread_id,
        started_at: now,
        ended_at: None,
        status: "running".to_owned(),
        resumed_run_id: None,
        closing_run_id: None,
        actions: Vec::new(),
        summary: None,
        spend: None,
        steer,
        error: None,
    });
    a.set_state("working", None, now);
    a.next_wake = Some(json!({"kind": "round", "run_id": scheduled.run_id}));
    tracing::info!(assignment = %a.assignment_id, round, run = %scheduled.run_id, "assignment round started");
    Ok(())
}

/// An assignment round always carries the door the checkpoint promises,
/// whatever the agent's own allow-list says: a tool the agent cannot see is
/// a promise the platform broke (the desk said so, honestly, on the first
/// closing turn).
/// The tools the work must not call ride on every round's run, where a
/// guard refuses them; the world it runs in likewise.
fn hold_constraints(a: &Assignment, payload: &mut RunPayload) {
    if !a.forbidden_tools.is_empty() {
        payload
            .config
            .get_or_insert_with(Default::default)
            .forbidden_tools = Some(a.forbidden_tools.clone());
    }
}

fn grant_progress_door(payload: &mut RunPayload) {
    if let Some(list) = payload
        .config
        .as_mut()
        .and_then(|c| c.tool_allowlist.as_mut())
    {
        if !list
            .iter()
            .any(|t| t == crate::platform_tools::ASSIGNMENT_PROGRESS)
        {
            list.push(crate::platform_tools::ASSIGNMENT_PROGRESS.to_owned());
        }
    }
}

/// The one message a closing turn carries.
pub const CLOSING_TURN: &str = "Record your progress now: call assignment.progress with what is done (with the numbers), what is unresolved, and the next step — or complete=true if the request is fully achieved. Do nothing else.";

/// One more run on the round's own thread — the conversation continues
/// from the agent's answer — asking only for the record. Exactly one per
/// round; the wake it produces carries `closing`.
async fn closing_turn(state: &Arc<AppState>, a: &mut Assignment, idx: usize) -> Result<(), String> {
    let round = a.rounds[idx].clone();
    let internal_assistant = crate::auth::scope_id(&a.tenant, &a.assistant_id);
    let assistant = state
        .server_store
        .get_assistant(&internal_assistant)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("the agent `{}` is gone", a.assistant_id))?;
    let internal_thread_id = crate::auth::scope_id(&a.tenant, &round.thread_id);
    let mut payload = RunPayload {
        input: Some(json!({"messages": [{"role": "user", "content": CLOSING_TURN}]})),
        metadata: Some(
            json!({"channel": "assignment", "assignment_id": a.assignment_id, "round": round.round, "closing": true, "on_behalf_of": a.owner, "created_by": a.owner, "chain": a.chain}),
        ),
        assistant_id: Some(a.assistant_id.clone()),
        ..RunPayload::default()
    };
    crate::routes::apply_assistant_defaults(
        state,
        &a.tenant,
        &internal_thread_id,
        &assistant,
        &mut payload,
    )
    .await;
    hold_constraints(a, &mut payload);
    grant_progress_door(&mut payload);
    // The round's bound: the assignment's tokens per round, or the agent's
    // own budget when that is tighter. A round that spends it ends with
    // its record and the next round continues from there.
    let budget = payload
        .config
        .get_or_insert_with(Default::default)
        .budget
        .get_or_insert_with(Default::default);
    budget.max_tokens = Some(budget.max_tokens.map_or(a.max_tokens_per_round, |own| {
        own.min(a.max_tokens_per_round)
    }));
    let scheduled = crate::runs::schedule(
        &state.run_deps,
        &internal_thread_id,
        &round.thread_id,
        &assistant.graph,
        payload,
        MultitaskStrategy::Enqueue,
    )
    .await
    .map_err(|e| format!("the closing turn was not accepted: {e:?}"))?;
    if let Some(plane) = &state.run_deps.assignments {
        plane.bind(&scheduled.run_id, &a.assignment_id);
    }
    a.rounds[idx].closing_run_id = Some(scheduled.run_id.clone());
    a.rounds[idx].status = "running".to_owned();
    a.rounds[idx].ended_at = None;
    tracing::info!(assignment = %a.assignment_id, round = round.round, run = %scheduled.run_id, "assignment round: closing turn asked for the record");
    Ok(())
}

/// A round ended: close it from the evidence and decide what is next.
async fn close_round(state: &Arc<AppState>, wake: Wake) {
    let Ok(Some(mut a)) = state.server_store.get_assignment(&wake.assignment_id).await else {
        return;
    };
    let tenant = TenantContext::new(a.tenant.clone(), Vec::new());
    let now = Utc::now();
    let status = wake
        .terminal
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_owned();
    let approval_of = wake.approval_of.clone();
    let Some(idx) = a.rounds.iter().position(|r| {
        r.run_id == wake.run_id
            || r.resumed_run_id.as_deref() == Some(&wake.run_id)
            || r.closing_run_id.as_deref() == Some(&wake.run_id)
            || approval_of.as_deref() == Some(r.run_id.as_str())
    }) else {
        return;
    };
    let actions = run_actions(state, &tenant, &wake.run_id).await;
    {
        let round = &mut a.rounds[idx];
        if round.run_id != wake.run_id && !wake.closing {
            round.resumed_run_id = Some(wake.run_id.clone());
        }
        round.actions.extend(actions);
        if wake.closing {
            // The closing turn's spend adds to the round's; its words do not
            // replace the round's answer.
            let extra = wake
                .terminal
                .get("spend")
                .and_then(|s| s.get("tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(spend) = round.spend.as_mut() {
                if let Some(t) = spend.get("tokens").and_then(Value::as_u64) {
                    spend["tokens"] = json!(t + extra);
                }
            }
            round.status = if status == "success" {
                "success".to_owned()
            } else {
                status.clone()
            };
        } else {
            round.spend = wake.terminal.get("spend").cloned();
            round.summary = last_assistant_text(&wake.terminal).or(round.summary.clone());
            round.error = wake
                .terminal
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            round.status = status.clone();
        }
        if status != "interrupted" {
            round.ended_at = Some(now);
        }
    }
    let round_no = a.rounds[idx].round;
    match a.state.as_str() {
        "cancelled" | "done" => {
            a.updated_at = now;
            keep(state, &a).await;
            return;
        }
        _ => {}
    }
    match status.as_str() {
        "interrupted" => {
            a.set_state(
                "waiting",
                Some("paused before an irreversible action — decide it in the Inbox".to_owned()),
                now,
            );
            a.next_wake = Some(json!({"kind": "approval", "run_id": wake.run_id}));
        }
        "cancelled" => {
            a.set_state(
                "blocked",
                Some(format!("round {round_no} was cancelled")),
                now,
            );
            a.next_wake = Some(json!({"kind": "person"}));
        }
        "success" => {
            let recorded_this_round = [
                Some(a.rounds[idx].run_id.as_str()),
                Some(wake.run_id.as_str()),
                a.rounds[idx].closing_run_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|id| a.progress.by_run.as_deref() == Some(id));
            if !recorded_this_round
                && !wake.closing
                && a.rounds[idx].closing_run_id.is_none()
                && a.rounds[idx].summary.is_some()
                && a.state != "paused"
            {
                // An answer, no record: ask once, on the same thread.
                match closing_turn(state, &mut a, idx).await {
                    Ok(()) => {
                        a.updated_at = now;
                        keep(state, &a).await;
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%error, assignment = %a.assignment_id, "closing turn not started")
                    }
                }
            }
            if a.progress.complete {
                a.set_state(
                    "done",
                    Some("the agent reported the request achieved".to_owned()),
                    now,
                );
                a.next_wake = None;
            } else if let Some(waiting) = a
                .progress
                .waiting_for
                .clone()
                .filter(|_| recorded_this_round)
            {
                a.set_state("waiting", Some(format!("waiting for: {waiting}")), now);
                a.next_wake = Some(json!({"kind": "person"}));
            } else if !recorded_this_round && a.rounds[idx].summary.is_none() {
                a.set_state(
                    "blocked",
                    Some(format!(
                        "round {round_no} ended with neither a progress record nor an answer"
                    )),
                    now,
                );
                a.next_wake = Some(json!({"kind": "person"}));
            } else if !recorded_this_round
                && (a.rounds_used() >= a.max_rounds || a.state == "paused")
            {
                a.rounds[idx].error = Some(
                    "ended without a progress record; what it said carries to the next round"
                        .to_owned(),
                );
                a.set_state("waiting", Some(format!("round {round_no} ended with an answer but no progress record, and its {} rounds are spent — read what it said, then grant a round or mark it done", a.max_rounds)), now);
                a.next_wake = Some(json!({"kind": "person"}));
            } else if !recorded_this_round {
                // The answer carries as the round's words; the next round is
                // told to record. The rounds bound how long this can go on.
                a.rounds[idx].error = Some(
                    "ended without a progress record; what it said carries to the next round"
                        .to_owned(),
                );
                if let Err(error) = start_round(state, &mut a, None).await {
                    a.set_state("blocked", Some(error), now);
                    a.next_wake = Some(json!({"kind": "person"}));
                }
            } else if a.state == "paused" {
                a.next_wake = Some(json!({"kind": "person"}));
            } else if a.rounds_used() >= a.max_rounds {
                a.set_state(
                    "waiting",
                    Some(format!(
                        "its {} rounds are spent — continue to grant more",
                        a.max_rounds
                    )),
                    now,
                );
                a.next_wake = Some(json!({"kind": "person"}));
            } else {
                if let Err(error) = start_round(state, &mut a, None).await {
                    a.set_state("blocked", Some(error), now);
                    a.next_wake = Some(json!({"kind": "person"}));
                }
            }
        }
        _ => {
            let recorded_this_round = a.progress.by_run.as_deref()
                == Some(a.rounds[idx].run_id.as_str())
                || a.progress.by_run.as_deref() == Some(wake.run_id.as_str());
            let budget_cut = a.rounds[idx]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("budget"));
            let no_progress = a.rounds[idx]
                .error
                .as_deref()
                .and_then(no_progress_reason)
                .map(str::to_owned);
            if budget_cut
                && recorded_this_round
                && !a.progress.complete
                && a.progress.waiting_for.is_none()
                && a.rounds_used() < a.max_rounds
                && a.state != "paused"
            {
                // The round ran out of budget after recording: the record
                // carries; the next round continues from it.
                if let Err(error) = start_round(state, &mut a, None).await {
                    a.set_state("blocked", Some(error), now);
                    a.next_wake = Some(json!({"kind": "person"}));
                }
            } else if let Some(why) = no_progress.as_deref().filter(|_| {
                !a.progress.complete
                    && a.progress.waiting_for.is_none()
                    && a.rounds_used() < a.max_rounds
                    && a.state != "paused"
            }) {
                // The loop stopped the round because its reads answered
                // nothing new. The next round is told so, as a steer — the
                // one thing it must not do is read the same way again.
                let steer = format!(
                    "the previous round stopped — {why}. Do not read the same way again: change the query, the source or the tool, or record what you found and what you need."
                );
                if let Err(error) = start_round(state, &mut a, Some(steer)).await {
                    a.set_state("blocked", Some(error), now);
                    a.next_wake = Some(json!({"kind": "person"}));
                }
            } else if let Some(why) = no_progress.as_deref() {
                a.set_state(
                    "waiting",
                    Some(format!(
                        "round {round_no} stopped: {why} — steer it, grant a round, or mark it done"
                    )),
                    now,
                );
                a.next_wake = Some(json!({"kind": "person"}));
            } else {
                a.set_state(
                    "blocked",
                    Some(format!(
                        "round {round_no} failed: {}",
                        a.rounds[idx]
                            .error
                            .clone()
                            .unwrap_or_else(|| "the run errored".to_owned())
                    )),
                    now,
                );
                a.next_wake = Some(json!({"kind": "person"}));
            }
        }
    }
    a.updated_at = now;
    keep(state, &a).await;
}

/// The driver: wakes from the run path, one at a time, in order.
pub(crate) fn start_driver(state: Arc<AppState>) {
    let Some(plane) = state.run_deps.assignments.clone() else {
        return;
    };
    let Some(mut rx) = plane.inbox.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    tokio::spawn(async move {
        while let Some(wake) = rx.recv().await {
            close_round(&state, wake).await;
        }
    });
}

/// After a restart: a round that was running has no run any more. Close it
/// from its persisted journal and continue — the work was delegated.
pub(crate) async fn restore(state: &Arc<AppState>) {
    // Give the run manager's own restore a moment to bring back what it can.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let Ok(all) = state.server_store.list_assignments(None).await else {
        return;
    };
    for mut a in all.into_iter().filter(|a| a.state == "working") {
        let Some(running) = a.running_round().cloned() else {
            continue;
        };
        if state
            .run_deps
            .manager
            .info(&running.run_id)
            .await
            .is_some_and(|info| {
                !matches!(
                    info.status.as_str(),
                    "success" | "error" | "interrupted" | "cancelled" | "timeout"
                )
            })
        {
            continue;
        }
        let tenant = TenantContext::new(a.tenant.clone(), Vec::new());
        let now = Utc::now();
        let actions = run_actions(state, &tenant, &running.run_id).await;
        if let Some(round) = a.rounds.iter_mut().find(|r| r.run_id == running.run_id) {
            round.actions.extend(actions);
            round.status = "restart".to_owned();
            round.ended_at = Some(now);
            round.error = Some(
                "the server restarted while this round ran; what it did is in the journal"
                    .to_owned(),
            );
        }
        tracing::info!(assignment = %a.assignment_id, round = running.round, "assignment round interrupted by a restart; continuing");
        // A record that already says complete is the work done: the round
        // was cut short between recording it and answering, and a new
        // round would only read it all again.
        if a.progress.complete {
            a.set_state("done", Some("the agent recorded the request achieved; the round that recorded it was cut short by a restart before it answered".to_owned()), now);
            a.next_wake = None;
        } else if a.rounds_used() >= a.max_rounds {
            a.set_state(
                "waiting",
                Some(format!(
                    "its {} rounds are spent — continue to grant more",
                    a.max_rounds
                )),
                now,
            );
            a.next_wake = Some(json!({"kind": "person"}));
        } else if let Err(error) = start_round(state, &mut a, None).await {
            a.set_state("blocked", Some(error), now);
            a.next_wake = Some(json!({"kind": "person"}));
        }
        keep(state, &a).await;
    }
}

async fn served(state: &AppState, a: &Assignment) -> Value {
    let mut value = serde_json::to_value(a).unwrap_or(Value::Null);
    // Delivery, apart from the record: whether the owner was told, and saw.
    let notices = crate::notices::for_assignment(state, &a.tenant, &a.assignment_id).await;
    value["delivery"] = crate::notices::delivery(&notices);
    value["told"] = json!(crate::notices::told(&notices));
    // The chain's budget, when an agent started this: what the chain has
    // spent against the delegating agent's cap.
    if let Some(root) = a
        .chain
        .as_ref()
        .and_then(|c| c.get("root_run_id"))
        .and_then(Value::as_str)
    {
        let cap = a
            .chain
            .as_ref()
            .and_then(|c| c.get("max_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(crate::chain_spend::CHAIN_MAX_TOKENS_DEFAULT);
        let spend = crate::chain_spend::read(state.server_store.as_ref(), root).await;
        value["chain_spend"] = json!({"spent_tokens": spend.spent_tokens, "max_tokens": cap, "runs": spend.runs, "over": crate::chain_spend::spent(&spend, cap)});
    }
    // A round still marked running whose run is gone reads as interrupted.
    if let Some(rounds) = value.get_mut("rounds").and_then(Value::as_array_mut) {
        for (round, record) in rounds.iter_mut().zip(a.rounds.iter()) {
            if record.status == "running" {
                let live = state
                    .run_deps
                    .manager
                    .info(&record.run_id)
                    .await
                    .map(|i| i.status.as_str().to_owned());
                round["live"] = json!(live);
            }
        }
    }
    let spent: f64 = a
        .rounds
        .iter()
        .filter_map(|r| r.spend.as_ref())
        .filter_map(|s| s.get("cost_usd").and_then(Value::as_f64))
        .sum();
    // A run's spend says `tokens` as a number; older shapes nest it.
    let tokens: u64 = a
        .rounds
        .iter()
        .filter_map(|r| r.spend.as_ref())
        .filter_map(|s| {
            s.get("tokens")
                .and_then(Value::as_u64)
                .or_else(|| s.get("total_tokens").and_then(Value::as_u64))
                .or_else(|| s.pointer("/tokens/total_tokens").and_then(Value::as_u64))
        })
        .sum();
    value["spend_total"] = json!({"cost_usd": spent, "total_tokens": tokens});
    value["rounds_used"] = json!(a.rounds_used());
    value
}

#[derive(Debug, Deserialize)]
pub struct CreateAssignment {
    pub assistant_id: String,
    pub request: String,
    /// A world (by name or id) every round runs in.
    #[serde(default)]
    pub world: Option<String>,
    /// Several worlds, one per system the agent touches; `world` is the first.
    #[serde(default)]
    pub worlds: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tools of the agent's that this work must not call.
    #[serde(default)]
    pub forbidden_tools: Vec<String>,
    #[serde(default)]
    pub success: Option<String>,
    #[serde(default)]
    pub constraints: Option<String>,
    #[serde(default)]
    pub max_rounds: Option<u64>,
    #[serde(default)]
    pub max_tokens_per_round: Option<u64>,
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
}

pub(crate) async fn create_assignment(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(input): Json<CreateAssignment>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let a = create(&state, &tenant, input, tenant.attribution(), None).await?;
    Ok((StatusCode::CREATED, Json(served(&state, &a).await)))
}

/// Delegate: the assignment is made, kept, and its first round started.
/// `owner` is who delegated — a person from the route, an agent's run from
/// `assignment.create` — and `chain` where the work sits in a chain of
/// agent-started work, when an agent started it.
pub(crate) async fn create(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    input: CreateAssignment,
    owner: Value,
    chain: Option<Value>,
) -> Result<Assignment, ApiError> {
    let request = input.request.trim().to_owned();
    if request.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_request",
            "say what you want achieved — the request is kept verbatim".to_owned(),
        ));
    }
    let max_rounds = input.max_rounds.unwrap_or(DEFAULT_ROUNDS).clamp(1, 50);
    let max_tokens_per_round = input
        .max_tokens_per_round
        .unwrap_or(DEFAULT_ROUND_TOKENS)
        .clamp(5_000, 5_000_000);
    let internal_assistant = crate::auth::scope_id(tenant.tenant(), &input.assistant_id);
    let assistant = state
        .server_store
        .get_assistant(&internal_assistant)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("unknown agent `{}`", input.assistant_id)))?;
    let resolved = crate::worlds::resolve_worlds(
        state.as_ref(),
        tenant.tenant(),
        input.world.as_deref(),
        &input.worlds,
        "the delegation",
    )
    .await?;
    let world = resolved.first().cloned();
    let (worlds, world_names): (Vec<String>, Vec<String>) = if resolved.len() > 1 {
        resolved
            .iter()
            .map(|w| (w.world_id.clone(), w.name.clone()))
            .unzip()
    } else {
        (Vec::new(), Vec::new())
    };
    // A forbidden tool must be one the agent has: a name it does not
    // carry forbids nothing, and a typo would read as a constraint held.
    let carried: Vec<String> = assistant
        .config
        .pointer("/studio_intent/tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let forbidden_tools: Vec<String> = input
        .forbidden_tools
        .iter()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(unknown) = forbidden_tools.iter().find(|t| !carried.contains(t)) {
        return Err(ApiError::bad_request(format!(
            "agent `{}` has no tool `{unknown}` to forbid; it carries {}",
            assistant.name,
            if carried.is_empty() {
                "none".to_owned()
            } else {
                carried.join(", ")
            }
        )));
    }
    let now = Utc::now();
    let operator = owner
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("someone")
        .to_owned();
    let title = brief(&request, 80);
    let goal = Goal::new(
        title,
        request.clone(),
        GoalProvenance::Operator { operator },
        Some(max_rounds),
        now,
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let mut a = Assignment {
        assignment_id: uuid::Uuid::new_v4().to_string(),
        tenant: tenant.tenant().to_owned(),
        goal,
        owner,
        assistant_id: input.assistant_id.clone(),
        assistant_name: assistant.name.clone(),
        request,
        success: input
            .success
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        constraints: input
            .constraints
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        deadline: input.deadline,
        max_rounds,
        max_tokens_per_round,
        world: world.as_ref().map(|w| w.world_id.clone()),
        world_name: world.as_ref().map(|w| w.name.clone()),
        worlds,
        world_names,
        tags: input
            .tags
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect(),
        forbidden_tools,
        chain,
        state: "working".to_owned(),
        state_reason: None,
        progress: Progress::default(),
        rounds: Vec::new(),
        steers: Vec::new(),
        next_wake: None,
        created_at: now,
        updated_at: now,
    };
    if let Err(error) = start_round(state, &mut a, None).await {
        a.set_state("blocked", Some(error), now);
    }
    state
        .server_store
        .put_assignment(&a)
        .await
        .map_err(ApiError::internal)?;
    Ok(a)
}

pub(crate) async fn list_assignments(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let mut all = state
        .server_store
        .list_assignments(Some(tenant.tenant()))
        .await
        .map_err(ApiError::internal)?;
    all.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    let mut out = Vec::with_capacity(all.len());
    for a in &all {
        out.push(served(&state, a).await);
    }
    Ok(Json(json!({"assignments": out})))
}

pub(crate) async fn get_assignment(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let a = load(&state, tenant.tenant(), &id).await?;
    Ok(Json(served(&state, &a).await))
}

#[derive(Debug, Deserialize, Default)]
pub struct ContinuePayload {
    #[serde(default)]
    pub steer: Option<String>,
}

/// Another round, now — with a steer when the owner has one. Grants a round
/// past the budget: the person's word is the budget.
pub(crate) async fn continue_assignment(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
    payload: Option<Json<ContinuePayload>>,
) -> Result<Json<Value>, ApiError> {
    let mut a = load(&state, tenant.tenant(), &id).await?;
    if matches!(a.state.as_str(), "done" | "cancelled") {
        return Err(ApiError::conflict(format!("assignment is {}", a.state)));
    }
    if a.running_round().is_some() {
        return Err(ApiError::conflict(
            "a round is running — it continues by itself when that round ends".to_owned(),
        ));
    }
    let now = Utc::now();
    let steer = payload
        .and_then(|p| p.0.steer)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Some(text) = &steer {
        a.steers.push(Steer {
            at: now,
            by: tenant.attribution(),
            text: text.clone(),
        });
    }
    if a.rounds_used() >= a.max_rounds {
        a.max_rounds = a.rounds_used() + 1;
    }
    if let Err(error) = start_round(&state, &mut a, steer).await {
        a.set_state("blocked", Some(error), now);
    }
    state
        .server_store
        .put_assignment(&a)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(served(&state, &a).await))
}

async fn set_state_route(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: &str,
    to: &str,
    reason: String,
) -> Result<Json<Value>, ApiError> {
    let mut a = load(state, tenant.tenant(), id).await?;
    if matches!(a.state.as_str(), "done" | "cancelled") {
        return Err(ApiError::conflict(format!(
            "assignment is already {}",
            a.state
        )));
    }
    let now = Utc::now();
    if to == "cancelled" {
        if let Some(round) = a.running_round().cloned() {
            let _ = crate::runs::cancel_run(&state.run_deps, &round.run_id).await;
        }
    }
    a.set_state(to, Some(reason), now);
    a.next_wake = if to == "paused" {
        Some(json!({"kind": "person"}))
    } else {
        None
    };
    state
        .server_store
        .put_assignment(&a)
        .await
        .map_err(ApiError::internal)?;
    // A person acting on it knows; an agent that delegated it does not.
    if a.owner.get("kind").and_then(Value::as_str) == Some("agent") {
        tell_owner(state, &a).await;
    }
    Ok(Json(served(state, &a).await))
}

pub(crate) async fn pause_assignment(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_state_route(
        &state,
        &tenant,
        &id,
        "paused",
        "paused by its owner — the running round finishes; no new round starts".to_owned(),
    )
    .await
}

pub(crate) async fn cancel_assignment(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_state_route(
        &state,
        &tenant,
        &id,
        "cancelled",
        "cancelled by its owner".to_owned(),
    )
    .await
}

pub(crate) async fn finish_assignment(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    set_state_route(
        &state,
        &tenant,
        &id,
        "done",
        "marked done by its owner".to_owned(),
    )
    .await
}

/// The progress door's write: the agent's record for its assignment.
pub(crate) async fn record_progress(
    state: &AppState,
    tenant: &str,
    run_id: Option<&str>,
    assignment_id: &str,
    progress: Progress,
) -> Result<Value, String> {
    let mut a = state
        .server_store
        .get_assignment(assignment_id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|a| a.tenant == tenant)
        .ok_or_else(|| format!("unknown assignment `{assignment_id}`"))?;
    if let (Some(run), Some(plane)) = (run_id, &state.run_deps.assignments) {
        if let Some(bound) = plane.assignment_of(run) {
            if bound != assignment_id {
                return Err(format!(
                    "this run works assignment `{bound}`, not `{assignment_id}`"
                ));
            }
        }
    }
    let now = Utc::now();
    a.progress = Progress {
        recorded_at: Some(now),
        by_run: run_id.map(str::to_owned),
        ..progress
    };
    a.updated_at = now;
    state
        .server_store
        .put_assignment(&a)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "recorded": true,
        "round": a.rounds_used(),
        "rounds_remaining": a.max_rounds.saturating_sub(a.rounds_used()),
        "next": if a.progress.complete { "the assignment is done" } else if a.progress.waiting_for.is_some() { "the assignment waits for a person" } else if a.max_rounds > a.rounds_used() { "another round follows when this one ends" } else { "the rounds are spent; the owner grants more" },
    }))
}
