//! Skills run-integration tests (R0.13 agent core, wave 4a).
//!
//! Five groups:
//!
//! - **Golden files** — the wire shapes of `SkillBinding`, the
//!   `SkillSelection` ranking record, the `SkillGateTool` refusal body, and
//!   the composed assembly's section manifest, pinned under
//!   `tests/golden/`. `UPDATE_GOLDEN=1` blesses an intentional change.
//! - **Shortlist determinism** — selection is a pure function: shuffled
//!   catalogs and repeated runs produce byte-identical selections.
//! - **The gating wrapper** — an out-of-active-skill call is refused with
//!   the structured `skill_tool_gate` payload (journaled as a `ToolCall`
//!   under the `RecordingTool` pattern), in-set calls pass through,
//!   `effect_request` delegates, and the active set is per-invocation:
//!   updating the handle changes what the same wrapper admits.
//! - **Version-pointer authority** — promote/rollback of the `skill:{name}`
//!   learn pointer changes which revision the pipeline binds, byte-exact.
//! - **The composition recipe** — the wave-1 pipeline assembles a skills
//!   section from this module's selection and resolution, and the manifest
//!   carries the `name@revision:hash` pins.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::context::{
    ContextInputs, ContextPipeline, ContextPolicy, SectionKind, SectionPolicy,
    CONTEXT_POLICY_SCHEMA_VERSION,
};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::learn::{
    CandidateId, PromotionAuthority, PromotionDecision, PromotionReceipt, RollbackReceipt,
    SurfaceKey, VersionPointer,
};
use rusty_agent_runtime::llm::ToolCall;
use rusty_agent_runtime::memory::ContextBudget;
use rusty_agent_runtime::record::{Effect, EventStatus, PayloadRef, RunEventKind};
use rusty_agent_runtime::replay::RecordingTool;
use rusty_agent_runtime::skill::{SkillMetadata, SkillPackage, SkillRegistry, SkillSource};
use rusty_agent_runtime::skills::{
    parse_skill_tool_gate_refusal, resolve_active_skill, select_skills, skill_section_entries,
    skill_tool_gate_refusal, ActiveSkill, ActiveSkillSet, ActiveSkills, SkillBinding,
    SkillCatalogEntry, SkillGateTool, SkillPin, SkillSelectionFeatures, SkillSelectionPolicy,
    SKILL_TOOL_GATE_KIND,
};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use rusty_agent_runtime::error::Result;

// ---------- golden-file machinery (the tests/learn.rs discipline) ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
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

fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
}

fn register(registry: &mut SkillRegistry, name: &str, body: &str) -> SkillMetadata {
    let package =
        SkillPackage::from_markdown(&skill_md(name, &format!("The {name} skill."), body))
            .expect("valid package");
    registry
        .register(
            package,
            SkillSource::LocalPath {
                path: "/skills/test".to_owned(),
            },
            "skills-run-tests",
        )
        .expect("registers")
        .version
        .metadata()
}

fn binding(tags: &[&str], tools: &[&str], task_shape: Option<&str>) -> SkillBinding {
    SkillBinding {
        trigger_tags: tags.iter().map(|t| t.to_string()).collect(),
        task_shape: task_shape.map(str::to_owned),
        cost_class: None,
        tools: tools.iter().map(|t| t.to_string()).collect(),
    }
}

fn catalog_entry(metadata: SkillMetadata, binding: SkillBinding) -> SkillCatalogEntry {
    SkillCatalogEntry { metadata, binding }
}

struct Search;

#[async_trait]
impl Tool for Search {
    fn name(&self) -> &str {
        "web.search"
    }
    fn description(&self) -> &str {
        "Searches the web."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"echo": args}))
    }
}

struct Email;

#[async_trait]
impl Tool for Email {
    fn name(&self) -> &str {
        "email.send"
    }
    fn description(&self) -> &str {
        "Sends email."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"to": {"type": "string"}}})
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"sent": args}))
    }
}

fn tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Search);
    registry.register(Email);
    registry
}

/// A registry with `web-research` at two revisions and `email-drafting` at
/// one — the catalog every test derives from.
fn skill_registry() -> (SkillRegistry, SkillMetadata, SkillMetadata, SkillMetadata) {
    let mut registry = SkillRegistry::new();
    let research_v1 = register(&mut registry, "web-research", "Search, then summarize.");
    let research_v2 = register(
        &mut registry,
        "web-research",
        "Search, then summarize with citations.",
    );
    let drafting = register(&mut registry, "email-drafting", "Draft a reply, then confirm.");
    (registry, research_v1, research_v2, drafting)
}

fn catalog(research_v2: &SkillMetadata, drafting: &SkillMetadata) -> Vec<SkillCatalogEntry> {
    vec![
        catalog_entry(
            research_v2.clone(),
            binding(&["web", "research"], &["web.search"], Some("open-ended lookup tasks")),
        ),
        catalog_entry(
            drafting.clone(),
            binding(&["email"], &["email.send"], None),
        ),
    ]
}

// ---------- golden files ----------

#[test]
fn golden_skill_binding_shape() {
    assert_golden(
        "skill_binding.json",
        &binding(&["web", "research"], &["web.search"], Some("open-ended lookup tasks")),
    );
}

#[test]
fn golden_skill_selection_shape() {
    let (_registry, _v1, v2, drafting) = skill_registry();
    let features = SkillSelectionFeatures {
        task_tags: vec!["research".into()],
        available_tools: Some(BTreeSet::from(["web.search".to_string()])),
    };
    // `email-drafting` declares a tool the narrowed run lacks: excluded.
    let selection = select_skills(
        &features,
        &catalog(&v2, &drafting),
        &SkillSelectionPolicy::default(),
    );
    assert_golden("skill_selection.json", &selection);
}

#[test]
fn golden_skill_tool_gate_refusal_shape() {
    let active = ActiveSkillSet::new(vec![ActiveSkill {
        name: "web-research".into(),
        revision: 2,
        content_hash: "a".repeat(64),
        tools: vec!["web.search".into()],
    }]);
    let payload = skill_tool_gate_refusal("email.send", &active);
    // Byte-exact: the compact string the failure-isolation channel carries,
    // key order pinned by serde_json's default sorted map, inside and out.
    assert_eq!(
        payload,
        "ERROR: {\"active_skills\":[{\"content_hash\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"name\":\"web-research\",\"revision\":2}],\"declared_tools\":[\"web.search\"],\"kind\":\"skill_tool_gate\",\"tool\":\"email.send\"}"
    );
    let body: Value = serde_json::from_str(payload.strip_prefix("ERROR: ").unwrap()).unwrap();
    assert_golden("skill_tool_gate_refusal.json", &body);
    // Round-trip: the parse half decodes exactly what the builder emitted.
    let parsed = parse_skill_tool_gate_refusal(&payload).unwrap();
    assert_eq!(parsed.tool, "email.send");
    assert_eq!(parsed.declared_tools, vec!["web.search".to_string()]);
}

// ---------- shortlist determinism ----------

#[test]
fn shortlist_is_byte_identical_under_shuffle_and_repetition() {
    let (_registry, _v1, v2, drafting) = skill_registry();
    let features = SkillSelectionFeatures {
        task_tags: vec!["research".into()],
        available_tools: None,
    };
    let policy = SkillSelectionPolicy::default();
    let base = select_skills(&features, &catalog(&v2, &drafting), &policy);

    let mut shuffled = catalog(&v2, &drafting);
    shuffled.reverse();
    let rerun = select_skills(&features, &shuffled, &policy);
    assert_eq!(
        serde_json::to_string(&base).unwrap(),
        serde_json::to_string(&rerun).unwrap(),
    );
    let again = select_skills(&features, &catalog(&v2, &drafting), &policy);
    assert_eq!(
        serde_json::to_string(&base).unwrap(),
        serde_json::to_string(&again).unwrap(),
    );
}

// ---------- the gating wrapper ----------

fn gated_registry(active: &ActiveSkills) -> ToolRegistry {
    SkillGateTool::wrap_registry(&tool_registry(), active.clone())
}

fn active_research(hash: &str) -> ActiveSkillSet {
    ActiveSkillSet::new(vec![ActiveSkill {
        name: "web-research".into(),
        revision: 2,
        content_hash: hash.to_owned(),
        tools: vec!["web.search".into()],
    }])
}

#[tokio::test]
async fn out_of_active_skill_calls_are_refused_in_set_calls_pass() {
    let (_registry, _v1, v2, _drafting) = skill_registry();
    let active = ActiveSkills::new();
    active.set(active_research(&v2.content_hash));
    let gated = gated_registry(&active);

    // In-set: dispatch passes through untouched.
    let search = gated.get("web.search").unwrap();
    let ok = search.call(json!({"query": "rust"})).await.unwrap();
    assert_eq!(ok["echo"]["query"], json!("rust"));

    // Out-of-set: the structured refusal, never a dispatch.
    let email = gated.get("email.send").unwrap();
    let refused = email.call(json!({"to": "a@b.c"})).await.unwrap();
    let Value::String(payload) = &refused else {
        panic!("refusal is a string payload");
    };
    let parsed = parse_skill_tool_gate_refusal(payload).unwrap();
    assert_eq!(parsed.tool, "email.send");
    assert_eq!(parsed.declared_tools, vec!["web.search".to_string()]);
    assert_eq!(parsed.active_skills.len(), 1);
    assert_eq!(parsed.active_skills[0].name, "web-research");
    assert_eq!(parsed.active_skills[0].revision, 2);

    // Round-trip: build → parse → rebuild is byte-identical.
    let rebuilt = skill_tool_gate_refusal(
        &parsed.tool,
        &ActiveSkillSet::new(vec![ActiveSkill {
            name: parsed.active_skills[0].name.clone(),
            revision: parsed.active_skills[0].revision,
            content_hash: parsed.active_skills[0].content_hash.clone(),
            tools: parsed.declared_tools.clone(),
        }]),
    );
    assert_eq!(&rebuilt, payload);
}

#[tokio::test]
async fn the_gate_is_per_invocation_state_not_a_static_allowlist() {
    let (_registry, _v1, v2, _drafting) = skill_registry();
    let active = ActiveSkills::new();
    let gated = gated_registry(&active);
    let email = gated.get("email.send").unwrap();

    // No active skill: nothing is narrowed.
    assert!(email.call(json!({"to": "a@b.c"})).await.unwrap()["sent"]["to"] == json!("a@b.c"));

    // Assembly activates web-research: email.send is now refused.
    active.set(active_research(&v2.content_hash));
    let refused = email.call(json!({"to": "a@b.c"})).await.unwrap();
    assert!(parse_skill_tool_gate_refusal(refused.as_str().unwrap()).is_some());

    // The next assembly activates email-drafting instead: the same wrapper
    // admits the same tool.
    active.set(ActiveSkillSet::new(vec![ActiveSkill {
        name: "email-drafting".into(),
        revision: 1,
        content_hash: "d".repeat(64),
        tools: vec!["email.send".into()],
    }]));
    assert!(email.call(json!({"to": "a@b.c"})).await.unwrap()["sent"]["to"] == json!("a@b.c"));

    // A skill that declares no tools narrows nothing.
    active.set(ActiveSkillSet::new(vec![ActiveSkill {
        name: "web-research".into(),
        revision: 2,
        content_hash: v2.content_hash.clone(),
        tools: vec![],
    }]));
    assert!(email.call(json!({"to": "a@b.c"})).await.unwrap()["sent"]["to"] == json!("a@b.c"));
}

#[test]
fn wrapper_identity_and_effect_request_delegate() {
    let active = ActiveSkills::new();
    let inner: Arc<dyn Tool> = Arc::new(Email);
    let wrapped = SkillGateTool::new(inner.clone(), active);

    assert_eq!(wrapped.name(), inner.name());
    assert_eq!(wrapped.description(), inner.description());
    assert_eq!(wrapped.parameters_schema(), inner.parameters_schema());
    assert_eq!(wrapped.effect(), inner.effect());
    assert_eq!(wrapped.effect_kind(), inner.effect_kind());
    let call = ToolCall::new("c1", "email.send", json!({"to": "a@b.c"}));
    assert_eq!(wrapped.effect_request(&call), inner.effect_request(&call));
}

#[tokio::test]
async fn gate_refusal_journals_as_an_ordinary_tool_call() {
    let (_registry, _v1, v2, _drafting) = skill_registry();
    let journal = Journal::new("run-gate", "thread-gate", Clock::System);
    let active = ActiveSkills::new();
    active.set(active_research(&v2.content_hash));

    let recording = RecordingTool::new(
        Arc::new(SkillGateTool::new(Arc::new(Email), active.clone())) as Arc<dyn Tool>,
        journal.clone(),
        "parent-event",
    )
    .node("tools");

    let refused = recording.call(json!({"to": "a@b.c"})).await.unwrap();
    let payload = refused.as_str().unwrap().to_owned();

    // The allowed call through the same journaled path.
    let search = RecordingTool::new(
        Arc::new(SkillGateTool::new(Arc::new(Search), active)) as Arc<dyn Tool>,
        journal.clone(),
        "parent-event",
    );
    let ok = search.call(json!({"query": "rust"})).await.unwrap();

    let snapshot = journal.snapshot();
    let tool_events: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ToolCall)
        .collect();
    assert_eq!(tool_events.len(), 2, "both calls journaled");

    let Some(PayloadRef::Inline(output)) = &tool_events[0].output else {
        panic!("refusal output is inline");
    };
    let recorded = output.as_str().unwrap();
    assert_eq!(recorded, payload, "the journaled payload is byte-exact");
    assert!(recorded.contains(&format!("\"kind\":\"{SKILL_TOOL_GATE_KIND}\"")));
    let parsed = parse_skill_tool_gate_refusal(recorded).unwrap();
    assert_eq!(parsed.tool, "email.send");
    // A gate refusal is the tool's answer, not a dispatch failure: the
    // event closes Ok, exactly as ValidatingTool's refusal does.
    assert_eq!(tool_events[0].status, EventStatus::Ok);

    let Some(PayloadRef::Inline(output)) = &tool_events[1].output else {
        panic!("pass-through output is inline");
    };
    assert_eq!(output["echo"]["query"], json!("rust"));
    let _ = ok;
}

// ---------- version-pointer authority: promote / rollback, byte-exact ----------

fn promotion(candidate: &CandidateId, surface: &SurfaceKey, previous: Option<CandidateId>) -> PromotionReceipt {
    PromotionReceipt {
        candidate_id: candidate.clone(),
        surface: surface.clone(),
        previous,
        decision: PromotionDecision {
            authority: PromotionAuthority::Approval {
                approved_by: "ops-lead".into(),
            },
            canary: None,
        },
        promoted_at: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    }
}

/// The pin store for the fixture candidates: candidate id → skill pin.
fn pin_for<'a>(
    v1: (&'a CandidateId, &'a SkillMetadata),
    v2: (&'a CandidateId, &'a SkillMetadata),
) -> impl Fn(&CandidateId) -> Option<SkillPin> + 'a {
    move |id| {
        let (id_a, meta_a) = v1;
        let (id_b, meta_b) = v2;
        if id == id_a {
            Some(SkillPin {
                name: meta_a.name.clone(),
                content_hash: meta_a.content_hash.clone(),
                binding: binding(&["web", "research"], &["web.search"], None),
            })
        } else if id == id_b {
            Some(SkillPin {
                name: meta_b.name.clone(),
                content_hash: meta_b.content_hash.clone(),
                binding: binding(&["web", "research"], &["web.search"], None),
            })
        } else {
            None
        }
    }
}

fn skills_only_policy() -> ContextPolicy {
    ContextPolicy {
        schema_version: CONTEXT_POLICY_SCHEMA_VERSION.to_owned(),
        budget: ContextBudget::new(4096),
        tokenizer: Default::default(),
        identity: None,
        task: Some(SectionPolicy::new(256)),
        skills: Some(SectionPolicy::new(512)),
        tools: None,
        memory: None,
        history: None,
        compaction: None,
    }
}

/// Assemble the skills section for the active set the pointer binds.
async fn assemble_with_pointer(
    registry: &SkillRegistry,
    pointer: &VersionPointer,
    research: &SkillMetadata,
    drafting: &SkillMetadata,
    pin: &dyn Fn(&CandidateId) -> Option<SkillPin>,
) -> rusty_agent_runtime::context::ContextAssembly {
    let active_skill = resolve_active_skill(registry, pointer, "run-w4a", pin).unwrap();
    let active = ActiveSkillSet::new(active_skill.into_iter().collect());
    let features = SkillSelectionFeatures {
        task_tags: vec!["research".into()],
        available_tools: None,
    };
    let selection = select_skills(
        &features,
        &catalog(research, drafting),
        &SkillSelectionPolicy::default(),
    );
    let entries = skill_section_entries(registry, &selection, &active).unwrap();
    let pipeline = ContextPipeline::new(skills_only_policy())
        .unwrap()
        .with_policy_pin("skills-test", None, None);
    let inputs = ContextInputs {
        task: Some("Research the topic.".into()),
        skills: entries,
        ..Default::default()
    };
    pipeline.assemble(&inputs, None).await.unwrap()
}

#[tokio::test]
async fn promotion_and_rollback_change_the_bound_revision_byte_exactly() {
    let (registry, v1, v2, drafting) = skill_registry();
    let cand_v1 = CandidateId::from("1".repeat(64));
    let cand_v2 = CandidateId::from("2".repeat(64));
    let surface = SurfaceKey::new("skill:web-research");
    let pin = pin_for((&cand_v1, &v1), (&cand_v2, &v2));

    // Nothing promoted: the skill does not bind — the registry's latest
    // pointer (v2) is authorship history, never an activation fallback.
    let pointer = VersionPointer::new(surface.clone());
    let none_active = resolve_active_skill(&registry, &pointer, "run-w4a", &pin).unwrap();
    assert!(none_active.is_none());

    // Promote v1: the pipeline binds revision 1.
    let pointer = pointer.promoted(&promotion(&cand_v1, &surface, None));
    let assembly_v1 =
        assemble_with_pointer(&registry, &pointer, &v2 /* catalog latest */, &drafting, &pin).await;
    let bytes_v1 = serde_json::to_string(&assembly_v1).unwrap();
    let skills_report = assembly_v1
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Skills)
        .expect("skills section reported");
    assert_eq!(
        skills_report.ids,
        vec![
            format!("web-research@1:{}", v1.content_hash),
            format!("email-drafting@1:{}", drafting.content_hash),
        ],
        "the manifest pins the bound revision and hash",
    );

    // Promote v2: the bound revision — and the assembled bytes — change.
    let pointer = pointer.promoted(&promotion(&cand_v2, &surface, Some(cand_v1.clone())));
    let assembly_v2 = assemble_with_pointer(&registry, &pointer, &v2, &drafting, &pin).await;
    let bytes_v2 = serde_json::to_string(&assembly_v2).unwrap();
    assert_ne!(bytes_v1, bytes_v2, "a new revision is a new assembly");
    let skills_report = assembly_v2
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Skills)
        .unwrap();
    assert_eq!(
        skills_report.ids,
        vec![
            format!("web-research@2:{}", v2.content_hash),
            format!("email-drafting@1:{}", drafting.content_hash),
        ],
    );

    // Rollback re-points to v1: the restored assembly is byte-identical —
    // the pointer chooses among immutable, content-addressed versions.
    let rollback = RollbackReceipt {
        surface: surface.clone(),
        from: cand_v2.clone(),
        to: Some(cand_v1.clone()),
        cause: "regression on the recorded dataset".into(),
        rolled_back_at: chrono::DateTime::from_timestamp_millis(1_700_000_001_000).unwrap(),
    };
    let pointer = pointer.rolled_back(&rollback);
    let assembly_restored = assemble_with_pointer(&registry, &pointer, &v2, &drafting, &pin).await;
    let bytes_restored = serde_json::to_string(&assembly_restored).unwrap();
    assert_eq!(bytes_v1, bytes_restored, "rollback is byte-exact");
}

// ---------- the composition recipe with the wave-1 pipeline ----------

#[tokio::test]
async fn pipeline_composition_assembles_and_pins_the_skills_section() {
    let (registry, _v1, v2, drafting) = skill_registry();
    let cand_v1 = CandidateId::from("1".repeat(64));
    let cand_v2 = CandidateId::from("2".repeat(64));
    let surface = SurfaceKey::new("skill:web-research");
    let pin = pin_for((&cand_v1, &v2), (&cand_v2, &v2));

    // Admission: the pointer binds v2; the shortlist ranks the catalog;
    // the section entries carry tier-1 for both and tier-2 for the active.
    let pointer = VersionPointer::new(surface.clone())
        .promoted(&promotion(&cand_v2, &surface, None));
    let active_skill = resolve_active_skill(&registry, &pointer, "run-w4a", &pin)
        .unwrap()
        .expect("promoted skill binds");
    assert_eq!(active_skill.revision, v2.revision);
    let active = ActiveSkillSet::new(vec![active_skill]);

    let features = SkillSelectionFeatures {
        task_tags: vec!["research".into()],
        available_tools: Some(BTreeSet::from([
            "web.search".to_string(),
            "email.send".to_string(),
        ])),
    };
    let selection = select_skills(
        &features,
        &catalog(&v2, &drafting),
        &SkillSelectionPolicy::default(),
    );
    let entries = skill_section_entries(&registry, &selection, &active).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].name, "web-research",
        "the tag-matched skill ranks first"
    );
    assert_eq!(
        entries[0].body.as_deref(),
        Some("Search, then summarize with citations.\n"),
        "the active skill's tier-2 body loads",
    );
    assert!(entries[1].body.is_none(), "shortlisted-only: tier-1");

    let pipeline = ContextPipeline::new(skills_only_policy())
        .unwrap()
        .with_policy_pin("skills-test", None, None);
    let inputs = ContextInputs {
        task: Some("Research the topic.".into()),
        skills: entries,
        ..Default::default()
    };
    let assembly = pipeline.assemble(&inputs, None).await.unwrap();

    // The manifest carries the skill name/revision/hash pins.
    let skills_report = assembly
        .manifest
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Skills)
        .expect("skills section reported");
    assert_eq!(
        skills_report.ids,
        vec![
            format!("web-research@2:{}", v2.content_hash),
            format!("email-drafting@1:{}", drafting.content_hash),
        ],
    );

    // The assembled context carries the section and the active body.
    let skills_message = assembly
        .messages
        .iter()
        .find(|m| m.content.as_deref().is_some_and(|c| c.contains("# Skills")))
        .expect("skills section assembled");
    let content = skills_message.content.as_deref().unwrap();
    assert!(content.contains("web-research (revision 2,"));
    assert!(content.contains("## Skill: web-research\nSearch, then summarize with citations."));
    assert!(!content.contains("## Skill: email-drafting"));

    // Determinism: the same inputs assemble byte-identically.
    let again = pipeline.assemble(&inputs, None).await.unwrap();
    assert_eq!(
        serde_json::to_string(&assembly).unwrap(),
        serde_json::to_string(&again).unwrap(),
    );

    assert_golden("skills_assembly.json", &assembly.manifest);
}
