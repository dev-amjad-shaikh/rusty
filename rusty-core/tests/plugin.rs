//! Plugin kernel integration tests.
//!
//! The kernel's contract, exercised through the public API against the
//! real registries:
//!
//! - a registration's guard dropping (unload, failed apply, kernel drop)
//!   removes exactly that registration from the dispatch surface;
//! - an `apply` that fails — or panics — midway leaves no
//!   half-registrations behind (the kernel's honesty property);
//! - unload order across plugins is reverse load order (LIFO), observed
//!   through the dropped tools themselves;
//! - duplicate identities are refused, while a failed tombstone does not
//!   claim its identity;
//! - hot reload proves the old registrations are gone before the new
//!   `apply` runs — proven by the new plugin itself, at apply time;
//! - an unloaded plugin's tool can no longer be dispatched, asserted
//!   through `ToolExecutor` against a fresh registry snapshot.
//!
//! The `wasm`-gated group loads a real capsule through the bridge:
//! hand-written component text compiled by wasmtime's `wat` support, the
//! `tests/capsule.rs` convention — no guest toolchain required.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::llm::ToolCall;
use rusty_agent_runtime::plugin::{FiberState, Plugin, PluginContext, PluginKernel};
use rusty_agent_runtime::tool::{Tool, ToolExecutor, ToolRegistry};

/// A tool answering a fixed string; `log_drop` tools record their own
/// destruction so unload order is observable through the real path.
struct FixedTool {
    name: String,
    output: String,
    drop_log: Option<Arc<Mutex<Vec<String>>>>,
}

impl FixedTool {
    fn new(name: &str, output: &str) -> Self {
        Self {
            name: name.to_owned(),
            output: output.to_owned(),
            drop_log: None,
        }
    }
}

impl Drop for FixedTool {
    fn drop(&mut self) {
        if let Some(log) = &self.drop_log {
            log.lock().unwrap().push(self.name.clone());
        }
    }
}

#[async_trait]
impl Tool for FixedTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "A fixed-answer test tool."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _args: Value) -> Result<Value> {
        Ok(Value::String(self.output.clone()))
    }
}

/// A plugin registering the named tools in order; `fail_at` / `panic_at`
/// plant a failure after that many registrations. Holds only name/output
/// pairs — the registered tools are built in `apply`, so the plugin
/// value's own drop never touches the drop log.
struct ScriptedPlugin {
    id: String,
    tools: Vec<(String, String)>,
    drop_log: Option<Arc<Mutex<Vec<String>>>>,
    fail_at: Option<usize>,
    panic_at: Option<usize>,
}

impl ScriptedPlugin {
    fn new(id: &str, tools: Vec<FixedTool>) -> Self {
        Self {
            id: id.to_owned(),
            tools: tools
                .into_iter()
                .map(|tool| (tool.name.clone(), tool.output.clone()))
                .collect(),
            drop_log: None,
            fail_at: None,
            panic_at: None,
        }
    }

    /// A one-tool plugin whose registered tool records its own drop.
    fn logging(id: &str, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            drop_log: Some(log),
            ..Self::new(id, vec![FixedTool::new(id, "")])
        }
    }

    fn failing_after(id: &str, tools: Vec<FixedTool>, count: usize) -> Self {
        Self {
            fail_at: Some(count),
            ..Self::new(id, tools)
        }
    }
}

impl Plugin for ScriptedPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn apply(&self, ctx: &mut PluginContext) -> Result<()> {
        for (index, (name, output)) in self.tools.iter().enumerate() {
            if self.fail_at == Some(index) {
                return Err(rusty_agent_runtime::error::RustyError::Plugin(
                    "scripted failure".to_owned(),
                ));
            }
            if self.panic_at == Some(index) {
                panic!("scripted panic");
            }
            ctx.register_tool(FixedTool {
                name: name.clone(),
                output: output.clone(),
                drop_log: self.drop_log.clone(),
            })?;
        }
        Ok(())
    }
}

/// Dispatch one call through the real executor path over a fresh snapshot.
async fn dispatch(kernel: &PluginKernel, tool: &str) -> String {
    let executor = ToolExecutor::new(kernel.tools());
    let results = executor
        .execute_batch(&[ToolCall::new("c1", tool, json!({}))])
        .await;
    results[0].content.clone().unwrap()
}

#[tokio::test]
async fn unload_removes_registrations_from_the_dispatch_surface() {
    let mut kernel = PluginKernel::new(ToolRegistry::new());
    kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "greeter",
                vec![
                    FixedTool::new("greet", "hello"),
                    FixedTool::new("farewell", "bye"),
                ],
            )),
            json!({}),
        )
        .unwrap();

    assert_eq!(dispatch(&kernel, "greet").await, "hello");
    assert_eq!(dispatch(&kernel, "farewell").await, "bye");

    kernel.unload("greeter").unwrap();

    assert!(!kernel.tools().contains("greet"));
    assert!(!kernel.tools().contains("farewell"));
    let gone = dispatch(&kernel, "greet").await;
    assert!(
        gone.starts_with("ERROR:") && gone.contains("unknown tool"),
        "got: {gone}"
    );
}

#[tokio::test]
async fn midway_apply_failure_unwinds_everything() {
    let mut kernel = PluginKernel::new(ToolRegistry::new());
    let error = kernel
        .load(
            Box::new(ScriptedPlugin::failing_after(
                "half",
                vec![
                    FixedTool::new("registered-a", "a"),
                    FixedTool::new("registered-b", "b"),
                    FixedTool::new("never", "n"),
                ],
                2,
            )),
            json!({}),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("scripted failure"),
        "got: {error}"
    );

    // The honesty property: the two completed registrations are gone too.
    let snapshot = kernel.tools();
    assert!(!snapshot.contains("registered-a"));
    assert!(!snapshot.contains("registered-b"));
    assert!(!snapshot.contains("never"));

    // The fiber is a tombstone: it records the failure, holds no guards,
    // and does not claim the identity — a corrected version loads under it.
    let fiber = kernel.fiber("half").unwrap();
    assert_eq!(
        fiber.state(),
        &FiberState::Failed("plugin error: scripted failure".to_owned())
    );
    assert!(fiber.registrations().is_empty());

    kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "half",
                vec![FixedTool::new("recovered", "ok")],
            )),
            json!({}),
        )
        .unwrap();
    assert_eq!(dispatch(&kernel, "recovered").await, "ok");
    assert_eq!(kernel.fiber("half").unwrap().state(), &FiberState::Active);
}

#[tokio::test]
async fn panicking_apply_is_contained_and_unwound() {
    let mut kernel = PluginKernel::new(ToolRegistry::new());
    let mut plugin = ScriptedPlugin::new("bomber", vec![FixedTool::new("before-panic", "x")]);
    plugin.panic_at = Some(1);
    plugin
        .tools
        .push(("after-panic".to_owned(), "y".to_owned()));

    let error = kernel.load(Box::new(plugin), json!({})).unwrap_err();
    assert!(error.to_string().contains("panicked"), "got: {error}");
    assert!(error.to_string().contains("scripted panic"), "got: {error}");
    assert!(!kernel.tools().contains("before-panic"));
    assert!(matches!(
        kernel.fiber("bomber").unwrap().state(),
        FiberState::Failed(_)
    ));
}

#[test]
fn lifo_unload_across_plugins() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut kernel = PluginKernel::new(ToolRegistry::new());
    for id in ["first", "second", "third"] {
        kernel
            .load(
                Box::new(ScriptedPlugin::logging(id, Arc::clone(&log))),
                json!({}),
            )
            .unwrap();
    }

    kernel.unload_all();

    // The tools' own drops are the observable removal order: reverse load.
    assert_eq!(
        *log.lock().unwrap(),
        vec!["third".to_owned(), "second".to_owned(), "first".to_owned()]
    );
    assert!(kernel.fibers().is_empty());
}

#[test]
fn duplicate_identity_is_refused_without_disturbing_the_loaded_plugin() {
    let mut kernel = PluginKernel::new(ToolRegistry::new());
    kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "unique",
                vec![FixedTool::new("kept", "original")],
            )),
            json!({}),
        )
        .unwrap();

    let error = kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "unique",
                vec![FixedTool::new("intruder", "replacement")],
            )),
            json!({}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("already loaded"), "got: {error}");

    // The refusal touched nothing: the original registration stands, the
    // intruder's was never made.
    let snapshot = kernel.tools();
    assert!(snapshot.contains("kept"));
    assert!(!snapshot.contains("intruder"));
    assert_eq!(kernel.fibers().len(), 1);
}

#[tokio::test]
async fn hot_reload_proves_absence_then_presence() {
    /// v2 refuses to apply while its predecessor's registration is still
    /// visible — the absence half of the proof is asserted by the new
    /// plugin itself, inside its own `apply`.
    struct GreeterV2;

    impl Plugin for GreeterV2 {
        fn id(&self) -> &str {
            "greeter"
        }

        fn apply(&self, ctx: &mut PluginContext) -> Result<()> {
            assert!(
                !ctx.has_tool("greet"),
                "the old registration must be gone before the new apply runs"
            );
            ctx.register_tool(FixedTool::new("greet", "hello v2"))
        }
    }

    let mut kernel = PluginKernel::new(ToolRegistry::new());
    kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "greeter",
                vec![FixedTool::new("greet", "hello v1")],
            )),
            json!({"version": 1}),
        )
        .unwrap();
    assert_eq!(dispatch(&kernel, "greet").await, "hello v1");

    kernel
        .reload(Box::new(GreeterV2), json!({"version": 2}))
        .unwrap();
    assert_eq!(dispatch(&kernel, "greet").await, "hello v2");
    assert_eq!(
        kernel.fiber("greeter").unwrap().config(),
        &json!({"version": 2})
    );
}

#[test]
fn reload_without_an_active_plugin_is_an_error() {
    struct Nothing;
    impl Plugin for Nothing {
        fn id(&self) -> &str {
            "absent"
        }
        fn apply(&self, _ctx: &mut PluginContext) -> Result<()> {
            Ok(())
        }
    }
    let mut kernel = PluginKernel::new(ToolRegistry::new());
    let error = kernel.reload(Box::new(Nothing), json!({})).unwrap_err();
    assert!(
        error.to_string().contains("no active plugin"),
        "got: {error}"
    );
}

#[test]
fn plugins_may_not_shadow_base_tools_or_each_other() {
    let mut base = ToolRegistry::new();
    base.register(FixedTool::new("core-tool", "base"));
    let mut kernel = PluginKernel::new(base);

    let error = kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "shadow-base",
                vec![FixedTool::new("core-tool", "shadowed")],
            )),
            json!({}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("may not shadow"), "got: {error}");

    kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "owner",
                vec![FixedTool::new("taken", "first")],
            )),
            json!({}),
        )
        .unwrap();
    let error = kernel
        .load(
            Box::new(ScriptedPlugin::new(
                "shadow-plugin",
                vec![
                    FixedTool::new("own-tool", "mine"),
                    FixedTool::new("taken", "second"),
                ],
            )),
            json!({}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("may not shadow"), "got: {error}");

    // The shadowing plugin's earlier registration unwound with the failure.
    let snapshot = kernel.tools();
    assert!(!snapshot.contains("own-tool"));
    let executor = ToolExecutor::new(snapshot);
    assert_eq!(
        executor.registry().get("core-tool").unwrap().name(),
        "core-tool"
    );
}

#[test]
fn config_flows_to_apply_and_its_rejection_unwinds() {
    struct NeedsKey;
    impl Plugin for NeedsKey {
        fn id(&self) -> &str {
            "needs-key"
        }
        fn apply(&self, ctx: &mut PluginContext) -> Result<()> {
            ctx.register_tool(FixedTool::new("configured", "yes"))?;
            match ctx.config().get("api_key").and_then(Value::as_str) {
                Some(key) if !key.is_empty() => Ok(()),
                _ => Err(rusty_agent_runtime::error::RustyError::Plugin(
                    "config.api_key is required".to_owned(),
                )),
            }
        }
    }

    let mut kernel = PluginKernel::new(ToolRegistry::new());
    let error = kernel.load(Box::new(NeedsKey), json!({})).unwrap_err();
    assert!(error.to_string().contains("api_key"), "got: {error}");
    assert!(!kernel.tools().contains("configured"));

    kernel
        .load(Box::new(NeedsKey), json!({"api_key": "k"}))
        .unwrap();
    assert!(kernel.tools().contains("configured"));
}

#[test]
fn kernel_drop_unloads_everything_lifo() {
    let log = Arc::new(Mutex::new(Vec::new()));
    {
        let mut kernel = PluginKernel::new(ToolRegistry::new());
        for id in ["one", "two"] {
            kernel
                .load(
                    Box::new(ScriptedPlugin::logging(id, Arc::clone(&log))),
                    json!({}),
                )
                .unwrap();
        }
        // No explicit unload_all: dropping the kernel unwinds it.
    }
    assert_eq!(
        *log.lock().unwrap(),
        vec!["two".to_owned(), "one".to_owned()]
    );
}

// ---------- the capsule bridge (feature `wasm`) ----------

#[cfg(feature = "wasm")]
mod capsule_bridge {
    //! A real capsule — hand-written component text, the `tests/capsule.rs`
    //! convention — loaded as a plugin: its declared interface becomes the
    //! guarded tool registration, and unloading removes its reach from the
    //! dispatch surface.

    use super::*;
    use std::collections::BTreeSet;

    use rusty_agent_runtime::capsule::{
        CapabilityGrant, CapsuleIdentity, CapsuleInterface, CapsuleManifest, ResourceBudget,
        WORLD_V1,
    };
    use rusty_agent_runtime::capsule_host::CapsuleHost;
    use rusty_agent_runtime::plugin::CapsulePlugin;
    use rusty_agent_runtime::record::{sha256_hex, Effect};

    fn wat_escape(raw: &str) -> String {
        raw.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// The canonical-ABI bump `realloc` and `result<string, string>`
    /// writer, the `tests/capsule.rs` reference-guest discipline.
    const REALLOC: &str = r#"
    (global $heap (mut i32) (i32.const 1024))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (global.set $heap
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (global.get $heap) (local.get $new_size)))
      (local.get $ptr))"#;

    const WRITE_RESULT: &str = r#"
      (i32.store8 (i32.const 512) (local.get $disc))
      (i32.store (i32.const 516) (local.get $ptr))
      (i32.store (i32.const 520) (local.get $len))
      (i32.const 512)"#;

    /// A pure-compute component answering a static JSON payload.
    fn pure_guest_wat(output_json: &str) -> String {
        let escaped = wat_escape(output_json);
        let len = output_json.len();
        format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    {REALLOC}
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (local $disc i32) (local $ptr i32) (local $len i32)
      (local.set $disc (i32.const 0))
      (local.set $ptr (i32.const 16))
      (local.set $len (i32.const {len}))
      {WRITE_RESULT})
    (data (i32.const 16) "{escaped}"))
  (core instance $i (instantiate $m))
  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $i "run")
      (memory (core memory $i "memory"))
      (realloc (core func $i "realloc"))))
  (export "run" (func $run)))"#
        )
    }

    fn manifest_for(wat: &str, name: &str) -> CapsuleManifest {
        CapsuleManifest {
            identity: CapsuleIdentity {
                name: name.to_owned(),
                description: None,
            },
            version: "0.1.0".into(),
            build_digest: sha256_hex(wat.as_bytes()),
            interface: CapsuleInterface {
                world: WORLD_V1.into(),
                input_schema: None,
                output_schema: None,
            },
            effects: BTreeSet::from([Effect::Pure]),
            capabilities: BTreeSet::<CapabilityGrant>::new(),
            budget: ResourceBudget::default(),
        }
    }

    #[tokio::test]
    async fn capsule_plugin_loads_dispatches_and_unloads() {
        let wat = pure_guest_wat(r#"{"answer":"from-the-capsule"}"#);
        let host =
            CapsuleHost::from_bytes(manifest_for(&wat, "capsule-tool"), wat.as_bytes()).unwrap();
        let plugin = CapsulePlugin::new(host).unwrap();
        assert_eq!(plugin.id(), "capsule-tool");

        let mut kernel = PluginKernel::new(ToolRegistry::new());
        kernel.load(Box::new(plugin), json!({})).unwrap();

        // The capsule's declared interface is its tool surface: default
        // object schema, declared effect ceiling, guest-produced output.
        let snapshot = kernel.tools();
        let tool = snapshot.get("capsule-tool").unwrap();
        assert_eq!(tool.parameters_schema(), json!({"type": "object"}));
        assert_eq!(tool.effect(), Effect::Pure);
        let executor = ToolExecutor::new(snapshot);
        let results = executor
            .execute_batch(&[ToolCall::new("c1", "capsule-tool", json!({}))])
            .await;
        assert_eq!(
            results[0].content.as_deref(),
            Some(r#"{"answer":"from-the-capsule"}"#)
        );

        // Unloading removes the capsule's reach like any other plugin's.
        kernel.unload("capsule-tool").unwrap();
        let gone = super::dispatch(&kernel, "capsule-tool").await;
        assert!(
            gone.starts_with("ERROR:") && gone.contains("unknown tool"),
            "got: {gone}"
        );
    }
}
