//! Context pipeline tests (R0.13 wave 1): the golden assembly, the
//! determinism proof, budget and compaction behavior, the `context_policy`
//! candidate delta, and the wave's exit criterion — exact replay of a run
//! whose history section compacted mid-run.
//!
//! Golden files under `tests/golden/` pin every wire shape this module
//! owns. `UPDATE_GOLDEN=1` blesses a change; the diff is the contract
//! change under review.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::context::{
    AssemblingChatModel, CompactionPolicy, ContextInputs, ContextPipeline, ContextPolicy,
    EstimatedTokenCounter, MemorySectionPolicy, SectionKind, SectionManifest, SectionPolicy,
    SkillSectionEntry, StoredSummary, TokenCounter, ToolsSectionPolicy, CONTEXT_PIPELINE_PARENT,
    CONTEXT_POLICY_SCHEMA_VERSION, MANIFEST_MESSAGE_NAME, SKILLS_READ_DIRECTIVE,
    SUMMARY_CLIPPED_NOTICE, SUMMARY_MARKER,
};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    surface_for_kind, Candidate, CandidateContent, CandidateKind, EnvelopeRule, EvidenceSpan,
    PromotionEnvelope,
};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role, ToolCall};
use rusty_agent_runtime::memory::{
    estimated_tokens, InMemoryMemoryStore, JournaledMemory, MemoryKind, MemoryProvenance,
    MemoryQuery, MemoryRecord, MemoryReplaySource, MemoryScope, MemorySource, MemoryStore,
    ProvenanceAuthor, ScopeAddress, ValidityWindow, DEFAULT_TOKEN_MARGIN_PERCENT,
};
use rusty_agent_runtime::record::{Effect, PayloadRef, RunEvent, RunEventKind};
use rusty_agent_runtime::replay::{ExactReplay, RecordingChatModel, ReplayingChatModel};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_runtime::tool_select::{
    manifests_for_registry, ExclusionReason, ToolSelectionOverlay, ToolSelectionPolicy,
};

// ---------- golden-file machinery ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- shared fixtures ----------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;

fn logical_clock() -> Clock {
    Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS)
}

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn provenance() -> MemoryProvenance {
    MemoryProvenance {
        author: ProvenanceAuthor::Agent {
            agent_id: "researcher-7".into(),
        },
        evidence: Default::default(),
        written_at: ts(1_750_000_001_000),
    }
}

fn timezone_record() -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Preference,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        provenance(),
        0.9,
        ValidityWindow::starting(ts(1_750_000_000_000)),
        ts(1_750_000_001_000),
        json!({"timezone": "UTC+4"}),
    )
    .unwrap()
    .with_key("user.timezone")
    .with_priority(5)
}

fn language_record() -> MemoryRecord {
    MemoryRecord::new(
        MemoryKind::Preference,
        ScopeAddress::new(MemoryScope::User, "user-7"),
        provenance(),
        0.8,
        ValidityWindow::starting(ts(1_750_000_000_000)),
        ts(1_750_000_002_000),
        json!({"language": "en-US"}),
    )
    .unwrap()
    .with_key("user.language")
}

async fn store_with_records() -> Arc<InMemoryMemoryStore> {
    let store = Arc::new(InMemoryMemoryStore::new());
    store.put(&timezone_record()).await.unwrap();
    store.put(&language_record()).await.unwrap();
    store
}

fn echo_schema() -> Value {
    json!({"type": "function", "function": {"name": "echo", "description": "Echoes its input.", "parameters": {"type": "object", "properties": {"text": {"type": "string"}}}}})
}

fn search_schema() -> Value {
    json!({"type": "function", "function": {"name": "search", "description": "Searches the index.", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}})
}

fn skill_entry() -> SkillSectionEntry {
    SkillSectionEntry {
        name: "summarize".into(),
        revision: "3".into(),
        content_hash: "a".repeat(64),
        metadata: "Distill long threads into decisions.".into(),
        body: Some("1. Read the thread.\n2. List decisions.".into()),
    }
}

/// The policy every test derives from: all six sections enabled, a
/// compaction policy whose trigger the short golden history stays below.
fn policy() -> ContextPolicy {
    ContextPolicy {
        schema_version: CONTEXT_POLICY_SCHEMA_VERSION.to_owned(),
        budget: rusty_agent_runtime::memory::ContextBudget::new(4096),
        tokenizer: Default::default(),
        identity: Some(SectionPolicy::new(256)),
        task: Some(SectionPolicy::new(256)),
        skills: Some(SectionPolicy::new(512)),
        tools: Some(ToolsSectionPolicy::new(512)),
        memory: Some(MemorySectionPolicy {
            budget_tokens: 512,
            overflow: None,
            query: MemoryQuery {
                scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
                ..Default::default()
            },
        }),
        history: Some(SectionPolicy::new(1024)),
        compaction: Some(CompactionPolicy {
            trigger_tokens: 400,
            keep_recent_messages: 2,
            keep_recent_steps: 0,
            summary_max_tokens: 128,
            prompt: "Summarize the conversation prefix, preserving decisions.".into(),
        }),
        recall: None,
    }
}

fn golden_inputs() -> ContextInputs {
    ContextInputs {
        identity: Some("You are Rusty, a governed agent runtime test double.".into()),
        task: Some("Summarize the user's preferences.".into()),
        skills: vec![skill_entry()],
        tools: vec![echo_schema(), search_schema()],
        history: vec![
            ChatMessage::user("what do you know about me?"),
            ChatMessage::assistant("Let me check memory."),
            ChatMessage::user("go ahead"),
        ],
        ..Default::default()
    }
}

async fn assemble_with_store(
    pipeline: &ContextPipeline,
    inputs: &ContextInputs,
) -> rusty_agent_runtime::context::ContextAssembly {
    let journal = Journal::new("run-context-test", "t-context", logical_clock());
    let memory = JournaledMemory::new(&journal, MemorySource::Store(store_with_records().await));
    pipeline.assemble(inputs, Some(&memory)).await.unwrap()
}

// ---------- the golden assembly ----------

#[tokio::test]
async fn golden_context_assembly_shape() {
    let pipeline =
        ContextPipeline::new(policy())
            .unwrap()
            .with_policy_pin("test-policy", None, None);
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;

    // The manifest is the reserved, model-visible metadata message; it
    // rides last, behind everything a provider could serve from its
    // prefix cache, so a call's prefix is the whole of the last call's.
    let manifest_message = assembly
        .messages
        .iter()
        .find(|m| m.name.as_deref() == Some(MANIFEST_MESSAGE_NAME))
        .expect("the manifest is in the assembly");
    assert_eq!(
        assembly
            .messages
            .iter()
            .position(|m| m.name.as_deref() == Some(MANIFEST_MESSAGE_NAME)),
        Some(assembly.messages.len() - 1),
        "last"
    );
    assert_eq!(
        manifest_message.name.as_deref(),
        Some(MANIFEST_MESSAGE_NAME)
    );
    assert!(manifest_message
        .content
        .as_deref()
        .is_some_and(|c| c.starts_with("context-manifest-v1\n")));

    assert_golden("context_assembly.json", &assembly);
}

#[test]
fn golden_context_policy_candidate_shape() {
    let candidate = Candidate::new(
        CandidateContent::ContextPolicy {
            name: "default".into(),
            policy: policy().to_value().unwrap(),
        },
        ProvenanceAuthor::Distiller {
            name: "context-distiller".into(),
        },
        EvidenceSpan::default(),
        ts(1_750_000_010_000),
    )
    .unwrap();
    assert_golden("candidate_context_policy.json", &candidate);
}

// ---------- determinism ----------

#[tokio::test]
async fn equal_inputs_produce_byte_equal_assemblies() {
    // Two pipelines, two stores, two journals: the same logical inputs.
    let first = assemble_with_store(
        &ContextPipeline::new(policy())
            .unwrap()
            .with_policy_pin("test-policy", None, None),
        &golden_inputs(),
    )
    .await;
    let second = assemble_with_store(
        &ContextPipeline::new(policy())
            .unwrap()
            .with_policy_pin("test-policy", None, None),
        &golden_inputs(),
    )
    .await;
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap(),
        "equal inputs and equal policy must produce byte-equal assemblies"
    );
}

// ---------- budgets ----------

#[tokio::test]
async fn identity_overflow_is_a_configuration_error_not_a_truncation() {
    let mut policy = policy();
    policy.identity = Some(SectionPolicy::new(8)); // far too small
    let pipeline = ContextPipeline::new(policy).unwrap();
    let journal = Journal::new("run-context-test", "t-context", logical_clock());
    let memory = JournaledMemory::new(&journal, MemorySource::Store(store_with_records().await));
    let error = pipeline
        .assemble(&golden_inputs(), Some(&memory))
        .await
        .unwrap_err();
    assert!(
        matches!(error, RustyError::InvalidUpdate(_)),
        "identity overflow must fail, got {error:?}"
    );
}

/// The wire order is the cache order, and each place in it was measured,
/// not assumed: a person's recall is usually the same bytes turn after
/// turn, so it stays ahead of the history, inside the cached prefix
/// (2026-09-12: 65% of a second prompt cached this way, 20% with the recall
/// moved behind the history); the manifest changes every call, so it rides
/// last, behind everything cacheable. Every history message precedes the
/// manifest, and the sections keep their canonical order in the manifest.
#[tokio::test]
async fn memory_rides_ahead_of_the_history_and_the_manifest_rides_last() {
    let pipeline = ContextPipeline::new(policy()).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;
    let messages = &assembly.messages;
    let memory_at = messages
        .iter()
        .position(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Memory"))
        })
        .expect("a recall");
    let manifest_at = messages
        .iter()
        .position(|m| m.name.as_deref() == Some(MANIFEST_MESSAGE_NAME))
        .expect("the manifest");
    let history: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != Role::System)
        .map(|(i, _)| i)
        .collect();
    for at in &history {
        assert!(
            *at > memory_at,
            "history at {at} follows the recall at {memory_at}: {messages:#?}"
        );
    }
    assert_eq!(manifest_at, messages.len() - 1, "the manifest is last");
    for at in &history {
        assert!(*at < manifest_at, "history at {at} precedes the manifest");
    }
    let kinds: Vec<SectionKind> = assembly.manifest.sections.iter().map(|s| s.kind).collect();
    let memory_report = kinds
        .iter()
        .position(|k| *k == SectionKind::Memory)
        .unwrap();
    let history_report = kinds
        .iter()
        .position(|k| *k == SectionKind::History)
        .unwrap();
    assert!(memory_report < history_report);
}

#[tokio::test]
async fn memory_section_truncates_the_lowest_ranked_record() {
    let mut policy = policy();
    // Both records fit the journaled read's budget; the section's rendered
    // budget fits only the higher-priority one.
    policy.budget = rusty_agent_runtime::memory::ContextBudget::new(100_000);
    policy.memory = Some(MemorySectionPolicy {
        budget_tokens: 80,
        overflow: None,
        query: MemoryQuery {
            scope: Some(ScopeAddress::new(MemoryScope::User, "user-7")),
            ..Default::default()
        },
    });
    let pipeline = ContextPipeline::new(policy).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;

    let memory_section = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Memory)
        .expect("memory section report");
    assert!(memory_section.truncated);
    assert_eq!(memory_section.ids, vec![timezone_record().memory_id]);
    // The model is told what the budget left out — by count, with the way
    // to get it — so a lost note is a known unknown, not a silent one.
    let memory_message = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Memory"))
        })
        .expect("the memory message");
    let body = memory_message.content.as_deref().unwrap();
    assert!(
        body.ends_with("— 1 more note not shown for budget; ask with memory.recall for the rest"),
        "the omitted line names the count: {body}"
    );
}

/// With a memory section, the declared sections still sum within the
/// window: memory comes out of the history's share instead of being added
/// on top (35,488 against 32,000 at the default, before this).
#[test]
fn standard_policy_with_memory_sums_within_the_window() {
    for window in [4_096u32, 8_192, 32_000, 128_000, 200_000] {
        let memory = (window.max(4_096) / 8).clamp(512, 4_096);
        let policy = ContextPolicy::standard(window)
            .with_memory_section(memory)
            .with_recall(1_200, 5);
        let declared = |p: &ContextPolicy| {
            p.identity.as_ref().unwrap().budget_tokens
                + p.task.as_ref().unwrap().budget_tokens
                + p.skills.as_ref().unwrap().budget_tokens
                + p.tools.as_ref().unwrap().budget_tokens
                + p.memory.as_ref().map(|m| m.budget_tokens).unwrap_or(0)
                + p.recall.as_ref().map(|r| r.budget_tokens).unwrap_or(0)
                + p.history.as_ref().unwrap().budget_tokens
                + 512
        };
        let without = declared(&ContextPolicy::standard(window));
        let sum = declared(&policy);
        // Where the plain policy fits the window — every window that gives
        // the history more than its 1,024 floor — it still fits with memory,
        // because memory comes out of the history's share. At a window so
        // small the floor binds (4k), the floor wins and the sum was already
        // over; that is the floor's contract, not this one's.
        if without <= window.max(4_096) {
            assert!(
                sum <= window.max(4_096),
                "window {window}: sections sum to {sum}, {without} without memory"
            );
        }
        assert!(
            policy.history.as_ref().unwrap().budget_tokens >= 1_024,
            "window {window}: history keeps its floor"
        );
        let compaction = policy.compaction.as_ref().unwrap();
        assert!(
            compaction.trigger_tokens < policy.history.as_ref().unwrap().budget_tokens,
            "window {window}: the trigger ({}) follows the history's share ({})",
            compaction.trigger_tokens,
            policy.history.as_ref().unwrap().budget_tokens
        );
    }
}

#[test]
fn the_task_section_makes_room_for_the_blocks() {
    // Three full blocks (4,200 chars) and the situation at an 8k window:
    // the task budget grows past its 128 default to hold them, out of the
    // history's share, and the compaction trigger follows the history.
    let policy = ContextPolicy::standard(8_000)
        .with_memory_section(1_000)
        .with_task_room(4_200 + 512);
    let task = policy.task.as_ref().unwrap().budget_tokens;
    assert!(task > 1_000, "task budget {task} holds the blocks");
    let history = policy.history.as_ref().unwrap().budget_tokens;
    assert!(history >= 1_024);
    assert!(policy.compaction.as_ref().unwrap().trigger_tokens < history);
    // Small text: nothing changes.
    let same = ContextPolicy::standard(8_000).with_task_room(64);
    assert_eq!(
        same.task.as_ref().unwrap().budget_tokens,
        ContextPolicy::standard(8_000)
            .task
            .as_ref()
            .unwrap()
            .budget_tokens
    );
}

#[tokio::test]
async fn skill_procedures_ride_whole_or_not_at_all() {
    // Three skills whose procedures do not fit a 256-token section: the
    // section names all three with when to use them, carries no cut
    // procedure, says to read a skill first, and the manifest counts the
    // three left out. With room, every procedure rides in full.
    let skills: Vec<SkillSectionEntry> = (1..=3)
        .map(|i| SkillSectionEntry {
            name: format!("skill-{i}"),
            revision: "r1".into(),
            content_hash: format!("h{i}"),
            metadata: format!("use for case {i}"),
            body: Some(format!("Step one of {i}. ").repeat(60)),
        })
        .collect();
    let mut tight = policy();
    tight.skills = Some(SectionPolicy::new(256));
    let pipeline = ContextPipeline::new(tight).unwrap();
    let inputs = ContextInputs {
        skills: skills.clone(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let section = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Skills"))
        })
        .expect("skills section");
    let text = section.content.as_deref().unwrap();
    assert!(text.contains("- skill-3 (revision r1, h3): use for case 3"));
    assert!(
        !text.contains("Step one of"),
        "no procedure rides cut: {text}"
    );
    assert!(text.ends_with(SKILLS_READ_DIRECTIVE));
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Skills)
        .unwrap();
    assert_eq!(report.skill_bodies_omitted, 3);
    assert!(!report.truncated);

    // The run read skill-2: its procedure rides whole ahead of the
    // directive, which still names the two not shown; the manifest counts two.
    let mut read_it = policy();
    read_it.skills = Some(SectionPolicy::new(512));
    let pipeline = ContextPipeline::new(read_it).unwrap();
    let mut history = golden_inputs().history;
    history.push(ChatMessage::assistant_tool_calls(vec![
        rusty_agent_runtime::llm::ToolCall::new(
            "r1",
            "skills.read",
            serde_json::json!({"name": "skill-2"}),
        ),
    ]));
    history.push(ChatMessage::tool_result("r1", "the procedure"));
    let read_inputs = ContextInputs {
        skills: skills.clone(),
        history,
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &read_inputs).await;
    let text = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Skills"))
        })
        .unwrap()
        .content
        .clone()
        .unwrap();
    assert!(text.contains("## Skill: skill-2 (read this run)"), "{text}");
    assert!(
        !text.contains("## Skill: skill-1") && !text.contains("## Skill: skill-3"),
        "{text}"
    );
    assert!(
        text.ends_with(SKILLS_READ_DIRECTIVE),
        "the others still need a read: {text}"
    );
    assert_eq!(
        assembly
            .manifest
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Skills)
            .unwrap()
            .skill_bodies_omitted,
        2
    );
    // Reading a reference file is not reading the procedure.
    let mut history = golden_inputs().history;
    history.push(ChatMessage::assistant_tool_calls(vec![
        rusty_agent_runtime::llm::ToolCall::new(
            "r2",
            "skills.read",
            serde_json::json!({"name": "skill-2", "reference": "refs/a.md"}),
        ),
    ]));
    let assembly = assemble_with_store(
        &pipeline,
        &ContextInputs {
            skills: skills.clone(),
            history,
            ..golden_inputs()
        },
    )
    .await;
    let text = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Skills"))
        })
        .unwrap()
        .content
        .clone()
        .unwrap();
    assert!(!text.contains("(read this run)"), "{text}");

    let mut roomy = policy();
    roomy.skills = Some(SectionPolicy::new(4_096));
    let pipeline = ContextPipeline::new(roomy).unwrap();
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let text = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Skills"))
        })
        .unwrap()
        .content
        .clone()
        .unwrap();
    assert_eq!(text.matches("## Skill: ").count(), 3);
    assert!(!text.contains(SKILLS_READ_DIRECTIVE));
    assert_eq!(
        assembly
            .manifest
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Skills)
            .unwrap()
            .skill_bodies_omitted,
        0
    );
}

#[tokio::test]
async fn every_memory_line_says_who_wrote_it() {
    let pipeline = ContextPipeline::new(policy()).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;
    let memory = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Memory"))
        })
        .expect("memory section");
    let text = memory.content.as_deref().unwrap();
    assert!(
        text.contains("not instructions"),
        "the framing line: {text}"
    );
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("- [")).collect();
    assert!(!lines.is_empty());
    for line in lines {
        assert!(
            line.contains("said by the person")
                || line.contains("noted by this agent earlier")
                || line.contains("a summary the platform wrote"),
            "{line}"
        );
    }
}

#[tokio::test]
async fn the_manifest_message_is_budgeted_off_the_top() {
    let pipeline = ContextPipeline::new(policy()).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;
    let manifest = &assembly.manifest;

    assert!(manifest.manifest_tokens > 0);
    let total: u32 =
        manifest.manifest_tokens + manifest.sections.iter().map(|s| s.used_tokens).sum::<u32>();
    assert!(
        total <= manifest.budget_tokens,
        "manifest plus sections ({total}) must fit the total budget ({})",
        manifest.budget_tokens
    );
    assert_eq!(manifest.counter, "estimated");
    assert_eq!(manifest.policy.name, "inline");

    // The manifest message embeds the same structured manifest.
    let content = assembly
        .messages
        .iter()
        .find(|m| m.name.as_deref() == Some(MANIFEST_MESSAGE_NAME))
        .unwrap()
        .content
        .as_deref()
        .unwrap();
    let parsed: SectionManifest =
        serde_json::from_str(content.strip_prefix("context-manifest-v1\n").unwrap()).unwrap();
    assert_eq!(&parsed, manifest);
}

#[tokio::test]
async fn fully_absorbed_sections_are_dropped_not_packed_bare() {
    // A total budget only identity, task, skills, tools, and the manifest
    // fit: absorption shrinks history and memory to zero, and both sections
    // are dropped from the assembly rather than packed as a bare header or
    // an empty message list.
    let mut policy = policy();
    policy.budget = rusty_agent_runtime::memory::ContextBudget::new(480);
    let pipeline = ContextPipeline::new(policy).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;

    let kinds: Vec<SectionKind> = assembly.manifest.sections.iter().map(|s| s.kind).collect();
    assert!(
        !kinds.contains(&SectionKind::History),
        "absorbed-to-zero history must be dropped, not packed empty: {kinds:?}"
    );
    assert!(
        !kinds.contains(&SectionKind::Memory),
        "absorbed-to-zero memory must be dropped, not packed as a bare header: {kinds:?}"
    );
    assert!(assembly
        .messages
        .iter()
        .all(|m| m.content.as_deref() != Some("# Memory")));
}

// ---------- governed tool selection ----------

/// A toy tool for the shortlist exit criterion: distinct name, shared
/// schema shape, pinned effect class.
struct ToyTool {
    name: String,
    description: String,
    effect: Effect,
}

#[async_trait::async_trait]
impl Tool for ToyTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!(null))
    }
}

#[tokio::test]
async fn a_forty_tool_registry_shortlists_to_a_pinned_top_k() {
    // Forty tools: three tag-matched, three above the run's effect ceiling,
    // the rest score-zero eligible. The pinned expectation: the tag-matched
    // three first (score 10000 each, ties broken by ascending name), then
    // the five lowest-named score-zero eligible tools — eight total, the
    // policy's k.
    let mut registry = ToolRegistry::new();
    for i in 1..=40u32 {
        let effect = match i {
            7 | 19 | 31 => Effect::NonIdempotent,
            _ => Effect::Idempotent,
        };
        registry.register(ToyTool {
            name: format!("tool_{i:02}"),
            description: format!("Toy tool number {i}."),
            effect,
        });
    }
    let overlay = || ToolSelectionOverlay {
        tags: vec!["search".to_owned()],
        ..Default::default()
    };
    let overlays = std::collections::BTreeMap::from([
        ("tool_05".to_owned(), overlay()),
        ("tool_12".to_owned(), overlay()),
        ("tool_23".to_owned(), overlay()),
    ]);
    let manifests = manifests_for_registry(&registry, &overlays).unwrap();
    assert_eq!(manifests.len(), 40);

    let mut policy = policy();
    policy.tools = Some(
        ToolsSectionPolicy::new(512).with_selection(ToolSelectionPolicy {
            cutoff: 20,
            k: 8,
            ..Default::default()
        }),
    );
    let pipeline = ContextPipeline::new(policy).unwrap();
    let inputs = ContextInputs {
        tool_manifests: manifests,
        task_tags: vec!["search".to_owned()],
        effect_ceiling: Some(Effect::Idempotent),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;

    let expected: Vec<String> = [
        "tool_05", "tool_12", "tool_23", "tool_01", "tool_02", "tool_03", "tool_04", "tool_06",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(assembly.tools.len(), 8);
    let tools_section = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Tools)
        .expect("tools section report");
    // The ranking chose these eight; on the wire they stand in name order,
    // so the same selection makes the same bytes on every turn (the
    // provider's prefix cache). The ranking itself is in the shortlist.
    let mut by_name = expected.clone();
    by_name.sort();
    assert_eq!(tools_section.ids, by_name);
    let wire_names: Vec<String> = assembly
        .tools
        .iter()
        .filter_map(|t| {
            t.pointer("/function/name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        wire_names, by_name,
        "the wire tools follow the same stable order"
    );

    // The section manifest carries the FULL selection outcome: every scored
    // manifest in the ranking, every exclusion with its reason.
    let shortlist = tools_section
        .shortlist
        .as_ref()
        .expect("the governed shortlist is recorded");
    let selected_names: Vec<&str> = shortlist.selected.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        selected_names,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(shortlist.ranking.len(), 37);
    assert_eq!(shortlist.excluded.len(), 3);
    assert!(shortlist
        .excluded
        .iter()
        .all(|e| e.reason == ExclusionReason::EffectAboveCeiling));

    // The assembled schemas are the canonical registry shape, so the model
    // sees byte-identical schemas on either path.
    let tool_05 = assembly
        .tools
        .iter()
        .find(|t| t.pointer("/function/name").and_then(|v| v.as_str()) == Some("tool_05"))
        .expect("the top-ranked tool is on the wire");
    assert_eq!(
        *tool_05,
        json!({"type": "function", "function": {"name": "tool_05", "description": "Toy tool number 5.", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}})
    );
}

// ---------- token accounting ----------

#[test]
fn estimated_counter_is_bytes_per_four_plus_margin() {
    let counter = EstimatedTokenCounter::new(DEFAULT_TOKEN_MARGIN_PERCENT);
    let message = ChatMessage::user("abcd");
    let bytes = serde_json::to_vec(&message).unwrap().len() as u64;
    assert_eq!(
        counter.count(std::slice::from_ref(&message), "any-model"),
        estimated_tokens(bytes, DEFAULT_TOKEN_MARGIN_PERCENT)
    );
}

// ---------- compaction ----------

/// A scripted model: pops one canned text response per `chat` call.
struct ScriptedModel {
    script: Mutex<VecDeque<String>>,
}

impl ScriptedModel {
    fn new(lines: Vec<&str>) -> Self {
        Self {
            script: Mutex::new(lines.into_iter().map(str::to_owned).collect()),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let content = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message: ChatMessage::assistant(content),
            model: Some("scripted-1".into()),
            usage: None,
        })
    }
}

/// A model that panics if it is ever called — the replay sentinel.
struct PanicModel {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChatModel for PanicModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("exact replay hit the network: PanicModel was invoked")
    }
}

fn long_history() -> Vec<ChatMessage> {
    (1..=6)
        .map(|i| ChatMessage::user(format!("user turn {i} with some content")))
        .collect()
}

fn compacting_policy() -> ContextPolicy {
    let mut policy = policy();
    policy.compaction = Some(CompactionPolicy {
        trigger_tokens: 64, // six history messages exceed this; three do not
        keep_recent_messages: 2,
        keep_recent_steps: 0,
        summary_max_tokens: 128,
        prompt: "Summarize the conversation prefix, preserving decisions.".into(),
    });
    policy
}

#[tokio::test]
async fn compaction_fires_at_the_trigger_and_marks_the_summary() {
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["earlier turns, summarized"]));
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer);
    let inputs = ContextInputs {
        history: long_history(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;

    let history = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .expect("history section report");
    let compaction = history.compaction.as_ref().expect("compaction fired");
    assert_eq!(compaction.watermark, 4);

    // The assembled history ends the message list: the marked generated
    // summary, then the two verbatim tail messages. The input history is
    // untouched.
    // Without the manifest and the memory recall (both ride ahead of the
    // newest message): the summary, then the two verbatim tail messages.
    let without_manifest: Vec<_> = assembly
        .messages
        .iter()
        .filter(|m| {
            m.name.as_deref() != Some(MANIFEST_MESSAGE_NAME)
                && !m
                    .content
                    .as_deref()
                    .is_some_and(|c| c.starts_with("# Memory"))
        })
        .cloned()
        .collect();
    let summary = &without_manifest[without_manifest.len() - 3];
    assert!(
        summary
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with(SUMMARY_MARKER)),
        "the summary message is marked as generated: {summary:?}"
    );
    let tail = &without_manifest[without_manifest.len() - 2..];
    assert_eq!(
        tail[0].content.as_deref(),
        Some("user turn 5 with some content")
    );
    assert_eq!(
        tail[1].content.as_deref(),
        Some("user turn 6 with some content")
    );
    assert_eq!(
        inputs.history.len(),
        6,
        "compaction never mutates the channel"
    );
    assert_eq!(
        inputs.history[0].content.as_deref(),
        Some("user turn 1 with some content")
    );
}

/// Three tool-using steps: ask, call, result, answer — twelve messages.
fn tool_history() -> Vec<ChatMessage> {
    let mut history = Vec::new();
    for i in 1..=3 {
        history.push(ChatMessage::user(format!(
            "user question {i} with some content"
        )));
        history.push(ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            format!("call-{i}"),
            "lookup",
            json!({ "q": i }),
        )]));
        history.push(ChatMessage::tool_result(
            format!("call-{i}"),
            format!("result {i} with some content"),
        ));
        history.push(ChatMessage::assistant(format!(
            "answer {i} with some content"
        )));
    }
    history
}

fn verbatim_tail(assembly: &rusty_agent_runtime::context::ContextAssembly) -> Vec<ChatMessage> {
    let summary_at = assembly
        .messages
        .iter()
        .position(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with(SUMMARY_MARKER))
        })
        .expect("a summary message");
    // The person's question, when compaction kept it after the summary,
    // rides before the tail; the tail is what follows it.
    let kept = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .and_then(|h| h.compaction.as_ref())
        .is_some_and(|c| c.question_kept);
    assembly.messages[summary_at + 1 + usize::from(kept)..]
        .iter()
        .filter(|m| {
            m.name.as_deref() != Some(MANIFEST_MESSAGE_NAME)
                && !m
                    .content
                    .as_deref()
                    .is_some_and(|c| c.starts_with("# Memory"))
        })
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_long_turn_keeps_the_question_it_is_answering() {
    // One question, then six tool steps: the kept tail is the last two
    // steps, so the question falls in the summarised part. It is kept
    // verbatim after the summary, and the report says so.
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["the earlier lookups, summarized"]));
    let mut policy = compacting_policy();
    policy.compaction.as_mut().unwrap().keep_recent_messages = 2;
    policy.compaction.as_mut().unwrap().keep_recent_steps = 2;
    let pipeline = ContextPipeline::new(policy)
        .unwrap()
        .with_summarizer(summarizer);
    let mut history = vec![ChatMessage::user(
        "My laptop will not join the office Wi-Fi since this morning.",
    )];
    for i in 1..=6 {
        history.push(ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            format!("call-{i}"),
            "lookup",
            json!({ "q": i }),
        )]));
        history.push(ChatMessage::tool_result(
            format!("call-{i}"),
            format!("result {i} with some content"),
        ));
    }
    let inputs = ContextInputs {
        history,
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    assert!(
        report.compaction.as_ref().unwrap().question_kept,
        "{report:?}"
    );
    let summary_at = assembly
        .messages
        .iter()
        .position(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with(SUMMARY_MARKER))
        })
        .unwrap();
    let after = &assembly.messages[summary_at + 1];
    assert!(
        matches!(after.role, Role::User)
            && after
                .content
                .as_deref()
                .is_some_and(|c| c.contains("Wi-Fi")),
        "the question follows the summary: {after:?}"
    );
}

#[tokio::test]
async fn the_verbatim_window_never_splits_a_tool_call_from_its_results() {
    // keep_recent_messages = 2 would begin the tail at the last tool
    // result, orphaning it from its call; the tail begins at the call.
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["earlier steps, summarized"]));
    let mut policy = compacting_policy();
    policy.compaction.as_mut().unwrap().keep_recent_messages = 2;
    let pipeline = ContextPipeline::new(policy)
        .unwrap()
        .with_summarizer(summarizer);
    let inputs = ContextInputs {
        history: tool_history(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let history = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    assert_eq!(
        history.compaction.as_ref().unwrap().watermark,
        9,
        "back from the tool result to its call"
    );
    let tail = verbatim_tail(&assembly);
    assert!(
        matches!(tail[0].role, Role::Assistant) && !tail[0].tool_calls.is_empty(),
        "the tail opens with the call: {tail:?}"
    );
    assert!(matches!(tail[1].role, Role::Tool));
    assert_eq!(
        tail[2].content.as_deref(),
        Some("answer 3 with some content")
    );
}

#[tokio::test]
async fn the_verbatim_window_is_counted_in_steps() {
    // Two steps kept: the last question with its call, result and answer
    // — four messages, more than the two-message floor.
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["earlier steps, summarized"]));
    let mut policy = compacting_policy();
    policy.compaction.as_mut().unwrap().keep_recent_messages = 2;
    policy.compaction.as_mut().unwrap().keep_recent_steps = 3;
    let pipeline = ContextPipeline::new(policy)
        .unwrap()
        .with_summarizer(summarizer);
    let inputs = ContextInputs {
        history: tool_history(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let history = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    assert_eq!(
        history.compaction.as_ref().unwrap().watermark,
        8,
        "three steps back: the last question"
    );
    let tail = verbatim_tail(&assembly);
    assert_eq!(tail.len(), 4);
    assert_eq!(
        tail[0].content.as_deref(),
        Some("user question 3 with some content")
    );
}

#[tokio::test]
async fn the_verbatim_tail_fits_the_history_budget() {
    // Three steps kept would be eight messages; a 192-token history with
    // a 128-token summary bound leaves 64 for the tail — about three
    // messages. The tail is what fits, and it opens at a step boundary.
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["earlier steps, summarized"]));
    let mut policy = compacting_policy();
    policy.history = Some(SectionPolicy::new(192));
    policy.compaction.as_mut().unwrap().keep_recent_messages = 2;
    policy.compaction.as_mut().unwrap().keep_recent_steps = 3;
    let counter = EstimatedTokenCounter::new(policy.budget.margin_percent);
    let pipeline = ContextPipeline::new(policy)
        .unwrap()
        .with_summarizer(summarizer);
    let inputs = ContextInputs {
        history: tool_history(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let history = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    let watermark = history.compaction.as_ref().unwrap().watermark;
    assert!(
        watermark > 8,
        "the tail is what fits, not the three steps asked: watermark {watermark}"
    );
    let tail = verbatim_tail(&assembly);
    let tail_tokens: u32 = tail
        .iter()
        .map(|m| counter.count(std::slice::from_ref(m), ""))
        .sum();
    // The tail is what fits, kept whole to its step: at most the room plus
    // the step's own call.
    assert!(tail_tokens <= 64 + 40, "tail costs {tail_tokens}");
    assert!(
        !matches!(tail[0].role, Role::Tool),
        "the tail opens at a step boundary: {tail:?}"
    );
    assert!(
        !history.truncated,
        "nothing is truncated once the tail fits"
    );
}

#[tokio::test]
async fn an_oversized_tool_result_is_clipped_with_a_notice() {
    // A 4,000-char tool result against a 1,024-token history: cut to half
    // the budget, the head kept, the notice naming the bytes it had; the
    // history is not truncated and the manifest counts the clip.
    let mut policy = policy();
    policy.compaction = None;
    let pipeline = ContextPipeline::new(policy).unwrap();
    let big = "result line with some content; ".repeat(130);
    let history = vec![
        ChatMessage::user("look it up with some content"),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "call-1",
            "lookup",
            json!({ "q": 1 }),
        )]),
        ChatMessage::tool_result("call-1", big.clone()),
    ];
    let inputs = ContextInputs {
        history,
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    assert_eq!(report.clipped_tool_results, 1);
    assert!(
        !report.truncated,
        "the clipped result fits; nothing is dropped"
    );
    let tool = assembly
        .messages
        .iter()
        .find(|m| matches!(m.role, Role::Tool))
        .expect("the tool result is in the prompt");
    let content = tool.content.as_deref().unwrap();
    assert!(
        content.starts_with("result line with some content;"),
        "the head is kept"
    );
    assert!(
        content.contains(&format!("clipped from {} bytes", big.len())),
        "{content}"
    );
    assert!(content.len() < big.len() / 2);
}

#[tokio::test]
async fn the_tail_never_opens_with_a_tool_result() {
    // The last step's result is bigger than the room: the tail still
    // opens with the call, and the result is clipped rather than orphaned.
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["earlier steps, summarized"]));
    let mut policy = compacting_policy();
    policy.history = Some(SectionPolicy::new(320));
    policy.compaction.as_mut().unwrap().keep_recent_messages = 2;
    let pipeline = ContextPipeline::new(policy)
        .unwrap()
        .with_summarizer(summarizer);
    let mut history = tool_history();
    history.pop();
    let big = "a very long result line with some content; ".repeat(60);
    history.push(ChatMessage::tool_result("call-3", big));
    let inputs = ContextInputs {
        history,
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    assert_eq!(
        report.compaction.as_ref().unwrap().watermark,
        9,
        "back to the call: {report:?}"
    );
    let tail = verbatim_tail(&assembly);
    assert!(
        matches!(tail[0].role, Role::Assistant) && !tail[0].tool_calls.is_empty(),
        "{tail:?}"
    );
    assert_eq!(report.clipped_tool_results, 1);
    assert!(tail.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|c| c.contains("clipped from"))));
}

/// A model that records what it was asked, answering one line.
struct CapturingModel {
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    answer: String,
}

#[async_trait::async_trait]
impl ChatModel for CapturingModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(ChatResponse {
            message: ChatMessage::assistant(self.answer.clone()),
            model: None,
            usage: None,
        })
    }
}

fn stored(history: &[ChatMessage], watermark: usize, summary: &str) -> StoredSummary {
    StoredSummary {
        watermark,
        summary: summary.to_owned(),
        prefix_hash: StoredSummary::prefix_hash(&history[..watermark]),
    }
}

#[tokio::test]
async fn a_stored_summary_is_reused_when_the_watermark_holds() {
    // No summariser at all: the trigger fires, the stored summary covers
    // exactly the compacted prefix, and no call is made.
    let history = long_history();
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_prior_summary(stored(&history, 4, "kept from last step"));
    let inputs = ContextInputs {
        history: history.clone(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap()
        .compaction
        .clone()
        .unwrap();
    assert_eq!(report.watermark, 4);
    assert!(report.reused_summary);
    assert!(assembly.messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|c| c.starts_with(SUMMARY_MARKER) && c.contains("kept from last step"))));
    assert_eq!(
        assembly.stored_summary,
        Some(stored(&history, 4, "kept from last step"))
    );
}

#[tokio::test]
async fn a_stored_summary_is_revised_with_the_turns_since() {
    // The stored summary covers two messages; the watermark is four: the
    // summariser is asked once, with the summary so far and only the two
    // turns since — never the whole prefix again.
    let history = long_history();
    let model = Arc::new(CapturingModel {
        requests: Mutex::new(Vec::new()),
        answer: "revised".into(),
    });
    let summarizer: Arc<dyn ChatModel> = model.clone();
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer)
        .with_prior_summary(stored(&history, 2, "the first two turns"));
    let inputs = ContextInputs {
        history: history.clone(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap()
        .compaction
        .clone()
        .unwrap();
    assert_eq!(
        (report.watermark, report.reused_summary, report.delta_from),
        (4, false, Some(2))
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let transcript = requests[0][1].content.as_deref().unwrap();
    assert!(
        transcript.contains("SUMMARY SO FAR (covering messages 1..=2"),
        "{transcript}"
    );
    assert!(transcript.contains("the first two turns"));
    assert!(
        transcript.contains("user turn 3") && transcript.contains("user turn 4"),
        "{transcript}"
    );
    assert!(
        !transcript.contains("user turn 1 with"),
        "the covered turns are not sent again: {transcript}"
    );
    assert_eq!(
        assembly
            .stored_summary
            .as_ref()
            .map(|s| (s.watermark, s.summary.as_str())),
        Some((4, "revised"))
    );
}

#[tokio::test]
async fn the_stored_summary_is_bounded_and_the_delta_clips_tool_results() {
    // A summariser that writes 6,000 chars: the prompt's summary fits its
    // 128-token bound and the stored one stays under twice that. The turns
    // since carry a 4,000-char tool result: the summariser sees it clipped.
    let history = {
        let mut h = long_history();
        h.push(ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "call-9",
            "lookup",
            json!({ "q": 9 }),
        )]));
        h.push(ChatMessage::tool_result(
            "call-9",
            "result line with some content; ".repeat(130),
        ));
        h.push(ChatMessage::user("and then with some content"));
        h.push(ChatMessage::user("one more with some content"));
        h
    };
    let long = "a decision about every item, ".repeat(200);
    let model = Arc::new(CapturingModel {
        requests: Mutex::new(Vec::new()),
        answer: long.clone(),
    });
    let summarizer: Arc<dyn ChatModel> = model.clone();
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer)
        .with_prior_summary(stored(&history, 4, "the first four turns"));
    let inputs = ContextInputs {
        history: history.clone(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let kept = assembly.stored_summary.as_ref().unwrap();
    assert!(
        kept.summary.len() < long.len() / 4,
        "stored {} of {} chars",
        kept.summary.len(),
        long.len()
    );
    assert!(kept.summary.len() > 300);
    let transcript = model.requests.lock().unwrap()[0][1]
        .content
        .clone()
        .unwrap();
    assert!(
        transcript.contains("clipped from"),
        "the summariser sees the result clipped: {}",
        transcript.len()
    );
    assert!(transcript.contains("under about"));
}

#[tokio::test]
async fn a_stored_summary_for_a_changed_prefix_starts_over() {
    // The thread was rewound: the stored summary's hash no longer matches
    // the leading messages, so the whole prefix is summarised afresh.
    let history = long_history();
    let mut other = history.clone();
    other[0] = ChatMessage::user("a different opening turn with some content");
    let model = Arc::new(CapturingModel {
        requests: Mutex::new(Vec::new()),
        answer: "fresh".into(),
    });
    let summarizer: Arc<dyn ChatModel> = model.clone();
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer)
        .with_prior_summary(stored(&other, 2, "stale"));
    let inputs = ContextInputs {
        history,
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap()
        .compaction
        .clone()
        .unwrap();
    assert_eq!((report.reused_summary, report.delta_from), (false, None));
    let transcript = model.requests.lock().unwrap()[0][1]
        .content
        .clone()
        .unwrap();
    assert!(!transcript.contains("SUMMARY SO FAR"));
    assert!(transcript.contains("user turn 1 with"));
}

#[tokio::test]
async fn a_clipped_summary_tells_the_model_so() {
    let long = "a decision was taken about every one of the forty items, ".repeat(40);
    let summarizer: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec![long.as_str()]));
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer);
    let inputs = ContextInputs {
        history: long_history(),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let history = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .unwrap();
    let report = history.compaction.as_ref().unwrap();
    assert!(report.summary_truncated);
    assert!(report.summary_tokens <= 128);
    let summary = assembly
        .messages
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with(SUMMARY_MARKER))
        })
        .unwrap();
    assert!(
        summary
            .content
            .as_deref()
            .unwrap()
            .ends_with(SUMMARY_CLIPPED_NOTICE),
        "{summary:?}"
    );
}

#[tokio::test]
async fn compaction_trigger_without_a_summarizer_fails_loudly() {
    let pipeline = ContextPipeline::new(compacting_policy()).unwrap();
    let inputs = ContextInputs {
        history: long_history(),
        ..golden_inputs()
    };
    let journal = Journal::new("run-context-test", "t-context", logical_clock());
    let memory = JournaledMemory::new(&journal, MemorySource::Store(store_with_records().await));
    let error = pipeline.assemble(&inputs, Some(&memory)).await.unwrap_err();
    assert!(
        matches!(error, RustyError::InvalidUpdate(_)),
        "a fired trigger without a summarizer is a configuration error, got {error:?}"
    );
}

#[tokio::test]
async fn an_unenforceable_summary_bound_is_a_configuration_error() {
    // The generated-marker framing alone exceeds the bound: no truncation
    // can satisfy it, so the pipeline fails loud rather than reporting a
    // manifest whose summary_tokens silently violate the policy.
    let mut policy = compacting_policy();
    policy.compaction.as_mut().unwrap().summary_max_tokens = 1;
    let summarizer: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["earlier turns, summarized"]));
    let pipeline = ContextPipeline::new(policy)
        .unwrap()
        .with_summarizer(summarizer);
    let inputs = ContextInputs {
        history: long_history(),
        ..golden_inputs()
    };
    let journal = Journal::new("run-context-test", "t-context", logical_clock());
    let memory = JournaledMemory::new(&journal, MemorySource::Store(store_with_records().await));
    let error = pipeline.assemble(&inputs, Some(&memory)).await.unwrap_err();
    assert!(
        matches!(error, RustyError::InvalidUpdate(_)),
        "an unenforceable summary bound is a configuration error, got {error:?}"
    );
}

#[tokio::test]
async fn compaction_stays_silent_below_the_trigger() {
    // No summarizer: proves the trigger never fires for the short history.
    let pipeline = ContextPipeline::new(compacting_policy()).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;
    let history = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .expect("history section report");
    assert!(history.compaction.is_none());
}

// ---------- the learn-plane delta ----------

#[test]
fn context_policy_candidates_join_the_pipeline() {
    let candidate = Candidate::new(
        CandidateContent::ContextPolicy {
            name: "default".into(),
            policy: policy().to_value().unwrap(),
        },
        ProvenanceAuthor::Distiller {
            name: "context-distiller".into(),
        },
        EvidenceSpan::default(),
        ts(1_750_000_010_000),
    )
    .unwrap();
    assert_eq!(candidate.kind(), CandidateKind::ContextPolicy);
    assert_eq!(candidate.kind().as_str(), "context_policy");
    assert_eq!(
        candidate.surface(),
        surface_for_kind(CandidateKind::ContextPolicy, "default")
    );
    assert_eq!(candidate.surface().to_string(), "context:default");
    // The wave-1 envelope answer: approval, always (the semantic blast
    // radius the registry kinds already price).
    assert_eq!(
        PromotionEnvelope::r08_default().rule_for(CandidateKind::ContextPolicy),
        &EnvelopeRule::Approval
    );
    candidate.verify_address().unwrap();
}

#[test]
fn context_policy_parses_fail_closed() {
    let value = policy().to_value().unwrap();
    let parsed = ContextPolicy::from_value(&value).unwrap();
    assert_eq!(parsed, policy());

    let mut wrong_version = value;
    wrong_version["schema_version"] = json!("context-policy-v0");
    assert!(ContextPolicy::from_value(&wrong_version).is_err());
}

// ---------- the wave-1 exit criterion: exact replay of a compacted run ----------

const RUN_ID: &str = "run-context-replay";
const THREAD_ID: &str = "t-context-replay";

fn short_history() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("hello"),
        ChatMessage::assistant("hi there"),
        ChatMessage::user("what do you know about me?"),
    ]
}

/// The AssemblingChatModel half that record and replay share: pipeline plus
/// the pinned-at-admission inputs. The inner model differs per mode (the
/// evidence wrapper sits inside the assembler, so the journaled `ModelCall`
/// input is the assembled request).
fn assembling_around(
    inner: Arc<dyn ChatModel>,
    pipeline: ContextPipeline,
    memory: JournaledMemory,
) -> AssemblingChatModel {
    AssemblingChatModel::new(inner, pipeline)
        .with_identity("You are Rusty, a governed agent runtime test double.")
        .with_task("Summarize the user's preferences.")
        .with_skills(vec![skill_entry()])
        .with_memory(memory)
}

/// Record one pipeline-assembled run: two model calls, the second with a
/// history long enough to fire the compaction trigger. Returns the journal
/// snapshot.
async fn record_compacted_run() -> JournalSnapshot {
    let journal = Journal::new(RUN_ID, THREAD_ID, logical_clock());
    let memory = JournaledMemory::new(&journal, MemorySource::Store(store_with_records().await));

    // The per-mode wiring the design fixes: the summarizer slot is wrapped
    // exactly as the run's own model — recording mode journals it under the
    // static pipeline parent.
    let summarizer: Arc<dyn ChatModel> = Arc::new(RecordingChatModel::new(
        Arc::new(ScriptedModel::new(vec!["earlier turns, summarized"])),
        journal.clone(),
        CONTEXT_PIPELINE_PARENT,
    ));
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer)
        .with_policy_pin("test-policy", None, None);
    let main: Arc<dyn ChatModel> =
        Arc::new(ScriptedModel::new(vec!["first answer", "second answer"]));

    let tools = vec![echo_schema(), search_schema()];
    for (invocation, history) in [short_history(), long_history()].into_iter().enumerate() {
        // Per invocation, exactly as react builds its wrapper: the recording
        // wrapper around the real model carries the invocation's causal
        // parent; the assembler runs the pipeline and forwards the assembly.
        let inner: Arc<dyn ChatModel> = Arc::new(RecordingChatModel::new(
            main.clone(),
            journal.clone(),
            format!("{RUN_ID}:agent:{invocation}"),
        ));
        let assembling = assembling_around(inner, pipeline.clone(), memory.clone());
        assembling.chat(&history, &tools).await.unwrap();
    }

    journal.snapshot()
}

/// An event's resolved payload (inline, or looked through the artifact map).
fn resolve(snapshot: &JournalSnapshot, payload: &PayloadRef) -> Value {
    match payload {
        PayloadRef::Inline(value) => value.clone(),
        PayloadRef::Artifact(reference) => snapshot
            .artifacts
            .get(&reference.sha256)
            .cloned()
            .unwrap_or_else(|| panic!("dangling artifact reference {}", reference.sha256)),
    }
}

fn events_of_kind(snapshot: &JournalSnapshot, kind: RunEventKind) -> Vec<&RunEvent> {
    snapshot
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .collect()
}

/// The structured manifest out of a journaled `ModelCall` request: the
/// assembled messages' reserved manifest message.
fn manifest_of(request: &Value) -> SectionManifest {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .expect("model call request carries messages");
    let manifest_message = messages
        .iter()
        .find(|m| m.get("name").and_then(Value::as_str) == Some(MANIFEST_MESSAGE_NAME))
        .expect("assembly carries the manifest message");
    let content = manifest_message
        .get("content")
        .and_then(Value::as_str)
        .unwrap();
    serde_json::from_str(content.strip_prefix("context-manifest-v1\n").unwrap()).unwrap()
}

#[tokio::test]
async fn exact_replay_of_a_compacted_run_serves_every_call() {
    let snapshot = record_compacted_run().await;

    // The recorded run journaled three model calls: two assembled, one
    // compaction summarization — the summarization parented to the static
    // pipeline marker and preceding the assembled call it fed.
    let model_calls = events_of_kind(&snapshot, RunEventKind::ModelCall);
    assert_eq!(model_calls.len(), 3);
    assert_eq!(
        model_calls[1].parent.as_deref(),
        Some(CONTEXT_PIPELINE_PARENT)
    );
    let recorded_manifest =
        manifest_of(&resolve(&snapshot, model_calls[2].input.as_ref().unwrap()));
    let recorded_compaction = recorded_manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::History)
        .and_then(|s| s.compaction.clone())
        .expect("the second assembled call compacted");
    assert_eq!(recorded_compaction.watermark, 4);

    // Replay: panic sentinels behind replaying wrappers over the run's own
    // shared ReplaySource — the compaction call is one more journaled
    // ModelCall in the stream, served in order.
    let replay = ExactReplay::new(snapshot.clone()).unwrap();
    let rjournal = replay.fresh_journal(logical_clock());
    let source = replay.source();
    let memory_source = MemoryReplaySource::new(&snapshot);
    let memory = JournaledMemory::new(&rjournal, MemorySource::Replay(memory_source.clone()));

    let summarizer_calls = Arc::new(AtomicUsize::new(0));
    let main_calls = Arc::new(AtomicUsize::new(0));
    let summarizer: Arc<dyn ChatModel> = Arc::new(ReplayingChatModel::new(
        Arc::new(PanicModel {
            calls: summarizer_calls.clone(),
        }),
        source.clone(),
        rjournal.clone(),
        CONTEXT_PIPELINE_PARENT,
    ));
    let pipeline = ContextPipeline::new(compacting_policy())
        .unwrap()
        .with_summarizer(summarizer)
        .with_policy_pin("test-policy", None, None);

    let tools = vec![echo_schema(), search_schema()];
    for (invocation, history) in [short_history(), long_history()].into_iter().enumerate() {
        let inner: Arc<dyn ChatModel> = Arc::new(ReplayingChatModel::new(
            Arc::new(PanicModel {
                calls: main_calls.clone(),
            }),
            source.clone(),
            rjournal.clone(),
            format!("{RUN_ID}:agent:{invocation}"),
        ));
        let assembling = assembling_around(inner, pipeline.clone(), memory.clone());
        assembling.chat(&history, &tools).await.unwrap();
    }

    // Zero outbound calls, both cursors exhausted.
    assert_eq!(summarizer_calls.load(Ordering::SeqCst), 0);
    assert_eq!(main_calls.load(Ordering::SeqCst), 0);
    assert!(
        source.is_exhausted(),
        "unserved effects: {:?}",
        source.remaining()
    );
    assert!(memory_source.is_exhausted());

    // The replayed journal reproduces the recorded evidence byte-for-byte —
    // the trigger re-fired at the same watermark, the summary was served
    // from the journal, and the assembled request hash-matched the recorded
    // ModelCall it precedes (the serve would have failed otherwise).
    let replayed = rjournal.snapshot();
    assert_eq!(snapshot.events, replayed.events);
    assert_eq!(snapshot.head_hash, replayed.head_hash);

    let replayed_calls = events_of_kind(&replayed, RunEventKind::ModelCall);
    let replayed_manifest = manifest_of(&resolve(
        &replayed,
        replayed_calls[2].input.as_ref().unwrap(),
    ));
    assert_eq!(
        replayed_manifest
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::History)
            .and_then(|s| s.compaction.clone()),
        Some(recorded_compaction)
    );
}

/// Lane-one recall: the notes that bear on the turn's last message render
/// after the history and before the manifest, the memory read that chose
/// them carries the message as its query text, and a note that does not
/// bear on the message is not among them.
#[tokio::test]
async fn lane_one_recall_renders_the_bearing_notes_after_the_history() {
    let mut policy = policy().with_recall(400, 5);
    policy.budget = rusty_agent_runtime::memory::ContextBudget::new(100_000);
    let pipeline = ContextPipeline::new(policy).unwrap();
    let mut inputs = golden_inputs();
    inputs.history.push(ChatMessage::user(
        "which language should I write in for you?",
    ));
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let messages = &assembly.messages;
    let recall_at = messages
        .iter()
        .position(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("# Recalled for this turn"))
        })
        .expect("a recall section");
    let last_history = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .expect("history");
    assert!(
        recall_at > last_history,
        "recall follows the history: {messages:#?}"
    );
    assert_eq!(
        recall_at,
        messages.len() - 2,
        "recall sits just before the manifest"
    );
    let body = messages[recall_at].content.as_deref().unwrap();
    assert!(
        body.contains(&language_record().memory_id),
        "the language note bears on the message: {body}"
    );
    assert!(
        !body.contains(&timezone_record().memory_id),
        "the timezone note does not: {body}"
    );
    let report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Recall)
        .expect("a recall report");
    assert_eq!(report.ids, vec![language_record().memory_id]);
}

#[tokio::test]
async fn a_small_tools_section_keeps_every_tool_in_its_short_form() {
    // Eight tools whose descriptions, written for the model, run long: at
    // full length only two or three fit the section. The agent must keep
    // all eight, each by its first sentence, rather than the first few.
    let mut registry = ToolRegistry::new();
    let long_tail = " It also explains, at length, every edge case, every field it returns and how to page through them.".repeat(8);
    for i in 1..=8u32 {
        registry.register(ToyTool {
            name: format!("tool_{i:02}"),
            description: format!("Look up thing number {i}.{long_tail}"),
            effect: Effect::Idempotent,
        });
    }
    let manifests = manifests_for_registry(&registry, &std::collections::BTreeMap::new()).unwrap();
    let mut policy = policy();
    policy.tools = Some(
        ToolsSectionPolicy::new(512).with_selection(ToolSelectionPolicy {
            cutoff: 20,
            k: 8,
            ..Default::default()
        }),
    );
    let pipeline = ContextPipeline::new(policy).unwrap();
    let inputs = ContextInputs {
        tool_manifests: manifests,
        effect_ceiling: Some(Effect::Idempotent),
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;

    assert_eq!(assembly.tools.len(), 8, "every tool is kept");
    let section = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Tools)
        .expect("tools section report");
    assert_eq!(section.tools_shortened, 8, "{section:?}");
    for tool in &assembly.tools {
        let description = tool
            .pointer("/function/description")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(
            description.starts_with("Look up thing number") && !description.contains("edge case"),
            "{description}"
        );
        assert!(
            tool.pointer("/function/parameters/properties/q/type")
                .is_some(),
            "the parameters stay callable: {tool}"
        );
    }
}

#[tokio::test]
async fn a_history_cut_to_its_budget_still_carries_the_question() {
    // No compaction; a history section too small for the whole turn. The
    // budget cuts the oldest steps, and the person's question with them —
    // unless its room is taken first. It must reach the model, ahead of the
    // newest steps, and no step may open with an orphaned tool result.
    let mut policy = policy();
    policy.compaction = None;
    policy.history = Some(SectionPolicy::new(160));
    let pipeline = ContextPipeline::new(policy).unwrap();
    let mut history = vec![ChatMessage::user(
        "My laptop will not join the office Wi-Fi since this morning.",
    )];
    for i in 1..=8 {
        history.push(ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            format!("call-{i}"),
            "lookup",
            json!({ "q": i }),
        )]));
        history.push(ChatMessage::tool_result(
            format!("call-{i}"),
            format!("result {i}: a few lines of what the lookup returned for this step"),
        ));
    }
    let inputs = ContextInputs {
        history,
        ..golden_inputs()
    };
    let assembly = assemble_with_store(&pipeline, &inputs).await;
    let conversation: Vec<&ChatMessage> = assembly
        .messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .collect();
    assert!(
        matches!(conversation[0].role, Role::User)
            && conversation[0]
                .content
                .as_deref()
                .is_some_and(|c| c.contains("Wi-Fi")),
        "the question leads what the model sees: {conversation:?}"
    );
    assert!(
        !matches!(conversation[1].role, Role::Tool),
        "no orphaned tool result after the question: {conversation:?}"
    );
    assert!(
        conversation.len() < 17,
        "the budget still cut the older steps"
    );
}
