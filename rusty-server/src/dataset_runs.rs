//! Running a dataset version against an agent: every case becomes a real
//! run of the agent on a fresh thread, and its journal is judged against
//! what the case says should be true (the eval crate's assertions — tool
//! trajectory, forbidden tools, final-state predicates, cost and latency
//! bounds). The verdicts are stored per evaluation and read back by the
//! studio while the cases finish one by one.
//!
//! This is the studio's evaluation loop over the same engine a customer's
//! agents run on: no separate harness, no mocked model — the run is the
//! evidence, and it lands in Observe like any other run.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_eval::{
    AssertionResult, JudgeModel, JudgeRequest, ModelJudge, RunEvidence, RunStatus as EvidenceStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::evaluations::PublishedEvalCase;
use crate::routes::{AppState, internal_err};
use crate::runs::{self, MultitaskStrategy, RunPayload};
use crate::server_store::ServerStore;
use crate::threads::ThreadRecord;

const EVALUATION_NAMESPACE: &str = "studio_eval_dataset_evaluations";
/// Evaluations kept per dataset version (newest first).
const MAX_EVALUATIONS_PER_VERSION: usize = 20;

/// A model's verdict on the reply, against the case's rubric.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JudgeOutcome {
    pub score: f64,
    pub passed: bool,
    pub rationale: String,
}

/// One case, run and judged (or still running).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseEvaluation {
    pub case_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The real run the case became; absent until it is scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// How the run ended: `done`, `interrupted`, `failed`; absent while it
    /// runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub passed: bool,
    #[serde(default)]
    pub assertions: Vec<AssertionResult>,
    /// The model's verdict on the reply, when the case carries a rubric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeOutcome>,
    #[serde(default)]
    pub tool_calls: Vec<String>,
    /// The runs a person's approvals continued this case in, in order —
    /// a case with a consequential write pauses at the gate and goes on in
    /// a resumed run; its evidence is the whole chain's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resumed_run_ids: Vec<String>,
    /// Approvals the evaluation gave for itself because the case ran in a
    /// world: the effect landed in the stand-in, not the live system. A
    /// case not in a world waits for a person, as a live run does.
    #[serde(default)]
    pub approvals_in_world: u32,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The world the case ran in (its `world:<name>` tag), reset first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<String>,
}

/// A skill revision an evaluation runs its cases under, whatever the
/// current one is: named by a person comparing revisions, or by the gate
/// judging a candidate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillPin {
    pub name: String,
    pub revision: u64,
}

/// A dataset version run against one agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetEvaluation {
    pub evaluation_id: String,
    pub name: String,
    pub version: String,
    pub assistant_id: String,
    /// The agent version the cases ran under — the evidence a promotion
    /// is bound to.
    #[serde(default)]
    pub assistant_version_id: String,
    /// What the version depended on when the cases ran: skill revisions,
    /// the connections behind its tools, the model route. Evidence is
    /// current only while these stand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Value>,
    /// The skill revision the cases ran under, when one was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_pin: Option<SkillPin>,
    /// The server process that runs (or ran) it — see [`AppState::boot_id`].
    #[serde(default)]
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<Value>,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// `running`, `done`, `over_budget` (the spend cap was reached before
    /// every case ran), or `error` (the evaluation itself could not run).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub passed: usize,
    pub total: usize,
    /// The most this evaluation may spend, in USD, when it judges a
    /// candidate: the cases stop once it is reached. Named by whoever
    /// started it (`started_by.budget_usd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// What the cases have cost so far.
    #[serde(default)]
    pub spent_usd: f64,
    pub cases: Vec<CaseEvaluation>,
}

/// The judge model as the eval crate's strict parser wants it. A local
/// model asked for a `submit_judgment` tool call sometimes writes the call
/// as JSON text — `{"name": "submit_judgment", "arguments": {…}}` — or adds
/// fields beside `score` and `rationale`; both are the verdict, spelled
/// loosely. This adapter turns them into the exact tool call before the
/// parser sees them and leaves every other answer untouched, so the crate's
/// contract stays strict and the server stays honest about what the model
/// said.
struct JudgeAdapter(Arc<dyn rusty_agent_runtime::llm::ChatModel>);

#[async_trait::async_trait]
impl rusty_agent_runtime::llm::ChatModel for JudgeAdapter {
    async fn chat(
        &self,
        messages: &[rusty_agent_runtime::llm::ChatMessage],
        tools: &[Value],
    ) -> rusty_agent_runtime::error::Result<rusty_agent_runtime::llm::ChatResponse> {
        let mut response = self.0.chat(messages, tools).await?;
        if response.message.tool_calls.is_empty() {
            if let Some(verdict) = response.message.content.as_deref().and_then(loose_verdict) {
                response.message =
                    rusty_agent_runtime::llm::ChatMessage::assistant_tool_calls(vec![
                        rusty_agent_runtime::llm::ToolCall::new(
                            "judgment",
                            "submit_judgment",
                            verdict,
                        ),
                    ]);
            }
        }
        Ok(response)
    }
}

/// `{score, rationale}` out of a loosely spelled verdict, or `None` when the
/// text is not one.
fn loose_verdict(text: &str) -> Option<Value> {
    let text = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: Value = serde_json::from_str(text).ok()?;
    let object = match (
        value.get("name").and_then(Value::as_str),
        value.get("arguments"),
    ) {
        (Some("submit_judgment"), Some(arguments)) => arguments.clone(),
        _ => value,
    };
    let score = object.get("score")?.as_f64()?;
    let rationale = object.get("rationale")?.as_str()?.to_owned();
    Some(json!({ "score": score, "rationale": rationale }))
}

fn namespace(tenant: &str) -> String {
    format!("{tenant}/{EVALUATION_NAMESPACE}")
}

pub(crate) fn evaluation_key(name: &str, version: &str) -> String {
    key(name, version)
}

pub(crate) fn evaluation_namespace(tenant: &str) -> String {
    namespace(tenant)
}

fn key(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// The evaluations stored for a dataset version, newest first.
pub(crate) async fn stored(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<Vec<DatasetEvaluation>, ApiError> {
    match store
        .kv_get(&namespace(tenant), &key(name, version))
        .await
        .map_err(internal_err)?
    {
        Some(item) => serde_json::from_value(item.value)
            .map_err(|error| ApiError::internal(format!("decode dataset evaluations: {error}"))),
        None => Ok(Vec::new()),
    }
}

/// The evaluations of a dataset version, newest first. One still marked
/// `running` by a process that is no longer this one was orphaned by a
/// restart — its background task died with that process — and is closed
/// as an error here, once, so it never reads as running forever.
pub(crate) async fn list(
    state: &AppState,
    tenant: &str,
    name: &str,
    version: &str,
) -> Result<Vec<DatasetEvaluation>, ApiError> {
    let mut all = stored(&state.server_store, tenant, name, version).await?;
    let mut orphaned = false;
    for evaluation in &mut all {
        if evaluation.status == "running" && evaluation.boot_id != state.boot_id {
            evaluation.status = "error".to_owned();
            evaluation.error = Some("the server restarted while this evaluation ran; the cases that had not finished did not run".to_owned());
            evaluation.finished_at = Some(Utc::now());
            orphaned = true;
        }
    }
    if orphaned {
        write_all(&state.server_store, tenant, name, version, &all).await?;
    }
    Ok(all)
}

async fn write_all(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    name: &str,
    version: &str,
    all: &[DatasetEvaluation],
) -> Result<(), ApiError> {
    store
        .kv_put(
            &namespace(tenant),
            &key(name, version),
            serde_json::to_value(all).map_err(|e| ApiError::internal(e.to_string()))?,
        )
        .await
        .map_err(internal_err)?;
    Ok(())
}

/// Store `evaluation`, replacing an earlier record of the same id. The
/// version's list is read, edited, and written back; two evaluations of the
/// same version running at once each rewrite the whole list, so the last
/// writer's view of the *other* one may be a step stale — never lost, since
/// each rewrites itself again at its next case.
async fn save(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    evaluation: &DatasetEvaluation,
) -> Result<(), ApiError> {
    let mut all = stored(store, tenant, &evaluation.name, &evaluation.version).await?;
    all.retain(|e| e.evaluation_id != evaluation.evaluation_id);
    all.insert(0, evaluation.clone());
    all.sort_by_key(|b| std::cmp::Reverse(b.started_at));
    all.truncate(MAX_EVALUATIONS_PER_VERSION);
    write_all(store, tenant, &evaluation.name, &evaluation.version, &all).await
}

/// Start running `cases` against the agent: the record is stored as
/// `running` with every case pending, the runs happen on a background task
/// one case at a time, and the record is rewritten after each verdict.
// One parameter per admission-time decision; the record is built from
// exactly these, and grouping would only reshuffle the same data.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start(
    state: Arc<AppState>,
    tenant: String,
    name: String,
    version: String,
    assistant: crate::assistants::AssistantRecord,
    cases: Vec<PublishedEvalCase>,
    started_by: Option<Value>,
    pin: Option<SkillPin>,
) -> Result<DatasetEvaluation, ApiError> {
    // The revision to run under: the one named, or the candidate when a
    // skill revision itself started this (the gate).
    let pin = pin.or_else(|| {
        skill_pin(started_by.as_ref()).map(|(name, revision)| SkillPin { name, revision })
    });
    let pins: Vec<(String, u64)> = pin.iter().map(|p| (p.name.clone(), p.revision)).collect();
    let budget_usd = started_by
        .as_ref()
        .and_then(|s| s.get("budget_usd"))
        .and_then(Value::as_f64)
        .filter(|b| *b > 0.0);
    let evaluation = DatasetEvaluation {
        evaluation_id: uuid::Uuid::new_v4().to_string(),
        name,
        version,
        assistant_id: assistant.assistant_id.clone(),
        assistant_version_id: assistant.active_version_id.clone().unwrap_or_default(),
        dependencies: Some(
            crate::promotion::dependency_snapshot_pinned(&state, &tenant, &assistant, &pins).await,
        ),
        skill_pin: pin,
        boot_id: state.boot_id.clone(),
        started_by,
        started_at: Utc::now(),
        finished_at: None,
        status: "running".to_owned(),
        error: None,
        passed: 0,
        total: cases.len(),
        budget_usd,
        spent_usd: 0.0,
        cases: cases
            .iter()
            .map(|c| CaseEvaluation {
                case_id: c.case.id.clone(),
                tags: c.case.tags.clone(),
                run_id: None,
                thread_id: None,
                status: None,
                passed: false,
                assertions: Vec::new(),
                judge: None,
                tool_calls: Vec::new(),
                resumed_run_ids: Vec::new(),
                approvals_in_world: 0,
                latency_ms: 0,
                cost_usd: 0.0,
                total_tokens: 0,
                error: None,
                world: world_tag(&c.case.tags),
            })
            .collect(),
    };
    save(&state.server_store, &tenant, &evaluation).await?;
    let started = evaluation.clone();
    tokio::spawn(async move {
        let mut evaluation = evaluation;
        let pin = evaluation.skill_pin.clone().map(|p| (p.name, p.revision));
        let mut over_budget = false;
        for (index, case) in cases.into_iter().enumerate() {
            // The spend cap: a candidate that has cost its budget is not
            // judged further; the cases it never reached say so, and the
            // gate reads the evaluation as over budget, not as passed.
            if let Some(budget) = evaluation.budget_usd.filter(|b| evaluation.spent_usd >= *b) {
                over_budget = true;
                for pending in evaluation.cases.iter_mut().skip(index) {
                    pending.status = Some("skipped".to_owned());
                    pending.error = Some(format!(
                        "not run: the candidate's budget of ${budget:.2} was spent after {index} case{}",
                        if index == 1 { "" } else { "s" }
                    ));
                }
                break;
            }
            let verdict = run_case(&state, &tenant, &assistant, &case, pin.as_ref()).await;
            evaluation.spent_usd += verdict.cost_usd;
            evaluation.cases[index] = verdict;
            evaluation.passed = evaluation.cases.iter().filter(|c| c.passed).count();
            if let Err(error) = save(&state.server_store, &tenant, &evaluation).await {
                tracing::warn!(evaluation_id = %evaluation.evaluation_id, %error, "dataset evaluation persistence failed");
            }
        }
        evaluation.status = if over_budget { "over_budget" } else { "done" }.to_owned();
        evaluation.finished_at = Some(Utc::now());
        if let Err(error) = save(&state.server_store, &tenant, &evaluation).await {
            tracing::warn!(evaluation_id = %evaluation.evaluation_id, %error, "dataset evaluation persistence failed");
        }
        // A candidate the gate judged: under the auto policy it may activate now.
        if let Some(said) =
            crate::promotion::after_candidate_evaluation(&state, &tenant, &evaluation).await
        {
            tracing::info!(evaluation_id = %evaluation.evaluation_id, version = %evaluation.assistant_version_id, "candidate gate: {said}");
        }
    });
    Ok(started)
}

/// The world a case runs in: its `world:<name>` tag.
pub(crate) fn world_tag(tags: &[String]) -> Option<String> {
    tags.iter()
        .find_map(|t| t.strip_prefix("world:"))
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
}

/// One case as a real run of the agent on a fresh thread, judged.
async fn run_case(
    state: &std::sync::Arc<AppState>,
    tenant: &str,
    assistant: &crate::assistants::AssistantRecord,
    case: &PublishedEvalCase,
    pin: Option<&(String, u64)>,
) -> CaseEvaluation {
    let mut verdict = CaseEvaluation {
        case_id: case.case.id.clone(),
        tags: case.case.tags.clone(),
        run_id: None,
        thread_id: None,
        status: None,
        passed: false,
        assertions: Vec::new(),
        judge: None,
        tool_calls: Vec::new(),
        resumed_run_ids: Vec::new(),
        approvals_in_world: 0,
        latency_ms: 0,
        cost_usd: 0.0,
        total_tokens: 0,
        error: None,
        world: world_tag(&case.case.tags),
    };
    // A case in a world: the world goes back to its seed first, so the
    // create path is the create path every time; then the run is told.
    let world_id = match world_tag(&case.case.tags) {
        Some(name) => match state.worlds.find(tenant, &name).await {
            Ok(Some(world)) => match state.worlds.reset(tenant, &world.world_id).await {
                Ok(_) => Some(world.world_id),
                Err(error) => {
                    verdict.status = Some("failed".to_owned());
                    verdict.error = Some(format!("world `{name}` could not be reset: {error}"));
                    return verdict;
                }
            },
            Ok(None) => {
                verdict.status = Some("failed".to_owned());
                verdict.error = Some(format!(
                    "the case names world `{name}`, which this tenant does not hold — make it in Evals → Worlds, or drop the tag"
                ));
                return verdict;
            }
            Err(error) => {
                verdict.status = Some("failed".to_owned());
                verdict.error = Some(format!("world `{name}` could not be read: {error}"));
                return verdict;
            }
        },
        None => None,
    };
    let thread_id = uuid::Uuid::new_v4().to_string();
    let internal_thread_id = crate::auth::scope_id(tenant, &thread_id);
    let record = ThreadRecord {
        thread_id: thread_id.clone(),
        tenant: tenant.to_string(),
        graph: assistant.graph.clone(),
        metadata: json!({"trigger": "evaluation", "assistant_id": assistant.assistant_id, "case_id": case.case.id}),
        forked_from: None,
        seed_length: None,
        created_at: Utc::now(),
    };
    if let Err(error) = state
        .server_store
        .create_thread(&internal_thread_id, &record)
        .await
    {
        verdict.status = Some("failed".to_owned());
        verdict.error = Some(format!("thread could not be created: {error}"));
        return verdict;
    }
    verdict.thread_id = Some(thread_id.clone());
    // A case bound to a run carries the charter that run heard, as its
    // leading system message. The version under evaluation speaks for
    // itself: drop that message and let the assistant's defaults put the
    // current charter in its place — kept, an evaluation would replay the
    // old charter and could never show that an agent improved.
    let mut input = case.case.input.clone();
    if crate::routes::assistant_instructions(&assistant.config).is_some() {
        drop_leading_system_message(&mut input);
    }
    // A case recorded from a person's run replays as that person: an agent
    // that remembers and recalls per person finds their notes, and its
    // approvals are theirs. Without this every memory-bound case fails as
    // nobody's.
    let mut metadata = json!({"channel": "evaluation", "case_id": case.case.id});
    let tenant_context = crate::auth::TenantContext::new(tenant.to_owned(), Vec::new());
    if let Ok(Some(source)) =
        crate::routes::recall_run(state, &tenant_context, &case.source.run_id).await
    {
        if let Some(who) = source
            .metadata
            .as_ref()
            .and_then(|m| m.get("created_by"))
            .filter(|v| !v.is_null())
        {
            metadata["created_by"] = who.clone();
            metadata["on_behalf_of"] = who.clone();
        }
    }
    let mut payload = RunPayload {
        input: Some(input),
        metadata: Some(metadata),
        assistant_id: Some(assistant.assistant_id.clone()),
        ..RunPayload::default()
    };
    crate::routes::apply_assistant_defaults(
        state,
        tenant,
        &internal_thread_id,
        assistant,
        &mut payload,
    )
    .await;
    if let Some(world_id) = &world_id {
        payload.config.get_or_insert_with(Default::default).world = Some(world_id.clone());
    }
    // An evaluation a skill revision started runs its follower under that
    // revision — the candidate — whatever the current one is.
    if let Some(pin) = pin {
        let names = crate::routes::assistant_skills(&assistant.config);
        if !names.is_empty() {
            let entries = crate::platform_tools::skill_entries_pinned(
                state,
                tenant,
                &names,
                std::slice::from_ref(pin),
            )
            .await;
            payload.config.get_or_insert_with(Default::default).skills =
                Some(serde_json::to_value(entries).unwrap_or(Value::Null));
        }
    }
    let started = std::time::Instant::now();
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
        Ok(scheduled) => scheduled,
        Err(error) => {
            verdict.status = Some("failed".to_owned());
            verdict.error = Some(format!("the run was not accepted: {error}"));
            return verdict;
        }
    };
    verdict.run_id = Some(scheduled.run_id.clone());
    let mut terminal = scheduled.terminal;
    let _ = terminal.wait_for(|v| v.is_some()).await;
    let mut terminal = terminal.borrow().clone().unwrap_or(Value::Null);
    // A case whose run paused at the approval gate goes on in the run a
    // person's decision starts: follow the chain, so a write case is judged
    // on what it did once approved, not on the pause. A denial ends it here.
    let mut chain = vec![scheduled.run_id.clone()];
    while terminal.get("status").and_then(Value::as_str) == Some("interrupted") {
        // In a world the evaluation approves for itself: the effect lands
        // in the stand-in, so a suite with a consequential write is a
        // suite, not a queue of questions for a person. The record says
        // the evaluation decided, and in which world.
        if let Some(world) = world_tag(&case.case.tags) {
            let paused = chain.last().expect("a run").clone();
            let by = json!({"kind": "service", "principal_id": format!("evaluation:{}", verdict.run_id.as_deref().unwrap_or("?")), "name": format!("the evaluation, in world {world}")});
            let context = crate::auth::TenantContext::new(tenant.to_owned(), Vec::new());
            match crate::approvals::decide_run(
                state,
                &context,
                &paused,
                true,
                Some(format!(
                    "in world {world}: the effect lands in the stand-in, not the live system"
                )),
                by,
            )
            .await
            {
                Ok(_) => verdict.approvals_in_world += 1,
                Err(error) => {
                    tracing::warn!(run_id = %paused, %error, "the evaluation could not approve the run in its world; waiting for a person")
                }
            }
        }
        let Some(resumed) = follow_approval(state, chain.last().expect("a run")).await else {
            break;
        };
        terminal = wait_terminal(state, &resumed).await;
        chain.push(resumed);
    }
    verdict.resumed_run_ids = chain[1..].to_vec();
    let latency_ms = started.elapsed().as_millis() as u64;

    let status = match terminal.get("status").and_then(Value::as_str) {
        Some("success") => EvidenceStatus::Done,
        Some("interrupted") => EvidenceStatus::Interrupted,
        other => EvidenceStatus::Failed {
            error: terminal
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("run ended {}", other.unwrap_or("without a status"))),
        },
    };
    let final_state = match &status {
        EvidenceStatus::Failed { .. } => Value::Null,
        _ => state
            .checkpointer
            .get_latest(&internal_thread_id)
            .await
            .ok()
            .flatten()
            .map(|cp| cp.state.to_value())
            .unwrap_or(Value::Null),
    };
    let mut journals = Vec::new();
    for run_id in &chain {
        if let Ok(Some(snapshot)) = state.server_store.get_journal(run_id).await {
            if let Ok(journal) = Journal::from_snapshot(snapshot, Clock::System) {
                journals.push(journal);
            }
        }
    }
    if journals.is_empty() {
        verdict.status = Some("failed".to_owned());
        verdict.error = Some("the run left no journal to judge".to_owned());
        verdict.latency_ms = latency_ms;
        return verdict;
    }
    // The chain's evidence is one: every run's tool calls in order, cost
    // and tokens summed, the status and final state of the last.
    let mut evidence = RunEvidence::from_journal(
        &journals[0],
        status.clone(),
        final_state.clone(),
        latency_ms,
    );
    for journal in &journals[1..] {
        let more = RunEvidence::from_journal(journal, status.clone(), final_state.clone(), 0);
        evidence.tool_calls.extend(more.tool_calls);
        evidence.cost_usd += more.cost_usd;
        evidence.total_tokens = evidence.total_tokens.saturating_add(more.total_tokens);
    }
    // What the chain left in the world: every row the stand-in stamped as
    // written by one of its runs. The evidence a `world_writes` expectation
    // is judged on — what the agent did, read back from where it did it.
    if let Some(world_id) = &world_id {
        evidence.world_writes = world_writes_of(state, tenant, world_id, &chain).await;
    }
    verdict.tool_calls = evidence
        .tool_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    verdict.latency_ms = evidence.latency_ms;
    verdict.cost_usd = evidence.cost_usd;
    verdict.total_tokens = evidence.total_tokens;
    verdict.assertions = case
        .case
        .expect
        .assertions()
        .iter()
        .map(|assertion| assertion.evaluate(&evidence))
        .collect();
    verdict.status = Some(
        match &evidence.status {
            EvidenceStatus::Done => "done",
            EvidenceStatus::Interrupted => "interrupted",
            EvidenceStatus::Failed { .. } => "failed",
        }
        .to_owned(),
    );
    if let EvidenceStatus::Failed { error } = &evidence.status {
        verdict.error = Some(error.clone());
    }
    // The rubric: what a good reply must do, decided by the server's judge
    // model over the run's final state. A rubric without a judge model is a
    // failed case that says so, never a silent pass.
    if let Some(rubric) = case
        .case
        .expect
        .rubric
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        verdict.judge = Some(match &state.config.verifier {
            None => JudgeOutcome {
                score: 0.0,
                passed: false,
                rationale: "this server has no judge model configured; a rubric needs one"
                    .to_owned(),
            },
            Some(_) if !evidence.status.is_done() => JudgeOutcome {
                score: 0.0,
                passed: false,
                rationale: "the run did not finish, so there is no reply to judge".to_owned(),
            },
            Some(verifier) => {
                let request = JudgeRequest {
                    case_id: case.case.id.clone(),
                    input: case.case.input.clone(),
                    expectations: case.case.expect.clone(),
                    evidence: evidence.clone(),
                };
                match ModelJudge::new(Arc::new(JudgeAdapter(verifier.0.clone())), rubric) {
                    Ok(judge) => match judge.judge(&request).await {
                        Ok(v) => JudgeOutcome {
                            score: v.score,
                            passed: v.passed,
                            rationale: v.rationale,
                        },
                        Err(error) => JudgeOutcome {
                            score: 0.0,
                            passed: false,
                            rationale: format!("the judge could not decide: {error}"),
                        },
                    },
                    Err(error) => JudgeOutcome {
                        score: 0.0,
                        passed: false,
                        rationale: format!("the rubric was refused: {error}"),
                    },
                }
            }
        });
    }
    verdict.passed = evidence.status.is_done()
        && verdict.assertions.iter().all(|a| a.passed)
        && verdict.judge.as_ref().is_none_or(|j| j.passed);
    verdict
}

/// The rows of the world that say one of `runs` wrote them, table by table.
async fn world_writes_of(
    state: &Arc<AppState>,
    tenant: &str,
    world_id: &str,
    runs: &[String],
) -> Vec<rusty_eval::WorldWrite> {
    let world = match state.worlds.find(tenant, world_id).await {
        Ok(Some(world)) => world,
        Ok(None) => return Vec::new(),
        Err(error) => {
            tracing::warn!(world_id, %error, "the world could not be read back for the case's evidence");
            return Vec::new();
        }
    };
    let Some(tables) = world.state.get("tables").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut writes = Vec::new();
    for (table, rows) in tables {
        for row in rows.as_array().into_iter().flatten() {
            let by = row
                .get("_written_by")
                .and_then(|b| b.get("run_id"))
                .and_then(Value::as_str);
            if by.is_some_and(|id| runs.iter().any(|r| r == id)) {
                writes.push(rusty_eval::WorldWrite {
                    table: table.clone(),
                    row: row.clone(),
                });
            }
        }
    }
    writes
}

/// Remove a leading `system` message from a `messages` input, if any.
fn drop_leading_system_message(input: &mut Value) {
    let Some(messages) = input.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let leads = messages
        .first()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("system");
    if leads {
        messages.remove(0);
    }
}

/// The skill revision an evaluation is pinned to, when a skill revision
/// started it: `started_by {kind: "skill", name, revision}`.
pub(crate) fn skill_pin(started_by: Option<&Value>) -> Option<(String, u64)> {
    let by = started_by?;
    if by.get("kind").and_then(Value::as_str) != Some("skill") {
        return None;
    }
    Some((
        by.get("name")?.as_str()?.to_owned(),
        by.get("revision")?.as_u64()?,
    ))
}

/// The run a person's decision continued `run_id` in, once decided: waits
/// for the approval that paused it; `None` when the pause was no approval
/// of ours, or the decision was a denial.
pub(crate) async fn follow_approval(state: &AppState, run_id: &str) -> Option<String> {
    loop {
        let record = state
            .run_deps
            .approvals
            .list()
            .into_iter()
            .find(|r| r.run_id == run_id)?;
        match record.status.as_str() {
            "pending" => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            // Approved or denied, the paused run continues as another run —
            // a denial with the person's reason, finishing without the call.
            "approved" | "denied" => match record.resumed_run_id {
                Some(resumed) => return Some(resumed),
                None => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            },
            _ => return None,
        }
    }
}

/// Why an evaluation did not pass, one line per case that failed: the
/// judge's rationale when the judge failed it, else the first assertion
/// that did not hold, else the run's error. What a gate shows beside
/// "0 of 1 passed", so the decision is made on the reason, not the count.
pub(crate) fn failures_of(evaluation: &DatasetEvaluation) -> Vec<Value> {
    evaluation
        .cases
        .iter()
        .filter(|c| c.status.is_some() && !c.passed)
        .map(|c| {
            let judged = c
                .judge
                .as_ref()
                .filter(|j| !j.passed)
                .map(|j| j.rationale.trim().to_owned())
                .filter(|r| !r.is_empty());
            let held = c
                .assertions
                .iter()
                .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
                .find(|a| a.get("passed").and_then(Value::as_bool) == Some(false))
                .map(|a| {
                    let name = a
                        .get("assertion")
                        .and_then(Value::as_str)
                        .unwrap_or("an assertion");
                    match a
                        .get("detail")
                        .and_then(Value::as_str)
                        .filter(|d| !d.trim().is_empty())
                    {
                        Some(detail) => format!("{name}: {detail}"),
                        None => format!("{name} did not hold"),
                    }
                });
            let said = judged
                .or(held)
                .or_else(|| c.error.clone())
                .unwrap_or_else(|| "did not pass".to_owned());
            json!({"case_id": c.case_id, "said": said})
        })
        .collect()
}

/// A run's terminal value, once it has one.
pub(crate) async fn wait_terminal(state: &AppState, run_id: &str) -> Value {
    loop {
        if let Some(info) = state.run_deps.manager.info(run_id).await {
            if let Some(terminal) = info.terminal {
                return terminal;
            }
        } else if let Ok(Some(snapshot)) = state.server_store.get_journal(run_id).await {
            // The process no longer holds it: its journal's last word.
            return json!({"status": crate::routes::journal_status(&snapshot.events)});
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tool_call_written_as_text_becomes_the_verdict() {
        let v = super::loose_verdict(
            r#"{"name": "submit_judgment", "arguments": {"score": 0.9, "rationale": "Fine."}}"#,
        )
        .unwrap();
        assert_eq!(v, serde_json::json!({"score": 0.9, "rationale": "Fine."}));
        let v = super::loose_verdict(
            "```json\n{\"score\": 0.2, \"rationale\": \"No.\", \"extra\": 1}\n```",
        )
        .unwrap();
        assert_eq!(v, serde_json::json!({"score": 0.2, "rationale": "No."}));
        assert!(super::loose_verdict("I think it is fine.").is_none());
        assert!(super::loose_verdict(r#"{"name": "other", "arguments": {}}"#).is_none());
    }
}
