//! Sandboxed execution of untrusted/community nodes via WebAssembly.
//!
//! [`WasmNode`] wraps a compiled WASM module and runs it as an ordinary
//! [`Node`]: the graph engine cannot tell the difference between a native
//! closure node and a sandboxed guest. This enables loading third-party or
//! user-authored node logic without trusting it with host memory, threads,
//! the filesystem, or the network — the guest only ever sees a JSON input
//! and returns a JSON output.
//!
//! # Guest ABI (v0)
//!
//! A guest module targeting `wasm_node` ABI **v0** must export:
//!
//! | Export  | Signature                        | Meaning                                    |
//! |---------|----------------------------------|--------------------------------------------|
//! | `memory`| `(memory)`                       | Linear memory the host reads/writes.       |
//! | `alloc` | `(func (param i32) (result i32))`| `alloc(len) -> ptr`: reserve `len` bytes in guest memory; host writes the input JSON there. |
//! | `run`   | `(func (param i32 i32) (result i64))` | `run(input_ptr, input_len) -> packed output`: high 32 bits = output ptr, low 32 bits = output len. |
//!
//! - **Input JSON** written at `alloc`'d ptr:
//!   `{"state": <state snapshot object>, "config": <NodeConfig>}`
//! - **Output JSON** read back from the packed ptr/len, one of:
//!   - `{"updates": {...}, "command": {...}?}` — mapped to
//!     [`NodeOutput::updates`] / [`NodeOutput::command`]
//!   - `{"error": "..."}` — mapped to [`RustyError::Node`]
//!   - `{"interrupt": <value>}` — mapped to
//!     [`RustyError::Interrupt`], so a sandboxed guest can still
//!     participate in human-in-the-loop suspend/resume.
//! - The module must require **no imports** (pure compute; no WASI).
//!
//! # Sandboxing
//!
//! - **Fuel metering** (`consume_fuel`): every guest instruction consumes
//!   fuel; a per-call budget ([`SandboxLimits::fuel`]) aborts infinite
//!   loops with a trap instead of hanging the executor.
//! - **Memory growth cap** ([`SandboxLimits::max_memory_bytes`]): a
//!   `ResourceLimiter` rejects guest `memory.grow` beyond the cap.
//! - **No host capabilities**: the module is instantiated with an empty
//!   `Linker` — no WASI, no host functions, no ambient authority.
//!
//! Modules are compiled once per [`WasmNode`] (cranelift) and cached; each
//! `run()` creates a fresh `Store`, so guests keep no state across
//! super-steps (matching the engine's idempotency contract).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use wasmtime::{Config, Engine, Linker, Module, ResourceLimiter, Store};

use crate::error::{Result, RustyError};
use crate::node::{Command, Node, NodeContext, NodeOutput};

/// Per-call sandbox limits for a guest invocation.
#[derive(Debug, Clone)]
pub struct SandboxLimits {
    /// Fuel budget per `run()` call. Roughly one fuel per guest
    /// instruction; `10_000_000` comfortably covers JSON processing for
    /// small inputs while killing infinite loops quickly.
    pub fuel: u64,

    /// Maximum total linear-memory bytes a guest may grow to.
    pub max_memory_bytes: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            max_memory_bytes: 16 * 1024 * 1024, // 16 MiB
        }
    }
}

/// Store-local state implementing the memory-growth cap.
struct StoreLimits {
    max_memory_bytes: usize,
}

impl ResourceLimiter for StoreLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> std::result::Result<bool, wasmtime::Error> {
        Ok(desired <= self.max_memory_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> std::result::Result<bool, wasmtime::Error> {
        // Guests get tables for function pointers; cap generously but finite.
        Ok(desired <= 100_000)
    }
}

/// Output JSON schema of ABI v0 (see module docs).
#[derive(Debug, Deserialize)]
struct GuestOutput {
    #[serde(default)]
    updates: HashMap<String, Value>,
    #[serde(default)]
    command: Option<Command>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    interrupt: Option<Value>,
}

/// A [`Node`] that executes a sandboxed WebAssembly guest module.
///
/// Cheap to clone (the `Engine`/`Module` are `Arc`-backed); the compiled
/// module is shared and instantiation is per call.
#[derive(Clone, Debug)]
pub struct WasmNode {
    name: String,
    engine: Engine,
    module: Module,
    limits: SandboxLimits,
}

impl WasmNode {
    /// Compile a module from a `.wasm` (or `.wat`, via the `wat` feature)
    /// file, using a fresh default sandboxing engine.
    pub fn from_file(name: impl Into<String>, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| {
            RustyError::Node(format!("wasm node: failed to read {}: {e}", path.display()))
        })?;
        Self::from_bytes(name, bytes)
    }

    /// Compile a module from bytes (binary WASM or WAT text), using a fresh
    /// default sandboxing engine.
    pub fn from_bytes(name: impl Into<String>, bytes: impl AsRef<[u8]>) -> Result<Self> {
        Self::with_engine(name, bytes, default_engine()?)
    }

    /// Compile a module against a caller-provided (shared) engine.
    ///
    /// The engine **must** have `consume_fuel(true)` configured (see
    /// [`SandboxLimits`]); use [`default_engine`] unless you need to share
    /// one compilation cache across many nodes.
    pub fn with_engine(
        name: impl Into<String>,
        bytes: impl AsRef<[u8]>,
        engine: Engine,
    ) -> Result<Self> {
        let name = name.into();
        let module = Module::new(&engine, bytes.as_ref()).map_err(|e| {
            RustyError::Node(format!("wasm node '{name}': failed to compile module: {e}"))
        })?;
        let node = Self {
            name,
            engine,
            module,
            limits: SandboxLimits::default(),
        };
        node.validate_abi()?;
        Ok(node)
    }

    /// Override the default sandbox limits (fuel, memory cap).
    pub fn with_limits(mut self, limits: SandboxLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Fail fast at construction time if the module does not satisfy the
    /// ABI v0 export contract.
    fn validate_abi(&self) -> Result<()> {
        let bad = |msg: &str| {
            RustyError::Node(format!(
                "wasm node '{}': ABI v0 violation: {msg}",
                self.name
            ))
        };
        let mut has_memory = false;
        let mut has_alloc = false;
        let mut has_run = false;
        for export in self.module.exports() {
            match (export.name(), export.ty()) {
                ("memory", wasmtime::ExternType::Memory(_)) => has_memory = true,
                ("alloc", wasmtime::ExternType::Func(_)) => has_alloc = true,
                ("run", wasmtime::ExternType::Func(_)) => has_run = true,
                _ => {}
            }
        }
        if !has_memory {
            return Err(bad("missing exported memory \"memory\""));
        }
        if !has_alloc {
            return Err(bad("missing exported function \"alloc\" (fn(i32) -> i32)"));
        }
        if !has_run {
            return Err(bad(
                "missing exported function \"run\" (fn(i32, i32) -> i64)",
            ));
        }
        Ok(())
    }

    /// Synchronous guest invocation; runs on a blocking thread.
    fn run_guest(
        name: &str,
        engine: &Engine,
        module: &Module,
        limits: &SandboxLimits,
        input: &[u8],
    ) -> Result<NodeOutput> {
        let node_err = |msg: String| RustyError::Node(format!("wasm node '{name}': {msg}"));

        let mut store = Store::new(
            engine,
            StoreLimits {
                max_memory_bytes: limits.max_memory_bytes,
            },
        );
        store.limiter(|s| s);
        store.set_fuel(limits.fuel).map_err(|e| {
            node_err(format!(
                "failed to set fuel (engine needs consume_fuel): {e}"
            ))
        })?;

        let linker = Linker::new(engine);
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|e| node_err(format!("instantiation failed: {e}")))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| node_err("no exported memory".into()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| node_err(format!("bad \"alloc\" export: {e}")))?;
        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "run")
            .map_err(|e| node_err(format!("bad \"run\" export: {e}")))?;

        // Write input into guest memory.
        let input_len: i32 = input
            .len()
            .try_into()
            .map_err(|_| node_err("input JSON exceeds 2 GiB".into()))?;
        let input_ptr = alloc
            .call(&mut store, input_len)
            .map_err(|e| node_err(format!("guest alloc({input_len}) trapped: {e}")))?;
        memory
            .write(&mut store, input_ptr as usize, input)
            .map_err(|e| node_err(format!("host write into guest memory failed: {e}")))?;

        // Run the guest; fuel exhaustion and OOB surface here as traps.
        let packed = run
            .call(&mut store, (input_ptr, input_len))
            .map_err(|e| node_err(format!("guest run trapped: {e}")))?;
        let out_ptr = ((packed as u64) >> 32) as u32 as usize;
        let out_len = (packed as u64 & 0xFFFF_FFFF) as u32 as usize;

        // The packed ptr/len are guest-controlled: validate them against the
        // actual memory size *before* allocating the output buffer on the
        // host. The memory-growth cap limits the guest's memory, not this
        // host-side allocation — without this check a malicious guest gets a
        // cheap multi-GiB host memory amplification.
        let memory_size = memory.data_size(&store);
        let end = out_ptr.checked_add(out_len).ok_or_else(|| {
            node_err("guest returned an out_ptr + out_len that overflows usize".into())
        })?;
        if end > memory_size {
            return Err(node_err(format!(
                "guest returned output range [{out_ptr}, {end}) beyond its \
                 {memory_size}-byte memory"
            )));
        }

        let mut buf = vec![0u8; out_len];
        memory
            .read(&store, out_ptr, &mut buf)
            .map_err(|e| node_err(format!("host read from guest memory failed: {e}")))?;

        let guest: GuestOutput = serde_json::from_slice(&buf)
            .map_err(|e| node_err(format!("invalid output JSON: {e}")))?;

        // Priority: error > interrupt > updates/command.
        if let Some(msg) = guest.error {
            return Err(node_err(msg));
        }
        if let Some(value) = guest.interrupt {
            return Err(RustyError::Interrupt { value });
        }
        Ok(NodeOutput {
            updates: guest.updates,
            command: guest.command,
        })
    }
}

/// Build the default sandboxing engine: cranelift codegen + fuel metering.
pub fn default_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.consume_fuel(true);
    Engine::new(&config)
        .map_err(|e| RustyError::Node(format!("wasm node: failed to create engine: {e}")))
}

#[async_trait]
impl Node for WasmNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn effect(&self) -> crate::record::Effect {
        // The guest is arbitrary code behind a sandbox boundary; the
        // restrictive class applies until capsule manifests (R0.9) declare
        // the guest's real effect surface.
        crate::record::Effect::NonIdempotent
    }

    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput> {
        let input = serde_json::json!({
            "state": ctx.state().to_value(),
            "config": ctx.config(),
        });
        let input = serde_json::to_vec(&input)?;

        // The guest call is synchronous and CPU-bound; keep it off the
        // async executor. `Engine`/`Module` are Arc-backed and Send+Sync.
        let name = self.name.clone();
        let engine = self.engine.clone();
        let module = self.module.clone();
        let limits = self.limits.clone();
        let name_for_task = name.clone();
        tokio::task::spawn_blocking(move || {
            Self::run_guest(&name_for_task, &engine, &module, &limits, &input)
        })
        .await
        .map_err(|e| RustyError::Node(format!("wasm node '{name}': task join failed: {e}")))?
    }
}

/// Convenience: build a shared, fuel-metered engine wrapped in `Arc` for
/// sharing one compilation cache across many [`WasmNode`]s.
pub fn shared_engine() -> Result<Arc<Engine>> {
    Ok(Arc::new(default_engine()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeConfig;
    use crate::state::State;
    use serde_json::json;

    /// Build a WAT module whose `run` ignores its input and returns a static
    /// output string placed at offset 16 via a data segment.
    fn static_output_wat(output_json: &str) -> String {
        let len = output_json.len();
        // Escape for WAT string literal: quotes and backslashes.
        let escaped = output_json.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            r#"(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "alloc") (param $len i32) (result i32)
    (global.get $heap))
  (func (export "run") (param i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const 16)) (i64.const 32))
      (i64.extend_i32_u (i32.const {len}))))
  (data (i32.const 16) "{escaped}"))"#
        )
    }

    fn test_ctx() -> NodeContext {
        let mut state = State::new();
        state.insert("messages", json!(["hi"]));
        NodeContext::new(
            state,
            NodeConfig {
                thread_id: "t-wasm".into(),
                step: 1,
                resume: None,
                extra: HashMap::new(),
            },
        )
    }

    #[tokio::test]
    async fn wasm_node_executes_and_maps_updates() {
        let wat = static_output_wat(r#"{"updates":{"result":42},"command":{"goto":["next"]}}"#);
        let node = WasmNode::from_bytes("guest-ok", wat).unwrap();
        assert_eq!(node.name(), "guest-ok");

        let out = node.run(test_ctx()).await.unwrap();
        assert_eq!(out.updates.get("result"), Some(&json!(42)));
        // `Command` does not derive PartialEq; compare fields directly.
        let cmd = out.command.expect("expected routing command");
        assert_eq!(cmd.goto, vec!["next".to_string()]);
        assert!(cmd.resume.is_none());
    }

    #[tokio::test]
    async fn wasm_node_maps_guest_error_to_node_error() {
        let wat = static_output_wat(r#"{"error":"boom from guest"}"#);
        let node = WasmNode::from_bytes("guest-err", wat).unwrap();

        let err = node.run(test_ctx()).await.unwrap_err();
        match err {
            RustyError::Node(msg) => {
                assert!(msg.contains("guest-err"));
                assert!(msg.contains("boom from guest"));
            }
            other => panic!("expected Node error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wasm_node_maps_interrupt_to_hitl_error() {
        let wat = static_output_wat(r#"{"interrupt":{"question":"approve?"}}"#);
        let node = WasmNode::from_bytes("guest-hitl", wat).unwrap();

        let err = node.run(test_ctx()).await.unwrap_err();
        assert!(err.is_interrupt());
        assert_eq!(
            err.interrupt_value(),
            Some(&json!({"question": "approve?"}))
        );
    }

    #[tokio::test]
    async fn wasm_node_fuel_exhaustion_errors_instead_of_hanging() {
        let wat = r#"(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "run") (param i32 i32) (result i64)
    (loop $forever (br $forever))
    unreachable))"#;
        let node = WasmNode::from_bytes("guest-loop", wat).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(30), node.run(test_ctx()))
            .await
            .expect("guest infinite loop must be killed by fuel, not hang");
        let err = result.unwrap_err();
        match err {
            RustyError::Node(msg) => assert!(msg.contains("trapped")),
            other => panic!("expected Node trap error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn guest_controlled_out_len_is_validated_before_host_allocation() {
        // run returns ptr 0 / len ~4 GiB: must error, never allocate.
        let wat = r#"(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "run") (param i32 i32) (result i64)
    (i64.const 4294967295)))"#;
        let node = WasmNode::from_bytes("guest-dos", wat).unwrap();

        let err = node.run(test_ctx()).await.unwrap_err();
        match err {
            RustyError::Node(msg) => assert!(msg.contains("beyond"), "got: {msg}"),
            other => panic!("expected Node error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_growth_beyond_cap_is_denied() {
        // run probes the limiter: if memory.grow by 10 pages succeeds it
        // traps (unreachable); only a *denied* grow (-1) lets it return the
        // static output. Success therefore proves the cap was enforced.
        let wat = static_output_wat(r#"{"updates":{"ok":true}}"#).replace(
            "(func (export \"run\") (param i32 i32) (result i64)\n",
            "(func (export \"run\") (param i32 i32) (result i64)\n    (if (i32.ne (memory.grow (i32.const 10)) (i32.const -1)) (then unreachable))\n",
        );
        let node = WasmNode::from_bytes("guest-grow", wat)
            .unwrap()
            // Exactly one 64 KiB page: the initial page fits, any grow is over.
            .with_limits(SandboxLimits {
                fuel: 10_000_000,
                max_memory_bytes: 64 * 1024,
            });

        let out = node.run(test_ctx()).await.unwrap();
        assert_eq!(out.updates.get("ok"), Some(&json!(true)));
    }

    #[test]
    fn abi_violation_rejected_at_construction() {
        // Missing "run" export.
        let wat = r#"(module (memory (export "memory") 1))"#;
        let err = WasmNode::from_bytes("bad-abi", wat).unwrap_err();
        match err {
            RustyError::Node(msg) => assert!(msg.contains("ABI v0")),
            other => panic!("expected Node error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shared_engine_across_nodes() {
        let engine = shared_engine().unwrap();
        let a = WasmNode::with_engine(
            "a",
            static_output_wat(r#"{"updates":{"x":1}}"#),
            (*engine).clone(),
        )
        .unwrap();
        let b = WasmNode::with_engine(
            "b",
            static_output_wat(r#"{"updates":{"y":2}}"#),
            (*engine).clone(),
        )
        .unwrap();
        let out_a = a.run(test_ctx()).await.unwrap();
        let out_b = b.run(test_ctx()).await.unwrap();
        assert_eq!(out_a.updates.get("x"), Some(&json!(1)));
        assert_eq!(out_b.updates.get("y"), Some(&json!(2)));
    }
}
