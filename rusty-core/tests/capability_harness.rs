use std::fs;

use async_trait::async_trait;
use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::builtins::{
    CalculatorTool, KnowledgeDocument, KnowledgeSearchTool, SandboxedDocumentReaderTool,
    TextInspectorTool,
};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use serde_json::{json, Value};

#[test]
fn executable_catalog_is_sorted_and_exact() {
    let mut tools = ToolRegistry::new();
    tools.register(TextInspectorTool);
    tools.register(CalculatorTool);

    let catalog = tools.capabilities().expect("valid built-in catalog");
    assert_eq!(
        catalog
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["calculator", "inspect_text"]
    );
    assert_eq!(catalog[0].effect, Effect::Pure);
    assert_eq!(
        catalog[0].parameters_schema["required"],
        json!(["operation", "left", "right"])
    );
    assert!(catalog[1].description.contains("Unicode"));
}

#[tokio::test]
async fn native_pack_executes_with_structured_results() {
    let calculator = CalculatorTool;
    assert_eq!(
        calculator
            .call(json!({"operation": "multiply", "left": 7, "right": 6}))
            .await
            .unwrap(),
        json!({"result": 42.0})
    );
    assert!(calculator
        .call(json!({"operation": "divide", "left": 1, "right": 0}))
        .await
        .unwrap_err()
        .to_string()
        .contains("division by zero"));

    let inspector = TextInspectorTool;
    assert_eq!(
        inspector
            .call(json!({"text": "one two\nthree"}))
            .await
            .unwrap(),
        json!({"words": 3, "characters": 13, "bytes": 13, "lines": 2})
    );
}

#[tokio::test]
async fn knowledge_search_is_bounded_ranked_and_cited() {
    let search = KnowledgeSearchTool::new(vec![
        KnowledgeDocument {
            id: "runtime".into(),
            title: "Rusty runtime".into(),
            text: "Durable graphs execute typed tools and record exact evidence.".into(),
        },
        KnowledgeDocument {
            id: "studio".into(),
            title: "Rusty Studio".into(),
            text: "Studio creates agents and opens their run traces.".into(),
        },
    ])
    .unwrap();

    let result = search
        .call(json!({"query": "Rusty tools evidence", "limit": 1}))
        .await
        .unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(result["results"][0]["id"], json!("runtime"));
    assert!(result["results"][0]["excerpt"]
        .as_str()
        .unwrap()
        .contains("exact evidence"));
}

#[tokio::test]
async fn document_reader_accepts_text_formats_and_refuses_escape() {
    let root = std::env::temp_dir().join(format!("rusty-doc-reader-{}", uuid::Uuid::new_v4()));
    let outside = std::env::temp_dir().join(format!("rusty-doc-outside-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("guide.md"), "# Guide\nUse exact evidence.").unwrap();
    fs::write(
        root.join("inventory.csv"),
        "name,effect\nreader,read_only\n",
    )
    .unwrap();
    fs::write(root.join("policy.json"), r#"{"approval":"required"}"#).unwrap();
    fs::write(&outside, "private").unwrap();
    let reader = SandboxedDocumentReaderTool::new(&root).unwrap();

    let result = reader.call(json!({"path": "guide.md"})).await.unwrap();
    assert_eq!(result["kind"], json!("markdown"));
    assert_eq!(result["content"], json!("# Guide\nUse exact evidence."));
    assert_eq!(
        reader.call(json!({"path": "inventory.csv"})).await.unwrap()["kind"],
        json!("csv")
    );
    assert_eq!(
        reader.call(json!({"path": "policy.json"})).await.unwrap()["kind"],
        json!("json")
    );

    let error = reader
        .call(json!({"path": format!("../{}", outside.file_name().unwrap().to_string_lossy())}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("inside the configured root"));

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(outside);
}

struct InvalidTool;

#[async_trait]
impl Tool for InvalidTool {
    fn name(&self) -> &str {
        "bad tool"
    }

    fn description(&self) -> &str {
        "Not advertisable."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _args: Value) -> Result<Value> {
        Err(RustyError::Tool("must not execute".into()))
    }
}

#[test]
fn invalid_executable_contract_never_becomes_catalog_truth() {
    let mut tools = ToolRegistry::new();
    tools.register(InvalidTool);
    let error = tools.capabilities().unwrap_err();
    assert!(error.to_string().contains("tool name"));
}

#[test]
fn registry_restriction_is_exact_and_fail_closed() {
    let mut tools = ToolRegistry::new();
    tools.register(TextInspectorTool);
    tools.register(CalculatorTool);

    let selected = tools
        .restricted_to(&["inspect_text".to_string()])
        .expect("known subset");
    assert_eq!(selected.names().collect::<Vec<_>>(), ["inspect_text"]);
    assert!(tools
        .restricted_to(&["missing".to_string()])
        .unwrap_err()
        .to_string()
        .contains("not registered"));
    assert!(tools
        .restricted_to(&["calculator".to_string(), "calculator".to_string()])
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
}

// --------------------------------------------------------------------- //
// Resolved capability sets
// --------------------------------------------------------------------- //

use rusty_agent_runtime::capability::{CapabilityRef, CapabilitySet};
use rusty_agent_runtime::record::RunManifest;

fn pack() -> (ToolRegistry, Vec<rusty_agent_runtime::tool::ToolCapability>) {
    let mut tools = ToolRegistry::new();
    tools.register(TextInspectorTool);
    tools.register(CalculatorTool);
    let catalog = tools.capabilities().expect("valid built-in catalog");
    (tools, catalog)
}

#[test]
fn compose_addresses_members_canonically() {
    let (_registry, catalog) = pack();
    let names = || vec!["calculator".to_string(), "inspect_text".to_string()];

    let set = CapabilitySet::compose(&names(), &[], &catalog).expect("known members");
    assert!(set.id().starts_with("cs-"));
    assert_eq!(set.id().len(), "cs-".len() + 64);

    // Member order is not identity: the same members in another order are
    // the same set.
    let mut reversed = names();
    reversed.reverse();
    let same = CapabilitySet::compose(&reversed, &[], &catalog).unwrap();
    assert_eq!(set.id(), same.id());

    // Membership is identity: one fewer member is a different set.
    let narrower = CapabilitySet::compose(&names()[..1], &[], &catalog).unwrap();
    assert_ne!(set.id(), narrower.id());

    // Skill/connector references ride under the same address.
    let with_refs = CapabilitySet::compose(
        &names(),
        &[CapabilityRef::skill("research-pack@1.2.0").unwrap()],
        &catalog,
    )
    .unwrap();
    assert_ne!(set.id(), with_refs.id());
    assert_eq!(with_refs.refs()[0].kind.as_str(), "skill");
}

#[test]
fn compose_fails_closed_on_unknown_and_duplicate_members() {
    let (_registry, catalog) = pack();

    let unknown = CapabilitySet::compose(&["web_search".to_string()], &[], &catalog)
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("web_search"), "got: {unknown}");
    assert!(unknown.contains("does not advertise"), "got: {unknown}");

    let duplicate = CapabilitySet::compose(
        &["calculator".to_string(), "calculator".to_string()],
        &[],
        &catalog,
    )
    .unwrap_err()
    .to_string();
    assert!(duplicate.contains("duplicate"), "got: {duplicate}");

    let bad_ref = CapabilityRef::skill("  padded").unwrap_err().to_string();
    assert!(bad_ref.contains("trimmed"), "got: {bad_ref}");
}

#[test]
fn empty_set_is_a_legitimate_tool_free_composition() {
    let (_registry, catalog) = pack();
    let set = CapabilitySet::compose(&[], &[], &catalog).expect("empty set");
    assert!(set.is_empty());
    assert!(set.resolve_allowlist().is_empty());
    // The empty set still has a stable content address: tool-free is a
    // declared composition, not the absence of one.
    assert!(set.id().starts_with("cs-"));
    let again = CapabilitySet::compose(&[], &[], &catalog).unwrap();
    assert_eq!(set.id(), again.id());
}

#[test]
fn resolution_feeds_the_executor_allowlist_contract() {
    let (registry, catalog) = pack();
    let set = CapabilitySet::compose(
        &["inspect_text".to_string(), "calculator".to_string()],
        &[],
        &catalog,
    )
    .unwrap();

    // The resolved allowlist is exactly what `RunConfig::tool_allowlist`
    // and `ToolRegistry::restricted_to` consume: sorted, deduped names.
    let allowlist = set.resolve_allowlist();
    assert_eq!(allowlist, ["calculator", "inspect_text"]);
    let narrowed = registry.restricted_to(&allowlist).expect("resolves");
    assert_eq!(narrowed.len(), 2);

    set.validate_against(&registry).expect("all members live");
    let mut reduced = ToolRegistry::new();
    reduced.register(CalculatorTool);
    let error = set.validate_against(&reduced).unwrap_err().to_string();
    assert!(error.contains("inspect_text"), "got: {error}");
}

#[test]
fn child_inheritance_intersects_and_never_widens() {
    let (_registry, catalog) = pack();
    let parent = CapabilitySet::compose(&["calculator".to_string()], &[], &catalog).unwrap();
    let declared = CapabilitySet::compose(
        &["calculator".to_string(), "inspect_text".to_string()],
        &[],
        &catalog,
    )
    .unwrap();

    // The child declared a superset; it resolves only what the parent held.
    let child = CapabilitySet::intersect_for_child(&parent, &declared);
    assert_eq!(child.tools(), &["calculator"]);
    assert_eq!(child.id(), parent.id());
    // The resolved id differs from the declared one: the manifest pins the
    // honest, narrowed composition.
    assert_ne!(child.id(), declared.id());

    // A disjoint declaration resolves to the empty (tool-free) set — never
    // to the declared members.
    let stranger = CapabilitySet::compose(&["inspect_text".to_string()], &[], &catalog).unwrap();
    let resolved = CapabilitySet::intersect_for_child(&parent, &stranger);
    assert!(resolved.is_empty());
}

#[test]
fn replay_fails_typed_when_a_member_left_the_registry() {
    let (registry, catalog) = pack();
    let pinned = CapabilitySet::compose(
        &["calculator".to_string(), "inspect_text".to_string()],
        &[],
        &catalog,
    )
    .unwrap();

    // The same registry re-resolves the same set: replay may proceed.
    pinned.replay_guard(&registry).expect("members intact");
    let reresolved = CapabilitySet::compose(pinned.tools(), pinned.refs(), &catalog).unwrap();
    assert_eq!(pinned.id(), reresolved.id());

    // A registry that dropped a member refuses typed, never silently
    // narrowing the replayed run.
    let mut current = ToolRegistry::new();
    current.register(CalculatorTool);
    let error = pinned.replay_guard(&current).unwrap_err();
    assert!(
        matches!(error, RustyError::Replay(_)),
        "expected typed replay error, got: {error:?}"
    );
    assert!(error.to_string().contains("inspect_text"));
    assert!(error.to_string().contains(pinned.id()));
}

#[test]
fn manifest_pins_the_set_and_unpinned_manifests_stay_byte_stable() {
    let (_registry, catalog) = pack();
    let set = CapabilitySet::compose(&["calculator".to_string()], &[], &catalog).unwrap();

    // A manifest with no capability pin is byte-identical to before: the
    // field is absent from the wire, never null.
    assert_eq!(serde_json::to_value(RunManifest::new()).unwrap(), json!({}));

    let manifest = RunManifest::new()
        .pin_prompt("system", "Be brief.")
        .pin_capability_set(&set);
    let value = serde_json::to_value(&manifest).unwrap();
    assert_eq!(value["capability_set"], json!(set.id()));
    let back: RunManifest = serde_json::from_value(value).unwrap();
    assert_eq!(back, manifest);
}

#[test]
fn serde_round_trip_recomputes_the_address() {
    let (_registry, catalog) = pack();
    let set = CapabilitySet::compose(
        &["calculator".to_string()],
        &[CapabilityRef::connector("search@prod").unwrap()],
        &catalog,
    )
    .unwrap();

    // The wire shape carries members only; the id is derived on read.
    let value = serde_json::to_value(&set).unwrap();
    assert!(value.get("id").is_none());
    let back: CapabilitySet = serde_json::from_value(value).unwrap();
    assert_eq!(back, set);
    assert_eq!(back.id(), set.id());

    // A tampered member list cannot smuggle a stale address: the id is
    // recomputed from what actually arrives.
    let tampered = json!({"tools": ["calculator", "inspect_text"], "refs": []});
    let widened: CapabilitySet = serde_json::from_value(tampered).unwrap();
    assert_ne!(widened.id(), set.id());
}
