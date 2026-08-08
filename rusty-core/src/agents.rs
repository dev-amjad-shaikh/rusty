//! Agent Fabric contracts (R0.7): durable agent identity, the
//! versioned capability manifest, the state-scope taxonomy, the
//! supervision vocabulary (wave 2), and the typed coordination
//! patterns (wave 3).
//!
//! This module freezes the wire shapes the agent fabric builds on — the
//! server's agent registry and activation leases, the agent hosts (a later
//! wave), and the SDKs must agree on them byte-for-byte, exactly the rule
//! [`crate::durable`] set for the R0.6 queue before it. Nothing here
//! performs I/O, scheduling, or supervision; these are pure data contracts
//! plus the addressing grammar both sides of a mailbox must share.
//!
//! The three pillars:
//!
//! - [`AgentId`] — stable, tenant-namespaced agent identity. Identity is
//!   names and records, not processes: it survives redeploys and crashes
//!   because the registry record, the thread, and the mailbox all derive
//!   from the one id. [`AgentId::mailbox_recipient`] is the addressing
//!   discipline the R0.6 queue carries mailbox traffic under.
//! - [`CapabilityManifest`] — the versioned declaration of what an agent
//!   runs, which message kinds its mailbox accepts, which [`StateScope`]s
//!   it may touch, and its budget ceiling. The exact-match
//!   `manifest_version` pin is the agent-level form of R0.6's worker
//!   version pinning: a team started against one manifest never has its
//!   semantics changed mid-flight by a redeploy.
//! - [`StateScope`] — the closed taxonomy of state an agent may read and
//!   write, mapped onto stores that already exist (the agent's checkpoint
//!   log, a shared team thread, the server KV namespaces).
//! - [`SupervisionPolicy`] — the declared restart semantics
//!   ([`RestartPolicy`], OTP's vocabulary) plus the intensity/period
//!   budget and supervisor address; [`EscalationNotice`] is the message
//!   shape an exhausted budget submits to the supervisor's mailbox.
//!   Escalation is a message, not an exit, because Rusty agents are data
//!   and runs, not processes.
//! - [`CoordinationContract`] — the four typed patterns (delegate, fan-out,
//!   race, quorum) a delegator submits as one declaration (wave 3). The
//!   contracts are pure data; the runtime guarantees (bounded fan-out, the
//!   race effect gate, quorum thresholds, journaled dispositions) are
//!   enforced by the server against these shapes.
//!
//! Golden-file tests under `tests/golden/` pin the serialized shapes; any
//! accidental contract drift fails CI. To bless an intentional change,
//! re-run with `UPDATE_GOLDEN=1` and review the diff.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::durable::{ArtifactContract, ErrorClass};
use crate::llm::Usage;
use crate::record::{Effect, PayloadRef};

/// The address prefix distinguishing mailbox recipients from worker pools
/// on the task queue: a `recipient` of `agent:{agent_id}` is mailbox
/// traffic, drained one message at a time by the agent's activation; any
/// other recipient is ordinary queue work. Server-side, recipient-addressed
/// tasks are excluded from the pool claim path — a pool worker must never
/// steal a mailbox message out from under the turn-serialization
/// discipline.
pub const AGENT_RECIPIENT_PREFIX: &str = "agent:";

/// A stable agent identity (R0.7).
///
/// Tenant-namespaced like every other server id (`{tenant}/researcher-7`
/// inherits the v0.5 isolation model unchanged — a cross-tenant agent
/// resolves to nothing). Transparent over `String`: the wire shape is the
/// id itself. The newtype — not convention — is what keeps agent ids
/// distinct from run ids, task ids, and pool names at every construction
/// site, the same discipline [`crate::record::PolicyVersion`] applies to
/// policy pins.
///
/// The constructor does not validate: the server's route layer enforces the
/// id grammar (non-empty, bounded, no path separators) at the boundary,
/// the same division of labor every other server id follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    /// Wrap an id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The queue recipient addressing this agent's mailbox:
    /// `agent:{agent_id}`. A mailbox is an addressing discipline on the
    /// existing task queue — not a new queue — so mailbox messages inherit
    /// the R0.6 durability, retry, dead-letter, and cancellation machinery
    /// unchanged.
    pub fn mailbox_recipient(&self) -> String {
        format!("{AGENT_RECIPIENT_PREFIX}{}", self.0)
    }

    /// The thread holding this agent's private state: `agent:{agent_id}` —
    /// a naming convention over the existing checkpointer, not a new store,
    /// so checkpoints, time travel, and fork-on-replay work on agent state
    /// unmodified. The checkpoint log *is* the private state; restart is
    /// re-driving it.
    pub fn thread_id(&self) -> String {
        format!("{AGENT_RECIPIENT_PREFIX}{}", self.0)
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Strip the mailbox prefix from a recipient string: `Some(agent_id)` when
/// `recipient` is mailbox traffic (`agent:{agent_id}`), `None` for ordinary
/// pool/pinned-worker recipients. The server uses this to keep mailbox
/// messages off the pool claim path.
pub fn agent_id_from_recipient(recipient: &str) -> Option<&str> {
    recipient.strip_prefix(AGENT_RECIPIENT_PREFIX)
}

/// A state scope an agent may read and write (R0.7): the closed taxonomy
/// naming where agent state lives, declared per agent in its
/// [`CapabilityManifest::scopes`].
///
/// Each scope maps onto a store that already exists — the contract is the
/// name and the declaration, not a new mechanism. Access is checked against
/// the manifest's declaration: an undeclared access fails fast before any
/// I/O, the same shape as a write to an undeclared channel failing at the
/// barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateScope {
    /// The agent's own thread (`agent:{agent_id}`). The checkpoint log is
    /// the private state: per-turn writes land in channels, the boundary
    /// checkpoint persists them, restart restores them.
    Private,

    /// A thread shared by a team's members (`team:{team_id}`), written only
    /// through mailbox-driven turns so every mutation has a journaled
    /// author. Shared mutable team state outside the turn discipline is not
    /// offered — that is the shared-state bug class the channel/reducer
    /// model was built to kill.
    Team,

    /// The server KV store under a `user:{user_id}` namespace (inside the
    /// tenant's `{tenant}/` isolation prefix).
    User,

    /// The server KV store's tenant namespace itself — configuration and
    /// reference data shared by every agent and run in the tenant.
    Tenant,
}

/// An agent-level budget ceiling (R0.7): the whole-activity bound across
/// turns, the way [`crate::durable::TaskBudget`] bounds one task across its
/// attempts.
///
/// Every field is optional and omitted from the wire when unset, so a
/// manifest carrying no budget — or a partial one — produces no shape
/// change for older readers. `None` means unbounded within the tenant's
/// own quotas, never an invented default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentBudget {
    /// Token ceiling across all of the agent's turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,

    /// Cost ceiling in USD across all turns. Evidence-grade `f64`, matching
    /// [`crate::record::RunEvent::cost_usd`]: the ledger aggregates
    /// elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,

    /// Wall-clock deadline for the whole agent activity, across turns.
    /// Expiry is cancellation by clock one level up from
    /// [`crate::durable::TaskEnvelope::deadline`]: the server stamps it
    /// onto the agent's mailbox messages (the earlier bound wins), and its
    /// breach is a supervision signal — the turn's outstanding work is
    /// cancelled, journaled, and the declared [`SupervisionPolicy`]
    /// decides restart vs escalate (R0.7 wave 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
}

/// The message kind carrying a supervision escalation (R0.7 wave 2).
///
/// Escalation is a *message*, not an exit: when a supervised agent exhausts
/// its restart budget, the runtime submits an [`EscalationNotice`] under
/// this kind to the supervisor's mailbox — durable, journaled,
/// retry-policy-bearing like any other mailbox traffic. A supervisor's
/// manifest must declare this kind in [`CapabilityManifest::accepts`] for
/// the escalation to be delivered; when it cannot be delivered (no
/// supervisor declared, unknown supervisor, or the kind not accepted) the
/// notice dead-letters with the full evidence attached — the design's
/// "root escalations dead-letter" rule, honoring the chosen default of
/// open question 2 (DLQ + operator, no runtime-level root policy).
pub const ESCALATION_MESSAGE_KIND: &str = "escalated";

/// A restart policy (R0.7 wave 2): how a supervised agent is restarted
/// after a failed turn, in Erlang/OTP's vocabulary because that vocabulary
/// is the reference implementation of operational restart semantics.
///
/// "Restart" here means what the design doc defines it to mean: a new run
/// on the agent's thread, restoring the latest checkpoint — the mailbox is
/// untouched, so the crashed turn's message returns to visibility at its
/// own lease expiry and is re-delivered under its idempotency key. State
/// loss is bounded and explicit (the in-flight turn re-executes from its
/// start; everything checkpointed survives), unlike OTP's process restart,
/// because the state is the checkpoint log, not the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Always restarted after a failed turn (within the declared
    /// intensity/period budget).
    Permanent,
    /// Restarted only after an *abnormal* termination. A turn cancelled by
    /// clock (deadline) or by the cancellation tree is control flow, not a
    /// crash — OTP's transient rule applied to the R0.6
    /// [`ErrorClass::Cancelled`] semantics.
    Transient,
    /// Never restarted after a failure: the first failed turn escalates.
    Temporary,
}

/// What triggered a supervision decision (R0.7 wave 2), recorded on every
/// [`SupervisionAttempt`] so the escalation's attempt history reads as
/// evidence, not just as a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisionTrigger {
    /// A mailbox turn settled as failed (the worker running the turn
    /// classified the error into the shared [`ErrorClass`] taxonomy).
    TurnFailed,
    /// The agent's whole-activity deadline
    /// ([`AgentBudget::deadline`]) passed: cancellation by clock one level
    /// up from the task deadline.
    DeadlineBreached,
    /// An operator restarted the agent deliberately
    /// (`POST /agents/{id}/restart`). Manual restarts carry no failure
    /// class and do not consume the restart budget.
    ManualRestart,
}

/// One entry in a supervised agent's attempt history (R0.7 wave 2): the
/// failure (or operator action) a supervision decision was made about.
///
/// The history is the evidence an escalation carries — the design's
/// "escalation with its attempt history intact" — so every entry records
/// the trigger, the classification, and the turn task it came from, not
/// merely a count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisionAttempt {
    /// 1-based position in the agent's full supervision history. When the
    /// decision was a restart, this is the restart ordinal the journaled
    /// `SupervisionEvent` names.
    pub ordinal: u32,
    /// What the runtime observed.
    pub trigger: SupervisionTrigger,
    /// The failed turn's classification (core's closed [`ErrorClass`]
    /// taxonomy). `None` for [`SupervisionTrigger::ManualRestart`] — an
    /// operator action has no failure class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClass>,
    /// The human-readable failure message (or the operator's reason).
    pub message: String,
    /// The mailbox turn task this attempt came from, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// When the attempt was recorded.
    pub at: DateTime<Utc>,
}

/// The supervision policy declared per agent in its
/// [`CapabilityManifest`] (R0.7 wave 2): the restart vocabulary plus the
/// intensity/period budget bounding how much failure the runtime tolerates
/// before escalating to the supervisor.
///
/// Policy is static and declared — no learned supervision in R0.7; the
/// journaled decisions are the R0.10 policy plane's training data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisionPolicy {
    /// When the agent is restarted after a failed turn.
    pub restart: RestartPolicy,
    /// The maximum restarts tolerated within `period_ms` before the
    /// runtime stops restarting and escalates (OTP's restart intensity).
    pub intensity: u32,
    /// The sliding window `intensity` is counted over, in milliseconds
    /// (OTP's restart period). A quiet gap longer than this resets the
    /// count.
    pub period_ms: u64,
    /// The supervisor's agent id (external, same tenant) whose mailbox
    /// receives the [`EscalationNotice`] when the budget is exhausted.
    /// `None` — a root agent — dead-letters its escalations for an
    /// operator, per open question 2's chosen default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<String>,
}

impl SupervisionPolicy {
    /// A policy with no supervisor (a root agent): set the optional field
    /// directly; it is public.
    pub fn new(restart: RestartPolicy, intensity: u32, period_ms: u64) -> Self {
        Self {
            restart,
            intensity,
            period_ms,
            supervisor: None,
        }
    }

    /// Whether a turn ending with `error_class` (`None` = the agent's
    /// deadline breached, cancellation by clock) may be restarted under
    /// this policy — OTP's restart semantics mapped onto the shared
    /// [`ErrorClass`] taxonomy: `permanent` restarts on any termination,
    /// `transient` only on abnormal ones (a cancellation — operator or
    /// clock — is control flow, not a crash), `temporary` never.
    pub fn allows_restart_after(&self, error_class: Option<ErrorClass>) -> bool {
        match self.restart {
            RestartPolicy::Permanent => true,
            RestartPolicy::Transient => {
                matches!(error_class, Some(class) if class != ErrorClass::Cancelled)
            }
            RestartPolicy::Temporary => false,
        }
    }
}

/// The escalation message submitted to a supervisor's mailbox (or the
/// dead-letter queue) when an agent exhausts its restart budget (R0.7 wave
/// 2). This is the payload of every mailbox message of kind
/// [`ESCALATION_MESSAGE_KIND`].
///
/// Escalation-as-message is the structural change from OTP: Rusty agents
/// are data and runs, not processes, so there is no exit signal to trap —
/// the exhausted supervision budget submits this notice instead, durable
/// and journaled like any other mailbox traffic, naming the failed agent,
/// the policy that gave out, and the full attempt history as evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EscalationNotice {
    /// The failed agent (external id within the tenant).
    pub agent_id: String,
    /// The policy that exhausted, verbatim — the supervisor (or the
    /// operator reading the DLQ) sees exactly what was declared, not a
    /// paraphrase.
    pub policy: SupervisionPolicy,
    /// The full attempt history at escalation time, oldest first.
    pub attempts: Vec<SupervisionAttempt>,
    /// When the runtime escalated.
    pub escalated_at: DateTime<Utc>,
}

/// The versioned capability manifest of an agent (R0.7 wave 1): what the
/// agent runs, what its mailbox accepts, which state scopes it may touch,
/// and its budget ceiling.
///
/// Registration pins the manifest in the server's agent registry; a team
/// started against `researcher/1.4.0` pins its mailbox traffic to that
/// manifest, so a mid-team redeploy never changes semantics under an
/// in-flight coordination. `manifest_version` is an *exact* version string,
/// deliberately: semver ranges make a team's behavior depend on the version
/// grammar's rules rather than on what the team actually recorded — the
/// same reason R0.6's worker version pin is exact match only.
///
/// Every field past `agent_kind` / `manifest_version` is additive and
/// omitted from the wire when empty, so manifests written against later
/// waves keep deserializing here and a minimal manifest stays minimal on
/// the wire. The supervision policy (R0.7 wave 2) landed the same additive
/// way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityManifest {
    /// What the agent runs: a graph/assistant identity (a graph plus
    /// config, as the server's assistant registry already models it).
    pub agent_kind: String,

    /// The exact manifest version string (e.g. `researcher/1.4.0`). Exact
    /// match, surviving retries and redeploys — see the type docs.
    pub manifest_version: String,

    /// The message kinds the mailbox accepts: message kind → the
    /// [`ArtifactContract`] its payload must satisfy. A send naming a kind
    /// outside this map fails fast at submission
    /// ([`crate::durable::ErrorClass::InvalidInput`] semantics — never
    /// retried) instead of dead-lettering after the attempt budget.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accepts: BTreeMap<String, ArtifactContract>,

    /// The declared [`StateScope`]s the agent may read and write. Journaled
    /// with the spawn event ([`crate::record::RunEventKind::AgentSpawn`]),
    /// checked at every access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<StateScope>,

    /// The agent-level budget ceiling. `None` (the default) means no
    /// agent-level bound; tenant quotas still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<AgentBudget>,

    /// The declared supervision policy (R0.7 wave 2): restart semantics,
    /// the intensity/period budget, and the supervisor escalation is
    /// addressed to. `None` (the default) means unmanaged — failures stand
    /// on their own, no restarts and no escalations; the runtime only
    /// supervises agents that declare a policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<SupervisionPolicy>,
}

impl CapabilityManifest {
    /// A minimal manifest: the agent kind and its exact version, accepting
    /// no message kinds and declaring no scopes. Set the optional fields
    /// directly; they are public.
    pub fn new(agent_kind: impl Into<String>, manifest_version: impl Into<String>) -> Self {
        Self {
            agent_kind: agent_kind.into(),
            manifest_version: manifest_version.into(),
            accepts: BTreeMap::new(),
            scopes: Vec::new(),
            budget: None,
            supervision: None,
        }
    }

    /// The contract the manifest declares for message `kind`, or `None`
    /// when the mailbox refuses that kind outright.
    pub fn accepts_kind(&self, kind: &str) -> Option<&ArtifactContract> {
        self.accepts.get(kind)
    }
}

// ---------------------------------------------------------------------------
// Coordination patterns (R0.7 wave 3)
// ---------------------------------------------------------------------------

/// The reserved message kind a coordination's outcome is delivered to the
/// delegator's mailbox under (R0.7 wave 3). Reserved the way
/// [`ESCALATION_MESSAGE_KIND`] is: an agent that wants to submit a pattern
/// must declare this kind in its manifest, otherwise the submission is
/// rejected at the door — a delegator that cannot receive the outcome would
/// strand every pattern it starts.
pub const COORDINATION_RESULT_KIND: &str = "coordination_result";

/// Returns `true` when `kind` is the reserved coordination outcome kind.
pub fn is_coordination_result(kind: &str) -> bool {
    kind == COORDINATION_RESULT_KIND
}

/// The four typed coordination patterns (R0.7 wave 3). The variant name is
/// the wire tag of [`CoordinationContract`] and the `pattern` field of every
/// artifact the runtime produces for the pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationKind {
    /// Hand one task to one member; the member's result is the outcome.
    Delegate,
    /// Run members under a bounded in-flight window and merge the results.
    FanOut,
    /// First completed member wins; losers are cancel-signalled.
    Race,
    /// Accept the first `k` completions and resolve them into one outcome.
    Quorum,
}

/// What a fan-out does when one member fails terminally (R0.7 wave 3).
/// There is no third option on purpose: "retry the member" is the queue's
/// retry taxonomy, already expressed on the member task itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberFailurePolicy {
    /// Any terminal member failure fails the whole pattern; the remaining
    /// members are cancel-signalled.
    FailFast,
    /// Completed members' results are merged; missing members are journaled
    /// in the outcome's dispositions. The pattern still completes.
    Partial,
}

/// The terminal settlement of one member, as observed by the pattern from
/// the member's durable task record (R0.7 wave 3). This is the evidence
/// vocabulary of [`CoordinationOutcome::members`] — a member that crashed
/// mid-pattern surfaces as `Failed` (retry budget exhausted) or `Dead`
/// (lease expired past its deadline), never as silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberSettlement {
    /// The member task settled `completed` with a result.
    Completed,
    /// The member task settled `failed` (terminal, retry budget exhausted or
    /// explicitly non-retryable).
    Failed,
    /// The member task settled `cancelled` (cancel-signalled by the pattern,
    /// by the delegator, or by its deadline).
    Cancelled,
    /// The member task landed in the dead-letter queue.
    Dead,
}

/// The settled status of a whole coordination (R0.7 wave 3). Outcome-only:
/// a pattern in flight has no `CoordinationOutcome` yet, so there is no
/// `Open` variant to drift out of sync with reality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationStatus {
    /// The pattern produced its result (a member result, a merge, a winner,
    /// or a resolver decision).
    Completed,
    /// The pattern failed: the delegate failed, a fail-fast fan-out member
    /// failed, or every race candidate failed.
    Failed,
    /// The pattern was cancelled before it could settle.
    Cancelled,
    /// Quorum only: fewer than `k` members can possibly complete, so the
    /// threshold is unreachable. The threshold is never silently downgraded
    /// — `k` in the journaled contract is the `k` that was enforced.
    Unreachable,
}

/// A narrow slice of context handed to a member alongside its input (R0.7
/// wave 3). Grants only ever **narrow** what the target agent already
/// declared in its manifest — a delegation must never become a privilege
/// escalation path. The server rejects a widening grant at submission.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextGrant {
    /// The state scopes the member may touch for this delegation; each must
    /// be a subset of the target manifest's declared scopes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<StateScope>,
    /// Named context channels (team thread ids, KV prefixes) the member may
    /// read. Free-form strings: channels are addressing, not taxonomy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

impl ContextGrant {
    /// `true` when every granted scope is declared by the target manifest —
    /// the only direction a grant may point. Checked at submission; a
    /// widening grant is a 400, not a runtime surprise.
    pub fn narrows(&self, declared: &[StateScope]) -> bool {
        self.scopes.iter().all(|scope| declared.contains(scope))
    }
}

fn default_effect_non_idempotent() -> Effect {
    Effect::NonIdempotent
}

/// Sparse-wire helper for `bool` fields whose meaningful value is `true`:
/// `false` is the default and is never serialized.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One member assignment inside a pattern (R0.7 wave 3): which agent, pinned
/// to which exact manifest version, running which declared message kind,
/// over which input. There is deliberately **no caller-supplied idempotency
/// key** — the runtime derives `coordination:{coordination_id}:{member}` so
/// a retried submission converges on the same member task instead of
/// forking duplicates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delegation {
    /// The member's name inside this pattern. Labels dispositions, member
    /// task ids, and trace nodes; unique within one contract.
    pub member: String,
    /// The target agent's id (external form; the server namespaces it per
    /// tenant like every other id).
    pub agent_id: String,
    /// The exact manifest version the delegation is pinned to. The server
    /// refuses anything but an exact match against the registered record —
    /// the agent-level form of R0.6's worker version pinning.
    pub manifest_version: String,
    /// The message kind submitted to the member's mailbox; must be declared
    /// in the pinned manifest's `accepts`.
    pub kind: String,
    /// The member's input, inline or referenced per the journal's payload
    /// discipline.
    pub input: PayloadRef,
    /// The declared effect class of the work. Defaults to
    /// [`Effect::NonIdempotent`] — the safe assumption, and the one that
    /// makes the race gate fail closed: an undeclared effect is never
    /// eligible to race.
    #[serde(default = "default_effect_non_idempotent")]
    pub effect: Effect,
    /// An optional member deadline. Composed with the target agent's budget
    /// deadline at submission (the earlier wins), exactly like direct
    /// mailbox sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
}

/// `delegate` — hand one task to one member (R0.7 wave 3). The member's
/// settlement is the pattern's settlement; with `handoff`, the journaled
/// outcome doubles as the delegator's handoff record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateContract {
    /// The single member assignment.
    pub delegate: Delegation,
    /// The context granted to the member. May only narrow the target's
    /// declared scopes ([`ContextGrant::narrows`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextGrant>,
    /// The contract the member's result must satisfy. Carried on the wire in
    /// wave 3; contract enforcement on the result lands with schema
    /// validation in a later wave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_contract: Option<ArtifactContract>,
    /// When `true`, the outcome is journaled as the delegator's handoff
    /// record — the delegate's result stands in for the delegator's own
    /// next step. Sparse wire: absent means `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub handoff: bool,
}

/// `fan_out` — run members under a bounded in-flight window and merge the
/// completed results in deterministic order (R0.7 wave 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanOutContract {
    /// The member assignments. Member names must be unique.
    pub members: Vec<Delegation>,
    /// The in-flight window: at most this many member tasks are
    /// unsubmitted-or-running at once. Must be ≥ 1 — the window is the
    /// backpressure guarantee, and a zero window would deadlock the pattern.
    pub max_in_flight: u32,
    /// What one terminal member failure does to the whole pattern.
    pub on_member_failure: MemberFailurePolicy,
}

/// `race` — first completed candidate wins; losers are cancel-signalled
/// (R0.7 wave 3). Every candidate must declare a freely-repeatable effect:
/// a loser may be cancelled at any point, so any candidate that is not safe
/// to abandon makes the whole pattern unsafe. That is a submission-time
/// gate, not a runtime check — see [`RaceContract::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaceContract {
    /// The candidates. Member names must be unique; every candidate's
    /// `effect` must satisfy [`Effect::is_freely_repeatable`].
    pub candidates: Vec<Delegation>,
}

/// How a quorum resolves its first `k` accepted results into one outcome
/// (R0.7 wave 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolver", rename_all = "snake_case")]
pub enum QuorumResolver {
    /// Strict majority over byte-identical outputs: one output must collect
    /// more than half of the accepted votes, else the outcome is
    /// no-majority (decided = `false`).
    MajorityEqual,
    /// The accepted outputs as an array, in deterministic (member task-id)
    /// order — the caller's own policy consumes the k-tuple.
    FirstK,
    /// A named custom resolver. The wire shape is pinned for a later
    /// resolver registry; wave 3 rejects it at submission — shipping the
    /// shape without the mechanism keeps the contract stable without
    /// promising semantics the runtime cannot yet honor.
    Custom {
        /// The registry name of the resolver.
        name: String,
    },
}

/// `quorum` — accept the first `k` completions and resolve them into one
/// outcome (R0.7 wave 3). The threshold is a hard floor: if fewer than `k`
/// members can still complete, the pattern settles
/// [`CoordinationStatus::Unreachable`] — it fails open with the evidence
/// journaled, and never silently downgrades `k`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuorumContract {
    /// The member assignments. Member names must be unique.
    pub members: Vec<Delegation>,
    /// The acceptance threshold: the pattern settles as soon as this many
    /// members have completed. Must be within `1..=members.len()`.
    pub threshold: u32,
    /// How the accepted results become the outcome.
    pub resolver: QuorumResolver,
}

/// A rejection of a coordination contract at validation time (R0.7 wave 3).
/// Every variant maps to a 400 at submission: these are contract violations,
/// not runtime failures, and they must be caught before a single member
/// task exists.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoordinationViolation {
    /// A race candidate declared an effect that is not freely repeatable.
    #[error(
        "race candidate '{member}' declares effect '{effect}', which is not freely \
         repeatable: a race loser may be cancelled at any point, so every candidate \
         must be safe to abandon"
    )]
    RaceEffectNotFreelyRepeatable {
        /// The offending member name.
        member: String,
        /// The declared effect's wire name.
        effect: String,
    },
    /// A pattern was declared with no members.
    #[error("{pattern} declares no members")]
    EmptyMembers {
        /// The pattern's wire name.
        pattern: String,
    },
    /// Two members share a name inside one contract.
    #[error(
        "{pattern} declares member '{member}' twice; member names label task ids \
             and dispositions and must be unique within a pattern"
    )]
    DuplicateMember {
        /// The pattern's wire name.
        pattern: String,
        /// The duplicated member name.
        member: String,
    },
    /// A fan-out window of zero.
    #[error(
        "fan-out max_in_flight must be at least 1: the window is the backpressure \
             guarantee, and a zero window can never make progress"
    )]
    MaxInFlightZero,
    /// A quorum threshold outside `1..=members.len()`.
    #[error(
        "quorum threshold {threshold} is out of range 1..={members}: the threshold \
             is a hard floor and is never silently clamped"
    )]
    QuorumThresholdOutOfRange {
        /// The declared threshold.
        threshold: u32,
        /// The number of declared members.
        members: usize,
    },
    /// A custom quorum resolver, which wave 3 does not honor.
    #[error(
        "custom quorum resolver '{name}' is not supported in wave 3: resolvers are \
             majority_equal and first_k; the custom wire shape is pinned for a later \
             resolver registry"
    )]
    CustomResolverUnsupported {
        /// The rejected registry name.
        name: String,
    },
}

fn effect_wire_name(effect: Effect) -> &'static str {
    match effect {
        Effect::Pure => "pure",
        Effect::ReadOnly => "read_only",
        Effect::Idempotent => "idempotent",
        Effect::Compensatable => "compensatable",
        Effect::NonIdempotent => "non_idempotent",
    }
}

/// Returns `true` when `members` contains a duplicate `member` name. Used by
/// every multi-member validator — member names key dispositions and
/// deterministic task ids, so a duplicate would make the evidence
/// ambiguous.
fn duplicate_member(members: &[Delegation]) -> Option<&str> {
    for (i, m) in members.iter().enumerate() {
        if members[..i]
            .iter()
            .any(|earlier| earlier.member == m.member)
        {
            return Some(m.member.as_str());
        }
    }
    None
}

impl RaceContract {
    /// The race effect gate, enforced at submission: every candidate must be
    /// freely repeatable. This is the pattern's whole safety argument — a
    /// loser is cancel-signalled at an arbitrary point, so a candidate that
    /// cannot be safely abandoned (a `NonIdempotent` charge, a
    /// `Compensatable` write) must never enter a race.
    pub fn validate(&self) -> Result<(), CoordinationViolation> {
        if self.candidates.is_empty() {
            return Err(CoordinationViolation::EmptyMembers {
                pattern: "race".to_string(),
            });
        }
        if let Some(member) = duplicate_member(&self.candidates) {
            return Err(CoordinationViolation::DuplicateMember {
                pattern: "race".to_string(),
                member: member.to_string(),
            });
        }
        for candidate in &self.candidates {
            if !candidate.effect.is_freely_repeatable() {
                return Err(CoordinationViolation::RaceEffectNotFreelyRepeatable {
                    member: candidate.member.clone(),
                    effect: effect_wire_name(candidate.effect).to_string(),
                });
            }
        }
        Ok(())
    }
}

impl FanOutContract {
    /// Structural validation enforced at submission: a non-empty member set
    /// with unique names and a window of at least one.
    pub fn validate(&self) -> Result<(), CoordinationViolation> {
        if self.members.is_empty() {
            return Err(CoordinationViolation::EmptyMembers {
                pattern: "fan_out".to_string(),
            });
        }
        if let Some(member) = duplicate_member(&self.members) {
            return Err(CoordinationViolation::DuplicateMember {
                pattern: "fan_out".to_string(),
                member: member.to_string(),
            });
        }
        if self.max_in_flight == 0 {
            return Err(CoordinationViolation::MaxInFlightZero);
        }
        Ok(())
    }
}

impl QuorumContract {
    /// Structural validation enforced at submission: a non-empty member set
    /// with unique names, a threshold inside `1..=members.len()`, and a
    /// resolver wave 3 can honor. The threshold is checked exactly and never
    /// clamped — a silent clamp would change the pattern's semantics without
    /// a journaled decision.
    pub fn validate(&self) -> Result<(), CoordinationViolation> {
        if self.members.is_empty() {
            return Err(CoordinationViolation::EmptyMembers {
                pattern: "quorum".to_string(),
            });
        }
        if let Some(member) = duplicate_member(&self.members) {
            return Err(CoordinationViolation::DuplicateMember {
                pattern: "quorum".to_string(),
                member: member.to_string(),
            });
        }
        if self.threshold == 0 || self.threshold as usize > self.members.len() {
            return Err(CoordinationViolation::QuorumThresholdOutOfRange {
                threshold: self.threshold,
                members: self.members.len(),
            });
        }
        if let QuorumResolver::Custom { name } = &self.resolver {
            return Err(CoordinationViolation::CustomResolverUnsupported { name: name.clone() });
        }
        Ok(())
    }
}

/// One vote group in a majority tally: an output and how many accepted
/// members produced it byte-identically (R0.7 wave 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuorumTally {
    /// The output the group agrees on.
    pub output: Value,
    /// How many of the accepted members produced it.
    pub votes: u32,
}

/// The result of resolving a quorum's accepted outputs (R0.7 wave 3).
/// Returned by [`resolve_quorum`]; the server converts it into the
/// journaled [`QuorumResolverRecord`].
#[derive(Debug, Clone, PartialEq)]
pub enum QuorumOutcome {
    /// Majority resolved: one output collected a strict majority.
    Decided {
        /// The winning output.
        output: Value,
    },
    /// First-K resolution: the accepted outputs in deterministic order.
    FirstK {
        /// The k accepted outputs, ordered by member task id.
        outputs: Vec<Value>,
    },
    /// No output reached a strict majority. The pattern still completes —
    /// the no-majority evidence is the outcome.
    NoMajority {
        /// The full tally, sorted by votes (desc) then output bytes (asc).
        tallies: Vec<QuorumTally>,
    },
}

/// Resolve a quorum's accepted outputs into an outcome (R0.7 wave 3).
///
/// `accepted` is `(member task id, output)` pairs; the function sorts them
/// by task id first, so the result is deterministic regardless of the order
/// the caller observed completions in — replaying the same journal always
/// reproduces the same resolution. Pure: no I/O, no clock, no randomness.
pub fn resolve_quorum(
    resolver: &QuorumResolver,
    accepted: &[(String, Value)],
) -> Result<QuorumOutcome, CoordinationViolation> {
    let mut accepted: Vec<&(String, Value)> = accepted.iter().collect();
    accepted.sort_by(|a, b| a.0.cmp(&b.0));
    match resolver {
        QuorumResolver::FirstK => Ok(QuorumOutcome::FirstK {
            outputs: accepted
                .iter()
                .map(|(_, output)| (*output).clone())
                .collect(),
        }),
        QuorumResolver::MajorityEqual => {
            // Group by byte-identical output. serde_json::Value equality is
            // structural (object key order is irrelevant), which is exactly
            // the "same answer" a voter means.
            let mut tallies: Vec<QuorumTally> = Vec::new();
            for (_, output) in &accepted {
                match tallies.iter_mut().find(|t| t.output == *output) {
                    Some(tally) => tally.votes += 1,
                    None => tallies.push(QuorumTally {
                        output: (*output).clone(),
                        votes: 1,
                    }),
                }
            }
            // Deterministic evidence order: most votes first, ties broken by
            // the output's serialized bytes.
            tallies.sort_by(|a, b| {
                b.votes
                    .cmp(&a.votes)
                    .then_with(|| a.output.to_string().cmp(&b.output.to_string()))
            });
            let total = accepted.len() as u32;
            match tallies.first() {
                Some(winner) if winner.votes * 2 > total => Ok(QuorumOutcome::Decided {
                    output: winner.output.clone(),
                }),
                _ => Ok(QuorumOutcome::NoMajority { tallies }),
            }
        }
        QuorumResolver::Custom { name } => {
            Err(CoordinationViolation::CustomResolverUnsupported { name: name.clone() })
        }
    }
}

/// Merge a fan-out's completed member results into the pattern result (R0.7
/// wave 3). `results` is `(member task id, output)` pairs for the
/// **completed** members; the merge orders them by task id, so the merged
/// array is byte-deterministic no matter which order members finished in.
/// Missing members are not in the array at all — they are journaled in the
/// outcome's dispositions instead, which is where partial failure evidence
/// belongs.
pub fn merge_fan_out(results: &[(String, Value)]) -> Vec<Value> {
    let mut sorted: Vec<&(String, Value)> = results.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted
        .into_iter()
        .map(|(_, output)| output.clone())
        .collect()
}

/// One member's terminal evidence inside a settled coordination (R0.7 wave
/// 3). Derived from the member's durable task record — never from an
/// in-memory observation — so a server restart recomputes the same
/// disposition from the same store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberDisposition {
    /// The member name from the contract.
    pub member: String,
    /// The member's deterministic task id — the correlation key into the
    /// task queue's own evidence (and the DLQ, when `Dead`).
    pub task_id: String,
    /// How the member settled.
    pub settlement: MemberSettlement,
    /// The member's result, when it completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<PayloadRef>,
    /// The failure's error class, when it failed or died.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClass>,
    /// The failure's message, when it failed or died.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The token usage the member reported on settlement, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Usage>,
    /// The cost the member reported on settlement, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// The journaled record of how a quorum was resolved (R0.7 wave 3): the
/// resolver, the exact inputs it saw, and what it decided. Carried on
/// [`CoordinationOutcome::resolver`] so the decision is auditable without
/// re-running the resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuorumResolverRecord {
    /// The resolver that ran.
    pub resolver: QuorumResolver,
    /// The accepted outputs, in the deterministic order the resolver saw
    /// them (member task-id order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Value>,
    /// The resolved output. `None` under no-majority: the inputs and the
    /// `decided` flag are the evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Whether the resolver reached a decision (`false` = no-majority).
    pub decided: bool,
}

/// The settled outcome of a coordination (R0.7 wave 3) — the payload of the
/// `CoordinationEnd` journal event and of the `coordination_result` message
/// delivered to the delegator's mailbox. One shape for all four patterns:
/// the delegator consumes one contract no matter which pattern it started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinationOutcome {
    /// The pattern's id (external form).
    pub coordination_id: String,
    /// Which pattern ran.
    pub pattern: CoordinationKind,
    /// How the pattern settled.
    pub status: CoordinationStatus,
    /// The pattern's result: the delegate's result, the fan-out merge, the
    /// race winner's result, or the quorum resolution. Absent when the
    /// pattern failed, was cancelled, or reached no decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<PayloadRef>,
    /// Every member's terminal evidence, in contract declaration order —
    /// including members that never produced a result, which is the point:
    /// missing members are journaled, never silent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberDisposition>,
    /// Total tokens spent on work the pattern discarded (race losers,
    /// cancelled members), when the members reported usage. The pattern's
    /// waste accounting is part of the outcome, not a side channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasted_tokens: Option<u64>,
    /// Total reported cost of discarded work, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasted_cost_usd: Option<f64>,
    /// The quorum resolution record, present only for the quorum pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<QuorumResolverRecord>,
}

/// The typed pattern declaration a delegator submits (R0.7 wave 3). The
/// `pattern` tag is the wire discriminator; each variant carries exactly
/// the contract of one pattern. The delegate variant is boxed: its contract
/// embeds a [`Delegation`] with an optional [`ContextGrant`], which would
/// otherwise make every variant pay the largest variant's size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "pattern", rename_all = "snake_case")]
pub enum CoordinationContract {
    /// See [`DelegateContract`].
    Delegate(Box<DelegateContract>),
    /// See [`FanOutContract`].
    FanOut(FanOutContract),
    /// See [`RaceContract`].
    Race(RaceContract),
    /// See [`QuorumContract`].
    Quorum(QuorumContract),
}

impl CoordinationContract {
    /// Which pattern this contract declares.
    pub fn kind(&self) -> CoordinationKind {
        match self {
            Self::Delegate(_) => CoordinationKind::Delegate,
            Self::FanOut(_) => CoordinationKind::FanOut,
            Self::Race(_) => CoordinationKind::Race,
            Self::Quorum(_) => CoordinationKind::Quorum,
        }
    }

    /// Every member assignment in declaration order — one for delegate, all
    /// for the multi-member patterns.
    pub fn members(&self) -> Vec<&Delegation> {
        match self {
            Self::Delegate(c) => vec![&c.delegate],
            Self::FanOut(c) => c.members.iter().collect(),
            Self::Race(c) => c.candidates.iter().collect(),
            Self::Quorum(c) => c.members.iter().collect(),
        }
    }

    /// The structural validation every pattern runs at submission. Dispatch
    /// lives here so the server has exactly one gate to call.
    pub fn validate(&self) -> Result<(), CoordinationViolation> {
        match self {
            Self::Delegate(_) => Ok(()),
            Self::FanOut(c) => c.validate(),
            Self::Race(c) => c.validate(),
            Self::Quorum(c) => c.validate(),
        }
    }
}

/// The payload of a member task the pattern submits (R0.7 wave 3). This is
/// the detection gate for the settle hooks: when any task settles, the
/// server parses its payload as a `CoordinationMessage` and, only if a
/// scoped coordination record matches, drives the pattern forward. Ordinary
/// queue tasks never parse — the pattern field and member field together
/// make a false positive a deliberate act, not an accident.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinationMessage {
    /// The pattern this task belongs to (external id form).
    pub coordination_id: String,
    /// The member name this task executes.
    pub member: String,
    /// Which pattern is driving, denormalized onto the message so a reader
    /// of the task queue can classify member work without a record lookup.
    pub pattern: CoordinationKind,
    /// The member's input, from the contract.
    pub input: PayloadRef,
    /// The context grant handed to the member, when the pattern declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextGrant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_manifest() -> CapabilityManifest {
        let mut manifest = CapabilityManifest::new("researcher", "researcher/1.4.0");
        manifest.accepts.insert(
            "summarize".into(),
            ArtifactContract {
                kind: "application/json".into(),
                max_bytes: Some(65_536),
                schema: Some(json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"topic": {"type": "string"}},
                    "required": ["topic"],
                })),
            },
        );
        manifest.scopes = vec![StateScope::Private, StateScope::Team];
        manifest.budget = Some(AgentBudget {
            max_tokens: Some(250_000),
            max_cost_usd: Some(1.50),
            deadline: DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000),
        });
        manifest
    }

    #[test]
    fn agent_id_derives_mailbox_and_thread_addresses() {
        let id = AgentId::new("acme/researcher-7");
        assert_eq!(id.as_str(), "acme/researcher-7");
        assert_eq!(id.mailbox_recipient(), "agent:acme/researcher-7");
        // The thread convention mirrors the mailbox address — one name, two
        // views (queue addressing, checkpoint log).
        assert_eq!(id.thread_id(), "agent:acme/researcher-7");
        // Transparent wire shape: the bare string, no wrapper object.
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            json!("acme/researcher-7")
        );
        assert_eq!(id.to_string(), "acme/researcher-7");
    }

    #[test]
    fn recipient_parsing_strips_only_the_agent_prefix() {
        assert_eq!(
            agent_id_from_recipient("agent:researcher-7"),
            Some("researcher-7")
        );
        assert_eq!(agent_id_from_recipient("pool-default"), None);
        assert_eq!(agent_id_from_recipient("agent:"), Some(""));
        // A prefix alone is not a mailbox address ("agents:x" is not one).
        assert_eq!(agent_id_from_recipient("agents:researcher-7"), None);
    }

    #[test]
    fn state_scope_serde_names_are_snake_case() {
        for (scope, name) in [
            (StateScope::Private, "private"),
            (StateScope::Team, "team"),
            (StateScope::User, "user"),
            (StateScope::Tenant, "tenant"),
        ] {
            assert_eq!(serde_json::to_value(scope).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<StateScope>(json!(name)).unwrap(),
                scope
            );
        }
        assert!(serde_json::from_value::<StateScope>(json!("global")).is_err());
    }

    #[test]
    fn manifest_serde_roundtrip_and_sparse_wire_shape() {
        let manifest = sample_manifest();
        let back: CapabilityManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(manifest, back);

        // A minimal manifest stays minimal on the wire: only the two
        // required keys, nothing null or empty — pre-R0.7-shaped readers see
        // no new keys at all.
        let minimal = CapabilityManifest::new("researcher", "researcher/1.4.0");
        let value = serde_json::to_value(&minimal).unwrap();
        assert_eq!(
            value,
            json!({"agent_kind": "researcher", "manifest_version": "researcher/1.4.0"})
        );

        // And the minimal shape — what the smallest client writes — loads
        // with honest defaults: nothing accepted, no scopes, no budget.
        let back: CapabilityManifest = serde_json::from_value(value).unwrap();
        assert_eq!(back, minimal);
        assert!(back.accepts_kind("summarize").is_none());
    }

    #[test]
    fn manifest_lookup_finds_only_declared_kinds() {
        let manifest = sample_manifest();
        let contract = manifest.accepts_kind("summarize").expect("declared kind");
        assert_eq!(contract.kind, "application/json");
        assert!(contract.schema.is_some());
        assert!(manifest.accepts_kind("delete_everything").is_none());
    }

    #[test]
    fn artifact_contract_schema_field_is_additive() {
        // A pre-R0.7 contract — exactly the shape the R0.6 golden pins —
        // deserializes with the schema unset.
        let r06_shape = json!({"kind": "application/json", "max_bytes": 65536});
        let contract: ArtifactContract = serde_json::from_value(r06_shape).unwrap();
        assert_eq!(contract.schema, None);
        // Unset: absent from the wire, so R0.6 readers see no shape change.
        assert_eq!(
            serde_json::to_value(&contract).unwrap(),
            json!({"kind": "application/json", "max_bytes": 65536})
        );

        // Set: carried as the schema document itself, surviving the
        // round-trip.
        let with_schema = ArtifactContract {
            kind: "application/json".into(),
            max_bytes: None,
            schema: Some(json!({"type": "object"})),
        };
        let back: ArtifactContract =
            serde_json::from_str(&serde_json::to_string(&with_schema).unwrap()).unwrap();
        assert_eq!(with_schema, back);
    }

    #[test]
    fn agent_event_kinds_stay_additive_to_the_closed_enum() {
        use crate::record::RunEventKind;
        // The R0.7 variants serialize snake_case like every event kind
        // before them, and pre-R0.7 kinds deserialize unchanged — old
        // journals keep loading (the rule R0.6's EffectReceipt followed).
        for (kind, name) in [
            (RunEventKind::AgentSpawn, "agent_spawn"),
            (RunEventKind::AgentExit, "agent_exit"),
            (RunEventKind::MailboxSend, "mailbox_send"),
            (RunEventKind::MailboxReceive, "mailbox_receive"),
            (RunEventKind::SupervisionEvent, "supervision_event"),
            (RunEventKind::CoordinationStart, "coordination_start"),
            (RunEventKind::CoordinationEnd, "coordination_end"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<RunEventKind>(json!(name)).unwrap(),
                kind
            );
        }
        assert_eq!(
            serde_json::from_value::<RunEventKind>(json!("model_call")).unwrap(),
            RunEventKind::ModelCall
        );
        assert!(serde_json::from_value::<RunEventKind>(json!("agent_magic")).is_err());
    }

    #[test]
    fn restart_policy_and_trigger_serde_names_are_snake_case() {
        for (policy, name) in [
            (RestartPolicy::Permanent, "permanent"),
            (RestartPolicy::Transient, "transient"),
            (RestartPolicy::Temporary, "temporary"),
        ] {
            assert_eq!(serde_json::to_value(policy).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<RestartPolicy>(json!(name)).unwrap(),
                policy
            );
        }
        for (trigger, name) in [
            (SupervisionTrigger::TurnFailed, "turn_failed"),
            (SupervisionTrigger::DeadlineBreached, "deadline_breached"),
            (SupervisionTrigger::ManualRestart, "manual_restart"),
        ] {
            assert_eq!(serde_json::to_value(trigger).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<SupervisionTrigger>(json!(name)).unwrap(),
                trigger
            );
        }
        assert!(serde_json::from_value::<RestartPolicy>(json!("always")).is_err());
    }

    #[test]
    fn allows_restart_after_maps_the_otp_vocabulary_onto_error_class() {
        let permanent = SupervisionPolicy::new(RestartPolicy::Permanent, 3, 60_000);
        let transient = SupervisionPolicy::new(RestartPolicy::Transient, 3, 60_000);
        let temporary = SupervisionPolicy::new(RestartPolicy::Temporary, 3, 60_000);

        // Permanent restarts on any termination, including cancellation.
        assert!(permanent.allows_restart_after(Some(ErrorClass::Transient)));
        assert!(permanent.allows_restart_after(Some(ErrorClass::Cancelled)));
        assert!(permanent.allows_restart_after(None));
        // Transient restarts only on abnormal termination: cancellation
        // (operator or deadline clock) is control flow, not a crash.
        assert!(transient.allows_restart_after(Some(ErrorClass::Timeout)));
        assert!(!transient.allows_restart_after(Some(ErrorClass::Cancelled)));
        assert!(!transient.allows_restart_after(None));
        // Temporary never restarts after a failure.
        assert!(!temporary.allows_restart_after(Some(ErrorClass::Unknown)));
        assert!(!temporary.allows_restart_after(None));
    }

    #[test]
    fn supervision_policy_wire_shape_is_sparse_and_additive() {
        // Root policy: no supervisor key on the wire at all.
        let root = SupervisionPolicy::new(RestartPolicy::Permanent, 3, 60_000);
        assert_eq!(
            serde_json::to_value(&root).unwrap(),
            json!({"restart": "permanent", "intensity": 3, "period_ms": 60000})
        );
        let back: SupervisionPolicy =
            serde_json::from_str(&serde_json::to_string(&root).unwrap()).unwrap();
        assert_eq!(root, back);

        let with_supervisor = SupervisionPolicy {
            supervisor: Some("boss".into()),
            ..root.clone()
        };
        assert_eq!(
            serde_json::to_value(&with_supervisor).unwrap(),
            json!({
                "restart": "permanent",
                "intensity": 3,
                "period_ms": 60000,
                "supervisor": "boss",
            })
        );

        // A minimal manifest — the pre-wave-2 shape — keeps deserializing
        // with the policy unset, and stays minimal on the wire.
        let minimal = CapabilityManifest::new("researcher", "researcher/1.4.0");
        assert_eq!(minimal.supervision, None);
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            json!({"agent_kind": "researcher", "manifest_version": "researcher/1.4.0"})
        );
    }

    #[test]
    fn attempt_history_and_escalation_notice_round_trip() {
        let t0 = DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000).unwrap();
        let attempts = vec![
            SupervisionAttempt {
                ordinal: 1,
                trigger: SupervisionTrigger::TurnFailed,
                error_class: Some(ErrorClass::Transient),
                message: "model timed out".into(),
                task_id: Some("task-1".into()),
                at: t0,
            },
            // A manual restart carries no failure class and no task —
            // both keys vanish from the wire.
            SupervisionAttempt {
                ordinal: 2,
                trigger: SupervisionTrigger::ManualRestart,
                error_class: None,
                message: "operator reset".into(),
                task_id: None,
                at: t0 + chrono::Duration::seconds(5),
            },
        ];
        let manual = serde_json::to_value(&attempts[1]).unwrap();
        // Sparse wire: unset fields are absent, not null.
        assert!(manual.get("error_class").is_none());
        assert!(manual.get("task_id").is_none());

        let notice = EscalationNotice {
            agent_id: "looper".into(),
            policy: SupervisionPolicy {
                supervisor: Some("boss".into()),
                ..SupervisionPolicy::new(RestartPolicy::Permanent, 2, 60_000)
            },
            attempts,
            escalated_at: t0 + chrono::Duration::seconds(9),
        };
        let back: EscalationNotice =
            serde_json::from_str(&serde_json::to_string(&notice).unwrap()).unwrap();
        assert_eq!(notice, back);
        // The attempt history survives the round-trip intact — ordinals,
        // classes, task ids: the escalation's evidence is the point.
        assert_eq!(back.attempts.len(), 2);
        assert_eq!(back.attempts[0].ordinal, 1);
        assert_eq!(back.attempts[0].error_class, Some(ErrorClass::Transient));
        assert_eq!(back.attempts[0].task_id.as_deref(), Some("task-1"));
    }

    // ---------------------------------------------------------------------
    // Coordination patterns (wave 3)
    // ---------------------------------------------------------------------

    fn delegation(member: &str, effect: Effect) -> Delegation {
        Delegation {
            member: member.into(),
            agent_id: format!("agent-{member}"),
            manifest_version: "researcher/1.4.0".into(),
            kind: "summarize".into(),
            input: PayloadRef::inline(json!({"topic": member})),
            effect,
            deadline: None,
        }
    }

    #[test]
    fn race_effect_gate_fails_closed() {
        // Freely-repeatable candidates pass: pure, read_only, idempotent.
        for effect in [Effect::Pure, Effect::ReadOnly, Effect::Idempotent] {
            let contract = RaceContract {
                candidates: vec![delegation("a", effect), delegation("b", Effect::Pure)],
            };
            assert!(contract.validate().is_ok(), "{effect:?} must be raceable");
        }
        // NonIdempotent and Compensatable are rejected, naming the member
        // and the effect — and the *default* (undeclared) effect is
        // NonIdempotent, so an undeclared candidate is rejected too.
        let undeclared: Delegation = serde_json::from_value(json!({
            "member": "a",
            "agent_id": "agent-a",
            "manifest_version": "researcher/1.4.0",
            "kind": "summarize",
            "input": {"kind": "inline", "value": {"topic": "a"}},
        }))
        .unwrap();
        assert_eq!(undeclared.effect, Effect::NonIdempotent);
        let contract = RaceContract {
            candidates: vec![undeclared, delegation("b", Effect::Pure)],
        };
        assert_eq!(
            contract.validate(),
            Err(CoordinationViolation::RaceEffectNotFreelyRepeatable {
                member: "a".into(),
                effect: "non_idempotent".into(),
            })
        );
        let contract = RaceContract {
            candidates: vec![delegation("a", Effect::Compensatable)],
        };
        assert!(matches!(
            contract.validate(),
            Err(CoordinationViolation::RaceEffectNotFreelyRepeatable { .. })
        ));
    }

    #[test]
    fn pattern_contracts_validate_structure() {
        // Empty member sets are rejected for every multi-member pattern.
        let race = RaceContract { candidates: vec![] };
        assert_eq!(
            race.validate(),
            Err(CoordinationViolation::EmptyMembers {
                pattern: "race".into()
            })
        );
        // Duplicate member names make dispositions ambiguous — rejected.
        let fan_out = FanOutContract {
            members: vec![delegation("a", Effect::Pure), delegation("a", Effect::Pure)],
            max_in_flight: 1,
            on_member_failure: MemberFailurePolicy::Partial,
        };
        assert_eq!(
            fan_out.validate(),
            Err(CoordinationViolation::DuplicateMember {
                pattern: "fan_out".into(),
                member: "a".into(),
            })
        );
        // A zero window can never make progress.
        let fan_out = FanOutContract {
            members: vec![delegation("a", Effect::Pure)],
            max_in_flight: 0,
            on_member_failure: MemberFailurePolicy::FailFast,
        };
        assert_eq!(
            fan_out.validate(),
            Err(CoordinationViolation::MaxInFlightZero)
        );
        // Quorum thresholds are exact: 0 and > members are both out of
        // range, never clamped.
        let quorum = QuorumContract {
            members: vec![delegation("a", Effect::Pure)],
            threshold: 0,
            resolver: QuorumResolver::MajorityEqual,
        };
        assert_eq!(
            quorum.validate(),
            Err(CoordinationViolation::QuorumThresholdOutOfRange {
                threshold: 0,
                members: 1,
            })
        );
        let quorum = QuorumContract {
            threshold: 2,
            ..quorum
        };
        assert!(matches!(
            quorum.validate(),
            Err(CoordinationViolation::QuorumThresholdOutOfRange { .. })
        ));
        // Custom resolvers are a pinned wire shape, not a wave-3 mechanism.
        let quorum = QuorumContract {
            members: vec![delegation("a", Effect::Pure)],
            threshold: 1,
            resolver: QuorumResolver::Custom {
                name: "semantic_vote".into(),
            },
        };
        assert_eq!(
            quorum.validate(),
            Err(CoordinationViolation::CustomResolverUnsupported {
                name: "semantic_vote".into(),
            })
        );
    }

    #[test]
    fn context_grant_only_narrows() {
        let declared = vec![StateScope::Private, StateScope::Team];
        let grant = ContextGrant {
            scopes: vec![StateScope::Team],
            channels: vec!["thread:team".into()],
        };
        assert!(grant.narrows(&declared));
        let widening = ContextGrant {
            scopes: vec![StateScope::Tenant],
            channels: vec![],
        };
        assert!(!widening.narrows(&declared));
        // An empty grant narrows trivially — it grants nothing.
        assert!(ContextGrant::default().narrows(&[]));
    }

    #[test]
    fn merge_fan_out_orders_by_task_id_not_completion_order() {
        let results = vec![
            ("t--c--b".to_string(), json!("B")),
            ("t--c--d".to_string(), json!("D")),
            ("t--c--a".to_string(), json!("A")),
        ];
        assert_eq!(
            merge_fan_out(&results),
            vec![json!("A"), json!("B"), json!("D")]
        );
        // Input permutation cannot change the merge.
        let mut reversed = results.clone();
        reversed.reverse();
        assert_eq!(merge_fan_out(&reversed), merge_fan_out(&results));
    }

    #[test]
    fn resolve_quorum_majority_decides_and_tallies_ties() {
        let accepted = vec![
            ("t--c--a".to_string(), json!("X")),
            ("t--c--b".to_string(), json!("Y")),
            ("t--c--c".to_string(), json!("X")),
        ];
        let outcome = resolve_quorum(&QuorumResolver::MajorityEqual, &accepted).unwrap();
        assert_eq!(outcome, QuorumOutcome::Decided { output: json!("X") });

        // Two-two split: no strict majority, and the tallies are
        // deterministic (votes desc, then output bytes).
        let tied = vec![
            ("t--c--a".to_string(), json!("X")),
            ("t--c--b".to_string(), json!("Y")),
            ("t--c--c".to_string(), json!("Y")),
            ("t--c--d".to_string(), json!("X")),
        ];
        let outcome = resolve_quorum(&QuorumResolver::MajorityEqual, &tied).unwrap();
        assert_eq!(
            outcome,
            QuorumOutcome::NoMajority {
                tallies: vec![
                    QuorumTally {
                        output: json!("X"),
                        votes: 2,
                    },
                    QuorumTally {
                        output: json!("Y"),
                        votes: 2,
                    },
                ]
            }
        );

        // Object equality is structural: key order does not split a vote.
        let objects = vec![
            ("t--c--a".to_string(), json!({"p": 1, "q": 2})),
            ("t--c--b".to_string(), json!({"q": 2, "p": 1})),
            ("t--c--c".to_string(), json!({"p": 9})),
        ];
        let outcome = resolve_quorum(&QuorumResolver::MajorityEqual, &objects).unwrap();
        assert_eq!(
            outcome,
            QuorumOutcome::Decided {
                output: json!({"p": 1, "q": 2})
            }
        );
    }

    #[test]
    fn resolve_quorum_first_k_is_task_id_ordered_and_deterministic() {
        let accepted = vec![
            ("t--c--b".to_string(), json!(2)),
            ("t--c--a".to_string(), json!(1)),
            ("t--c--c".to_string(), json!(3)),
        ];
        let outcome = resolve_quorum(&QuorumResolver::FirstK, &accepted).unwrap();
        assert_eq!(
            outcome,
            QuorumOutcome::FirstK {
                outputs: vec![json!(1), json!(2), json!(3)]
            }
        );
        // Permuted input, identical outcome — replay determinism.
        let mut permuted = accepted.clone();
        permuted.reverse();
        assert_eq!(
            resolve_quorum(&QuorumResolver::FirstK, &permuted).unwrap(),
            outcome
        );
        // Custom resolvers are rejected here too — defense in depth behind
        // the submission gate.
        assert!(matches!(
            resolve_quorum(&QuorumResolver::Custom { name: "x".into() }, &accepted),
            Err(CoordinationViolation::CustomResolverUnsupported { .. })
        ));
    }

    #[test]
    fn coordination_contract_dispatch_and_sparse_wire() {
        let contract = CoordinationContract::Delegate(Box::new(DelegateContract {
            delegate: delegation("only", Effect::Pure),
            context: None,
            result_contract: None,
            handoff: false,
        }));
        assert_eq!(contract.kind(), CoordinationKind::Delegate);
        assert_eq!(contract.members().len(), 1);
        assert!(contract.validate().is_ok());

        // Sparse wire: unset optional fields and false handoff vanish;
        // the pattern tag is the discriminator.
        let wire = serde_json::to_value(&contract).unwrap();
        assert_eq!(wire["pattern"], json!("delegate"));
        assert!(wire["delegate"].get("context").is_none());
        assert!(wire["delegate"].get("result_contract").is_none());
        assert!(wire["delegate"].get("handoff").is_none());
        // The effect is explicit on the wire even when defaulted in.
        assert_eq!(wire["delegate"]["effect"], json!("pure"));
        let back: CoordinationContract =
            serde_json::from_str(&serde_json::to_string(&contract).unwrap()).unwrap();
        assert_eq!(contract, back);
    }

    #[test]
    fn coordination_message_and_outcome_round_trip() {
        let message = CoordinationMessage {
            coordination_id: "c1".into(),
            member: "a".into(),
            pattern: CoordinationKind::FanOut,
            input: PayloadRef::inline(json!({"topic": "a"})),
            context: None,
        };
        let back: CoordinationMessage =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(message, back);
        assert!(is_coordination_result(COORDINATION_RESULT_KIND));
        assert!(!is_coordination_result("summarize"));

        let outcome = CoordinationOutcome {
            coordination_id: "c1".into(),
            pattern: CoordinationKind::FanOut,
            status: CoordinationStatus::Completed,
            result: Some(PayloadRef::inline(json!(["A"]))),
            members: vec![MemberDisposition {
                member: "a".into(),
                task_id: "t--c1--a".into(),
                settlement: MemberSettlement::Completed,
                result: Some(PayloadRef::inline(json!("A"))),
                error_class: None,
                error: None,
                tokens: None,
                cost_usd: None,
            }],
            wasted_tokens: None,
            wasted_cost_usd: None,
            resolver: None,
        };
        let wire = serde_json::to_value(&outcome).unwrap();
        // Sparse: no waste fields, no resolver record on a fan-out.
        assert!(wire.get("wasted_tokens").is_none());
        assert!(wire.get("wasted_cost_usd").is_none());
        assert!(wire.get("resolver").is_none());
        let back: CoordinationOutcome =
            serde_json::from_str(&serde_json::to_string(&outcome).unwrap()).unwrap();
        assert_eq!(outcome, back);
    }
}
