//! The goals plane: durable, revisioned objectives an agent works toward
//! across steps, turns, and restarts.
//!
//! A goal is not a task. A task is one run's work; a goal outlives the run
//! — it is the answer to "what is the agent trying to achieve, where does
//! that stand, and who moved it last?" The plane keeps that answer honest:
//!
//! - [`Goal`] — one objective: a content-derived id (`goal-` + SHA-256 of
//!   the canonical title and description, so re-creating the same goal
//!   converges on the same record), a phase in the closed machine
//!   ([`GoalPhase`]: `active | paused | blocked | complete`), a monotonic
//!   revision, the creator's [`GoalProvenance`] (the backlog's closed
//!   `operator:*` / `harness:*` vocabulary, plus `agent:*` for goals an
//!   agent sets itself), and an optional round cap — a budget of work
//!   rounds the goal may spend before it auto-blocks.
//! - [`GoalStore`] — the persisted half: one JSON file holding every goal
//!   and the audit trail, rewritten atomically (temp-write-then-rename,
//!   the checkpointer's discipline) on every accepted change, format
//!   versioned with loud refusal on anything it cannot read, and
//!   tamper-evident by construction — a goal's id is re-derived from its
//!   contents at deserialization and a mismatch fails closed.
//! - The audit trail ([`GoalAuditEntry`]) — every state change journaled
//!   in the store: who (provenance), what (from → to phase, the revision
//!   it produced), why (a mandatory reason), and when (the injected
//!   clock). Creation is the first entry; there is no silent mutation
//!   anywhere in the plane.
//! - The tools ([`CreateGoalTool`], [`GetGoalTool`], [`UpdateGoalTool`]) —
//!   the plane as journaled tool calls: `create_goal` is
//!   [`Effect::Idempotent`] under the content-derived id, `get_goal` is
//!   [`Effect::ReadOnly`], and `update_goal` is [`Effect::Compensatable`]
//!   (a phase move can be answered by the reverse move; the trail keeps
//!   both).
//!
//! Every clock read is caller-injected (`now` parameters and the
//! [`Clock`] seam the tools carry): the plane is deterministic end to
//! end, and a journaled call's inputs fully determine its result.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Result, RustyError};
use crate::journal::Clock;
use crate::record::Effect;
use crate::tool::Tool;

/// The goal id prefix: a goal id is `goal-` followed by the lowercase hex
/// SHA-256 of the goal's canonical identity (title + description) — the
/// same digest convention backlog entries and capability sets follow.
pub const GOAL_ID_PREFIX: &str = "goal-";

/// The persisted goals file's format version. Reading a file that
/// declares anything else fails closed — a goals file the plane cannot
/// interpret is evidence to preserve, not to guess at.
pub const GOALS_FORMAT_VERSION: u32 = 1;

/// The provenance label the harness itself carries when the plane moves a
/// goal (the round-cap auto-block). A transition the harness made is
/// attributed to the harness, never laundered through the caller.
pub const HARNESS_GOALS_PROVENANCE: &str = "harness:goals";

/// Bounds on goal text fields. A goal is an operator-facing artifact; the
/// bounds keep one objective from spending the store's readability.
pub const MAX_GOAL_TITLE_BYTES: usize = 128;
/// See [`MAX_GOAL_TITLE_BYTES`].
pub const MAX_GOAL_DESCRIPTION_BYTES: usize = 2048;
/// See [`MAX_GOAL_TITLE_BYTES`].
pub const MAX_GOAL_REASON_BYTES: usize = 512;
/// The bound on one provenance id (the `*` in `operator:*`).
pub const MAX_GOAL_ACTOR_BYTES: usize = 128;
/// The most audit entries `get_goal` renders in one reply. The trail is
/// unbounded by design (it is the evidence); the view over it is not.
pub const MAX_GOAL_TRAIL_VIEW: usize = 64;

/// Where a goal stands. Closed serde vocabulary — the store matches on it
/// and an unknown phase fails at deserialization, never mid-transition.
///
/// The machine: `active → paused | blocked | complete`, and
/// `paused | blocked → active`. `complete` is terminal: a finished goal is
/// evidence, and reopening one would fork what its audit trail means. A
/// paused goal cannot skip to blocked or complete — it resumes first, so
/// every non-active state is reachable only from `active` and the trail
/// reads as a honest work/rest rhythm rather than a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    /// Being worked; the only phase that spends round-cap budget.
    Active,
    /// Deliberately set aside; resumes to `active`.
    Paused,
    /// Cannot progress (a dependency failed, the round cap is spent);
    /// resumes to `active`.
    Blocked,
    /// Achieved. Terminal.
    Complete,
}

impl GoalPhase {
    /// Whether the transition `self → to` exists in the machine.
    fn allows(self, to: GoalPhase) -> bool {
        match self {
            GoalPhase::Active => matches!(
                to,
                GoalPhase::Paused | GoalPhase::Blocked | GoalPhase::Complete
            ),
            GoalPhase::Paused | GoalPhase::Blocked => matches!(to, GoalPhase::Active),
            GoalPhase::Complete => false,
        }
    }
}

/// Who created or moved a goal. The vocabulary is closed so the audit
/// question "did the harness move this goal itself?" always has a typed
/// answer — the same discipline the backlog's provenance keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalProvenance {
    /// A human operator (`operator:{id}`).
    Operator {
        /// The operator id (the `*` in `operator:*`).
        operator: String,
    },
    /// A harness subsystem (`harness:{component}`) — the plane's own
    /// auto-block carries `harness:goals`.
    Harness {
        /// The component id (the `*` in `harness:*`).
        component: String,
    },
    /// An agent (`agent:{id}`) — goals an agent sets or moves itself.
    Agent {
        /// The agent id (the `*` in `agent:*`).
        agent: String,
    },
}

impl GoalProvenance {
    /// The audit label: `operator:{id}`, `harness:{component}`, or
    /// `agent:{id}`.
    pub fn label(&self) -> String {
        match self {
            GoalProvenance::Operator { operator } => format!("operator:{operator}"),
            GoalProvenance::Harness { component } => format!("harness:{component}"),
            GoalProvenance::Agent { agent } => format!("agent:{agent}"),
        }
    }
}

/// Parse an actor string in the closed `operator:*` / `harness:*` /
/// `agent:*` vocabulary into its typed provenance. Anything else fails
/// closed naming the vocabulary — a tool caller cannot invent an origin.
pub fn parse_actor(actor: &str) -> Result<GoalProvenance> {
    let provenance = match actor.split_once(':') {
        Some(("operator", id)) => GoalProvenance::Operator {
            operator: id.to_owned(),
        },
        Some(("harness", id)) => GoalProvenance::Harness {
            component: id.to_owned(),
        },
        Some(("agent", id)) => GoalProvenance::Agent {
            agent: id.to_owned(),
        },
        _ => {
            return Err(RustyError::Tool(format!(
                "actor `{actor}` is outside the provenance vocabulary — actors are \
                 `operator:{{id}}`, `harness:{{component}}`, or `agent:{{id}}`"
            )))
        }
    };
    let id = match &provenance {
        GoalProvenance::Operator { operator } => operator,
        GoalProvenance::Harness { component } => component,
        GoalProvenance::Agent { agent } => agent,
    };
    check_text("actor id", id, MAX_GOAL_ACTOR_BYTES)?;
    Ok(provenance)
}

/// Bounded, control-free text — the shape rule every goal string field
/// shares.
fn check_text(field: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return Err(RustyError::Tool(format!(
            "goal {field} must be non-empty, trimmed, control-free, and at most {max} bytes"
        )));
    }
    Ok(())
}

/// The content-derived goal id: `goal-` + SHA-256 over the canonical
/// serialization of the title and description. The description is part of
/// the identity — two goals that share a title but mean different things
/// are different goals.
fn goal_id(title: &str, description: &str) -> String {
    let canonical = crate::record::canonicalize_value(&json!({
        "description": description,
        "title": title,
    }));
    let bytes = serde_json::to_vec(&canonical).expect("a serde_json::Value always serializes");
    format!("{GOAL_ID_PREFIX}{}", crate::record::sha256_hex(&bytes))
}

/// One goal. Identity is content-derived from the title and description —
/// re-creating the same goal converges on the same id — while phase,
/// revision, round budget, and timestamps are state over that identity,
/// so a transition never forks what the goal *is*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Goal {
    /// `goal-` + SHA-256 of the canonical identity (title + description).
    pub id: String,
    /// What to achieve, one line.
    pub title: String,
    /// What achieving it means.
    pub description: String,
    /// Where the goal stands in the machine.
    pub phase: GoalPhase,
    /// The state version: `1` at creation, `+1` per accepted change —
    /// transitions and spent rounds both count. Monotonic within a goal;
    /// the audit trail carries one entry per revision.
    pub revision: u64,
    /// Who created the goal.
    pub provenance: GoalProvenance,
    /// The work-round budget, when the goal has one. A capped goal spends
    /// one round per [`GoalStore::record_round`] and auto-blocks when the
    /// budget is spent. An uncapped goal has no budget to spend and
    /// refuses round accounting.
    pub max_rounds: Option<u64>,
    /// The unspent budget; `Some` exactly when `max_rounds` is.
    pub rounds_remaining: Option<u64>,
    /// When the goal was created (injected clock).
    pub created_at: DateTime<Utc>,
    /// When the goal last changed state (injected clock).
    pub updated_at: DateTime<Utc>,
}

/// The wire shape, for id-verifying deserialization.
#[derive(Deserialize)]
struct GoalBody {
    id: String,
    title: String,
    description: String,
    phase: GoalPhase,
    revision: u64,
    provenance: GoalProvenance,
    #[serde(default)]
    max_rounds: Option<u64>,
    #[serde(default)]
    rounds_remaining: Option<u64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'de> Deserialize<'de> for Goal {
    /// Read a goal back and re-derive its id from the identity fields: a
    /// goals file whose ids do not match their contents fails closed, the
    /// same discipline the backlog and the content-addressed stores keep.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let body = GoalBody::deserialize(deserializer)?;
        let derived = goal_id(&body.title, &body.description);
        if derived != body.id {
            return Err(serde::de::Error::custom(format!(
                "goal id `{}` does not match its contents (derived `{derived}`); the file is \
                 corrupt or tampered",
                body.id
            )));
        }
        Ok(Goal {
            id: body.id,
            title: body.title,
            description: body.description,
            phase: body.phase,
            revision: body.revision,
            provenance: body.provenance,
            max_rounds: body.max_rounds,
            rounds_remaining: body.rounds_remaining,
            created_at: body.created_at,
            updated_at: body.updated_at,
        })
    }
}

impl Goal {
    /// Create a goal. The id derives from the content, so the same goal
    /// stated twice is the same goal; `now` is the injected clock. A cap
    /// of zero is refused: a goal that can never spend a round is a
    /// caller bug, not a storage event.
    pub fn new(
        title: impl Into<String>,
        description: impl Into<String>,
        provenance: GoalProvenance,
        max_rounds: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        let title = title.into();
        let description = description.into();
        check_text("title", &title, MAX_GOAL_TITLE_BYTES)?;
        check_text("description", &description, MAX_GOAL_DESCRIPTION_BYTES)?;
        if let Some(cap) = max_rounds {
            if cap == 0 {
                return Err(RustyError::Tool(
                    "a round cap of zero budgets no work at all — refuse the goal, do not \
                     store one that auto-blocks on its first round"
                        .to_owned(),
                ));
            }
        }
        let id = goal_id(&title, &description);
        Ok(Goal {
            id,
            title,
            description,
            phase: GoalPhase::Active,
            revision: 1,
            provenance,
            max_rounds,
            rounds_remaining: max_rounds,
            created_at: now,
            updated_at: now,
        })
        .and_then(|goal| goal.validate())
    }

    /// Structural invariants that must hold for any goal, however
    /// constructed — checked at the end of every constructor and
    /// transition.
    fn validate(self) -> Result<Self> {
        if self.revision == 0 {
            return Err(RustyError::Tool(format!(
                "goal `{}` has revision 0 — revisions count state versions from 1",
                self.id
            )));
        }
        match (self.max_rounds, self.rounds_remaining) {
            (Some(cap), Some(remaining)) if remaining <= cap => {}
            (None, None) => {}
            _ => {
                return Err(RustyError::Tool(format!(
                    "goal `{}` has an inconsistent round budget ({:?} remaining of {:?}) — the \
                     budget is Some/Some with remaining ≤ cap, or absent entirely",
                    self.id, self.rounds_remaining, self.max_rounds
                )))
            }
        }
        Ok(self)
    }

    /// Apply a phase transition, returning the next goal. Illegal
    /// transitions fail closed naming both states; every transition
    /// carries a reason and bumps the revision.
    pub fn transition(
        &self,
        to: GoalPhase,
        reason: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        if !self.phase.allows(to) {
            return Err(RustyError::Tool(format!(
                "goal `{}` cannot move {:?} → {:?}: the machine is active → paused | blocked | \
                 complete, paused | blocked → active, and complete is terminal",
                self.id, self.phase, to
            )));
        }
        let reason = reason.into();
        check_text("reason", &reason, MAX_GOAL_REASON_BYTES)?;
        Goal {
            phase: to,
            revision: self.revision + 1,
            updated_at: now,
            ..self.clone()
        }
        .validate()
    }

    /// Spend one work round, returning the next goal. Only an active,
    /// capped goal can spend: an inactive goal is not working, and an
    /// uncapped goal has no budget to account. A spent-out budget refuses
    /// here — the auto-block the store applies at zero is the goal saying
    /// so itself, and this keeps the budget from ever going negative.
    fn work_round(&self, now: DateTime<Utc>) -> Result<Self> {
        if self.phase != GoalPhase::Active {
            return Err(RustyError::Tool(format!(
                "goal `{}` is {:?}, not active — only a working goal spends round budget",
                self.id, self.phase
            )));
        }
        let (cap, remaining) = match (self.max_rounds, self.rounds_remaining) {
            (Some(cap), Some(remaining)) => (cap, remaining),
            _ => {
                return Err(RustyError::Tool(format!(
                    "goal `{}` has no round cap — round accounting applies to capped goals; an \
                     uncapped goal has no budget to spend",
                    self.id
                )))
            }
        };
        if remaining == 0 {
            return Err(RustyError::Tool(format!(
                "goal `{}` has spent its round cap of {cap} — resume it with new budget by \
                 recreating the objective, not by spending below zero",
                self.id
            )));
        }
        Goal {
            rounds_remaining: Some(remaining - 1),
            revision: self.revision + 1,
            updated_at: now,
            ..self.clone()
        }
        .validate()
    }
}

/// What one audit entry records. Closed vocabulary: creation, an explicit
/// transition, a spent work round (phase unchanged), and the harness's
/// round-cap auto-block — distinguished so the trail answers "who moved
/// this goal, and was it the budget?" without inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalAuditKind {
    /// The goal entered the store (`from` is `None`).
    Created,
    /// A caller-requested phase transition.
    Transition,
    /// One work round spent against an active goal (`from == to ==
    /// active`).
    Round,
    /// The round cap reached zero and the plane blocked the goal — the
    /// actor is `harness:goals`, never the caller.
    AutoBlock,
}

/// One journaled state change: who, what, why, when, and the revision it
/// produced. The trail is append-only and persisted alongside the goals —
/// every revision of every goal has exactly one entry, so the trail *is*
/// the goal's history, not a sample of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalAuditEntry {
    /// The goal this entry belongs to.
    pub goal_id: String,
    /// What kind of change this is.
    pub kind: GoalAuditKind,
    /// The revision the goal reached with this entry.
    pub revision: u64,
    /// The phase before the change (`None` at creation).
    pub from: Option<GoalPhase>,
    /// The phase after the change.
    pub to: GoalPhase,
    /// Who made the change.
    pub actor: GoalProvenance,
    /// Why — mandatory, bounded, control-free.
    pub reason: String,
    /// When (the injected clock).
    pub at: DateTime<Utc>,
}

/// The persisted goals file's envelope.
#[derive(Serialize, Deserialize)]
struct GoalsFile {
    format_version: u32,
    goals: Vec<Goal>,
    trail: Vec<GoalAuditEntry>,
}

/// The in-memory projection: goals by id, plus the append-only trail.
#[derive(Debug, Default)]
struct GoalStoreState {
    goals: BTreeMap<String, Goal>,
    trail: Vec<GoalAuditEntry>,
}

/// Map an IO error into the store's error convention (the journal
/// artifact store's discipline).
fn goals_io_error(context: String, e: std::io::Error) -> RustyError {
    RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
        e.kind(),
        format!("{context}: {e}"),
    )))
}

/// The persisted goals plane: one JSON file holding every goal and the
/// audit trail, rewritten atomically (temp-write-then-rename, the
/// checkpointer's discipline) on every accepted change.
///
/// The in-memory index is rebuilt from the file at open; the file is the
/// truth and the index its projection. There is no removal: a goal is
/// created, moved, and completed — never deleted — so the audit question
/// "what was the agent working toward?" keeps a durable answer.
#[derive(Debug)]
pub struct GoalStore {
    path: PathBuf,
    state: Mutex<GoalStoreState>,
}

impl GoalStore {
    /// Open the goals store at `path`, creating nothing yet (the file
    /// appears on the first accepted write). A present file that does not
    /// parse, declares another format version, fails id verification, or
    /// carries a trail entry for a goal it does not hold fails closed.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let file: GoalsFile = serde_json::from_slice(&bytes).map_err(|error| {
                    RustyError::Tool(format!(
                        "goals file `{}` does not parse: {error}; refusing to guess at it",
                        path.display()
                    ))
                })?;
                if file.format_version != GOALS_FORMAT_VERSION {
                    return Err(RustyError::Tool(format!(
                        "goals file `{}` declares format version {}, this plane reads \
                         {GOALS_FORMAT_VERSION}",
                        path.display(),
                        file.format_version
                    )));
                }
                let mut goals = BTreeMap::new();
                for goal in file.goals {
                    if goals.insert(goal.id.clone(), goal).is_some() {
                        return Err(RustyError::Tool(format!(
                            "goals file `{}` holds a duplicate goal id",
                            path.display()
                        )));
                    }
                }
                for entry in &file.trail {
                    if !goals.contains_key(&entry.goal_id) {
                        return Err(RustyError::Tool(format!(
                            "goals file `{}` journals an entry for goal `{}`, which it does \
                             not hold — the trail and the goals disagree, so neither is trusted",
                            path.display(),
                            entry.goal_id
                        )));
                    }
                }
                GoalStoreState {
                    goals,
                    trail: file.trail,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => GoalStoreState::default(),
            Err(e) => {
                return Err(goals_io_error(
                    format!("read goals `{}`", path.display()),
                    e,
                ))
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, GoalStoreState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the current projection back, atomically.
    async fn persist(&self, state: &GoalStoreState) -> Result<()> {
        let file = GoalsFile {
            format_version: GOALS_FORMAT_VERSION,
            goals: state.goals.values().cloned().collect(),
            trail: state.trail.clone(),
        };
        let bytes = serde_json::to_vec(&file)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                goals_io_error(format!("create goals dir `{}`", parent.display()), e)
            })?;
        }
        crate::checkpoint::JsonFileCheckpointer::atomic_write(&self.path, &bytes).await
    }

    /// Store a newly created goal and journal its creation. `true` when
    /// the goal is new; `false` when the id is already occupied (a
    /// converged re-creation — the id *is* the identity, so the stored
    /// goal is never mutated by insertion, only by validated transitions).
    /// Identity fields that disagree with the id's occupant fail closed:
    /// with a content-derived id that means a hash collision or
    /// tampering, never an update.
    pub async fn create(&self, goal: Goal, reason: impl Into<String>) -> Result<(Goal, bool)> {
        let reason = reason.into();
        check_text("reason", &reason, MAX_GOAL_REASON_BYTES)?;
        let snapshot = {
            let mut state = self.lock();
            match state.goals.get(&goal.id) {
                Some(existing) => {
                    if existing.title != goal.title || existing.description != goal.description {
                        return Err(RustyError::Tool(format!(
                            "goal id `{}` is occupied by different identity fields — a \
                             content-address collision or tampering, not an update",
                            goal.id
                        )));
                    }
                    return Ok((existing.clone(), false));
                }
                None => {
                    let entry = GoalAuditEntry {
                        goal_id: goal.id.clone(),
                        kind: GoalAuditKind::Created,
                        revision: goal.revision,
                        from: None,
                        to: goal.phase,
                        actor: goal.provenance.clone(),
                        reason,
                        at: goal.created_at,
                    };
                    state.goals.insert(goal.id.clone(), goal.clone());
                    state.trail.push(entry);
                    GoalStoreState {
                        goals: state.goals.clone(),
                        trail: state.trail.clone(),
                    }
                }
            }
        };
        self.persist(&snapshot).await?;
        Ok((goal, true))
    }

    /// Apply a validated phase transition (see [`Goal::transition`]),
    /// journal it, and persist the outcome. Returns the goal's new state.
    pub async fn transition(
        &self,
        id: &str,
        to: GoalPhase,
        actor: GoalProvenance,
        reason: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Goal> {
        let reason = reason.into();
        let (next, snapshot) = {
            let mut state = self.lock();
            let current = state
                .goals
                .get(id)
                .ok_or_else(|| RustyError::Tool(format!("unknown goal `{id}`")))?;
            let next = current.transition(to, reason.clone(), now)?;
            let entry = GoalAuditEntry {
                goal_id: id.to_owned(),
                kind: GoalAuditKind::Transition,
                revision: next.revision,
                from: Some(current.phase),
                to: next.phase,
                actor,
                reason,
                at: now,
            };
            state.goals.insert(id.to_owned(), next.clone());
            state.trail.push(entry);
            (
                next,
                GoalStoreState {
                    goals: state.goals.clone(),
                    trail: state.trail.clone(),
                },
            )
        };
        self.persist(&snapshot).await?;
        Ok(next)
    }

    /// Spend one work round against an active, capped goal, journal it,
    /// and persist the outcome. When the spend exhausts the budget the
    /// goal auto-blocks in the same call: a second journaled entry,
    /// authored by `harness:goals`, with the cap named as the reason —
    /// the budget's verdict is attributed to the budget, never to the
    /// caller who happened to spend the last round.
    pub async fn record_round(
        &self,
        id: &str,
        actor: GoalProvenance,
        reason: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Goal> {
        let reason = reason.into();
        check_text("reason", &reason, MAX_GOAL_REASON_BYTES)?;
        let (next, snapshot) = {
            let mut state = self.lock();
            let current = state
                .goals
                .get(id)
                .cloned()
                .ok_or_else(|| RustyError::Tool(format!("unknown goal `{id}`")))?;
            let mut next = current.work_round(now)?;
            state.trail.push(GoalAuditEntry {
                goal_id: id.to_owned(),
                kind: GoalAuditKind::Round,
                revision: next.revision,
                from: Some(current.phase),
                to: next.phase,
                actor,
                reason,
                at: now,
            });
            if next.rounds_remaining == Some(0) {
                let cap = next.max_rounds.expect("a spent budget has a cap");
                let blocked = Goal {
                    phase: GoalPhase::Blocked,
                    revision: next.revision + 1,
                    updated_at: now,
                    ..next.clone()
                }
                .validate()?;
                state.trail.push(GoalAuditEntry {
                    goal_id: id.to_owned(),
                    kind: GoalAuditKind::AutoBlock,
                    revision: blocked.revision,
                    from: Some(next.phase),
                    to: GoalPhase::Blocked,
                    actor: GoalProvenance::Harness {
                        component: "goals".to_owned(),
                    },
                    reason: format!(
                        "round cap exhausted: the goal spent the last of its {cap} work \
                         rounds, and the cap auto-blocked it"
                    ),
                    at: now,
                });
                next = blocked;
            }
            state.goals.insert(id.to_owned(), next.clone());
            (
                next,
                GoalStoreState {
                    goals: state.goals.clone(),
                    trail: state.trail.clone(),
                },
            )
        };
        self.persist(&snapshot).await?;
        Ok(next)
    }

    /// One goal by id.
    pub fn get(&self, id: &str) -> Option<Goal> {
        self.lock().goals.get(id).cloned()
    }

    /// Every goal, ordered by id (deterministic — ids are content
    /// addresses, so the order is stable across processes).
    pub fn list(&self) -> Vec<Goal> {
        self.lock().goals.values().cloned().collect()
    }

    /// The number of goals.
    pub fn len(&self) -> usize {
        self.lock().goals.len()
    }

    /// `true` when the store holds no goals.
    pub fn is_empty(&self) -> bool {
        self.lock().goals.is_empty()
    }

    /// The whole audit trail, in journal order.
    pub fn trail(&self) -> Vec<GoalAuditEntry> {
        self.lock().trail.clone()
    }

    /// One goal's audit trail, in journal order.
    pub fn trail_for(&self, id: &str) -> Vec<GoalAuditEntry> {
        self.lock()
            .trail
            .iter()
            .filter(|entry| entry.goal_id == id)
            .cloned()
            .collect()
    }
}

// --------------------------------------------------------------------- //
// Tools — the plane as journaled tool calls
// --------------------------------------------------------------------- //

/// Render a goal as the tools' bounded JSON view of it.
fn goal_view(goal: &Goal) -> Value {
    json!({
        "id": goal.id,
        "title": goal.title,
        "description": goal.description,
        "phase": goal.phase,
        "revision": goal.revision,
        "provenance": goal.provenance.label(),
        "max_rounds": goal.max_rounds,
        "rounds_remaining": goal.rounds_remaining,
        "created_at": goal.created_at,
        "updated_at": goal.updated_at,
    })
}

/// `create_goal` — state a durable objective. The id derives from the
/// title and description, so stating the same goal twice converges on the
/// same record and reports `inserted: false` rather than forking it.
///
/// [`Effect::Idempotent`]: creation is keyed by a content address.
pub struct CreateGoalTool {
    store: Arc<GoalStore>,
    clock: Clock,
}

impl std::fmt::Debug for CreateGoalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateGoalTool")
            .field("clock", &self.clock)
            .finish()
    }
}

impl CreateGoalTool {
    /// A creation tool writing to `store`, timestamping through `clock`
    /// (the injected clock seam — logical in tests and demos).
    pub fn new(store: Arc<GoalStore>, clock: Clock) -> Self {
        Self { store, clock }
    }
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn description(&self) -> &str {
        "State a durable goal: title, description, provenance actor, and an optional work-round \
         cap. The id is content-derived, so re-creating the same goal converges on the existing \
         record. The goal lands active; every later move goes through update_goal with a reason."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "maxLength": MAX_GOAL_TITLE_BYTES},
                "description": {"type": "string", "maxLength": MAX_GOAL_DESCRIPTION_BYTES},
                "actor": {
                    "type": "string",
                    "maxLength": MAX_GOAL_ACTOR_BYTES + 9,
                    "description": "Who states the goal: `operator:{id}`, `harness:{component}`, \
                                    or `agent:{id}` — the closed provenance vocabulary."
                },
                "reason": {"type": "string", "maxLength": MAX_GOAL_REASON_BYTES},
                "max_rounds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional work-round budget. Each recorded round spends one; \
                                    at zero the goal auto-blocks with the cap named as the reason."
                }
            },
            "required": ["title", "description", "actor", "reason"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Idempotent
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let string = |field: &str| -> Result<String> {
            args.get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| RustyError::Tool(format!("`{field}` must be a string")))
        };
        let max_rounds = match args.get("max_rounds") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let cap = value.as_u64().ok_or_else(|| {
                    RustyError::Tool("`max_rounds` must be a positive integer".to_owned())
                })?;
                Some(cap)
            }
        };
        let goal = Goal::new(
            string("title")?,
            string("description")?,
            parse_actor(&string("actor")?)?,
            max_rounds,
            self.clock.now(),
        )?;
        let (stored, inserted) = self.store.create(goal, string("reason")?).await?;
        let mut view = goal_view(&stored);
        view["inserted"] = json!(inserted);
        Ok(view)
    }
}

/// `get_goal` — read one goal and, on request, its audit trail.
///
/// [`Effect::ReadOnly`]: the tool changes nothing; replay serves the
/// journaled answer.
pub struct GetGoalTool {
    store: Arc<GoalStore>,
}

impl std::fmt::Debug for GetGoalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetGoalTool").finish()
    }
}

impl GetGoalTool {
    /// A read tool over `store`.
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn description(&self) -> &str {
        "Read one goal by id: phase, revision, provenance, round budget, and timestamps. With \
         `include_trail`, also return its audit trail (who/what/why/when per revision), bounded \
         to the most recent entries with the total reported."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal_id": {"type": "string"},
                "include_trail": {"type": "boolean", "default": false}
            },
            "required": ["goal_id"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let goal_id = args
            .get("goal_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RustyError::Tool("`goal_id` must be a string".to_owned()))?;
        let goal = self
            .store
            .get(goal_id)
            .ok_or_else(|| RustyError::Tool(format!("unknown goal `{goal_id}`")))?;
        let mut view = goal_view(&goal);
        if args
            .get("include_trail")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let trail = self.store.trail_for(goal_id);
            let total = trail.len();
            let entries: Vec<Value> = trail
                .into_iter()
                .rev()
                .take(MAX_GOAL_TRAIL_VIEW)
                .map(|entry| {
                    json!({
                        "kind": entry.kind,
                        "revision": entry.revision,
                        "from": entry.from,
                        "to": entry.to,
                        "actor": entry.actor.label(),
                        "reason": entry.reason,
                        "at": entry.at,
                    })
                })
                .collect();
            view["trail_total"] = json!(total);
            view["trail"] = json!(entries);
        }
        Ok(view)
    }
}

/// `update_goal` — move a goal through its machine or spend a work round.
/// The action vocabulary is closed: `pause`, `resume`, `block`,
/// `complete` are phase moves; `record_round` spends budget. Every call
/// carries an actor and a reason, and both land in the audit trail with
/// the revision they produced.
///
/// [`Effect::Compensatable`]: a phase move is answered by the reverse
/// move (`pause` by `resume`); the trail keeps both halves, which is what
/// makes the compensation auditable rather than silent.
pub struct UpdateGoalTool {
    store: Arc<GoalStore>,
    clock: Clock,
}

impl std::fmt::Debug for UpdateGoalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateGoalTool")
            .field("clock", &self.clock)
            .finish()
    }
}

impl UpdateGoalTool {
    /// An update tool writing to `store`, timestamping through `clock`.
    pub fn new(store: Arc<GoalStore>, clock: Clock) -> Self {
        Self { store, clock }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn description(&self) -> &str {
        "Move a goal through its phase machine (pause/resume/block/complete) or spend one \
         work round (record_round). Every action needs an actor and a reason; both are \
         journaled. Spending the last round of a capped goal auto-blocks it, attributed to \
         harness:goals with the cap named as the reason."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "goal_id": {"type": "string"},
                "action": {
                    "type": "string",
                    "enum": ["pause", "resume", "block", "complete", "record_round"]
                },
                "actor": {
                    "type": "string",
                    "maxLength": MAX_GOAL_ACTOR_BYTES + 9,
                    "description": "Who moves the goal: `operator:{id}`, `harness:{component}`, \
                                    or `agent:{id}`."
                },
                "reason": {"type": "string", "maxLength": MAX_GOAL_REASON_BYTES}
            },
            "required": ["goal_id", "action", "actor", "reason"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Compensatable
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let string = |field: &str| -> Result<String> {
            args.get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| RustyError::Tool(format!("`{field}` must be a string")))
        };
        let goal_id = string("goal_id")?;
        let actor = parse_actor(&string("actor")?)?;
        let reason = string("reason")?;
        let now = self.clock.now();
        let goal = match string("action")?.as_str() {
            "pause" => {
                self.store
                    .transition(&goal_id, GoalPhase::Paused, actor, reason, now)
                    .await?
            }
            "resume" => {
                self.store
                    .transition(&goal_id, GoalPhase::Active, actor, reason, now)
                    .await?
            }
            "block" => {
                self.store
                    .transition(&goal_id, GoalPhase::Blocked, actor, reason, now)
                    .await?
            }
            "complete" => {
                self.store
                    .transition(&goal_id, GoalPhase::Complete, actor, reason, now)
                    .await?
            }
            "record_round" => {
                self.store
                    .record_round(&goal_id, actor, reason, now)
                    .await?
            }
            other => {
                return Err(RustyError::Tool(format!(
                    "unknown action `{other}` — update_goal takes pause, resume, block, \
                     complete, or record_round"
                )))
            }
        };
        Ok(goal_view(&goal))
    }
}
