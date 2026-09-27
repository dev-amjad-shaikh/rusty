//! The platform as tools: what the Composer builds with.
//!
//! The Composer is an ordinary agent on the same engine as every other —
//! a charter, an allow-list, the ReAct loop, the journal, the verifier. What
//! makes it the Composer is its tools: they open the platform's own doors.
//! It reads the catalog (`catalog.*`), creates agents (`agents.create`),
//! registers skills (`skills.register`) and describes connectors
//! (`connectors.register`) through exactly the paths the studio uses, so
//! what it builds is what a person would have built by hand — versioned,
//! attributed, reviewable in the studio.
//!
//! Some doors stay shut to it on purpose: it cannot spawn an MCP server
//! (a process on the host is an operator's decision), cannot configure a
//! connection (credentials are a person's), and cannot hand these platform
//! tools to the agents it creates unless told to. When it needs any of
//! those, it says so — that is the point of asking it.
//!
//! Catalog answers are brief by design. The first Composer run showed why:
//! a twenty-thousand-character skills dump filled the context, compaction
//! summarized it away, and the model asked for it again until the
//! stuck-turn detector stopped the run. A catalog the model can hold whole
//! is one it reads once.
//!
//! [`PlatformTools`] is a live [`ToolSource`] beside `ConnectionTools` and
//! `McpTools`; the server fills it at boot with tools that hold a weak
//! handle to the application state. Only the default tenant is served,
//! the same limit the other cells name.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock, Weak};

/// Runs that read content from outside — a fetched web page — by run id,
/// with what they read. A note written later in such a run is marked
/// untrusted by the runtime; the model has no say in it. One run is one
/// turn, so the mark lasts until the next person message starts a new run.
static TAINTED_RUNS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

pub(crate) fn taint_run(run_id: &str, what: String) {
    let mut runs = TAINTED_RUNS.lock().unwrap_or_else(|e| e.into_inner());
    if runs.len() > 10_000 {
        runs.clear();
    }
    runs.insert(run_id.to_owned(), what);
}

pub(crate) fn run_taint(run_id: &str) -> Option<String> {
    TAINTED_RUNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run_id)
        .cloned()
}

use async_trait::async_trait;
use chrono::Utc;
use rusty_agent_runtime::connector::ConnectorManifest;
use serde_json::{json, Value};

use crate::auth::{scope_id, TenantContext, DEFAULT_TENANT};
use crate::routes::AppState;
use rusty_agent_runtime::context::SkillSectionEntry;
use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::skill::{SkillPackage, SkillSource};
use rusty_agent_runtime::tool::{Tool, ToolSource};

/// The name the seeded Composer agent carries.
pub const COMPOSER_NAME: &str = "Composer";

/// The platform's second agent: reads what an agent's runs say and files a
/// better charter as a version a person activates.
pub const COACH_NAME: &str = "Coach";
pub const AGENTS_READ: &str = "agents.read";
pub const RUNS_REVIEW: &str = "runs.review";
pub const AGENTS_REVISE: &str = "agents.revise";
pub const GAPS_FILE: &str = "gaps.file";
/// An agent delegates a durable outcome to another agent, as a person
/// would from the studio: rounds until done, the record kept, in the chain.
pub const ASSIGNMENT_CREATE: &str = "assignment.create";
/// The plan an agent writes and keeps current; the loop renders it each
/// call and checks it before a final answer.
pub const PLAN: &str = rusty_agent_runtime::react::PLAN_TOOL;
/// The gap backlog in priority order, for an agent that picks work from it.
pub const GAPS_WORK_ORDER: &str = "gaps.work_order";
/// A run claims a gap it answered; the claim closes on the run's verdict.
pub const GAPS_RESOLVE: &str = "gaps.resolve";
/// Agents can start work: one task into a named pool, worked later by an
/// agent configured for that pool as its own run.
pub const TASKS_ENQUEUE: &str = "tasks.enqueue";
/// How deep a chain of work an agent may start: a task's run may queue
/// tasks, whose runs may queue tasks, to this depth and no further.
pub const MAX_CHAIN_DEPTH: u32 = 3;

/// The curated-memory tool: edit one of the agent's declared blocks.
pub const MEMORY_BLOCK_EDIT: &str = "memory.block_edit";

/// A declared memory block: a labelled, described, size-limited slot that
/// exists before it has content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DeclaredBlock {
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_block_limit")]
    pub char_limit: usize,
}

fn default_block_limit() -> usize {
    1_500
}

/// The blocks a run declares: the working copy's own list when the run
/// carries one (a draft under test), else the agent's published config.
pub fn declared_blocks_for_run(
    config: &Value,
    declared_override: Option<&Vec<Value>>,
) -> Vec<DeclaredBlock> {
    match declared_override {
        Some(list) => declared_blocks(&json!({"studio_intent": {"memory": {"blocks": list}}})),
        None => declared_blocks(config),
    }
}

/// The platform's default blocks, mounted on every agent that has memory;
/// the builder may declare more under `studio_intent.memory.blocks`.
pub fn default_blocks() -> Vec<DeclaredBlock> {
    vec![
        DeclaredBlock { label: "person".into(), description: "Who the agent is talking to, in their words: name, role, team, how they like things done.".into(), char_limit: 1_200 },
        DeclaredBlock { label: "working".into(), description: "What is in the middle of being done: the current task, its state, what is next.".into(), char_limit: 1_500 },
        DeclaredBlock { label: "decisions".into(), description: "What was decided and why, one line each; a decision that stands until it is superseded here.".into(), char_limit: 1_500 },
    ]
}

/// The blocks an assistant declares: the defaults, then any the builder
/// added or resized under `studio_intent.memory.blocks[]` (`label`,
/// `description`, `char_limit`).
pub fn declared_blocks(config: &Value) -> Vec<DeclaredBlock> {
    let mut blocks = default_blocks();
    if let Some(list) = config
        .pointer("/studio_intent/memory/blocks")
        .and_then(Value::as_array)
    {
        for b in list {
            let Some(label) = b
                .get("label")
                .and_then(Value::as_str)
                .map(|l| l.trim().to_lowercase())
                .filter(|l| block_label_ok(l))
            else {
                continue;
            };
            let description = b
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let char_limit = b
                .get("char_limit")
                .and_then(Value::as_u64)
                .map(|n| (n as usize).clamp(100, 8_000));
            if let Some(existing) = blocks.iter_mut().find(|d| d.label == label) {
                if !description.is_empty() {
                    existing.description = description;
                }
                if let Some(limit) = char_limit {
                    existing.char_limit = limit;
                }
            } else {
                blocks.push(DeclaredBlock {
                    label,
                    description,
                    char_limit: char_limit.unwrap_or(1_500),
                });
            }
        }
    }
    blocks
}

pub(crate) fn block_label_ok(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 32
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The default limit of a block declared without one.
pub(crate) const DEFAULT_BLOCK_CHAR_LIMIT: usize = 1_500;

/// Every live `block.*` note in the agent's scope, the newest per label:
/// what the agent holds, declared on the record or not.
pub(crate) async fn block_records(
    state: &AppState,
    tenant: &str,
    agent_id: &str,
) -> Result<Vec<rusty_agent_runtime::memory::MemoryRecord>> {
    use rusty_agent_runtime::memory::{MemoryQuery, MemoryScope, ScopeAddress};
    let live = state
        .server_store
        .query_memory(
            tenant,
            &MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::Agent, agent_id)),
                ..Default::default()
            },
            Utc::now(),
        )
        .await
        .map_err(|e| tool_err(e.to_string()))?;
    let mut newest: std::collections::BTreeMap<String, rusty_agent_runtime::memory::MemoryRecord> =
        std::collections::BTreeMap::new();
    for r in live
        .into_iter()
        .filter(|r| r.key.as_deref().is_some_and(|k| k.starts_with("block.")))
    {
        let key = r.key.clone().unwrap_or_default();
        let replace = newest.get(&key).is_none_or(|held| {
            (r.created_at, r.memory_id.clone()) > (held.created_at, held.memory_id.clone())
        });
        if replace {
            newest.insert(key, r);
        }
    }
    Ok(newest.into_values().collect())
}

/// The newest live note under `block.<label>` in the agent's scope: the
/// block's value, and the record it is.
pub(crate) async fn block_record(
    state: &AppState,
    tenant: &str,
    agent_id: &str,
    label: &str,
) -> Result<Option<rusty_agent_runtime::memory::MemoryRecord>> {
    use rusty_agent_runtime::memory::{MemoryQuery, MemoryScope, ScopeAddress};
    let live = state
        .server_store
        .query_memory(
            tenant,
            &MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::Agent, agent_id)),
                key: Some(format!("block.{label}")),
                ..Default::default()
            },
            Utc::now(),
        )
        .await
        .map_err(tool_err)?;
    Ok(live.into_iter().max_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    }))
}

pub(crate) fn block_value(record: &rusty_agent_runtime::memory::MemoryRecord) -> String {
    match &record.content {
        rusty_agent_runtime::record::PayloadRef::Inline(v) => v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => String::new(),
    }
}

/// The external agent id of a stored assistant: the record's id with the
/// tenant scope stripped, whichever separator the scope used.
/// The agent's id as the tenant sees it, from the store's scoped id.
pub(crate) fn agent_id_of<'a>(tenant: &str, scoped: &'a str) -> &'a str {
    external_agent_id(tenant, scoped)
}

fn external_agent_id<'a>(tenant: &str, scoped: &'a str) -> &'a str {
    scoped
        .strip_prefix(&format!("{tenant}/"))
        .or_else(|| scoped.strip_prefix(&format!("{tenant}:")))
        .unwrap_or(scoped)
}

/// The blocks rendered for a run's situation section: label, description,
/// value — in declaration order, byte-identical for identical blocks.
pub(crate) async fn memory_blocks_text(
    state: &AppState,
    tenant: &str,
    assistant: &crate::assistants::AssistantRecord,
    declared: Vec<DeclaredBlock>,
) -> Option<String> {
    let agent_id = external_agent_id(tenant, &assistant.assistant_id);
    let mut out = String::from(
        "# Memory blocks\nCurated notes this agent keeps, refreshed at the start of every run; edit them with memory.block_edit (add, replace or remove one line at a time; the change shows from the next run).",
    );
    for block in declared {
        let record = block_record(state, tenant, agent_id, &block.label)
            .await
            .ok()
            .flatten();
        let value = record.as_ref().map(block_value).unwrap_or_default();
        out.push_str(&format!("\n\n## {} — {}\n", block.label, block.description));
        if value.trim().is_empty() {
            out.push_str("[empty]");
        } else {
            out.push_str(value.trim());
        }
    }
    Some(out)
}

/// Write a block's new value as the newest live note under `block.<label>`
/// in the agent's scope, superseding the previous one — the tool's write
/// and a person's write in the studio are the same record.
// The write carries the record's identity fields as explicit parameters:
// grouping them into a struct would move the construction cost onto every
// caller that builds a fresh record each time.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_block(
    state: &AppState,
    tenant: &str,
    agent_id: &str,
    label: &str,
    value: &str,
    author: rusty_agent_runtime::memory::ProvenanceAuthor,
    previous: Option<&rusty_agent_runtime::memory::MemoryRecord>,
    untrusted: bool,
) -> Result<rusty_agent_runtime::memory::MemoryRecord> {
    use rusty_agent_runtime::memory::{
        MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, ScopeAddress, ValidityWindow,
    };
    let now = Utc::now();
    let confidence = if matches!(
        author,
        rusty_agent_runtime::memory::ProvenanceAuthor::Human { .. }
    ) {
        1.0
    } else {
        0.95
    };
    let mut record = MemoryRecord::new(
        MemoryKind::Fact,
        ScopeAddress::new(MemoryScope::Agent, agent_id),
        MemoryProvenance {
            author,
            evidence: Default::default(),
            written_at: now,
        },
        confidence,
        ValidityWindow {
            valid_from: now,
            valid_until: None,
        },
        now,
        json!({ "text": value }),
    )
    .map_err(|e| tool_err(e.to_string()))?
    .with_key(format!("block.{label}"))
    .with_priority(10)
    .with_tags(if untrusted {
        vec!["block", rusty_agent_runtime::memory::ORIGIN_UNTRUSTED_TAG]
    } else {
        vec!["block"]
    });
    if let Some(prev) = previous {
        record = record.with_supersedes(prev.memory_id.clone());
    }
    state
        .server_store
        .put_memory(tenant, &record, &json!({ "text": value }))
        .await
        .map_err(|e| tool_err(e.to_string()))?;
    Ok(record)
}

struct MemoryBlockEdit(Doors);

#[async_trait]
impl Tool for MemoryBlockEdit {
    fn name(&self) -> &str {
        MEMORY_BLOCK_EDIT
    }
    fn description(&self) -> &str {
        "Edit one of your curated memory blocks — the labelled notes shown under # Memory blocks at the start of every run (person, working, decisions, and any your builder declared). One line per entry: add a line, replace a line (matched by a distinctive substring), or remove one. Each block has a character limit; an edit past it is refused with the limit named, so keep entries short and remove what is stale. The block is durable at once and shows in the prompt from the next run."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {"type": "string", "description": "The block: person, working, decisions, or a declared one."},
                "action": {"type": "string", "enum": ["add", "replace", "remove"], "description": "add a line; replace the line matching `match`; remove the line matching `match`."},
                "match": {"type": "string", "description": "For replace and remove: a distinctive substring of exactly one existing line."},
                "content": {"type": "string", "description": "For add and replace: the line, one sentence, as the person said it."}
            },
            "required": ["label", "action"]
        })
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> Result<Value> {
        use rusty_agent_runtime::memory::ProvenanceAuthor;
        let state = self.0.open()?;
        let run = rusty_agent_runtime::tool::current_run()
            .ok_or_else(|| tool_err("memory.block_edit only works inside a run"))?;
        let Some(agent_id) = run.agent_id.clone() else {
            return Err(tool_err("this run is of no agent — no blocks to edit"));
        };
        let label = text(&args, "label").to_lowercase();
        if !block_label_ok(&label) {
            return Err(tool_err(
                "label: lowercase letters, digits, _ or -, at most 32 characters",
            ));
        }
        let tenant_ctx = context_now();
        // The working copy under test declares its own blocks; a run of the
        // published agent reads the published declaration.
        let declared_override = state
            .run_deps
            .manager
            .snapshot(&run.run_id)
            .await
            .and_then(|s| {
                s.payload
                    .config
                    .as_ref()
                    .and_then(|c| c.memory_blocks_declared.clone())
            });
        let declared = find_assistant(&state, &tenant_ctx, &agent_id)
            .await?
            .map(|a| declared_blocks_for_run(&a.config, declared_override.as_ref()))
            .unwrap_or_else(default_blocks);
        let Some(block) = declared.into_iter().find(|b| b.label == label) else {
            return Ok(
                json!({"ok": false, "label": label, "note": "no block by that label is declared for this agent; the blocks are named under # Memory blocks"}),
            );
        };
        let action = text(&args, "action");
        let content = text(&args, "content");
        // A block is shown in every later run and read by people: a line
        // carrying a credential is refused, as memory.remember refuses it.
        if let Some(what) = rusty_agent_runtime::memory::looks_like_secret(&content) {
            return Ok(
                json!({"ok": false, "label": label, "note": format!("not written: that line carries {what}. Credentials live in connections, never in memory; write the fact around it without the value.")}),
            );
        }
        let matcher = text(&args, "match").to_lowercase();
        let tenant_owned = tenant_now();
        let tenant = tenant_owned.as_str();
        let previous = block_record(&state, tenant, &agent_id, &label).await?;
        let mut lines: Vec<String> = previous
            .as_ref()
            .map(block_value)
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect();
        match action.as_str() {
            "add" => {
                if content.is_empty() {
                    return Err(tool_err("add needs `content`: the line to add"));
                }
                if lines.iter().any(|l| l.eq_ignore_ascii_case(content.trim())) {
                    return Ok(
                        json!({"ok": true, "label": label, "changed": false, "note": "that line is already in the block"}),
                    );
                }
                lines.push(content.trim().to_owned());
            }
            "replace" | "remove" => {
                if matcher.is_empty() {
                    return Err(tool_err(format!(
                        "{action} needs `match`: a distinctive substring of one existing line"
                    )));
                }
                let hits: Vec<usize> = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.to_lowercase().contains(&matcher))
                    .map(|(i, _)| i)
                    .collect();
                match hits.as_slice() {
                    [] => {
                        return Ok(
                            json!({"ok": false, "label": label, "note": "no line in the block contains that match; the block's lines are shown under # Memory blocks"}),
                        );
                    }
                    [one] => {
                        if action == "replace" {
                            if content.is_empty() {
                                return Err(tool_err("replace needs `content`: the new line"));
                            }
                            lines[*one] = content.trim().to_owned();
                        } else {
                            lines.remove(*one);
                        }
                    }
                    many => {
                        return Ok(
                            json!({"ok": false, "label": label, "note": format!("{} lines contain that match; use a more distinctive one", many.len())}),
                        );
                    }
                }
            }
            other => {
                return Err(tool_err(format!(
                    "action must be add, replace or remove, not `{other}`"
                )));
            }
        }
        let value = lines.join("\n");
        if value.chars().count() > block.char_limit {
            return Ok(json!({
                "ok": false,
                "label": label,
                "limit": block.char_limit,
                "attempted": value.chars().count(),
                "note": format!("the block would hold {} characters and its limit is {}; remove or shorten a line first", value.chars().count(), block.char_limit),
            }));
        }
        let untrusted = run_taint(&run.run_id).is_some();
        let record = write_block(
            &state,
            tenant,
            &agent_id,
            &label,
            &value,
            ProvenanceAuthor::Agent {
                agent_id: agent_id.clone(),
            },
            previous.as_ref(),
            untrusted,
        )
        .await?;
        if let Some(journal) = &run.journal {
            let draft = rusty_agent_runtime::journal::EventDraft::new(rusty_agent_runtime::record::RunEventKind::MemoryWrite, Effect::Idempotent)
                .input(json!({ "effect_key": format!("memory:agent:{}:{}", agent_id, record.memory_id), "memory_id": record.memory_id }))
                .output(serde_json::to_value(&record).unwrap_or(Value::Null));
            journal.record(draft);
        }
        Ok(json!({
            "ok": true,
            "label": label,
            "changed": true,
            "version": record.memory_id,
            "supersedes": previous.as_ref().map(|p| p.memory_id.clone()),
            "lines": lines.len(),
            "chars": value.chars().count(),
            "limit": block.char_limit,
            "note": if untrusted { "durable now, marked as written after reading outside content: the block shows as unverified from the next run" } else { "durable now; the block shows this from the next run" },
            "origin": if untrusted { "untrusted" } else { "agent" },
        }))
    }
}
pub const KNOWLEDGE_READ: &str = "knowledge.read";
pub const RUNS_READ: &str = "runs.read";
/// The delegation primitive every agent may hold: ask another agent, as
/// the same person, and get its answer back.
pub const AGENTS_ASK: &str = "agents.ask";
/// How long an ask waits for the other agent before answering "still
/// running" with the run id.
pub const COACH_TOOLS: [&str; 9] = [
    "agents.list",
    AGENTS_READ,
    RUNS_REVIEW,
    RUNS_READ,
    AGENTS_REVISE,
    "catalog.tools",
    "catalog.connectors",
    "catalog.skills",
    "skills.register",
];

/// The Coach's charter, pinned like the Composer's.
pub const COACH_CHARTER: &str = "You are the Coach: you make an agent better from what its runs say. \
Given an agent's name or id:\n\
1. agents.read — its charter, tools and skills.\n\
2. runs.review — its recent runs with the verifier's verdicts. Read the failed ones first: what \
was asked, what the run did, why the verifier said it fell short. Read the verified ones too, so \
the change keeps what works.\n\
3. Name the pattern behind the failures in two sentences — one cause, not a list.\n\
4. Write one revised charter that removes the cause. It must contain at least one sentence the \
current charter does not: the step the failed runs skipped, in imperative form, first among the \
things it must do (\"Before answering anything, call X with …\"), or the rule they misread, \
reworded with a worked example. Keep the agent's job, its tools, and every rule that passed. \
Keep it as short as the original.\n\
5. File it with agents.revise, the pattern as `why`. Give the change, not the whole text: \
`add_first` for the step the failed runs skipped (with the tool's arguments in it, e.g. the \
table and the query's shape from tools_detail), `replace` for a sentence they misread. It \
becomes a version a person activates in Improve; you never activate.\n\
A refusal from agents.revise is an answer, not a retry: never send the same arguments again — change \
the change, or stop and report what you found. A step the charter already states and the model did \
not follow is the model's slip, not the charter's gap: say so and file nothing, or add a worked \
example of the failed case. \
Answer with the pattern, the sentence you added or changed (quoted), and the version id. No failed runs: say so \
and file nothing. Every failed run older than the charter that runs now (under_current_charter \
false) and the current runs passing: the charter was already changed for it — say so and file \
nothing; agents.revise refuses in that case with the numbers, and the numbers are your answer. Failures that come from a tool, a system or the model rather than the charter: say that \
and file nothing. runs.review lists the agent's open gaps by class: a connector or tool the \
platform found missing from what its skills declare (connectors_missing, tools_missing) is not a \
charter fault — name the missing connection in your answer and file nothing for it. Never file a charter identical to the one that runs, nor one a person declined: \
agents.read lists the proposals already filed and the ones declined with the person's reason. A \
decline's reason is a fact the next change must answer; when it says the runs are right, file \
nothing.\n\
A person may ask for a capability rather than a fix — a tool the agent should call, a skill it \
should follow. Then: catalog.tools for the platform's tools and catalog.connectors for a \
connected system's operations (named `connector.operation`, e.g. servicenow.create-record) — \
exact names only, never invented; a procedure the person describes becomes a skill with \
skills.register (name, when to use it, the method, the tools it calls), then agents.revise with \
add_tools and add_skills, and add_first for the step that says when the agent reaches for them. \
One revision carries all of it; a tool that is not on the platform is something a person must \
connect first — say which. Nothing you add runs until a person applies it.";

/// The platform's third agent: reads what the followers of a skill did in
/// their verified runs and files a better procedure as a revision the
/// gate holds until the followers' suites pass — a person promotes.
pub const CONSOLIDATOR_NAME: &str = "Consolidator";
pub const SKILLS_REVISE: &str = "skills.revise";
pub const CONSOLIDATOR_TOOLS: [&str; 6] = [
    "agents.list",
    AGENTS_READ,
    RUNS_REVIEW,
    RUNS_READ,
    SKILLS_READ,
    SKILLS_REVISE,
];

/// The Consolidator's charter, pinned like the Coach's.
pub const CONSOLIDATOR_CHARTER: &str = "You are the Consolidator: you make a skill better from what the agents that \
follow it did. Given a skill's name:\n\
1. skills.read — its procedure and its followers (the agents whose intent names it).\n\
2. runs.review — each follower's recent runs with the verifier's verdicts. Read the verified ones \
first: what was asked, which calls they made and in what order, what the answer said. Read the \
failed ones too: what the procedure let them skip or misread.\n\
3. Name, in two sentences, the trajectory the verified runs share that the procedure does not state \
— a step every verified run took, an order they kept, a check they made — or the sentence the \
failed runs misread. One pattern, not a list.\n\
4. File it with skills.revise, the pattern as `why`. Give the change, not the whole text: \
`add_step` for the step the verified runs took and the procedure leaves out, written as the \
agent should run it (the tool, its arguments' shape, what to do with the result); `replace` for a \
sentence the failed runs misread, reworded with the case that failed. Keep the skill's purpose, \
its tools and every step that passed. Keep it as short as the original.\n\
The revision enters the gate: it is held while every follower's suite runs against it, and a person \
promotes it in Catalog → Skills; you never promote. A refusal from skills.revise is an answer, not a \
retry: never send the same arguments again — change the change, or stop and report what you found. \
Answer with the pattern, the step or sentence you filed (quoted), the revision number, and whether it \
is held. No verified runs, or nothing the procedure leaves out: say so and file nothing. Failures that \
come from a tool, a system or the model rather than the procedure: say that and file nothing. Never \
file a procedure identical to the current one.";

/// The one platform door every agent may hold: read the skill catalog, or
/// one skill's full procedure, on demand — progressive disclosure, so an
/// agent's context carries names and descriptions and loads a body when it
/// activates the skill.
pub const SKILLS_READ: &str = "skills.read";

/// The tool names that are the platform's own — the Composer's doors. An
/// agent the Composer builds does not get them unless asked.
pub const ASSIGNMENT_PROGRESS: &str = "assignment.progress";

/// Web search: the engine's result page read as text, so a research agent
/// finds a page instead of guessing its URL.
pub const WEB_SEARCH: &str = "web.search";
/// A suggested edit to a knowledge source: the agent proposes, a person accepts.
pub const KNOWLEDGE_PROPOSE_EDIT: &str = "knowledge.propose_edit";
/// Two sources disagree: the agent flags it; a person rules which stands.
pub const KNOWLEDGE_FLAG_CONFLICT: &str = "knowledge.flag_conflict";
pub const PLATFORM_TOOLS: [&str; 22] = [
    WEB_SEARCH,
    KNOWLEDGE_PROPOSE_EDIT,
    KNOWLEDGE_FLAG_CONFLICT,
    MEMORY_BLOCK_EDIT,
    ASSIGNMENT_PROGRESS,
    "catalog.tools",
    "catalog.skills",
    "catalog.connectors",
    "catalog.mcp",
    "agents.list",
    "agents.create",
    "skills.register",
    "connectors.register",
    CONNECTORS_PROBE,
    CONNECTORS_EXTEND,
    GAPS_FILE,
    GAPS_WORK_ORDER,
    GAPS_RESOLVE,
    TASKS_ENQUEUE,
    ASSIGNMENT_CREATE,
    PLAN,
    KNOWLEDGE_READ,
];
/// The Composer's two doors into a connected system: look into it through
/// one of its read-only operations, and add operations to it.
pub const CONNECTORS_PROBE: &str = "connectors.probe";
pub const CONNECTORS_EXTEND: &str = "connectors.extend";

/// The Composer's charter. Pinned here so a wording change is a diff; the
/// seeded agent is an ordinary assistant a person can edit in the studio,
/// and the platform re-versions it when this text changes — unless a
/// person has taken it over.
pub const COMPOSER_CHARTER: &str = "You are the Composer: the agent that designs agents on this \
platform. People describe what they want; you read what the platform actually has, write the \
specification, and the person approves it — creation is their click, or their word.

What you must do:
- Read the platform before you decide anything: call catalog.tools, catalog.skills, \
catalog.connectors and catalog.mcp first, each once. Never assume a tool, connector or skill \
exists.
- A tool that takes a table, object or query parameter (servicenow.list-records, \
servicenow.get-record, salesforce.soql-query, and any operation whose parameters say table or \
object) reads whatever it is told; an instruction like \"use list-records to retrieve search \
history\" names no table and makes an agent that guesses. Before you write the block: call \
connectors.probe to find the tables the request needs — for ServiceNow, servicenow.list-records \
with table sys_db_object, sysparm_query nameLIKE<word>^ORlabelLIKE<word> (search, query, \
interaction, incident…), sysparm_fields name,label, sysparm_limit 10 — a label says what a \
table holds — then probe each table you will name with sysparm_limit 2 to see its fields and \
that it has rows; an empty one holds nothing to analyze, so choose another. Write, for each thing the request asks \
for, the exact `table <name>` and the query in the instructions. agents.create answers \
charter_warnings when a table-taking tool is given and no table is named; repeat them.
- Answer with the specification, always, as ONE fenced code block whose info string is exactly \
`agent`, in this line format and nothing else inside it:
```agent
name: <short, specific>
purpose: <one line: what it is for and for whom>
role: <one or two sentences: what it is, in the second person — \"You are …\">
status: proposed | created | blocked
assistant_id: <only when created>
instructions:
- <what it must do, one step per line, numbered by order>
tools:
- <exact tool name from catalog.tools> — <when to use it>
skills:
- <skill name from catalog.skills or skills.register>
constraints:
- <what it must never do>
output: <how it answers: tone, length, format>
done_when:
- <what done looks like, observable>
needs:
- <what a person must add first, and where: Catalog → Connectors / Catalog → MCP / a skill — \
or `none` when every tool above is in catalog.tools>
schedule: <when it runs on its own — `every 30 minutes`, `every 2 hours`, `every day`, `every \
morning`, or a 5-field cron expression in UTC — or `none` when a person starts every run>
standing_message: <what it is told each time it runs on its own; omit when schedule is none>
budget: <the most one run may spend — `60000 tokens`, `$0.10`, or both as `60000 tokens, \
$0.10` — or `none`. A desk that reads a system and remembers spends 25–40k tokens on one \
question; 20000 stops it before it answers. Propose a tighter one only when the request \
names a spend or cost limit>
```
- After the block, three headed lines: WHAT I BUILT (the agent's name and the id agents.create \
returned in this conversation, and any tools connectors.extend added — otherwise \"nothing yet — \
review the specification and create it, or tell me to\"), WHAT IT CAN DO NOW (from what you \
probed, not what you assume), WHAT IT STILL NEEDS (what a person must add, and what you \
recommend building for it).
- Prefer what exists. If a capability exists only as an unconnected connector, name it under \
needs and say a person connects it in Catalog → Connectors. If it exists nowhere and you know \
the API well, describe it with connectors.register; otherwise name the MCP server or connector \
a person would add, and why.
- A request the tools do not cover yet is a problem to solve, not a stop. When the request \
names data or actions no tool covers and a connected system holds them, run the loop: (1) know \
what the system has to offer — catalog.connectors with connector: <id> gives its API root and \
every operation's method, path and parameters; (2) look into it — connectors.probe calls one of \
its read-only operations and shows what came back: for ServiceNow, servicenow.list-records with \
table sys_db_object and sysparm_query nameLIKE<word> finds the tables whose name contains the \
word, and the same on a table with sysparm_limit 3 shows its fields and proves it has rows; \
(3) build what is missing — when a generic operation already reads it, write the table and the \
query into the instructions; when none does, connectors.extend adds operations to that \
connector following the shapes you read, and the tools exist from that call; (4) probe one new \
tool once to prove the path answers; (5) remember the shape with memory.remember — the system, \
the tables, the operations — so the next request on that system starts from it. Then say what \
you found, what you built, and what you recommend a person adds where you could not. `needs: \
none` is true only after that loop.
- When the person asks whether the agent needs another tool, table or data source, that is a \
new turn: read catalog.tools and catalog.connectors again, probe when it helps, and answer from \
the results. Never say a capability is available, covered or not needed without a call in that \
turn that shows it.
- A revised block differs from the last one, and you say in one line what changed. When \
nothing should change, say the specification is unchanged and why; never call an unchanged \
block a revision.
- The library ships skills for the systems it knows: catalog.skills lists them with the tools \
they assume, and catalog.connectors with connector: <id> lists the ones for that system. When \
the agent's tools match a skill's tools, list that skill under skills — a skill is the procedure \
the connector's builders wrote, and the agent reads it before it acts. When a procedure is worth \
reusing and no skill has it, register it with skills.register and list it under skills.
- When the request says the agent works on its own — each morning, hourly, nightly, on a \
schedule, without being asked — propose the schedule and the standing message; an agent's loop \
is part of the agent. Creation attaches it. Otherwise schedule is none.
- Create with agents.create ONLY when the person tells you to (\"create it\", \"go ahead\", \
\"build it\"). Then use, as the charter, the block's role, instructions, tools with their \
notes, skills, constraints, output and done_when, in that order under those headings, pass \
the block's schedule, standing_message and budget (as max_tokens_per_run / \
max_cost_usd_per_run) when it has them, and re-emit the block with status: created and the \
assistant_id.
- Update the block when the person asks for changes; it is the one artifact.

What you must never do:
- Invent a tool name, a connector or a skill. Every name comes from catalog.tools, \
catalog.skills, or something you registered in this conversation. A connector with \
connected: false has no tools yet.
- Say you built something you did not. Only a result from agents.create is an agent.
- When agents.create answers skill_warnings or charter_warnings, repeat them to the person \
under WHAT I BUILT: a skill that assumes tools the agent lacks makes it stop where the tool \
should be; a table-taking tool with no table named makes it guess.\n\
- A front door that routes to other agents gets agents.ask and a charter that names each agent \
and what it is for; the other agents keep their own tools. agents.ask is not a platform tool.\n\
- Carry every fact the request states into the instructions word for word — names, numbers, \
ids, coordinates, lists and what each item is for. A charter without them makes an agent that \
says it does not know.\n\
- Say an agent needs what it already has. A tool listed by catalog.tools is connected and \
usable now; a need exists only for a connector with connected: false, or a skill or server \
that is absent.
- Give an agent write tools (anything not read-only) the request does not need; when you do, \
say so under constraints. An irreversible tool (effect `irreversible` in catalog.tools) is safe \
to give: the run pauses before it and a person approves or denies in the Inbox — say that \
under constraints too, so the person knows the agent will ask.
- Ask more than one question before proposing; assume reasonably and say so.
- Rebuild what this conversation already built; refer to it by name and id.
- Call the same catalog tool twice in a turn (a question from the person starts a new turn). \
If the catalog is no longer in view after compaction, read it again once.

How you answer: two or three plain sentences, then the block, then the three lines.

Done when: the specification is complete and every tool it names exists, or the person holds \
an exact list of what to add first.";

/// The live cell the react registry reads platform tools from.
#[derive(Default)]
pub struct PlatformTools {
    tools: RwLock<Vec<Arc<dyn Tool>>>,
}

impl std::fmt::Debug for PlatformTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.tools.read().map(|t| t.len()).unwrap_or(0);
        f.debug_struct("PlatformTools")
            .field("tools", &count)
            .finish()
    }
}

impl PlatformTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Fill the cell with the doors, bound to `state`.
    pub(crate) fn fill(&self, state: &Arc<AppState>) {
        let doors = Doors {
            state: Arc::downgrade(state),
            graph: state.config.agent_graph.clone(),
        };
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(CatalogTools(doors.clone())),
            Arc::new(CatalogSkills(doors.clone())),
            Arc::new(CatalogConnectors(doors.clone())),
            Arc::new(CatalogMcp(doors.clone())),
            Arc::new(AssignmentProgress(doors.clone())),
            Arc::new(AgentsList(doors.clone())),
            Arc::new(AgentsCreate(doors.clone())),
            Arc::new(SkillsRegister(doors.clone())),
            Arc::new(SkillsRevise(doors.clone())),
            Arc::new(ConnectorsRegister(doors.clone())),
            Arc::new(ConnectorsProbe(doors.clone())),
            Arc::new(ConnectorsExtend(doors.clone())),
            Arc::new(MemoryRemember(doors.clone())),
            Arc::new(MemoryRecall(doors.clone())),
            Arc::new(AgentsRead(doors.clone())),
            Arc::new(RunsReview(doors.clone())),
            Arc::new(RunsRead(doors.clone())),
            Arc::new(AgentsAsk(doors.clone())),
            Arc::new(AgentsRevise(doors.clone())),
            Arc::new(SkillsRead(doors.clone())),
            Arc::new(ArtifactsWrite(doors.clone())),
            Arc::new(ArtifactsRead(doors.clone())),
            Arc::new(GapsFile(doors.clone())),
            Arc::new(GapsWorkOrder(doors.clone())),
            Arc::new(GapsResolve(doors.clone())),
            Arc::new(TasksEnqueue(doors.clone())),
            Arc::new(AssignmentCreate(doors.clone())),
            Arc::new(PlanTool),
            Arc::new(MemoryBlockEdit(doors.clone())),
            Arc::new(KnowledgeRead(doors.clone())),
            Arc::new(WebSearch(doors.clone())),
            Arc::new(KnowledgeProposeEdit(doors.clone())),
            Arc::new(KnowledgeFlagConflict(doors.clone())),
            Arc::new(WebFetch(doors)),
        ];
        if let Ok(mut cell) = self.tools.write() {
            *cell = tools;
        }
    }
}

impl ToolSource for PlatformTools {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().map(|t| t.clone()).unwrap_or_default()
    }
}

/// A weak handle to the state: a tool never keeps the server alive.
/// The tenant the calling run acts in: its execution block's, or the
/// default for a tool called outside a run (a test, a direct call).
pub(crate) fn tenant_now() -> String {
    rusty_agent_runtime::tool::current_run()
        .and_then(|run| run.tenant().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_TENANT.to_owned())
}

/// The calling run's tenant context: its tenant and, as the principal,
/// the actor who admitted the run — so what a platform door creates is
/// attributed to them and scoped to their tenant.
/// The stand-in world this run rehearses in, when it does. A rehearsal's
/// question is answered in the stand-in and never by the live system; a
/// tool that would leave something on the estate — an agent, a skill, a
/// connector, a gap — answers the same way, and leaves nothing.
pub(crate) fn rehearsal_world() -> Option<String> {
    let run = rusty_agent_runtime::tool::current_run()?;
    run.execution
        .as_ref()?
        .get("world")?
        .as_str()
        .map(str::to_owned)
}

fn rehearsal_note(what: &str, world: &str) -> Value {
    json!({
        "created": false, "filed": false, "registered": false,
        "rehearsal": world,
        "note": format!("this run rehearses in the stand-in world `{world}`, so no {what} was made; say what you would have made, and a person can make it for real"),
    })
}

pub(crate) fn context_now() -> TenantContext {
    let run = rusty_agent_runtime::tool::current_run();
    let tenant = run
        .as_ref()
        .and_then(|r| r.tenant().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_TENANT.to_owned());
    let context = TenantContext::new(tenant, Vec::new());
    let Some(actor) = run.as_ref().and_then(|r| r.actor().cloned()) else {
        return context;
    };
    let id = actor
        .get("principal_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if id.is_empty() {
        return context;
    }
    let name = actor
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let kind = match actor.get("kind").and_then(Value::as_str) {
        Some("service") => crate::auth::PrincipalKind::Service,
        _ => crate::auth::PrincipalKind::User,
    };
    context.with_principal(crate::auth::Principal {
        id,
        name,
        kind,
        roles: Vec::new(),
    })
}

#[derive(Clone)]
struct Doors {
    state: Weak<AppState>,
    graph: String,
}

impl Doors {
    fn open(&self) -> Result<Arc<AppState>> {
        self.state
            .upgrade()
            .ok_or_else(|| RustyError::Tool("the server is shutting down".to_owned()))
    }
}

fn tool_err(message: impl Into<String>) -> RustyError {
    RustyError::Tool(message.into())
}

fn is_platform_tool(name: &str) -> bool {
    PLATFORM_TOOLS.contains(&name)
}

/// Tool lines as the Composer writes them — `name` or `name — when to use
/// it` — split into the names (checked and allow-listed) and the notes by
/// name (kept on the agent, shown to its model on the tool).
fn split_tool_lines(lines: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut names = Vec::new();
    let mut notes = BTreeMap::new();
    for line in lines {
        let (name, note) = match line.split_once(" — ").or_else(|| line.split_once(" - ")) {
            Some((name, note)) => (name.trim().to_owned(), note.trim().to_owned()),
            None => (line.trim().to_owned(), String::new()),
        };
        if name.is_empty() {
            continue;
        }
        if !note.is_empty() {
            notes.insert(name.clone(), note);
        }
        names.push(name);
    }
    (names, notes)
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A cadence as people say it — `every 30 minutes`, `every 2 hours`, `every
/// day`, `every morning` (09:00 UTC), `hourly`, `daily`, or a 5-field cron
/// expression — as the scheduler takes it: `(interval_secs, cron_expr)`.
fn cadence_of(words: &str) -> Option<(Option<u64>, Option<String>)> {
    let t = words.trim().to_ascii_lowercase();
    let t = t.strip_prefix("every ").unwrap_or(&t).trim().to_owned();
    if t.is_empty() || t == "none" || t == "never" {
        return None;
    }
    let fields: Vec<&str> = t.split_whitespace().collect();
    if fields.len() == 5 && t.chars().all(|c| c.is_ascii_digit() || "*/,- ".contains(c)) {
        return crate::crons::validate_schedule(None, Some(&t))
            .ok()
            .map(|_| (None, Some(t)));
    }
    let interval = match t.as_str() {
        "morning" | "day at 9" | "day at 09:00" => {
            return Some((None, Some("0 9 * * *".to_owned())));
        }
        "weekday morning" | "weekday" => return Some((None, Some("0 9 * * 1-5".to_owned()))),
        "minute" => 60,
        "hourly" | "hour" => 3_600,
        "daily" | "day" | "night" | "nightly" => 86_400,
        "weekly" | "week" => 7 * 86_400,
        other => {
            let (n, unit) = other.trim_end_matches('s').split_once(' ')?;
            let n: u64 = n.trim().parse().ok().filter(|n| *n >= 1)?;
            let unit = unit.trim();
            let size = if unit.starts_with("min") {
                60
            } else if unit.starts_with('h') {
                3_600
            } else if unit == "day" {
                86_400
            } else if unit == "week" {
                7 * 86_400
            } else {
                return None;
            };
            n * size
        }
    };
    crate::crons::validate_schedule(Some(interval), None)
        .ok()
        .map(|_| (Some(interval), None))
}

/// The cadence back in words, for the Composer's answer.
fn cadence_words(interval_secs: Option<u64>, cron_expr: Option<&str>) -> String {
    if let Some(expr) = cron_expr {
        return format!("at {expr} (UTC)");
    }
    let secs = interval_secs.unwrap_or(0);
    for (word, size) in [
        ("week", 7 * 86_400),
        ("day", 86_400),
        ("hour", 3_600),
        ("minute", 60),
    ] {
        if secs.is_multiple_of(size) {
            let n = secs / size;
            return if n == 1 {
                format!("every {word}")
            } else {
                format!("every {n} {word}s")
            };
        }
    }
    format!("every {secs} seconds")
}

fn text(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// The first sentence of a description, bounded — a catalog answer the
/// model can hold whole.
fn brief(text: &str, max: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = one_line
        .find(". ")
        .map(|i| &one_line[..=i])
        .unwrap_or(&one_line)
        .trim()
        .to_owned();
    if first.len() <= max {
        return first;
    }
    let mut cut = max;
    while !first.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", first[..cut].trim_end())
}

// ---- catalog.tools --------------------------------------------------------

struct CatalogTools(Doors);

#[async_trait]
impl Tool for CatalogTools {
    fn name(&self) -> &str {
        "catalog.tools"
    }
    fn description(&self) -> &str {
        "Every tool an agent on this platform can be given right now: exact name, effect (read_only is safe; anything else changes the world) and a one-line description. Tools named `connector.operation` come from a configured connection; `server.tool` from a mounted MCP server; plain names are built in. Call this once before naming any tool."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tools: Vec<Value> = state
            .registry
            .tool_capabilities(&self.0.graph)
            .into_iter()
            .filter(|c| !is_platform_tool(&c.name))
            .map(|c| {
                let origin = if !c.name.contains('.') { "built in" } else { "a connection or MCP server" };
                json!({"name": c.name, "effect": c.effect, "origin": origin, "description": brief(&c.description, 110)})
            })
            .collect();
        Ok(json!({ "graph": self.0.graph, "count": tools.len(), "tools": tools }))
    }
}

// ---- catalog.skills -------------------------------------------------------

struct CatalogSkills(Doors);

#[async_trait]
impl Tool for CatalogSkills {
    fn name(&self) -> &str {
        "catalog.skills"
    }
    fn description(&self) -> &str {
        "Every skill on this platform: a reusable procedure an agent can be told to follow — name, when to use it, the tools it assumes. Give an agent a skill by name in agents.create."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let skills: Vec<Value> = state
            .skills
            .list(&tenant_now())
            .await
            .into_iter()
            .map(|s| json!({"name": s.name, "description": brief(&s.description, 120), "tools": s.allowed_tools}))
            .collect();
        Ok(json!({ "count": skills.len(), "skills": skills }))
    }
}

// ---- catalog.connectors ---------------------------------------------------

/// Order connector versions: numeric when they are numbers ("2" < "10"),
/// otherwise by text.
fn version_rank(version: &str) -> (u64, String) {
    (
        version.trim().parse::<u64>().unwrap_or(0),
        version.to_owned(),
    )
}

struct CatalogConnectors(Doors);

#[async_trait]
impl Tool for CatalogConnectors {
    fn name(&self) -> &str {
        "catalog.connectors"
    }
    fn description(&self) -> &str {
        "The connector library (systems the platform knows how to reach, each with its operations) and which of them are connected. Only a connector with connected: true has tools (named `connector.operation`, listed by catalog.tools). connected: false means a person must configure it in Catalog → Connectors first; until then none of its operations can be given to an agent. With `connector: <id>`, one system in full: its API root and every operation's method, path and parameters — what to read before connectors.probe or connectors.extend."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"connector": {"type": "string", "description": "A connector id from the library, for its operations in full."}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let manifests = state
            .connectors
            .list_manifests(&tenant_now())
            .await
            .map_err(tool_err)?;
        let instances = state
            .connectors
            .list_instances(&tenant_now())
            .await
            .map_err(tool_err)?;
        if let Some(wanted) = args
            .get("connector")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let Some(m) = newest_manifest(&manifests, wanted) else {
                return Ok(
                    json!({"found": false, "connector": wanted, "note": "no connector in the library has that id"}),
                );
            };
            let connected = connected_ids(&state, &instances).await.contains(&m.id);
            // The skills the library ships for this system: procedures that
            // name its tools, so a new connection connects the dots from day one.
            let prefix = format!("{}.", m.id);
            let skills: Vec<Value> = state
                .skills
                .list(&tenant_now())
                .await
                .into_iter()
                .filter(|s| s.allowed_tools.iter().any(|t| t.starts_with(&prefix)))
                .map(|s| json!({"name": s.name, "description": brief(&s.description, 140), "tools": s.allowed_tools}))
                .collect();
            return Ok(json!({
                "found": true,
                "id": m.id,
                "skills": skills,
                "name": m.display_name,
                "version": m.version,
                "connected": connected,
                "description": m.description,
                "base_url": m.base_url,
                "configuration": m.connection_specification.get("properties").and_then(Value::as_object).map(|p| p.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                "operations": m.operations.iter().filter(|op| op.name != m.check).map(|op| json!({
                    "name": format!("{}.{}", m.id, op.name),
                    "method": op.method,
                    "path": op.path,
                    "effect": op.effect,
                    "description": op.description,
                    "parameters": op.params_schema.get("properties").and_then(Value::as_object).map(|p| p.iter().map(|(k, v)| json!({"name": k, "type": v.get("type"), "description": v.get("description")})).collect::<Vec<_>>()).unwrap_or_default(),
                    "required": op.params_schema.get("required"),
                })).collect::<Vec<_>>(),
                "extend": "connectors.extend adds operations to this connector following these shapes; connectors.probe calls a read-only one to see what the system holds",
            }));
        }
        // A connection is on one version of a connector; the connector is
        // connected whatever version that is. Resolving by id — through the
        // manifest each instance was sealed against, which the store keeps
        // even when the library has moved on — is what stops a newer version
        // in the library from reading as "not connected" beside a live
        // connection.
        let mut connected_ids: Vec<String> = Vec::new();
        for instance in &instances {
            if let Ok(Some(m)) = state
                .connectors
                .get_manifest(&tenant_now(), &instance.manifest_hash)
                .await
            {
                if !connected_ids.contains(&m.id) {
                    connected_ids.push(m.id);
                }
            }
        }
        // One entry per connector, its newest version: the library is what
        // can be reached, not every revision of how.
        let mut newest: Vec<&ConnectorManifest> = Vec::new();
        for m in &manifests {
            match newest.iter().position(|n| n.id == m.id) {
                Some(at) if version_rank(&m.version) > version_rank(&newest[at].version) => {
                    newest[at] = m
                }
                Some(_) => {}
                None => newest.push(m),
            }
        }
        let library: Vec<Value> = newest
            .iter()
            .map(|m| {
                let connected = connected_ids.contains(&m.id);
                let operations: Vec<String> = m
                    .operations
                    .iter()
                    .filter(|op| op.name != m.check)
                    .map(|op| {
                        let effect = serde_json::to_value(op.effect)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default();
                        format!("{}.{} ({effect})", m.id, op.name)
                    })
                    .collect();
                json!({"id": m.id, "name": m.display_name, "connected": connected, "description": brief(&m.description, 100), "operations": operations})
            })
            .collect();
        Ok(json!({ "library": library, "connections": instances.len() }))
    }
}

/// The newest version of a connector in the library, by id.
fn newest_manifest<'a>(
    manifests: &'a [ConnectorManifest],
    id: &str,
) -> Option<&'a ConnectorManifest> {
    manifests
        .iter()
        .filter(|m| m.id == id)
        .max_by_key(|m| version_rank(&m.version))
}

/// The ids of the connectors with a live connection, resolved through the
/// manifest each connection was sealed against.
async fn connected_ids(
    state: &AppState,
    instances: &[rusty_agent_runtime::connector::ConnectorInstance],
) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for instance in instances {
        if let Ok(Some(m)) = state
            .connectors
            .get_manifest(&tenant_now(), &instance.manifest_hash)
            .await
        {
            if !ids.contains(&m.id) {
                ids.push(m.id);
            }
        }
    }
    ids
}

// ---- connectors.probe -----------------------------------------------------

/// The most of a probe's answer the model sees; a system's table listing
/// is read for its shape, not copied whole.
const PROBE_CHARS: usize = 6000;

struct ConnectorsProbe(Doors);

#[async_trait]
impl Tool for ConnectorsProbe {
    fn name(&self) -> &str {
        CONNECTORS_PROBE
    }
    fn description(&self) -> &str {
        "Look into a connected system before deciding what to build: call one of its read-only operations — a tool named `connector.operation` from catalog.tools — with arguments, and read what came back. This is how you learn what a system holds. For ServiceNow: servicenow.list-records with table sys_db_object, sysparm_query nameLIKE<word>^ORlabelLIKE<word>, sysparm_fields name,label lists the tables whose name or label contains the word (a table's label says what it holds: `Text Search Query`, `Interaction`); the same on the table itself with sysparm_limit 3 shows its fields and proves it has rows — an empty result means the table holds nothing to analyze, so look for another. Never a write: an operation that is not read-only is refused."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["tool"], "properties": {
            "tool": {"type": "string", "description": "The tool's exact name, e.g. servicenow.list-records."},
            "arguments": {"type": "object", "description": "The call's arguments, as the tool's parameters name them."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let name = args
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let Some(cell) = &state.connection_tools else {
            return Ok(json!({"probed": false, "note": "this server mounts no connection tools"}));
        };
        use rusty_agent_runtime::tool::ToolSource as _;
        let tools = cell.tools();
        let Some(tool) = tools.iter().find(|t| t.name() == name) else {
            let connector = name.split('.').next().unwrap_or("").to_owned();
            let same: Vec<String> = tools
                .iter()
                .map(|t| t.name().to_owned())
                .filter(|n| n.starts_with(&format!("{connector}.")))
                .collect();
            return Ok(json!({
                "probed": false,
                "tool": name,
                "note": if same.is_empty() { "no connected system has a tool by that name — catalog.tools lists them; a connector with connected: false has none".to_owned() } else { format!("no tool by that name; {connector} has: {}", same.join(", ")) },
            }));
        };
        if !matches!(tool.effect(), Effect::ReadOnly) {
            return Ok(
                json!({"probed": false, "tool": name, "note": format!("a probe reads only; {name} is {:?}. Look with a read-only operation instead.", tool.effect())}),
            );
        }
        let result = match tool.call(arguments).await {
            Ok(value) => value,
            Err(error) => {
                return Ok(
                    json!({"probed": false, "tool": name, "note": format!("the system answered with an error: {error}")}),
                );
            }
        };
        let text = result.to_string();
        if text.chars().count() > PROBE_CHARS {
            let head: String = text.chars().take(PROBE_CHARS).collect();
            return Ok(
                json!({"probed": true, "tool": name, "truncated": true, "bytes": text.len(), "result_head": head, "note": "the answer was longer than shown; narrow the query or lower the limit to see the rest"}),
            );
        }
        Ok(json!({"probed": true, "tool": name, "result": result}))
    }
}

// ---- connectors.extend ----------------------------------------------------

#[derive(serde::Deserialize)]
struct ExtendParam {
    name: String,
    #[serde(default = "string_type")]
    r#type: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: String,
}

fn string_type() -> String {
    "string".to_owned()
}

#[derive(serde::Deserialize)]
struct ExtendOp {
    name: String,
    description: String,
    method: String,
    path: String,
    effect: String,
    #[serde(default)]
    params: Vec<ExtendParam>,
}

/// The connector's next version: everything it has plus these operations,
/// each authenticated the way its existing operations are. Pure: the
/// caller registers it and moves the connections.
fn extended_manifest(
    newest: &ConnectorManifest,
    versions: &[String],
    ops: &[ExtendOp],
) -> std::result::Result<ConnectorManifest, String> {
    if ops.is_empty() {
        return Err("name at least one operation to add".to_owned());
    }
    let next = versions
        .iter()
        .map(|v| version_rank(v).0)
        .max()
        .unwrap_or(0)
        + 1;
    let template = newest
        .operations
        .iter()
        .find(|op| op.name != newest.check)
        .or_else(|| newest.operations.first())
        .ok_or_else(|| "the connector has no operation to follow".to_owned())?;
    let auth = serde_json::to_value(&template.auth).map_err(|e| e.to_string())?;
    let headers = serde_json::to_value(&template.headers).map_err(|e| e.to_string())?;
    let mut value = serde_json::to_value(newest).map_err(|e| e.to_string())?;
    value["version"] = json!(next.to_string());
    value["hash"] = json!("");
    let existing: Vec<String> = newest.operations.iter().map(|op| op.name.clone()).collect();
    let mut added: Vec<String> = Vec::new();
    for op in ops {
        let name = crate::connector_draft::kebab(&op.name);
        if name.is_empty() {
            return Err("an operation needs a name".to_owned());
        }
        if existing.contains(&name) || added.contains(&name) {
            return Err(format!(
                "`{}.{name}` exists already; a new operation needs a new name",
                newest.id
            ));
        }
        if !op.path.trim().starts_with('/') {
            return Err(format!(
                "`{name}`: the path must start with `/` and follow the connector's base URL, like its other operations"
            ));
        }
        let mut properties = serde_json::Map::new();
        for p in &op.params {
            let mut schema = serde_json::Map::new();
            schema.insert("type".to_owned(), json!(p.r#type));
            if !p.description.trim().is_empty() {
                schema.insert("description".to_owned(), json!(p.description.trim()));
            }
            properties.insert(p.name.clone(), Value::Object(schema));
        }
        let required: Vec<&str> = op
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name.as_str())
            .collect();
        let mut params_schema = json!({"type": "object", "properties": properties});
        if !required.is_empty() {
            params_schema["required"] = json!(required);
        }
        value["operations"]
            .as_array_mut()
            .expect("operations is an array")
            .push(json!({
                "name": name,
                "description": op.description.trim(),
                "method": op.method.trim().to_ascii_uppercase(),
                "path": op.path.trim(),
                "effect": op.effect.trim().to_ascii_lowercase(),
                "params_schema": params_schema,
                "headers": headers,
                "auth": auth,
            }));
        added.push(name);
    }
    let manifest: ConnectorManifest = serde_json::from_value(value)
        .map_err(|e| format!("the extended connector does not parse: {e}"))?;
    manifest.validate().map_err(|e| e.to_string())?;
    let findings = rusty_agent_runtime::connector::lint_manifest(&manifest);
    if !findings.is_empty() {
        let words: Vec<String> = findings.iter().map(ToString::to_string).collect();
        return Err(format!(
            "the extended connector is not ready: {}",
            words.join("; ")
        ));
    }
    manifest.sealed().map_err(|e| e.to_string())
}

struct ConnectorsExtend(Doors);

#[async_trait]
impl Tool for ConnectorsExtend {
    fn name(&self) -> &str {
        CONNECTORS_EXTEND
    }
    fn description(&self) -> &str {
        "Build the tools a connected system lacks: add operations to its connector as a new version — everything it has plus these — and every connection to it moves to that version, so the new tools (`connector.operation`) exist from this call for any agent. Read catalog.connectors with connector: <id> first and follow the shapes it shows: the same base URL, paths like its other operations, parameters as the API names them. Effects: read_only for reads; idempotent, compensatable or irreversible for writes. Refused when the connector is not connected, when a name exists already, or when the system does not answer the check afterwards."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["connector", "operations"], "properties": {
            "connector": {"type": "string", "description": "The connector's id, e.g. servicenow."},
            "operations": {"type": "array", "items": {"type": "object", "required": ["name", "description", "method", "path", "effect"], "properties": {
                "name": {"type": "string", "description": "kebab-case; becomes the tool `connector.name`"},
                "description": {"type": "string", "description": "what it does and when to use it — the model reads this"},
                "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"]},
                "path": {"type": "string", "description": "/path/{param}, after the connector's base URL; query parameters may be written in, e.g. ?sysparm_limit={limit}"},
                "effect": {"type": "string", "enum": ["read_only", "idempotent", "compensatable", "irreversible"]},
                "params": {"type": "array", "items": {"type": "object", "required": ["name"], "properties": {
                    "name": {"type": "string"}, "type": {"type": "string", "enum": ["string", "integer", "number", "boolean"]},
                    "required": {"type": "boolean"}, "description": {"type": "string"}}}}
            }}}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let id = args
            .get("connector")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let ops: Vec<ExtendOp> =
            serde_json::from_value(args.get("operations").cloned().unwrap_or_else(|| json!([])))
                .map_err(|e| tool_err(format!("the operations do not parse: {e}")))?;
        let manifests = state
            .connectors
            .list_manifests(&tenant_now())
            .await
            .map_err(tool_err)?;
        let Some(newest) = newest_manifest(&manifests, &id) else {
            return Ok(
                json!({"extended": false, "connector": id, "note": "no connector in the library has that id — catalog.connectors names them; a system the library lacks is described with connectors.register"}),
            );
        };
        let instances = state
            .connectors
            .list_instances(&tenant_now())
            .await
            .map_err(tool_err)?;
        let mut mine = Vec::new();
        for instance in &instances {
            if let Ok(Some(m)) = state
                .connectors
                .get_manifest(&tenant_now(), &instance.manifest_hash)
                .await
            {
                if m.id == newest.id {
                    mine.push(instance.clone());
                }
            }
        }
        if mine.is_empty() {
            return Ok(
                json!({"extended": false, "connector": id, "note": "that connector is not connected: a person connects it in Catalog → Connectors first; its operations become tools only then"}),
            );
        }
        let versions: Vec<String> = manifests
            .iter()
            .filter(|m| m.id == newest.id)
            .map(|m| m.version.clone())
            .collect();
        let manifest = match extended_manifest(newest, &versions, &ops) {
            Ok(m) => m,
            Err(note) => return Ok(json!({"extended": false, "connector": id, "note": note})),
        };
        state
            .connectors
            .put_manifest(&tenant_now(), &manifest)
            .await
            .map_err(tool_err)?;
        let mut moved = 0usize;
        let mut stayed: Vec<String> = Vec::new();
        for instance in &mine {
            match crate::connectors::follow_version(&state, &tenant, instance, &manifest).await {
                Ok(_) => moved += 1,
                Err(reason) => stayed.push(reason),
            }
        }
        crate::connectors::refresh_connection_tools(&state).await;
        let new_tools: Vec<String> = manifest
            .operations
            .iter()
            .filter(|op| !newest.operations.iter().any(|had| had.name == op.name))
            .map(|op| format!("{}.{}", manifest.id, op.name))
            .collect();
        let all_tools: Vec<String> = manifest
            .operations
            .iter()
            .filter(|op| op.name != manifest.check)
            .map(|op| format!("{}.{}", manifest.id, op.name))
            .collect();
        if moved == 0 {
            return Ok(json!({
                "extended": false,
                "connector": manifest.id,
                "version": manifest.version,
                "note": format!("version {} is in the library but no connection could follow it: {}. The new tools do not exist until one does.", manifest.version, stayed.join("; ")),
            }));
        }
        Ok(json!({
            "extended": true,
            "connector": manifest.id,
            "version": manifest.version,
            "new_tools": new_tools,
            "tools": all_tools,
            "connections_moved": moved,
            "connections_stayed": stayed,
            "note": "these tools exist now for any agent; name them under tools with when to use each, and probe one to prove the path is right",
        }))
    }
}

// ---- web.fetch ------------------------------------------------------------

/// The most of a page an agent reads at once.
const PAGE_CHARS: usize = 20_000;
const PAGE_BYTES: usize = 4 * 1024 * 1024;

/// HTML to the text a reader would see: scripts and styles dropped, tags
/// removed, entities the common few, whitespace collapsed.
/// The part of a page that is its content: the `<main>` element when
/// the page marks one, else its `<article>`, else the whole page. Skip
/// links, browser banners and site navigation sit outside it, and would
/// otherwise be chunked and cited as if they were the document.
pub(crate) fn content_region(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    for tag in ["main", "article"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let start = lower.match_indices(&open).map(|(at, _)| at).find(|&at| {
            matches!(
                lower.as_bytes().get(at + open.len()),
                Some(b'>' | b' ' | b'\t' | b'\n' | b'\r')
            )
        });
        if let (Some(start), Some(end)) = (start, lower.rfind(&close)) {
            if end > start {
                return &html[start..end + close.len()];
            }
        }
    }
    html
}

pub(crate) fn page_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < html.len() {
        if lower[i..].starts_with("<script")
            || lower[i..].starts_with("<style")
            || lower[i..].starts_with("<noscript")
        {
            let tag = if lower[i..].starts_with("<script") {
                "</script>"
            } else if lower[i..].starts_with("<style") {
                "</style>"
            } else {
                "</noscript>"
            };
            match lower[i..].find(tag) {
                Some(end) => {
                    i += end + tag.len();
                    continue;
                }
                None => break,
            }
        }
        if bytes[i] == b'<' {
            match html[i..].find('>') {
                Some(end) => {
                    let tag = &lower[i..i + end];
                    if tag.starts_with("<p")
                        || tag.starts_with("</p")
                        || tag.starts_with("<br")
                        || tag.starts_with("<li")
                        || tag.starts_with("<h")
                        || tag.starts_with("</div")
                        || tag.starts_with("<tr")
                        || tag.starts_with("</tr")
                    {
                        out.push('\n');
                    } else if tag.starts_with("<td") || tag.starts_with("<th") {
                        out.push_str(" | ");
                    }
                    i += end + 1;
                    continue;
                }
                None => break,
            }
        }
        let ch = html[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_lines = 0;
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_lines += 1;
            if blank_lines <= 1 {
                collapsed.push('\n');
            }
        } else {
            blank_lines = 0;
            collapsed.push_str(&line);
            collapsed.push('\n');
        }
    }
    collapsed.trim().to_owned()
}

/// An agent that finds a knowledge source wrong while working proposes
/// the corrected body; a person accepts or declines on the Knowledge page.
struct KnowledgeProposeEdit(Doors);

#[async_trait]
impl Tool for KnowledgeProposeEdit {
    fn name(&self) -> &str {
        KNOWLEDGE_PROPOSE_EDIT
    }
    fn description(&self) -> &str {
        "Propose a correction to a knowledge source you found wrong while working — the whole corrected body and why (what you found, and the tool result that shows it). A person reads it on the Knowledge page and accepts or declines; nothing changes until they do. One open suggestion per source from you at a time; say in your answer that you proposed it."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["source_id", "body", "why"], "properties": {
            "source_id": {"type": "string", "description": "The source_id a search_knowledge citation carries."},
            "body": {"type": "string", "description": "The corrected body, whole — what the source should say."},
            "why": {"type": "string", "description": "What is wrong and what shows it: the tool result, the record, the date."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let run = rusty_agent_runtime::tool::current_run()
            .ok_or_else(|| tool_err("knowledge.propose_edit only works inside a run"))?;
        let source_id = text(&args, "source_id");
        let body = text(&args, "body");
        let why = text(&args, "why");
        if source_id.is_empty() || body.trim().is_empty() || why.trim().is_empty() {
            return Ok(
                json!({"proposed": false, "note": "say the source_id, the whole corrected body, and why"}),
            );
        }
        if body.len() > 200_000 {
            return Ok(
                json!({"proposed": false, "note": "the body is too long for a suggestion (200,000 characters at most)"}),
            );
        }
        let base = crate::knowledge::knowledge_base(&state, &tenant);
        let versions = base
            .versions_of(&source_id)
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        let Some(latest) = versions.last() else {
            return Ok(
                json!({"proposed": false, "source_id": source_id, "note": "no knowledge source has that id; use the source_id exactly as the citation carries it"}),
            );
        };
        let agent = run.agent_id.clone().unwrap_or_else(|| "agent".to_owned());
        let open = crate::knowledge_edits::all(&state, tenant.tenant())
            .await
            .into_iter()
            .find(|e| {
                e.state == "waiting"
                    && e.source_id == source_id
                    && e.proposed_by.get("agent_id").and_then(Value::as_str) == Some(agent.as_str())
            });
        if let Some(open) = open {
            return Ok(
                json!({"proposed": false, "edit_id": open.edit_id, "source_id": source_id, "note": "you already proposed an edit to this source and a person has not decided yet; say so and do not propose again"}),
            );
        }
        let edit = crate::knowledge_edits::KnowledgeEdit {
            edit_id: uuid::Uuid::new_v4().to_string(),
            tenant: tenant.tenant().to_owned(),
            source_id: source_id.clone(),
            title: latest.title.clone(),
            body: body.trim().to_owned(),
            why: why.trim().chars().take(2_000).collect(),
            proposed_by: json!({"kind": "agent", "agent_id": agent, "run_id": run.run_id}),
            proposed_at: Utc::now(),
            state: "waiting".to_owned(),
            decided_by: None,
            decided_at: None,
            reason: None,
            version: None,
        };
        crate::knowledge_edits::keep(&state, &edit)
            .await
            .map_err(tool_err)?;
        Ok(
            json!({"proposed": true, "edit_id": edit.edit_id, "source_id": source_id, "title": latest.title, "note": "proposed — a person reads it on the Knowledge page under Suggested edits and accepts or declines; the source is unchanged until they do. Say in your answer that you proposed it."}),
        )
    }
}

/// An agent that finds two knowledge sources disagreeing flags it; until a
/// person rules, both sources' hits carry the conflict.
struct KnowledgeFlagConflict(Doors);

#[async_trait]
impl Tool for KnowledgeFlagConflict {
    fn name(&self) -> &str {
        KNOWLEDGE_FLAG_CONFLICT
    }
    fn description(&self) -> &str {
        "Flag two knowledge sources that disagree on a claim — when search_knowledge returns sources that say different things about the same fact. A person rules which stands on the Knowledge page; until then every search hit from either source is marked contested, and you state neither side as fact. Name both source_ids as the citations carry them, the claim in one line, and what each says."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["source_a", "source_b", "claim", "a_says", "b_says"], "properties": {
            "source_a": {"type": "string", "description": "The first source_id, as its citation carries it."},
            "source_b": {"type": "string", "description": "The second source_id."},
            "claim": {"type": "string", "description": "What they disagree about, in one line: \"the first step when the VPN keeps dropping\"."},
            "a_says": {"type": "string", "description": "What the first source says on it."},
            "b_says": {"type": "string", "description": "What the second source says on it."},
            "why": {"type": "string", "description": "How you found it: the question you were answering."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let run = rusty_agent_runtime::tool::current_run()
            .ok_or_else(|| tool_err("knowledge.flag_conflict only works inside a run"))?;
        let agent = run.agent_id.clone().unwrap_or_else(|| "agent".to_owned());
        let payload = crate::knowledge_conflicts::FilePayload {
            source_a: text(&args, "source_a"),
            source_b: text(&args, "source_b"),
            claim: text(&args, "claim"),
            a_says: text(&args, "a_says"),
            b_says: text(&args, "b_says"),
            why: text(&args, "why"),
        };
        match crate::knowledge_conflicts::file(
            &state,
            &tenant,
            payload,
            json!({"kind": "agent", "agent_id": agent, "run_id": run.run_id}),
        )
        .await
        {
            Ok((conflict, true)) => Ok(
                json!({"flagged": true, "conflict_id": conflict.conflict_id, "claim": conflict.claim, "between": [conflict.title_a, conflict.title_b], "note": "flagged — a person rules which source stands on the Knowledge page under Conflicts. Until then state neither side as fact: say the sources disagree, what each says, and that a person decides."}),
            ),
            Ok((conflict, false)) => Ok(
                json!({"flagged": false, "conflict_id": conflict.conflict_id, "claim": conflict.claim, "note": "this pair is already flagged and waits for a person's ruling; say so and state neither side as fact"}),
            ),
            Err(error) => Ok(json!({"flagged": false, "note": error.message()})),
        }
    }
}

/// The search engine's HTML result page — no key, no account; the
/// operator allows its host on the egress ceiling like any other.
const SEARCH_HOST: &str = "html.duckduckgo.com";
const SEARCH_RESULTS: usize = 10;

/// One search hit as the engine's page names it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The hits on the engine's HTML result page: each result anchor's title
/// and target (the redirect's `uddg` parameter decoded, or the plain href),
/// and the snippet that follows it before the next result. Bounded.
pub(crate) fn search_hits(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0;
    while hits.len() < limit {
        let Some(at) = html[from..].find("class=\"result__a\"") else {
            break;
        };
        let start = from + at;
        let Some(href_at) = html[start..].find("href=\"") else {
            break;
        };
        let href_start = start + href_at + 6;
        let Some(href_len) = html[href_start..].find('"') else {
            break;
        };
        let href = &html[href_start..href_start + href_len];
        let url = match href.find("uddg=") {
            Some(i) => {
                let rest = &href[i + 5..];
                let end = rest.find('&').unwrap_or(rest.len());
                percent_decode(&rest[..end])
            }
            None => href.replace("&amp;", "&"),
        };
        let Some(gt) = html[href_start + href_len..].find('>') else {
            break;
        };
        let title_start = href_start + href_len + gt + 1;
        let Some(title_len) = html[title_start..].find("</a>") else {
            break;
        };
        let title = page_text(&html[title_start..title_start + title_len])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let after = title_start + title_len;
        let next_result = html[after..]
            .find("class=\"result__a\"")
            .map(|i| after + i)
            .unwrap_or(html.len());
        let snippet = match html[after..next_result].find("result__snippet") {
            Some(i) => {
                let s = after + i;
                let s = html[s..].find('>').map(|j| s + j + 1).unwrap_or(s);
                let closes = [html[s..].find("</a>"), html[s..].find("</div>")]
                    .into_iter()
                    .flatten()
                    .min();
                let e = closes.map(|j| s + j).unwrap_or(next_result.min(s + 600));
                page_text(&html[s..e])
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            None => String::new(),
        };
        if url.starts_with("http") && !title.is_empty() {
            hits.push(SearchHit {
                title,
                url,
                snippet: snippet.chars().take(400).collect(),
            });
        }
        from = after;
    }
    hits
}

/// Search the web: the engine's result page read as text, so a research
/// agent finds a page to read with web.fetch instead of guessing its URL.
struct WebSearch(Doors);

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        WEB_SEARCH
    }
    fn description(&self) -> &str {
        "Search the web for pages on a topic: titles, URLs and snippets from the search engine's result page (DuckDuckGo, no account). Use it to find the page to read, then read it with web.fetch. The engine's host must be allowed on the platform's egress ceiling, as any host must; a refusal says so — then say you could not search, and do not invent a URL."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["query"], "properties": {
            "query": {"type": "string", "description": "What to search for, as you would type it."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 10, "description": "At most this many hits (default 5)."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if query.is_empty() {
            return Ok(json!({"searched": false, "note": "say what to search for"}));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(5)
            .clamp(1, SEARCH_RESULTS);
        let encoded: String = query
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_owned(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        let url = format!("https://{SEARCH_HOST}/html/?q={encoded}");
        let policy = Some(Arc::new(crate::connectors::effective_egress_policy(
            &state,
            Some(SEARCH_HOST),
        )));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| tool_err(format!("http client: {e}")))?;
        let transport =
            crate::connectors::ReqwestConnectorTransport::new(client, policy, WEB_SEARCH);
        use rusty_agent_runtime::connector::ConnectorTransport as _;
        let response = match transport
            .send(rusty_agent_runtime::connector::CheckRequest {
                method: rusty_agent_runtime::connector::HttpMethod::Get,
                url: url.clone(),
                headers: vec![
                    (
                        "user-agent".to_owned(),
                        "Mozilla/5.0 (compatible; rusty-server/web.search)".to_owned(),
                    ),
                    ("accept".to_owned(), "text/html".to_owned()),
                ],
                timeout: std::time::Duration::from_secs(20),
                max_response_bytes: PAGE_BYTES,
                body: None,
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let message = error.to_string();
                if message.contains("egress") {
                    let failure = rusty_agent_runtime::tool::ToolFailure::new(
                        "denied",
                        WEB_SEARCH,
                        format!(
                            "the egress policy does not allow the search engine's host ({SEARCH_HOST}): {message}"
                        ),
                        false,
                        false,
                        "say you could not search and do not invent a URL; an operator allows html.duckduckgo.com in Settings → Security → Sites agents may reach",
                    );
                    return Err(rusty_agent_runtime::error::RustyError::Tool(
                        serde_json::to_string(&failure).unwrap_or(message),
                    ));
                }
                return Ok(json!({"searched": false, "query": query, "note": message}));
            }
        };
        if !(200..300).contains(&response.status) {
            return Ok(
                json!({"searched": false, "query": query, "status": response.status, "note": format!("the search engine answered {}", response.status)}),
            );
        }
        let html = String::from_utf8_lossy(&response.body).to_string();
        let hits = search_hits(&html, limit);
        Ok(json!({
            "searched": true,
            "query": query,
            "engine": "duckduckgo (html)",
            "hits": hits,
            "note": if hits.is_empty() { "no hits the engine's page named — try other words" } else { "read a hit with web.fetch — its host must be allowed on the egress ceiling too" },
        }))
    }
}

#[cfg(test)]
mod search_hits_tests {
    use super::search_hits;

    #[test]
    fn hits_are_read_from_the_engines_result_page() {
        let html = r#"<div class="result"><h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdeveloper.jamf.com%2Fjamf-pro%2Freference&amp;rut=abc">Jamf Pro API <b>Reference</b></a></h2><a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">The Jamf Pro API is a <b>RESTful</b> interface&#39;s docs.</a></div>
<div class="result"><a class="result__a" href="https://docs.example.test/page">Second hit</a><div class="result__snippet">Plain snippet.</div></div>
<div class="result"><a class="result__a" href="javascript:void(0)">Not a page</a></div>"#;
        let hits = search_hits(html, 5);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].title, "Jamf Pro API Reference");
        assert_eq!(hits[0].url, "https://developer.jamf.com/jamf-pro/reference");
        assert_eq!(
            hits[0].snippet,
            "The Jamf Pro API is a RESTful interface's docs."
        );
        assert_eq!(hits[1].url, "https://docs.example.test/page");
        assert_eq!(hits[1].snippet, "Plain snippet.");
        assert_eq!(search_hits(html, 1).len(), 1);
    }
}

/// What a page fetch answered: the status, the page's text (HTML read as
/// text) and its `<title>` when it has one.
pub(crate) struct FetchedPage {
    pub status: u16,
    pub text: String,
    pub title: Option<String>,
}

/// Why a page could not be read: the egress ceiling refused its host, or
/// the request itself failed.
pub(crate) enum FetchRefusal {
    Egress(String),
    Failed(String),
}

/// Fetch one https page through the platform's egress policy — the path
/// `web.fetch` and a person's *Add source from a web page* share: no
/// redirects followed, no credentials sent, bounded in bytes.
pub(crate) async fn fetch_page(
    state: &AppState,
    url: &str,
) -> std::result::Result<FetchedPage, FetchRefusal> {
    if !url.starts_with("https://") {
        return Err(FetchRefusal::Failed("an https URL is required".to_owned()));
    }
    let host = crate::connectors::host_of(url);
    let policy = Some(Arc::new(crate::connectors::effective_egress_policy(
        state,
        host.as_deref(),
    )));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| FetchRefusal::Failed(format!("http client: {e}")))?;
    let transport = crate::connectors::ReqwestConnectorTransport::new(client, policy, "web.fetch");
    use rusty_agent_runtime::connector::ConnectorTransport as _;
    let response = transport
        .send(rusty_agent_runtime::connector::CheckRequest {
            method: rusty_agent_runtime::connector::HttpMethod::Get,
            url: url.to_owned(),
            headers: vec![
                ("user-agent".to_owned(), "rusty-server/web.fetch".to_owned()),
                (
                    "accept".to_owned(),
                    "text/html, text/plain, application/json".to_owned(),
                ),
            ],
            timeout: std::time::Duration::from_secs(30),
            max_response_bytes: PAGE_BYTES,
            body: None,
        })
        .await
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("egress") {
                FetchRefusal::Egress(message)
            } else {
                FetchRefusal::Failed(message)
            }
        })?;
    let raw = String::from_utf8_lossy(&response.body).to_string();
    let html = raw.trim_start().starts_with('<');
    let title = if html {
        let lower = raw.to_lowercase();
        lower.find("<title").and_then(|at| {
            let open = raw[at..].find('>')? + at + 1;
            let close = lower[open..].find("</title>")? + open;
            let t = page_text(&raw[open..close]);
            let t = t.trim().to_owned();
            (!t.is_empty()).then_some(t)
        })
    } else {
        None
    };
    let text = if html {
        page_text(content_region(&raw))
    } else {
        raw
    };
    Ok(FetchedPage {
        status: response.status,
        text,
        title,
    })
}

struct WebFetch(Doors);

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web.fetch"
    }
    fn description(&self) -> &str {
        "Read one web page as text — a product's documentation, a vendor's reference — from a host the platform's egress policy allows (the connected systems' hosts and the operator's allow-list; any other host is refused, and the refusal says so). Read-only; no credentials are sent. Use it to learn from documentation, then answer from what the page says, citing the URL."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["url"], "properties": {
            "url": {"type": "string", "description": "An https URL."},
            "max_chars": {"type": "integer", "description": "At most this many characters of text (default 20000)."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if !url.starts_with("https://") {
            return Ok(json!({"fetched": false, "url": url, "note": "an https URL is required"}));
        }
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(PAGE_CHARS)
            .clamp(500, PAGE_CHARS);
        let page = match fetch_page(&state, &url).await {
            Ok(page) => page,
            // A host the egress ceiling does not allow is a refusal, not a
            // page that failed: the loop's failure taxonomy (`denied`) —
            // the model says so and does not retry; the campaign counts it.
            Err(FetchRefusal::Egress(message)) => {
                let failure = rusty_agent_runtime::tool::ToolFailure::new(
                    "denied",
                    "web.fetch",
                    format!("the egress policy does not allow that host ({url}): {message}"),
                    false,
                    false,
                    "say the host is not allowed and do not invent the page; an operator widens the ceiling in Settings → Security → Sites agents may reach",
                );
                return Err(rusty_agent_runtime::error::RustyError::Tool(
                    serde_json::to_string(&failure).unwrap_or(message),
                ));
            }
            Err(FetchRefusal::Failed(message)) => {
                return Ok(json!({"fetched": false, "url": url, "note": message}));
            }
        };
        if !(200..300).contains(&page.status) {
            return Ok(
                json!({"fetched": false, "url": url, "status": page.status, "note": format!("the page answered {}", page.status)}),
            );
        }
        let response_status = page.status;
        let text = page.text;
        // What the page says is someone else's words: a note this run
        // writes from here on is marked as learned from outside content.
        if let Some(run) = rusty_agent_runtime::tool::current_run() {
            taint_run(&run.run_id, format!("the page {url}"));
        }
        let shown: String = text.chars().take(max_chars).collect();
        Ok(json!({
            "fetched": true,
            "url": url,
            "status": response_status,
            "chars": text.chars().count(),
            "truncated": shown.chars().count() < text.chars().count(),
            "text": shown,
        }))
    }
}

#[cfg(test)]
mod page_text_tests {
    use super::{content_region, page_text};

    #[test]
    fn a_fetched_page_keeps_its_main_content_and_drops_the_chrome() {
        let page = "<html><body><a href='#m'>Skip to main content</a>\
            <div>This browser is no longer supported.</div><nav>Docs | Learn</nav>\
            <main id=\"main\"><h1>Outlook troubleshooting</h1><p>Rebuild the OST.</p></main>\
            <footer>Privacy</footer></body></html>";
        let text = page_text(content_region(page));
        assert!(
            text.contains("Outlook troubleshooting") && text.contains("Rebuild the OST."),
            "{text}"
        );
        assert!(
            !text.contains("Skip to main")
                && !text.contains("no longer supported")
                && !text.contains("Privacy"),
            "{text}"
        );
        // An <article> stands in when there is no <main>; a <mainframe> is not one.
        let article = "<mainframe>x</mainframe><header>Site</header><article><p>Body</p></article>";
        assert_eq!(page_text(content_region(article)).trim(), "Body");
        // A page that marks neither keeps everything.
        assert_eq!(content_region("<p>plain</p>"), "<p>plain</p>");
    }

    #[test]
    fn a_page_reads_as_its_text_without_scripts_styles_or_tags() {
        let html = "<html><head><style>body{}</style><script>var x=1;</script></head><body><h1>Encoded &amp; queries</h1><p>Use <b>LIKE</b> for contains.</p><ul><li>one</li><li>two</li></ul><table><tr><td>a</td><td>b</td></tr></table></body></html>";
        let text = page_text(html);
        assert!(text.starts_with("Encoded & queries"), "{text}");
        assert!(text.contains("Use LIKE for contains."));
        assert!(text.contains("one\ntwo"));
        assert!(text.contains("| a | b"));
        assert!(!text.contains("var x") && !text.contains("body{}"));
    }
}

// ---- catalog.mcp ----------------------------------------------------------

struct CatalogMcp(Doors);

#[async_trait]
impl Tool for CatalogMcp {
    fn name(&self) -> &str {
        "catalog.mcp"
    }
    fn description(&self) -> &str {
        "The MCP servers this platform connects to and the tools each one mounts (`server.tool`). Adding a server is an administrator's job in Catalog → MCP; name what is needed and they add it."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let mut overview = crate::mcp_servers::overview(&state);
        if let Some(servers) = overview.get_mut("servers").and_then(Value::as_array_mut) {
            for server in servers {
                if let Some(tools) = server.get_mut("tools").and_then(Value::as_array_mut) {
                    for tool in tools {
                        if let Some(description) = tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(|d| brief(d, 90))
                        {
                            tool["description"] = json!(description);
                        }
                    }
                }
            }
        }
        Ok(overview)
    }
}

// ---- agents.list ----------------------------------------------------------

struct AgentsList(Doors);

#[async_trait]
impl Tool for AgentsList {
    fn name(&self) -> &str {
        "agents.list"
    }
    fn description(&self) -> &str {
        "The agents that already exist, with their tools and skills — so you extend or reuse one rather than build a duplicate."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let agents: Vec<Value> = state
            .server_store
            .list_assistants()
            .await
            .map_err(tool_err)?
            .into_iter()
            .filter_map(|a| {
                let id = tenant.unscope(&a.assistant_id)?.to_owned();
                Some(json!({
                    "assistant_id": id,
                    "name": a.name,
                    "description": a.metadata.get("description"),
                    "tools": a.config.pointer("/studio_intent/tools"),
                    "skills": a.config.pointer("/studio_intent/skills"),
                }))
            })
            .collect();
        Ok(json!({ "agents": agents }))
    }
}

/// An assistant by external id or by name. The name as a model writes it:
/// case aside, a leading "the" and a trailing "agent" or "desk" aside
/// ("the Incident Q&A agent" is Incident Q&A); failing that, the one live
/// agent whose name contains the words. An ambiguous name finds nothing —
/// the caller lists the candidates (`agent_candidates`) for the model.
pub(crate) async fn find_assistant(
    state: &AppState,
    tenant: &TenantContext,
    who: &str,
) -> Result<Option<crate::assistants::AssistantRecord>> {
    let views = state
        .server_store
        .list_assistants()
        .await
        .map_err(tool_err)?;
    let wanted = agent_name_key(who);
    let mut loose: Vec<&crate::assistants::AssistantView> = Vec::new();
    for view in &views {
        let Some(external) = tenant.unscope(&view.assistant_id) else {
            continue;
        };
        if external == who || view.name == who {
            return state
                .server_store
                .get_assistant(&view.assistant_id)
                .await
                .map_err(tool_err);
        }
        if view.archived_at.is_none() && !wanted.is_empty() {
            let name = agent_name_key(&view.name);
            if name == wanted {
                return state
                    .server_store
                    .get_assistant(&view.assistant_id)
                    .await
                    .map_err(tool_err);
            }
            if name.contains(&wanted) {
                loose.push(view);
            }
        }
    }
    match loose.as_slice() {
        [one] => state
            .server_store
            .get_assistant(&one.assistant_id)
            .await
            .map_err(tool_err),
        _ => Ok(None),
    }
}

/// A name as a model writes it, reduced for matching: lowercase, "the"
/// in front and "agent"/"desk" behind dropped, spaces collapsed.
fn agent_name_key(name: &str) -> String {
    // A model writes a name the way it saw it, HTML-escaped included:
    // `Incident Q&amp;A` is Incident Q&A.
    let lower = name
        .trim()
        .to_lowercase()
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut words: Vec<&str> = lower.split_whitespace().collect();
    if words.first() == Some(&"the") {
        words.remove(0);
    }
    if words.len() > 1
        && matches!(
            words.last(),
            Some(&"agent") | Some(&"desk") | Some(&"assistant")
        )
    {
        words.pop();
    }
    words.join(" ")
}

/// Live agents whose names share a word with `who`, for a refusal that
/// helps the model name one: at most eight names.
async fn agent_candidates(state: &AppState, tenant: &TenantContext, who: &str) -> Vec<String> {
    let key = agent_name_key(who);
    let words: Vec<&str> = key.split_whitespace().filter(|w| w.len() > 2).collect();
    let Ok(views) = state.server_store.list_assistants().await else {
        return Vec::new();
    };
    let mut out: Vec<String> = views
        .iter()
        .filter(|v| v.archived_at.is_none() && tenant.unscope(&v.assistant_id).is_some())
        .filter(|v| {
            let name = agent_name_key(&v.name);
            words.iter().any(|w| name.contains(w))
        })
        .map(|v| v.name.clone())
        .collect();
    out.sort();
    out.dedup();
    out.truncate(8);
    out
}

/// The skills an agent's intent names, each with the tools its procedure
/// assumes and which of those the agent does not have. A skill that says
/// "retrieve with `kb_search`" on an agent without it makes the agent
/// refuse where it should search — and nothing in the run says why.
async fn skills_with_missing_tools(state: &AppState, tenant: &str, config: &Value) -> Vec<Value> {
    let agent_tools: Vec<String> = config
        .pointer("/studio_intent/tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let names: Vec<String> = config
        .pointer("/studio_intent/skills")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for name in names {
        match state.skills.get(tenant, &name).await {
            Some(version) => {
                let assumes = version.metadata().allowed_tools.clone();
                let missing: Vec<&String> = assumes
                    .iter()
                    .filter(|t| !agent_tools.contains(t))
                    .collect();
                out.push(json!({
                    "name": name,
                    "assumes_tools": assumes,
                    "missing_tools": missing,
                    "note": if missing.is_empty() { Value::Null } else { json!("its procedure names tools this agent does not have; the agent follows the procedure and stops where the tool should be") },
                }));
            }
            None => out.push(json!({"name": name, "note": "no skill of this name is registered"})),
        }
    }
    out
}

// ---- agents.read ----------------------------------------------------------

struct AgentsRead(Doors);

#[async_trait]
impl Tool for AgentsRead {
    fn name(&self) -> &str {
        AGENTS_READ
    }
    fn description(&self) -> &str {
        "One agent in full: its charter (the instructions its model reads first), its tools with when each is for, the skills it follows, its versions, and the proposals filed for it — each waiting, or declined by a person with their reason. By id or by name. Also its goal with how far its live runs of the last seven days are from it, and the gaps it filed that stay open — the brief a review starts from."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["agent"], "properties": {"agent": {"type": "string", "description": "The agent's id or name."}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let who = args
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let Some(record) = find_assistant(&state, &tenant, &who).await? else {
            return Ok(
                json!({"found": false, "agent": who, "note": "no agent has that id or name — agents.list names them"}),
            );
        };
        let external = tenant
            .unscope(&record.assistant_id)
            .unwrap_or(&record.assistant_id)
            .to_owned();
        let skills = skills_with_missing_tools(&state, &tenant_now(), &record.config).await;
        // What each of its tools is and takes, from the graph's catalog: a
        // charter that says "search" without the query's shape is one the
        // model may not manage, and a Coach cannot write the shape blind.
        let named: Vec<String> = record
            .config
            .pointer("/studio_intent/tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let tools_detail: Vec<Value> = state
            .registry
            .tool_capabilities(&record.graph)
            .into_iter()
            .filter(|c| named.contains(&c.name))
            .map(|c| {
                let params: Vec<Value> = c
                    .parameters_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|props| {
                        props
                            .iter()
                            .map(|(k, v)| json!({"name": k, "type": v.get("type"), "description": v.get("description")}))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"name": c.name, "description": c.description, "effect": c.effect, "parameters": params, "required": c.parameters_schema.get("required")})
            })
            .collect();
        // What was already proposed for this agent, and what a person said
        // no to: a Coach that reads this does not file the same idea twice,
        // and writes the next change against the reason.
        let active_id = record.active_version_id();
        let active_at = record.version(&active_id).map(|v| v.created_at);
        let proposals: Vec<Value> = record
            .versions
            .iter()
            .filter(|v| v.version_id != active_id && v.metadata.get("proposed_by").is_some())
            .filter(|v| active_at.is_none_or(|at| v.created_at > at))
            .map(|v| {
                json!({
                    "version_id": v.version_id,
                    "why": v.metadata.get("why"),
                    "filed_at": v.created_at,
                    "charter": v.config.pointer("/studio_intent/instructions"),
                    "status": if record.decline_of(&v.version_id).is_some() { "declined" } else { "waiting" },
                    "declined": record.decline_of(&v.version_id).map(|d| json!({"by": d.by.get("name").cloned().unwrap_or(d.by.clone()), "reason": d.reason, "at": d.at})),
                })
            })
            .collect();
        let goal = goal_brief(&state, &tenant, &record, &external).await;
        let gaps = gaps_filed_brief(
            &state,
            &tenant,
            &[external.as_str(), record.assistant_id.as_str()],
        )
        .await;
        Ok(json!({
            "found": true,
            "assistant_id": external,
            "name": record.name,
            "description": record.metadata.get("description"),
            "charter": record.config.pointer("/studio_intent/instructions"),
            "tools": record.config.pointer("/studio_intent/tools"),
            "tools_detail": tools_detail,
            "skills": skills,
            "goal": goal,
            "gaps_filed": gaps,
            "active_version_id": active_id,
            "versions": record.versions.len(),
            "proposals": proposals,
        }))
    }
}

/// The goal the studio keeps on the agent (`metadata.studio.goal`) measured
/// where the Goal card's number comes from (`goal::measure`), with the target
/// read against it. `null` when the agent has no goal; `current` null when
/// no run counted.
async fn goal_brief(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    record: &crate::assistants::AssistantRecord,
    external: &str,
) -> Value {
    let Some(goal) = record
        .metadata
        .pointer("/studio/goal")
        .filter(|g| g.is_object())
    else {
        return Value::Null;
    };
    let metric = goal
        .get("metric")
        .and_then(Value::as_str)
        .unwrap_or(crate::goal::METRICS[0])
        .to_owned();
    let target = goal.get("target").and_then(Value::as_f64).unwrap_or(0.0);
    let lower_is_better = goal
        .get("lowerIsBetter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let m = crate::goal::measure(state, tenant, external, &metric).await;
    let current = m.current;
    let counted = m.sample;
    let on_target = current.map(|c| {
        if lower_is_better {
            c <= target
        } else {
            c >= target
        }
    });
    let shortfall = current.map(|c| {
        if lower_is_better {
            (c - target).max(0.0)
        } else {
            (target - c).max(0.0)
        }
    });
    let mut body = m.to_value();
    body["objective"] = goal.get("objective").cloned().unwrap_or(Value::Null);
    body["target"] = json!(target);
    body["lower_is_better"] = json!(lower_is_better);
    body["on_target"] = json!(on_target);
    body["shortfall"] = json!(shortfall);
    body["note"] = json!(match (current, on_target) {
        (None, _) => "no live run counted in the window — nothing to measure yet".to_owned(),
        (Some(c), Some(true)) => format!("on target: {c}% against {target}%"),
        (Some(c), _) => format!(
            "below target: {c}% against {target}% over {counted} run(s) — the shortfall the review is for"
        ),
    });
    body
}

/// The gaps this agent filed that stay open, by priority, at most eight:
/// what it could not do for the people it serves, in their words.
async fn gaps_filed_brief(state: &AppState, tenant: &TenantContext, actors: &[&str]) -> Value {
    let Ok(ledger) = crate::routes::load_gap_ledger(state, tenant.tenant()).await else {
        return json!([]);
    };
    let mut mine: Vec<&rusty_agent_runtime::gaps::GapLedgerEntry> = actors
        .iter()
        .flat_map(|actor| ledger.filed_by(actor).collect::<Vec<_>>())
        .filter(|e| {
            !matches!(
                e.status,
                rusty_agent_runtime::gaps::GapStatus::Closed
                    | rusty_agent_runtime::gaps::GapStatus::Parked
            )
        })
        .collect();
    mine.sort_by(|a, b| a.gap_id.cmp(&b.gap_id));
    mine.dedup_by(|a, b| a.gap_id == b.gap_id);
    mine.sort_by(|a, b| {
        b.priority_score()
            .cmp(&a.priority_score())
            .then_with(|| b.filed_at.cmp(&a.filed_at))
    });
    let open = mine.len();
    let listed: Vec<Value> = mine
        .iter()
        .take(8)
        .map(|e| {
            json!({
                "gap_id": e.gap_id,
                "question": match &e.subject { rusty_agent_runtime::gaps::GapSubject::QuestionShape { text } => text.clone(), other => format!("{other:?}") },
                "missing": e.statement,
                "askers": e.volume,
                "status": e.status,
                "filed_at": e.filed_at,
            })
        })
        .collect();
    json!({"open": open, "listed": listed})
}

/// What an agent's recent runs say about the charter that runs now: the
/// failures before it, and the runs under it with how they went. One
/// tally, read by runs.review to say so and by agents.revise to refuse.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CharterEvidence {
    pub since: Option<chrono::DateTime<Utc>>,
    pub seen: usize,
    pub failed_before: usize,
    pub runs_under: usize,
    pub failed_under: usize,
    pub verified_under: usize,
}

impl CharterEvidence {
    /// Tally runs as (started at, verdict) against the moment the charter
    /// that runs was created. A run with no timestamp, or no charter
    /// moment, counts as seen and nothing else.
    pub(crate) fn tally<'a>(
        runs: impl IntoIterator<Item = (Option<chrono::DateTime<Utc>>, Option<&'a str>)>,
        since: Option<chrono::DateTime<Utc>>,
    ) -> Self {
        let mut out = Self {
            since,
            seen: 0,
            failed_before: 0,
            runs_under: 0,
            failed_under: 0,
            verified_under: 0,
        };
        for (at, verdict) in runs {
            out.seen += 1;
            let (Some(at), Some(since)) = (at, since) else {
                continue;
            };
            if at >= since {
                out.runs_under += 1;
                match verdict {
                    Some("failed") => out.failed_under += 1,
                    Some("verified") => out.verified_under += 1,
                    _ => {}
                }
            } else if verdict == Some("failed") {
                out.failed_before += 1;
            }
        }
        out
    }

    /// The failures were answered already: every failed run predates the
    /// charter that runs, runs under it exist, and none of them failed.
    pub(crate) fn already_answered(&self) -> bool {
        self.failed_before > 0 && self.runs_under > 0 && self.failed_under == 0
    }

    pub(crate) fn as_json(&self) -> Value {
        json!({
            "charter_since": self.since,
            "runs_seen": self.seen,
            "failed_before_current_charter": self.failed_before,
            "runs_under_current_charter": self.runs_under,
            "failed_under_current_charter": self.failed_under,
            "verified_under_current_charter": self.verified_under,
        })
    }
}

/// The tally for one agent from the same recall runs.review reads.
async fn charter_evidence(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    record: &crate::assistants::AssistantRecord,
    external: &str,
) -> Result<CharterEvidence> {
    let since = record
        .version(&record.active_version_id())
        .map(|v| v.created_at);
    let recalled = crate::routes::recall_runs(state, tenant, 100)
        .await
        .map_err(|e| tool_err(format!("{e:?}")))?;
    let runs: Vec<(Option<chrono::DateTime<Utc>>, Option<String>)> = recalled
        .into_iter()
        .map(crate::routes::RecalledRun::into_wire)
        .filter(|run| run.get("assistant_id").and_then(Value::as_str) == Some(external))
        .map(|run| {
            (
                run.get("created_at")
                    .and_then(Value::as_str)
                    .and_then(|t| t.parse::<chrono::DateTime<Utc>>().ok()),
                run.pointer("/verification/verdict")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect();
    Ok(CharterEvidence::tally(
        runs.iter().map(|(at, v)| (*at, v.as_deref())),
        since,
    ))
}

// ---- runs.review ----------------------------------------------------------

struct RunsReview(Doors);

#[async_trait]
impl Tool for RunsReview {
    fn name(&self) -> &str {
        RUNS_REVIEW
    }
    fn description(&self) -> &str {
        "An agent's recent runs with what the verifier said about each: verdict (verified, failed, unverified), the reason, what the run was asked, and what it did (calls, writes, refusals). Failed runs first. Read this before changing a charter."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["agent"], "properties": {
            "agent": {"type": "string", "description": "The agent's id or name."},
            "limit": {"type": "integer", "description": "At most this many runs (default 12, max 40)."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let who = args
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(12)
            .clamp(1, 40) as usize;
        let Some(record) = find_assistant(&state, &tenant, &who).await? else {
            return Ok(
                json!({"found": false, "agent": who, "note": "no agent has that id or name"}),
            );
        };
        let external = tenant
            .unscope(&record.assistant_id)
            .unwrap_or(&record.assistant_id)
            .to_owned();
        // A run older than the charter that runs now ran under an earlier
        // one; a failure there may be fixed already. Say so per run.
        let charter_since = record
            .version(&record.active_version_id())
            .map(|v| v.created_at);
        let recalled = crate::routes::recall_runs(&state, &tenant, 100)
            .await
            .map_err(|e| tool_err(format!("{e:?}")))?;
        let mut runs: Vec<Value> = Vec::new();
        for run in recalled
            .into_iter()
            .map(crate::routes::RecalledRun::into_wire)
        {
            if run.get("assistant_id").and_then(Value::as_str) != Some(external.as_str()) {
                continue;
            }
            let run_id = run
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let asked = match crate::routes::recall_run(&state, &tenant, &run_id).await {
                Ok(Some(info)) => info
                    .input
                    .as_ref()
                    .and_then(|input| input.get("messages"))
                    .and_then(Value::as_array)
                    .and_then(|messages| {
                        messages
                            .iter()
                            .rev()
                            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                    })
                    .and_then(|m| m.get("content").and_then(Value::as_str))
                    .map(|text| text.chars().take(400).collect::<String>()),
                _ => None,
            };
            let verification = run.get("verification").cloned().unwrap_or(Value::Null);
            let created_at = run
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|t| t.parse::<chrono::DateTime<Utc>>().ok());
            let under_current = match (created_at, charter_since) {
                (Some(at), Some(since)) => Some(at >= since),
                _ => None,
            };
            runs.push(json!({
                "run_id": run_id,
                "status": run.get("status"),
                "created_at": run.get("created_at"),
                "under_current_charter": under_current,
                "channel": run.get("channel").or(run.get("trigger")),
                "asked": asked,
                "verdict": verification.get("verdict"),
                "reason": verification.get("reason"),
                "did": verification.get("evidence").map(|e| json!({
                    "calls": e.get("calls").and_then(Value::as_array).map(|c| c.iter().map(|call| format!("{} ({})", call.get("tool").and_then(Value::as_str).unwrap_or("?"), call.get("outcome").and_then(Value::as_str).unwrap_or("?"))).collect::<Vec<_>>()),
                    "writes": e.get("writes"),
                    "refused": e.get("refused"),
                })),
            }));
        }
        let rank = |v: &Value| match v.get("verdict").and_then(Value::as_str) {
            Some("failed") => 0,
            Some("unverified") => 1,
            Some("verified") => 2,
            _ => 3,
        };
        runs.sort_by_key(rank);
        let failed = runs.iter().filter(|r| rank(r) == 0).count();
        let verified = runs.iter().filter(|r| rank(r) == 2).count();
        let evidence = CharterEvidence::tally(
            runs.iter().map(|r| {
                (
                    r.get("created_at")
                        .and_then(Value::as_str)
                        .and_then(|t| t.parse::<chrono::DateTime<Utc>>().ok()),
                    r.get("verdict").and_then(Value::as_str),
                )
            }),
            charter_since,
        );
        let total = runs.len();
        // The agent's open gaps, by class: what the platform found missing
        // (a connector its skills assume, a tool), what the agent itself
        // filed, what others filed against it. A gap is the agent's when
        // one of its runs is the evidence. None of these is a charter
        // fault; the Coach reads them so it does not write one for them.
        let run_ids: std::collections::HashSet<String> = runs
            .iter()
            .filter_map(|r| r.get("run_id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let mut gaps = json!({ "connectors_missing": [], "tools_missing": [], "filed_by_the_agent": [], "other": [] });
        if let Ok(ledger) = crate::routes::load_gap_ledger(&state, tenant.tenant()).await {
            for entry in ledger.work_order() {
                if !entry.evidence.iter().any(|c| run_ids.contains(&c.id)) {
                    continue;
                }
                let subject = match &entry.subject {
                    rusty_agent_runtime::gaps::GapSubject::QuestionShape { text } => text.clone(),
                    rusty_agent_runtime::gaps::GapSubject::Intent { intent_id } => {
                        intent_id.clone()
                    }
                };
                let item = json!({
                    "gap_id": entry.gap_id,
                    "subject": subject,
                    "what_was_missing": entry.statement,
                    "closes_when": if entry.closes_on_tools().is_empty() { "a person closes it".to_owned() } else { format!("{} becomes available", entry.closes_on_tools().join(", ")) },
                    "runs_affected": entry.evidence.iter().filter(|c| run_ids.contains(&c.id)).count(),
                    "volume": entry.volume,
                });
                let class = match entry.origin {
                    rusty_agent_runtime::gaps::GapOrigin::Platform
                        if subject.starts_with("connector:") =>
                    {
                        "connectors_missing"
                    }
                    rusty_agent_runtime::gaps::GapOrigin::Platform => "tools_missing",
                    rusty_agent_runtime::gaps::GapOrigin::AgentDeclared => "filed_by_the_agent",
                    _ => "other",
                };
                if let Some(list) = gaps.get_mut(class).and_then(Value::as_array_mut) {
                    list.push(item);
                }
            }
        }
        runs.truncate(limit);
        let note = if evidence.already_answered() {
            Some(format!(
                "every failed run ({}) predates the charter that runs now, and the {} runs under it have none failed ({} verified): the charter was already changed for those failures. agents.revise will refuse to file for them; say so to the person and file nothing.",
                evidence.failed_before, evidence.runs_under, evidence.verified_under
            ))
        } else if failed > 0 && evidence.failed_under == 0 {
            Some("every failed run predates the charter that runs now and nothing has run under it yet; it may already have fixed them — say so before filing anything".to_owned())
        } else {
            None
        };
        Ok(json!({
            "found": true,
            "assistant_id": external,
            "name": record.name,
            "charter_since": charter_since,
            "runs_seen": total,
            "failed": failed,
            "failed_under_current_charter": evidence.failed_under,
            "verified": verified,
            "evidence": evidence.as_json(),
            "note": note,
            "gaps": gaps,
            "gaps_note": "connectors_missing and tools_missing were found by the platform from what the agent's skills declare: a failure that needed one of them is not a charter fault — say the connection is missing and file nothing for it. filed_by_the_agent is what the agent said it lacked; a charter can tell it what to do without the tool, nothing more.",
            "runs": runs,
        }))
    }
}

// ---- runs.read ------------------------------------------------------------

struct RunsRead(Doors);

#[async_trait]
impl Tool for RunsRead {
    fn name(&self) -> &str {
        RUNS_READ
    }
    fn description(&self) -> &str {
        "One run in detail: every tool call with the arguments the model gave and what came back, and the final reply. Read a failed run here before deciding what to change — a call with a malformed argument reads as a clean miss in the verdict."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["run_id"], "properties": {"run_id": {"type": "string", "description": "The run's id, from runs.review."}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let run_id = args
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let evidence = crate::routes::run_evidence(&state, &tenant, &run_id)
            .await
            .map_err(|e| tool_err(format!("{e:?}")))?;
        let events = evidence
            .journal
            .map(|snapshot| snapshot.events)
            .unwrap_or_default();
        let clip = |v: &Value| {
            let text = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            text.chars().take(600).collect::<String>()
        };
        let mut calls = Vec::new();
        let mut reply: Option<String> = None;
        for event in &events {
            let Ok(v) = serde_json::to_value(event) else {
                continue;
            };
            match v.get("kind").and_then(Value::as_str) {
                Some("tool_call") => calls.push(json!({
                    "tool": v.pointer("/input/value/tool"),
                    "arguments": v.pointer("/input/value/arguments").map(&clip),
                    "result": v.pointer("/output/value").map(&clip),
                })),
                Some("model_call") => {
                    if let Some(text) = v
                        .pointer("/output/value/message/content")
                        .and_then(Value::as_str)
                    {
                        if !text.trim().is_empty() {
                            reply = Some(text.chars().take(800).collect());
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(json!({"run_id": run_id, "calls": calls, "reply": reply, "events": events.len()}))
    }
}

// ---- agents.ask -----------------------------------------------------------

struct AgentsAsk(Doors);

#[async_trait]
impl Tool for AgentsAsk {
    fn name(&self) -> &str {
        AGENTS_ASK
    }
    fn description(&self) -> &str {
        "Ask another agent on this platform, in the person's own words, and get its final answer back — a specialist desk doing its part of the job. It runs as itself, with its own charter, tools and memory, for the same person; anything it must ask a person for pauses on its own gate. Answers with its reply, its run id and the verifier's verdict. One level deep: an agent asked this way cannot ask another. Pass the question through as the person put it, with what they said about themselves."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["agent", "message"], "properties": {
            "agent": {"type": "string", "description": "The agent's name or id, as agents.list shows it."},
            "message": {"type": "string", "description": "The question, in the person's words, with whatever they said that the other agent needs (their territory, the office, the incident number)."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let who = args
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let message = args
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if message.is_empty() {
            return Ok(json!({"asked": false, "note": "say what to ask"}));
        }
        let context = rusty_agent_runtime::tool::current_run();
        // One level deep: the asking run's depth is in its metadata; an
        // agent reached by an ask has depth 1 and may not ask further.
        let (depth, from_run, from_agent, attribution) = match &context {
            Some(ctx) => {
                let info = crate::routes::recall_run(&state, &tenant, &ctx.run_id)
                    .await
                    .ok()
                    .flatten();
                let depth = info
                    .as_ref()
                    .and_then(|i| i.metadata.as_ref())
                    .and_then(|m| m.pointer("/delegation/depth"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                (
                    depth,
                    Some(ctx.run_id.clone()),
                    ctx.agent_id.clone(),
                    ctx.attribution.clone(),
                )
            }
            None => (0, None, None, None),
        };
        if depth >= 1 {
            return Ok(
                json!({"asked": false, "note": "this run was itself started by an ask; an agent reached that way answers with its own tools and does not ask another"}),
            );
        }
        let Some(record) = find_assistant(&state, &tenant, &who).await? else {
            return Ok(
                json!({"asked": false, "agent": who, "note": "no agent has that id or name — agents.list names them"}),
            );
        };
        if record.archived_at.is_some() {
            return Ok(json!({"asked": false, "agent": who, "note": "that agent is archived"}));
        }
        // An agent another agent spawned answers no one until a person has
        // reviewed it: publishing it in the studio is that review, and it
        // asks for a written reason when the agent has no suite to gate it.
        // Without this a loop could spawn a maintainer and put it to work in
        // the same breath, with no evals and no person anywhere near it.
        if record.metadata.get("awaiting_review").is_some() {
            let external = tenant
                .unscope(&record.assistant_id)
                .unwrap_or(&record.assistant_id)
                .to_owned();
            return Ok(json!({
                "asked": false,
                "agent": external,
                "awaiting_review": record.metadata.get("awaiting_review").cloned(),
                "note": "that agent was spawned by an agent and no person has reviewed it yet; it answers once a person publishes it. Say what you would have asked it.",
            }));
        }
        let external = tenant
            .unscope(&record.assistant_id)
            .unwrap_or(&record.assistant_id)
            .to_owned();
        if from_agent.as_deref() == Some(external.as_str()) {
            return Ok(
                json!({"asked": false, "agent": external, "note": "that is this agent; answer it yourself"}),
            );
        }
        let thread_id = uuid::Uuid::new_v4().to_string();
        let internal_thread_id = scope_id(&tenant_now(), &thread_id);
        let thread = crate::threads::ThreadRecord {
            thread_id: thread_id.clone(),
            tenant: tenant_now(),
            graph: record.graph.clone(),
            metadata: json!({"trigger": "delegation", "assistant_id": external, "delegated_from": {"run_id": from_run, "agent_id": from_agent}}),
            forked_from: None,
            seed_length: None,
            created_at: Utc::now(),
        };
        state
            .server_store
            .create_thread(&internal_thread_id, &thread)
            .await
            .map_err(tool_err)?;
        let mut metadata = json!({
            "delegation": {"depth": depth + 1, "from_run": from_run, "from_agent": from_agent},
        });
        if let Some(who) = &attribution {
            // The same person: the other agent remembers and recalls for
            // them, and its approvals are theirs to decide.
            metadata["created_by"] = who.clone();
            metadata["on_behalf_of"] = who.clone();
        }
        // The asked agent runs where the asking one runs, and under what it
        // must not do: in the same world (a stand-in's question is answered
        // in the stand-in, never by the live system behind it) and with the
        // same forbidden tools (a constraint is not shed by asking someone
        // else to break it).
        let inherited = match &from_run {
            Some(run_id) => match state.server_store.get_accepted_run(run_id).await {
                Ok(Some(accepted)) => accepted.payload.config,
                _ => None,
            },
            None => None,
        };
        let carried = inherited.as_ref().and_then(|c| {
            match (
                c.world.clone(),
                c.forbidden_tools.clone().filter(|f| !f.is_empty()),
            ) {
                (None, None) => None,
                (world, forbidden_tools) => Some(crate::runs::RunConfigPayload {
                    world,
                    worlds: c.worlds.clone(),
                    forbidden_tools,
                    ..crate::runs::RunConfigPayload::default()
                }),
            }
        });
        let mut payload = crate::runs::RunPayload {
            input: Some(json!({"messages": [{"role": "user", "content": message}]})),
            metadata: Some(metadata),
            assistant_id: Some(record.assistant_id.clone()),
            config: carried,
            ..crate::runs::RunPayload::default()
        };
        crate::routes::apply_assistant_defaults(
            &state,
            &tenant_now(),
            &internal_thread_id,
            &record,
            &mut payload,
        )
        .await;
        let scheduled = crate::runs::schedule(
            &state.run_deps,
            &internal_thread_id,
            &thread_id,
            &record.graph,
            payload,
            crate::runs::MultitaskStrategy::Enqueue,
        )
        .await
        .map_err(|e| tool_err(format!("the other agent's run was not accepted: {e:?}")))?;
        let run_id = scheduled.run_id.clone();
        let mut terminal = scheduled.terminal;
        let ask_wait = state.config.ask_wait;
        let finished = tokio::time::timeout(ask_wait, terminal.wait_for(|v| v.is_some()))
            .await
            .is_ok();
        if !finished {
            return Ok(json!({
                "asked": true,
                "agent": record.name,
                "assistant_id": external,
                "run_id": run_id,
                "status": "still running",
                "note": format!("{} has not answered within {} seconds; its run continues and its answer is in Observe under this run id — say so, do not invent its answer", record.name, ask_wait.as_secs()),
            }));
        }
        let mut terminal = terminal.borrow().clone().unwrap_or(Value::Null);
        // The asked run paused at the approval gate: wait for the person's
        // decision, within what is left of the ask's patience, and answer
        // with what the asked agent did once decided — the asker's reply
        // then says the outcome, not the pause. A denial is an answer too.
        // Undecided by the deadline, say so; the run continues once decided.
        let started = std::time::Instant::now();
        let mut undecided = false;
        let mut denied = false;
        let mut final_run = run_id.clone();
        while terminal.get("status").and_then(Value::as_str) == Some("interrupted") {
            let left = ask_wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                undecided = true;
                break;
            }
            match tokio::time::timeout(
                left,
                crate::dataset_runs::follow_approval(&state, &final_run),
            )
            .await
            {
                Ok(Some(resumed)) => {
                    // Denied, the continuation is the agent saying what it
                    // would have done; its reply, not the claim before the
                    // pause, is what the asker hears.
                    denied = state
                        .run_deps
                        .approvals
                        .load(&final_run)
                        .is_some_and(|r| r.status == "denied");
                    terminal = match tokio::time::timeout(
                        ask_wait.saturating_sub(started.elapsed()),
                        crate::dataset_runs::wait_terminal(&state, &resumed),
                    )
                    .await
                    {
                        Ok(t) => t,
                        Err(_) => {
                            undecided = true;
                            break;
                        }
                    };
                    final_run = resumed;
                }
                Ok(None) => {
                    // No approval of ours: the asked run ends here.
                    break;
                }
                Err(_) => {
                    undecided = true;
                    break;
                }
            }
        }
        let status = terminal
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let reply = state
            .checkpointer
            .get_latest(&internal_thread_id)
            .await
            .ok()
            .flatten()
            .map(|cp| cp.state.to_value())
            .and_then(|st| st.get("messages").and_then(Value::as_array).cloned())
            .and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .find(|m| {
                        m.get("role").and_then(Value::as_str) == Some("assistant")
                            && m.get("content")
                                .and_then(Value::as_str)
                                .is_some_and(|c| !c.trim().is_empty())
                    })
                    .and_then(|m| m.get("content").and_then(Value::as_str).map(str::to_owned))
            });
        // The verdict lands a few seconds after the terminal; wait a little
        // for it, then answer without it rather than hold the caller.
        let mut verdict = None;
        if !undecided {
            for _ in 0..12 {
                if let Some(v) = state.verifications.load(&final_run) {
                    verdict = Some(v);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        let note = if undecided {
            Some(format!(
                "it paused for a person's decision (Inbox) and none came within {} seconds; its run continues once decided — say so, do not invent the outcome",
                ask_wait.as_secs()
            ))
        } else if denied {
            Some(
                "a person declined what it asked to do; it finished without it — say so".to_owned(),
            )
        } else if reply.is_none() {
            Some("it answered nothing; say so".to_owned())
        } else {
            None
        };
        Ok(json!({
            "asked": true,
            "agent": record.name,
            "assistant_id": external,
            "run_id": run_id,
            "resumed_run_id": if final_run == run_id { Value::Null } else { json!(final_run) },
            "status": status,
            "interrupted": undecided,
            "decided": if final_run == run_id { Value::Null } else { json!(if denied { "denied" } else { "approved" }) },
            "reply": reply,
            "verdict": verdict.as_ref().and_then(|v| v.get("verdict").cloned()),
            "verdict_reason": verdict.as_ref().and_then(|v| v.get("reason").cloned()),
            "note": note,
        }))
    }
}

// ---- knowledge.read -------------------------------------------------------

/// The whole of a knowledge source `search_knowledge` cited, in order. The
/// search answers with an excerpt that can stop mid-sentence; a maintainer
/// then reached for `read_document`, which reads files, and reported the
/// fact as unconfirmed. This reads the source the citation names.
struct KnowledgeRead(Doors);

#[async_trait]
impl Tool for KnowledgeRead {
    fn name(&self) -> &str {
        KNOWLEDGE_READ
    }
    fn description(&self) -> &str {
        "Read a whole knowledge source, in order, by the source_id a search_knowledge citation carries — when the excerpt stopped short and you need the rest before you state a fact. Answers the full text with the chunk boundaries marked, so you can cite the chunk you used."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["source_id"], "properties": {
            "source_id": {"type": "string", "description": "The source_id from a search_knowledge citation, e.g. vpn-access-runbook-internal."},
            "max_chars": {"type": "integer", "description": "Stop after this many characters (default 20000).", "minimum": 500}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let source_id = text(&args, "source_id");
        if source_id.is_empty() {
            return Ok(
                json!({"read": false, "note": "say which source: the source_id a search_knowledge citation carries"}),
            );
        }
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .unwrap_or(20_000)
            .max(500) as usize;
        let base = crate::knowledge::knowledge_base(&state, &tenant);
        let versions = base
            .versions_of(&source_id)
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        let Some(latest) = versions.last() else {
            return Ok(
                json!({"read": false, "source_id": source_id, "note": "no knowledge source has that id; use the source_id exactly as the citation carries it"}),
            );
        };
        let mut chunks = state
            .knowledge
            .chunks_of(tenant.tenant(), &latest.content_hash)
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        chunks.sort_by_key(|c| c.chunk_index);
        let mut out = String::new();
        let mut read = 0usize;
        let mut truncated = false;
        for chunk in &chunks {
            let Some(body) = base
                .chunk_content(&chunk.content_address)
                .await
                .map_err(|e| tool_err(e.to_string()))?
            else {
                continue;
            };
            if out.len() + body.len() > max_chars {
                truncated = true;
                break;
            }
            out.push_str(&format!(
                "\n[chunk {} · {}]\n",
                chunk.chunk_index, chunk.chunk_id
            ));
            out.push_str(&body);
            read += 1;
        }
        Ok(json!({
            "read": true,
            "source_id": source_id,
            "title": latest.title,
            "chunks": chunks.len(),
            "chunks_read": read,
            "truncated": truncated,
            "text": out,
        }))
    }
}

// ---- gaps.file ------------------------------------------------------------

/// An agent's own report that it could not finish: a capability it needed and
/// did not have, or knowledge nothing in the workspace could source. It lands
/// in the gap ledger as `agent_declared`, where a person or a hunt works it.
struct GapsFile(Doors);

#[async_trait]
impl Tool for GapsFile {
    fn name(&self) -> &str {
        GAPS_FILE
    }
    fn description(&self) -> &str {
        "File what stopped you into the gap backlog: a tool that does not exist, or knowledge no source could answer. Say what you were trying to do, what was missing, and how many askers it affects. It is filed for a person to work, not answered now; file it once and carry on with what you can do."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["question", "missing"], "properties": {
            "question": {"type": "string", "description": "The question or task you were serving, in the asker's words."},
            "missing": {"type": "string", "description": "What you did not have, in one sentence — name the tool you would need and what it would do, or the fact no source could answer."},
            "tool": {"type": "string", "description": "When what stopped you is a tool or a connector's operation you can name, its exact name (`connector.operation`, as catalog.tools lists it, e.g. `microsoft-365.list-service-health`). The gap closes by itself the moment that tool becomes available."},
            "volume": {"type": "integer", "description": "How many askers this affects over a month, when you counted it. Default 1.", "minimum": 1},
            "needs_a_person": {"type": "boolean", "description": "True when only the business can decide it (a policy contradiction). False (default) when building the missing capability closes it."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        // A rehearsal stands in for the systems, not for the platform: a
        // tool the desk lacked in a stand-in world is lacking for real, so
        // the gap is filed for real — its evidence says the run rehearsed.
        let rehearsal = rehearsal_world();
        let state = self.0.open()?;
        let tenant = context_now();
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let missing = args
            .get("missing")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if question.is_empty() || missing.is_empty() {
            return Ok(
                json!({"filed": false, "note": "say both: the question you were serving, and what you did not have"}),
            );
        }
        let volume = args
            .get("volume")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        // The tool that would close it: named outright, else the first
        // `connector.operation` the sentence mentions.
        let tool = args
            .get("tool")
            .and_then(Value::as_str)
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .or_else(|| {
                // Of the tools the sentence names, the one the platform has
                // — "agents.directory/agents.list" names the real one second
                // — else the first, so the gap closes on the tool that can
                // exist rather than on a name nobody will ever register.
                let named = rusty_agent_runtime::gaps::tools_named(&missing);
                let available = crate::routes::available_tool_names(&state);
                named
                    .iter()
                    .find(|t| rusty_agent_runtime::gaps::tool_available(t, &available))
                    .cloned()
                    .or_else(|| named.into_iter().next())
            });
        let closure = if args
            .get("needs_a_person")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            rusty_agent_runtime::gaps::ClosureCriteria::BusinessDecisionRequired
        } else if let Some(tool) = tool.clone() {
            // The capability arriving closes it — no person, no tally.
            rusty_agent_runtime::gaps::ClosureCriteria::CapabilityPresent { tool }
        } else {
            // Building the capability is the deliverable; the ledger closes it
            // when the failures it causes stop.
            rusty_agent_runtime::gaps::ClosureCriteria::FailureRateBelow {
                threshold_millis: 50,
            }
        };
        let subject = rusty_agent_runtime::gaps::GapSubject::QuestionShape {
            text: question.trim().to_lowercase(),
        };
        // The run that hit the wall is the evidence: the ledger refuses a gap
        // filed on nothing, and this is the one record that proves the need.
        let run = rusty_agent_runtime::tool::current_run();
        let Some(run_id) = run.as_ref().map(|r| r.run_id.clone()) else {
            return Ok(
                json!({"filed": false, "note": "a gap is filed from a run, and this call has none"}),
            );
        };
        let filer = run
            .as_ref()
            .and_then(|r| r.agent_id.clone())
            .unwrap_or_else(|| "agent".to_owned());
        let evidence = vec![rusty_agent_runtime::gaps::Citation {
            kind: rusty_agent_runtime::gaps::CitationKind::RunReceipt,
            id: run_id,
            note: Some(match &rehearsal {
                Some(world) => format!(
                    "the run that needed it (a rehearsal in the stand-in world `{world}`): {missing}"
                ),
                None => format!("the run that needed it: {missing}"),
            }),
        }];
        let now = Utc::now();
        let gap_id = crate::routes::mutate_gap_ledger(&state, &tenant, |ledger| {
            ledger.file_gap(
                subject,
                missing.clone(),
                evidence,
                rusty_agent_runtime::gaps::GapOrigin::AgentDeclared,
                closure,
                volume,
                0,
                // The agent that hit the wall, not a generic "agent": the
                // roster counts a maintainer's open gaps by this.
                &filer,
                now,
            )
        })
        .await;
        // A refusal is an answer, not a retry: the model is told plainly and
        // moves on rather than filing the same gap five times.
        let gap_id = match gap_id {
            Ok(id) => id,
            Err(e) => {
                return Ok(
                    json!({"filed": false, "note": format!("the ledger refused it: {e}. Do not send this again — say what you can answer without the missing capability.")}),
                );
            }
        };
        Ok(json!({
            "filed": true,
            "gap_id": gap_id,
            "worked_by": "a person or a hunt, from the gap backlog",
            "closes_when": match &tool { Some(t) => format!("`{t}` becomes available"), None => "a person closes it".to_owned() },
            "note": "filed — do not file this again this run; say what you can answer without it",
        }))
    }
}

// ---- plan ------------------------------------------------------------------

/// The agent's plan: written and kept current here, read back by the loop
/// into every model call, checked before the final answer.
struct PlanTool;

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &str {
        PLAN
    }
    fn description(&self) -> &str {
        "Write your plan for a request with more than one part, and keep it current: the steps in order, each todo, doing, done, or skipped with a reason. The plan is shown to you on every turn, and you are not done until every step is done or skipped — an answer with steps open is sent back once. Call it again to update statuses; send the whole plan each time."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["steps"], "properties": {
            "steps": {"type": "array", "maxItems": rusty_agent_runtime::react::PLAN_MAX_STEPS, "items": {"type": "object", "required": ["text"], "properties": {
                "text": {"type": "string", "description": "One step, imperative, under 200 characters."},
                "status": {"type": "string", "enum": ["todo", "doing", "done", "skipped"], "description": "Default todo."},
                "note": {"type": "string", "description": "Why it was skipped, or what it found."}
            }}}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let Some(plan) = rusty_agent_runtime::react::Plan::parse(&args) else {
            return Ok(
                json!({"kept": false, "note": "a plan is a list of steps with text; send at least one"}),
            );
        };
        let open = plan.open().len();
        Ok(json!({
            "kept": true,
            "steps": plan.steps.len(),
            "done": plan.done(),
            "open": open,
            "note": if open == 0 { "every step is done or skipped — answer now" } else { "the plan is on your situation each turn; work the open steps, update it, then answer" },
        }))
    }
}

// ---- gaps.work_order / gaps.resolve ---------------------------------------

/// The closure criterion in words, for the model.
fn closes_when(entry: &rusty_agent_runtime::gaps::GapLedgerEntry) -> String {
    use rusty_agent_runtime::gaps::ClosureCriteria as C;
    match &entry.closure_criteria {
        C::CapabilityPresent { tool } => {
            format!("`{tool}` becomes available, or a run answers it and is verified")
        }
        C::FailureRateBelow { .. } => {
            "a run answers it and the verifier confirms the answer (gaps.resolve)".to_owned()
        }
        C::BusinessDecisionRequired => "a person decides it".to_owned(),
        C::ArtifactPromoted { candidate_id } => format!("candidate `{candidate_id}` is promoted"),
        C::BlockFilled { block_label } => format!("the memory block `{block_label}` is filled"),
    }
}

struct GapsWorkOrder(Doors);

#[async_trait]
impl Tool for GapsWorkOrder {
    fn name(&self) -> &str {
        GAPS_WORK_ORDER
    }
    fn description(&self) -> &str {
        "The platform's gap backlog in priority order: what agents could not answer or do, filed from their runs, with what closes each. Read it to pick work: a question you can answer from your knowledge, skills and tools — answer it in full, then gaps.resolve it with its gap_id. A gap that needs a tool or a person is not yours to close."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "limit": {"type": "integer", "description": "How many to read, highest priority first. Default 5, at most 20.", "minimum": 1, "maximum": 20},
            "all": {"type": "boolean", "description": "True to include gaps a run has claimed, gaps waiting on a person, and closed ones. Default false: the open queue only."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = context_now();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 20) as usize;
        let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
        let ledger = crate::routes::load_gap_ledger(&state, tenant.tenant())
            .await
            .map_err(|e| RustyError::Tool(format!("{e:?}")))?;
        let order = ledger.work_order();
        let open = order.len();
        // `all`: every entry the ledger holds — claimed, waiting on a
        // person, closed — by priority; the queue holds the open ones only.
        let listed: Vec<&rusty_agent_runtime::gaps::GapLedgerEntry> = if all {
            let mut every: Vec<_> = ledger.entries().collect();
            every.sort_by(|a, b| {
                b.priority_score()
                    .cmp(&a.priority_score())
                    .then_with(|| a.filed_at.cmp(&b.filed_at))
            });
            every
        } else {
            order
        };
        let gaps: Vec<Value> = listed
            .into_iter()
            .take(limit)
            .map(|e| {
                let question = match &e.subject {
                    rusty_agent_runtime::gaps::GapSubject::QuestionShape { text } => text.clone(),
                    other => serde_json::to_value(other)
                        .ok()
                        .and_then(|v| {
                            v.get("intent_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_default(),
                };
                json!({
                    "gap_id": e.gap_id,
                    "question": question,
                    "missing": e.statement,
                    "status": e.status,
                    "askers": e.volume,
                    "priority": e.priority_score(),
                    "filed_at": e.filed_at,
                    "closes_when": closes_when(e),
                    "needs_tool": e.closes_on_tools(),
                    "runs_cited": e.evidence.len(),
                })
            })
            .collect();
        Ok(json!({
            "open": open,
            "shown": gaps.len(),
            "gaps": gaps,
            "note": if open == 0 { "the queue is empty: nothing is filed that a run has not claimed" } else { "answer one you can from what you know and the tools you have, then gaps.resolve it; a gap that needs a tool you lack or a person's decision is not yours" },
        }))
    }
}

struct GapsResolve(Doors);

#[async_trait]
impl Tool for GapsResolve {
    fn name(&self) -> &str {
        GAPS_RESOLVE
    }
    fn description(&self) -> &str {
        "Claim a gap from gaps.work_order that you have answered in this run: it leaves the queue, and closes when this run's outcome is verified — the verifier reads your answer. A verdict that is not verified puts it back. Answer the question in full in your reply first; say in `how` where the answer came from."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["gap_id", "how"], "properties": {
            "gap_id": {"type": "string", "description": "The gap's id, as gaps.work_order gave it."},
            "how": {"type": "string", "description": "Where the answer came from, in one sentence: the skill, the source, the tool you read."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        if let Some(world) = rehearsal_world() {
            return Ok(rehearsal_note("claim on a gap", &world));
        }
        let state = self.0.open()?;
        let tenant = context_now();
        let gap_id = args
            .get("gap_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let how = args
            .get("how")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if gap_id.is_empty() || how.is_empty() {
            return Ok(
                json!({"claimed": false, "note": "say both: the gap_id from gaps.work_order, and where your answer came from"}),
            );
        }
        let run = rusty_agent_runtime::tool::current_run();
        let Some(run_id) = run.as_ref().map(|r| r.run_id.clone()) else {
            return Ok(
                json!({"claimed": false, "note": "a gap is claimed from a run, and this call has none"}),
            );
        };
        let agent = run
            .as_ref()
            .and_then(|r| r.agent_id.clone())
            .unwrap_or_else(|| "agent".to_owned());
        let actor = format!("run:{run_id}");
        let now = Utc::now();
        let claimed = crate::routes::mutate_gap_ledger(&state, &tenant, |ledger| {
            ledger.claim(&gap_id, &actor, now)
        })
        .await;
        if let Err(e) = claimed {
            return Ok(
                json!({"claimed": false, "gap_id": gap_id, "note": format!("the ledger refused it: {e}. Do not send this again — the gap is not open, or the id is not one gaps.work_order gave.")}),
            );
        }
        crate::gaps::record_claim(
            &state,
            &run_id,
            crate::gaps::GapClaim {
                tenant: tenant.tenant().to_owned(),
                gap_id: gap_id.clone(),
                how,
                agent,
            },
        )
        .await;
        Ok(json!({
            "claimed": true,
            "gap_id": gap_id,
            "closes_when": "this run's outcome is verified; a verdict that is not verified returns it to the backlog",
            "note": "claimed — the verifier judges the answer in your reply, so give it in full",
        }))
    }
}

// ---- tasks.enqueue --------------------------------------------------------

struct TasksEnqueue(Doors);

#[async_trait]
impl Tool for TasksEnqueue {
    fn name(&self) -> &str {
        TASKS_ENQUEUE
    }
    fn description(&self) -> &str {
        "Queue work for later, or for another agent: one task into a named pool, with one paragraph saying what is to be done and for whom. An agent configured for that pool picks it up as its own run; you will not see its result in this run. Use it for a follow-up, a batch, a second pair of eyes — never to repeat this run's own job. A task queued by a task's run counts down a chain budget of three, and past it the queue refuses, so a chain of work cannot fan out without end."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["pool", "message"], "properties": {
            "pool": {"type": "string", "description": "The pool that works it, e.g. `filing`, `research`, `default`. An agent must be configured for the pool or the task waits."},
            "message": {"type": "string", "description": "What is to be done and for whom, in one paragraph, with every fact the worker needs — it starts with nothing but this."},
            "kind": {"type": "string", "description": "A short label for the kind of work (default `ask`)."},
            "world": {"type": "string", "description": "A stand-in world to rehearse it in, by name; absent means live systems."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    fn idempotency_key(&self, args: &Value) -> Option<String> {
        let pool = text(args, "pool");
        let message = text(args, "message");
        if pool.is_empty() || message.is_empty() {
            return None;
        }
        Some(format!(
            "tasks.enqueue:{pool}:{}",
            rusty_agent_runtime::record::sha256_hex(message.as_bytes())
        ))
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let run = rusty_agent_runtime::tool::current_run()
            .ok_or_else(|| tool_err("tasks.enqueue only works inside a run"))?;
        let tenant = context_now();
        let pool = text(&args, "pool").to_lowercase();
        let message = text(&args, "message");
        if message.is_empty() {
            return Err(tool_err("say what is to be done: `message`, one paragraph"));
        }
        if message.len() > 4_000 {
            return Err(tool_err("one task per call, under 4000 characters"));
        }
        crate::tasks::validate_pool(&pool).map_err(tool_err)?;
        let kind = {
            let k = text(&args, "kind");
            if k.is_empty() {
                "ask".to_owned()
            } else {
                k
            }
        };
        // The chain budget: the task or assignment this run works on, when
        // it works one, carries the depth its own run was started at.
        let (mine, own_depth, carried) = chain_position(&state, &tenant, &run).await?;
        let depth = own_depth + 1;
        if depth > MAX_CHAIN_DEPTH {
            return Ok(json!({
                "queued": false,
                "chain_depth": depth,
                "limit": MAX_CHAIN_DEPTH,
                "note": format!("not queued: this run is itself {} tasks deep, and a chain of work stops at {}. Finish here and say what is left for a person.", depth - 1, MAX_CHAIN_DEPTH),
            }));
        }
        // The chain's token budget follows the work: spent, nothing more starts.
        let (root, cap, spend) = match chain_budget(&state, &tenant, &run, carried.as_ref()).await?
        {
            Ok(v) => v,
            Err(mut refused) => {
                refused["queued"] = json!(false);
                refused["chain_depth"] = json!(depth);
                return Ok(refused);
            }
        };
        let agent = run.agent_id.clone().unwrap_or_else(|| "agent".to_owned());
        let mut payload = json!({
            "message": message,
            "enqueued_by": run.attribution.clone().unwrap_or_else(|| json!({"kind": "agent", "agent_id": agent})),
            "chain": {"depth": depth, "run_id": run.run_id, "agent_id": agent, "parent_task_id": mine.as_ref().map(|t| t.task_id.clone()), "root_run_id": root, "max_tokens": cap},
        });
        let chain_budget_words = json!({"root_run_id": root, "spent_tokens": spend.spent_tokens, "max_tokens": cap, "runs": spend.runs});
        let world = text(&args, "world");
        if !world.is_empty() {
            payload["world"] = json!(world);
        }
        let request = crate::routes::EnqueueTaskPayload {
            kind,
            payload,
            pool: Some(pool.clone()),
            max_attempts: Some(1),
            idempotency_key: self.idempotency_key(&args),
            parent_task_id: mine.as_ref().map(|t| t.task_id.clone()),
            ..Default::default()
        };
        let mut record = crate::routes::build_task_record(request, &tenant)
            .map_err(|e| tool_err(format!("{e:?}")))?;
        if let Some(name) = payload_world_name(&state, &tenant, &record.payload).await {
            record.payload["world_name"] = json!(name);
        }
        crate::routes::enforce_task_quota(&state, &tenant, 1)
            .await
            .map_err(|e| tool_err(format!("{e:?}")))?;
        let (task, deduplicated) = state
            .server_store
            .enqueue_task(&record)
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        Ok(json!({
            "queued": true,
            "task_id": task.task_id,
            "pool": pool,
            "chain_depth": depth,
            "chain_budget": chain_budget_words,
            "deduplicated": deduplicated,
            "note": format!("queued for the `{pool}` pool; an agent configured for it works it as its own run — you will not see the result here. Do not queue this again."),
        }))
    }
}

/// Where the current run sits in a chain of agent-started work: the task
/// it works (and that task), or the assignment round it is, each carrying
/// the depth it was started at; a run a person or a schedule started is at
/// depth 0.
async fn chain_position(
    state: &AppState,
    tenant: &TenantContext,
    run: &rusty_agent_runtime::tool::RunContext,
) -> Result<(Option<crate::tasks::TaskRecord>, u32, Option<Value>)> {
    let mine = state
        .server_store
        .list_tasks(tenant.tenant(), None)
        .await
        .map_err(|e| tool_err(e.to_string()))?
        .into_iter()
        .find(|t| t.run_id.as_deref() == Some(run.run_id.as_str()));
    if let Some(task) = mine {
        let chain = task.payload.get("chain").cloned();
        let depth = chain
            .as_ref()
            .and_then(|c| c.get("depth"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        return Ok((Some(task), depth, chain));
    }
    let assignments = state
        .server_store
        .list_assignments(Some(tenant.tenant()))
        .await
        .map_err(|e| tool_err(e.to_string()))?;
    let chain = assignments
        .iter()
        .find(|a| {
            a.rounds.iter().any(|r| {
                r.run_id == run.run_id || r.resumed_run_id.as_deref() == Some(run.run_id.as_str())
            })
        })
        .and_then(|a| a.chain.clone());
    let depth = chain
        .as_ref()
        .and_then(|c| c.get("depth"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Ok((None, depth, chain))
}

/// The chain a run's new work joins — root and cap — and what that chain
/// has spent: refused in words when the cap is spent.
async fn chain_budget(
    state: &AppState,
    tenant: &TenantContext,
    run: &rusty_agent_runtime::tool::RunContext,
    carried: Option<&Value>,
) -> Result<std::result::Result<(String, u64, crate::chain_spend::ChainSpend), Value>> {
    // The run's own config first: the working copy under test carries its
    // cap; a chain already started keeps the cap it was started with.
    let own_cap = state
        .run_deps
        .manager
        .snapshot(&run.run_id)
        .await
        .and_then(|s| s.payload.config.as_ref().and_then(|c| c.chain_max_tokens));
    let agent_config = match run.agent_id.as_deref() {
        Some(id) => find_assistant(state, tenant, id).await?.map(|r| r.config),
        None => None,
    };
    let (root, mut cap) = crate::chain_spend::chain_of(carried, &run.run_id, agent_config.as_ref());
    if carried.and_then(|c| c.get("root_run_id")).is_none() {
        if let Some(own) = own_cap {
            cap = own;
        }
    }
    let spend = crate::chain_spend::read(state.server_store.as_ref(), &root).await;
    if crate::chain_spend::spent(&spend, cap) {
        return Ok(Err(
            json!({"chain_budget": {"root_run_id": root, "spent_tokens": spend.spent_tokens, "max_tokens": cap, "runs": spend.runs}, "note": crate::chain_spend::words(&spend, cap)}),
        ));
    }
    Ok(Ok((root, cap, spend)))
}

// ---- assignment.create -----------------------------------------------------

struct AssignmentCreate(Doors);

#[async_trait]
impl Tool for AssignmentCreate {
    fn name(&self) -> &str {
        ASSIGNMENT_CREATE
    }
    fn description(&self) -> &str {
        "Hand another agent a durable piece of work, as a person would from the studio: it works in rounds until it says done or needs a person, keeps its own record, and the owner reads it in Observe → Assignments. For work with an end and a record — a report, a clean-up, a check across many records — not a question (agents.ask answers now) and not your own later work (tasks.enqueue). Say the request verbatim and what done looks like. You will not see the result in this run; say what you delegated and to whom. When the work ends you are told through your memory: a note under the assignment, recalled at the start of your next run and by the words that name the work."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["agent", "request"], "properties": {
            "agent": {"type": "string", "description": "The agent to entrust: its id or exact name (agents.list)."},
            "request": {"type": "string", "description": "What you want achieved, in one paragraph — kept verbatim as the agent's brief."},
            "success": {"type": "string", "description": "What done looks like, in one sentence."},
            "constraints": {"type": "string", "description": "What it must not do or touch, when there is such a thing."},
            "max_rounds": {"type": "integer", "description": "The most rounds it may take. Default 3, at most 5 from an agent.", "minimum": 1, "maximum": 5},
            "world": {"type": "string", "description": "A stand-in world (by name) its rounds rehearse in. Default: the world this run rehearses in, if any; live systems otherwise."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let run = rusty_agent_runtime::tool::current_run()
            .ok_or_else(|| tool_err("assignment.create only works inside a run"))?;
        let tenant = context_now();
        let who = text(&args, "agent");
        let request = text(&args, "request");
        if who.is_empty() || request.is_empty() {
            return Ok(
                json!({"created": false, "note": "say both: the agent to entrust (id or name), and the request in one paragraph"}),
            );
        }
        if request.len() > 4_000 {
            return Ok(
                json!({"created": false, "note": "one request per call, under 4000 characters"}),
            );
        }
        let Some(record) = find_assistant(&state, &tenant, &who).await? else {
            let candidates = agent_candidates(&state, &tenant, &who).await;
            return Ok(json!({
                "created": false,
                "agent": who,
                "candidates": candidates,
                "note": if candidates.is_empty() { "no agent has that id or name; nothing similar is on this platform — say so, and do the work yourself if you can".to_owned() } else { "no agent has exactly that name; the candidates are the live agents whose names share a word with it — pick one by its exact name, once".to_owned() },
            }));
        };
        let external = tenant
            .unscope(&record.assistant_id)
            .unwrap_or(&record.assistant_id)
            .to_owned();
        let me = run.agent_id.clone().unwrap_or_else(|| "agent".to_owned());
        if me == external || me == record.assistant_id {
            return Ok(
                json!({"created": false, "agent": external, "note": "an agent does not assign work to itself: give it to another agent, or queue your own later work with tasks.enqueue"}),
            );
        }
        // The chain budget: work this run starts sits one deeper than the
        // work it does, and a chain stops at the limit.
        let (_, own_depth, carried) = chain_position(&state, &tenant, &run).await?;
        let depth = own_depth + 1;
        if depth > MAX_CHAIN_DEPTH {
            return Ok(json!({
                "created": false,
                "chain_depth": depth,
                "limit": MAX_CHAIN_DEPTH,
                "note": format!("not delegated: this run is itself {own_depth} deep in agent-started work, and a chain stops at {MAX_CHAIN_DEPTH}. Finish here and say what is left for a person."),
            }));
        }
        let world = {
            let w = text(&args, "world");
            if w.is_empty() {
                rehearsal_world()
            } else {
                Some(w)
            }
        };
        let owner = json!({
            "kind": "agent",
            "principal_id": me,
            "name": run.attribution.as_ref().and_then(|a| a.get("name")).and_then(Value::as_str).map(|n| format!("{n} (via an agent)")).unwrap_or_else(|| "an agent".to_owned()),
            "agent_id": me,
            "run_id": run.run_id,
        });
        let input = crate::assignments::CreateAssignment {
            assistant_id: external.clone(),
            request: request.clone(),
            world,
            worlds: Vec::new(),
            tags: vec!["delegated-by-agent".to_owned()],
            forbidden_tools: Vec::new(),
            success: Some(text(&args, "success")).filter(|s| !s.is_empty()),
            constraints: Some(text(&args, "constraints")).filter(|s| !s.is_empty()),
            max_rounds: args
                .get("max_rounds")
                .and_then(Value::as_u64)
                .map(|r| r.clamp(1, 5)),
            max_tokens_per_round: None,
            deadline: None,
        };
        // The chain's token budget follows the work: spent, nothing more starts.
        let (root, cap, spend) = match chain_budget(&state, &tenant, &run, carried.as_ref()).await?
        {
            Ok(v) => v,
            Err(mut refused) => {
                refused["created"] = json!(false);
                refused["chain_depth"] = json!(depth);
                return Ok(refused);
            }
        };
        let chain = json!({"depth": depth, "run_id": run.run_id, "agent_id": me, "root_run_id": root, "max_tokens": cap});
        let a = crate::assignments::create(&state, &tenant, input, owner, Some(chain))
            .await
            .map_err(|e| tool_err(format!("{e:?}")))?;
        let chain_budget_words = json!({"root_run_id": root, "spent_tokens": spend.spent_tokens, "max_tokens": cap, "runs": spend.runs});
        Ok(json!({
            "created": true,
            "assignment_id": a.assignment_id,
            "agent": record.name,
            "state": a.state,
            "chain_depth": depth,
            "chain_budget": chain_budget_words,
            "world": a.world_name,
            "note": format!("delegated to {}; it works in rounds and keeps its record in Observe → Assignments. You will not see the result here — say what you delegated. Do not delegate this again.", record.name),
        }))
    }
}

/// The name of a world the task's payload names, when the platform knows
/// it; the task route resolves worlds the same way.
async fn payload_world_name(
    state: &AppState,
    tenant: &TenantContext,
    payload: &Value,
) -> Option<String> {
    let world = payload.get("world").and_then(Value::as_str)?;
    let resolved =
        crate::worlds::resolve_worlds(state, tenant.tenant(), Some(world), &[], "the task")
            .await
            .ok()?;
    resolved.first().map(|w| w.name.clone())
}

// ---- agents.revise --------------------------------------------------------

struct AgentsRevise(Doors);

#[async_trait]
impl Tool for AgentsRevise {
    fn name(&self) -> &str {
        AGENTS_REVISE
    }
    fn description(&self) -> &str {
        "File a revision of an agent as a new version: its charter, the tools it may call, the skills it follows — any of them, in one call. Nothing changes until a person applies it in Improve, where the reason you give is shown beside it. Give the change, not the whole text: `add_first` puts one imperative step first among what the agent must do; `replace` swaps one sentence for another (from → to, exact text). `instructions` (the whole charter) only when many parts change."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["agent", "why"], "properties": {
            "agent": {"type": "string", "description": "The agent's id or name."},
            "add_first": {"type": "string", "description": "One imperative step to put first among what the agent must do, written as the agent should run it — concrete words, never placeholders in angle brackets, e.g. `Before answering anything, pick two single words from the question (email, phone) and call servicenow.list-records on table kb_knowledge with sysparm_query short_descriptionLIKEemail^ORtextLIKEemail^ORshort_descriptionLIKEphone^ORtextLIKEphone, fields number,short_description,text, limit 5`."},
            "replace": {"type": "array", "items": {"type": "object", "required": ["from", "to"], "properties": {"from": {"type": "string"}, "to": {"type": "string"}}}, "description": "Sentences to swap: `from` must appear in the current charter exactly."},
            "instructions": {"type": "string", "description": "The complete revised charter, only when many parts change."},
            "add_tools": {"type": "array", "items": {"type": "string"}, "description": "Tools the agent may call from now on — exact names from catalog.tools or catalog.connectors."},
            "remove_tools": {"type": "array", "items": {"type": "string"}, "description": "Tools it may no longer call."},
            "add_skills": {"type": "array", "items": {"type": "string"}, "description": "Skills it follows from now on — names registered with skills.register or listed by catalog.skills."},
            "remove_skills": {"type": "array", "items": {"type": "string"}, "description": "Skills it no longer follows."},
            "why": {"type": "string", "description": "The pattern in the failed runs this removes — or what the person asked for — in one or two sentences."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        if let Some(world) = rehearsal_world() {
            return Ok(rehearsal_note("revision", &world));
        }
        let state = self.0.open()?;
        let tenant = context_now();
        let who = args
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let why = args
            .get("why")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if why.is_empty() {
            return Ok(
                json!({"filed": false, "note": "say why: the pattern in the failed runs this removes"}),
            );
        }
        let Some(record) = find_assistant(&state, &tenant, &who).await? else {
            return Ok(
                json!({"filed": false, "agent": who, "note": "no agent has that id or name"}),
            );
        };
        let external = tenant
            .unscope(&record.assistant_id)
            .unwrap_or(&record.assistant_id)
            .to_owned();
        // The floor under the Coach's judgment: when every failure predates
        // the charter that runs and the runs under it pass, the charter was
        // already changed for those failures — nothing is filed for them,
        // whatever the model sends.
        let evidence = charter_evidence(&state, &tenant, &record, &external).await?;
        if evidence.already_answered() {
            return Ok(json!({
                "filed": false,
                "assistant_id": external,
                "evidence": evidence.as_json(),
                "note": format!(
                    "every failed run ({}) is older than the charter that runs now ({}); the {} runs under it have none failed ({} verified). The charter was already changed for those failures, so nothing was filed. Tell the person these numbers. A change they want anyway is theirs to make on the agent's page.",
                    evidence.failed_before,
                    evidence.since.map(|t| t.format("since %Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "since its creation".to_owned()),
                    evidence.runs_under,
                    evidence.verified_under
                ),
            }));
        }
        // What the agent may call and follow, when the revision changes it.
        let names = |key: &str| -> Vec<String> {
            args.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let (add_tools, remove_tools, add_skills, remove_skills) = (
            names("add_tools"),
            names("remove_tools"),
            names("add_skills"),
            names("remove_skills"),
        );
        let capability_change = !(add_tools.is_empty()
            && remove_tools.is_empty()
            && add_skills.is_empty()
            && remove_skills.is_empty());
        if !add_tools.is_empty() {
            let offered: Vec<String> = state
                .registry
                .tool_capabilities(&self.0.graph)
                .into_iter()
                .map(|c| c.name)
                .collect();
            let unknown: Vec<&String> = add_tools.iter().filter(|t| !offered.contains(t)).collect();
            if !unknown.is_empty() {
                return Ok(json!({
                    "filed": false,
                    "assistant_id": external,
                    "note": format!(
                        "these tools do not exist on the platform: {} — call catalog.tools (or catalog.connectors for a connected system's operations) and use exact names, or say what a person must connect first",
                        unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                }));
            }
        }
        if !add_skills.is_empty() {
            let found = skill_entries(&state, tenant.tenant(), &add_skills).await;
            if found.len() != add_skills.len() {
                return Ok(json!({
                    "filed": false,
                    "assistant_id": external,
                    "note": "not every skill named exists on the platform: register the procedure with skills.register first, or use a name from catalog.skills",
                }));
            }
        }
        let current = record
            .config
            .pointer("/studio_intent/instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        // The change, composed here: a step put first, sentences swapped, or
        // the whole text when the caller wrote one.
        let mut instructions = args
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| current.clone());
        if let Some(step) = args
            .get("add_first")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let step = step.trim_start_matches('-').trim();
            if instructions.contains(step) {
                return Ok(json!({
                    "filed": false,
                    "assistant_id": external,
                    "note": "that step is already in the charter, word for word, and the runs that failed ran under it: the model did not follow what it was told. That is not a charter gap. Either reword the step with a worked example of the failed case (replace), or say the charter is not the cause and file nothing. Do not send the same step again.",
                }));
            }
            instructions = put_first_step(&instructions, step);
        }
        if let Some(swaps) = args.get("replace").and_then(Value::as_array) {
            for swap in swaps {
                let from = swap.get("from").and_then(Value::as_str).unwrap_or("");
                let to = swap.get("to").and_then(Value::as_str).unwrap_or("");
                if from.is_empty() {
                    continue;
                }
                if !instructions.contains(from) {
                    return Ok(
                        json!({"filed": false, "note": format!("`replace.from` is not in the charter exactly: {from:?}; copy the sentence as it stands")}),
                    );
                }
                instructions = instructions.replace(from, to);
            }
        }
        // Two replacements that land on the same rule leave it twice; a
        // charter says each thing once.
        let instructions = dedupe_lines(instructions.trim());
        let charter_changed = current != instructions;
        if charter_changed && instructions.len() < 40 {
            return Ok(
                json!({"filed": false, "note": "the result is too short to be a charter; give add_first, replace, or the whole instructions"}),
            );
        }
        if !charter_changed && !capability_change {
            return Ok(json!({
                "filed": false,
                "assistant_id": external,
                "note": "this is the charter that runs already, word for word, so nothing was filed. A revision must contain a sentence the current charter does not: the step the failed runs skipped, in imperative form, first among the things it must do — or the rule they misread, reworded with an example. Write that sentence and file again.",
            }));
        }
        // A person said no to this exact charter once; their reason is the
        // fact the next change must answer.
        let declined_same = record.declined.iter().find(|d| {
            record
                .version(&d.version_id)
                .and_then(|v| {
                    v.config
                        .pointer("/studio_intent/instructions")
                        .and_then(Value::as_str)
                        .map(|s| dedupe_lines(s.trim()))
                })
                .as_deref()
                == Some(instructions.as_str())
        });
        if let Some(d) = declined_same {
            let who =
                d.by.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("a person");
            return Ok(json!({
                "filed": false,
                "assistant_id": external,
                "declined_version_id": d.version_id,
                "note": format!("{who} declined this exact charter on {}: {:?}. Nothing was filed. A decline's reason is a fact: change the change so it answers the reason, or say the runs are right and file nothing. Do not file it again.", d.at.format("%Y-%m-%d %H:%M UTC"), d.reason),
            }));
        }
        let mut config = record.config.clone();
        if config.pointer("/studio_intent").is_none() {
            config["studio_intent"] = json!({});
        }
        config["studio_intent"]["instructions"] = json!(instructions);
        if capability_change {
            let intent = &mut config["studio_intent"];
            let mut tools: Vec<Value> = intent
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            tools.retain(|t| {
                !t.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| remove_tools.iter().any(|r| r == n))
            });
            for name in &add_tools {
                if !tools
                    .iter()
                    .any(|t| t.get("name").and_then(Value::as_str) == Some(name))
                {
                    tools.push(json!({"name": name}));
                }
            }
            intent["tools"] = Value::Array(tools);
            let mut skills: Vec<String> = intent
                .get("skills")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            skills.retain(|k| !remove_skills.contains(k));
            for name in &add_skills {
                if !skills.contains(name) {
                    skills.push(name.clone());
                }
            }
            intent["skills"] = json!(skills);
        }
        let mut metadata = record.metadata.clone();
        metadata["proposed_by"] =
            json!({"principal_id": "coach", "name": COACH_NAME, "kind": "service"});
        metadata["why"] = json!(why);
        let active = record.active_version_id().to_owned();
        let version = crate::assistants::AssistantVersionRecord::new(
            Some(active.clone()),
            record.name.clone(),
            record.graph.clone(),
            config,
            metadata,
            Utc::now(),
        );
        let version_id = version.version_id.clone();
        state
            .server_store
            .create_assistant_version(&record.assistant_id, &active, &version)
            .await
            .map_err(tool_err)?;
        // The candidate gate: the suites bound to the agent start against
        // the version filed, so the person who opens the proposal reads a
        // verdict, not a promise. Without a suite nothing judges it, and
        // the answer says so.
        let gate = match state.server_store.get_assistant(&record.assistant_id).await {
            Ok(Some(filed)) => {
                crate::promotion::gate_candidate(
                    &state,
                    &tenant,
                    &filed,
                    &version_id,
                    json!({"proposed_by": "coach"}),
                )
                .await
            }
            _ => Vec::new(),
        };
        let judged = gate
            .iter()
            .filter(|g| g.get("evaluation_id").is_some())
            .count();
        let gate_note = if gate.is_empty() {
            "no suite is bound to this agent: nothing judges the candidate; record cases from its runs first".to_owned()
        } else {
            format!(
                "{judged} suite{} started against the candidate, each capped at ${:.2}; the proposal shows the verdict",
                if judged == 1 { "" } else { "s" },
                crate::promotion::CANDIDATE_BUDGET_USD
            )
        };
        Ok(json!({
            "filed": true,
            "assistant_id": external,
            "name": record.name,
            "version_id": version_id,
            "active_version_id": active,
            "charter_changed": charter_changed,
            "tools_added": add_tools,
            "tools_removed": remove_tools,
            "skills_added": add_skills,
            "skills_removed": remove_skills,
            "gate": {"suites": gate, "note": gate_note},
            "activates_when": "a person applies it in Improve",
        }))
    }
}

/// The same non-blank line kept once, first occurrence wins; blank lines stay.
fn dedupe_lines(text: &str) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let key = line.trim();
        if key.is_empty() || !seen.contains(&key) {
            if !key.is_empty() {
                seen.push(key);
            }
            out.push(line);
        }
    }
    out.join("\n")
}

/// A charter with `step` placed first among what the agent must do: under a
/// "What you must do:" heading as its first bullet when there is one, else
/// as a new first section after the opening paragraph.
fn put_first_step(charter: &str, step: &str) -> String {
    let step = step.trim().trim_start_matches('-').trim();
    if charter.contains(step) {
        return charter.to_owned();
    }
    let mut lines: Vec<String> = charter.lines().map(str::to_owned).collect();
    if let Some(at) = lines
        .iter()
        .position(|l| l.trim().to_lowercase().starts_with("what you must do"))
    {
        lines.insert(at + 1, format!("- {step}"));
        return lines.join("\n");
    }
    let first_blank = lines
        .iter()
        .position(|l| l.trim().is_empty())
        .unwrap_or(lines.len());
    let mut out = lines[..first_blank].to_vec();
    out.push(String::new());
    out.push("What you must do:".to_owned());
    out.push(format!("- {step}"));
    out.extend(lines[first_blank..].iter().cloned());
    out.join("\n")
}

// ---- agents.create --------------------------------------------------------

struct AgentsCreate(Doors);

#[async_trait]
impl Tool for AgentsCreate {
    fn name(&self) -> &str {
        "agents.create"
    }
    fn description(&self) -> &str {
        "Create an agent on the platform. It exists from the moment this returns: versioned, runnable in the Playground, editable in the studio. Every tool name must come from catalog.tools and every skill from catalog.skills or skills.register; unknown names are refused with the list of what was unknown. A name that already exists is refused unless allow_duplicate is true. Platform tools (catalog.*, agents.*, skills.*, connectors.*) are refused unless platform_tools is true."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "A short, specific name."},
                "description": {"type": "string", "description": "One line: what it is for."},
                "charter": {"type": "string", "description": "Its standing instructions, read before anything else every conversation: what it is, what it must do, what it must never do, how it answers, what done looks like."},
                "tools": {"type": "array", "items": {"type": "string"}, "description": "Exact tool names from catalog.tools, each optionally followed by ` — ` and when to use it (`servicenow.list-records — to read today's incidents`). The note is kept on the agent and shown to its model on the tool itself."},
                "skills": {"type": "array", "items": {"type": "string"}, "description": "Skill names it follows, from catalog.skills."},
                "schedule": {"type": "string", "description": "When it runs on its own: `every 30 minutes`, `every 2 hours`, `every day`, `every morning`, or a 5-field cron expression (UTC). Omit when a person starts every run."},
                "standing_message": {"type": "string", "description": "What it is told each time it runs on its own. Required with schedule."},
                "max_tokens_per_run": {"type": "integer", "description": "The most tokens one run may spend; the run stops at the step that crosses it. Omit for no bound."},
                "max_cost_usd_per_run": {"type": "number", "description": "The most one run may cost in USD, judged on the model's journaled cost; only a priced model can trip it. Omit for no bound."},
                "platform_tools": {"type": "boolean", "description": "Allow platform tools on this agent (rare)."},
                "allow_duplicate": {"type": "boolean", "description": "Create even though an agent with this name exists (rare)."}
            },
            "required": ["name", "description", "charter", "tools"]
        })
    }
    fn effect(&self) -> Effect {
        Effect::Compensatable
    }
    async fn call(&self, args: Value) -> Result<Value> {
        if let Some(world) = rehearsal_world() {
            return Ok(rehearsal_note("agent", &world));
        }
        let state = self.0.open()?;
        let name = text(&args, "name");
        let description = text(&args, "description");
        let charter = text(&args, "charter");
        if name.is_empty() || charter.is_empty() {
            return Err(tool_err("an agent needs a name and a charter"));
        }
        // A tool line may carry the builder's note after ` — `; the name is
        // what is checked and allow-listed, the note is kept on the agent.
        let (tools, notes) = split_tool_lines(&string_list(&args, "tools"));
        let skills = string_list(&args, "skills");
        let allow_platform = args
            .get("platform_tools")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let allow_duplicate = args
            .get("allow_duplicate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Its loop is part of the agent: a schedule named here is attached
        // right after creation, before anything else can go wrong.
        let schedule = text(&args, "schedule");
        let standing = text(&args, "standing_message");
        let budget = rusty_agent_runtime::meter::RunBudget {
            max_tokens: args
                .get("max_tokens_per_run")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0),
            max_cost_usd: args
                .get("max_cost_usd_per_run")
                .and_then(Value::as_f64)
                .filter(|c| c.is_finite() && *c > 0.0),
        };
        let cadence = if schedule.is_empty() || schedule.eq_ignore_ascii_case("none") {
            None
        } else {
            let parsed = cadence_of(&schedule).ok_or_else(|| tool_err(format!(
                "`{schedule}` is not a cadence — say `every 30 minutes`, `every 2 hours`, `every day`, `every morning`, or a 5-field cron expression"
            )))?;
            if standing.is_empty() {
                return Err(tool_err(
                    "an agent that runs on its own needs a standing_message — what it is told each time",
                ));
            }
            Some(parsed)
        };

        // One agent per name unless told otherwise: a second request in the
        // same conversation is a second agent, not the first one again.
        if !allow_duplicate {
            let tenant = context_now();
            let existing = state
                .server_store
                .list_assistants()
                .await
                .map_err(tool_err)?;
            if let Some(same) = existing.iter().find(|a| a.name.eq_ignore_ascii_case(&name)) {
                let id = tenant
                    .unscope(&same.assistant_id)
                    .unwrap_or(&same.assistant_id)
                    .to_owned();
                return Err(tool_err(format!(
                    "an agent named `{name}` already exists ({id}). If this is a different agent, give it a different name; if you mean to build a second copy, pass allow_duplicate: true. Do not rebuild what already exists."
                )));
            }
        }

        let offered: Vec<String> = state
            .registry
            .tool_capabilities(&self.0.graph)
            .into_iter()
            .map(|c| c.name)
            .collect();
        let unknown: Vec<&String> = tools.iter().filter(|t| !offered.contains(t)).collect();
        if !unknown.is_empty() {
            return Err(tool_err(format!(
                "these tools do not exist on the platform: {} — call catalog.tools and use exact names, or say what a person must connect first",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let platform: Vec<&String> = tools.iter().filter(|t| is_platform_tool(t)).collect();
        if !platform.is_empty() && !allow_platform {
            return Err(tool_err(format!(
                "platform tools are the Composer's, not an agent's: {} — leave them out, or pass platform_tools: true if the request really needs them",
                platform
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let known_skills = state.skills.list(&tenant_now()).await;
        let missing: Vec<&String> = skills
            .iter()
            .filter(|s| !known_skills.iter().any(|k| &k.name == *s))
            .collect();
        if !missing.is_empty() {
            return Err(tool_err(format!(
                "these skills do not exist: {} — register them with skills.register first, or use names from catalog.skills",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        // Every table the charter names is called once through the
        // connection before the agent exists: a table the system rejects is
        // an agent that fails every run, and the answer says where to look.
        let capabilities = state.registry.tool_capabilities(&self.0.graph);
        let rejected = tables_rejected(&state, &capabilities, &tools, &charter).await;
        if !rejected.is_empty() {
            let lines: Vec<String> = rejected
                .iter()
                .map(|(tool, table, why)| format!("`{table}` via {tool}: {why}"))
                .collect();
            return Ok(json!({
                "created": false,
                "name": name,
                "tables_rejected": rejected.iter().map(|(tool, table, why)| json!({"tool": tool, "table": table, "reason": why})).collect::<Vec<_>>(),
                "note": format!(
                    "not created: the charter names tables the system rejects — {}. Find the real ones with connectors.probe: servicenow.list-records with table sys_db_object, sysparm_query nameLIKE<word> (a word from what the request asks for), sysparm_fields name,label, sysparm_limit 10; then probe the table you choose with sysparm_limit 2. Name those tables and create again.",
                    lines.join("; ")
                ),
            }));
        }

        let assistant_id = uuid::Uuid::new_v4().to_string();
        let mut sorted = tools.clone();
        sorted.sort();
        sorted.dedup();
        let mut config = json!({
            "studio_intent": {
                "instructions": charter,
                "tools": sorted
                    .iter()
                    .map(|t| match notes.get(t) {
                        Some(note) => json!({"name": t, "when": note}),
                        None => json!({"name": t}),
                    })
                    .collect::<Vec<_>>(),
                "skills": skills,
            }
        });
        if !budget.is_empty() {
            config["studio_intent"]["budget"] = serde_json::to_value(budget).unwrap_or(Value::Null);
        }
        // Who built it. An agent that spawns another — one maintainer per
        // article — is recorded as the builder, with the run as the evidence;
        // the Composer only owns what the Composer itself made.
        let run = rusty_agent_runtime::tool::current_run();
        let builder = run.as_ref().and_then(|r| r.agent_id.clone());
        let created_by = match &builder {
            Some(id) => {
                let external = tenant_now();
                let name = find_assistant(&state, &context_now(), id)
                    .await
                    .ok()
                    .flatten()
                    .map(|a| a.name)
                    .unwrap_or_else(|| id.clone());
                let _ = external;
                json!({"principal_id": id, "name": name, "kind": "agent"})
            }
            None => json!({"principal_id": "composer", "name": COMPOSER_NAME, "kind": "service"}),
        };
        let mut metadata = json!({
            "description": description,
            "created_by": created_by,
        });
        if let (Some(id), Some(run_id)) = (&builder, run.as_ref().map(|r| r.run_id.clone())) {
            metadata["spawned_by"] = json!({"assistant_id": id, "run_id": run_id});
            // An agent that spawns an agent does not hand it a loop of its
            // own, and cannot put it to work either: a spawned agent answers
            // no one until a person publishes it — that is the review — and
            // runs unattended only once a person gives it a schedule. Otherwise a
            // loop running on its own can leave live, self-firing agents
            // behind it, which is the one mistake no one can undo.
            metadata["awaiting_review"] = json!({
                "why": "spawned by an agent, not by a person",
                "means": "it answers no one, its maker included, until a person publishes it; publishing is the review, and a person gives it a schedule before it runs on its own",
            });
        }
        let record = crate::assistants::AssistantRecord::new(
            scope_id(&tenant_now(), &assistant_id),
            name.clone(),
            self.0.graph.clone(),
            config,
            metadata,
            Utc::now(),
        );
        let created = state
            .server_store
            .create_assistant(&record)
            .await
            .map_err(tool_err)?;
        if !created {
            return Err(tool_err("the agent id collided; call again"));
        }
        // The schedule is the agent's loop, attributed to the Composer's
        // service principal the way the agent itself is. An agent-spawned
        // agent gets none: see `awaiting_review` above.
        let cadence = if builder.is_some() { None } else { cadence };
        let schedule_words = match cadence {
            Some((interval_secs, cron_expr)) => {
                let cron = crate::crons::CronRecord {
                    cron_id: scope_id(&tenant_now(), &uuid::Uuid::new_v4().to_string()),
                    graph: self.0.graph.clone(),
                    interval_secs,
                    cron_expr: cron_expr.clone(),
                    input: Some(json!({"messages": [{"role": "user", "content": standing}]})),
                    assistant_id: Some(assistant_id.clone()),
                    created_by: Some(
                        json!({"principal_id": "composer", "name": COMPOSER_NAME, "kind": "service"}),
                    ),
                    metadata: Value::Null,
                    on_run_completed: Default::default(),
                    created_at: Utc::now(),
                    last_run_at: None,
                    runs_fired: 0,
                    held: None,
                    stalled: None,
                    max_runs: None,
                    max_tokens: None,
                    tokens_spent: 0,
                    worlds: Vec::new(),
                    world_names: Vec::new(),
                    world: None,
                    world_name: None,
                };
                state
                    .server_store
                    .create_cron(&cron)
                    .await
                    .map_err(tool_err)?;
                Some(cadence_words(interval_secs, cron_expr.as_deref()))
            }
            None => None,
        };
        let write_tools: Vec<String> = state
            .registry
            .tool_capabilities(&self.0.graph)
            .into_iter()
            .filter(|c| sorted.contains(&c.name) && !c.effect.is_freely_repeatable())
            .map(|c| c.name)
            .collect();
        // A skill whose procedure names tools the agent lacks makes the agent
        // stop where the tool should be; say so, in the receipt, at creation.
        let mut skill_warnings: Vec<String> = Vec::new();
        for skill in &skills {
            if let Some(version) = state.skills.get(&tenant_now(), skill).await {
                let metadata = version.metadata();
                let missing: Vec<&String> = metadata
                    .allowed_tools
                    .iter()
                    .filter(|t| !sorted.contains(t))
                    .collect();
                if !missing.is_empty() {
                    skill_warnings.push(format!(
                        "skill `{skill}` assumes tools this agent does not have ({}); it will follow the procedure and stop where the tool should be — give it those tools, or leave the skill out",
                        missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
        }
        // A tool that reads whatever table it is told, given to a charter that
        // names none, makes an agent that guesses; say so in the receipt.
        let charter_warnings = charter_warnings(
            &state.registry.tool_capabilities(&self.0.graph),
            &sorted,
            &charter,
        );
        Ok(json!({
            "assistant_id": assistant_id,
            "name": name,
            "tools": sorted,
            "skills": skills,
            "skill_warnings": skill_warnings,
            "charter_warnings": charter_warnings,
            "write_tools": write_tools,
            "schedule": schedule_words,
            "open": format!("/playground?assistant={assistant_id}"),
            "edit": format!("/agents/{assistant_id}/edit"),
        }))
    }
}

// ---- assignment.progress --------------------------------------------------

/// The agent's own record of a delegated assignment: what is done, what is
/// unresolved, the next step — or what it waits for, or that it is done.
struct AssignmentProgress(Doors);

#[async_trait]
impl Tool for AssignmentProgress {
    fn name(&self) -> &str {
        ASSIGNMENT_PROGRESS
    }
    fn description(&self) -> &str {
        "Record your progress on the assignment you are working (the ASSIGNMENT block names it). Say what is now done, what is unresolved, and the next step; set waiting_for when you need a person (a decision, an answer) and complete=true when the request is fully achieved. Call it before you finish a round — the owner reads this, and your next round starts from it."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["assignment_id", "done", "unresolved"],
            "properties": {
                "assignment_id": {"type": "string", "description": "The assignment id from the ASSIGNMENT block."},
                "done": {"type": "array", "items": {"type": "string"}, "description": "What is achieved so far, with the numbers or identifiers that show it."},
                "unresolved": {"type": "array", "items": {"type": "string"}, "description": "Open questions and what would settle them."},
                "next_step": {"type": "string", "description": "The next useful action, concrete enough to start from cold."},
                "waiting_for": {"type": "string", "description": "What you need from a person before you can continue, when you do."},
                "complete": {"type": "boolean", "description": "true when the request is fully achieved."}
            }
        })
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    fn idempotency_key(&self, args: &Value) -> Option<String> {
        Some(format!("{ASSIGNMENT_PROGRESS}:{}", args))
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let assignment_id = text(&args, "assignment_id");
        if assignment_id.is_empty() {
            return Err(tool_err(
                "assignment_id is required — the ASSIGNMENT block names it",
            ));
        }
        let list = |key: &str| -> Vec<String> {
            args.get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        let progress = crate::assignments::Progress {
            done: list("done"),
            unresolved: list("unresolved"),
            next_step: Some(text(&args, "next_step")).filter(|s| !s.is_empty()),
            waiting_for: Some(text(&args, "waiting_for")).filter(|s| !s.is_empty()),
            complete: args
                .get("complete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            recorded_at: None,
            by_run: None,
        };
        let run = rusty_agent_runtime::tool::current_run();
        crate::assignments::record_progress(
            &state,
            &tenant_now(),
            run.as_ref().map(|r| r.run_id.as_str()),
            &assignment_id,
            progress,
        )
        .await
        .map_err(tool_err)
    }
}

// ---- artifacts.write ------------------------------------------------------

/// The door an agent has to produce something that outlives its reply: a
/// named, versioned artifact with the run as its lineage.
pub const ARTIFACTS_WRITE: &str = "artifacts.write";

struct ArtifactsWrite(Doors);

#[async_trait]
impl Tool for ArtifactsWrite {
    fn name(&self) -> &str {
        ARTIFACTS_WRITE
    }
    fn description(&self) -> &str {
        "Write a document you produced — a brief, a report, a summary — as a named artifact of this run. The name is what it is filed under; writing the same name again adds a version, so an adaptation for another audience is a new name (report-for-engineering) and a correction is a new version. Answers the artifact's address, name and version; say them in your reply. The text is kept exactly as you wrote it."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "text"],
            "properties": {
                "name": {"type": "string", "description": "What it is filed under: letters, digits, `-` and `_`, e.g. q3-laptop-spend."},
                "text": {"type": "string", "description": "The document, complete, as it should be read — Markdown unless media_type says otherwise."},
                "media_type": {"type": "string", "description": "text/markdown unless said otherwise."}
            }
        })
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    fn idempotency_key(&self, args: &Value) -> Option<String> {
        Some(format!(
            "{ARTIFACTS_WRITE}:{}:{}",
            text(args, "name"),
            rusty_agent_runtime::record::sha256_hex(text(args, "text").as_bytes())
        ))
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let name = text(&args, "name");
        let body = text(&args, "text");
        if name.is_empty() || body.trim().is_empty() {
            return Err(tool_err(
                "name and text are both required: what to file it under, and the document itself",
            ));
        }
        if name.len() > 80
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(tool_err(
                "the name is letters, digits, `-`, `_` or `.` — up to 80 of them",
            ));
        }
        let media_type = Some(text(&args, "media_type"))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "text/markdown".to_owned());
        let run = rusty_agent_runtime::tool::current_run()
            .ok_or_else(|| tool_err("artifacts.write runs inside a run — there is none here"))?;
        let journal = run.journal.as_ref().ok_or_else(|| tool_err("this run keeps no journal, so an artifact would have no lineage; nothing was written"))?;
        let tenant = crate::auth::TenantContext::new(tenant_now(), Vec::new());
        let mut answer = crate::artifacts::commit_from_live_run(
            &state,
            &tenant,
            journal,
            &name,
            body.as_bytes(),
            &media_type,
        )
        .await
        .map_err(tool_err)?;
        answer["text_head"] = json!(body.chars().take(240).collect::<String>());
        Ok(answer)
    }
}

// ---- artifacts.read -------------------------------------------------------

/// The door back into what a run filed: an artifact by name — its newest
/// version, or one by index — or by address, with its text and lineage.
pub const ARTIFACTS_READ: &str = "artifacts.read";

/// The most text one read answers with; a longer artifact says so.
const ARTIFACT_READ_CEILING: usize = 64 * 1024;

struct ArtifactsRead(Doors);

#[async_trait]
impl Tool for ArtifactsRead {
    fn name(&self) -> &str {
        ARTIFACTS_READ
    }
    fn description(&self) -> &str {
        "Read an artifact a run filed — a brief, a report, a summary — by its name (the newest version, or an earlier one by number) or by its address. Answers the text exactly as it was written, with which run produced it. Read the artifact you are asked to build on before writing anything from it; never restate figures from memory when the artifact is there to read."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The artifact's name, e.g. q3-laptop-spend."},
                "version": {"type": "integer", "description": "A version by number, 0 first; the newest when unsaid."},
                "artifact_id": {"type": "string", "description": "An address (64 hex characters), instead of a name."}
            }
        })
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let tenant = tenant_now();
        let name = text(&args, "name");
        let by_id = text(&args, "artifact_id");
        if name.is_empty() && by_id.is_empty() {
            return Err(tool_err("say which artifact: a name, or an address"));
        }
        let record = if !name.is_empty() {
            state
                .server_store
                .get_run_artifact_by_name(&tenant, &name)
                .await
                .map_err(tool_err)?
                .ok_or_else(|| tool_err(format!("no artifact is filed under `{name}`")))?
        } else {
            state
                .server_store
                .get_run_artifact(&tenant, &by_id)
                .await
                .map_err(tool_err)?
                .ok_or_else(|| tool_err(format!("no artifact at `{by_id}`")))?
        };
        let of = record.versions.len().max(1);
        let wanted = args
            .get("version")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let (version, sha256) = match wanted {
            Some(v) => {
                let entry = record.versions.get(v).ok_or_else(|| {
                    tool_err(format!(
                        "`{}` has versions 0 to {}; there is no version {v}",
                        record.name.as_deref().unwrap_or(&record.artifact_id),
                        of.saturating_sub(1)
                    ))
                })?;
                (v, entry.sha256.clone())
            }
            None => (
                of.saturating_sub(1),
                record
                    .versions
                    .last()
                    .map(|v| v.sha256.clone())
                    .unwrap_or_else(|| record.artifact_id.clone()),
            ),
        };
        let bytes = state
            .server_store
            .get_run_artifact_bytes(&sha256)
            .await
            .map_err(tool_err)?;
        let truncated = bytes.len() > ARTIFACT_READ_CEILING;
        let shown = if truncated {
            &bytes[..ARTIFACT_READ_CEILING]
        } else {
            &bytes[..]
        };
        let text = String::from_utf8_lossy(shown).into_owned();
        Ok(json!({
            "name": record.name,
            "artifact_id": sha256,
            "version": version,
            "of": of,
            "bytes": bytes.len(),
            "media_type": record.media_type,
            "run_id": record.lineage.run_id,
            "truncated": truncated,
            "text": text,
        }))
    }
}

// ---- skills.read ----------------------------------------------------------

struct SkillsRead(Doors);

#[async_trait]
impl Tool for SkillsRead {
    fn name(&self) -> &str {
        SKILLS_READ
    }
    fn description(&self) -> &str {
        "Skills are reusable procedures. Without a name, this lists every skill on the platform: name and when to use it. With a name, it returns that skill's full procedure and the names of its reference files; with a name and a reference, that reference's text — what the skill learned from the system it follows (a table's fields, how issues were resolved, what the catalog offers). Read a skill before following it; read its reference before answering from it."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "A skill's name, for its full procedure. Omit to list the catalog."},
                "reference": {"type": "string", "description": "One of the skill's reference files, by the path skills.read listed, for its text."}
            }
        })
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let name = text(&args, "name");
        if name.is_empty() {
            let skills: Vec<Value> = state
                .skills
                .list(&tenant_now())
                .await
                .into_iter()
                .map(|s| json!({"name": s.name, "when": brief(&s.description, 140)}))
                .collect();
            return Ok(
                json!({ "count": skills.len(), "skills": skills, "next": "call skills.read with a name for its full procedure" }),
            );
        }
        let Some(version) = state.skills.resolve(&tenant_now(), &name).await else {
            return Err(tool_err(format!(
                "no skill named `{name}` — call skills.read without a name to list them"
            )));
        };
        let wanted = text(&args, "reference");
        if !wanted.is_empty() {
            let path = wanted.trim_start_matches('/');
            let Some(bytes) = version
                .reference(path)
                .or_else(|| version.reference(&format!("references/{path}")))
            else {
                return Ok(
                    json!({"name": version.name(), "reference": path, "found": false, "references": version.reference_paths().collect::<Vec<_>>(), "note": "no reference by that path; the list names them"}),
                );
            };
            // A reference the nightly check found stale still reads, with
            // what moved under it said first: the agent decides knowing.
            let (full, stale) = crate::freshness::as_read(
                &state,
                &tenant_now(),
                version.name(),
                String::from_utf8_lossy(bytes).into_owned(),
            )
            .await;
            let shown: String = full.chars().take(12_000).collect();
            let truncated = shown.len() < full.len();
            return Ok(json!({
                "name": version.name(),
                "reference": path,
                "found": true,
                "text": shown,
                "truncated": truncated,
                "stale": stale,
            }));
        }
        // The agents whose intent names this skill: whose runs a
        // Consolidator reads, whose suites judge a revision.
        let followers: Vec<Value> = match state.server_store.list_assistants().await {
            Ok(views) => {
                let mut out = Vec::new();
                for view in views {
                    if let Ok(Some(record)) =
                        state.server_store.get_assistant(&view.assistant_id).await
                    {
                        if record.archived_at.is_none()
                            && crate::routes::assistant_skills(&record.config)
                                .iter()
                                .any(|s| s == version.name())
                        {
                            out.push(json!({"assistant_id": crate::auth::strip_owned(&tenant_now(), &view.assistant_id).unwrap_or(view.assistant_id.as_str()), "name": record.name}));
                        }
                    }
                }
                out
            }
            Err(_) => Vec::new(),
        };
        Ok(json!({
            "name": version.name(),
            "revision": version.revision(),
            "description": version.metadata().description,
            "allowed_tools": version.metadata().allowed_tools,
            "procedure": version.body(),
            "followers": followers,
            "references": version.reference_paths().collect::<Vec<_>>(),
            "next": if version.reference_paths().next().is_some() { "call skills.read with the name and a reference for what the skill learned" } else { "" },
        }))
    }
}

// ---- skills.register ------------------------------------------------------

struct SkillsRegister(Doors);

#[async_trait]
impl Tool for SkillsRegister {
    fn name(&self) -> &str {
        "skills.register"
    }
    fn description(&self) -> &str {
        "Register a reusable procedure as a skill: a kebab-case name, one line saying when an agent should use it, the procedure in Markdown (when to use, the method step by step, what done looks like), and the tool names it assumes. Content-addressed: the same text twice is the same revision. An agent follows it when named in agents.create."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "kebab-case, e.g. triage-and-route"},
                "description": {"type": "string", "description": "When an agent should reach for it, one line."},
                "procedure": {"type": "string", "description": "Markdown: ## When to use, ## Method (numbered), ## Done when."},
                "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool names the procedure calls, from catalog.tools."},
                "license": {"type": "string"}
            },
            "required": ["name", "description", "procedure"]
        })
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> Result<Value> {
        if let Some(world) = rehearsal_world() {
            return Ok(rehearsal_note("skill", &world));
        }
        let state = self.0.open()?;
        let name = text(&args, "name");
        let description = text(&args, "description");
        let procedure = text(&args, "procedure");
        let allowed = string_list(&args, "allowed_tools");
        let license = text(&args, "license");
        let mut front = vec![
            "---".to_owned(),
            format!("name: {name}"),
            format!("description: {}", description.replace('\n', " ")),
        ];
        if !license.is_empty() {
            front.push(format!("license: {license}"));
        }
        if !allowed.is_empty() {
            front.push(format!("allowed-tools: {}", allowed.join(", ")));
        }
        front.push("---".to_owned());
        let skill_md = format!("{}\n\n{}\n", front.join("\n"), procedure);
        let package =
            SkillPackage::from_markdown(&skill_md).map_err(|e| tool_err(e.to_string()))?;
        let registration = state
            .skills
            .register(
                &tenant_now(),
                package,
                SkillSource::Registry {
                    name: "composer".to_owned(),
                },
                COMPOSER_NAME.to_owned(),
            )
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        // The gate applies to an agent's registration as to a person's: a
        // revision of a followed skill is held until the suites pass.
        let (gate, held, current) = if registration.already_registered {
            (Vec::new(), false, registration.version.revision())
        } else {
            crate::skills::after_registration(
                &state,
                &context_now(),
                registration.version.name(),
                registration.version.revision(),
            )
            .await
        };
        Ok(json!({
            "name": registration.version.name(),
            "revision": registration.version.revision(),
            "already_registered": registration.already_registered,
            "held": held,
            "current": current,
            "gate": gate,
        }))
    }
}

// ---- skills.revise --------------------------------------------------------

/// The Consolidator's door: one change to a skill's procedure, filed as a
/// new revision that enters the gate.
struct SkillsRevise(Doors);

#[async_trait]
impl Tool for SkillsRevise {
    fn name(&self) -> &str {
        SKILLS_REVISE
    }
    fn description(&self) -> &str {
        "File a revision of a skill's procedure from what its followers' verified runs did: one step to add first (add_step), sentences to replace (replace), or the whole procedure — with why. The revision is registered under the Consolidator's name and held at the gate until every follower's suite passes; a person promotes it. Refuses a procedure identical to the current one, a `replace.from` that is not in it, and a change without a why."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "required": ["skill", "why"], "properties": {
            "skill": {"type": "string", "description": "The skill's name."},
            "add_step": {"type": "string", "description": "One step to put first in the method, written as the agent should run it — the tool, its arguments' shape, what to do with the result. Concrete words, never placeholders."},
            "replace": {"type": "array", "items": {"type": "object", "required": ["from", "to"], "properties": {"from": {"type": "string"}, "to": {"type": "string"}}}, "description": "Sentences to reword: `from` exactly as the procedure has it, `to` the new sentence."},
            "procedure": {"type": "string", "description": "The complete revised procedure in Markdown, only when many parts change."},
            "why": {"type": "string", "description": "The trajectory the verified runs share that the procedure did not state, or the sentence the failed runs misread — one or two sentences."}
        }})
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let state = self.0.open()?;
        let skill = text(&args, "skill");
        let why = text(&args, "why");
        if why.trim().is_empty() {
            return Ok(
                json!({"filed": false, "note": "say why: the trajectory the verified runs share that the procedure leaves out, or the sentence the failed runs misread"}),
            );
        }
        let tenant = tenant_now();
        let Some(current) = state.skills.resolve(&tenant, &skill).await else {
            return Ok(
                json!({"filed": false, "skill": skill, "note": "no skill by that name — skills.read without a name lists them"}),
            );
        };
        let before = current.body().to_owned();
        let mut body = before.clone();
        let whole = text(&args, "procedure");
        if !whole.trim().is_empty() {
            body = whole.trim().to_owned();
        }
        if let Some(list) = args.get("replace").and_then(Value::as_array) {
            for pair in list {
                let from = pair.get("from").and_then(Value::as_str).unwrap_or("");
                let to = pair.get("to").and_then(Value::as_str).unwrap_or("");
                if from.trim().is_empty() {
                    continue;
                }
                if !body.contains(from) {
                    return Ok(
                        json!({"filed": false, "note": format!("`replace.from` is not in the procedure exactly: {from:?}; copy the sentence as it stands")}),
                    );
                }
                body = body.replacen(from, to, 1);
            }
        }
        let step = text(&args, "add_step");
        if !step.trim().is_empty() {
            // First in the method: after the title line when the body opens
            // with one, else at the top.
            let step = step.trim();
            let mut lines: Vec<&str> = body.lines().collect();
            let at = if lines.first().is_some_and(|l| l.starts_with('#')) {
                1
            } else {
                0
            };
            let inserted = format!("\n**First:** {step}\n");
            lines.insert(at, &inserted);
            body = lines.join("\n");
        }
        let body = body.trim().to_owned();
        if body == before.trim() {
            return Ok(
                json!({"filed": false, "skill": skill, "revision": current.revision(), "note": "the procedure is unchanged: nothing to file. Give add_step, replace, or the whole procedure."}),
            );
        }
        // The package: the new SKILL.md with the frontmatter as it stands,
        // and every reference and asset the current revision carries.
        let meta = current.metadata();
        let mut front = vec![
            "---".to_owned(),
            format!("name: {}", current.name()),
            format!("description: {}", meta.description.replace('\n', " ")),
        ];
        if let Some(license) = &meta.license {
            front.push(format!("license: {license}"));
        }
        if !meta.allowed_tools.is_empty() {
            front.push(format!("allowed-tools: {}", meta.allowed_tools.join(", ")));
        }
        front.push("---".to_owned());
        let skill_md = format!("{}\n\n{}\n", front.join("\n"), body);
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        files.insert("SKILL.md".to_owned(), skill_md.into_bytes());
        for path in current.reference_paths() {
            if let Some(bytes) = current.reference(path) {
                files.insert(
                    format!("references/{}", path.trim_start_matches("references/")),
                    bytes.to_vec(),
                );
            }
        }
        for path in current.asset_paths() {
            if let Some(bytes) = current.asset(path) {
                files.insert(
                    format!("assets/{}", path.trim_start_matches("assets/")),
                    bytes.to_vec(),
                );
            }
        }
        let package = SkillPackage::from_files(files).map_err(|e| tool_err(e.to_string()))?;
        let registration = state
            .skills
            .register(
                &tenant,
                package,
                SkillSource::Registry {
                    name: "consolidator".to_owned(),
                },
                CONSOLIDATOR_NAME.to_owned(),
            )
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        if registration.already_registered {
            return Ok(
                json!({"filed": false, "skill": skill, "revision": registration.version.revision(), "note": "this exact procedure is already a revision; nothing new was filed"}),
            );
        }
        let (gate, held, current_now) = crate::skills::after_registration(
            &state,
            &context_now(),
            registration.version.name(),
            registration.version.revision(),
        )
        .await;
        tracing::info!(skill = %skill, revision = registration.version.revision(), held, %why, "consolidator filed a skill revision");
        Ok(json!({
            "filed": true,
            "skill": registration.version.name(),
            "revision": registration.version.revision(),
            "held": held,
            "current": current_now,
            "gate": gate,
            "why": why,
            "next": if held { "held at the gate: the followers' suites run against it; a person promotes it in Catalog → Skills" } else { "no follower has a suite, so it is current now; say so" },
        }))
    }
}

// ---- connectors.register --------------------------------------------------

struct ConnectorsRegister(Doors);

#[async_trait]
impl Tool for ConnectorsRegister {
    fn name(&self) -> &str {
        "connectors.register"
    }
    fn description(&self) -> &str {
        "Describe a system by hand — its API root, how it authenticates, and each operation as method, path, parameters and effect — and register it as a connector in the library. Its operations become tools named `id.operation` only after a person configures a connection with credentials in Catalog → Connectors; say that in your reply. Use only for APIs you know well; never guess paths."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "description": {"type": "string"},
                "base_url": {"type": "string", "description": "https://… the API root"},
                "documentation_url": {"type": "string"},
                "auth": {"type": "string", "enum": ["bearer", "basic", "header", "query", "none"]},
                "auth_name": {"type": "string", "description": "the header or query parameter name, for those styles"},
                "check_path": {"type": "string", "description": "a parameterless GET that proves the credentials, e.g. /me"},
                "operations": {"type": "array", "items": {"type": "object", "properties": {
                    "name": {"type": "string"}, "description": {"type": "string"},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"]},
                    "path": {"type": "string", "description": "/path/{param}"},
                    "effect": {"type": "string", "enum": ["read_only", "idempotent", "compensatable", "irreversible"]},
                    "params": {"type": "array", "items": {"type": "object", "properties": {
                        "name": {"type": "string"}, "type": {"type": "string", "enum": ["string", "integer", "number", "boolean"]},
                        "required": {"type": "boolean"}, "description": {"type": "string"}}, "required": ["name"]}}
                }, "required": ["name", "description", "method", "path", "effect"]}}
            },
            "required": ["name", "base_url", "documentation_url", "auth", "operations"]
        })
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> Result<Value> {
        if let Some(world) = rehearsal_world() {
            return Ok(rehearsal_note("connector", &world));
        }
        let state = self.0.open()?;
        let draft: crate::connector_draft::ConnectorDraft = serde_json::from_value(args)
            .map_err(|e| tool_err(format!("the description does not parse: {e}")))?;
        let (manifest, registered) =
            crate::connector_draft::register_described(&state, &tenant_now(), &draft)
                .await
                .map_err(tool_err)?;
        let tools: Vec<String> = manifest
            .operations
            .iter()
            .filter(|op| op.name != manifest.check)
            .map(|op| format!("{}.{}", manifest.id, op.name))
            .collect();
        Ok(json!({
            "id": manifest.id,
            "registered": registered,
            "tools_once_connected": tools,
            "next": "a person configures a connection to it in Catalog → Connectors (credentials are theirs); then these tools exist and can be given to an agent",
        }))
    }
}

// ---- the Composer itself --------------------------------------------------

/// Seed the Composer if no assistant by that name exists; when one does and
/// it is still the platform's, re-version it onto the pinned charter.
pub(crate) async fn seed_composer(state: &AppState) {
    let existing = match state.server_store.list_assistants().await {
        Ok(list) => list,
        Err(error) => {
            tracing::warn!(%error, "composer: could not list assistants");
            return;
        }
    };
    let charter_config = json!({"studio_intent": {
        "instructions": COMPOSER_CHARTER,
        "tools": PLATFORM_TOOLS.iter().chain([SKILLS_READ].iter()).map(|t| json!({"name": t})).collect::<Vec<_>>(),
    }});
    let rusty = json!({
        "description": "Builds agents from what the platform has, and says what it would need.",
        "created_by": {"principal_id": "rusty", "name": "Rusty", "kind": "service"},
    });
    if let Some(existing) = existing.iter().find(|a| a.name == COMPOSER_NAME) {
        // The platform maintains its own agent's charter — as long as it is
        // still the platform's. A person who edited the Composer owns it
        // from then on, and the pinned text is only a suggestion.
        let rusty_owned = existing
            .metadata
            .pointer("/created_by/principal_id")
            .and_then(Value::as_str)
            == Some("rusty");
        if rusty_owned && existing.config != charter_config {
            let version = crate::assistants::AssistantVersionRecord::new(
                Some(existing.active_version_id.clone()),
                COMPOSER_NAME.to_owned(),
                state.config.agent_graph.clone(),
                charter_config,
                rusty,
                Utc::now(),
            );
            let version_id = version.version_id.clone();
            match state
                .server_store
                .create_assistant_version(
                    &existing.assistant_id,
                    &existing.active_version_id,
                    &version,
                )
                .await
            {
                Ok(outcome) => {
                    tracing::info!(?outcome, "composer: charter re-versioned");
                    match state
                        .server_store
                        .activate_assistant_version(
                            &existing.assistant_id,
                            &version_id,
                            &existing.active_version_id,
                        )
                        .await
                    {
                        Ok(outcome) => tracing::info!(?outcome, "composer: new charter active"),
                        Err(error) => tracing::warn!(%error, "composer: new charter not activated"),
                    }
                }
                Err(error) => tracing::warn!(%error, "composer: charter not re-versioned"),
            }
        }
        return;
    }
    let assistant_id = uuid::Uuid::new_v4().to_string();
    let record = crate::assistants::AssistantRecord::new(
        scope_id(&tenant_now(), &assistant_id),
        COMPOSER_NAME.to_owned(),
        state.config.agent_graph.clone(),
        charter_config,
        rusty,
        Utc::now(),
    );
    match state.server_store.create_assistant(&record).await {
        Ok(true) => tracing::info!(%assistant_id, "composer seeded"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "composer: not seeded"),
    }
}

pub(crate) async fn seed_coach(state: &AppState) {
    let existing = match state.server_store.list_assistants().await {
        Ok(list) => list,
        Err(error) => {
            tracing::warn!(%error, "coach: could not list assistants");
            return;
        }
    };
    let charter_config = json!({"studio_intent": {
        "instructions": COACH_CHARTER,
        "tools": COACH_TOOLS.iter().map(|t| json!({"name": t})).collect::<Vec<_>>(),
    }});
    let rusty = json!({
        "description": "Reads what an agent's runs say and files a better charter for a person to activate.",
        "created_by": {"principal_id": "rusty", "name": "Rusty", "kind": "service"},
    });
    if let Some(existing) = existing.iter().find(|a| a.name == COACH_NAME) {
        // The platform maintains its own agent's charter — as long as it is
        // still the platform's. A person who edited the Composer owns it
        // from then on, and the pinned text is only a suggestion.
        let rusty_owned = existing
            .metadata
            .pointer("/created_by/principal_id")
            .and_then(Value::as_str)
            == Some("rusty");
        if rusty_owned && existing.config != charter_config {
            let version = crate::assistants::AssistantVersionRecord::new(
                Some(existing.active_version_id.clone()),
                COACH_NAME.to_owned(),
                state.config.agent_graph.clone(),
                charter_config,
                rusty,
                Utc::now(),
            );
            let version_id = version.version_id.clone();
            match state
                .server_store
                .create_assistant_version(
                    &existing.assistant_id,
                    &existing.active_version_id,
                    &version,
                )
                .await
            {
                Ok(outcome) => {
                    tracing::info!(?outcome, "coach: charter re-versioned");
                    match state
                        .server_store
                        .activate_assistant_version(
                            &existing.assistant_id,
                            &version_id,
                            &existing.active_version_id,
                        )
                        .await
                    {
                        Ok(outcome) => tracing::info!(?outcome, "coach: new charter active"),
                        Err(error) => tracing::warn!(%error, "coach: new charter not activated"),
                    }
                }
                Err(error) => tracing::warn!(%error, "coach: charter not re-versioned"),
            }
        }
        return;
    }
    let assistant_id = uuid::Uuid::new_v4().to_string();
    let record = crate::assistants::AssistantRecord::new(
        scope_id(&tenant_now(), &assistant_id),
        COACH_NAME.to_owned(),
        state.config.agent_graph.clone(),
        charter_config,
        rusty,
        Utc::now(),
    );
    match state.server_store.create_assistant(&record).await {
        Ok(true) => tracing::info!(%assistant_id, "composer seeded"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "coach: not seeded"),
    }
}

/// The skills section entries for an agent's named skills: tier-1 metadata
/// plus the body, from the latest revision. Unknown names are skipped with
/// a warning — an agent that names a skill the plane lost still runs.
pub(crate) async fn seed_consolidator(state: &AppState) {
    let existing = match state.server_store.list_assistants().await {
        Ok(list) => list,
        Err(error) => {
            tracing::warn!(%error, "consolidator: could not list assistants");
            return;
        }
    };
    let charter_config = json!({"studio_intent": {
        "instructions": CONSOLIDATOR_CHARTER,
        "tools": CONSOLIDATOR_TOOLS.iter().map(|t| json!({"name": t})).collect::<Vec<_>>(),
    }});
    let rusty = json!({
        "description": "Reads what a skill's followers did in their verified runs and files a better procedure as a revision the gate holds for a person to promote.",
        "created_by": {"principal_id": "rusty", "name": "Rusty", "kind": "service"},
    });
    if let Some(existing) = existing.iter().find(|a| a.name == CONSOLIDATOR_NAME) {
        let rusty_owned = existing
            .metadata
            .pointer("/created_by/principal_id")
            .and_then(Value::as_str)
            == Some("rusty");
        if rusty_owned && existing.config != charter_config {
            let version = crate::assistants::AssistantVersionRecord::new(
                Some(existing.active_version_id.clone()),
                CONSOLIDATOR_NAME.to_owned(),
                state.config.agent_graph.clone(),
                charter_config,
                rusty,
                Utc::now(),
            );
            let version_id = version.version_id.clone();
            match state
                .server_store
                .create_assistant_version(
                    &existing.assistant_id,
                    &existing.active_version_id,
                    &version,
                )
                .await
            {
                Ok(_) => {
                    if let Err(error) = state
                        .server_store
                        .activate_assistant_version(
                            &existing.assistant_id,
                            &version_id,
                            &existing.active_version_id,
                        )
                        .await
                    {
                        tracing::warn!(%error, "consolidator: new charter not activated");
                    } else {
                        tracing::info!("consolidator: charter re-versioned and active");
                    }
                }
                Err(error) => tracing::warn!(%error, "consolidator: charter not re-versioned"),
            }
        }
        return;
    }
    let assistant_id = uuid::Uuid::new_v4().to_string();
    let record = crate::assistants::AssistantRecord::new(
        scope_id(&tenant_now(), &assistant_id),
        CONSOLIDATOR_NAME.to_owned(),
        state.config.agent_graph.clone(),
        charter_config,
        rusty,
        Utc::now(),
    );
    match state.server_store.create_assistant(&record).await {
        Ok(true) => tracing::info!(%assistant_id, "consolidator seeded"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "consolidator: not seeded"),
    }
}

pub(crate) async fn skill_entries(
    state: &AppState,
    tenant: &str,
    names: &[String],
) -> Vec<SkillSectionEntry> {
    skill_entries_pinned(state, tenant, names, &[]).await
}

/// The skills an agent follows as its runs see them: the current revision
/// of each — or the pinned one, when an evaluation judges a candidate
/// revision through this follower.
pub(crate) async fn skill_entries_pinned(
    state: &AppState,
    tenant: &str,
    names: &[String],
    pins: &[(String, u64)],
) -> Vec<SkillSectionEntry> {
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let version = match pins.iter().find(|(n, _)| n == name) {
            Some((_, revision)) => {
                state
                    .skills
                    .get_version(
                        tenant,
                        name,
                        rusty_agent_runtime::skill::SkillVersionSelector::Revision(*revision),
                    )
                    .await
            }
            None => state.skills.resolve(tenant, name).await,
        };
        match version {
            Some(version) => entries.push(SkillSectionEntry {
                name: version.name().to_owned(),
                revision: version.revision().to_string(),
                content_hash: version.content_hash().to_owned(),
                metadata: version.metadata().description,
                body: Some(version.body().to_owned()),
            }),
            None => tracing::warn!(skill = %name, "an agent names a skill the plane does not hold"),
        }
    }
    entries
}

/// The read half of memory as a fact, not a paragraph: by key (`posted:INC0010093`)
/// or by words that must appear. Reads the person's scope when the run is
/// with a person, and the agent's own scope — the same scopes `memory.remember`
/// writes. The check an agent makes before doing something it must not do
/// twice.
struct MemoryRecall(Doors);

#[async_trait]
impl Tool for MemoryRecall {
    fn name(&self) -> &str {
        MEMORY_RECALL
    }
    fn description(&self) -> &str {
        "Ask your memory precise questions and get facts back: by key (e.g. `posted:INC0010093` — found or not) or by words that must appear in the note. Ask about a whole list at once (`contains` or `keys` as an array) and get one answer per item. Reads what is remembered for the person this run is with and for this agent. Use it before doing anything you must never do twice, and remember with the same key afterwards."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {"type": "string", "description": "The exact key a note was remembered under (e.g. `posted:INC0010093`)."},
                "keys": {"type": "array", "items": {"type": "string"}, "description": "Several exact keys at once: one answer each."},
                "contains": {"oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}], "description": "Words that must appear in the note, case-insensitive (e.g. `INC0010093`); a list asks about each item at once."},
                "limit": {"type": "integer", "description": "At most this many notes per question (default 20)."}
            }
        })
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        use rusty_agent_runtime::memory::{MemoryQuery, MemoryScope, ScopeAddress};
        let state = self.0.open()?;
        // The questions: keys and/or words, singly or as lists.
        let mut questions: Vec<(bool, String)> = Vec::new(); // (by_key, needle)
        let mut push_all = |by_key: bool, value: Option<&Value>| match value {
            Some(Value::String(one)) if !one.trim().is_empty() => {
                questions.push((by_key, one.trim().to_owned()))
            }
            Some(Value::Array(many)) => {
                for item in many
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    questions.push((by_key, item.to_owned()));
                }
            }
            _ => {}
        };
        push_all(true, args.get("key"));
        push_all(true, args.get("keys"));
        push_all(false, args.get("contains"));
        if questions.is_empty() {
            return Err(tool_err(
                "ask by `key`/`keys` or by `contains` — an empty question has no fact",
            ));
        }
        if questions.len() > 100 {
            return Err(tool_err("at most 100 questions in one call"));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 100) as usize)
            .unwrap_or(20);
        let run = rusty_agent_runtime::tool::current_run().ok_or_else(|| {
            tool_err("memory.recall only works inside a run — there is nobody to recall for")
        })?;
        let mut scopes = Vec::new();
        if let Some(person) = run.person_id() {
            scopes.push(ScopeAddress::new(MemoryScope::User, person));
        }
        if let Some(agent) = run.agent_id.as_deref() {
            scopes.push(ScopeAddress::new(MemoryScope::Agent, agent));
        }
        if scopes.is_empty() {
            return Err(tool_err(
                "this run has neither a person behind it nor an agent it is of — nothing to recall for",
            ));
        }
        let now = Utc::now();
        // One read per scope; every question is answered over the same notes.
        let mut held: Vec<(String, rusty_agent_runtime::memory::MemoryRecord)> = Vec::new();
        for scope in &scopes {
            let query = MemoryQuery {
                scope: Some(scope.clone()),
                ..Default::default()
            };
            let found = state
                .server_store
                .query_memory(&tenant_now(), &query, now)
                .await
                .map_err(|e| tool_err(e.to_string()))?;
            held.extend(found.into_iter().map(|r| (scope.as_address(), r)));
        }
        let text_of = |record: &rusty_agent_runtime::memory::MemoryRecord| -> String {
            match &record.content {
                rusty_agent_runtime::record::PayloadRef::Inline(v) => v
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                _ => String::new(),
            }
        };
        let mut answers = Vec::new();
        for (by_key, needle) in &questions {
            let lowered = needle.to_lowercase();
            let notes: Vec<Value> = held
                .iter()
                .filter(|(_, r)| {
                    if *by_key {
                        r.key.as_deref().is_some_and(|k| {
                            rusty_agent_runtime::memory_tiers::keys_match(k, needle)
                        })
                    } else {
                        text_of(r).to_lowercase().contains(&lowered)
                    }
                })
                .take(limit)
                .map(|(scope, r)| {
                    json!({
                        "text": text_of(r),
                        "key": r.key,
                        "kind": r.kind,
                        "for": scope,
                        "memory_id": r.memory_id,
                        "remembered_at": r.created_at,
                    })
                })
                .collect();
            answers.push(json!({
                "asked": if *by_key { json!({ "key": needle }) } else { json!({ "contains": needle }) },
                "found": !notes.is_empty(),
                "count": notes.len(),
                "notes": notes,
            }));
        }
        if answers.len() == 1 {
            return Ok(answers.remove(0));
        }
        Ok(json!({
            "found": answers.iter().any(|a| a["found"] == json!(true)),
            "answers": answers,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::brief;

    #[test]
    fn a_brief_is_the_first_sentence_bounded() {
        assert_eq!(
            brief("Reads a file.  Handles   encodings. More.", 80),
            "Reads a file."
        );
        assert_eq!(
            brief("No period at all in this one", 80),
            "No period at all in this one"
        );
        let long = brief(&"x".repeat(200), 50);
        assert!(long.len() <= 54 && long.ends_with('…'));
    }
}

/// The parameters that make a tool read "whatever it is told": a table,
/// an object, a query. A charter that gives such a tool and names no
/// table makes an agent that guesses which.
const WHATEVER_PARAMS: [&str; 5] = ["table", "object", "sobject", "table_name", "entity"];

fn takes_whatever(capability: &rusty_agent_runtime::tool::ToolCapability) -> Option<String> {
    let required: Vec<String> = capability
        .parameters_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    required
        .into_iter()
        .find(|p| WHATEVER_PARAMS.contains(&p.as_str()))
}

/// Whether `charter` names a table for such a tool: the word `table` (or
/// `object`) followed by an identifier, as in `table incident`, `table:
/// sys_db_object`, `on the table ts_query`, `object Opportunity`.
fn names_a_table(charter: &str) -> bool {
    let lower = charter.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("table").or_else(|| rest.find("object")) {
        let after = &rest[at..];
        let word_len = if after.starts_with("table") { 5 } else { 6 };
        let tail = after[word_len..].trim_start_matches(|c: char| {
            c == ':' || c == '=' || c == '`' || c == '"' || c == '\'' || c.is_whitespace()
        });
        let ident: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if ident.len() >= 3
            && ident.chars().any(|c| c.is_ascii_alphabetic())
            && ![
                "the", "and", "for", "from", "with", "that", "name", "names", "each", "this",
                "its", "api", "data",
            ]
            .contains(&ident.as_str())
        {
            return true;
        }
        rest = &rest[at + word_len..];
    }
    false
}

/// Bare words that are tables on a system of record even without an
/// underscore or backticks; anything else must look like one.
const BARE_TABLES: [&str; 8] = [
    "incident",
    "problem",
    "task",
    "interaction",
    "change",
    "request",
    "asset",
    "user",
];

/// Whether a word after `table` is a table's name and not the sentence
/// going on: it carries an underscore (`sys_db_object`, `kb_knowledge`),
/// was written in backticks, or is one of the bare few.
fn looks_like_a_table(ident: &str, quoted: bool) -> bool {
    ident.len() >= 3
        && ident.chars().any(|c| c.is_ascii_alphabetic())
        && (quoted || ident.contains('_') || BARE_TABLES.contains(&ident))
}

/// The tables a charter names: the identifier after `table` (or `object`),
/// each once, in order of appearance — when it looks like a table, not the
/// prose that follows the word ("table when the question…").
fn tables_named(charter: &str) -> Vec<String> {
    let lower = charter.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut rest = lower.as_str();
    loop {
        let (at, word_len) = match (rest.find("table"), rest.find("object")) {
            (Some(t), Some(o)) if o < t => (o, 6),
            (Some(t), _) => (t, 5),
            (None, Some(o)) => (o, 6),
            (None, None) => break,
        };
        let after = &rest[at + word_len..];
        let tail = after.trim_start_matches(|c: char| {
            c == ':' || c == '=' || c == '"' || c == '\'' || c.is_whitespace()
        });
        let quoted = tail.starts_with('`');
        let tail = tail.trim_start_matches('`');
        let ident: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if looks_like_a_table(&ident, quoted) && !out.contains(&ident) {
            out.push(ident);
        }
        rest = &rest[at + word_len..];
    }
    out
}

/// Whether an error from the system means the table is not there, as
/// opposed to a call the check itself got wrong.
fn says_no_such_table(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("invalid table")
        || lower.contains("unknown table")
        || lower.contains("no such table")
        || lower.contains("does not exist")
        || lower.contains("404")
        || lower.contains("not found")
}

/// Call each table-taking tool the agent is given with each table the
/// charter names, and collect the ones the system rejects.
async fn tables_rejected(
    state: &AppState,
    capabilities: &[rusty_agent_runtime::tool::ToolCapability],
    tools: &[String],
    charter: &str,
) -> Vec<(String, String, String)> {
    let Some(cell) = &state.connection_tools else {
        return Vec::new();
    };
    use rusty_agent_runtime::tool::ToolSource as _;
    let mounted = cell.tools();
    let tables = tables_named(charter);
    let mut out = Vec::new();
    for c in capabilities.iter().filter(|c| tools.contains(&c.name)) {
        let Some(param) = takes_whatever(c) else {
            continue;
        };
        let Some(tool) = mounted.iter().find(|t| t.name() == c.name) else {
            continue;
        };
        if !matches!(tool.effect(), Effect::ReadOnly) {
            continue;
        }
        for table in &tables {
            if let Err(error) = tool.call(json!({ &param: table })).await {
                let text = error.to_string();
                if says_no_such_table(&text) {
                    out.push((
                        c.name.clone(),
                        table.clone(),
                        text.chars().take(200).collect(),
                    ));
                }
            }
        }
    }
    out
}

/// The warnings a charter earns at creation for the tools it gives.
fn charter_warnings(
    capabilities: &[rusty_agent_runtime::tool::ToolCapability],
    tools: &[String],
    charter: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    if names_a_table(charter) {
        return out;
    }
    for c in capabilities.iter().filter(|c| tools.contains(&c.name)) {
        if let Some(param) = takes_whatever(c) {
            out.push(format!(
                "`{}` takes a {param} and the charter names none — the agent will guess which. Look into the system with connectors.probe and write `{param} <name>` and the query for each thing it reads.",
                c.name
            ));
        }
    }
    out
}

#[cfg(test)]
mod charter_warning_tests {
    use super::{charter_warnings, names_a_table};
    use rusty_agent_runtime::tool::ToolCapability;
    use serde_json::json;

    fn cap(name: &str, required: &[&str]) -> ToolCapability {
        serde_json::from_value(json!({
            "name": name, "description": "", "effect": "read_only",
            "parameters_schema": {"type": "object", "required": required, "properties": {}}
        }))
        .expect("a capability")
    }

    #[test]
    fn a_table_taking_tool_needs_a_named_table_in_the_charter() {
        let caps = vec![cap("servicenow.list-records", &["table"]), cap("echo", &[])];
        let tools = vec!["servicenow.list-records".to_owned(), "echo".to_owned()];
        let vague = charter_warnings(
            &caps,
            &tools,
            "Use servicenow.list-records to retrieve search history and interaction records.",
        );
        assert_eq!(vague.len(), 1);
        assert!(vague[0].contains("takes a table"));
        assert!(charter_warnings(
            &caps,
            &tools,
            "Call servicenow.list-records on table ts_query with sysparm_query …"
        )
        .is_empty());
        assert!(charter_warnings(
            &caps,
            &tools,
            "Read the table: interaction, then the table `sys_db_object`."
        )
        .is_empty());
        assert!(charter_warnings(&caps, &["echo".to_owned()], "Say hello.").is_empty());
        assert!(!names_a_table("the table data from the API"));
        assert!(names_a_table("object Opportunity"));
    }

    #[test]
    fn the_tables_a_charter_names_are_read_once_each_in_order() {
        use super::tables_named;
        let charter = "Use servicenow.list-records with table incident and sysparm_query active=true. Then table sys_search; then the table `interaction`, and table incident again. Read the table data.";
        assert_eq!(
            tables_named(charter),
            ["incident", "sys_search", "interaction"]
        );
        // Prose after the word is not a table: only names that look like one.
        let prose = "Query table incident with the fields; decide which table when the question fits; search the knowledge table for articles; read table `kb_knowledge`; count on table cmdb_ci by class.";
        assert_eq!(tables_named(prose), ["incident", "kb_knowledge", "cmdb_ci"]);
        assert!(super::says_no_such_table(
            "HTTP 400: {\"error\":{\"message\":\"Invalid table sys_search\"}}"
        ));
        assert!(!super::says_no_such_table(
            "missing required argument sysparm_query"
        ));
    }
}

#[cfg(test)]
mod extend_tests {
    use super::{extended_manifest, ExtendOp, ExtendParam};
    use rusty_agent_runtime::connector::ConnectorManifest;
    use serde_json::json;

    fn pack() -> ConnectorManifest {
        serde_json::from_value(json!({
            "id": "acme", "version": "3", "display_name": "Acme", "description": "Acme over its API.",
            "documentation_url": "https://acme.example/docs", "base_url": "https://{instance}.acme.example",
            "connection_specification": {"type": "object", "additionalProperties": false, "required": ["instance", "credentials"], "properties": {"instance": {"type": "string"}, "credentials": {"type": "object", "additionalProperties": false, "properties": {"token": {"type": "string", "rusty_secret": true}}, "required": ["token"]}}},
            "operations": [
                {"name": "list-things", "description": "List things.", "method": "GET", "path": "/api/things?limit={limit}", "effect": "read_only", "params_schema": {"type": "object", "properties": {"limit": {"type": "integer"}}}, "auth": [{"style": "bearer", "token": "{credentials.token}"}]},
                {"name": "check", "description": "Confirm the credentials reach Acme.", "method": "GET", "path": "/api/me", "effect": "read_only", "params_schema": {"type": "object", "properties": {}}, "auth": [{"style": "bearer", "token": "{credentials.token}"}]}
            ],
            "check": "check"
        })).map(|m: ConnectorManifest| m.sealed().expect("seals")).expect("a valid pack")
    }

    #[test]
    fn the_next_version_carries_everything_plus_the_new_operations_authenticated_alike() {
        let ops = vec![ExtendOp {
            name: "List Interactions".to_owned(),
            description: "List interaction records.".to_owned(),
            method: "get".to_owned(),
            path: "/api/interactions?limit={limit}".to_owned(),
            effect: "read_only".to_owned(),
            params: vec![ExtendParam {
                name: "limit".to_owned(),
                r#type: "integer".to_owned(),
                required: false,
                description: "at most this many".to_owned(),
            }],
        }];
        let next =
            extended_manifest(&pack(), &["1".to_owned(), "3".to_owned()], &ops).expect("extends");
        assert_eq!(next.version, "4");
        assert_eq!(next.id, "acme");
        assert!(!next.hash.is_empty());
        // Sealing orders the operations by name; what matters is that every
        // one the connector had is still there beside the new one.
        let names: Vec<&str> = next.operations.iter().map(|op| op.name.as_str()).collect();
        assert_eq!(names, ["check", "list-interactions", "list-things"]);
        let added = next
            .operations
            .iter()
            .find(|op| op.name == "list-interactions")
            .unwrap();
        let followed = next
            .operations
            .iter()
            .find(|op| op.name == "list-things")
            .unwrap();
        assert_eq!(
            serde_json::to_value(&added.auth).unwrap(),
            serde_json::to_value(&followed.auth).unwrap()
        );
        assert_eq!(
            added.params_schema["properties"]["limit"]["type"],
            json!("integer")
        );
    }

    #[test]
    fn a_name_the_connector_has_and_a_path_off_the_root_are_refused() {
        let dup = vec![ExtendOp {
            name: "list-things".to_owned(),
            description: "again".to_owned(),
            method: "GET".to_owned(),
            path: "/x".to_owned(),
            effect: "read_only".to_owned(),
            params: vec![],
        }];
        assert!(extended_manifest(&pack(), &["3".to_owned()], &dup)
            .unwrap_err()
            .contains("exists already"));
        let off = vec![ExtendOp {
            name: "elsewhere".to_owned(),
            description: "off root".to_owned(),
            method: "GET".to_owned(),
            path: "https://other.example/x".to_owned(),
            effect: "read_only".to_owned(),
            params: vec![],
        }];
        assert!(extended_manifest(&pack(), &["3".to_owned()], &off)
            .unwrap_err()
            .contains("must start with"));
        assert!(extended_manifest(&pack(), &["3".to_owned()], &[])
            .unwrap_err()
            .contains("at least one"));
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::CharterEvidence;
    use chrono::{Duration, Utc};

    #[test]
    fn failures_before_the_charter_with_clean_runs_under_it_are_answered_already() {
        let since = Utc::now() - Duration::hours(1);
        let before = Some(since - Duration::minutes(30));
        let after = Some(since + Duration::minutes(5));
        let tally = CharterEvidence::tally(
            [
                (before, Some("failed")),
                (before, Some("failed")),
                (after, Some("verified")),
                (after, Some("unverified")),
            ],
            Some(since),
        );
        assert_eq!(
            (
                tally.failed_before,
                tally.runs_under,
                tally.failed_under,
                tally.verified_under
            ),
            (2, 2, 0, 1)
        );
        assert!(tally.already_answered());

        // A failure under the charter that runs is the Coach's to fix.
        let open = CharterEvidence::tally(
            [(before, Some("failed")), (after, Some("failed"))],
            Some(since),
        );
        assert!(!open.already_answered());
        // Nothing has run under it yet: unknown, not answered.
        let unknown = CharterEvidence::tally([(before, Some("failed"))], Some(since));
        assert!(!unknown.already_answered());
        // No failure anywhere: nothing to answer, nothing to refuse for.
        let clean = CharterEvidence::tally(
            [(before, Some("verified")), (after, Some("verified"))],
            Some(since),
        );
        assert!(!clean.already_answered());
        // No charter moment: every run is only seen.
        let none =
            CharterEvidence::tally([(before, Some("failed")), (after, Some("verified"))], None);
        assert_eq!((none.seen, none.failed_before, none.runs_under), (2, 0, 0));
        assert!(!none.already_answered());
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::{cadence_of, cadence_words};

    #[test]
    fn cadences_as_people_say_them() {
        assert_eq!(cadence_of("every 30 minutes"), Some((Some(1_800), None)));
        assert_eq!(cadence_of("every 2 hours"), Some((Some(7_200), None)));
        assert_eq!(cadence_of("hourly"), Some((Some(3_600), None)));
        assert_eq!(cadence_of("every day"), Some((Some(86_400), None)));
        assert_eq!(
            cadence_of("every morning"),
            Some((None, Some("0 9 * * *".to_owned())))
        );
        assert_eq!(
            cadence_of("0 9 * * 1-5"),
            Some((None, Some("0 9 * * 1-5".to_owned())))
        );
        assert_eq!(cadence_of("none"), None);
        assert_eq!(cadence_of("whenever"), None);
        assert_eq!(cadence_of("every 0 minutes"), None);
        assert_eq!(cadence_words(Some(7_200), None), "every 2 hours");
        assert_eq!(cadence_words(None, Some("0 9 * * *")), "at 0 9 * * * (UTC)");
    }
}

// ---- memory.remember ------------------------------------------------------
//
// An ordinary agent tool, not one of the platform's doors: any agent may
// name it. It writes to the memory of the person the run is for — or, when
// the run has no person behind it (a schedule, a service key), to the
// agent's own — through the same content-addressed store the memory API
// uses, and records the write on the run's journal as evidence. The next
// run for that person reads it back in its `# Memory` section.

pub const MEMORY_REMEMBER: &str = "memory.remember";
/// Ask memory a precise question and get a fact back.
pub const MEMORY_RECALL: &str = "memory.recall";

struct MemoryRemember(Doors);

#[async_trait]
impl Tool for MemoryRemember {
    fn name(&self) -> &str {
        MEMORY_REMEMBER
    }
    fn description(&self) -> &str {
        "Remember something for next time. What you learned doing your work — a fact about a topic, a source, a gap — is yours: it is kept for this agent and shown to you at the start of every future run, whoever or whatever started it. A preference the person stated, or a fact about the person you are talking to (say about: person), is proposed to that person: it waits until they accept it in the studio, and is shown only in their conversations after that — tell them it waits for their acceptance, not that it is kept. Store one thing per call, in one plain sentence, as they said it."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "One plain sentence: what to remember, as the person said it."},
                "kind": {"type": "string", "enum": ["fact", "preference"], "description": "`fact` for something true you learned (kept for this agent, every run); `preference` for how the person likes things done (kept for the person). Default fact."},
                "about": {"type": "string", "enum": ["my_work", "person"], "description": "Who a fact is about. `my_work` (default): the topic you are working on, kept for this agent. `person`: the person you are talking to, kept for them alone."},
                "key": {"type": "string", "description": "Optional short handle (`units`, `timezone`) so a later statement on the same subject supersedes this one: the newest note under a key is the one recalled, the older stays readable by id."},
                "importance": {"type": "integer", "minimum": 0, "maximum": 10, "description": "How much this matters next time, 0–10 (default 5). 8 and above for things that must never be missed — a constraint the person set, a decision that stands."},
                "trigger_phrases": {"type": "array", "items": {"type": "string"}, "description": "Up to five words or short phrases which, when a later message contains them, should bring this note back regardless of rank."}
            },
            "required": ["text"]
        })
    }
    fn effect(&self) -> Effect {
        // Content-addressed: the same sentence for the same person converges.
        Effect::Idempotent
    }
    async fn call(&self, args: Value) -> Result<Value> {
        use rusty_agent_runtime::memory::{
            MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord, MemoryScope, ProvenanceAuthor,
            ScopeAddress, ValidityWindow,
        };
        let state = self.0.open()?;
        let sentence = text(&args, "text");
        if sentence.is_empty() {
            return Err(tool_err("say what to remember — one plain sentence"));
        }
        if sentence.len() > 2000 {
            return Err(tool_err("one thing per call, under 2000 characters"));
        }
        // A credential is never remembered: memory is shown in every later
        // run and read by people. The refusal names the kind, not the value.
        if let Some(what) = rusty_agent_runtime::memory::looks_like_secret(&sentence) {
            return Ok(
                json!({"remembered": false, "note": format!("not remembered: that sentence carries {what}. Credentials live in connections, never in memory; remember the fact around it without the value.")}),
            );
        }
        let run = rusty_agent_runtime::tool::current_run().ok_or_else(|| {
            tool_err("memory.remember only works inside a run — there is nobody to remember for")
        })?;
        // Where it lives decides who sees it again. What the agent learned
        // about its work is the agent's, so a run a person started in the
        // Test panel and the same agent's nightly run read one memory —
        // keeping it under the person split the two, and the night lost
        // what the afternoon had learned. A preference, or a fact the
        // model marks `about: person`, stays with that person.
        let about_person = args.get("about").and_then(Value::as_str) == Some("person")
            || args.get("kind").and_then(Value::as_str) == Some("preference");
        let (scope, author) = match (run.person_id(), run.agent_id.as_deref(), about_person) {
            (_, Some(agent), false) => (
                ScopeAddress::new(MemoryScope::Agent, agent),
                ProvenanceAuthor::Agent {
                    agent_id: agent.to_owned(),
                },
            ),
            (Some(person), _, _) => (
                ScopeAddress::new(MemoryScope::User, person),
                ProvenanceAuthor::Agent {
                    agent_id: run.agent_id.clone().unwrap_or_else(|| "agent".to_owned()),
                },
            ),
            (None, Some(agent), true) => (
                ScopeAddress::new(MemoryScope::Agent, agent),
                ProvenanceAuthor::Agent {
                    agent_id: agent.to_owned(),
                },
            ),
            (None, None, _) => {
                return Err(tool_err(
                    "this run has neither a person behind it nor an agent it is of — nothing to remember for",
                ));
            }
        };
        let kind = match args.get("kind").and_then(Value::as_str) {
            Some("preference") => MemoryKind::Preference,
            _ => MemoryKind::Fact,
        };
        let now = Utc::now();
        // The runtime, not the model, says whether this run read outside
        // content before the write: such a note is kept, marked, and held
        // at lower confidence.
        let taint = run_taint(&run.run_id);
        let mut record = MemoryRecord::new(
            kind,
            scope.clone(),
            MemoryProvenance {
                author,
                evidence: rusty_agent_runtime::memory::MemoryEvidence {
                    run_id: Some(run.run_id.clone()),
                    ..Default::default()
                },
                written_at: now,
            },
            if taint.is_some() { 0.5 } else { 0.9 },
            ValidityWindow {
                valid_from: now,
                valid_until: None,
            },
            now,
            json!({ "text": sentence }),
        )
        .map_err(|e| tool_err(e.to_string()))?;
        // The key in its canonical form, so two spellings of one subject
        // supersede each other instead of standing side by side.
        let key_as_given = text(&args, "key");
        let domain = if matches!(kind, MemoryKind::Preference) {
            "preference"
        } else {
            "fact"
        };
        let key = if key_as_given.is_empty() {
            String::new()
        } else {
            rusty_agent_runtime::memory_tiers::canonical_key(&key_as_given, domain)
                .unwrap_or_default()
        };
        if !key.is_empty() {
            record = record.with_key(key.clone());
        }
        // Importance is the rank the recall lanes use; trigger phrases ride
        // as tags so lane-one recall can match them without a model call.
        // Both live on fields the record already has, so nothing about the
        // record's shape or content address changes.
        let importance = args
            .get("importance")
            .and_then(Value::as_i64)
            .unwrap_or(5)
            .clamp(0, 10);
        record = record.with_priority(importance);
        let triggers: Vec<String> = args
            .get("trigger_phrases")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|t| t.trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .take(5)
                    .collect()
            })
            .unwrap_or_default();
        let mut tags: Vec<String> = triggers.iter().map(|t| format!("trigger:{t}")).collect();
        if taint.is_some() {
            tags.push(rusty_agent_runtime::memory::ORIGIN_UNTRUSTED_TAG.to_owned());
        }
        if !tags.is_empty() {
            record = record.with_tags(tags);
        }
        let tenant_owned = tenant_now();
        let tenant = tenant_owned.as_str();
        // Same key, same scope: the new note supersedes the newest live one,
        // so a later statement on a subject replaces the earlier instead of
        // standing beside it as a conflict. The older stays readable by id.
        let mut superseded: Option<String> = None;
        if !key.is_empty() {
            let live = state
                .server_store
                .query_memory(
                    tenant,
                    &MemoryQuery {
                        scope: Some(scope.clone()),
                        include_candidates: true,
                        ..Default::default()
                    },
                    now,
                )
                .await
                .map_err(|e| tool_err(e.to_string()))?;
            // A note written before keys were canonical still matches by
            // its canonical form.
            let same_subject = |k: &str| rusty_agent_runtime::memory_tiers::keys_match(k, &key);
            let live: Vec<_> = live
                .into_iter()
                .filter(|r| r.key.as_deref().is_some_and(same_subject))
                .collect();
            let newest = live.into_iter().max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.memory_id.cmp(&b.memory_id))
            });
            if let Some(prev) = newest {
                let same_text = matches!(&prev.content, rusty_agent_runtime::record::PayloadRef::Inline(v) if v.get("text").and_then(Value::as_str) == Some(sentence.as_str()));
                if !same_text {
                    record = record.with_supersedes(prev.memory_id.clone());
                    superseded = Some(prev.memory_id);
                }
            }
        }
        // The same sentence for the same scope converges: a note already held
        // is answered with its id, not stored twice — a run that remembers
        // what it remembered last time adds nothing.
        let held = state
            .server_store
            .query_memory(
                tenant,
                &rusty_agent_runtime::memory::MemoryQuery {
                    scope: Some(scope.clone()),
                    include_candidates: true,
                    ..Default::default()
                },
                now,
            )
            .await
            .map_err(|e| tool_err(e.to_string()))?
            .into_iter()
            .find(|r| match &r.content {
                rusty_agent_runtime::record::PayloadRef::Inline(v) => {
                    v.get("text").and_then(Value::as_str) == Some(sentence.as_str())
                }
                _ => false,
            });
        if let Some(held) = held {
            return Ok(json!({
                "remembered": sentence,
                "for": scope.as_address(),
                "memory_id": held.memory_id,
                "new": false,
                "note": "already remembered — nothing stored twice",
            }));
        }
        // A note about a person is theirs to accept: an agent proposes it,
        // and it is recalled only once the person has accepted it in the
        // studio (`POST /memory/{id}/accept`). The agent's own work lands.
        let proposed_to = (scope.scope == MemoryScope::User).then(|| scope.id.clone());
        if proposed_to.is_some() {
            record = record.with_candidacy(rusty_agent_runtime::memory::Candidacy::Pending);
        }
        let created = state
            .server_store
            .put_memory(tenant, &record, &json!({ "text": sentence }))
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        if let Some(journal) = &run.journal {
            let draft = rusty_agent_runtime::journal::EventDraft::new(
                rusty_agent_runtime::record::RunEventKind::MemoryWrite,
                Effect::Idempotent,
            )
            .input(json!({ "effect_key": format!("memory:{}:{}", scope.as_address(), record.memory_id), "memory_id": record.memory_id }))
            .output(serde_json::to_value(&record).unwrap_or(Value::Null));
            journal.record(draft);
        }
        let mut out = json!({
            "remembered": sentence,
            "for": scope.as_address(),
            "memory_id": record.memory_id,
            "new": created,
            "importance": importance,
        });
        if !key.is_empty() {
            out["key"] = json!(key);
            if key != key_as_given {
                out["key_as_given"] = json!(key_as_given);
            }
        }
        if let Some(person) = proposed_to {
            out["waits_for"] = json!(person);
            out["note"] = json!(
                "proposed to the person it is about — recalled once they accept it in the studio, so do not count on it next time yet"
            );
        }
        if let Some(prev) = superseded {
            out["supersedes"] = json!(prev);
            out["note"] = json!(
                "the earlier note under this key is superseded: recall returns this one, the old stays readable by id"
            );
        }
        if let Some(what) = taint {
            out["origin"] = json!("untrusted");
            out["origin_note"] = json!(format!(
                "kept, marked as learned after reading {what}: it is shown as unverified and held at lower confidence until a person confirms it"
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tool_line_tests {
    use super::split_tool_lines;

    #[test]
    fn a_tool_line_keeps_its_note_and_a_bare_name_has_none() {
        let (names, notes) = split_tool_lines(&[
            "servicenow.list-records — to read today's incidents".to_owned(),
            "slack.post-message - only after the brief is written".to_owned(),
            "  skills.read  ".to_owned(),
            "".to_owned(),
        ]);
        assert_eq!(
            names,
            vec![
                "servicenow.list-records",
                "slack.post-message",
                "skills.read"
            ]
        );
        assert_eq!(
            notes.get("servicenow.list-records").map(String::as_str),
            Some("to read today's incidents")
        );
        assert_eq!(
            notes.get("slack.post-message").map(String::as_str),
            Some("only after the brief is written")
        );
        assert!(!notes.contains_key("skills.read"));
    }
}
