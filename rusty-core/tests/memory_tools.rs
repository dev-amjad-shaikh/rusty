//! The agent's memory tool surface (EP-06-S03): the append/replace pair
//! declared as engine-state writes, mounted through the in-process MCP
//! bridge; the block's version guard and char limit as model-visible
//! rejections; write-time recall annotations persisted on the record; the
//! repair envelope on schema violations; and the side-session catalog
//! narrow by construction.

use std::sync::Arc;

use rusty_agent_runtime::journal::Clock;
use rusty_agent_runtime::mcp::InProcessMcpBridge;
use rusty_agent_runtime::memory::{
    InMemoryMemoryStore, MemoryQuery, MemoryScope, MemoryStore, ScopeAddress,
};
use rusty_agent_runtime::memory_tools::{
    MEMORY_APPEND_ENTRY_TOOL, MEMORY_REPLACE_BLOCK_TOOL, MemoryToolset, maintenance_names,
};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::{
    EffectClass, Placement, SandboxRequirement, Tool, ToolRegistry, resolve_placement,
};
use rusty_agent_runtime::tool_select::{ToolPredicate, ValidatingTool, filtered};
use serde_json::{Value, json};

fn toolset(store: Arc<dyn MemoryStore>) -> MemoryToolset {
    MemoryToolset::new(
        store,
        "support-1",
        ScopeAddress::new(MemoryScope::Agent, "support-1"),
        Clock::logical(1_700_000_000_000, 1_000),
    )
}

fn store() -> Arc<dyn MemoryStore> {
    Arc::new(InMemoryMemoryStore::new())
}

/// Call one tool out of a registry by name.
async fn call(
    registry: &ToolRegistry,
    name: &str,
    args: Value,
) -> rusty_agent_runtime::error::Result<Value> {
    registry
        .get(name)
        .expect("tool registered")
        .call(args)
        .await
}

/// The one record the default query serves for `key`, when any.
async fn current(store: &Arc<dyn MemoryStore>, key: &str) -> Option<Value> {
    let query = MemoryQuery {
        key: Some(key.to_owned()),
        ..MemoryQuery::default()
    };
    store
        .query(&query, Clock::logical(1_800_000_000_000, 1_000).now())
        .await
        .unwrap()
        .into_iter()
        .next()
        .map(|record| serde_json::to_value(record).unwrap())
}

// --------------------------------------------------------------------- //
// AC 1: engine-state tools, mounted through the in-process bridge
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_tools_declare_engine_state_writes_and_mount_through_the_bridge() {
    let store = store();
    let registry = toolset(Arc::clone(&store)).registry();

    for name in [MEMORY_APPEND_ENTRY_TOOL, MEMORY_REPLACE_BLOCK_TOOL] {
        let tool = registry.get(name).expect("registered");
        assert_eq!(tool.effect_class(), EffectClass::Write);
        assert_eq!(tool.sandbox_requirement(), SandboxRequirement::None);
        assert_eq!(tool.effect(), Effect::Idempotent);
        assert_eq!(
            resolve_placement(tool.as_ref(), false).unwrap(),
            Placement::InProcess,
            "an engine-state write needs no sandbox"
        );
    }

    // Mounted through the in-process MCP bridge (EP-05-S09) once Idempotent
    // joins the allowed effects: discovery lists them, dispatch executes.
    let bridge = InProcessMcpBridge::new(Arc::new(registry)).with_allowed_effects(vec![
        Effect::Pure,
        Effect::ReadOnly,
        Effect::Idempotent,
    ]);
    let client = bridge.client().unwrap();
    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(names.contains(&MEMORY_APPEND_ENTRY_TOOL));
    assert!(names.contains(&MEMORY_REPLACE_BLOCK_TOOL));

    let result = client
        .call_tool(
            MEMORY_APPEND_ENTRY_TOOL,
            json!({"content": {"fact": "vpn drops"}, "key": "vpn-stability"}),
        )
        .await
        .unwrap();
    // The bridge serves the tool's JSON result as a text content item.
    let result: Value =
        serde_json::from_str(result.as_str().expect("text result")).expect("json result");
    assert_eq!(result["inserted"], json!(true));
    assert!(result["memory_id"].as_str().unwrap().len() == 64);
    assert!(current(&store, "vpn-stability").await.is_some());
}

#[tokio::test]
async fn a_replayed_append_converges_on_the_content_address() {
    let store = store();
    let registry = toolset(Arc::clone(&store)).registry();
    let args = json!({"content": {"fact": "vpn drops"}, "key": "vpn-stability"});

    // The logical clock ticks per read, so pin timestamps equal by
    // appending twice through one fresh clock each: identical content and
    // provenance derive the identical address.
    let first = call(&registry, MEMORY_APPEND_ENTRY_TOOL, args.clone())
        .await
        .unwrap();
    assert_eq!(first["inserted"], json!(true));

    let registry_again = toolset(Arc::clone(&store)).registry();
    let second = call(&registry_again, MEMORY_APPEND_ENTRY_TOOL, args)
        .await
        .unwrap();
    assert_eq!(second["memory_id"], first["memory_id"]);
    assert_eq!(second["inserted"], json!(false), "the replay converges");
}

// --------------------------------------------------------------------- //
// AC 3: write-time annotations persist on the record
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_append_persists_write_time_recall_annotations() {
    let store = store();
    let registry = toolset(Arc::clone(&store)).registry();

    let result = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({
            "content": {"tone": "warm"},
            "kind": "preference",
            "key": "tone",
            "trigger_phrases": ["draft reply", "customer note"],
            "importance": 7,
            "confidence": 0.75,
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["inserted"], json!(true));

    let record = current(&store, "tone").await.expect("stored");
    assert_eq!(record["kind"], json!("preference"));
    assert_eq!(record["tags"], json!(["draft reply", "customer note"]));
    assert_eq!(record["priority"], json!(7));
    assert_eq!(record["provenance"]["author"]["type"], json!("agent"));
    assert_eq!(
        record["provenance"]["author"]["agent_id"],
        json!("support-1")
    );

    // Lane-one reads the annotations with zero model calls: tag equality.
    let tagged = store
        .query(
            &MemoryQuery {
                tags: vec!["draft reply".to_owned()],
                ..MemoryQuery::default()
            },
            Clock::logical(1_800_000_000_000, 1_000).now(),
        )
        .await
        .unwrap();
    assert_eq!(tagged.len(), 1);
}

#[tokio::test]
async fn the_append_validates_and_supersedes_by_key() {
    let store = store();
    let registry = toolset(Arc::clone(&store)).registry();

    call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": {"v": 1}, "key": "slot"}),
    )
    .await
    .unwrap();
    let replaced = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": {"v": 2}, "key": "slot", "supersedes_key": "slot"}),
    )
    .await
    .unwrap();
    assert!(replaced["supersedes"].as_str().is_some());

    // The unknown supersession key, a broken confidence, and an
    // out-of-band importance are model-visible refusals.
    let error = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": {"v": 3}, "supersedes_key": "ghost"}),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("nothing keyed `ghost`"),
        "{error}"
    );

    let error = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": 1, "confidence": 0.0}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("(0, 1]"), "{error}");

    let error = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": 1, "importance": 11}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("0..=10"), "{error}");
}

// --------------------------------------------------------------------- //
// AC 2: the block write's version guard and char limit
// --------------------------------------------------------------------- //

#[tokio::test]
async fn the_block_write_supersedes_under_a_version_guard() {
    let store = store();
    let registry = toolset(Arc::clone(&store)).registry();

    // No block yet: the model-visible refusal says to append first.
    let error = call(
        &registry,
        MEMORY_REPLACE_BLOCK_TOOL,
        json!({"key": "profile", "content": {"name": "Ada"}}),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("append the entry first"),
        "{error}"
    );

    let appended = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": {"name": "Ada"}, "key": "profile", "importance": 9}),
    )
    .await
    .unwrap();
    let original_id = appended["memory_id"].as_str().unwrap().to_string();

    // A stale guard refuses and names the current id.
    let error = call(
        &registry,
        MEMORY_REPLACE_BLOCK_TOOL,
        json!({"key": "profile", "content": {"name": "Grace"}, "expected_memory_id": "stale"}),
    )
    .await
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("version conflict"), "{message}");
    assert!(message.contains(&original_id), "{message}");

    // The right guard writes: the chain supersedes, the annotations
    // (priority 9) carry over, and default recall serves the new value.
    let replaced = call(
        &registry,
        MEMORY_REPLACE_BLOCK_TOOL,
        json!({
            "key": "profile",
            "content": {"name": "Grace"},
            "expected_memory_id": original_id,
        }),
    )
    .await
    .unwrap();
    assert_eq!(replaced["supersedes"], appended["memory_id"]);
    let record = current(&store, "profile").await.expect("current");
    assert_eq!(record["content"]["kind"], json!("inline"));
    assert_eq!(record["content"]["value"], json!({"name": "Grace"}));
    assert_eq!(
        record["priority"],
        json!(9),
        "the edit preserves annotations"
    );

    // The superseded record is retained as evidence.
    let all = store
        .query(
            &MemoryQuery {
                key: Some("profile".to_owned()),
                include_superseded: true,
                ..MemoryQuery::default()
            },
            Clock::logical(1_800_000_000_000, 1_000).now(),
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn the_block_write_enforces_the_char_limit() {
    let store = store();
    let registry = toolset(Arc::clone(&store))
        .with_block_char_limit(16)
        .registry();
    call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": "short", "key": "bio"}),
    )
    .await
    .unwrap();

    let error = call(
        &registry,
        MEMORY_REPLACE_BLOCK_TOOL,
        json!({"key": "bio", "content": "this value is far too long for the slot"}),
    )
    .await
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("char limit is 16"), "{message}");
}

// --------------------------------------------------------------------- //
// AC 4: schema violations answer with the repair envelope
// --------------------------------------------------------------------- //

#[tokio::test]
async fn schema_violations_answer_with_the_repair_envelope_before_the_tool_runs() {
    let store = store();
    let registry = ValidatingTool::wrap_registry(&toolset(Arc::clone(&store)).registry());

    // Importance outside 0–10 never reaches the tool: the refusal payload
    // is the conversational-repair contract (EP-05-S03).
    let result = call(
        &registry,
        MEMORY_APPEND_ENTRY_TOOL,
        json!({"content": 1, "importance": 11}),
    )
    .await
    .expect("the envelope is the tool's result, not a dispatch error");
    let payload = result.as_str().expect("the refusal payload");
    assert!(payload.contains("argument_validation"), "{payload}");
    assert!(payload.contains("importance"), "{payload}");
    assert!(store.all().await.unwrap().is_empty(), "the tool never ran");

    // A missing required argument takes the same path.
    let result = call(&registry, MEMORY_REPLACE_BLOCK_TOOL, json!({"key": "x"}))
        .await
        .unwrap();
    let payload = result.as_str().unwrap();
    assert!(payload.contains("argument_validation"), "{payload}");
}

// --------------------------------------------------------------------- //
// AC 5: the side-session catalog is narrow by construction
// --------------------------------------------------------------------- //

/// A tool a memory-maintenance session must never hold.
struct MessageTool;

#[async_trait::async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "send_message"
    }
    fn description(&self) -> &str {
        "Send a message."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn call(&self, _args: Value) -> rusty_agent_runtime::error::Result<Value> {
        Ok(json!("sent"))
    }
}

#[tokio::test]
async fn the_maintenance_catalog_holds_only_the_memory_tools() {
    let store = store();
    let toolset = toolset(Arc::clone(&store));

    let maintenance = toolset.maintenance_registry();
    let names: Vec<&str> = maintenance.names().collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&MEMORY_APPEND_ENTRY_TOOL));
    assert!(names.contains(&MEMORY_REPLACE_BLOCK_TOOL));

    // Composed out of a fuller registry, `filtered` narrows to exactly the
    // maintenance names: message sending is absent by construction, not by
    // instruction (EP-05-S04).
    let mut full = toolset.registry();
    full.register(MessageTool);
    let narrowed = filtered(
        ToolPredicate::ByName {
            names: maintenance_names(),
        },
        &full,
    );
    let names: Vec<&str> = narrowed.names().collect();
    assert_eq!(names.len(), 2);
    assert!(!names.contains(&"send_message"));
}
