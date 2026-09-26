//! Supervision (R0.7 Agent Fabric, wave 2): restart decisions,
//! escalation-as-message, and the per-agent supervision journal.
//!
//! The design contract is `docs/agent-fabric-design.md` §"Supervision:
//! restart and escalation". Three signals feed it, all already durable
//! records: mailbox turn failures (classified into the shared
//! [`ErrorClass`] taxonomy by the worker running the turn), the
//! agent-level deadline (cancellation by clock one level up), and the
//! operator's manual restart. No new failure-detection machinery — this
//! module is the decision, journal, and message mechanics those signals
//! drive.
//!
//! The two structural rules from the design:
//!
//! - **Restart is re-driving the checkpoint log, with the mailbox
//!   untouched.** The server-side half of a restart decision is exactly
//!   this module: the journaled decision, the attempt-history record, and
//!   (for a deadline breach) the cancellation of outstanding mailbox
//!   traffic. The *run* half — a new run on the agent's thread restoring
//!   the latest checkpoint — is the agent host's integration point: a host
//!   claiming the re-delivered turn re-drives the thread from its latest
//!   checkpoint (the W1b activation/mailbox machinery, unmodified). The
//!   agent-host run loop is not this wave's scope; the boundary is stated
//!   here so nobody mistakes the journaled decision for the re-drive.
//! - **Escalation is a message, not an exit.** An exhausted restart budget
//!   submits an [`EscalationNotice`] to the supervisor's mailbox (kind
//!   [`ESCALATION_MESSAGE_KIND`]); a root agent's notice dead-letters with
//!   the full evidence attached — open question 2's chosen default (DLQ +
//!   operator, no runtime-level root policy).
//!
//! Evidence: every decision lands as a `SupervisionEvent` in the agent's
//! **supervision journal** — a per-agent journal (`run id
//! `agent-supervision:{tenant}:{agent_id}``) holding the supervision
//! evidence for the agent's whole life, since supervision decisions are
//! not made inside any one run. Agent cancellations land there as
//! `AgentExit` events. `GET /agents/{id}/supervision` is the read surface;
//! the journal is integrity re-verified on every read, exactly like the
//! Flight Recorder endpoints.
//!
//! Concurrency: supervision triggers for one agent are serialized by the
//! turn protocol — only the turn's lease holder can settle it, the breach
//! path runs on the claiming host's next call, and the latches make the
//! escalation and breach handling exactly-once. The agent record's
//! last-writer-wins update is safe under that discipline (documented on
//! [`ServerStore::update_agent`](crate::server_store::ServerStore::update_agent)).

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rusty_agent_runtime::agents::{
    AgentId, EscalationNotice, SupervisionAttempt, SupervisionPolicy, SupervisionTrigger,
    ESCALATION_MESSAGE_KIND,
};
use rusty_agent_runtime::durable::ErrorClass;
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::record::{Effect, RunEvent, RunEventKind};
use serde_json::{json, Value};

use crate::agents::AgentRecord;
use crate::auth::TenantContext;
use crate::server_store::{ServerStore, StoreResult};
use crate::tasks::{self, TaskRecord, TaskStatus};

/// What the runtime observed about a supervised agent, bundled with the
/// evidence the decision and the attempt record need. Routes construct
/// this at the three trigger points (turn failure settlement, mailbox
/// claim past the agent deadline, the manual restart endpoint).
#[derive(Debug, Clone)]
pub(crate) enum Trigger {
    /// A mailbox turn settled as failed.
    TurnFailed {
        error_class: ErrorClass,
        message: String,
        task_id: String,
    },
    /// The agent's whole-activity deadline passed (cancellation by clock).
    DeadlineBreached,
    /// The operator restarted the agent deliberately.
    ManualRestart { reason: String },
}

impl Trigger {
    /// The core wire vocabulary for the attempt record and journal payload.
    fn kind(&self) -> SupervisionTrigger {
        match self {
            Self::TurnFailed { .. } => SupervisionTrigger::TurnFailed,
            Self::DeadlineBreached => SupervisionTrigger::DeadlineBreached,
            Self::ManualRestart { .. } => SupervisionTrigger::ManualRestart,
        }
    }

    /// The failure classification the policy's restart rule matches on;
    /// `None` for the clock and the operator (both are control flow —
    /// OTP's "not an abnormal termination").
    fn error_class(&self) -> Option<ErrorClass> {
        match self {
            Self::TurnFailed { error_class, .. } => Some(*error_class),
            Self::DeadlineBreached | Self::ManualRestart { .. } => None,
        }
    }

    /// The human-readable evidence line.
    fn message(&self) -> String {
        match self {
            Self::TurnFailed { message, .. } => message.clone(),
            Self::DeadlineBreached => "agent deadline breached (cancellation by clock)".to_string(),
            Self::ManualRestart { reason } => reason.clone(),
        }
    }

    /// The turn task this trigger came from, when there is one.
    fn task_id(&self) -> Option<String> {
        match self {
            Self::TurnFailed { task_id, .. } => Some(task_id.clone()),
            Self::DeadlineBreached | Self::ManualRestart { .. } => None,
        }
    }
}

/// What the supervision policy resolved to for one trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// No policy is declared: the agent is unmanaged — failures stand on
    /// their own, no restart, no escalation, no journaled decision.
    Unsupervised,
    /// The escalation latch is already set: the failure is counted (the
    /// `suppressed_failures` gauge), never re-restarted or re-escalated —
    /// a crash-looping agent must not flood its supervisor's mailbox.
    Suppressed,
    /// Restart the agent: a new run on its thread from the latest
    /// checkpoint (the host integration point — see the module docs),
    /// carrying the 1-based restart ordinal.
    Restart {
        /// The 1-based restart ordinal (the attempt's position in the
        /// agent's full history).
        ordinal: u32,
    },
    /// The restart budget is exhausted (or the policy never restarts after
    /// this termination class): escalate to the supervisor's mailbox — or
    /// the DLQ for a root agent.
    Escalate,
}

/// The pure decision: policy × history × trigger → action. Kept free of
/// I/O so the whole truth table is unit-testable without a store.
///
/// The intensity/period rule: count the failure/breach attempts inside the
/// sliding window `[now - period, now]` — manual restarts are operator
/// actions, not crashes, and never consume the budget. `intensity` is the
/// maximum restarts tolerated in the window; the attempt that would exceed
/// it escalates instead.
pub(crate) fn decide(
    policy: Option<&SupervisionPolicy>,
    attempts: &[SupervisionAttempt],
    escalated: bool,
    trigger: &Trigger,
    now: DateTime<Utc>,
) -> Decision {
    if matches!(trigger, Trigger::ManualRestart { .. }) {
        // The operator's reset is always a restart, budget or no budget —
        // it is also what *clears* the escalation latch.
        return Decision::Restart {
            ordinal: attempts.len() as u32 + 1,
        };
    }
    let Some(policy) = policy else {
        return Decision::Unsupervised;
    };
    if escalated {
        return Decision::Suppressed;
    }
    if !policy.allows_restart_after(trigger.error_class()) {
        return Decision::Escalate;
    }
    let window_start = now - Duration::milliseconds(policy.period_ms.min(i64::MAX as u64) as i64);
    let recent_failures = attempts
        .iter()
        .filter(|a| a.at >= window_start && !matches!(a.trigger, SupervisionTrigger::ManualRestart))
        .count() as u32;
    // The attempt being decided right now counts against the budget too:
    // `recent < intensity` ≡ `recent + 1 <= intensity`.
    if recent_failures < policy.intensity {
        Decision::Restart {
            ordinal: attempts.len() as u32 + 1,
        }
    } else {
        Decision::Escalate
    }
}

/// The deterministic run id of an agent's supervision journal. Distinct
/// from executor run ids (UUIDs) by construction, tenant-unique, and free
/// of `/` so the JSON-file layout keeps one file per journal.
pub(crate) fn supervision_journal_run_id(tenant: &str, agent_external: &str) -> String {
    format!("agent-supervision:{tenant}:{agent_external}")
}

/// Load the agent's supervision journal (or start it) and append one
/// event, persisting the grown snapshot. Returns the event id.
///
/// The journal is rebuilt from its persisted snapshot through
/// [`Journal::from_snapshot`] — the same integrity check the Flight
/// Recorder endpoints run on read — so a tampered or corrupt supervision
/// journal fails the append rather than silently forking the chain.
async fn journal_event(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    agent_external: &str,
    kind: RunEventKind,
    output: Value,
) -> StoreResult<String> {
    let run_id = supervision_journal_run_id(tenant.tenant(), agent_external);
    let journal = match store.get_journal(&run_id).await? {
        Some(snapshot) => Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
            format!(
                "supervision journal for agent `{agent_external}` failed its integrity check: {e}"
            )
        })?,
        // The journal's thread id is the agent's thread convention
        // (external form) — the checkpoint log is the agent's private
        // state, and the supervision journal is evidence *about* it.
        None => Journal::new(
            run_id,
            AgentId::new(agent_external).thread_id(),
            Clock::System,
        ),
    };
    let event_id = journal.record(
        EventDraft::new(kind, Effect::Pure)
            .output(output)
            // Supervision decisions are control plane, never failures: the
            // triggering failure's class travels in the output payload.
            .status(rusty_agent_runtime::record::EventStatus::Ok),
    );
    store.put_journal(&journal.snapshot()).await?;
    Ok(event_id)
}

/// The output payload of a `SupervisionEvent`: the policy (when one is
/// declared — a manual restart needs none), the trigger, the decision,
/// and — for escalations — the full attempt history. One shape for all
/// decisions so a reader needs no per-decision decoding.
fn supervision_payload(
    policy: Option<&SupervisionPolicy>,
    trigger: &Trigger,
    decision: &str,
    ordinal: u32,
    attempts: &[SupervisionAttempt],
) -> Value {
    json!({
        "decision": decision,
        "policy": policy,
        "trigger": trigger.kind(),
        "error_class": trigger.error_class(),
        "message": trigger.message(),
        "task_id": trigger.task_id(),
        "restart_ordinal": ordinal,
        "attempts": attempts,
    })
}

/// Where an escalation notice landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EscalationDelivery {
    /// Submitted to the supervisor's mailbox (kind `escalated`).
    Mailbox {
        /// The mailbox message's task id.
        task_id: String,
        /// `true` when the idempotency key named an already-submitted
        /// copy — the escalation was retried, not doubled.
        deduplicated: bool,
    },
    /// No supervisor could accept it (root agent, unknown supervisor, or
    /// the kind undeclared): the notice is a dead-letter record carrying
    /// the full evidence — open question 2's chosen default.
    DeadLetter {
        /// The dead-letter task's id (`GET /tasks?status=dead` lists it).
        task_id: String,
    },
}

/// Submit the escalation: to the declared supervisor's mailbox when that
/// agent exists (same tenant) and accepts [`ESCALATION_MESSAGE_KIND`],
/// dead-lettered otherwise. The idempotency key
/// (`escalation:{agent}:{ordinal}`) makes a retried escalation effectively
/// once, like every other submission on the queue.
async fn deliver_escalation(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    agent_external: &str,
    policy: &SupervisionPolicy,
    attempts: Vec<SupervisionAttempt>,
    now: DateTime<Utc>,
) -> StoreResult<EscalationDelivery> {
    let notice = EscalationNotice {
        agent_id: agent_external.to_string(),
        policy: policy.clone(),
        attempts,
        escalated_at: now,
    };
    let payload = serde_json::to_value(&notice)
        .map_err(|e| format!("escalation notice serialization failed: {e}"))?;
    // The key names the escalation episode (the triggering attempt's
    // ordinal): a crash between the decision journal and the submission
    // retries into a dedup, never a duplicate message.
    let ordinal = notice.attempts.last().map(|a| a.ordinal).unwrap_or(0);
    let idempotency_key = format!("escalation:{agent_external}:{ordinal}");

    if let Some(supervisor) = &policy.supervisor {
        if let Some(record) = store.get_agent(&tenant.scope(supervisor)).await? {
            if record
                .manifest
                .accepts_kind(ESCALATION_MESSAGE_KIND)
                .is_some()
            {
                let task = TaskRecord::new(
                    tasks::NewTask {
                        task_id: uuid::Uuid::new_v4().to_string(),
                        tenant: tenant.tenant().to_string(),
                        kind: ESCALATION_MESSAGE_KIND.to_string(),
                        payload,
                        // Pool is meaningless for mailbox traffic; the
                        // record requires one, so it carries the default.
                        pool: tasks::DEFAULT_POOL.to_string(),
                        recipient: Some(AgentId::new(supervisor).mailbox_recipient()),
                        max_attempts: tasks::DEFAULT_MAX_ATTEMPTS,
                        idempotency_key: Some(idempotency_key),
                        effect: None,
                        run_id: None,
                        thread_id: None,
                        deadline: None,
                        worker_version: None,
                        parent: None,
                        parent_task_id: None,
                        stage: 0,
                        status_category: crate::tasks::StatusCategory::Todo,
                    },
                    now,
                );
                let (task, deduplicated) = store.enqueue_task(&task).await?;
                return Ok(EscalationDelivery::Mailbox {
                    task_id: task.task_id,
                    deduplicated,
                });
            }
        }
    }

    // Root escalation (or an undeliverable one): the notice dead-letters
    // with the full evidence attached — the operator's `GET
    // /tasks?status=dead` is the surface, per the design's "the root's
    // escalation lands in the DLQ for an operator with the full evidence
    // chain attached". Runtime-internal, so it bypasses the submission
    // quota the way the outbox relay does: escalation is evidence, and
    // evidence must not be dropped under pressure.
    let mut task = TaskRecord::new(
        tasks::NewTask {
            task_id: uuid::Uuid::new_v4().to_string(),
            tenant: tenant.tenant().to_string(),
            kind: ESCALATION_MESSAGE_KIND.to_string(),
            payload,
            pool: tasks::DEFAULT_POOL.to_string(),
            recipient: None,
            max_attempts: tasks::DEFAULT_MAX_ATTEMPTS,
            idempotency_key: Some(idempotency_key),
            effect: None,
            run_id: None,
            thread_id: None,
            deadline: None,
            worker_version: None,
            parent: None,
            parent_task_id: None,
            stage: 0,
            status_category: crate::tasks::StatusCategory::Todo,
        },
        now,
    );
    task.status = TaskStatus::Dead;
    task.last_error = Some(
        "escalation dead-lettered: no supervisor declared, or none accepting `escalated`"
            .to_string(),
    );
    let (task, _) = store.dead_letter_task(&task).await?;
    Ok(EscalationDelivery::DeadLetter {
        task_id: task.task_id,
    })
}

/// What a supervision step did, surfaced for route responses and logs.
#[derive(Debug, Clone)]
pub(crate) struct SupervisionOutcome {
    /// The decision that was applied.
    pub decision: Decision,
    /// The journaled `SupervisionEvent` id, when a decision was journaled
    /// (`None` for `Unsupervised` / `Suppressed` — nothing new happened).
    pub event_id: Option<String>,
    /// Where the escalation landed, when the decision was `Escalate`.
    pub delivery: Option<EscalationDelivery>,
}

/// Run one supervision step for `agent` (already fetched, tenant-scoped):
/// decide, record the attempt, persist the record, journal the decision,
/// and deliver the escalation when the budget is exhausted.
///
/// Ordering: the *triggering fact* (the task settlement, the cancel tree)
/// is already durable before this runs — this function's own order is
/// state record → journal → escalation message, so a crash mid-step can
/// lose at most the escalation submission, which the idempotency key makes
/// retry-safe on the next trigger. The latches are set on the record
/// *before* delivery, so a crash cannot double-escalate either.
pub(crate) async fn supervise(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    agent_external: &str,
    mut agent: AgentRecord,
    trigger: Trigger,
    now: DateTime<Utc>,
) -> StoreResult<SupervisionOutcome> {
    let policy = agent.manifest.supervision.clone();
    let decision = decide(
        policy.as_ref(),
        &agent.supervision.attempts,
        agent.supervision.escalated,
        &trigger,
        now,
    );
    match decision {
        Decision::Unsupervised => Ok(SupervisionOutcome {
            decision,
            event_id: None,
            delivery: None,
        }),
        Decision::Suppressed => {
            // Counted, not appended: the escalation already carries the
            // history that matters, and an unbounded evidence log is its
            // own failure mode.
            agent.supervision.suppressed_failures += 1;
            store.update_agent(&agent).await?;
            Ok(SupervisionOutcome {
                decision,
                event_id: None,
                delivery: None,
            })
        }
        Decision::Restart { ordinal } => {
            let manual = matches!(trigger, Trigger::ManualRestart { .. });
            agent.supervision.attempts.push(SupervisionAttempt {
                ordinal,
                trigger: trigger.kind(),
                error_class: trigger.error_class(),
                message: trigger.message(),
                task_id: trigger.task_id(),
                at: now,
            });
            // The operator's reset clears both latches: the escalation (if
            // any) was handled by a human, the deadline (if breached) was
            // acknowledged. This is OTP's "operator fixed the child" path.
            if manual {
                agent.supervision.escalated = false;
                agent.supervision.deadline_breached = false;
                agent.supervision.suppressed_failures = 0;
            }
            store.update_agent(&agent).await?;
            let event_id = journal_event(
                store,
                tenant,
                agent_external,
                RunEventKind::SupervisionEvent,
                supervision_payload(
                    policy.as_ref(),
                    &trigger,
                    "restart",
                    ordinal,
                    &agent.supervision.attempts,
                ),
            )
            .await?;
            Ok(SupervisionOutcome {
                decision,
                event_id: Some(event_id),
                delivery: None,
            })
        }
        Decision::Escalate => {
            let policy = policy.expect("an escalation decision requires a declared policy");
            let ordinal = agent.supervision.next_ordinal();
            agent.supervision.attempts.push(SupervisionAttempt {
                ordinal,
                trigger: trigger.kind(),
                error_class: trigger.error_class(),
                message: trigger.message(),
                task_id: trigger.task_id(),
                at: now,
            });
            // Latch *before* delivery: a crash after this point cannot
            // re-escalate; the next trigger sees the latch and stops.
            agent.supervision.escalated = true;
            store.update_agent(&agent).await?;
            let event_id = journal_event(
                store,
                tenant,
                agent_external,
                RunEventKind::SupervisionEvent,
                supervision_payload(
                    Some(&policy),
                    &trigger,
                    "escalate",
                    ordinal,
                    &agent.supervision.attempts,
                ),
            )
            .await?;
            let delivery = deliver_escalation(
                store,
                tenant,
                agent_external,
                &policy,
                agent.supervision.attempts.clone(),
                now,
            )
            .await?;
            match &delivery {
                EscalationDelivery::Mailbox {
                    task_id,
                    deduplicated,
                } => tracing::info!(
                    agent = %agent_external,
                    %task_id,
                    %deduplicated,
                    "supervision escalated to the supervisor's mailbox"
                ),
                EscalationDelivery::DeadLetter { task_id } => tracing::warn!(
                    agent = %agent_external,
                    %task_id,
                    "supervision escalation dead-lettered (root or undeliverable)"
                ),
            }
            Ok(SupervisionOutcome {
                decision,
                event_id: Some(event_id),
                delivery: Some(delivery),
            })
        }
    }
}

/// Handle the agent-level deadline breach (the claim-path trigger):
/// latch the breach, cancel the agent's outstanding mailbox traffic —
/// children before parent, the cancellation tree's order — then let the
/// declared policy decide restart vs escalate over the wreckage.
///
/// Runs at most once per supervision episode (`deadline_breached`); the
/// caller checks the latch before invoking. Returns the cancellation
/// outcome plus the supervision outcome for the route's response.
pub(crate) async fn on_deadline_breach(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    agent_external: &str,
    mut agent: AgentRecord,
    now: DateTime<Utc>,
) -> StoreResult<(tasks::RunCancellation, SupervisionOutcome)> {
    agent.supervision.deadline_breached = true;
    store.update_agent(&agent).await?;
    let recipient = AgentId::new(agent_external).mailbox_recipient();
    let cancellation = store
        .cancel_agent_tasks(tenant.tenant(), &recipient, now)
        .await?;
    let outcome = supervise(
        store,
        tenant,
        agent_external,
        agent,
        Trigger::DeadlineBreached,
        now,
    )
    .await?;
    Ok((cancellation, outcome))
}

/// Journal an agent's cancellation as an `AgentExit` event in its
/// supervision journal (the wave-2 use of the W1b-inert variant): output
/// carries the terminal disposition and what the cancellation tree
/// touched — the mailbox tasks finalized, the lease holders signalled,
/// and the runs cancelled through `RunConfig::cancellation`.
///
/// Called only when the cancellation actually touched something; a
/// repeated cancel of an already-quiescent agent is a no-op and journals
/// nothing (an `AgentExit` per redundant cancel would be noise, not
/// evidence).
pub(crate) async fn journal_agent_exit(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    agent_external: &str,
    disposition: &str,
    detail: Value,
) -> StoreResult<String> {
    journal_event(
        store,
        tenant,
        agent_external,
        RunEventKind::AgentExit,
        json!({
            "disposition": disposition,
            "detail": detail,
        }),
    )
    .await
}

/// Read back an agent's supervision evidence for
/// `GET /agents/{id}/supervision`: the journaled events, integrity
/// re-verified on read exactly like the Flight Recorder endpoints —
/// tampered evidence is an error, never served as fact.
pub(crate) async fn supervision_events(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    agent_external: &str,
) -> StoreResult<Vec<RunEvent>> {
    let run_id = supervision_journal_run_id(tenant.tenant(), agent_external);
    let Some(snapshot) = store.get_journal(&run_id).await? else {
        return Ok(Vec::new());
    };
    let journal = Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
        format!("supervision journal for agent `{agent_external}` failed its integrity check: {e}")
    })?;
    Ok(journal.events())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::agents::RestartPolicy;

    fn policy(restart: RestartPolicy, intensity: u32) -> SupervisionPolicy {
        SupervisionPolicy::new(restart, intensity, 60_000)
    }

    fn failed(message: &str) -> Trigger {
        Trigger::TurnFailed {
            error_class: ErrorClass::Transient,
            message: message.to_string(),
            task_id: "task-1".to_string(),
        }
    }

    fn attempt(ordinal: u32, at: DateTime<Utc>) -> SupervisionAttempt {
        SupervisionAttempt {
            ordinal,
            trigger: SupervisionTrigger::TurnFailed,
            error_class: Some(ErrorClass::Transient),
            message: format!("failure {ordinal}"),
            task_id: Some("task-1".to_string()),
            at,
        }
    }

    #[test]
    fn unsupervised_agents_are_unmanaged() {
        let now = Utc::now();
        assert_eq!(
            decide(None, &[], false, &failed("boom"), now),
            Decision::Unsupervised
        );
        // Even a cancellation-class failure takes no supervision path.
        let cancelled = Trigger::TurnFailed {
            error_class: ErrorClass::Cancelled,
            message: "drained".to_string(),
            task_id: "t".to_string(),
        };
        assert_eq!(
            decide(None, &[], false, &cancelled, now),
            Decision::Unsupervised
        );
    }

    #[test]
    fn permanent_restarts_until_the_intensity_is_exceeded_then_escalates() {
        let now = Utc::now();
        let p = policy(RestartPolicy::Permanent, 2);
        // Attempts 1 and 2 restart; the third failure inside the window
        // exceeds intensity 2 and escalates.
        assert_eq!(
            decide(Some(&p), &[], false, &failed("1"), now),
            Decision::Restart { ordinal: 1 }
        );
        let history: Vec<_> = (1..=2).map(|o| attempt(o, now)).collect();
        assert_eq!(
            decide(Some(&p), &history, false, &failed("3"), now),
            Decision::Escalate
        );
    }

    #[test]
    fn attempts_outside_the_period_do_not_consume_the_budget() {
        let now = Utc::now();
        let p = SupervisionPolicy::new(RestartPolicy::Permanent, 1, 60_000);
        // One failure 10 minutes ago — outside the 60 s window — so this
        // failure restarts instead of escalating.
        let old = vec![attempt(1, now - Duration::minutes(10))];
        assert_eq!(
            decide(Some(&p), &old, false, &failed("recent"), now),
            Decision::Restart { ordinal: 2 }
        );
        // The same failure one period ago exactly is inside the window.
        let edge = vec![attempt(1, now - Duration::seconds(59))];
        assert_eq!(
            decide(Some(&p), &edge, false, &failed("recent"), now),
            Decision::Escalate
        );
    }

    #[test]
    fn transient_restarts_failures_but_escalates_cancellations() {
        let now = Utc::now();
        let p = policy(RestartPolicy::Transient, 3);
        assert!(matches!(
            decide(Some(&p), &[], false, &failed("boom"), now),
            Decision::Restart { .. }
        ));
        // A cancelled turn (operator or drain) is control flow, not a
        // crash — OTP's transient rule.
        let cancelled = Trigger::TurnFailed {
            error_class: ErrorClass::Cancelled,
            message: "cancelled".to_string(),
            task_id: "t".to_string(),
        };
        assert_eq!(
            decide(Some(&p), &[], false, &cancelled, now),
            Decision::Escalate
        );
        // A deadline breach is cancellation by clock: same rule.
        assert_eq!(
            decide(Some(&p), &[], false, &Trigger::DeadlineBreached, now),
            Decision::Escalate
        );
    }

    #[test]
    fn temporary_escalates_the_first_failure() {
        let now = Utc::now();
        let p = policy(RestartPolicy::Temporary, 5);
        assert_eq!(
            decide(Some(&p), &[], false, &failed("boom"), now),
            Decision::Escalate
        );
    }

    #[test]
    fn the_escalation_latch_suppresses_further_action() {
        let now = Utc::now();
        let p = policy(RestartPolicy::Permanent, 5);
        assert_eq!(
            decide(Some(&p), &[], true, &failed("still broken"), now),
            Decision::Suppressed
        );
        // Even a deadline breach is swallowed while latched.
        assert_eq!(
            decide(Some(&p), &[], true, &Trigger::DeadlineBreached, now),
            Decision::Suppressed
        );
    }

    #[test]
    fn manual_restart_always_restarts_and_needs_no_policy() {
        let now = Utc::now();
        let manual = Trigger::ManualRestart {
            reason: "operator reset".to_string(),
        };
        // No policy at all: the operator outranks the declaration.
        assert_eq!(
            decide(None, &[], true, &manual, now),
            Decision::Restart { ordinal: 1 }
        );
        // Latched escalation: cleared by the reset (the route clears the
        // latch; the decision is a restart).
        let p = policy(RestartPolicy::Temporary, 0);
        assert_eq!(
            decide(Some(&p), &[], true, &manual, now),
            Decision::Restart { ordinal: 1 }
        );
    }

    #[test]
    fn manual_restarts_in_history_do_not_consume_the_budget() {
        let now = Utc::now();
        let p = SupervisionPolicy::new(RestartPolicy::Permanent, 1, 60_000);
        let mut history = vec![attempt(1, now)];
        history.push(SupervisionAttempt {
            ordinal: 2,
            trigger: SupervisionTrigger::ManualRestart,
            error_class: None,
            message: "operator reset".to_string(),
            task_id: None,
            at: now,
        });
        // One failure + one manual restart in the window: intensity 1 is
        // consumed by the failure, but the manual restart is not a crash —
        // wait: one failure already restarts at intensity 1; the NEXT
        // failure escalates. The manual entry must not count.
        assert_eq!(
            decide(Some(&p), &history, false, &failed("second crash"), now),
            Decision::Escalate
        );
        let only_manual = vec![history[1].clone()];
        assert_eq!(
            decide(Some(&p), &only_manual, false, &failed("first crash"), now),
            Decision::Restart { ordinal: 2 }
        );
    }
}
