//! Harness demo: two scripted ReAct agents that walk the harness surfaces
//! end to end — the composer lane (draft → approval-gated publish →
//! read-only CLI) and the self-improvement loop (introspect → backlog →
//! draft-and-stage) — plus an experiment evaluator, served on
//! `127.0.0.1:8110`.
//!
//! The models are scripted and deterministic (no network, no credentials):
//! each is a small state machine over the run's message tail, so every run
//! produces exact model-call and tool-call evidence that
//! `rusty-server/tests/harness_flows.rs` asserts against.
//!
//! The fixture day is 2026-02-09 (UTC): the self-improver's logical clock
//! and the seeded backlog entry pin their timestamps to it so journaled
//! evidence stays deterministic.
//!
//! Run with: `cargo run --example harness_demo`
//!
//! Test hooks (mirroring server_demo's `RUSTY_DEMO_*` discipline):
//! `RUSTY_HARNESS_ADDR` overrides the bind address and
//! `RUSTY_HARNESS_STORE` the JSON-file store directory.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::DateTime;
use rusty_agent_runtime::composer::{
    ComposeSkillTool, ComposeToolDefinitionTool, ComposerSession, PublishComposedSkillTool,
    publish_effect_id,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::learn::Candidate;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::self_improve::{
    BacklogEntry, BacklogProvenance, BacklogStatus, BacklogStore, BuildGapSkillTool,
    CapabilityInspection, FEATURE_CAPABILITY_SETS, HARNESS_PROVENANCE, InspectCapabilitiesTool,
    Plane, ProposeBacklogTool,
};
use rusty_agent_runtime::skill::{SkillPackage, SkillRegistry};
use rusty_agent_runtime::tool::builtins::cli::{CliPolicy, CliTool};
use rusty_agent_server::{
    ExperimentOutcome, GraphRegistry, ServerConfig, StudioExperimentConfig,
    StudioExperimentEvaluator, serve,
};
use rusty_eval::{Dataset, ExperimentReport};
use serde_json::{Value, json};

/// The fixture day the self-improver's logical clock and seeded backlog
/// pin their timestamps to. Fixed so journaled evidence stays a
/// documented, testable fact rather than a moving target.
const DEMO_DAY: &str = "2026-02-09";

// ---------------------------------------------------------------------------
// Scripted models
// ---------------------------------------------------------------------------

/// Where a run's work begins. Threads accumulate messages across runs, so
/// rounds are counted from the tail — the tool replies that follow the last
/// user message — never from the head of the channel.
fn turn_progress(messages: &[ChatMessage]) -> (String, Vec<String>) {
    let last_user = messages
        .iter()
        .rposition(|message| message.role == Role::User);
    let user = last_user
        .and_then(|index| messages[index].content.clone())
        .unwrap_or_default();
    let replies = messages
        .iter()
        .skip(last_user.map_or(0, |index| index + 1))
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.content.clone())
        .collect();
    (user, replies)
}

/// A tool reply the executor already marked failed (`ERROR: …`).
fn is_tool_error(reply: &str) -> bool {
    reply.starts_with("ERROR:")
}

fn respond(message: ChatMessage, model: &str) -> Result<ChatResponse> {
    Ok(ChatResponse {
        message,
        model: Some(model.to_owned()),
        usage: None,
    })
}

/// The composer studio: draft the standup-brief skill, publish it under the
/// pre-minted approval, then prove the CLI tool runs read-only. A
/// "disallowed" ask drives one refused `rm` call instead.
struct ComposerModel {
    /// The approval token for the exact draft this model composes, minted at
    /// startup against `publish_effect_id("composer-studio", hash)`.
    approval: Value,
}

/// The skill the composer drafts. Fixed so the publish approval can be
/// minted before any run starts.
const COMPOSED_NAME: &str = "daily-standup-brief";
const COMPOSED_DESCRIPTION: &str =
    "Turn a morning's inbox notes into a standup brief.";
const COMPOSED_BODY: &str =
    "# Standup Brief\n\nList yesterday, today, and blockers, one line each.\n";

/// The SKILL.md text `ComposeSkillTool` assembles for these exact args —
/// the hash must match byte for byte, so the demo builds it with the same
/// format string rather than approximating it.
fn composed_skill_md() -> String {
    format!(
        "---\nname: {COMPOSED_NAME}\ndescription: {COMPOSED_DESCRIPTION}\n---\n\n{COMPOSED_BODY}\n"
    )
}

/// Mint the publish approval the way an operator would: hash the exact
/// package, derive the scoped publish effect id, approve it by name.
fn precompute_publish_approval() -> Result<Value> {
    let mut files = BTreeMap::new();
    files.insert("SKILL.md".to_owned(), composed_skill_md().into_bytes());
    let package = SkillPackage::from_files(files).map_err(|error| {
        RustyError::Tool(format!("the composed demo skill must parse: {error}"))
    })?;
    let effect_id = publish_effect_id("composer-studio", &package.content_hash());
    let token = ApprovalToken::approve(effect_id, "ops:harness-demo");
    serde_json::to_value(token)
        .map_err(|error| RustyError::Tool(format!("the approval token must serialize: {error}")))
}

#[async_trait]
impl ChatModel for ComposerModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let (user, replies) = turn_progress(messages);
        let disallowed = user.to_lowercase().contains("disallowed");
        let message = match replies.len() {
            0 if disallowed => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_rm",
                "run_cli",
                json!({"program": "rm", "args": ["-rf", "."], "cwd": ".", "timeout_ms": 1000}),
            )]),
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_compose",
                "compose_skill",
                json!({
                    "name": COMPOSED_NAME,
                    "description": COMPOSED_DESCRIPTION,
                    "body": COMPOSED_BODY,
                    "author": "agent:rusty"
                }),
            )]),
            1 if disallowed => ChatMessage::assistant(format!(
                "Refused: `run_cli` declined the command ({}) — only allowlisted, read-only programs run from this graph.",
                replies[0]
            )),
            1 => {
                let receipt = serde_json::from_str::<Value>(&replies[0]).unwrap_or(Value::Null);
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "call_publish",
                    "publish_composed_skill",
                    json!({
                        "content_hash": receipt["content_hash"].as_str().unwrap_or(""),
                        "approval": self.approval.clone()
                    }),
                )])
            }
            2 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_ls",
                "run_cli",
                json!({"program": "ls"}),
            )]),
            _ => {
                let receipt = serde_json::from_str::<Value>(&replies[1]).unwrap_or(Value::Null);
                ChatMessage::assistant(format!(
                    "Composed and published `{}` at revision {} (approved by {}), and listed the skills directory read-only.",
                    receipt["name"].as_str().unwrap_or(COMPOSED_NAME),
                    receipt["revision"].as_i64().unwrap_or(1),
                    receipt["approved_by"]
                        .as_str()
                        .unwrap_or("ops:harness-demo")
                ))
            }
        };
        respond(message, "rusty-harness-composer")
    }
}

// ---------------------------------------------------------------------------
// The self-improvement loop
// ---------------------------------------------------------------------------

/// The gaps the scripted self-improver records backlog entries for — picked
/// by id (not report position) so the journey asserts intent, not accident.
const SELF_IMPROVE_GAPS: [&str; 3] = [
    "surface-compaction",
    "telemetry-ledger",
    "agent-session-query",
];

/// The runbook skill the loop drafts for its pre-approved entry. The gap
/// (`operator-runbooks`) flips to `Present` only once a `runbook-*` skill is
/// really registered, so a staged-but-unpublished draft honestly changes
/// nothing in the next inspection.
const RUNBOOK_NAME: &str = "runbook-incident-review";
const RUNBOOK_DESCRIPTION: &str =
    "Review the open high-priority incidents and file a theme summary.";
const RUNBOOK_BODY: &str = "# Incident Review\n\n1. List the open priority-1 incidents.\n2. Group them by category and name the top theme.\n3. File a KB draft summarizing the theme.\n";

/// The seeded entry the demo operator pre-approves at startup (title and
/// rationale are the entry's identity — keep them byte-stable so restarts
/// converge on the same content-derived id).
const RUNBOOK_ENTRY_TITLE: &str = "Ship the incident-review runbook skill";
const RUNBOOK_ENTRY_RATIONALE: &str = "operator-runbooks is Absent: no `runbook-*` skill is registered, and the incident-review \
     workflow recurs across sessions — it belongs in a governed, scanned package.";

/// The self-improver: introspect the demo's own registries, record backlog
/// entries for the top gaps, then draft the approved runbook entry's skill
/// through the composer and stage its publish. The loop never publishes —
/// the approval gate stays with the operator, and the final message says so.
struct SelfImproverModel;

#[async_trait]
impl ChatModel for SelfImproverModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let (_user, replies) = turn_progress(messages);
        let message = match replies.len() {
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_inspect",
                "inspect_capabilities",
                json!({}),
            )]),
            1 => {
                let report = &replies[0];
                if is_tool_error(report) {
                    ChatMessage::assistant(format!(
                        "I couldn't introspect the harness ({report}); without the gap report there is nothing honest to record."
                    ))
                } else {
                    let report = serde_json::from_str::<Value>(report).unwrap_or(Value::Null);
                    let entries: Vec<Value> = SELF_IMPROVE_GAPS
                        .iter()
                        .map(|gap| {
                            let description = report["assessments"]
                                .as_array()
                                .and_then(|assessments| {
                                    assessments.iter().find(|a| a["id"] == json!(gap))
                                })
                                .and_then(|a| a["description"].as_str())
                                .unwrap_or(gap);
                            json!({
                                "title": format!("Close the `{gap}` gap"),
                                "rationale": description,
                                "gap_ids": [gap]
                            })
                        })
                        .collect();
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "call_propose",
                        "propose_backlog_entries",
                        json!({"entries": entries}),
                    )])
                }
            }
            2 => {
                let recorded = &replies[1];
                if is_tool_error(recorded) {
                    ChatMessage::assistant(format!(
                        "The backlog refused my proposals ({recorded}); I won't draft against an unrecorded gap."
                    ))
                } else {
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "call_build",
                        "build_gap_skill",
                        json!({
                            "gap_id": "operator-runbooks",
                            "name": RUNBOOK_NAME,
                            "description": RUNBOOK_DESCRIPTION,
                            "body": RUNBOOK_BODY,
                            "author": HARNESS_PROVENANCE
                        }),
                    )])
                }
            }
            _ => {
                let staged = &replies[2];
                if is_tool_error(staged) {
                    ChatMessage::assistant(format!(
                        "The runbook draft was refused ({staged}); nothing was staged, and the gap stays open."
                    ))
                } else {
                    let report = serde_json::from_str::<Value>(&replies[0]).unwrap_or(Value::Null);
                    let recorded =
                        serde_json::from_str::<Value>(&replies[1]).unwrap_or(Value::Null);
                    let staged = serde_json::from_str::<Value>(staged).unwrap_or(Value::Null);
                    ChatMessage::assistant(format!(
                        "Inspection: {} present, {} partial, {} absent. Recorded {} backlog entries \
                         (harness:self-improve, all `proposed`). Drafted `{RUNBOOK_NAME}` for the \
                         approved runbook entry — publish is staged behind effect id {} and awaits \
                         an operator approval; the gate stays with the operator.",
                        report["present"].as_u64().unwrap_or(0),
                        report["partial"].as_u64().unwrap_or(0),
                        report["absent"].as_u64().unwrap_or(0),
                        recorded["recorded"].as_array().map_or(0, Vec::len),
                        staged["publish_effect_id"].as_str().unwrap_or("(none)")
                    ))
                }
            }
        };
        respond(message, "rusty-harness-self-improver")
    }
}

// ---------------------------------------------------------------------------
// Experiment evaluator
// ---------------------------------------------------------------------------

/// A deterministic all-pass evaluator: the demo's experiment lane proves
/// the dataset → candidate → experiment → comparison plumbing end to end
/// without depending on model behavior. Both reports are identical, so a
/// regression verdict would be a comparison bug, not a flake.
#[derive(Debug)]
struct HarnessEvaluator;

/// The exact `ExperimentReport` wire shape, built as JSON and deserialized
/// so a schema drift fails here at startup instead of inside the lane.
fn harness_report(
    dataset: &Dataset,
    config: &StudioExperimentConfig,
    name: &str,
) -> ExperimentReport {
    let cases: Vec<Value> = dataset
        .cases()
        .iter()
        .map(|case| {
            let runs: Vec<Value> = (0..config.runs_per_case)
                .map(|repetition| {
                    json!({
                        "repetition": repetition,
                        "status": {"status": "done"},
                        "passed": true,
                        "assertions": [],
                        "judge": null,
                        "tool_calls": 0,
                        "latency_ms": 10,
                        "cost_usd": 0.001,
                        "total_tokens": 10
                    })
                })
                .collect();
            json!({
                "case_id": case.id,
                "tags": case.tags,
                "pass_rate": 1.0,
                "runs": runs
            })
        })
        .collect();
    let total_runs = dataset.cases().len() * config.runs_per_case;
    serde_json::from_value(json!({
        "format_version": 1,
        "name": name,
        "dataset_name": dataset.name(),
        "dataset_version": dataset.version(),
        "runs_per_case": config.runs_per_case,
        "max_concurrency": config.max_concurrency,
        "cases": cases,
        "summary": {
            "cases": dataset.cases().len(), "runs": total_runs, "runs_passed": total_runs,
            "run_pass_rate": 1.0, "case_pass_rate": 1.0,
            "assertions": [],
            "latency_ms": {"min": 10, "p50": 10, "p95": 10, "max": 10, "mean": 10.0},
            "total_cost_usd": total_runs as f64 * 0.001,
            "total_tokens": total_runs * 10
        }
    }))
    .expect("the harness report shape matches ExperimentReport")
}

#[async_trait]
impl StudioExperimentEvaluator for HarnessEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        dataset: &Dataset,
        config: &StudioExperimentConfig,
    ) -> std::result::Result<ExperimentOutcome, String> {
        Ok(ExperimentOutcome {
            baseline_report: harness_report(dataset, config, "serving-baseline"),
            candidate_report: harness_report(dataset, config, "candidate"),
        })
    }
}

// ---------------------------------------------------------------------------
// Graph assembly
// ---------------------------------------------------------------------------
/// `composer_studio`: the compose → approval-gated publish lane plus a
/// read-only, allowlisted `run_cli` jailed to the skills directory.
fn build_composer_graph(
    skills: Arc<Mutex<SkillRegistry>>,
) -> Result<(Graph, StateSpec, ToolRegistry)> {
    let session = ComposerSession::new("composer-studio");
    let mut tools = ToolRegistry::new();
    tools.register(ComposeSkillTool::new(session.clone()));
    tools.register(PublishComposedSkillTool::new(session, skills));
    tools.register(ComposeToolDefinitionTool::new(vec!["ls".to_owned()])?);
    let cli_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/harness_skills");
    tools.register(CliTool::new(
        CliPolicy::new(cli_root, ["ls"])?.with_read_only(true),
    ));
    let model: Arc<dyn ChatModel> = Arc::new(ComposerModel {
        approval: precompute_publish_approval()?,
    });
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// `self_improver`: the introspect → backlog → draft-and-stage loop. The
/// inspection closure is the honesty seam — it reads the demo's live skill
/// registry and the tool list assembled from the demo's real registries,
/// so the gap report can never claim more than the demo wires.
/// The backlog store is seeded (in `main`) with one operator-approved
/// runbook entry; everything else the loop proposes lands as `proposed`.
fn build_self_improver_graph(
    backlog: Arc<BacklogStore>,
    skills: Arc<Mutex<SkillRegistry>>,
    host_tool_names: Vec<String>,
) -> Result<(Graph, StateSpec, ToolRegistry)> {
    let session = ComposerSession::new("self-improver");
    let mut tool_names = host_tool_names;
    tool_names.extend(
        [
            "inspect_capabilities",
            "propose_backlog_entries",
            "build_gap_skill",
        ]
        .iter()
        .map(|name| (*name).to_owned()),
    );
    let inspect = Arc::new(move || CapabilityInspection {
        skill_names: skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .names()
            .map(str::to_owned)
            .collect(),
        tool_names: tool_names.clone(),
        // The demo binary wires these planes for real: the registries and
        // tools above, the server's memory/knowledge endpoints, the Flight
        // Recorder journaling every run, and per-run tool allowlists.
        planes: vec![
            Plane::Skills,
            Plane::Knowledge,
            Plane::Memory,
            Plane::Evidence,
            Plane::Tools,
        ],
        features: vec![FEATURE_CAPABILITY_SETS.to_owned()],
    });
    let mut tools = ToolRegistry::new();
    tools.register(InspectCapabilitiesTool::new(inspect));
    tools.register(ProposeBacklogTool::new(
        Arc::clone(&backlog),
        // A logical clock on the fixture day keeps the demo's journaled
        // evidence deterministic.
        Clock::logical(
            DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T09:00:00Z"))
                .expect("the fixture day parses")
                .timestamp_millis() as u64,
            60_000,
        ),
    ));
    tools.register(BuildGapSkillTool::new(backlog, session));
    let model: Arc<dyn ChatModel> = Arc::new(SelfImproverModel);
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// Seed the backlog with the demo operator's one pre-approved entry —
/// converging on restart (insertion is idempotent; an entry that already
/// moved on is left where it is).
async fn seed_backlog(backlog: &BacklogStore) -> Result<()> {
    let proposed = BacklogEntry::new(
        RUNBOOK_ENTRY_TITLE,
        RUNBOOK_ENTRY_RATIONALE,
        &["operator-runbooks".to_owned()],
        BacklogProvenance::operator("harness-demo")?,
        DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T08:00:00Z"))
            .expect("the fixture day parses")
            .with_timezone(&chrono::Utc),
    )?;
    if backlog.get(&proposed.id).is_none() {
        backlog.insert(proposed.clone()).await?;
        backlog
            .transition(
                &proposed.id,
                BacklogStatus::Approved,
                None,
                DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T08:05:00Z"))
                    .expect("the fixture day parses")
                    .with_timezone(&chrono::Utc),
            )
            .await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let skills = Arc::new(Mutex::new(SkillRegistry::new()));
    let (composer, composer_spec, composer_tools) = build_composer_graph(Arc::clone(&skills))?;

    // The self-improver's backlog lives under the same store root the
    // checkpointer uses; the operator's pre-approved runbook entry is
    // seeded before the graph is built.
    let store_root = std::env::var("RUSTY_HARNESS_STORE")
        .unwrap_or_else(|_| "./data/harness-demo-checkpoints".to_string());
    let backlog = Arc::new(
        BacklogStore::open(Path::new(&store_root).join("self-improve-backlog.json")).await?,
    );
    seed_backlog(&backlog).await?;
    let host_tool_names: Vec<String> = composer_tools.names().map(str::to_owned).collect();
    let (self_improver, self_improver_spec, self_improver_tools) =
        build_self_improver_graph(backlog, skills, host_tool_names)?;

    let mut registry = GraphRegistry::new();
    registry.register_with_tools("composer_studio", composer, composer_spec, &composer_tools)?;
    registry.register_with_tools(
        "self_improver",
        self_improver,
        self_improver_spec,
        &self_improver_tools,
    )?;

    let config = ServerConfig::new(
        std::env::var("RUSTY_HARNESS_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8110".to_string())
            .parse()
            .expect("RUSTY_HARNESS_ADDR must be a socket address"),
        std::env::var("RUSTY_HARNESS_STORE")
            .unwrap_or_else(|_| "./data/harness-demo-checkpoints".to_string()),
    )
    .with_studio_experiment_evaluator(Arc::new(HarnessEvaluator));

    // The menu below is printed with the *actual* address so the test-hook
    // override stays honest when a human runs the demo with it set.
    let base = format!("localhost:{}", config.bind_addr.port());
    println!("\nrusty harness demo on http://{base}\n");
    println!("  Graphs: composer_studio, self_improver");
    println!("  The models are scripted (no network, no credentials); every run");
    println!("  produces exact journaled evidence the flow test asserts against.\n");
    println!("  # liveness + registered graphs and their tool catalogs");
    println!("  curl {base}/ok");
    println!("  curl {base}/info | jq\n");
    println!("  # the composer drafts a skill, publishes it under its pre-minted");
    println!("  # approval, and proves run_cli is read-only and allowlisted");
    println!("  STUDIO=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"composer_studio\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$STUDIO/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Compose the standup brief skill, publish it, and list the skills directory.\"}}]}}}}' | jq\n"
    );
    println!("  # the self-improver introspects its own capabilities, records backlog");
    println!("  # entries for the top gaps, and stages a runbook skill behind the");
    println!("  # composer's approval gate (publishing stays with the operator)");
    println!("  LOOP=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"self_improver\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$LOOP/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!(
        "    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Inspect your capabilities, record the top gaps, and stage the runbook skill.\"}}]}}}}' | jq\n"
    );
    println!("  # every run's journaled evidence (run_id is in the terminal JSON)");
    println!("  curl -s {base}/runs/$RUN_ID/events | jq\n");
    println!("  # the governed surfaces the flow test drives: skills, memory,");
    println!("  # knowledge, datasets / candidates / experiments");
    println!("  curl -s {base}/skills | jq");
    println!("  curl -s -X POST {base}/memory/query \\");
    println!("    -H 'content-type: application/json' -d '{{}}' | jq");
    println!("  curl -s {base}/experiments | jq\n");

    serve(registry, config).await?;
    Ok(())
}
