//! Tool-selection integration tests (R0.13 agent core, wave 1b).
//!
//! Four groups:
//!
//! - **Golden files** — the wire shapes of `ToolSelectionOverlay`,
//!   `ToolManifest`, the `ValidatingTool` refusal body, and the
//!   `ToolShortlist` ranking record, pinned under `tests/golden/`.
//!   `UPDATE_GOLDEN=1` blesses an intentional change.
//! - **Determinism** — selection is a pure function: shuffled inputs and
//!   repeated runs produce byte-identical shortlists.
//! - **Journaled refusal** — a validation refusal through the
//!   `RecordingTool` pattern journals as an ordinary `ToolCall` carrying
//!   the structured payload, and the repaired call succeeds; both are in
//!   the journal.
//! - **The ReAct composition recipe** — construction-time narrowing via
//!   `restricted_to`, `ValidatingTool` wrappers via `register_shared`, and
//!   middleware for the unjournaled half, all over the shipped
//!   `ToolExecutor`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::llm::ToolCall;
use rusty_agent_runtime::middleware::{MiddlewareChain, ToolCallBlocklist};
use rusty_agent_runtime::record::{Effect, EventStatus, PayloadRef, RunEventKind};
use rusty_agent_runtime::replay::RecordingTool;
use rusty_agent_runtime::tool::{Tool, ToolExecutor, ToolRegistry};
use rusty_agent_runtime::tool_select::{
    apply_spec, argument_validation_refusal, filtered, manifests_for_registry,
    parse_argument_validation_refusal, prefixed, select, shortlist, ArgumentViolation, CostClass,
    DeferLoadingRegistry, PrefixedTool, PreparedOverride, PreparedTool, SelectionFeatures,
    ToolManifest, ToolOutcomeStats, ToolPredicate, ToolSelectionOverlay, ToolSelectionPolicy,
    ToolsetSpec, ValidatingTool, ARGUMENT_VALIDATION_KIND,
};

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
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"results": [], "query": args.get("query").cloned().unwrap_or(Value::Null)}))
    }
}

struct Fetch;

#[async_trait]
impl Tool for Fetch {
    fn name(&self) -> &str {
        "http.get"
    }
    fn description(&self) -> &str {
        "Fetches a URL."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"url": {"type": "string"}},
            "required": ["url"]
        })
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"status": 200, "url": args.get("url").cloned().unwrap_or(Value::Null)}))
    }
}

struct Send;

#[async_trait]
impl Tool for Send {
    fn name(&self) -> &str {
        "email.send"
    }
    fn description(&self) -> &str {
        "Sends an email."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"to": {"type": "string"}, "subject": {"type": "string"}},
            "required": ["to", "subject"]
        })
    }
    // NonIdempotent by default — the write-class effect.
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"sent": true, "to": args.get("to").cloned().unwrap_or(Value::Null)}))
    }
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Search);
    registry.register(Fetch);
    registry.register(Send);
    registry
}

fn overlays() -> BTreeMap<String, ToolSelectionOverlay> {
    let mut overlays = BTreeMap::new();
    overlays.insert(
        "web.search".to_owned(),
        ToolSelectionOverlay {
            tags: vec!["web".into(), "research".into()],
            when_to_use: Some("For open questions needing current information.".into()),
            cost_class: Some(CostClass::Medium),
            parallel_safe: None,
            batchable: Some(true),
            prerequisites: vec![],
        },
    );
    overlays.insert(
        "http.get".to_owned(),
        ToolSelectionOverlay {
            tags: vec!["web".into()],
            when_to_use: None,
            cost_class: Some(CostClass::Low),
            parallel_safe: Some(true),
            batchable: None,
            prerequisites: vec![],
        },
    );
    overlays.insert(
        "email.send".to_owned(),
        ToolSelectionOverlay {
            tags: vec!["notify".into()],
            when_to_use: Some("Only after the user confirmed the draft.".into()),
            cost_class: Some(CostClass::High),
            parallel_safe: None,
            batchable: None,
            prerequisites: vec!["http.get".into()],
        },
    );
    overlays
}

fn features() -> SelectionFeatures {
    let mut outcomes = BTreeMap::new();
    outcomes.insert(
        "web.search".to_owned(),
        ToolOutcomeStats {
            calls: 20,
            successes: 18,
            validation_failures: 1,
        },
    );
    SelectionFeatures {
        task_tags: vec!["web".into(), "research".into()],
        effect_ceiling: Effect::ReadOnly,
        outcomes,
    }
}

// ---------- golden files ----------

#[test]
fn golden_tool_selection_overlay_shape() {
    assert_golden("tool_selection_overlay.json", &overlays()["web.search"]);
}

#[test]
fn golden_tool_manifest_shape() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let manifest = manifests.iter().find(|m| m.name == "web.search").unwrap();
    assert_golden("tool_manifest.json", manifest);
}

#[test]
fn golden_argument_validation_refusal_shape() {
    let violations = vec![
        ArgumentViolation {
            path: String::new(),
            rule: "required".into(),
            message: "missing required property `query`".into(),
        },
        ArgumentViolation {
            path: "/limit".into(),
            rule: "type".into(),
            message: "expected \"integer\", found string".into(),
        },
    ];
    let body: Value = serde_json::from_str(
        argument_validation_refusal(&violations)
            .strip_prefix("ERROR: ")
            .unwrap(),
    )
    .unwrap();
    assert_golden("argument_validation_refusal.json", &body);
}

#[test]
fn golden_tool_shortlist_shape() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let outcome = select(&features(), &manifests, 5);
    assert_golden("tool_shortlist.json", &outcome);
}

// ---------- manifest parsing ----------

#[test]
fn overlay_round_trips_and_defaults_stay_absent() {
    // A sparse overlay carries no empty members on the wire (the evolution
    // rule: optional fields stay absent while unset).
    let sparse = ToolSelectionOverlay {
        tags: vec!["web".into()],
        ..Default::default()
    };
    let wire = serde_json::to_string(&sparse).unwrap();
    assert_eq!(wire, "{\"tags\":[\"web\"]}");
    let parsed: ToolSelectionOverlay = serde_json::from_str(&wire).unwrap();
    assert_eq!(parsed, sparse);

    // The full overlay round-trips.
    let full = &overlays()["email.send"];
    let parsed: ToolSelectionOverlay =
        serde_json::from_str(&serde_json::to_string(full).unwrap()).unwrap();
    assert_eq!(&parsed, full);
}

#[test]
fn manifest_parses_from_wire() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let manifest = &manifests[0];
    let parsed: ToolManifest =
        serde_json::from_str(&serde_json::to_string(manifest).unwrap()).unwrap();
    assert_eq!(&parsed, manifest);
}

#[test]
fn manifests_for_registry_is_sorted_and_complete() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["email.send", "http.get", "web.search"]);
    let send = &manifests[0];
    assert!(!send.parallel_safe, "write-class effect derives false");
    assert_eq!(send.prerequisites, ["http.get"]);
}

// ---------- selection determinism ----------

#[test]
fn selection_is_deterministic_under_input_reordering() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let mut reversed = manifests.clone();
    reversed.reverse();
    let forward = select(&features(), &manifests, 5);
    let backward = select(&features(), &reversed, 5);
    assert_eq!(forward, backward);
    // Byte-identical serialization across repeated selection.
    assert_eq!(
        serde_json::to_string(&forward).unwrap(),
        serde_json::to_string(&select(&features(), &manifests, 5)).unwrap()
    );
}

#[test]
fn shortlist_ranks_and_excludes_by_ceiling() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let outcome = select(&features(), &manifests, 5);
    // email.send is above the read-only ceiling.
    assert_eq!(outcome.excluded.len(), 1);
    assert_eq!(outcome.excluded[0].name, "email.send");
    // web.search outranks http.get on tag overlap + outcome stats.
    let order: Vec<&str> = outcome.ranking.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(order, ["web.search", "http.get"]);
}

#[test]
fn shortlist_policy_cutoff_boundary() {
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let policy = ToolSelectionPolicy {
        cutoff: 3,
        k: 1,
        ..Default::default()
    };
    // 3 manifests at cutoff 3: identity — every eligible tool, ranked.
    let outcome = shortlist(&features(), &manifests, &policy);
    assert_eq!(outcome.selected.len(), 2, "two eligible (one excluded)");
    let policy = ToolSelectionPolicy {
        cutoff: 2,
        k: 1,
        ..Default::default()
    };
    let outcome = shortlist(&features(), &manifests, &policy);
    assert_eq!(outcome.selected.len(), 1);
    assert_eq!(outcome.selected[0].name, "web.search");
}

// ---------- journaled refusal (the RecordingTool pattern) ----------

fn inline_payload(reference: &Option<PayloadRef>) -> &Value {
    match reference {
        Some(PayloadRef::Inline(value)) => value,
        other => panic!("expected an inline payload, got {other:?}"),
    }
}

#[tokio::test]
async fn validation_refusal_journals_and_repair_succeeds() {
    let journal = Journal::new("run-1", "thread-1", Clock::System);
    let recording = RecordingTool::new(
        Arc::new(ValidatingTool::new(Arc::new(Search))) as Arc<dyn Tool>,
        journal.clone(),
        "parent-event",
    )
    .node("tools");

    // The malformed call: refusal payload, journaled.
    let refused = recording.call(json!({"limit": "5"})).await.unwrap();
    let Value::String(payload) = &refused else {
        panic!("refusal is a string payload");
    };
    let violations = parse_argument_validation_refusal(payload).unwrap();
    assert_eq!(violations.len(), 2);

    // The repaired call succeeds.
    let repaired = recording
        .call(json!({"query": "rust", "limit": 5}))
        .await
        .unwrap();
    assert_eq!(repaired["query"], json!("rust"));

    let snapshot = journal.snapshot();
    let tool_events: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ToolCall)
        .collect();
    assert_eq!(tool_events.len(), 2, "both calls journaled");

    // Event 1: the refusal — output payload IS the structured contract.
    let refusal_output = inline_payload(&tool_events[0].output);
    let recorded = refusal_output.as_str().unwrap();
    assert!(recorded.starts_with("ERROR: "));
    assert!(recorded.contains(&format!("\"kind\":\"{ARGUMENT_VALIDATION_KIND}\"")));
    assert_eq!(
        parse_argument_validation_refusal(recorded).unwrap(),
        violations
    );

    // Event 2: the repaired call's success.
    let repaired_output = inline_payload(&tool_events[1].output);
    assert_eq!(repaired_output["query"], json!("rust"));
    assert_eq!(tool_events[1].status, EventStatus::Ok);
}

// ---------- the ReAct composition recipe ----------

#[tokio::test]
async fn recipe_narrow_validate_and_dispatch() {
    // 1. Shortlist at admission, then construction-time narrowing.
    let manifests = manifests_for_registry(&registry(), &overlays()).unwrap();
    let shortlist = shortlist(
        &features(),
        &manifests,
        &ToolSelectionPolicy {
            cutoff: 2,
            k: 2,
            ..Default::default()
        },
    );
    let names: Vec<String> = shortlist.selected.iter().map(|r| r.name.clone()).collect();
    let narrowed = registry().restricted_to(&names).unwrap();
    assert!(narrowed.contains("web.search"));
    assert!(
        !narrowed.contains("email.send"),
        "ceiling-excluded at admission"
    );

    // 2. Wrap the narrowed registry in validation (register_shared shape).
    let validated = ValidatingTool::wrap_registry(&narrowed);

    // 3. Middleware carries only the unjournaled half: a blocklist
    //    rejection is an opaque ERROR string, never the structured kind.
    let chain = MiddlewareChain::new().layer(ToolCallBlocklist::new(["http.get"]));
    let executor = ToolExecutor::new(validated).with_middleware(chain);

    let calls = vec![
        ToolCall::new("c1", "web.search", json!({"limit": "5"})),
        ToolCall::new("c2", "web.search", json!({"query": "rust"})),
        ToolCall::new("c3", "http.get", json!({"url": "https://example.com"})),
        ToolCall::new("c4", "email.send", json!({"to": "a@b.c", "subject": "hi"})),
    ];
    let results = executor.execute_batch(&calls).await;

    assert_eq!(results.len(), 4);
    // c1: structured validation refusal, byte-exact contract.
    let refusal = results[0].content.as_deref().unwrap();
    let violations = parse_argument_validation_refusal(refusal)
        .unwrap_or_else(|| panic!("c1 should be a structured refusal: {refusal}"));
    assert_eq!(violations.len(), 2);
    // c2: the repaired call succeeds.
    assert!(results[1]
        .content
        .as_deref()
        .unwrap()
        .contains("\"query\":\"rust\""));
    // c3: middleware rejection — opaque to the roll-up's structured tier.
    let blocked = results[2].content.as_deref().unwrap();
    assert!(blocked.starts_with("ERROR: "));
    assert!(parse_argument_validation_refusal(blocked).is_none());
    // c4: narrowed away at construction — unknown tool.
    assert!(results[3]
        .content
        .as_deref()
        .unwrap()
        .contains("unknown tool"));
}

// ---------- toolset combinator algebra ----------

struct Lookup;

#[async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "Looks up a record."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, args: Value) -> Result<Value> {
        Ok(json!({"id": args.get("id").cloned().unwrap_or(Value::Null)}))
    }
}

fn combinator_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Search);
    registry.register(Fetch);
    registry.register(Lookup);
    registry
}

#[tokio::test]
async fn prefixed_name_dispatches_to_inner_tool() {
    let registry = combinator_registry();
    let prefixed = prefixed("crm_", &registry);

    // The prefixed registry exposes the new names.
    assert!(prefixed.contains("crm_lookup"));
    assert!(!prefixed.contains("lookup"));

    // Dispatch by the prefixed name reaches the inner tool.
    let tool = prefixed.get("crm_lookup").unwrap();
    let result = tool.call(json!({"id": "42"})).await.unwrap();
    assert_eq!(result["id"], json!("42"));

    // The wrapper records the prefixed name as its effect kind.
    assert_eq!(tool.effect_kind(), "crm_lookup");
}

#[test]
fn filtered_registry_contains_only_matching_tools() {
    let registry = combinator_registry();
    let predicate = ToolPredicate::ByName {
        names: vec!["web.search".into(), "lookup".into()],
    };
    let filtered = filtered(predicate, &registry);

    assert_eq!(filtered.len(), 2);
    assert!(filtered.contains("web.search"));
    assert!(filtered.contains("lookup"));
    assert!(!filtered.contains("http.get"));
}

#[tokio::test]
async fn nested_filtered_then_prefixed() {
    let registry = combinator_registry();
    let predicate = ToolPredicate::ByName {
        names: vec!["web.search".into(), "lookup".into()],
    };
    // First filter, then prefix: only the filtered tools get prefixed.
    let nested = prefixed("crm_", &filtered(predicate, &registry));

    assert_eq!(nested.len(), 2);
    assert!(nested.contains("crm_web.search"));
    assert!(nested.contains("crm_lookup"));
    assert!(!nested.contains("crm_http.get"));

    // Dispatch still works through both layers.
    let tool = nested.get("crm_lookup").unwrap();
    let result = tool.call(json!({"id": "99"})).await.unwrap();
    assert_eq!(result["id"], json!("99"));
}

#[test]
fn toolset_spec_round_trips() {
    let spec = ToolsetSpec::Prefixed {
        prefix: "crm_".into(),
        inner: Box::new(ToolsetSpec::Filtered {
            predicate: ToolPredicate::ByName {
                names: vec!["lookup".into()],
            },
            inner: Box::new(ToolsetSpec::Base),
        }),
    };

    let wire = serde_json::to_string(&spec).unwrap();
    let parsed: ToolsetSpec = serde_json::from_str(&wire).unwrap();
    assert_eq!(parsed, spec);
}

#[test]
fn apply_spec_resolves_nested_stack() {
    let base = combinator_registry();
    let spec = ToolsetSpec::Prefixed {
        prefix: "crm_".into(),
        inner: Box::new(ToolsetSpec::Filtered {
            predicate: ToolPredicate::ByName {
                names: vec!["lookup".into(), "http.get".into()],
            },
            inner: Box::new(ToolsetSpec::Base),
        }),
    };

    let resolved = apply_spec(&base, &spec).unwrap();
    assert_eq!(resolved.len(), 2);
    assert!(resolved.contains("crm_lookup"));
    assert!(resolved.contains("crm_http.get"));
    assert!(!resolved.contains("crm_web.search"));
}

#[tokio::test]
async fn wrapper_preserves_effect_and_schema() {
    let registry = combinator_registry();
    let original = registry.get("lookup").unwrap();

    // Prefixed wrapper preserves effect class and schema.
    let prefixed_tool = Arc::new(PrefixedTool::new(original.clone(), "crm_"));
    assert_eq!(prefixed_tool.effect(), Effect::ReadOnly);
    assert_eq!(
        prefixed_tool.parameters_schema(),
        original.parameters_schema()
    );

    // Effect request delegates to inner (same hash, same idempotency).
    let call = ToolCall::new("c1", "crm_lookup", json!({"id": "x"}));
    assert_eq!(
        prefixed_tool.effect_request(&call),
        original.effect_request(&call)
    );
}

#[tokio::test]
async fn prepared_tool_overrides_presentation_but_validates_inner_schema() {
    let registry = combinator_registry();
    let original = registry.get("lookup").unwrap();

    let prepared = Arc::new(
        PreparedTool::new(
            original.clone(),
            "find_record",
            "Finds a record by identifier.",
            json!({"type": "object", "properties": {"identifier": {"type": "string"}}, "required": ["identifier"]}),
        )
        .unwrap(),
    );

    // Model-facing surface is overridden.
    assert_eq!(prepared.name(), "find_record");
    assert_eq!(prepared.description(), "Finds a record by identifier.");
    assert_ne!(prepared.parameters_schema(), original.parameters_schema());

    // Effect class and kind are preserved from inner.
    assert_eq!(prepared.effect(), Effect::ReadOnly);
    assert_eq!(prepared.effect_kind(), "find_record");

    // Dispatch with valid args against the INNER schema succeeds.
    let result = prepared.call(json!({"id": "42"})).await.unwrap();
    assert_eq!(result["id"], json!("42"));
}

#[tokio::test]
async fn prepared_tool_refuses_invalid_args_via_inner_schema() {
    let registry = combinator_registry();
    let original = registry.get("lookup").unwrap();

    let prepared = Arc::new(
        PreparedTool::new(
            original.clone(),
            "find_record",
            "Finds a record by identifier.",
            json!({"type": "object", "properties": {"identifier": {"type": "string"}}, "required": ["identifier"]}),
        )
        .unwrap(),
    );

    // Missing required property `id` in the INNER schema → refusal.
    let refusal = prepared.call(json!({})).await.unwrap();
    let Value::String(payload) = &refusal else {
        panic!("expected structured refusal, got {refusal:?}");
    };
    assert!(
        parse_argument_validation_refusal(payload).is_some(),
        "refusal should parse as argument_validation: {payload}"
    );
}

#[test]
fn prepared_spec_round_trips() {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "lookup".into(),
        PreparedOverride {
            name: "find_record".into(),
            description: "Finds a record.".into(),
            parameters_schema: json!({"type": "object"}),
        },
    );
    let spec = ToolsetSpec::Prepared {
        overrides,
        inner: Box::new(ToolsetSpec::Base),
    };
    let wire = serde_json::to_string(&spec).unwrap();
    let parsed: ToolsetSpec = serde_json::from_str(&wire).unwrap();
    assert_eq!(parsed, spec);
}

#[test]
fn apply_spec_prepared_resolves_overrides() {
    let base = combinator_registry();
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "lookup".into(),
        PreparedOverride {
            name: "find_record".into(),
            description: "Finds a record.".into(),
            parameters_schema: json!({"type": "object"}),
        },
    );
    let spec = ToolsetSpec::Prepared {
        overrides,
        inner: Box::new(ToolsetSpec::Base),
    };

    let resolved = apply_spec(&base, &spec).unwrap();
    assert!(resolved.contains("find_record"));
    assert!(!resolved.contains("lookup"));
    assert!(resolved.contains("web.search"));
    assert!(resolved.contains("http.get"));
}

#[tokio::test]
async fn defer_loading_starts_with_discovery_only() {
    let base = combinator_registry();
    let defer = DeferLoadingRegistry::new(&base);
    let registry = defer.registry();

    assert_eq!(registry.len(), 1);
    assert!(registry.contains("tool_discovery"));
    assert!(!registry.contains("lookup"));
    assert!(!defer.fully_revealed());
}

#[tokio::test]
async fn defer_loading_reveals_matching_tools() {
    let base = combinator_registry();
    let defer = DeferLoadingRegistry::new(&base);

    // Discover tools matching "look"
    let discovery = defer.registry().get("tool_discovery").unwrap();
    let result = discovery.call(json!({"query": "look"})).await.unwrap();
    assert_eq!(result["revealed"], json!(["lookup"]));

    // Revealed tool is now in the visible registry.
    let visible = defer.registry();
    assert!(visible.contains("lookup"));
    assert!(visible.contains("tool_discovery"));

    // Other tools remain hidden.
    assert!(!visible.contains("web.search"));
    assert!(!visible.contains("http.get"));
}

#[tokio::test]
async fn defer_loading_reveal_all_exhausts_hidden_pool() {
    let base = combinator_registry();
    let defer = DeferLoadingRegistry::new(&base);

    let discovery = defer.registry().get("tool_discovery").unwrap();
    let result = discovery.call(json!({"query": ""})).await.unwrap();
    let revealed = result["revealed"].as_array().unwrap();
    assert_eq!(revealed.len(), 3);

    assert!(defer.fully_revealed());
    let visible = defer.registry();
    assert_eq!(visible.len(), 4); // 3 revealed + discovery tool
}
