//! The plugin kernel: hot load/unload of capability bundles with
//! revertible registrations.
//!
//! Rusty's planes are statically composed — a graph build wires its tools
//! and skills once, and the composition never changes underneath a run.
//! The plugin kernel is the deliberately small dynamic edge on top of that
//! model: a [`Plugin`] contributes registrations at runtime, and every one
//! of them is *revertible by construction*. The guarantee is ownership,
//! not author discipline: each registration returns a
//! [`RegistrationGuard`] whose `Drop` removes exactly the entry it made,
//! the kernel collects a plugin's guards into its [`Fiber`], and unloading
//! drops the stack in reverse (LIFO) order. Hot load/unload is a system
//! invariant; a plugin cannot leak a half-registration, because the error
//! path of [`Plugin::apply`] unwinds what it already registered before the
//! failure is reported (the kernel's core honesty property, covered by
//! `rusty-core/tests/plugin.rs`).
//!
//! # Which registries participate — and which deliberately do not
//!
//! The tool plane participates: [`PluginContext::register_tool`] inserts
//! into the kernel's shared [`ToolRegistry`] and collects the guard. The
//! skill and connector planes do not, on purpose:
//!
//! - **Skills** are an append-only, content-addressed audit trail. A
//!   version that a run disclosed is replay evidence — its content hash is
//!   what exact replay pins — so unpublishing on unload would falsify the
//!   record of what the model saw. Publish stays the composer's governed,
//!   approval-gated path; a reversible skill overlay is its own wave.
//! - **Connector manifests** are idempotent content-addressed entries with
//!   live tenant instances hanging off them; removal could strand sessions
//!   the registry explicitly owns. Not a registration a guard can honestly
//!   undo.
//!
//! # Ordering and dependencies
//!
//! Unload order across plugins is reverse load order — the dependency
//! intuition that a later plugin may build on an earlier one's
//! registrations. Beyond that, cross-plugin dependencies are the author's
//! declaration at load time, not something the kernel infers: a reactive,
//! `inject`-style reactivation model (the DeepSeek harness's Cordis
//! answer) is deliberately deferred until the kernel proves a need for it.
//!
//! # The capsule bridge
//!
//! Behind the `wasm` feature, `CapsulePlugin` adapts a WASM capsule into
//! a plugin: the capsule's declared interface (name, input schema, effect
//! class) becomes one guarded tool registration, and the capability host's
//! enforcement is untouched. That is the runtime-code vehicle — sandboxed,
//! capability-granted guests loading through the same revertible path as
//! native plugins.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::tool::{Tool, ToolRegistry};

/// The longest a plugin identity may run, in bytes. Identities key the
/// kernel's fiber table and travel in errors and listings; bounded like
/// every other name in the crate.
pub const MAX_PLUGIN_ID_LEN: usize = 128;

/// The largest a plugin's declared config may be, serialized. Config is
/// loaded into the fiber and handed to `apply`; the ceiling keeps one
/// plugin from turning the fiber table into a blob store.
pub const MAX_PLUGIN_CONFIG_BYTES: usize = 64 * 1024;

/// Validate a plugin identity: non-empty, trimmed, control-free, bounded —
/// the same discipline as tool names, so an identity is safe to echo into
/// logs, errors, and listings verbatim.
fn validate_plugin_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > MAX_PLUGIN_ID_LEN
        || id != id.trim()
        || id.chars().any(char::is_control)
    {
        return Err(RustyError::Plugin(format!(
            "plugin id must be non-empty, trimmed, control-free, and at most {MAX_PLUGIN_ID_LEN} bytes; got `{id}`"
        )));
    }
    Ok(())
}

/// Lock a registry, tolerating poison: a panic while a lock was held means
/// a plugin misbehaved mid-registration, not that the map is corrupt —
/// and unwinding must keep working precisely in that situation.
fn lock(registry: &Mutex<ToolRegistry>) -> MutexGuard<'_, ToolRegistry> {
    registry.lock().unwrap_or_else(|e| e.into_inner())
}

/// A live registration's undo handle.
///
/// Dropping the guard removes exactly the registration it made: the undo
/// matches on the registered entry's identity (not just its name), so an
/// entry re-registered by someone else since is never removed by mistake.
/// The undo runs at most once — a guard that has already fired is inert —
/// and a guard whose kernel is gone (the registry outlived it nowhere) is
/// a no-op. Guards are created by [`PluginContext`] and collected straight
/// into the plugin's fiber; the type is public so callers can inspect what
/// a fiber holds ([`Fiber::registrations`]).
#[must_use = "a registration reverts when its guard drops"]
pub struct RegistrationGuard {
    /// The registry surface: `"tool"`. A string, not an enum, so future
    /// surfaces extend the vocabulary without a breaking change.
    kind: &'static str,
    /// What was registered (the tool name).
    name: String,
    /// The undo, taken on first fire. `None` means inert.
    undo: Option<Box<dyn FnOnce() + Send>>,
}

impl RegistrationGuard {
    /// A guard removing `tool` from `tools` on drop, only while the entry
    /// under its name is still the exact registration this guard made.
    fn tool(tools: &Arc<Mutex<ToolRegistry>>, name: String, tool: Arc<dyn Tool>) -> Self {
        let tools = Arc::downgrade(tools);
        Self {
            kind: "tool",
            name: name.clone(),
            undo: Some(Box::new(move || {
                let Some(registry) = tools.upgrade() else {
                    return;
                };
                let mut registry = lock(&registry);
                if registry
                    .get(&name)
                    .is_some_and(|current| Arc::ptr_eq(&current, &tool))
                {
                    registry.unregister(&name);
                }
            })),
        }
    }

    /// The registry surface this registration lives on (`"tool"`).
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The name this registration was made under.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for RegistrationGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationGuard")
            .field(
                "registration",
                &format_args!("{} `{}`", self.kind, self.name),
            )
            .field("armed", &self.undo.is_some())
            .finish()
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if let Some(undo) = self.undo.take() {
            undo();
        }
    }
}

/// A plugin: a named bundle that applies registrations into a context.
///
/// Object-safe by design — the kernel holds `Box<dyn Plugin>`. `Box`, not
/// `Arc`: the kernel is the plugin's sole owner for its whole lifecycle,
/// and unloading drops the plugin value after its registrations are gone,
/// so shared ownership would only obscure who keeps a plugin alive.
///
/// `apply` is synchronous. Registration is in-memory map insertion — the
/// capsule bridge compiles its guest at construction, not in `apply` — so
/// an async boundary here would buy nothing and cost every caller a
/// runtime.
pub trait Plugin: Send + Sync {
    /// The plugin's stable identity. The kernel refuses a second live
    /// plugin under one identity; hot reload is the same identity unloaded
    /// and re-applied.
    fn id(&self) -> &str;

    /// Contribute this plugin's registrations. On `Err` the kernel unwinds
    /// everything already registered through `ctx` — an `apply` either
    /// completes or leaves no trace.
    fn apply(&self, ctx: &mut PluginContext) -> Result<()>;
}

/// The registration surface handed to [`Plugin::apply`].
///
/// Carries the plugin's declared config ([`PluginContext::config`]) —
/// validation is the plugin's own job inside `apply`, and a refusal there
/// unwinds cleanly — and collects the guards of every registration it
/// hands out. The kernel owns the context; plugin code borrows it for the
/// duration of `apply` and cannot smuggle it out.
pub struct PluginContext {
    tools: Arc<Mutex<ToolRegistry>>,
    config: Value,
    guards: Vec<RegistrationGuard>,
}

impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("registrations", &self.guards)
            .finish_non_exhaustive()
    }
}

impl PluginContext {
    fn new(tools: Arc<Mutex<ToolRegistry>>, config: Value) -> Self {
        Self {
            tools,
            config,
            guards: Vec::new(),
        }
    }

    /// The config declared at load time, verbatim.
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// `true` if a tool with this name is currently registered — the
    /// kernel's base tools plus every live plugin's.
    pub fn has_tool(&self, name: &str) -> bool {
        lock(&self.tools).contains(name)
    }

    /// Register a tool, guarded. Refuses to shadow: a name already taken —
    /// by the base composition or another plugin — is an error, because
    /// the guard's undo must never remove an entry it did not make.
    pub fn register_tool<T: Tool + 'static>(&mut self, tool: T) -> Result<()> {
        self.register_shared_tool(Arc::new(tool))
    }

    /// The shared-ownership half of [`PluginContext::register_tool`].
    pub fn register_shared_tool(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.name().to_owned();
        {
            let mut registry = lock(&self.tools);
            if registry.contains(&name) {
                return Err(RustyError::Plugin(format!(
                    "tool `{name}` is already registered; plugins may not shadow existing tools"
                )));
            }
            registry.register_shared(Arc::clone(&tool));
        }
        self.guards
            .push(RegistrationGuard::tool(&self.tools, name, tool));
        Ok(())
    }

    /// How many registrations this context has collected so far.
    pub fn registration_count(&self) -> usize {
        self.guards.len()
    }
}

/// Where a fiber is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiberState {
    /// Applied and serving; its registrations are live.
    Active,
    /// Being unwound. Observable only if a guard's undo re-enters the
    /// kernel — transient by construction.
    Unloading,
    /// `apply` failed or panicked. The fiber is a tombstone: it holds no
    /// guards (its partial registrations were unwound before the state was
    /// recorded) and it does not claim the identity — a corrected plugin
    /// may load under it.
    Failed(String),
}

/// One loaded plugin's tracked state: identity, declared config,
/// lifecycle, and the guard stack unloading will unwind.
pub struct Fiber {
    id: String,
    config: Value,
    state: FiberState,
    /// Registration order; unwound from the back (LIFO).
    guards: Vec<RegistrationGuard>,
    /// Kept alive for the fiber's whole active life and dropped on unload,
    /// after its registrations are gone.
    #[allow(dead_code)]
    plugin: Box<dyn Plugin>,
}

impl std::fmt::Debug for Fiber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fiber")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("registrations", &self.guards)
            .finish_non_exhaustive()
    }
}

impl Fiber {
    /// The plugin's identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The config the plugin was loaded with.
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// The lifecycle state.
    pub fn state(&self) -> &FiberState {
        &self.state
    }

    /// What this fiber has registered, in registration order.
    pub fn registrations(&self) -> &[RegistrationGuard] {
        &self.guards
    }

    /// The tool names this fiber registered — the reload path's absence
    /// checklist.
    fn tool_names(&self) -> Vec<&str> {
        self.guards
            .iter()
            .filter(|guard| guard.kind() == "tool")
            .map(RegistrationGuard::name)
            .collect()
    }
}

/// Unwind a guard stack in reverse registration order. Pop-then-drop, one
/// at a time: each removal completes before the next begins, so a guard
/// that re-enters the registry observes a coherent world.
fn unwind(mut guards: Vec<RegistrationGuard>) {
    while let Some(guard) = guards.pop() {
        drop(guard);
    }
}

/// The plugin kernel: owns the shared tool registry and every fiber, and
/// makes load, unload, and hot reload revertible by construction.
pub struct PluginKernel {
    /// The dispatch surface: the base composition plus every active
    /// plugin's tools. Shared with the guards (they hold a `Weak` back to
    /// it) and snapshotted for executors via [`PluginKernel::tools`].
    tools: Arc<Mutex<ToolRegistry>>,
    /// Load order. Fibers unload from the back — LIFO across plugins.
    fibers: Vec<Fiber>,
}

impl std::fmt::Debug for PluginKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginKernel")
            .field("fibers", &self.fibers)
            .finish_non_exhaustive()
    }
}

impl Default for PluginKernel {
    fn default() -> Self {
        Self::new(ToolRegistry::new())
    }
}

impl PluginKernel {
    /// A kernel over `base`: the static composition the plugins extend.
    /// Base tools are not unloadable — they were never registered through
    /// a guard — and plugins may not shadow them.
    pub fn new(base: ToolRegistry) -> Self {
        Self {
            tools: Arc::new(Mutex::new(base)),
            fibers: Vec::new(),
        }
    }

    /// A snapshot of the current dispatch surface. `ToolRegistry` clones
    /// cheaply (the tools are `Arc`-backed); hand the snapshot to a
    /// [`crate::tool::ToolExecutor`]. A snapshot taken before an unload
    /// keeps its view — build executors from fresh snapshots when the
    /// plugin set may have changed.
    pub fn tools(&self) -> ToolRegistry {
        lock(&self.tools).clone()
    }

    /// Every fiber, in load order (tombstones included).
    pub fn fibers(&self) -> &[Fiber] {
        &self.fibers
    }

    /// One fiber by identity.
    pub fn fiber(&self, id: &str) -> Option<&Fiber> {
        self.fibers.iter().find(|fiber| fiber.id == id)
    }

    /// Load a plugin: validate, apply, collect its guards, activate.
    ///
    /// A live (active or unloading) fiber under the same identity refuses
    /// the load — one identity, one plugin. A failed tombstone does not
    /// refuse: it holds nothing, so the corrected version loads under the
    /// identity its predecessor failed into.
    ///
    /// A plugin that fails — or panics — midway through `apply` leaves no
    /// registrations behind: the partial guard stack is unwound LIFO
    /// before the error is returned and the tombstone recorded. Panics are
    /// contained into the same error channel, the `ToolExecutor`
    /// discipline: a misbehaving plugin cannot take the kernel down with
    /// it.
    pub fn load(&mut self, plugin: Box<dyn Plugin>, config: Value) -> Result<()> {
        let id = plugin.id().to_owned();
        validate_plugin_id(&id)?;
        let config_bytes = serde_json::to_vec(&config)?;
        if config_bytes.len() > MAX_PLUGIN_CONFIG_BYTES {
            return Err(RustyError::Plugin(format!(
                "plugin `{id}` config is {} bytes, above the {MAX_PLUGIN_CONFIG_BYTES}-byte ceiling",
                config_bytes.len()
            )));
        }
        self.fibers
            .retain(|fiber| fiber.id != id || !matches!(fiber.state, FiberState::Failed(_)));
        if self.fibers.iter().any(|fiber| fiber.id == id) {
            return Err(RustyError::Plugin(format!(
                "plugin `{id}` is already loaded; unload it first, or use `reload`"
            )));
        }

        let mut ctx = PluginContext::new(Arc::clone(&self.tools), config.clone());
        let applied = catch_unwind(AssertUnwindSafe(|| plugin.apply(&mut ctx)));
        let result = match applied {
            Ok(result) => result,
            Err(payload) => Err(RustyError::Plugin(format!(
                "plugin `{id}` panicked in apply: {}",
                panic_message(&*payload)
            ))),
        };

        match result {
            Ok(()) => {
                self.fibers.push(Fiber {
                    id,
                    config,
                    state: FiberState::Active,
                    guards: ctx.guards,
                    plugin,
                });
                Ok(())
            }
            Err(error) => {
                // The honesty property: a failed apply leaves no trace.
                // Unwind explicitly — LIFO — rather than relying on the
                // context's own drop order.
                unwind(ctx.guards);
                self.fibers.push(Fiber {
                    id,
                    config,
                    state: FiberState::Failed(error.to_string()),
                    guards: Vec::new(),
                    plugin,
                });
                Err(error)
            }
        }
    }

    /// Unload one active plugin: drop its guard stack LIFO, then the
    /// plugin itself. After return, its registrations are gone from the
    /// registry — not eventually, not on a GC: `Drop` ran.
    pub fn unload(&mut self, id: &str) -> Result<()> {
        let index = self
            .fibers
            .iter()
            .position(|fiber| fiber.id == id && fiber.state == FiberState::Active)
            .ok_or_else(|| RustyError::Plugin(format!("no active plugin under id `{id}`")))?;
        let mut fiber = self.fibers.remove(index);
        fiber.state = FiberState::Unloading;
        unwind(fiber.guards);
        // The plugin value drops here, after its registrations are gone.
        Ok(())
    }

    /// Hot reload: replace the plugin under `plugin.id()` with a new
    /// version. The old registrations are proven gone — checked against
    /// the registry itself, not assumed — before the new `apply` runs; a
    /// survivor aborts the reload before the new plugin is touched.
    pub fn reload(&mut self, plugin: Box<dyn Plugin>, config: Value) -> Result<()> {
        let id = plugin.id().to_owned();
        let prior: Vec<String> = match self.fiber(&id) {
            Some(fiber) if fiber.state == FiberState::Active => {
                fiber.tool_names().into_iter().map(str::to_owned).collect()
            }
            _ => {
                return Err(RustyError::Plugin(format!(
                    "no active plugin under id `{id}` to reload; use `load`"
                )))
            }
        };
        self.unload(&id)?;
        {
            let registry = lock(&self.tools);
            for name in &prior {
                if registry.contains(name) {
                    return Err(RustyError::Plugin(format!(
                        "reload of `{id}` aborted: registration `{name}` survived unloading"
                    )));
                }
            }
        }
        self.load(plugin, config)
    }

    /// Unload every active plugin in reverse load order. Tombstones drop
    /// with their fibers. Called by `Drop`; explicit is better when the
    /// caller wants the unload points ordered against its own teardown.
    pub fn unload_all(&mut self) {
        while let Some(mut fiber) = self.fibers.pop() {
            fiber.state = FiberState::Unloading;
            unwind(fiber.guards);
        }
    }
}

impl Drop for PluginKernel {
    fn drop(&mut self) {
        self.unload_all();
    }
}

/// Best-effort extraction of a panic payload for error reporting (the
/// `tool.rs` helper's twin — panic payloads cross no crate boundary).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string payload>".to_owned()
    }
}

// ---------- the capsule bridge (feature `wasm`) ----------

/// A capsule loaded as a plugin: the declared interface becomes one
/// guarded tool registration.
///
/// The bridge is thin on purpose. Admission (manifest validation, build
/// digest, compilation) already happened at
/// [`crate::capsule_host::CapsuleHost::from_bytes`]; capability
/// enforcement (structural import denial, scope matching, budgets) stays
/// entirely inside the host. What the bridge adds is exactly the kernel's
/// half: the capsule's *offered* surface — its name, its declared input
/// schema, its declared effect ceiling — joins the tool registry under a
/// guard, so unloading the plugin removes the capsule's reach from the
/// dispatch surface with the same revertibility proof as any native
/// plugin.
///
/// The honest edge: `Tool::call` carries no run journal, so in-capsule
/// capability uses and the `WasmCall` invocation event are not journaled
/// through this path — the invocation runs journal-less. Runs that need
/// the capsule evidence trail invoke the host directly with a journaled
/// `CapsuleInvocation`; the bridge is for capsules consumed as plain
/// tools, where the tool plane's own effect admission is the boundary.
#[cfg(feature = "wasm")]
pub struct CapsulePlugin {
    id: String,
    tool: CapsuleTool,
}

#[cfg(feature = "wasm")]
impl CapsulePlugin {
    /// Adapt an admitted capsule host. The declared interface is checked
    /// against the tool contract here — a capsule whose name, description,
    /// or input schema cannot be advertised as a tool fails at
    /// construction, not at first dispatch.
    pub fn new(host: crate::capsule_host::CapsuleHost) -> Result<Self> {
        let manifest = host.manifest();
        let name = manifest.identity.name.clone();
        let description = manifest
            .identity
            .description
            .clone()
            .unwrap_or_else(|| format!("WASM capsule `{name}`"));
        let schema = manifest
            .interface
            .input_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        crate::tool::validate_tool_contract(&name, &description, &schema).map_err(|e| {
            RustyError::Plugin(format!(
                "capsule `{name}` cannot be advertised as a tool: {e}"
            ))
        })?;
        let effect = manifest
            .effects
            .iter()
            .max()
            .copied()
            .unwrap_or(crate::record::Effect::Pure);
        Ok(Self {
            id: name.clone(),
            tool: CapsuleTool {
                host,
                name,
                description,
                schema,
                effect,
            },
        })
    }

    /// The host this plugin adapts.
    pub fn host(&self) -> &crate::capsule_host::CapsuleHost {
        &self.tool.host
    }
}

#[cfg(feature = "wasm")]
impl Plugin for CapsulePlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn apply(&self, ctx: &mut PluginContext) -> Result<()> {
        ctx.register_tool(self.tool.clone())
    }
}

/// The tool surface of one capsule: dispatch invokes the guest through its
/// host, journal-less (see [`CapsulePlugin`]).
#[cfg(feature = "wasm")]
#[derive(Clone)]
struct CapsuleTool {
    host: crate::capsule_host::CapsuleHost,
    name: String,
    description: String,
    schema: Value,
    effect: crate::record::Effect,
}

#[cfg(feature = "wasm")]
#[async_trait::async_trait]
impl Tool for CapsuleTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn effect(&self) -> crate::record::Effect {
        self.effect
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let outcome = self
            .host
            .invoke(crate::capsule_host::CapsuleInvocation::new(args))
            .await?;
        Ok(outcome.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A guard whose undo records that it fired — the unwind-order probe.
    fn recording_guard(name: &str, log: Arc<Mutex<Vec<String>>>) -> RegistrationGuard {
        let name = name.to_owned();
        RegistrationGuard {
            kind: "tool",
            name: name.clone(),
            undo: Some(Box::new(move || log.lock().unwrap().push(name.clone()))),
        }
    }

    #[test]
    fn guard_fires_once_and_reports_what_it_holds() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let guard = recording_guard("probe", Arc::clone(&log));
        let debug = format!("{guard:?}");
        assert!(debug.contains("tool `probe`"), "got: {debug}");
        assert!(debug.contains("armed: true"), "got: {debug}");
        drop(guard);
        assert_eq!(*log.lock().unwrap(), vec!["probe".to_owned()]);
        // The undo was taken on first fire; nothing remains to fire again.
    }

    #[test]
    fn unwind_is_reverse_registration_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let guards = vec![
            recording_guard("first", Arc::clone(&log)),
            recording_guard("second", Arc::clone(&log)),
            recording_guard("third", Arc::clone(&log)),
        ];
        unwind(guards);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["third".to_owned(), "second".to_owned(), "first".to_owned()]
        );
    }

    #[test]
    fn plugin_ids_follow_the_name_discipline() {
        assert!(validate_plugin_id("weather-pack").is_ok());
        assert!(validate_plugin_id("").is_err());
        assert!(validate_plugin_id(" padded").is_err());
        assert!(validate_plugin_id("with\ncontrol").is_err());
        assert!(validate_plugin_id(&"x".repeat(MAX_PLUGIN_ID_LEN + 1)).is_err());
    }

    #[test]
    fn config_ceiling_is_enforced_at_load() {
        struct Noop;
        impl Plugin for Noop {
            fn id(&self) -> &str {
                "noop"
            }
            fn apply(&self, _ctx: &mut PluginContext) -> Result<()> {
                Ok(())
            }
        }
        let mut kernel = PluginKernel::default();
        let oversized = json!({"blob": "x".repeat(MAX_PLUGIN_CONFIG_BYTES)});
        let error = kernel.load(Box::new(Noop), oversized).unwrap_err();
        assert!(error.to_string().contains("ceiling"), "got: {error}");
        assert!(kernel.fibers().is_empty());
    }
}
