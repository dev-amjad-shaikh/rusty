//! Coordination patterns runtime (R0.7 wave 3): the typed patterns on the
//! wave-1 substrate, outbox-submitted, with journaled evidence.
//!
//! The four typed contracts live in core ([`CoordinationContract`]); this
//! module is the runtime that drives them. The shape deliberately mirrors
//! [`crate::supervision`]: one convergent driver ([`drive`]) that rebuilds
//! all progress from the durable evidence — the coordination journal and
//! the member task records — and commits each pass as one unit. Nothing
//! the pattern knows lives only in memory, so a server crash anywhere in
//! the pipeline leaves the next drive to finish the work from the same
//! facts.
//!
//! The convergence argument, per drive:
//!
//! - **The journal is the latch book.** `CoordinationStart`, each member's
//!   `MailboxSend`, each settlement observation (`MailboxReceive`), and
//!   `CoordinationEnd` are scanned back out of the integrity-verified
//!   journal itself — never trusted from the record. What is journaled is
//!   not re-journaled; what is not is re-appended.
//! - **Member tasks are deterministic.** Member task ids
//!   (`{tenant}--{cid}--{member}`) and idempotency keys
//!   (`coordination:{cid}:{member}`) are derived, not minted, so a retried
//!   submission converges on the same task instead of forking duplicates —
//!   and the outbox dedupes a re-push by task id.
//! - **One commit point.** Each drive ends in
//!   [`ServerStore::journal_and_enqueue`] (journal + outbox rows as one
//!   unit) followed by the record update — evidence first, latches second.
//!   A crash between them is harmless: the next drive's journal scan
//!   rebuilds every latch the record lost.
//!
//! Member settlement drives the pattern: the complete / fail / cancel
//! routes call [`on_task_settled`] after the settlement is durable (the
//! supervision precedent — the hook composes after durability, never
//! inside the lease guard). Claim-path finalizations (a member's deadline
//! expiring unclaimed) have no route hook, so `GET /coordination/{id}`
//! reconciles on read: it runs the drive before answering. That impurity
//! is documented and convergent — a read that only observes changes
//! nothing.
//!
//! Crash semantics inherited from R0.6, stated honestly:
//!
//! - A leased loser that ignores its cancel hint and completes anyway does
//!   not change the settled outcome: the pattern's result is computed from
//!   the evidence at settle time, and cancellation is a promptness hint,
//!   not a guarantee.
//! - On the JSON-file backend a crash between the outbox write and the
//!   journal write can leave a task whose `MailboxSend` event re-journals
//!   with a new sequence id on the next drive — evidence imperfection in
//!   the crash window, the same honesty class
//!   [`ServerStore::checkpoint_and_enqueue`] documents. On Postgres the
//!   pair commits atomically.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::agents::{
    AgentId, CoordinationContract, CoordinationMessage, CoordinationOutcome, CoordinationStatus,
    MemberDisposition, MemberSettlement, QuorumOutcome, QuorumResolverRecord,
    COORDINATION_RESULT_KIND,
};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::record::{Effect, EventStatus, PayloadRef, RunEventKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::server_store::{ServerStore, StoreResult};
use crate::tasks::{self, TaskRecord};
use crate::TaskQuota;

/// The deterministic run id of a coordination's journal. Distinct from
/// executor run ids (UUIDs) and from the supervision journals by
/// construction, tenant-unique, and free of `/` so the JSON-file layout
/// keeps one file per journal (the `agent-supervision:` convention).
pub(crate) fn coordination_journal_run_id(tenant: &str, coordination_external: &str) -> String {
    format!("coordination:{tenant}:{coordination_external}")
}

/// The deterministic task id of one member's work. Path-safe (no `/` —
/// task files are a flat directory) and derived, not minted: a retried
/// drive re-derives the same id, and the outbox dedupes on it.
pub(crate) fn member_task_id(tenant: &str, coordination_external: &str, member: &str) -> String {
    format!("{tenant}--{coordination_external}--{member}")
}

/// The deterministic task id of the delegator-facing outcome message.
pub(crate) fn outcome_task_id(tenant: &str, coordination_external: &str) -> String {
    format!("{tenant}--{coordination_external}--outcome")
}

/// The deterministic task id of a fully-failed race's dead-letter entry.
/// Tenant-prefixed like every derived id — two tenants running the same
/// external coordination id must never collide in the shared task index.
pub(crate) fn race_dlq_task_id(tenant: &str, coordination_external: &str) -> String {
    format!("{tenant}--{coordination_external}--race-dlq")
}

/// The runtime-derived idempotency key of a member task — deliberately
/// not caller-supplied (see [`rusty_agent_runtime::agents::Delegation`]).
pub(crate) fn member_idempotency_key(coordination_external: &str, member: &str) -> String {
    format!("coordination:{coordination_external}:{member}")
}

/// One member's runtime bookkeeping inside a [`CoordinationRecord`]. The
/// member's *evidence* (its settlement, result, cost) is NOT stored here —
/// dispositions are derived from the durable task record on every drive,
/// so a value stored on this record could never disagree with the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemberRecord {
    /// The member name from the contract.
    pub member: String,
    /// The target agent's external id.
    pub agent_id: String,
    /// The pinned manifest version (submission-time validation made it
    /// exact; it is carried for the record, not re-checked on drive).
    pub manifest_version: String,
    /// The deterministic member task id.
    pub task_id: String,
    /// Latch: the member's task reached the outbox. Rebuilt from the
    /// journal's send events on every drive — an optimization, never the
    /// source of truth.
    #[serde(default)]
    pub submitted: bool,
}

/// The durable record of one coordination (R0.7 wave 3). One JSON file per
/// record under `{store_path}/coordinations/{tenant}/{id}.json` on the
/// default backend, or the `server_coordinations` table (payload JSONB)
/// with the `postgres` feature — the `server_agents` discipline.
///
/// The latches (`settled`, `outcome_delivered`, `dlq_written`,
/// `members[].submitted`) exist to short-circuit finished work, not to
/// carry truth: every one of them is rebuilt from the journal and the
/// member task records on the next drive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CoordinationRecord {
    /// Tenant-scoped id (`{tenant}/{external}` — the agents convention).
    pub coordination_id: String,
    /// The delegating agent's external id. `None` = a control-plane
    /// submission observed through `GET /coordination/{id}` alone; no
    /// outcome message is delivered.
    #[serde(default)]
    pub delegator: Option<String>,
    /// The causal parent event id, when this pattern was itself spawned by
    /// a journaled step (a delegator's turn, an outer pattern). Stitched
    /// into the team trace.
    #[serde(default)]
    pub parent: Option<String>,
    /// The typed pattern declaration, exactly as submitted and validated.
    pub contract: CoordinationContract,
    /// Member bookkeeping, in contract declaration order.
    pub members: Vec<MemberRecord>,
    /// Latch: the pattern settled (`CoordinationEnd` journaled).
    #[serde(default)]
    pub settled: bool,
    /// The settled outcome — also the `CoordinationEnd` event's output and
    /// the `coordination_result` message's payload. One fact, three views.
    #[serde(default)]
    pub outcome: Option<CoordinationOutcome>,
    /// Latch: the outcome message reached the outbox (or no delegator
    /// exists to receive one). "Delivered" means durably submitted to the
    /// delegator's mailbox, not consumed — consumption is the delegator's
    /// turn protocol's business.
    #[serde(default)]
    pub outcome_delivered: bool,
    /// Latch: the pattern's DLQ obligation (a race whose candidates all
    /// failed) is discharged — or never applied to this pattern.
    #[serde(default)]
    pub dlq_written: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CoordinationRecord {
    /// The external (unscoped) coordination id — the form task ids,
    /// journal run ids, and the outcome's `coordination_id` field use.
    pub(crate) fn external(&self, tenant: &TenantContext) -> String {
        self.coordination_id
            .strip_prefix(&format!("{}/", tenant.tenant()))
            .unwrap_or(&self.coordination_id)
            .to_string()
    }
}

/// The result of one [`drive`] pass: the updated record plus the current
/// member dispositions (terminal members only, unless the pattern settled
/// — then every member's final evidence, in contract order). The read
/// model for `GET /coordination/{id}`.
pub(crate) struct DriveOutcome {
    pub record: CoordinationRecord,
    pub dispositions: Vec<MemberDisposition>,
}

/// One member's live state inside a drive: its task record (when the
/// queue knows it) and its derived terminal disposition (when settled).
struct MemberState {
    task: Option<TaskRecord>,
    disposition: Option<MemberDisposition>,
}

/// The settle decision, computed once per drive from the member states.
/// Computing it as one pure-ish decision (instead of settling inline per
/// pattern) keeps the four patterns' end-games inspectable in one place.
struct SettlePlan {
    status: CoordinationStatus,
    result: Option<PayloadRef>,
    resolver: Option<QuorumResolverRecord>,
    /// Members whose work *contributed* to the outcome (the delegate, the
    /// merged fan-out members, the race winner, the accepted quorum
    /// members). Every other member's reported cost is waste, and every
    /// non-terminal one is cancel-signalled.
    contributing: HashSet<String>,
    /// A race whose candidates all failed dead-letters its outcome for an
    /// operator (the supervision root-escalation precedent).
    needs_dlq: bool,
}

/// Drive one coordination forward: journal what is missing, submit what is
/// due, settle what is decided — then commit ([`ServerStore::journal_and_enqueue`])
/// and latch. Idempotent and convergent by construction; see the module
/// docs for the argument.
pub(crate) async fn drive(
    store: &Arc<dyn ServerStore>,
    quota: &TaskQuota,
    tenant: &TenantContext,
    mut record: CoordinationRecord,
    now: DateTime<Utc>,
) -> StoreResult<DriveOutcome> {
    let external = record.external(tenant);

    // Terminal fast path: nothing left to do, ever. Dispositions are still
    // derived fresh — the read model never goes stale.
    if record.settled && record.outcome_delivered && record.dlq_written {
        let dispositions = match &record.outcome {
            Some(outcome) => outcome.members.clone(),
            None => current_dispositions(store, tenant, &record).await?,
        };
        return Ok(DriveOutcome {
            record,
            dispositions,
        });
    }

    // The journal is rebuilt from its persisted snapshot through
    // `Journal::from_snapshot` — the Flight Recorder's integrity check —
    // so a tampered coordination journal fails the drive rather than
    // silently forking the evidence chain (the supervision discipline).
    let run_id = coordination_journal_run_id(tenant.tenant(), &external);
    let journal = match store.get_journal(&run_id).await? {
        Some(snapshot) => Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
            format!("coordination journal `{run_id}` failed its integrity check: {e}")
        })?,
        None => Journal::new(run_id.clone(), run_id.clone(), Clock::System),
    };

    // Scan the latch book: what the journal already knows. Events carry
    // their member identity in the output payload, resolved through the
    // journal (artifact-spilled payloads included).
    let events = journal.events();
    let mut start_id: Option<String> = None;
    let mut send_ids: HashMap<String, String> = HashMap::new();
    let mut settlement_done: HashSet<String> = HashSet::new();
    let mut end_done = false;
    for event in &events {
        let member = event
            .output
            .as_ref()
            .and_then(|payload| journal.resolve(payload))
            .and_then(|output| output.get("member")?.as_str().map(str::to_string));
        match event.kind {
            RunEventKind::CoordinationStart => start_id = Some(event.id.clone()),
            RunEventKind::MailboxSend => {
                if let Some(member) = member {
                    send_ids.insert(member, event.id.clone());
                }
            }
            RunEventKind::MailboxReceive => {
                if let Some(member) = member {
                    settlement_done.insert(member);
                }
            }
            RunEventKind::CoordinationEnd => end_done = true,
            _ => {}
        }
    }

    let mut pending_tasks: Vec<TaskRecord> = Vec::new();
    let mut dirty = false;

    // The causal root first — every event the pattern spawns parents onto
    // it, so it must exist before any send does.
    if start_id.is_none() {
        let output = json!({
            "coordination_id": external,
            "delegator": record.delegator,
            "parent": record.parent,
            "contract": record.contract,
        });
        let mut draft =
            EventDraft::new(RunEventKind::CoordinationStart, Effect::Pure).output(output);
        if let Some(parent) = &record.parent {
            draft = draft.parent(parent.clone());
        }
        start_id = Some(journal.record(draft));
        dirty = true;
    }
    let start_id = start_id.expect("start event exists by construction");

    // Refetch the member tasks and derive their dispositions from the
    // queue's own records — the only settlement evidence the pattern
    // trusts.
    let mut states: Vec<MemberState> = Vec::with_capacity(record.members.len());
    for member in &mut record.members {
        let task = store.get_task(tenant.tenant(), &member.task_id).await?;
        // The submitted latch mirrors reality: a task the queue knows, or
        // a send the journal knows (the task is then a pending outbox row,
        // not yet visible to `get_task`).
        member.submitted = task.is_some() || send_ids.contains_key(&member.member);
        let disposition = task.as_ref().and_then(|task| disposition_of(member, task));
        states.push(MemberState { task, disposition });
    }

    if !end_done {
        // Windowed submission. The window is the fan-out's backpressure
        // guarantee: `in_flight` counts members whose work is submitted but
        // not terminally settled (including sends whose tasks are still
        // pending outbox rows — invisible to `get_task` but real work).
        let window = match &record.contract {
            CoordinationContract::Delegate(_)
            | CoordinationContract::Race(_)
            | CoordinationContract::Quorum(_) => u32::MAX,
            CoordinationContract::FanOut(contract) => contract.max_in_flight,
        };
        let mut in_flight = record
            .members
            .iter()
            .zip(states.iter())
            .filter(|(member, state)| member.submitted && state.disposition.is_none())
            .count() as u32;
        for member in record.members.iter_mut() {
            if member.submitted || in_flight >= window {
                continue;
            }
            // The quota is a gate, not an error, inside the drive: over
            // quota simply defers the remaining submissions to a later
            // drive (the pattern stays open; reconciled on read or on the
            // next settlement). Submissions at pattern creation are
            // pre-checked by the route with a 429.
            if !submission_fits_quota(store, quota, tenant, pending_tasks.len()).await? {
                break;
            }
            let delegation = contract_delegation(&record.contract, &member.member)
                .expect("member names come from the contract");
            // The target was validated at submission; a vanished record
            // here is corruption, answered as a store error.
            let agent = store
                .get_agent(&tenant.scope(&member.agent_id))
                .await?
                .ok_or_else(|| {
                    format!(
                        "coordination member `{}` target agent `{}` is no longer registered",
                        member.member, member.agent_id
                    )
                })?;
            // The member deadline composes with the agent's budget
            // deadline — the earlier bound wins, exactly like direct
            // mailbox sends.
            let deadline = match (
                delegation.deadline,
                agent.manifest.budget.and_then(|budget| budget.deadline),
            ) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let recipient = AgentId::new(member.agent_id.as_str()).mailbox_recipient();
            let send_id = journal.record(
                EventDraft::new(RunEventKind::MailboxSend, Effect::Pure)
                    .output(json!({
                        "coordination_id": external,
                        "member": member.member,
                        "task_id": member.task_id,
                        "agent_id": member.agent_id,
                        "manifest_version": member.manifest_version,
                        "kind": delegation.kind,
                        "recipient": recipient,
                    }))
                    .parent(start_id.clone()),
            );
            dirty = true;
            let message = CoordinationMessage {
                coordination_id: external.clone(),
                member: member.member.clone(),
                pattern: record.contract.kind(),
                input: delegation.input.clone(),
                context: match &record.contract {
                    CoordinationContract::Delegate(contract) => contract.context.clone(),
                    _ => None,
                },
            };
            pending_tasks.push(TaskRecord::new(
                tasks::NewTask {
                    task_id: member.task_id.clone(),
                    tenant: tenant.tenant().to_string(),
                    kind: delegation.kind.clone(),
                    payload: serde_json::to_value(&message)
                        .map_err(|e| format!("serialize coordination message: {e}"))?,
                    // Pool is meaningless for mailbox traffic; the record
                    // requires one, so it carries the default (the
                    // supervision convention).
                    pool: tasks::DEFAULT_POOL.to_string(),
                    recipient: Some(recipient),
                    max_attempts: tasks::DEFAULT_MAX_ATTEMPTS,
                    idempotency_key: Some(member_idempotency_key(&external, &member.member)),
                    effect: Some(delegation.effect),
                    run_id: None,
                    thread_id: None,
                    deadline,
                    worker_version: None,
                    parent: Some(send_id),
                },
                now,
            ));
            member.submitted = true;
            in_flight += 1;
        }

        // Settlement observations: every newly terminal member gets its
        // `MailboxReceive` — the recipient-side half of the mailbox pair,
        // journaled as the pattern's settlement evidence (failed members
        // with `status: error`).
        for (member, state) in record.members.iter().zip(states.iter()) {
            let Some(disposition) = &state.disposition else {
                continue;
            };
            if settlement_done.contains(&member.member) {
                continue;
            }
            let parent = send_ids
                .get(&member.member)
                .cloned()
                .unwrap_or_else(|| start_id.clone());
            journal.record(
                EventDraft::new(RunEventKind::MailboxReceive, Effect::Pure)
                    .status(if disposition.settlement == MemberSettlement::Completed {
                        EventStatus::Ok
                    } else {
                        EventStatus::Error
                    })
                    .output(settlement_json(&external, disposition))
                    .parent(parent),
            );
            settlement_done.insert(member.member.clone());
            dirty = true;
        }

        // The settle decision, computed once from the member states.
        if let Some(plan) = decide_settle(&record, &states) {
            settle(
                store,
                tenant,
                &journal,
                &mut record,
                &mut states,
                &mut pending_tasks,
                &mut settlement_done,
                &plan,
                &external,
                &start_id,
                now,
            )
            .await?;
            dirty = true;
        }
    }

    // One commit point: evidence + work, then the latches. See the module
    // docs for why this order is crash-safe.
    if dirty || !pending_tasks.is_empty() {
        store
            .journal_and_enqueue(&journal.snapshot(), &pending_tasks)
            .await?;
    }
    record.updated_at = now;
    store.update_coordination(&record).await?;

    let dispositions = match &record.outcome {
        Some(outcome) => outcome.members.clone(),
        None => states
            .into_iter()
            .filter_map(|state| state.disposition)
            .collect(),
    };
    Ok(DriveOutcome {
        record,
        dispositions,
    })
}

/// The settle half of the drive, split out so `drive` reads as the linear
/// pipeline it is. Cancels non-contributing members, journals the end,
/// delivers the outcome, and discharges the DLQ obligation.
#[allow(clippy::too_many_arguments)]
async fn settle(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    journal: &Journal,
    record: &mut CoordinationRecord,
    states: &mut [MemberState],
    pending_tasks: &mut Vec<TaskRecord>,
    settlement_done: &mut HashSet<String>,
    plan: &SettlePlan,
    external: &str,
    start_id: &str,
    now: DateTime<Utc>,
) -> StoreResult<()> {
    // Cancel-signal every non-contributing, non-terminal submitted member.
    // Cancellation is the R0.6 hint: a leased holder learns on its next
    // heartbeat; a queued task goes terminal immediately. The pattern does
    // not wait for either — the journaled disposition spells the signal as
    // `cancelled`, and a loser that completes anyway does not change the
    // settled outcome (documented in the module docs).
    for (member, state) in record.members.iter().zip(states.iter_mut()) {
        if plan.contributing.contains(&member.member) || state.disposition.is_some() {
            continue;
        }
        if member.submitted {
            // Terminal/unknown outcomes are both fine: the cancel is a
            // hint, and convergence never depends on it landing.
            let _ = store
                .cancel_task(tenant.tenant(), &member.task_id, now)
                .await;
        }
        let disposition = MemberDisposition {
            member: member.member.clone(),
            task_id: member.task_id.clone(),
            settlement: MemberSettlement::Cancelled,
            result: None,
            error_class: None,
            error: if member.submitted {
                None
            } else {
                Some(
                    "never submitted: the pattern settled before this member's window opened"
                        .into(),
                )
            },
            tokens: None,
            cost_usd: None,
        };
        if !settlement_done.contains(&member.member) {
            let parent = start_id.to_string();
            journal.record(
                EventDraft::new(RunEventKind::MailboxReceive, Effect::Pure)
                    .status(EventStatus::Error)
                    .output(settlement_json(external, &disposition))
                    .parent(parent),
            );
            settlement_done.insert(member.member.clone());
        }
        state.disposition = Some(disposition);
    }

    // The final dispositions: every member, in contract order — completed
    // contributors, failed members, cancelled losers. Missing members are
    // journaled, never silent.
    let dispositions: Vec<MemberDisposition> = record
        .members
        .iter()
        .zip(states.iter())
        .map(|(member, state)| {
            state.disposition.clone().unwrap_or(MemberDisposition {
                member: member.member.clone(),
                task_id: member.task_id.clone(),
                settlement: MemberSettlement::Cancelled,
                result: None,
                error_class: None,
                error: None,
                tokens: None,
                cost_usd: None,
            })
        })
        .collect();

    // Waste accounting: the cost of work the outcome discarded — race
    // losers, cancelled members, unaccepted quorum candidates. Reported
    // evidence only: members that reported nothing waste nothing.
    let wasted_tokens: u64 = dispositions
        .iter()
        .filter(|d| !plan.contributing.contains(&d.member))
        .filter_map(|d| d.tokens.map(|usage| usage.total_tokens))
        .sum();
    let wasted_cost: f64 = dispositions
        .iter()
        .filter(|d| !plan.contributing.contains(&d.member))
        .filter_map(|d| d.cost_usd)
        .sum();

    let outcome = CoordinationOutcome {
        coordination_id: external.to_string(),
        pattern: record.contract.kind(),
        status: plan.status,
        result: plan.result.clone(),
        members: dispositions,
        wasted_tokens: (wasted_tokens > 0).then_some(wasted_tokens),
        wasted_cost_usd: (wasted_cost > 0.0).then_some(wasted_cost),
        resolver: plan.resolver.clone(),
    };

    // The terminal fact, journaled exactly once (`drive` never calls here
    // when the end event already exists).
    let end_id = journal.record(
        EventDraft::new(RunEventKind::CoordinationEnd, Effect::Pure)
            .output(
                serde_json::to_value(&outcome)
                    .map_err(|e| format!("serialize coordination outcome: {e}"))?,
            )
            .parent(start_id.to_string()),
    );
    record.settled = true;
    record.outcome = Some(outcome);

    // The DLQ obligation: a race whose candidates all failed dead-letters
    // its outcome for an operator. Runtime-internal evidence, so it
    // bypasses the submission quota — the supervision root-escalation
    // precedent: evidence must not be dropped under pressure. Deduped by
    // the idempotency key, so a retried drive cannot double the entry.
    if plan.needs_dlq {
        let mut dlq = TaskRecord::new(
            tasks::NewTask {
                task_id: race_dlq_task_id(tenant.tenant(), external),
                tenant: tenant.tenant().to_string(),
                kind: COORDINATION_RESULT_KIND.to_string(),
                payload: serde_json::to_value(&record.outcome)
                    .map_err(|e| format!("serialize race outcome: {e}"))?,
                pool: tasks::DEFAULT_POOL.to_string(),
                recipient: None,
                max_attempts: tasks::DEFAULT_MAX_ATTEMPTS,
                idempotency_key: Some(format!("coordination:{external}:race-dlq")),
                effect: None,
                run_id: None,
                thread_id: None,
                deadline: None,
                worker_version: None,
                parent: Some(end_id.clone()),
            },
            now,
        );
        dlq.status = tasks::TaskStatus::Dead;
        dlq.last_error = Some(
            "race dead-lettered: every candidate failed; the outcome carries the evidence".into(),
        );
        store.dead_letter_task(&dlq).await?;
    }
    record.dlq_written = true;

    // Outcome delivery: one `coordination_result` message to the
    // delegator's mailbox, correlated by its deterministic task id.
    if let Some(delegator) = &record.delegator {
        let delegator_record = store
            .get_agent(&tenant.scope(delegator))
            .await?
            .ok_or_else(|| {
                format!("coordination delegator `{delegator}` is no longer registered")
            })?;
        let outcome_payload = serde_json::to_value(&record.outcome)
            .map_err(|e| format!("serialize coordination outcome: {e}"))?;
        pending_tasks.push(TaskRecord::new(
            tasks::NewTask {
                task_id: outcome_task_id(tenant.tenant(), external),
                tenant: tenant.tenant().to_string(),
                kind: COORDINATION_RESULT_KIND.to_string(),
                payload: outcome_payload,
                pool: tasks::DEFAULT_POOL.to_string(),
                recipient: Some(AgentId::new(delegator.as_str()).mailbox_recipient()),
                max_attempts: tasks::DEFAULT_MAX_ATTEMPTS,
                idempotency_key: Some(format!("coordination:{external}:outcome")),
                effect: None,
                run_id: None,
                thread_id: None,
                deadline: delegator_record
                    .manifest
                    .budget
                    .and_then(|budget| budget.deadline),
                worker_version: None,
                parent: Some(end_id),
            },
            now,
        ));
    }
    record.outcome_delivered = true;
    Ok(())
}

/// The settle decision for the record's current member states — `None`
/// when the pattern must wait for more evidence. One decision point for
/// all four patterns, so their end-games are inspectable side by side.
fn decide_settle(record: &CoordinationRecord, states: &[MemberState]) -> Option<SettlePlan> {
    let all_submitted = record.members.iter().all(|member| member.submitted);
    let terminal: Vec<&MemberDisposition> = states
        .iter()
        .filter_map(|state| state.disposition.as_ref())
        .collect();
    let completed: Vec<&MemberDisposition> = terminal
        .iter()
        .copied()
        .filter(|d| d.settlement == MemberSettlement::Completed)
        .collect();
    let all_terminal = all_submitted && terminal.len() == record.members.len();

    match &record.contract {
        CoordinationContract::Delegate(_) => {
            let disposition = states.first()?.disposition.as_ref()?;
            let (status, result) = match disposition.settlement {
                MemberSettlement::Completed => {
                    (CoordinationStatus::Completed, disposition.result.clone())
                }
                MemberSettlement::Cancelled => (CoordinationStatus::Cancelled, None),
                MemberSettlement::Failed | MemberSettlement::Dead => {
                    (CoordinationStatus::Failed, None)
                }
            };
            let contributing = if status == CoordinationStatus::Completed {
                HashSet::from([disposition.member.clone()])
            } else {
                HashSet::new()
            };
            Some(SettlePlan {
                status,
                result,
                resolver: None,
                contributing,
                needs_dlq: false,
            })
        }
        CoordinationContract::FanOut(contract) => {
            let failed = terminal.iter().any(|d| {
                matches!(
                    d.settlement,
                    MemberSettlement::Failed | MemberSettlement::Dead
                )
            });
            if contract.on_member_failure
                == rusty_agent_runtime::agents::MemberFailurePolicy::FailFast
                && failed
            {
                // Fail fast: the first terminal failure ends the pattern;
                // the remaining members are cancel-signalled.
                return Some(SettlePlan {
                    status: CoordinationStatus::Failed,
                    result: None,
                    resolver: None,
                    contributing: HashSet::new(),
                    needs_dlq: false,
                });
            }
            if !all_terminal {
                return None;
            }
            // The merge is byte-deterministic: completed members' results
            // ordered by task id, never by completion order. Missing
            // members are not in the array — they are in `members`, which
            // is where partial-failure evidence belongs.
            let results: Vec<(String, Value)> = completed
                .iter()
                .filter_map(|d| {
                    d.result.as_ref().and_then(|payload| match payload {
                        PayloadRef::Inline(value) => Some((d.task_id.clone(), value.clone())),
                        // Member results are journaled inline by
                        // construction (the complete route stores the raw
                        // value); an artifact ref here is unresolved
                        // evidence, skipped rather than panicked on.
                        PayloadRef::Artifact(_) => None,
                    })
                })
                .collect();
            Some(SettlePlan {
                status: CoordinationStatus::Completed,
                result: Some(PayloadRef::Inline(Value::Array(
                    rusty_agent_runtime::agents::merge_fan_out(&results),
                ))),
                resolver: None,
                contributing: completed.iter().map(|d| d.member.clone()).collect(),
                needs_dlq: false,
            })
        }
        CoordinationContract::Race(_) => {
            // The winner is the *earliest* completion by the task record's
            // own clock field (`updated_at` at settle) — deterministic
            // across drives because it is derived from durable records,
            // not observation order.
            let winner = completed
                .iter()
                .filter_map(|d| {
                    states
                        .iter()
                        .find(|state| {
                            state
                                .disposition
                                .as_ref()
                                .is_some_and(|x| x.member == d.member)
                        })
                        .and_then(|state| state.task.as_ref())
                        .map(|task| (task.updated_at, *d))
                })
                .min_by_key(|(settled_at, _)| *settled_at)
                .map(|(_, d)| d);
            if let Some(winner) = winner {
                return Some(SettlePlan {
                    status: CoordinationStatus::Completed,
                    result: winner.result.clone(),
                    resolver: None,
                    contributing: HashSet::from([winner.member.clone()]),
                    needs_dlq: false,
                });
            }
            if all_terminal {
                // Every candidate failed (or was cancelled): the pattern
                // fails, and its outcome dead-letters for an operator.
                return Some(SettlePlan {
                    status: CoordinationStatus::Failed,
                    result: None,
                    resolver: None,
                    contributing: HashSet::new(),
                    needs_dlq: true,
                });
            }
            None
        }
        CoordinationContract::Quorum(contract) => {
            let k = contract.threshold as usize;
            // Acceptance is completion-time order (the first k completions
            // by the records' own clock fields); the resolver's input order
            // is then re-derived deterministically by `resolve_quorum`
            // (task-id order), so replaying the same evidence always
            // reproduces the same resolution.
            let mut accepted: Vec<(DateTime<Utc>, &MemberDisposition)> = completed
                .iter()
                .filter_map(|d| {
                    states
                        .iter()
                        .find(|state| {
                            state
                                .disposition
                                .as_ref()
                                .is_some_and(|x| x.member == d.member)
                        })
                        .and_then(|state| state.task.as_ref())
                        .map(|task| (task.updated_at, *d))
                })
                .collect();
            accepted.sort_by_key(|(settled_at, _)| *settled_at);
            if accepted.len() >= k {
                let accepted = &accepted[..k];
                let inputs: Vec<(String, Value)> = accepted
                    .iter()
                    .filter_map(|(_, d)| {
                        d.result.as_ref().and_then(|payload| match payload {
                            PayloadRef::Inline(value) => Some((d.task_id.clone(), value.clone())),
                            PayloadRef::Artifact(_) => None,
                        })
                    })
                    .collect();
                let outcome =
                    rusty_agent_runtime::agents::resolve_quorum(&contract.resolver, &inputs)
                        .ok()?;
                let (resolver_output, decided, result) = match outcome {
                    QuorumOutcome::Decided { output } => {
                        (Some(output.clone()), true, Some(PayloadRef::Inline(output)))
                    }
                    QuorumOutcome::FirstK { outputs } => (
                        Some(Value::Array(outputs.clone())),
                        true,
                        Some(PayloadRef::Inline(Value::Array(outputs))),
                    ),
                    // No-majority is still a completed pattern: the vote
                    // ran, the evidence is the tallies — the outcome says
                    // `decided: false` instead of inventing a winner.
                    QuorumOutcome::NoMajority { .. } => (None, false, None),
                };
                let resolver_record = QuorumResolverRecord {
                    resolver: contract.resolver.clone(),
                    inputs: inputs.into_iter().map(|(_, output)| output).collect(),
                    output: resolver_output,
                    decided,
                };
                return Some(SettlePlan {
                    status: CoordinationStatus::Completed,
                    result,
                    resolver: Some(resolver_record),
                    contributing: accepted.iter().map(|(_, d)| d.member.clone()).collect(),
                    needs_dlq: false,
                });
            }
            // Reachability: members that can still complete are the
            // completed ones plus anything not terminally settled (queued,
            // leased, retry-scheduled, or not yet submitted). Below `k`
            // the threshold is unreachable — the pattern fails open with
            // the evidence journaled, and `k` is never silently downgraded.
            let impossible = terminal
                .iter()
                .filter(|d| d.settlement != MemberSettlement::Completed)
                .count();
            if record.members.len() - impossible < k {
                return Some(SettlePlan {
                    status: CoordinationStatus::Unreachable,
                    result: None,
                    resolver: None,
                    contributing: HashSet::new(),
                    needs_dlq: false,
                });
            }
            None
        }
    }
}

/// Derive a member's terminal disposition from its durable task record —
/// `None` while the task is open (queued, leased, or retry-scheduled).
fn disposition_of(member: &MemberRecord, task: &TaskRecord) -> Option<MemberDisposition> {
    let settlement = match task.status {
        tasks::TaskStatus::Completed => MemberSettlement::Completed,
        tasks::TaskStatus::Cancelled => MemberSettlement::Cancelled,
        tasks::TaskStatus::Dead => MemberSettlement::Dead,
        tasks::TaskStatus::Failed if task.next_attempt_at.is_none() => MemberSettlement::Failed,
        _ => return None,
    };
    Some(MemberDisposition {
        member: member.member.clone(),
        task_id: member.task_id.clone(),
        settlement,
        result: task.result.clone().map(PayloadRef::Inline),
        error_class: task.error_class,
        error: task.last_error.clone(),
        tokens: task.tokens,
        cost_usd: task.cost_usd,
    })
}

/// The member states recomputed for the terminal fast path — the read
/// model must not go stale just because the pattern is done.
async fn current_dispositions(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    record: &CoordinationRecord,
) -> StoreResult<Vec<MemberDisposition>> {
    let mut dispositions = Vec::new();
    for member in &record.members {
        if let Some(task) = store.get_task(tenant.tenant(), &member.task_id).await? {
            if let Some(disposition) = disposition_of(member, &task) {
                dispositions.push(disposition);
            }
        }
    }
    Ok(dispositions)
}

/// The output payload of a settlement observation (`MailboxReceive`): the
/// member's terminal evidence, one shape for all four settlements.
fn settlement_json(coordination_external: &str, disposition: &MemberDisposition) -> Value {
    json!({
        "coordination_id": coordination_external,
        "member": disposition.member,
        "task_id": disposition.task_id,
        "settlement": disposition.settlement,
        "result": disposition.result,
        "error_class": disposition.error_class,
        "error": disposition.error,
        "tokens": disposition.tokens,
        "cost_usd": disposition.cost_usd,
    })
}

/// The drive's quota gate: `true` when one more submission fits the
/// tenant's backlog and in-flight caps. Unlike the submission routes'
/// 429, an over-quota drive is not an error — the pattern waits.
async fn submission_fits_quota(
    store: &Arc<dyn ServerStore>,
    quota: &TaskQuota,
    tenant: &TenantContext,
    pending: usize,
) -> StoreResult<bool> {
    if quota.is_unlimited() {
        return Ok(true);
    }
    let usage = store.task_usage(tenant.tenant()).await?;
    if let Some(max) = quota.max_queued {
        if usage.queued as usize + pending + 1 > max {
            return Ok(false);
        }
    }
    if let Some(max) = quota.max_in_flight {
        if usage.in_flight as usize >= max {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The contract's delegation for one member name.
fn contract_delegation<'a>(
    contract: &'a CoordinationContract,
    member: &str,
) -> Option<&'a rusty_agent_runtime::agents::Delegation> {
    contract.members().into_iter().find(|d| d.member == member)
}

/// The settle-hook entry point: a task settled (complete route, terminal
/// fail, or immediate cancel); if it is a coordination member task, drive
/// its pattern forward. The detection gate has three parts, all cheap:
/// the payload must parse as a [`CoordinationMessage`], a scoped record
/// must exist for it, and the task must be that record's deterministic
/// member task. Ordinary queue work — and even a hand-crafted payload
/// naming a real coordination — fails the gate unless the task IS the
/// member's own.
pub(crate) async fn on_task_settled(
    store: &Arc<dyn ServerStore>,
    quota: &TaskQuota,
    tenant: &TenantContext,
    task: &TaskRecord,
    now: DateTime<Utc>,
) -> StoreResult<()> {
    let Ok(message) = serde_json::from_value::<CoordinationMessage>(task.payload.clone()) else {
        return Ok(());
    };
    let Some(record) = store
        .get_coordination(&tenant.scope(&message.coordination_id))
        .await?
    else {
        return Ok(());
    };
    let is_member_task = record
        .members
        .iter()
        .any(|member| member.member == message.member && member.task_id == task.task_id);
    if !is_member_task {
        return Ok(());
    }
    drive(store, quota, tenant, record, now).await?;
    Ok(())
}

/// Load a coordination's journal, integrity-verified, for the read
/// endpoints (`None` when the pattern never journaled — impossible for a
/// driven record, but the read is honest about it).
pub(crate) async fn load_journal(
    store: &Arc<dyn ServerStore>,
    tenant: &TenantContext,
    coordination_external: &str,
) -> StoreResult<Option<Journal>> {
    let run_id = coordination_journal_run_id(tenant.tenant(), coordination_external);
    match store.get_journal(&run_id).await? {
        Some(snapshot) => Ok(Some(
            Journal::from_snapshot(snapshot, Clock::System).map_err(|e| {
                format!("coordination journal `{run_id}` failed its integrity check: {e}")
            })?,
        )),
        None => Ok(None),
    }
}

/// Validate a member name or caller-supplied coordination id: the
/// character set that keeps derived task ids and file layouts
/// unambiguous.
pub(crate) fn validate_member_label(what: &str, value: &str) -> Result<(), String> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{what}` must match [A-Za-z0-9._-] and be 1..=128 chars"
        ))
    }
}

// --------------------------------------------------------------------- //
// JSON-file persistence (`{store_path}/coordinations/{tenant}/{id}.json`)
// --------------------------------------------------------------------- //

/// The coordinations directory under the store root. `coordinations` is a
/// reserved layout name (see [`crate::RESERVED_NAMES`]): client-chosen
/// thread ids may not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("coordinations")
}

/// Persist one record atomically (temp file + rename — the durability
/// discipline every file record in the server shares). The id carries a
/// `{tenant}/` prefix, so the parent directory is created, not just the
/// flat dir (the agents convention).
pub(crate) async fn persist(root: &Path, record: &CoordinationRecord) -> io::Result<()> {
    let dir = dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{}.json", record.coordination_id));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dir.join(format!("{}.tmp", record.coordination_id));
    if let Some(parent) = tmp.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records), mirroring the agents loader.
fn collect_json_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Load all persisted coordination records, skipping (with a warning) any
/// file that fails to parse — one corrupt record must not take the
/// registry down at boot.
pub(crate) fn load(root: &Path) -> HashMap<String, CoordinationRecord> {
    let mut files = Vec::new();
    collect_json_files(&dir(root), &mut files);
    let mut out = HashMap::new();
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<CoordinationRecord>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(record.coordination_id.clone(), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable coordination file")
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::agents::{
        DelegateContract, Delegation, FanOutContract, MemberFailurePolicy, QuorumContract,
        QuorumResolver, RaceContract,
    };
    use serde_json::json;

    fn delegation(member: &str) -> Delegation {
        Delegation {
            member: member.into(),
            agent_id: format!("{member}-agent"),
            manifest_version: "researcher/1.4.0".into(),
            kind: "summarize".into(),
            input: PayloadRef::inline(json!({"topic": member})),
            effect: Effect::Pure,
            deadline: None,
        }
    }

    #[test]
    fn derived_ids_are_deterministic_and_path_safe() {
        assert_eq!(
            coordination_journal_run_id("acme", "c-1"),
            "coordination:acme:c-1"
        );
        assert_eq!(member_task_id("acme", "c-1", "writer"), "acme--c-1--writer");
        assert_eq!(outcome_task_id("acme", "c-1"), "acme--c-1--outcome");
        assert_eq!(race_dlq_task_id("acme", "c-1"), "acme--c-1--race-dlq");
        assert_eq!(
            member_idempotency_key("c-1", "writer"),
            "coordination:c-1:writer"
        );
        for id in [
            member_task_id("acme", "c-1", "writer"),
            outcome_task_id("acme", "c-1"),
            race_dlq_task_id("acme", "c-1"),
        ] {
            assert!(!id.contains('/'), "task ids stay in one flat directory");
        }
    }

    #[test]
    fn validate_member_label_bounds() {
        assert!(validate_member_label("member", "writer-7").is_ok());
        assert!(validate_member_label("member", "a.b_c").is_ok());
        assert!(validate_member_label("member", "").is_err());
        assert!(validate_member_label("member", "bad/name").is_err());
        assert!(validate_member_label("member", &"x".repeat(129)).is_err());
    }

    #[test]
    fn record_latches_default_for_forward_compat() {
        // A record deserialized from a minimal payload gets the latches
        // defaulted — the additive-evolution discipline the rest of the
        // server follows.
        let record: CoordinationRecord = serde_json::from_value(json!({
            "coordination_id": "acme/c-1",
            "contract": {
                "pattern": "delegate",
                "delegate": {
                    "member": "writer",
                    "agent_id": "writer-agent",
                    "manifest_version": "researcher/1.4.0",
                    "kind": "summarize",
                    "input": {"kind": "inline", "value": {"topic": "x"}},
                },
            },
            "members": [{
                "member": "writer",
                "agent_id": "writer-agent",
                "manifest_version": "researcher/1.4.0",
                "task_id": "acme--c-1--writer",
            }],
            "created_at": "2027-01-15T08:00:00Z",
            "updated_at": "2027-01-15T08:00:00Z",
        }))
        .unwrap();
        assert!(!record.settled);
        assert!(!record.outcome_delivered);
        assert!(!record.dlq_written);
        assert!(!record.members[0].submitted);
        assert!(record.outcome.is_none());
    }

    #[test]
    fn decide_settle_waits_until_evidence_arrives() {
        let contract = CoordinationContract::FanOut(FanOutContract {
            members: vec![delegation("a"), delegation("b")],
            max_in_flight: 2,
            on_member_failure: MemberFailurePolicy::Partial,
        });
        let record = CoordinationRecord {
            coordination_id: "acme/c-1".into(),
            delegator: None,
            parent: None,
            contract,
            members: vec![
                MemberRecord {
                    member: "a".into(),
                    agent_id: "a-agent".into(),
                    manifest_version: "researcher/1.4.0".into(),
                    task_id: "acme--c-1--a".into(),
                    submitted: true,
                },
                MemberRecord {
                    member: "b".into(),
                    agent_id: "b-agent".into(),
                    manifest_version: "researcher/1.4.0".into(),
                    task_id: "acme--c-1--b".into(),
                    submitted: false,
                },
            ],
            settled: false,
            outcome: None,
            outcome_delivered: false,
            dlq_written: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        // Nothing terminal, one member unsubmitted: no decision.
        let states = vec![
            MemberState {
                task: None,
                disposition: None,
            },
            MemberState {
                task: None,
                disposition: None,
            },
        ];
        assert!(decide_settle(&record, &states).is_none());
    }

    #[test]
    fn contract_delegation_finds_members_across_patterns() {
        let race = CoordinationContract::Race(RaceContract {
            candidates: vec![delegation("a"), delegation("b")],
        });
        assert_eq!(
            contract_delegation(&race, "b").map(|d| d.agent_id.as_str()),
            Some("b-agent")
        );
        assert!(contract_delegation(&race, "z").is_none());
        let quorum = CoordinationContract::Quorum(QuorumContract {
            members: vec![delegation("a")],
            threshold: 1,
            resolver: QuorumResolver::MajorityEqual,
        });
        assert!(contract_delegation(&quorum, "a").is_some());
        let delegate = CoordinationContract::Delegate(Box::new(DelegateContract {
            delegate: delegation("only"),
            context: None,
            result_contract: None,
            handoff: false,
        }));
        assert!(contract_delegation(&delegate, "only").is_some());
    }
}
