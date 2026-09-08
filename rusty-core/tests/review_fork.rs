//! The post-turn background review fork (EP-07-S04): substance gating and
//! the warm/digest replay plan, the confined dispatch surface with its
//! cancellation boundary, a fork run journaling `side` stamps with fork
//! attribution, and the guarded ledger write (patch-before-create, `Trial`,
//! fork attribution).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::executor::{ExecutionOutcome, Executor, RunConfig};
use rusty_agent_runtime::journal::Clock;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::memory::{
    InMemoryMemoryStore, MemoryQuery, MemoryScope, MemoryStore, ScopeAddress,
};
use rusty_agent_runtime::memory_tools::{MEMORY_APPEND_ENTRY_TOOL, MemoryToolset};
use rusty_agent_runtime::prelude::{TrafficClass, TurnBoundary, TurnStamp};
use rusty_agent_runtime::react::{MESSAGES_CHANNEL, create_react_agent_with_recording};
use rusty_agent_runtime::record::{RunEventKind, RunManifest};
use rusty_agent_runtime::review_fork::{
    ForkSkillWrite, ForkWriteError, REVIEW_BOUNDARY_KIND, REVIEW_FORK_COMPONENT, ReplayMaterial,
    ReviewBoundaryTool, ReviewForkConfig, Substance, SubstanceRule, assess_substance,
    commit_skill_write, compact_digest, fork_dispatch_registry, plan_review, prompt_prefix_hash,
};
use rusty_agent_runtime::skill::{SkillPackage, SkillPromotionStatus, SkillRegistry, SkillSource};
use rusty_agent_runtime::skill_editorial::{EditorialProvenance, PatchPreference};
use rusty_agent_runtime::state::{Reducer, State, StateSpec};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

fn spec() -> StateSpec {
    StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages)
}

fn initial_state(messages: &[ChatMessage]) -> State {
    let messages: Vec<Value> = messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap())
        .collect();
    State::from_value(json!({ MESSAGES_CHANNEL: messages })).unwrap()
}

/// A scripted model recording the stamp each dispatch carried; optionally
/// cancels the fork token when `cancel_on_call` fires.
#[derive(Default)]
struct ForkProbe {
    script: Mutex<VecDeque<ChatMessage>>,
    stamps: Mutex<Vec<TurnStamp>>,
    cancel: Mutex<Option<(usize, CancellationToken)>>,
}

impl ForkProbe {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            ..Self::default()
        }
    }

    /// Cancel `token` when the model's `n`th (1-based) dispatch arrives —
    /// the scheduler's "live traffic resumed" signal mid-review.
    fn cancel_on_call(self, n: usize, token: CancellationToken) -> Self {
        *self.cancel.lock().unwrap() = Some((n, token));
        self
    }

    fn stamps(&self) -> Vec<TurnStamp> {
        self.stamps.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ChatModel for ForkProbe {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        panic!("a stamped fork run must never touch the plain dispatch path")
    }

    async fn chat_stamped(
        &self,
        stamp: &TurnStamp,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> RustyResult<ChatResponse> {
        self.stamps.lock().unwrap().push(stamp.clone());
        let mut cancel = self.cancel.lock().unwrap();
        if let Some((n, token)) = &*cancel {
            if self.stamps.lock().unwrap().len() == *n {
                token.cancel();
                *cancel = None;
            }
        }
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ChatMessage::assistant("done"));
        Ok(ChatResponse {
            message,
            model: None,
            usage: None,
        })
    }
}

/// A stand-in skill tool (read-only lookup).
struct SkillLookupTool;

#[async_trait::async_trait]
impl Tool for SkillLookupTool {
    fn name(&self) -> &str {
        "skill_lookup"
    }

    fn description(&self) -> &str {
        "looks up an active skill's body"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"name": {"type": "string"}}})
    }

    async fn call(&self, arguments: Value) -> RustyResult<Value> {
        Ok(json!({"skill": arguments["name"]}))
    }
}

/// A live-traffic tool the fork must never reach.
struct SendMessageTool;

#[async_trait::async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "sends a message to the user"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }

    async fn call(&self, _arguments: Value) -> RustyResult<Value> {
        panic!("the fork must never dispatch live-traffic tools")
    }
}

fn parent_registry(store: Arc<InMemoryMemoryStore>) -> ToolRegistry {
    let toolset = MemoryToolset::new(
        store,
        "support-1",
        ScopeAddress::new(MemoryScope::Agent, "support-1"),
        Clock::logical(1_700_000_000_000, 1_000),
    );
    let mut registry = toolset.registry();
    registry.register(SkillLookupTool);
    registry.register(SendMessageTool);
    registry
}

fn parent_turn(projection: Vec<ChatMessage>) -> rusty_agent_runtime::review_fork::ParentTurn {
    let mut prompts = BTreeMap::new();
    prompts.insert("system".to_owned(), "abc123prefix".to_owned());
    rusty_agent_runtime::review_fork::ParentTurn {
        thread_id: "thread-9".to_owned(),
        session_id: uuid::Uuid::from_u128(0xa11ce),
        turn_id: uuid::Uuid::from_u128(0x7),
        model: "gpt-5.2-2026-06-01".to_owned(),
        manifest: RunManifest {
            prompts,
            ..RunManifest::default()
        },
        projection,
        learn_invoked: false,
    }
}

fn substantive_projection() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("why does the vpn keep dropping?"),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c1",
            "search_kb",
            json!({"q": "vpn drops"}),
        )]),
        ChatMessage::tool_result("c1", "three incidents cite mtu mismatch"),
        ChatMessage::assistant("The drops correlate with an MTU mismatch; …"),
    ]
}

// --------------------------------------------------------------------- //
// AC1: substance and the warm replay plan
// --------------------------------------------------------------------- //

#[test]
fn a_substantive_turn_earns_a_warm_replay_plan() {
    let parent = parent_turn(substantive_projection());
    let plan = plan_review(&parent, &ReviewForkConfig::warm()).expect("a tool turn is substantive");

    assert_eq!(plan.substance, Substance::ToolCalls);
    assert_eq!(
        plan.fork_thread_id,
        "thread-9/review/00000000-0000-0000-0000-000000000007"
    );
    assert_eq!(plan.parent_thread_id, "thread-9");
    // The warm path: the parent's model, the full projection, the frozen
    // prefix inherited verbatim.
    assert_eq!(plan.model, parent.model);
    assert_eq!(plan.replay, ReplayMaterial::Projection(parent.projection));
    assert!(plan.replay.is_warm());
    assert_eq!(plan.manifest.prompts, parent.manifest.prompts);
    assert_eq!(plan.manifest.model.as_deref(), Some("gpt-5.2-2026-06-01"));
    assert_eq!(
        plan.prompt_prefix_hash,
        prompt_prefix_hash(&parent.manifest)
    );

    // The stamp: side traffic, fork attribution, parent thread as sub-id.
    assert_eq!(plan.stamp.traffic, TrafficClass::Side);
    assert_eq!(plan.stamp.session_id, parent.session_id);
    assert_eq!(plan.stamp.turn_id, parent.turn_id);
    assert_eq!(plan.stamp.issued_by.component, REVIEW_FORK_COMPONENT);
    assert_eq!(plan.stamp.issued_by.sub_id.as_deref(), Some("thread-9"));
}

#[test]
fn the_substance_rule_gates_the_fork() {
    // An incidental exchange earns nothing.
    let small_talk = vec![
        ChatMessage::user("thanks!"),
        ChatMessage::assistant("anytime."),
    ];
    assert_eq!(
        assess_substance(&small_talk, false, &SubstanceRule::default()),
        None
    );
    assert!(plan_review(&parent_turn(small_talk.clone()), &ReviewForkConfig::warm()).is_none());

    // An explicit learn command outranks every heuristic.
    assert_eq!(
        assess_substance(&small_talk, true, &SubstanceRule::default()),
        Some(Substance::ExplicitLearn)
    );

    // Long assistant output crosses the threshold.
    let long_answer = vec![
        ChatMessage::user("explain the rollout"),
        ChatMessage::assistant("x".repeat(2_500)),
    ];
    assert_eq!(
        assess_substance(&long_answer, false, &SubstanceRule::default()),
        Some(Substance::LongAssistantOutput)
    );
    let tight = SubstanceRule {
        assistant_chars: 3_000,
    };
    assert_eq!(assess_substance(&long_answer, false, &tight), None);
}

// --------------------------------------------------------------------- //
// AC2: the auxiliary model's compact digest
// --------------------------------------------------------------------- //

#[test]
fn an_auxiliary_model_replays_a_compact_digest() {
    let parent = parent_turn(substantive_projection());
    let config = ReviewForkConfig::warm().with_review_model("cheap-review-1");
    let plan = plan_review(&parent, &config).expect("substantive");

    assert_eq!(plan.model, "cheap-review-1");
    assert!(!plan.replay.is_warm());
    let ReplayMaterial::Digest(digest) = &plan.replay else {
        panic!("an auxiliary model replays a digest")
    };
    assert_eq!(digest.len(), 2, "a system frame plus the digest body");
    let body = digest[1].content.as_deref().unwrap();
    assert!(
        body.contains("user: why does the vpn keep dropping?"),
        "{body}"
    );
    assert!(body.contains("assistant (calls: search_kb)"), "{body}");
    // The frozen prefix is still inherited; only the replay material shrinks.
    assert_eq!(plan.manifest.prompts, parent.manifest.prompts);

    // An "auxiliary" that is the parent's model stays on the warm path.
    let same = ReviewForkConfig::warm().with_review_model("gpt-5.2-2026-06-01");
    let plan = plan_review(&parent, &same).expect("substantive");
    assert!(plan.replay.is_warm());
}

#[test]
fn the_digest_truncates_to_its_budget() {
    let projection = vec![
        ChatMessage::user("u".repeat(400)),
        ChatMessage::assistant("a".repeat(400)),
    ];
    let digest = compact_digest(&projection, 100);
    let body = digest[1].content.as_deref().unwrap();
    for line in body.lines() {
        // role prefix + colon + space + at most 100 chars + ellipsis
        assert!(line.len() <= 120, "line over budget: {line:?}");
    }
    assert!(body.contains('…'), "truncation is marked");
}

// --------------------------------------------------------------------- //
// AC3: the confined dispatch surface
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_dispatch_surface_excludes_live_tools_and_refuses_overreach() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let registry = parent_registry(store);
    let token = CancellationToken::new();
    let fork = fork_dispatch_registry(&registry, vec!["skill_lookup".to_owned()], token);

    let mut names: Vec<&str> = fork.names().collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            MEMORY_APPEND_ENTRY_TOOL,
            "memory_replace_block",
            "skill_lookup"
        ]
    );
    assert!(fork.get("send_message").is_none(), "live tools are absent");

    // The guard-layer backup: a boundary-wrapped tool outside the allowlist
    // is refused before it launches, with the refusal legible to the model.
    let boundary = ReviewBoundaryTool::new(
        Arc::new(SendMessageTool),
        Arc::new(["skill_lookup".to_owned()].into_iter().collect()),
        CancellationToken::new(),
    );
    let refusal = boundary.call(json!({"text": "hi"})).await.unwrap();
    let refusal = refusal.as_str().unwrap();
    assert!(refusal.starts_with("ERROR: "), "{refusal}");
    let body: Value = serde_json::from_str(refusal.trim_start_matches("ERROR: ")).unwrap();
    assert_eq!(body["kind"], json!(REVIEW_BOUNDARY_KIND));
    assert_eq!(body["tool"], json!("send_message"));
    assert!(
        body["reason"].as_str().unwrap().contains("confinement"),
        "{body}"
    );

    // The cancellation arm: an allowlisted tool on a cancelled fork is
    // refused before it launches (AC4's tool-level half — the executor's
    // own cancellation aborts the run at the next super-step).
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let boundary = ReviewBoundaryTool::new(
        Arc::new(SkillLookupTool),
        Arc::new(["skill_lookup".to_owned()].into_iter().collect()),
        cancelled,
    );
    let refusal = boundary.call(json!({"name": "vpn"})).await.unwrap();
    let body: Value =
        serde_json::from_str(refusal.as_str().unwrap().trim_start_matches("ERROR: ")).unwrap();
    assert_eq!(body["kind"], json!(REVIEW_BOUNDARY_KIND));
    assert!(
        body["reason"].as_str().unwrap().contains("cancelled"),
        "{body}"
    );
}

// --------------------------------------------------------------------- //
// AC5: the fork run journals side stamps with fork attribution
// --------------------------------------------------------------------- //

#[tokio::test]
async fn a_fork_run_journals_side_stamps_and_commits_memory() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let registry = parent_registry(Arc::clone(&store));
    let parent = parent_turn(substantive_projection());
    let plan = plan_review(&parent, &ReviewForkConfig::warm()).expect("substantive");

    let token = CancellationToken::new();
    let fork_tools =
        fork_dispatch_registry(&registry, vec!["skill_lookup".to_owned()], token.clone());

    let script = vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "r1",
            MEMORY_APPEND_ENTRY_TOOL,
            json!({"content": {"fact": "mtu mismatch"}, "key": "vpn-mtu", "importance": 6}),
        )]),
        ChatMessage::assistant("reviewed: one memory appended"),
    ];
    let model = Arc::new(ForkProbe::new(script));
    let probe = Arc::clone(&model);

    let journal = rusty_agent_runtime::journal::Journal::new(
        "run-review-fork",
        &plan.fork_thread_id,
        Clock::logical(1_800_000_000_000, 10),
    );
    let graph = create_react_agent_with_recording(model, fork_tools, journal.clone()).unwrap();
    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(plan.replay.messages()),
            RunConfig::new(&plan.fork_thread_id)
                .with_journal(journal.clone())
                .with_turn_stamp(plan.stamp.clone())
                .with_manifest(plan.manifest.clone())
                .with_cancellation(token),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ExecutionOutcome::Done(_)),
        "the review completes"
    );

    // Every provider call carried the fork's stamp — side traffic, the
    // review_fork component, the parent thread as sub-id — and journaled it
    // as a RequestHeader before dispatch.
    let stamps = probe.stamps();
    assert_eq!(stamps.len(), 2, "tool loop drives two model calls");
    for stamp in &stamps {
        assert_eq!(stamp.traffic, TrafficClass::Side);
        assert_eq!(stamp.issued_by.component, REVIEW_FORK_COMPONENT);
        assert_eq!(stamp.issued_by.sub_id.as_deref(), Some("thread-9"));
    }
    assert_eq!(stamps[0].turn_boundary, TurnBoundary::Start);
    assert_eq!(stamps[1].turn_boundary, TurnBoundary::Continuation);

    let headers: Vec<TurnStamp> = journal
        .events()
        .into_iter()
        .filter(|event| event.kind == RunEventKind::RequestHeader)
        .map(|event| {
            let input = journal.resolve(event.input.as_ref().unwrap()).unwrap();
            serde_json::from_value(input).expect("the header input is a TurnStamp")
        })
        .collect();
    assert_eq!(headers, stamps, "one journaled header per provider call");

    // The review's memory write committed, annotated by the fork session.
    let record = store
        .query(
            &MemoryQuery {
                key: Some("vpn-mtu".to_owned()),
                ..MemoryQuery::default()
            },
            Clock::logical(1_900_000_000_000, 1_000).now(),
        )
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the fork's append committed");
    assert_eq!(record.priority, 6);
}

// --------------------------------------------------------------------- //
// AC4: cancellation before the next tool launch
// --------------------------------------------------------------------- //

#[tokio::test]
async fn cancellation_denies_the_next_launch_and_keeps_committed_mutations() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let registry = parent_registry(Arc::clone(&store));
    let parent = parent_turn(substantive_projection());
    let plan = plan_review(&parent, &ReviewForkConfig::warm()).expect("substantive");

    let token = CancellationToken::new();
    let fork_tools = fork_dispatch_registry(&registry, Vec::<String>::new(), token.clone());

    // The scheduler cancels the fork when live traffic resumes: the model's
    // second dispatch (after the first append committed) is the signal.
    let script = vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "r1",
            MEMORY_APPEND_ENTRY_TOOL,
            json!({"content": {"v": 1}, "key": "committed"}),
        )]),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "r2",
            MEMORY_APPEND_ENTRY_TOOL,
            json!({"content": {"v": 2}, "key": "never-launched"}),
        )]),
        ChatMessage::assistant("stopped"),
    ];
    let model = Arc::new(ForkProbe::new(script).cancel_on_call(2, token.clone()));

    let journal = rusty_agent_runtime::journal::Journal::new(
        "run-review-cancel",
        &plan.fork_thread_id,
        Clock::logical(1_800_000_000_000, 10),
    );
    let graph = create_react_agent_with_recording(model, fork_tools, journal.clone()).unwrap();
    let outcome = Executor::new()
        .run(
            &graph,
            &spec(),
            initial_state(plan.replay.messages()),
            RunConfig::new(&plan.fork_thread_id)
                .with_journal(journal.clone())
                .with_turn_stamp(plan.stamp.clone())
                .with_cancellation(token),
        )
        .await;
    assert!(
        matches!(
            outcome,
            Err(rusty_agent_runtime::error::RustyError::Cancelled(_))
        ),
        "the review is cancelled, not completed: {outcome:?}"
    );

    // The committed mutation remains a valid ledger entry; the second
    // append never launched.
    let keys = store
        .query(
            &MemoryQuery::default(),
            Clock::logical(1_900_000_000_000, 1_000).now(),
        )
        .await
        .unwrap()
        .into_iter()
        .filter_map(|record| record.key)
        .collect::<Vec<_>>();
    assert_eq!(keys, vec!["committed".to_owned()]);
}

// --------------------------------------------------------------------- //
// AC6: the ledger write
// --------------------------------------------------------------------- //

fn package(name: &str, body: &str) -> SkillPackage {
    SkillPackage::from_markdown(&format!(
        "---\nname: {name}\ndescription: {name} skill\n---\n\n{body}\n"
    ))
    .unwrap()
}

#[test]
fn a_create_write_enters_trial_with_fork_attribution() {
    let mut registry = SkillRegistry::new();
    let write = ForkSkillWrite {
        package: package("vpn-mtu-fix", "Check the MTU before escalating."),
        editorial: EditorialProvenance::create_new(
            "no loaded skill covers vpn triage; the two nearest are hardware-only",
        )
        .unwrap(),
    };
    let commit = commit_skill_write(&mut registry, write, "review_fork:thread-9").unwrap();

    assert_eq!(commit.name, "vpn-mtu-fix");
    assert_eq!(commit.revision, 1);
    assert_eq!(commit.rung, PatchPreference::CreateNew);
    assert_eq!(commit.status, SkillPromotionStatus::Trial);
    assert_eq!(commit.author, "review_fork:thread-9");

    let version = registry.get("vpn-mtu-fix").expect("registered");
    assert_eq!(version.revision(), 1);
    assert_eq!(version.content_hash(), commit.content_hash);
    assert_eq!(version.provenance().author, "review_fork:thread-9");
    assert_eq!(version.provenance().source, SkillSource::Learned);
    assert_eq!(version.provenance().source.as_id_string(), "learned");
}

#[test]
fn patch_before_create_is_enforced_at_the_write_point() {
    let mut registry = SkillRegistry::new();
    let create = ForkSkillWrite {
        package: package("vpn-mtu-fix", "v1 body"),
        editorial: EditorialProvenance::create_new("no patch target exists").unwrap(),
    };
    commit_skill_write(&mut registry, create, "review_fork:thread-9").unwrap();

    // A create over the existing name is refused — patch it instead.
    let duplicate = ForkSkillWrite {
        package: package("vpn-mtu-fix", "v2 body"),
        editorial: EditorialProvenance::create_new("justified").unwrap(),
    };
    let error = commit_skill_write(&mut registry, duplicate, "review_fork:thread-9").unwrap_err();
    assert!(
        matches!(error, ForkWriteError::CreateOverExisting { ref name } if name == "vpn-mtu-fix"),
        "{error}"
    );

    // A patch against thin air is refused.
    let orphan = ForkSkillWrite {
        package: package("ghost", "body"),
        editorial: EditorialProvenance::patch(PatchPreference::PatchLoadedSkill).unwrap(),
    };
    let error = commit_skill_write(&mut registry, orphan, "review_fork:thread-9").unwrap_err();
    assert!(
        matches!(error, ForkWriteError::PatchWithoutTarget { ref name } if name == "ghost"),
        "{error}"
    );

    // The honest patch lands as the next revision, still Trial.
    let patch = ForkSkillWrite {
        package: package("vpn-mtu-fix", "v2 body"),
        editorial: EditorialProvenance::patch(PatchPreference::PatchLoadedSkill).unwrap(),
    };
    let commit = commit_skill_write(&mut registry, patch, "review_fork:thread-9").unwrap();
    assert_eq!(commit.revision, 2);
    assert_eq!(commit.rung, PatchPreference::PatchLoadedSkill);
    assert_eq!(commit.status, SkillPromotionStatus::Trial);
    assert_eq!(registry.get("vpn-mtu-fix").unwrap().revision(), 2);
}
