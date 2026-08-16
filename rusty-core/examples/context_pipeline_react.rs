//! The context pipeline consumed from ReAct (R0.13 wave 1): the composition
//! recipe the design fixes, end to end — governed tool selection wired into
//! assembly, the summarizer slot for mid-run compaction, and the assembling
//! `ChatModel` wrapper that lets `create_react_agent` receive the pipeline
//! without `react.rs` ever knowing.
//!
//! What the demo shows, in construction order:
//!
//! 1. **Manifests + overlays** — selection metadata derived from the
//!    registry's executable contracts, governed tags applied per tool.
//! 2. **Admission-time narrowing** — one shortlist over the full registry
//!    pins the run's tool set; `ToolRegistry::restricted_to` builds the
//!    narrowed registry the executor dispatches against.
//! 3. **Validated calling** — `ValidatingTool::wrap_registry` wraps the
//!    narrowed registry so malformed arguments are refused before dispatch.
//! 4. **The assembling wrapper** — `AssemblingChatModel` runs the pipeline
//!    over every call: the tools section re-shortlists under the policy per
//!    assembly, the history section compacts when its trigger fires
//!    (summarizer slot journaled under `CONTEXT_PIPELINE_PARENT`), and the
//!    journaled `ModelCall` input *is* the assembled request.
//!
//! Run with: `cargo run --example context_pipeline_react`

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::context::{
    AssemblingChatModel, CompactionPolicy, ContextPipeline, ContextPolicy, SectionManifest,
    SectionPolicy, ToolsSectionPolicy, CONTEXT_PIPELINE_PARENT, CONTEXT_POLICY_SCHEMA_VERSION,
    MANIFEST_MESSAGE_NAME,
};
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::tool_select::{
    manifests_for_registry, shortlist, SelectionFeatures, ToolSelectionOverlay,
    ToolSelectionPolicy, ValidatingTool,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Determinism parameters and the scripted model.
// ---------------------------------------------------------------------------

const CLOCK_START_MS: u64 = 1_700_000_000_000;
const CLOCK_TICK_MS: u64 = 10;
const RNG_SEED: u64 = 7;

struct ScriptedModel {
    responses: Mutex<VecDeque<ChatMessage>>,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let message = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RustyError::Llm("script exhausted".into()))?;
        Ok(ChatResponse {
            message,
            model: Some("scripted-mock-1".into()),
            usage: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tools: two reads and one write, so the effect ceiling has something to cut.
// ---------------------------------------------------------------------------

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes back the given text."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        Ok(json!(text))
    }
}

struct Search;

#[async_trait]
impl Tool for Search {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Searches the index for the given query."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        println!("    [tool:search] <- \"{query}\"");
        Ok(json!({"results": ["context pipeline: deterministic, budgeted, journaled"]}))
    }
}

struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Writes content to a file (irreversible)."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]})
    }
    // The default effect class — NonIdempotent — is what the run's
    // Idempotent ceiling excludes below.
    async fn call(&self, _args: Value) -> Result<Value> {
        Ok(json!({"written": true}))
    }
}

// ---------------------------------------------------------------------------
// The demo.
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Rusty Core: context pipeline inside ReAct ===\n");

    // -- The full registry and its governed overlays -------------------------
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    registry.register(Search);
    registry.register(WriteFile);
    let overlays = BTreeMap::from([
        (
            "echo".to_owned(),
            ToolSelectionOverlay {
                tags: vec!["utility".to_owned()],
                ..Default::default()
            },
        ),
        (
            "search".to_owned(),
            ToolSelectionOverlay {
                tags: vec!["search".to_owned()],
                when_to_use: Some("Look facts up before answering.".to_owned()),
                ..Default::default()
            },
        ),
    ]);
    let manifests = manifests_for_registry(&registry, &overlays)?;

    // The run's selection features: a search-flavored task, and an effect
    // ceiling of Idempotent — the write tool never reaches the model.
    let features = SelectionFeatures {
        task_tags: vec!["search".to_owned()],
        effect_ceiling: Effect::Idempotent,
        outcomes: BTreeMap::new(),
    };
    let selection_policy = ToolSelectionPolicy::default();

    // -- 1. admission-time narrowing -----------------------------------------
    // Shortlist once at admission and narrow the registry the executor
    // dispatches against: the run can only call what the shortlist selected.
    let admission = shortlist(&features, &manifests, &selection_policy);
    let selected: Vec<String> = admission.selected.iter().map(|r| r.name.clone()).collect();
    println!("admission shortlist: {selected:?}");
    for excluded in &admission.excluded {
        println!("  excluded: {} ({:?})", excluded.name, excluded.reason);
    }
    let narrowed = registry.restricted_to(&selected)?;

    // -- 2. validated calling -------------------------------------------------
    // Malformed arguments are refused before dispatch, with the structured
    // refusal payload the outcome roll-up parses.
    let validated = ValidatingTool::wrap_registry(&narrowed);

    // -- 3. the pipeline ------------------------------------------------------
    // The policy budgets the sections and pins the selection policy the
    // tools section re-runs per assembly; compaction keeps the growing
    // ReAct history inside its budget (the trigger is deliberately tiny so
    // the demo's second model call compacts).
    let policy = ContextPolicy {
        schema_version: CONTEXT_POLICY_SCHEMA_VERSION.to_owned(),
        budget: ContextBudget::new(4096),
        tokenizer: Default::default(),
        identity: Some(SectionPolicy::new(256)),
        task: Some(SectionPolicy::new(256)),
        skills: None,
        tools: Some(ToolsSectionPolicy::new(512).with_selection(selection_policy)),
        memory: None,
        history: Some(SectionPolicy::new(1024)),
        compaction: Some(CompactionPolicy {
            trigger_tokens: 96,
            keep_recent_messages: 2,
            summary_max_tokens: 128,
            prompt: "Summarize the conversation prefix, preserving decisions.".into(),
        }),
    };

    let journal = Journal::new(
        "run-context-demo",
        "context-pipeline-react",
        Clock::logical(CLOCK_START_MS, CLOCK_TICK_MS),
    );

    // The summarizer slot, wrapped per mode exactly as the run's own model:
    // recording mode journals the compaction call under the static pipeline
    // parent. Replay mode would wrap a panic sentinel in ReplayingChatModel
    // over the run's shared ReplaySource; unjournaled mode hands the bare
    // summarizer.
    let summarizer: Arc<dyn ChatModel> = Arc::new(RecordingChatModel::new(
        Arc::new(ScriptedModel {
            responses: Mutex::new(
                vec![ChatMessage::assistant("The user asked about the context pipeline.")]
                    .into(),
            ),
        }),
        journal.clone(),
        CONTEXT_PIPELINE_PARENT,
    ));
    let pipeline = ContextPipeline::new(policy)?
        .with_summarizer(summarizer)
        .with_policy_pin("context:demo", None, None);

    // -- 4. the assembling wrapper --------------------------------------------
    // The evidence wrapper sits INSIDE the assembler, so the journaled
    // ModelCall input is the assembled request. The per-call manifests the
    // pipeline shortlists are the narrowed registry's (the admission cut).
    let narrowed_manifests = manifests_for_registry(&narrowed, &overlays)?;
    let model: Arc<dyn ChatModel> = Arc::new(ScriptedModel {
        responses: Mutex::new(
            vec![
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "call_1",
                    "search",
                    json!({"query": "rusty context pipeline"}),
                )]),
                ChatMessage::assistant(
                    "The context pipeline assembles budgeted, journaled context.",
                ),
            ]
            .into(),
        ),
    });
    let inner: Arc<dyn ChatModel> = Arc::new(RecordingChatModel::new(
        model,
        journal.clone(),
        "run-context-demo:agent:0",
    ));
    let assembling = AssemblingChatModel::new(inner, pipeline)
        .with_identity("You are Rusty, a governed agent runtime demo.")
        .with_task("Answer the user's question about the context pipeline.")
        .with_tool_manifests(narrowed_manifests)
        .with_task_tags(features.task_tags.clone())
        .with_effect_ceiling(features.effect_ceiling);

    // The plain `create_react_agent` receives the assembler and never knows.
    // Note the run config carries NO journal: the pipeline's inner evidence
    // wrappers journal the assembled calls themselves. Attaching the journal
    // here AND using react's recording variant would wrap the assembler and
    // journal the pre-assembly request as well — double journaling breaks
    // the replay serving order (the assembled call must be the recorded
    // one). Pick one evidence composition; this recipe is it.
    let graph = create_react_agent(Arc::new(assembling), validated)?;

    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let initial = State::from_value(json!({
        "messages": [serde_json::to_value(ChatMessage::user(
            "what is the rusty context pipeline?"
        ))?]
    }))?;

    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            initial,
            RunConfig::new("context-pipeline-react").with_rng(RngSource::seeded(RNG_SEED)),
        )
        .await?;

    let messages: Vec<ChatMessage> = outcome
        .state()
        .get_as("messages")?
        .expect("messages channel");
    println!(
        "\nfinal answer: {:?}",
        messages.last().and_then(|m| m.content.as_deref())
    );

    // -- What the journal holds -----------------------------------------------
    let snapshot = journal.snapshot();
    println!("\njournaled {} event(s):", snapshot.events.len());
    for event in &snapshot.events {
        if matches!(event.kind, RunEventKind::ModelCall | RunEventKind::ToolCall) {
            println!(
                "  seq {:>2} {:?} parent={:?}",
                event.seq, event.kind, event.parent
            );
        }
    }

    // The section manifest out of each journaled assembled call: what every
    // section carried, at what cost — including the tools section's full
    // shortlist and the history section's compaction watermark.
    for event in snapshot
        .events
        .iter()
        .filter(|e| e.kind == RunEventKind::ModelCall)
    {
        let Some(PayloadRef::Inline(request)) = &event.input else {
            continue;
        };
        let Some(messages) = request.get("messages").and_then(Value::as_array) else {
            continue;
        };
        let Some(manifest_message) = messages.iter().find(|m| {
            m.get("name").and_then(Value::as_str) == Some(MANIFEST_MESSAGE_NAME)
        }) else {
            continue;
        };
        let content = manifest_message
            .get("content")
            .and_then(Value::as_str)
            .unwrap();
        let manifest: SectionManifest =
            serde_json::from_str(content.strip_prefix("context-manifest-v1\n").unwrap())?;
        println!("\n  assembled call (seq {}):", event.seq);
        for section in &manifest.sections {
            println!(
                "    {:>8}: used {:>4} / budget {:<4} ids={:?}{}{}",
                section.kind.as_str(),
                section.used_tokens,
                section.budget_tokens,
                section.ids,
                if section.truncated { " [truncated]" } else { "" },
                section
                    .compaction
                    .as_ref()
                    .map(|c| format!(" [compacted: watermark {}]", c.watermark))
                    .unwrap_or_default()
            );
            if let Some(shortlist) = &section.shortlist {
                let selected: Vec<&str> =
                    shortlist.selected.iter().map(|r| r.name.as_str()).collect();
                println!(
                    "             shortlist: selected={selected:?} ranked={} excluded={}",
                    shortlist.ranking.len(),
                    shortlist.excluded.len()
                );
            }
        }
    }
    Ok(())
}
