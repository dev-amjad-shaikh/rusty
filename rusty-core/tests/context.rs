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
    SkillSectionEntry, TokenCounter, CONTEXT_PIPELINE_PARENT, CONTEXT_POLICY_SCHEMA_VERSION,
    MANIFEST_MESSAGE_NAME, SUMMARY_MARKER,
};
use rusty_agent_runtime::error::{Result as RustyResult, RustyError};
use rusty_agent_runtime::journal::{Clock, Journal, JournalSnapshot};
use rusty_agent_runtime::learn::{
    surface_for_kind, Candidate, CandidateContent, CandidateKind, EnvelopeRule, EvidenceSpan,
    PromotionEnvelope,
};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::memory::{
    estimated_tokens, InMemoryMemoryStore, JournaledMemory, MemoryKind, MemoryProvenance,
    MemoryQuery, MemoryRecord, MemoryReplaySource, MemoryScope, MemorySource, MemoryStore,
    ProvenanceAuthor, ScopeAddress, ValidityWindow, DEFAULT_TOKEN_MARGIN_PERCENT,
};
use rusty_agent_runtime::record::{PayloadRef, RunEvent, RunEventKind};
use rusty_agent_runtime::replay::{
    ExactReplay, RecordingChatModel, ReplayingChatModel,
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
        tools: Some(SectionPolicy::new(512)),
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
            summary_max_tokens: 128,
            prompt: "Summarize the conversation prefix, preserving decisions.".into(),
        }),
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
    let pipeline = ContextPipeline::new(policy())
        .unwrap()
        .with_policy_pin("test-policy", None, None);
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;

    // The manifest is the reserved, model-visible metadata message riding
    // directly behind identity.
    let manifest_message = &assembly.messages[1];
    assert_eq!(manifest_message.name.as_deref(), Some(MANIFEST_MESSAGE_NAME));
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
}

#[tokio::test]
async fn the_manifest_message_is_budgeted_off_the_top() {
    let pipeline = ContextPipeline::new(policy()).unwrap();
    let assembly = assemble_with_store(&pipeline, &golden_inputs()).await;
    let manifest = &assembly.manifest;

    assert!(manifest.manifest_tokens > 0);
    let total: u32 = manifest.manifest_tokens
        + manifest.sections.iter().map(|s| s.used_tokens).sum::<u32>();
    assert!(
        total <= manifest.budget_tokens,
        "manifest plus sections ({total}) must fit the total budget ({})",
        manifest.budget_tokens
    );
    assert_eq!(manifest.counter, "estimated");
    assert_eq!(manifest.policy.name, "inline");

    // The manifest message embeds the same structured manifest.
    let content = assembly.messages[1].content.as_deref().unwrap();
    let parsed: SectionManifest =
        serde_json::from_str(content.strip_prefix("context-manifest-v1\n").unwrap()).unwrap();
    assert_eq!(&parsed, manifest);
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
        summary_max_tokens: 128,
        prompt: "Summarize the conversation prefix, preserving decisions.".into(),
    });
    policy
}

#[tokio::test]
async fn compaction_fires_at_the_trigger_and_marks_the_summary() {
    let summarizer: Arc<dyn ChatModel> = Arc::new(ScriptedModel::new(vec!["earlier turns, summarized"]));
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
    let summary = &assembly.messages[assembly.messages.len() - 3];
    assert!(
        summary
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with(SUMMARY_MARKER)),
        "the summary message is marked as generated: {summary:?}"
    );
    let tail = &assembly.messages[assembly.messages.len() - 2..];
    assert_eq!(
        tail[0].content.as_deref(),
        Some("user turn 5 with some content")
    );
    assert_eq!(
        tail[1].content.as_deref(),
        Some("user turn 6 with some content")
    );
    assert_eq!(inputs.history.len(), 6, "compaction never mutates the channel");
    assert_eq!(
        inputs.history[0].content.as_deref(),
        Some("user turn 1 with some content")
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
    let recorded_manifest = manifest_of(&resolve(&snapshot, model_calls[2].input.as_ref().unwrap()));
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
    assert!(source.is_exhausted(), "unserved effects: {:?}", source.remaining());
    assert!(memory_source.is_exhausted());

    // The replayed journal reproduces the recorded evidence byte-for-byte —
    // the trigger re-fired at the same watermark, the summary was served
    // from the journal, and the assembled request hash-matched the recorded
    // ModelCall it precedes (the serve would have failed otherwise).
    let replayed = rjournal.snapshot();
    assert_eq!(snapshot.events, replayed.events);
    assert_eq!(snapshot.head_hash, replayed.head_hash);

    let replayed_calls = events_of_kind(&replayed, RunEventKind::ModelCall);
    let replayed_manifest =
        manifest_of(&resolve(&replayed, replayed_calls[2].input.as_ref().unwrap()));
    assert_eq!(
        replayed_manifest
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::History)
            .and_then(|s| s.compaction.clone()),
        Some(recorded_compaction)
    );
}

