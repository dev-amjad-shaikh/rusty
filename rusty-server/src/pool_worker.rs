//! Assistants as workers: an agent bound to a pool (`studio_intent.pool`)
//! consumes the durable task queue. The worker claims one task at a time
//! per agent, runs the agent on a fresh thread with the task's payload as
//! its message, keeps the lease alive while the run executes, and settles
//! the task with the agent's reply as the result — or fails it with the
//! run's error so the queue's retry policy decides. The run is an ordinary
//! run: journaled, verified, in Observe, with `metadata.channel = "pool"`.
//!
//! No person is in the loop: a run that pauses for an approval cannot be
//! finished by a worker, so it fails the task without retry and says so.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use rusty_agent_runtime::durable::{ErrorClass, ResolvedRetryParameters};
use serde_json::{json, Value};

use crate::routes::AppState;
use crate::runs::{self, MultitaskStrategy, RunPayload};
use crate::tasks::{ClaimScope, CompletionReport, FailureReport, SettlementCost, TaskRecord};
use crate::threads::ThreadRecord;

/// How often every pool-bound agent looks for work.
const TICK: Duration = Duration::from_millis(1000);
/// The lease a claim takes, renewed while the run executes.
const LEASE_MS: u64 = 120_000;
/// How long a task waits for a person's decision when its run pauses at
/// the approval gate: the person was told the moment it paused; a quarter
/// of an hour is a queue's patience, not a person's.
const DECISION_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const HEARTBEAT: Duration = Duration::from_secs(30);

/// The pool an assistant works, from its intent — `None` when it works none.
pub(crate) fn pool_of(config: &Value) -> Option<String> {
    config
        .pointer("/studio_intent/pool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

/// The worker id a pool-bound assistant claims under.
fn worker_id(assistant_id: &str) -> String {
    format!("assistant:{assistant_id}")
}

/// Spawn the pool workers. One background task polls every pool-bound
/// assistant; each claimed task runs on its own task. Lives until the drain
/// token fires.
pub(crate) fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let busy: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = state.shutdown.cancelled() => {
                    tracing::info!("pool workers shutting down");
                    break;
                }
            }
            let views = match state.server_store.list_assistants().await {
                Ok(views) => views,
                Err(error) => {
                    tracing::warn!(%error, "pool workers: listing assistants failed");
                    continue;
                }
            };
            for view in views {
                if view.archived_at.is_some() {
                    continue;
                }
                let Some(pool) = pool_of(&view.config) else {
                    continue;
                };
                if busy.lock().expect("busy set").contains(&view.assistant_id) {
                    continue;
                }
                let tenant = crate::auth::tenant_of_internal(&view.assistant_id).to_string();
                let external = crate::auth::strip_owned(&tenant, &view.assistant_id)
                    .unwrap_or(&view.assistant_id)
                    .to_string();
                let pools = [pool.clone()];
                // The claim follows the tenant's active policy, as the claim route does.
                let timeout_record = crate::policy::active_policy_record(&state.server_store, &tenant)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "active policy unreadable; claiming on the static floor");
                        crate::policy::static_floor_record()
                    });
                let scope = ClaimScope {
                    pools: &pools,
                    pool_limits: &state.config.task_pool_limits,
                    worker_version: None,
                    timeout_policy: &timeout_record.policy,
                };
                let claimed = state
                    .server_store
                    .claim_task(&tenant, &worker_id(&external), &scope, LEASE_MS, Utc::now())
                    .await;
                let task = match claimed {
                    Ok(Some(task)) => task,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(assistant = %external, %pool, %error, "pool worker: claim failed");
                        continue;
                    }
                };
                busy.lock()
                    .expect("busy set")
                    .insert(view.assistant_id.clone());
                let state = Arc::clone(&state);
                let busy = Arc::clone(&busy);
                let internal_id = view.assistant_id.clone();
                tokio::spawn(async move {
                    work(&state, &tenant, &internal_id, &external, &pool, task).await;
                    busy.lock().expect("busy set").remove(&internal_id);
                });
            }
        }
    });
}

/// The task's payload as the agent's message: `messages` as given, else the
/// first of `message` / `text` / `ask` / `content` as the user's words, else
/// the payload itself, pretty-printed.
fn input_of(payload: &Value) -> Value {
    if let Some(messages) = payload.get("messages").filter(|m| m.is_array()) {
        return json!({ "messages": messages });
    }
    let words = ["message", "text", "ask", "content"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| serde_json::to_string_pretty(payload).unwrap_or_default());
    json!({ "messages": [{ "role": "user", "content": words }] })
}

/// One task as one run of the agent, settled either way.
async fn work(
    state: &AppState,
    tenant: &str,
    internal_id: &str,
    external: &str,
    pool: &str,
    task: TaskRecord,
) {
    let worker = worker_id(external);
    let now = Utc::now;
    let fail = |class: ErrorClass, message: String, retryable: bool| FailureReport {
        error_class: class,
        message,
        retryable,
        cost: SettlementCost::default(),
        retry: ResolvedRetryParameters::floor(task.max_attempts),
    };
    let assistant = match state.server_store.get_assistant(internal_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = fail_and_tell(
                state,
                tenant,
                &task,
                &worker,
                fail(
                    ErrorClass::DependencyFailure,
                    "the agent bound to this pool is gone".into(),
                    true,
                ),
            )
            .await;
            return;
        }
    };
    let thread_id = uuid::Uuid::new_v4().to_string();
    let internal_thread_id = crate::auth::scope_id(tenant, &thread_id);
    let record = ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: tenant.to_string(),
        graph: assistant.graph.clone(),
        metadata: json!({"trigger": "pool", "pool": pool, "task_id": task.task_id, "assistant_id": external}),
        forked_from: None,
        seed_length: None,
        created_at: Utc::now(),
    };
    if let Err(error) = state
        .server_store
        .create_thread(&internal_thread_id, &record)
        .await
    {
        let _ = fail_and_tell(
            state,
            tenant,
            &task,
            &worker,
            fail(
                ErrorClass::Transient,
                format!("thread could not be created: {error}"),
                true,
            ),
        )
        .await;
        return;
    }
    // A task that names a world (resolved to its id at enqueue) is worked
    // in it: the run acts in the stand-in, and admission refuses the run if
    // the world is gone, so nothing reaches the live system.
    let world = task
        .payload
        .get("world")
        .and_then(Value::as_str)
        .filter(|w| !w.is_empty())
        .map(str::to_owned);
    // The person who queued the work set this run going: it is attributed
    // to them (a pause at the gate tells them), while the run stays the
    // agent's own conversation — nobody's memory but the agent's is read.
    let mut run_metadata = json!({"channel": "pool", "pool": pool, "task_id": task.task_id});
    // The chain the task sits in, for the budget that follows it.
    if let Some(chain) = task.payload.get("chain") {
        run_metadata["chain"] = chain.clone();
    }
    if let Some(who) = task
        .payload
        .get("enqueued_by")
        .filter(|w| w.get("principal_id").is_some())
    {
        run_metadata["created_by"] = who.clone();
    }
    let mut payload = RunPayload {
        input: Some(input_of(&task.payload)),
        metadata: Some(run_metadata),
        assistant_id: Some(external.to_string()),
        config: world.map(|world| crate::runs::RunConfigPayload {
            world: Some(world),
            ..crate::runs::RunConfigPayload::default()
        }),
        ..RunPayload::default()
    };
    crate::routes::apply_assistant_defaults(
        state,
        tenant,
        &internal_thread_id,
        &assistant,
        &mut payload,
    )
    .await;
    let scheduled = match runs::schedule(
        &state.run_deps,
        &internal_thread_id,
        &thread_id,
        &assistant.graph,
        payload,
        MultitaskStrategy::Enqueue,
    )
    .await
    {
        Ok(s) => s,
        Err(error) => {
            let _ = fail_and_tell(
                state,
                tenant,
                &task,
                &worker,
                fail(
                    ErrorClass::Transient,
                    format!("the run was not accepted: {error}"),
                    true,
                ),
            )
            .await;
            return;
        }
    };
    // Keep the lease while the run executes.
    let mut terminal = scheduled.terminal;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            changed = terminal.changed() => {
                if changed.is_err() || terminal.borrow().is_some() { break; }
            }
            _ = heartbeat.tick() => {
                let _ = state.server_store.heartbeat_task(tenant, &task.task_id, &worker, LEASE_MS, now()).await;
            }
        }
    }
    let mut terminal = terminal.borrow().clone().unwrap_or(Value::Null);
    // The run paused at the approval gate. The person it acts for has been
    // told (a notice); the task keeps its lease and waits for the decision
    // — for a while — then goes on with the run the decision continued.
    // Undecided when the wait runs out, the task is dead-lettered rather
    // than retried: a fresh run behind a pending decision could do the
    // write twice.
    let mut final_run_id = scheduled.run_id.clone();
    let mut undecided = false;
    if terminal.get("status").and_then(Value::as_str) == Some("interrupted") {
        let waited_from = std::time::Instant::now();
        while terminal.get("status").and_then(Value::as_str) == Some("interrupted") {
            let left = DECISION_WAIT.saturating_sub(waited_from.elapsed());
            if left.is_zero() {
                undecided = true;
                break;
            }
            let paused = final_run_id.clone();
            let mut heartbeat = tokio::time::interval(HEARTBEAT);
            heartbeat.tick().await;
            let decided = tokio::select! {
                decided = tokio::time::timeout(left, crate::dataset_runs::follow_approval(state, &paused)) => decided,
                _ = async {
                    loop {
                        heartbeat.tick().await;
                        let _ = state.server_store.heartbeat_task(tenant, &task.task_id, &worker, LEASE_MS, now()).await;
                    }
                } => unreachable!("the heartbeat never returns"),
            };
            match decided {
                Ok(Some(resumed)) => {
                    let left = DECISION_WAIT.saturating_sub(waited_from.elapsed());
                    match tokio::time::timeout(
                        left,
                        crate::dataset_runs::wait_terminal(state, &resumed),
                    )
                    .await
                    {
                        Ok(t) => {
                            terminal = t;
                            final_run_id = resumed;
                        }
                        Err(_) => {
                            undecided = true;
                            break;
                        }
                    }
                }
                // Denied, or no approval of ours: the run is over as it stands.
                Ok(None) => break,
                Err(_) => {
                    undecided = true;
                    break;
                }
            }
        }
    }
    // The run answered, but an agent it asked (`agents.ask`) is still paused
    // for a decision — the ask's patience ran out before the person came.
    // A task is not done while a decision it caused is pending: keep the
    // lease, wait for that decision as for the run's own pause, and carry
    // what the asked agent did into the result. Undecided within the wait,
    // the task is dead-lettered like an undecided pause of its own.
    let mut delegated: Vec<Value> = Vec::new();
    let mut paused_where = "the run paused for an approval";
    if !undecided && terminal.get("status").and_then(Value::as_str) == Some("success") {
        let asked_from = [scheduled.run_id.clone(), final_run_id.clone()];
        let waited_from = std::time::Instant::now();
        for child in pending_delegated_from(state, &asked_from).await {
            let agent = match &child.assistant_id {
                Some(id) => state
                    .server_store
                    .get_assistant(&crate::auth::scope_id(tenant, id))
                    .await
                    .ok()
                    .flatten()
                    .map(|a| a.name)
                    .unwrap_or_else(|| id.clone()),
                None => child.graph.clone(),
            };
            let mut child_run = child.run_id.clone();
            let mut decided = "undecided";
            let mut reply = Value::Null;
            loop {
                let left = DECISION_WAIT.saturating_sub(waited_from.elapsed());
                if left.is_zero() {
                    break;
                }
                match wait_decided_keeping_lease(
                    state,
                    tenant,
                    &task.task_id,
                    &worker,
                    &child_run,
                    left,
                )
                .await
                {
                    Ok(Some((resumed, t))) => {
                        child_run = resumed;
                        if t.get("status").and_then(Value::as_str) == Some("interrupted") {
                            continue;
                        }
                        decided = "approved";
                        reply = last_reply(state, &crate::auth::scope_id(tenant, &child.thread_id))
                            .await;
                        break;
                    }
                    Ok(None) => {
                        decided = "denied";
                        reply = last_reply(state, &crate::auth::scope_id(tenant, &child.thread_id))
                            .await;
                        break;
                    }
                    Err(()) => break,
                }
            }
            if decided == "undecided" {
                undecided = true;
                paused_where = "an agent the run asked paused for an approval";
                final_run_id = child_run;
                break;
            }
            delegated.push(
                json!({"agent": agent, "run_id": child_run, "decided": decided, "reply": reply}),
            );
        }
    }
    let status = if undecided {
        "undecided"
    } else {
        terminal.get("status").and_then(Value::as_str).unwrap_or("")
    };
    let spend = terminal.get("spend");
    let cost = SettlementCost {
        tokens: spend
            .and_then(|s| s.get("tokens"))
            .and_then(Value::as_u64)
            .map(|t| rusty_agent_runtime::llm::Usage {
                total_tokens: t,
                ..Default::default()
            }),
        cost_usd: spend
            .and_then(|s| s.get("cost_usd"))
            .and_then(Value::as_f64),
    };
    match status {
        "success" => {
            let reply = last_reply(state, &internal_thread_id).await;
            // A task is settled by its run's verdict, not by the run ending:
            // a reply the verifier failed — a number no tool returned, a
            // claim the evidence contradicts — is not a result, it is a
            // failed attempt, and the queue retries it like any other.
            let verdict = if state.config.verifier.is_some() {
                let mut found = None;
                for _ in 0..24 {
                    if let Some(v) = state.verifications.load(&final_run_id) {
                        found = Some(v);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                found
            } else {
                None
            };
            if let Some(v) = verdict
                .as_ref()
                .filter(|v| v.get("verdict").and_then(Value::as_str) == Some("failed"))
            {
                let reason = v
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                let mut report = fail(
                    ErrorClass::Transient,
                    format!("the verifier failed the run {final_run_id}: {reason}"),
                    true,
                );
                report.cost = cost;
                if let Err(error) = fail_and_tell(state, tenant, &task, &worker, report).await {
                    tracing::warn!(task = %task.task_id, %error, "pool worker: failure could not be recorded");
                }
                return;
            }
            let report = CompletionReport {
                result: json!({
                    "reply": reply,
                    "run_id": final_run_id,
                    "first_run_id": scheduled.run_id,
                    "thread_id": thread_id,
                    "agent": external,
                    "verdict": verdict.as_ref().and_then(|v| v.get("verdict").cloned()),
                    "delegated": if delegated.is_empty() { Value::Null } else { json!(delegated) },
                }),
                receipt: None,
                cost,
            };
            if let Err(error) = state
                .server_store
                .complete_task(tenant, &task.task_id, &worker, report, now())
                .await
            {
                tracing::warn!(task = %task.task_id, %error, "pool worker: completion failed");
            }
        }
        "undecided" | "interrupted" => {
            let mut report = fail(
                ErrorClass::DependencyFailure,
                format!(
                    "{paused_where} (run {final_run_id}) and no decision came within {} minutes; it continues once decided (Inbox), and this task is not retried behind it",
                    DECISION_WAIT.as_secs() / 60
                ),
                false,
            );
            report.cost = cost;
            let _ = fail_and_tell(state, tenant, &task, &worker, report).await;
        }
        other => {
            let message = terminal
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("the run ended {other}"));
            let class = match terminal.get("error").and_then(Value::as_str) {
                Some("budget_exhausted") => ErrorClass::InvalidInput,
                Some("llm_error") => ErrorClass::Transient,
                _ => ErrorClass::Transient,
            };
            let retryable = !matches!(
                terminal.get("error").and_then(Value::as_str),
                Some("budget_exhausted")
            );
            let mut report = fail(
                class,
                format!("run {}: {message}", scheduled.run_id),
                retryable,
            );
            report.cost = cost;
            let _ = fail_and_tell(state, tenant, &task, &worker, report).await;
        }
    }
}

/// Fail the task, and when that failure is its last — the task is dead,
/// nobody will work it again — tell the person who queued it, once, in
/// the Inbox: what it was, why it ended, and where to look. A failure the
/// queue will retry says nothing; the board shows it.
async fn fail_and_tell(
    state: &AppState,
    tenant: &str,
    task: &TaskRecord,
    worker: &str,
    report: FailureReport,
) -> crate::server_store::StoreResult<crate::tasks::MutationOutcome> {
    let outcome = state
        .server_store
        .fail_task(tenant, &task.task_id, worker, report, Utc::now())
        .await?;
    if let Ok(Some(record)) = state.server_store.get_task(tenant, &task.task_id).await {
        if matches!(record.status, crate::tasks::TaskStatus::Dead) {
            if let Some(who) = record
                .payload
                .get("enqueued_by")
                .filter(|w| w.get("principal_id").is_some())
                .cloned()
            {
                let reason = record
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "it could not be done".to_owned());
                let title = format!("Your task {} could not be done", record.kind);
                let text = format!("After {} attempt{} in pool {}: {reason} Open Work to read it; queue it again if it should be tried afresh.", record.attempt, if record.attempt == 1 { "" } else { "s" }, record.pool);
                crate::notices::tell_in(
                    &state.server_store,
                    tenant,
                    &who,
                    &format!("task:{}:dead", record.task_id),
                    json!({"kind": "task", "task_id": record.task_id, "state": "dead", "pool": record.pool, "task_kind": record.kind}),
                    &title,
                    &text,
                )
                .await;
            }
        }
    }
    Ok(outcome)
}

/// The last thing the agent said on a thread — the reply a task settles with.
async fn last_reply(state: &AppState, internal_thread_id: &str) -> Value {
    state
        .checkpointer
        .get_latest(internal_thread_id)
        .await
        .ok()
        .flatten()
        .map(|cp| cp.state.to_value())
        .and_then(|v| {
            v.get("messages")?
                .as_array()?
                .iter()
                .rev()
                .find(|m| {
                    m.get("role").and_then(Value::as_str) == Some("assistant")
                        && m.get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|c| !c.trim().is_empty())
                })
                .and_then(|m| m.get("content").cloned())
        })
        .unwrap_or(Value::Null)
}

/// The pending approvals of runs the given runs asked (`agents.ask`): a
/// delegated run names the asker in its metadata.
async fn pending_delegated_from(
    state: &AppState,
    parents: &[String],
) -> Vec<crate::approvals::ApprovalRecord> {
    let mut out = Vec::new();
    for a in state
        .run_deps
        .approvals
        .list()
        .into_iter()
        .filter(|a| a.status == "pending")
    {
        let Ok(Some(accepted)) = state.server_store.get_accepted_run(&a.run_id).await else {
            continue;
        };
        let from = accepted
            .payload
            .metadata
            .as_ref()
            .and_then(|m| m.pointer("/delegation/from_run"))
            .and_then(Value::as_str);
        if from.is_some_and(|f| parents.iter().any(|p| p == f)) {
            out.push(a);
        }
    }
    out
}

/// Waits, keeping the task's lease alive, for the decision on `run` and
/// for the run it continued to end: `Ok(Some((resumed, terminal)))` once
/// decided and ended, `Ok(None)` when denied, `Err(())` when no decision
/// came within `left`.
async fn wait_decided_keeping_lease(
    state: &AppState,
    tenant: &str,
    task_id: &str,
    worker: &str,
    run: &str,
    left: Duration,
) -> std::result::Result<Option<(String, Value)>, ()> {
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let followed = async {
        let resumed = crate::dataset_runs::follow_approval(state, run).await?;
        let terminal = crate::dataset_runs::wait_terminal(state, &resumed).await;
        Some((resumed, terminal))
    };
    tokio::select! {
        decided = tokio::time::timeout(left, followed) => decided.map_err(|_| ()),
        _ = async {
            loop {
                heartbeat.tick().await;
                let _ = state.server_store.heartbeat_task(tenant, task_id, worker, LEASE_MS, Utc::now()).await;
            }
        } => unreachable!("the heartbeat never returns"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_becomes_the_agents_message() {
        assert_eq!(
            input_of(&json!({"message": "hi"}))["messages"][0]["content"],
            json!("hi")
        );
        assert_eq!(
            input_of(&json!({"ask": "what?"}))["messages"][0]["content"],
            json!("what?")
        );
        assert_eq!(
            input_of(&json!({"messages": [{"role": "user", "content": "as is"}]}))["messages"][0]
                ["content"],
            json!("as is")
        );
        let pretty = input_of(&json!({"order": 1}))["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(pretty.contains("\"order\": 1"));
        assert_eq!(
            pool_of(&json!({"studio_intent": {"pool": " briefs "}})),
            Some("briefs".into())
        );
        assert_eq!(pool_of(&json!({"studio_intent": {"pool": ""}})), None);
        assert_eq!(pool_of(&json!({})), None);
    }
}
