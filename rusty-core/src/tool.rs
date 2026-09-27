//! Tool abstraction, registry, and parallel tool-call dispatch.
//!
//! A [`Tool`] is an async callable with a JSON-schema-described parameter
//! surface. [`ToolRegistry`] holds the tools available to an agent and emits
//! OpenAI-format tool schemas for the chat API. [`ToolExecutor`] dispatches
//! a batch of [`crate::llm::ToolCall`]s **in parallel** (the `ToolNode`
//! pattern of the prebuilt ReAct agent) and returns one `role: "tool"`
//! message per call, preserving call order.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::effects::{EffectAdmissionContext, EffectRequest};
use crate::error::{Result, RustyError};
use crate::journal::{EventDraft, Journal};
use crate::llm::{ChatMessage, ToolCall};
use crate::middleware::{MiddlewareChain, ToolInvocation};
use crate::record::{Effect, RunEventKind};
use crate::sandbox::{EnforcementLevel, SandboxExecutor};

pub mod approval;

/// Effect classification for execution placement (EP-05-S06).
///
/// Unlike [`crate::record::Effect`] which classifies retry safety, this
/// taxonomy decides *where* a tool executes: in-process for engine-state
/// reads, or behind a sandbox seam for anything touching code, files, or
/// the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Reads engine state only — may execute in-process when paired with
    /// [`SandboxRequirement::None`].
    Read,
    /// Executes model-influenced code or touches the host filesystem.
    Execute,
    /// Opens network connections.
    Egress,
}

/// Sandbox isolation requirement (EP-05-S06).
///
/// Declares whether a tool *must* run behind a sandbox backend that can
/// report its enforcement level honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRequirement {
    /// No sandbox required — may run in-process if [`EffectClass::Read`].
    None,
    /// Must run in a sandbox backend that reports enforcement.
    Required,
}

/// Where a tool call is placed (EP-05-S06).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Execute in the kernel process.
    InProcess,
    /// Execute in a sandbox backend.
    Sandboxed,
}

/// Why placement could not be satisfied (EP-05-S06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementError {
    /// A tool declares [`EffectClass::Execute`] or [`EffectClass::Egress`]
    /// with [`SandboxRequirement::None`] — a configuration that would run
    /// an unsafe tool in-process.
    InvalidDeclaration {
        tool: String,
        effect_class: EffectClass,
        sandbox_requirement: SandboxRequirement,
    },
    /// No sandbox backend is available for a tool that requires one.
    NoBackendAvailable {
        tool: String,
        required: SandboxRequirement,
    },
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlacementError::InvalidDeclaration {
                tool,
                effect_class,
                sandbox_requirement,
            } => write!(
                f,
                "tool `{tool}` declares effect class `{effect_class:?}` with sandbox requirement \"{sandbox_requirement:?}\": Execute/Egress tools must require a sandbox"
            ),
            PlacementError::NoBackendAvailable { tool, required } => write!(
                f,
                "tool `{tool}` requires sandbox \"{required:?}\" but no sandbox backend is available"
            ),
        }
    }
}

/// Resolve the execution placement for a tool (EP-05-S06).
///
/// Returns `Ok(Placement::InProcess)` for [`EffectClass::Read`] +
/// [`SandboxRequirement::None`]. Returns `Ok(Placement::Sandboxed)` for
/// any tool with [`SandboxRequirement::Required`]. Returns `Err` for
/// invalid declarations or when a sandbox is required but no backend
/// exists.
pub fn resolve_placement(
    tool: &dyn Tool,
    _sandbox_available: bool,
) -> std::result::Result<Placement, PlacementError> {
    let class = tool.effect_class();
    let req = tool.sandbox_requirement();

    // AC 2: Execute/Egress + None is a registration-time error.
    if matches!(class, EffectClass::Execute | EffectClass::Egress)
        && matches!(req, SandboxRequirement::None)
    {
        return Err(PlacementError::InvalidDeclaration {
            tool: tool.name().to_owned(),
            effect_class: class,
            sandbox_requirement: req,
        });
    }

    // AC 1: Read + None → in-process.
    if matches!(class, EffectClass::Read) && matches!(req, SandboxRequirement::None) {
        return Ok(Placement::InProcess);
    }

    // Everything else needs a sandbox backend.
    if !_sandbox_available {
        return Err(PlacementError::NoBackendAvailable {
            tool: tool.name().to_owned(),
            required: req,
        });
    }

    Ok(Placement::Sandboxed)
}
pub mod builtins;

/// Maximum serialized size of one advertised tool argument schema.
///
/// Tool schemas are copied into model requests and the server capability
/// handshake. Keeping the boundary here prevents one implementation from
/// turning either surface into an unbounded payload.
pub const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1024;

/// Maximum size of the model-facing description advertised for one tool.
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4 * 1024;

/// Reserved node-config key carrying the exact run-scoped tool allowlist.
///
/// Only [`crate::executor::Executor`] writes this key. Prebuilt agents read
/// it to narrow both model-visible schemas and executable dispatch.
pub const TOOL_ALLOWLIST_KEY: &str = "__rusty_tool_allowlist";

/// One finalized tool call as a guard sees it (evidence and admission
/// wave): the tool name, the post-middleware arguments, the tool's declared
/// effect class, and the run scope (the thread id) the dispatch happens in.
///
/// The view is borrowed and read-only: a guard inspects, it never rewrites.
/// Admission-time mutation is the middleware layer's job; by the time a
/// guard runs, the call is final.
#[derive(Debug)]
pub struct GuardedCall<'a> {
    /// The resolved tool name (post-allowlist admission).
    pub tool: &'a str,
    /// The finalized, model-supplied arguments.
    pub arguments: &'a Value,
    /// The effect class the tool declared for itself.
    pub effect: Effect,
    /// The run scope (thread id) the dispatch happens in.
    pub scope: &'a str,
}

/// One guard's refusal of one call, attributable by construction: the guard
/// names itself and states its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardDenial {
    /// The denying guard's own name ([`ToolGuard::name`]).
    pub guard: String,
    /// Why the guard refused, in terms an audit can act on.
    pub reason: String,
}

impl GuardDenial {
    /// A denial from `guard` with `reason`.
    pub fn new(guard: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            guard: guard.into(),
            reason: reason.into(),
        }
    }
}

/// The journaled record of a guard-denied dispatch (evidence and admission
/// wave): the output payload of [`RunEventKind::ToolCallDenied`]. Every
/// guard that denied is listed — guards compose as any-denial-denies, and
/// the evidence names each of them, so the record is attributable to
/// declarations rather than to whichever guard happened to answer first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardDenialRecord {
    /// The tool whose dispatch was blocked.
    pub tool: String,
    /// The effect class the tool declared.
    pub effect: Effect,
    /// The run scope the denial happened in.
    pub scope: String,
    /// Every denial the registered guards returned for this call, in
    /// registration order.
    pub denials: Vec<GuardDenial>,
}

/// A deny-only dispatch guard (evidence and admission wave).
///
/// A guard sees the finalized call and either denies it or stays silent —
/// [`ToolGuard::check`] returns `Option<GuardDenial>` and there is
/// deliberately no allow result. The layer is monotonic: nothing a guard
/// can return widens access, so no registration order can undo another
/// guard's denial, and a guard added to a run can only ever narrow what the
/// allowlist admitted.
///
/// Guards are registered per run ([`crate::executor::RunConfig::with_tool_guards`])
/// and evaluated at dispatch, after allowlist admission and before the
/// effect boundary: a denial blocks the call before any one-shot approval
/// token is consumed. Every registered guard is evaluated on every call —
/// no short-circuit — so a denial can never hide behind an earlier guard's
/// pass. Denials are journaled as [`RunEventKind::ToolCallDenied`] when the
/// dispatcher carries a guard journal
/// ([`ToolExecutor::with_guard_journal`]); exact replay re-derives them by
/// re-running the same guards, never by serving them.
pub trait ToolGuard: std::fmt::Debug + Send + Sync {
    /// The guard's own name, journaled with every denial it returns.
    fn name(&self) -> &str;

    /// Judge one finalized call. `Some(denial)` blocks the dispatch; `None`
    /// is silence, not permission — the call proceeds only because no guard
    /// denied it.
    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial>;
}

/// The journal handle a [`ToolExecutor`] records guard denials into: the
/// run's journal plus the causal parent of the current invocation (the
/// node-input event id, the same anchor the recording wrappers use).
#[derive(Debug, Clone)]
pub(crate) struct GuardEvidence {
    pub(crate) journal: Journal,
    pub(crate) parent: String,
}

/// The executable contract Studio and other clients may safely present.
///
/// This is derived from a real [`Tool`] rather than separately authored
/// metadata. The graph registry therefore advertises the same name, schema,
/// and effect class that the runtime executor will use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCapability {
    /// Stable tool name emitted by the model in a tool call.
    pub name: String,
    /// Human/model-facing explanation of the action.
    pub description: String,
    /// JSON Schema object accepted by the tool.
    pub parameters_schema: Value,
    /// Runtime effect class enforced and journaled for calls.
    pub effect: crate::record::Effect,
    /// The read-only tool this write checks itself with when its answer is
    /// lost (a connector operation's `reconcile`), by full name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile: Option<String>,
}

impl ToolCapability {
    fn from_tool(tool: &dyn Tool) -> Result<Self> {
        validate_tool_contract(tool.name(), tool.description(), &tool.parameters_schema())?;
        Ok(Self {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            parameters_schema: tool.parameters_schema(),
            effect: tool.effect(),
            reconcile: None,
        })
    }
}

/// The executable-contract rules every advertised tool surface must meet:
/// a bounded, wire-safe name; a non-empty, trimmed, control-free,
/// bounded description; and a JSON-object parameter schema within
/// [`MAX_TOOL_SCHEMA_BYTES`]. `pub(crate)` so the composer plane validates
/// drafted tool definitions against exactly these rules instead of
/// restating them.
pub(crate) fn validate_tool_contract(
    name: &str,
    description: &str,
    parameters_schema: &Value,
) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(RustyError::Tool(format!(
            "tool name `{name}` must use 1..=128 ASCII letters, digits, `.`, `_`, `:`, or `-`"
        )));
    }
    if description.is_empty()
        || description != description.trim()
        || description.len() > MAX_TOOL_DESCRIPTION_BYTES
        || description.chars().any(char::is_control)
    {
        return Err(RustyError::Tool(format!(
            "tool `{name}` description must be non-empty, trimmed, control-free, and at most {MAX_TOOL_DESCRIPTION_BYTES} bytes"
        )));
    }
    if !parameters_schema.is_object() {
        return Err(RustyError::Tool(format!(
            "tool `{name}` parameters schema must be a JSON object"
        )));
    }
    let schema_bytes = serde_json::to_vec(parameters_schema).map_err(|error| {
        RustyError::Tool(format!(
            "tool `{name}` parameters schema did not serialize: {error}"
        ))
    })?;
    if schema_bytes.len() > MAX_TOOL_SCHEMA_BYTES {
        return Err(RustyError::Tool(format!(
            "tool `{name}` parameters schema exceeds {MAX_TOOL_SCHEMA_BYTES} bytes"
        )));
    }
    Ok(())
}

/// An invocable tool.
///
/// Implement directly for stateful tools, or wrap async closures with a
/// small adapter struct. `parameters_schema` should be a JSON Schema object
/// (`{"type": "object", "properties": {...}, "required": [...]}`).
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool name — must match what the model emits in `tool_calls`.
    fn name(&self) -> &str;

    /// Human/model-facing description used in the tool schema.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's arguments.
    fn parameters_schema(&self) -> Value;

    /// The declared effect classification of calling this tool (Flight
    /// Recorder, R0.5): recorded on tool-call journal events and used by
    /// retry/replay policy.
    ///
    /// The default is [`crate::record::Effect::NonIdempotent`] — the runtime
    /// cannot prove a tool call is safely repeatable, so it assumes the
    /// restrictive class. Override to `ReadOnly` for pure lookups or
    /// `Idempotent` for keyed writes; never declare a weaker class than the
    /// tool's real behavior.
    fn effect(&self) -> crate::record::Effect {
        crate::record::Effect::NonIdempotent
    }

    /// Whether this call carries the approval the tool itself verifies —
    /// an in-band token in its arguments, checked with `admit_irreversible`
    /// against the tool's own derived effect id. The run's approval gate
    /// does not ask a person again for such a call; the tool refuses it
    /// itself if the token is wrong. Default: no.
    fn carries_approval(&self, _call: &ToolCall) -> bool {
        false
    }

    /// Stable effect kind used for deterministic effect ids and compensation
    /// lookup. Defaults to the tool name.
    /// Execution placement effect class (EP-05-S06).
    ///
    /// Defaults to [`EffectClass::Read`] — the most permissive for
    /// in-process execution. Tools that touch code, files, or the network
    /// must override to [`EffectClass::Execute`] or [`EffectClass::Egress`]
    /// and pair with [`SandboxRequirement::Required`].
    fn effect_class(&self) -> EffectClass {
        EffectClass::Read
    }

    /// Sandbox isolation requirement (EP-05-S06).
    ///
    /// Defaults to [`SandboxRequirement::None`] — suitable for
    /// [`EffectClass::Read`] tools. Tools with [`EffectClass::Execute`] or
    /// [`EffectClass::Egress`] must override to [`SandboxRequirement::Required`].
    fn sandbox_requirement(&self) -> SandboxRequirement {
        SandboxRequirement::None
    }

    fn effect_kind(&self) -> &str {
        self.name()
    }

    /// Stable idempotency key for this call, when [`Tool::effect`] declares
    /// [`crate::record::Effect::Idempotent`]. The admission boundary rejects
    /// an idempotent call that returns `None` here.
    fn idempotency_key(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Describe this concrete call for the runtime admission boundary.
    ///
    /// The default combines the tool's declared class and stable kind with a
    /// canonical hash of the post-middleware arguments and tool-call id. The
    /// call id is the occurrence discriminator: two identical irreversible
    /// calls cannot spend the same approval. Wrappers that override this
    /// method must delegate it to remain transparent.
    fn effect_request(&self, call: &ToolCall) -> EffectRequest {
        let input = json!({
            "arguments": &call.arguments,
            "tool_call_id": &call.id,
        });
        EffectRequest::new(
            self.effect_kind(),
            self.effect(),
            &input,
            self.idempotency_key(&call.arguments),
        )
    }

    /// Execute the tool with model-supplied arguments.
    async fn call(&self, args: Value) -> Result<Value>;
}

/// Tools that exist because of something that changes while the server runs.
///
/// A registry is built once, when a graph is. The systems an agent can reach
/// are not: a connection configured this afternoon has to be callable this
/// afternoon, without anyone restarting the process. A source is consulted
/// on every read, so whatever it holds *now* is what the registry offers —
/// to the model's schema list, to dispatch, to the catalog a run validates
/// its allowlist against. A statically registered tool always wins a name
/// collision: a source can extend a graph, never shadow it.
pub trait ToolSource: Send + Sync {
    /// The tools this source currently provides.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// A registry of tools, shared cheaply via `Arc<dyn Tool>`.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    sources: Vec<Arc<dyn ToolSource>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("sources", &self.sources.len())
            .finish()
    }
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Re-registering the same name replaces the tool.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> &mut Self {
        let name = tool.name().to_owned();
        if let Err(PlacementError::InvalidDeclaration { .. }) = resolve_placement(&tool, false) {
            panic!(
                "tool `{name}` declares an invalid placement: Execute/Egress tools must require a sandbox"
            );
        }
        self.tools.insert(name, Arc::new(tool));
        self
    }

    /// Register a pre-shared tool.
    pub fn register_shared(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.name().to_owned();
        if let Err(PlacementError::InvalidDeclaration { .. }) =
            resolve_placement(tool.as_ref(), false)
        {
            panic!(
                "tool `{name}` declares an invalid placement: Execute/Egress tools must require a sandbox"
            );
        }
        self.tools.insert(name, tool);
        self
    }

    /// Remove a tool by name, returning what was registered.
    ///
    /// The removal half of the plugin kernel's revertible registrations
    /// ([`crate::plugin`]): a registration guard removes exactly the entry
    /// it inserted when its plugin unloads. Static composition has no use
    /// for it — a plane built once never unregisters — so no existing
    /// caller changes shape.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Attach a live source. Its tools join every read from now on.
    pub fn attach(&mut self, source: Arc<dyn ToolSource>) -> &mut Self {
        self.sources.push(source);
        self
    }

    /// Every tool this registry offers right now — the static ones plus
    /// whatever the attached sources currently provide — by name, with a
    /// statically registered tool winning any collision. Sorted, so the
    /// order the model sees is stable across reads.
    fn all(&self) -> Vec<(String, Arc<dyn Tool>)> {
        let mut merged: std::collections::BTreeMap<String, Arc<dyn Tool>> = self
            .sources
            .iter()
            .flat_map(|source| source.tools())
            .map(|tool| (tool.name().to_owned(), tool))
            .collect();
        for (name, tool) in &self.tools {
            merged.insert(name.clone(), Arc::clone(tool));
        }
        merged.into_iter().collect()
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.tools.get(name) {
            return Some(Arc::clone(tool));
        }
        self.sources
            .iter()
            .flat_map(|source| source.tools())
            .find(|tool| tool.name() == name)
    }

    /// `true` if a tool with this name is offered.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// All offered tool names, sorted.
    pub fn names(&self) -> impl Iterator<Item = String> {
        self.all().into_iter().map(|(name, _)| name)
    }

    /// Number of offered tools.
    pub fn len(&self) -> usize {
        self.all().len()
    }

    /// `true` if no tool is offered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.sources.iter().all(|source| source.tools().is_empty())
    }

    /// All offered tools, sorted by name.
    pub fn tools(&self) -> impl Iterator<Item = Arc<dyn Tool>> {
        self.all().into_iter().map(|(_, tool)| tool)
    }

    /// OpenAI-format tool schemas for the chat API, one per registered tool:
    /// `{"type": "function", "function": {"name", "description", "parameters"}}`.
    /// Pass directly as the `tools` argument of
    /// [`crate::llm::ChatModel::chat`].
    pub fn schemas(&self) -> Vec<Value> {
        self.tools()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters_schema(),
                    }
                })
            })
            .collect()
    }

    /// Derive the user-facing capability catalog from the executable tools.
    ///
    /// The result is sorted by stable tool name so `/info`, Studio reviews,
    /// and content-addressed configuration do not depend on `HashMap`
    /// iteration order. Invalid contracts fail closed before a graph can
    /// advertise them.
    pub fn capabilities(&self) -> Result<Vec<ToolCapability>> {
        let mut capabilities = self
            .tools()
            .map(|tool| ToolCapability::from_tool(tool.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        capabilities.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(capabilities)
    }

    /// Clone the exact subset named by `allowlist`.
    ///
    /// An empty allowlist produces an empty registry. Unknown or duplicate
    /// names fail closed so a configuration typo cannot silently broaden or
    /// ambiguously describe the tools available to a run.
    pub fn restricted_to(&self, allowlist: &[String]) -> Result<Self> {
        let mut selected = Self::new();
        let mut seen = HashSet::with_capacity(allowlist.len());
        for name in allowlist {
            if !seen.insert(name.as_str()) {
                return Err(RustyError::Tool(format!(
                    "tool allowlist contains duplicate `{name}`"
                )));
            }
            let tool = self.get(name).ok_or_else(|| {
                RustyError::Tool(format!(
                    "tool allowlist names `{name}`, which is not registered"
                ))
            })?;
            selected.register_shared(tool);
        }
        Ok(selected)
    }
}

/// Dispatches tool calls against a registry, in parallel.
///
/// Typical use in a ReAct `tools` node: take the assistant message's
/// `tool_calls`, `execute_batch` them, and append the resulting tool
/// messages to the `messages` channel via the `AddMessages` reducer.
///
/// Attach a [`MiddlewareChain`] via [`ToolExecutor::with_middleware`] to run
/// every dispatched call through the chain's tool hooks (Middleware /
/// Interceptor SDK): a layer may mutate the call, reject it (surfacing as an
/// `ERROR:` tool message under the same failure-isolation contract below),
/// or short-circuit it with a substitute result.
/// Whom a run is a conversation with, as the application declares it: a
/// person (their principal id), or nobody — a run a schedule or a webhook
/// fired acts for the agent, whoever created the channel. On the wire
/// `{"person": "<id>"}` or `{"nobody": true}` — never `null`, which an
/// optional field would read back as undeclared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Counterpart {
    Person(String),
    Nobody,
}

impl Counterpart {
    /// The wire form (see the type's docs).
    pub fn to_value(&self) -> Value {
        match self {
            Counterpart::Person(person) => serde_json::json!({ "person": person }),
            Counterpart::Nobody => serde_json::json!({ "nobody": true }),
        }
    }

    /// From the wire form; a bare string is a person, anything else nobody.
    pub fn from_value(value: &Value) -> Self {
        if let Some(person) = value.get("person").and_then(Value::as_str) {
            return Counterpart::Person(person.to_owned());
        }
        match value.as_str() {
            Some(person) if !person.is_empty() => Counterpart::Person(person.to_owned()),
            _ => Counterpart::Nobody,
        }
    }
}

/// The run a tool is acting in: who it is for, which agent, which thread,
/// and the run's journal. Set by the dispatching node for the duration of
/// each dispatch (a task-local, never threaded through arguments), read by
/// a tool through [`current_run`]. A tool called outside a run — a unit
/// test, a direct call — sees `None` and behaves as it always did.
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    /// The run's id (the journal's).
    pub run_id: String,
    /// The thread the run executes on.
    pub thread_id: String,
    /// Who started the run, as the run config declared it
    /// (`{principal_id, name, kind}` on the server); `None` when undeclared.
    pub attribution: Option<Value>,
    /// The run's execution authority as admission stamped it:
    /// `{tenant, actor, subject, via, admitted_at}`. `None` for a run the
    /// server admitted before it stamped one, or a direct call.
    pub execution: Option<Value>,
    /// The agent (assistant) this run is of, when the run declared one.
    pub agent_id: Option<String>,
    /// Whom this run is a conversation with, when the application declared
    /// it ([`crate::executor::RunConfig::counterpart`]): a person, or nobody
    /// — a run a schedule or a webhook fired, whoever created the channel.
    /// `None` is undeclared, and the attribution decides.
    pub counterpart: Option<Counterpart>,
    /// The run's journal, for a tool that records an effect of its own
    /// (a memory write) as evidence on the run.
    pub journal: Option<crate::journal::Journal>,
}

impl RunContext {
    /// The tenant the run acts in, from the execution block; `None` when
    /// the run declared none (the caller falls back to its default).
    pub fn tenant(&self) -> Option<&str> {
        self.execution.as_ref()?.get("tenant")?.as_str()
    }

    /// The actor who admitted the run (`{principal_id, name, kind}`), from
    /// the execution block, else the attribution.
    pub fn actor(&self) -> Option<&Value> {
        self.execution
            .as_ref()
            .and_then(|e| e.get("actor"))
            .filter(|a| a.is_object())
            .or(self.attribution.as_ref())
    }

    /// The person the run acts for: the declared `acting_for` when the
    /// application set one; otherwise the attribution's `principal_id`
    /// when its `kind` is `user`. A service principal, a channel-fired run
    /// that declared nobody, or an undeclared run has none.
    pub fn person_id(&self) -> Option<&str> {
        match &self.counterpart {
            Some(Counterpart::Person(person)) => return Some(person),
            Some(Counterpart::Nobody) => return None,
            None => {}
        }
        let attribution = self.attribution.as_ref()?;
        if attribution.get("kind").and_then(Value::as_str) != Some("user") {
            return None;
        }
        attribution.get("principal_id").and_then(Value::as_str)
    }
}

tokio::task_local! {
    static RUN_CONTEXT: RunContext;
}

/// The run the calling tool is acting in, when it is being dispatched by a
/// node that declared one; `None` otherwise.
pub fn current_run() -> Option<RunContext> {
    RUN_CONTEXT.try_with(|context| context.clone()).ok()
}

#[derive(Debug, Clone, Default)]
pub struct ToolExecutor {
    registry: ToolRegistry,
    middleware: MiddlewareChain,
    effect_admission: Option<EffectAdmissionContext>,
    guards: Vec<Arc<dyn ToolGuard>>,
    guard_evidence: Option<GuardEvidence>,
    thread_id: String,
    node: String,
    sandbox: Option<Arc<dyn SandboxExecutor>>,
    run_context: Option<RunContext>,
}

impl ToolExecutor {
    /// An executor over `registry`.
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            ..Self::default()
        }
    }

    /// The underlying registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Builder-style: run every dispatched call through `chain`'s tool
    /// hooks. Typically handed the chain from
    /// [`crate::node::NodeContext::middleware`].
    pub fn with_middleware(mut self, chain: MiddlewareChain) -> Self {
        self.middleware = chain;
        self
    }

    /// Builder-style: enforce the run-scoped effect boundary on every tool
    /// body this executor dispatches.
    pub fn with_effect_admission(mut self, context: EffectAdmissionContext) -> Self {
        self.effect_admission = Some(context);
        self
    }

    /// Builder-style: label dispatched calls with the thread and node they
    /// originate from (flowing into the [`ToolInvocation`] context).
    pub fn with_call_context(
        mut self,
        thread_id: impl Into<String>,
        node: impl Into<String>,
    ) -> Self {
        self.thread_id = thread_id.into();
        self.node = node.into();
        self
    }

    /// Builder-style: the run every dispatched tool acts in (see
    /// [`RunContext`] and [`current_run`]).
    pub fn with_run_context(mut self, context: RunContext) -> Self {
        self.run_context = Some(context);
        self
    }

    /// Builder-style: evaluate the run's deny-only guards on every
    /// finalized call, after allowlist admission and before the effect
    /// boundary. See [`ToolGuard`] for the monotonicity contract.
    pub fn with_tool_guards(mut self, guards: Vec<Arc<dyn ToolGuard>>) -> Self {
        self.guards = guards;
        self
    }

    /// Builder-style: journal guard denials into `journal` with causal
    /// parent `parent` (the current invocation's node-input event id).
    /// Without this handle a denial still blocks the dispatch — a guard's
    /// verdict is authoritative — but no evidence handle means no
    /// [`RunEventKind::ToolCallDenied`] event, so evidence-carrying runs
    /// should always attach it. The prebuilt ReAct tools node does.
    pub fn with_guard_journal(mut self, journal: Journal, parent: impl Into<String>) -> Self {
        self.guard_evidence = Some(GuardEvidence {
            journal,
            parent: parent.into(),
        });
        self
    }

    /// Builder-style: attach a sandbox backend for tools whose placement
    /// resolves to [`Placement::Sandboxed`].
    pub fn with_sandbox(mut self, sandbox: Arc<dyn SandboxExecutor>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// The attached middleware chain (empty when none was added).
    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    /// The run's registered deny-only guards (empty when none were added).
    pub fn guards(&self) -> &[Arc<dyn ToolGuard>] {
        &self.guards
    }

    /// The attached effect boundary, if enforcement is enabled.
    pub fn effect_admission(&self) -> Option<&EffectAdmissionContext> {
        self.effect_admission.as_ref()
    }

    /// The calls of a batch that admission would refuse for want of an
    /// approval, each with the effect id a token must be minted against —
    /// asked before any call runs, so a run can pause and ask a person
    /// instead of letting the model watch its own call fail.
    pub fn approvals_needed(
        &self,
        gate: &EffectAdmissionContext,
        calls: &[ToolCall],
    ) -> Vec<ApprovalNeeded> {
        let context = gate;
        calls
            .iter()
            .filter_map(|call| {
                let tool = self.registry.get(&call.name)?;
                if tool.carries_approval(call) {
                    return None;
                }
                let request = tool.effect_request(call);
                let effect_id = context.approval_required(&request)?;
                Some(ApprovalNeeded {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    arguments: call.arguments.clone(),
                    kind: request.kind().to_owned(),
                    effect_id: effect_id.as_str().to_owned(),
                })
            })
            .collect()
    }

    /// Execute a batch of tool calls concurrently.
    ///
    /// Returns one [`ChatMessage::tool_result`] per call, **in the same
    /// order as `calls`** (order stability matters for conversation
    /// reconstruction). Individual failures do not abort the batch: a failed
    /// call yields a tool message whose content is the error description
    /// (prefixed with `ERROR:`), so the model can observe and recover from
    /// tool failures — matching `ToolNode`'s default `handle_tool_errors`
    /// behavior. A *panicking* tool is contained the same way: the unwind is
    /// caught and reported as an `ERROR:` tool message instead of taking
    /// down the batch (and the executor task driving it).
    pub async fn execute_batch(&self, calls: &[ToolCall]) -> Vec<ChatMessage> {
        let futures = calls.iter().map(|call| async move {
            match self.execute_one(call).await {
                Ok(Value::String(content)) => {
                    ChatMessage::tool_result(&call.id, shape_text(&content))
                }
                Ok(other) => ChatMessage::tool_result(&call.id, shape_result(&other)),
                Err(error) => ChatMessage::tool_result(&call.id, format!("ERROR: {error}")),
            }
        });
        futures::future::join_all(futures).await
    }

    /// Run `future` with this executor's [`RunContext`] in scope, so a tool
    /// asking [`current_run`] gets the run it is acting in; without one the
    /// future runs as before and `current_run` answers `None`.
    async fn in_run_scope<F: std::future::Future<Output = T>, T>(&self, future: F) -> T {
        match &self.run_context {
            Some(context) => RUN_CONTEXT.scope(context.clone(), future).await,
            None => future.await,
        }
    }

    /// Dispatch one call through the full admission pipeline and return the
    /// raw result.
    ///
    /// This is [`ToolExecutor::execute_batch`]'s singular form for drivers
    /// that steer on the `Result` itself — the code-mode interpreter
    /// ([`builtins::codemode`]) fails or tolerates a program step on it —
    /// rather than the batch's failure-isolating `ERROR:` tool message.
    /// Middleware, guards, the effect boundary, and panic containment apply
    /// exactly as in the batch path; only the result channel differs (a
    /// contained panic surfaces as an `Err`, not a message).
    pub async fn execute_one(&self, call: &ToolCall) -> Result<Value> {
        self.in_run_scope(self.execute_one_unscoped(call)).await
    }

    async fn execute_one_unscoped(&self, call: &ToolCall) -> Result<Value> {
        let registry = self.registry.clone();
        let chain = self.middleware.clone();
        let effect_admission = self.effect_admission.clone();
        let guards = self.guards.clone();
        let guard_evidence = self.guard_evidence.clone();
        let thread_id = self.thread_id.clone();
        let node = self.node.clone();
        let sandbox = self.sandbox.clone();
        let result = std::panic::AssertUnwindSafe(async {
            // The dispatch closure takes the call by value: the future it
            // returns must own the call it dispatches, or the borrow would
            // tie the future to the closure's argument lifetime and fail to
            // escape `run_tool`.
            let dispatch = |call: ToolCall| {
                let registry = registry.clone();
                let effect_admission = effect_admission.clone();
                let guards = guards.clone();
                let guard_evidence = guard_evidence.clone();
                let thread_id = thread_id.clone();
                let node = node.clone();
                async move {
                    dispatch_tool(
                        &registry,
                        &call,
                        effect_admission.as_ref(),
                        &guards,
                        guard_evidence.as_ref(),
                        &thread_id,
                        &node,
                        sandbox.as_ref(),
                    )
                    .await
                }
            };
            if chain.is_empty() {
                dispatch(call.clone()).await
            } else {
                let mut invocation =
                    ToolInvocation::new(thread_id.clone(), node.clone(), call.clone());
                invocation.set_effect(registry.get(&call.name).map(|t| t.effect()));
                chain
                    .run_tool(&mut invocation, |invocation| {
                        // The lookup happens after before-hooks, so a layer
                        // may rewrite the arguments — or the target tool name
                        // itself; guards judge the finalized call, after that
                        // rewrite.
                        dispatch(invocation.call().clone())
                    })
                    .await
            }
        })
        .catch_unwind()
        .await;
        match result {
            Ok(value) => value,
            Err(payload) => Err(RustyError::Tool(format!(
                "tool `{}` panicked: {}",
                call.name,
                // `&*`: `&payload` would unsize-coerce the *Box* itself into
                // `&dyn Any`, hiding the real payload.
                panic_message(&*payload)
            ))),
        }
    }
}

/// Resolve, admit, and invoke one finalized call. Middleware reaches this
/// function only after its before-hooks have settled the tool name and
/// arguments, so admission cannot be bypassed by rewriting a call after it
/// was approved.
///
/// Under a shadow boundary (R0.12 wave 4) a refused call is not an error
/// by default: the admission context serves the recorded outcome from the
/// source run's journal — the hybrid-replay rule, pin the effect and
/// re-run the decision — and reports the refusal to its sink either way.
/// Only a call the recorded world never saw surfaces as a failure, and a
/// non-shadow context answers `None` from
/// [`EffectAdmissionContext::serve_shadow`] unchanged.
#[allow(clippy::too_many_arguments)]
async fn dispatch_tool(
    registry: &ToolRegistry,
    call: &ToolCall,
    effect_admission: Option<&EffectAdmissionContext>,
    guards: &[Arc<dyn ToolGuard>],
    guard_evidence: Option<&GuardEvidence>,
    scope: &str,
    node: &str,
    sandbox: Option<&Arc<dyn SandboxExecutor>>,
) -> Result<Value> {
    let tool = registry
        .get(&call.name)
        .ok_or_else(|| RustyError::Tool(format!("unknown tool `{}`", call.name)))?;
    // The guard layer: deny-only, evaluated on the finalized call after
    // allowlist admission (the restricted registry lookup above) and before
    // the effect boundary, so a denial never burns a one-shot approval.
    // Every guard is evaluated — no short-circuit — so no registration
    // order can hide a denial behind an earlier pass.
    if !guards.is_empty() {
        let guarded = GuardedCall {
            tool: &call.name,
            arguments: &call.arguments,
            effect: tool.effect(),
            scope,
        };
        let denials: Vec<GuardDenial> = guards
            .iter()
            .filter_map(|guard| guard.check(&guarded))
            .collect();
        if !denials.is_empty() {
            if let Some(evidence) = guard_evidence {
                let record = GuardDenialRecord {
                    tool: call.name.clone(),
                    effect: tool.effect(),
                    scope: scope.to_owned(),
                    denials: denials.clone(),
                };
                let mut draft = EventDraft::new(RunEventKind::ToolCallDenied, Effect::Pure)
                    .input(crate::replay::tool_call_request(
                        &call.name,
                        &call.arguments,
                    ))
                    .output(serde_json::to_value(&record)?)
                    .parent(evidence.parent.clone());
                if !node.is_empty() {
                    draft = draft.node(node.to_owned());
                }
                evidence.journal.record(draft);
            }
            let reasons = denials
                .iter()
                .map(|denial| format!("guard `{}`: {}", denial.guard, denial.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RustyError::Tool(format!(
                "tool guard denied `{}`: {reasons}",
                call.name
            )));
        }
    }
    // Placement resolution: after guards, before effect admission. The
    // placement is only *resolved* here — nothing executes until the call
    // is admitted below (Astra R04: one admitted-invocation boundary before
    // any backend, in-process or sandboxed alike).
    let placement = resolve_placement(tool.as_ref(), sandbox.is_some())
        .map_err(|e| RustyError::Tool(e.to_string()))?;
    if placement == Placement::Sandboxed {
        let backend = sandbox
            .as_ref()
            .expect("sandbox available verified by resolve_placement");
        // EP-05-S05 AC: Required + Partial enforcement → typed denial.
        if tool.sandbox_requirement() == SandboxRequirement::Required
            && backend.enforcement() == EnforcementLevel::Partial
        {
            return Err(RustyError::Tool(format!(
                "tool `{}` requires full sandbox enforcement but backend `{}` reports partial",
                call.name,
                backend.backend_id()
            )));
        }
    }

    // The effect boundary: every backend sits behind it. A refused call
    // never reaches a backend — in-process or sandboxed — and under a
    // shadow boundary is served from the recorded world instead. The permit
    // is held across the call so the compensation it selected outlives a
    // registry change until the effect finishes.
    let _permit = match effect_admission {
        Some(context) => {
            let request = tool.effect_request(call);
            match context.admit(&request) {
                Ok(permit) => Some(permit),
                Err(violation) => {
                    return match context.serve_shadow(
                        &request,
                        &crate::replay::tool_call_request(&call.name, &call.arguments),
                        &violation,
                    ) {
                        Some(recorded) => Ok(recorded),
                        None => Err(RustyError::Tool(format!(
                            "effect admission denied: {violation}"
                        ))),
                    };
                }
            }
        }
        None => None,
    };

    match placement {
        Placement::InProcess => tool.call(call.arguments.clone()).await,
        Placement::Sandboxed => {
            let backend = sandbox
                .as_ref()
                .expect("sandbox available verified by resolve_placement");
            // The backend runs the tool by name, outside any recording
            // wrapper the registry holds, so the journal is written here —
            // the same `ToolCall` event, with the same anchor, an in-process
            // call gets. A placement class is not a way out of the record.
            let started = guard_evidence.map(|evidence| evidence.journal.clock().now());
            let result = backend
                .execute(&call.name, &[serde_json::to_string(&call.arguments)?])
                .await
                .and_then(|result| serde_json::to_value(result).map_err(Into::into));
            if let (Some(evidence), Some(started)) = (guard_evidence, started) {
                let latency_ms = (evidence.journal.clock().now() - started)
                    .num_milliseconds()
                    .max(0) as u64;
                let mut draft = EventDraft::new(RunEventKind::ToolCall, tool.effect())
                    .input(crate::replay::tool_call_request(
                        &call.name,
                        &call.arguments,
                    ))
                    .latency_ms(latency_ms)
                    .parent(evidence.parent.clone());
                if !node.is_empty() {
                    draft = draft.node(node.to_owned());
                }
                draft = match &result {
                    Ok(value) => draft.output(value.clone()),
                    Err(error) => draft
                        .status(crate::record::EventStatus::Error)
                        .output(serde_json::json!({ "error": error.to_string() })),
                };
                evidence.journal.record(draft);
            }
            result
        }
    }
}

/// Best-effort extraction of a panic payload for error reporting.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string payload>".to_owned()
    }
}

// --------------------------------------------------------------------- //
// Failures the loop can act on
// --------------------------------------------------------------------- //

/// A tool failure the model can act on: its class, what the system said,
/// whether anything reached the system, whether calling again is safe,
/// and the one next action that fits. Serialized after `ERROR: ` as the
/// tool result, the same channel as any failure, so a model that ignores
/// the shape still reads the words.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolFailure {
    pub kind: String,
    /// invalid_arguments | denied | not_found | conflict | rate_limited |
    /// transient | unknown_outcome | dependency | bounded | reconcile_first
    /// | unexpected
    pub class: String,
    pub tool: String,
    pub detail: String,
    /// Whether the request may have reached the system.
    pub sent: bool,
    /// Whether the same call may be repeated as it is.
    pub retry_safe: bool,
    pub next: String,
}

impl ToolFailure {
    pub const KIND: &'static str = "tool_failure";

    pub fn new(
        class: &str,
        tool: &str,
        detail: impl Into<String>,
        sent: bool,
        retry_safe: bool,
        next: impl Into<String>,
    ) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            class: class.to_owned(),
            tool: tool.to_owned(),
            detail: detail.into(),
            sent,
            retry_safe,
            next: next.into(),
        }
    }

    /// The error a tool returns to carry this failure.
    pub fn into_error(self) -> RustyError {
        RustyError::Tool(serde_json::to_string(&self).unwrap_or_else(|_| self.detail.clone()))
    }

    /// A tool result's content as a failure, when it is one (with or
    /// without the `ERROR: ` prefix the batch executor adds).
    pub fn parse(content: &str) -> Option<Self> {
        let text = content
            .trim()
            .strip_prefix("ERROR:")
            .map(str::trim)
            .unwrap_or(content.trim());
        let text = text
            .strip_prefix("tool error:")
            .map(str::trim)
            .unwrap_or(text);
        let parsed: Self = serde_json::from_str(text).ok()?;
        (parsed.kind == Self::KIND).then_some(parsed)
    }
}

/// How many identical calls may fail before the loop is told to change
/// course.
pub const FAILURE_BOUND: usize = 2;

/// The failure policy, judged from the conversation: the same call (tool and
/// arguments) that already failed [`FAILURE_BOUND`] times is refused with
/// `bounded`; a write whose earlier identical attempt may have happened
/// (`unknown_outcome`) is refused with `reconcile_first` until a read-only
/// tool has answered since. Pure on the thread, so record and replay reach
/// the same verdict. `None` means dispatch.
pub fn failure_policy(
    history: &[ChatMessage],
    call: &ToolCall,
    registry: &ToolRegistry,
) -> Option<ToolFailure> {
    let effect_of = |name: &str| registry.get(name).map(|t| t.effect());
    // Pair every tool call in the thread with its result, in order.
    let mut attempts: Vec<(usize, &ToolCall, Option<&str>)> = Vec::new();
    for (i, message) in history.iter().enumerate() {
        for tc in &message.tool_calls {
            let result = history[i..]
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(tc.id.as_str()))
                .and_then(|m| m.content.as_deref());
            attempts.push((i, tc, result));
        }
    }
    let identical = |tc: &ToolCall| tc.name == call.name && tc.arguments == call.arguments;
    // A real failure: the tool ran and failed. The policy's own refusals
    // (`bounded`, `reconcile_first`) and the repeat notice are not attempts.
    let real_failure = |content: &str| {
        content.trim_start().starts_with("ERROR:")
            && !ToolFailure::parse(content)
                .is_some_and(|f| f.class == "bounded" || f.class == "reconcile_first")
    };
    let failures = attempts
        .iter()
        .filter(|(_, tc, r)| identical(tc) && r.is_some_and(real_failure))
        .count();
    if failures >= FAILURE_BOUND {
        return Some(ToolFailure::new(
            "bounded",
            &call.name,
            format!("this exact call already failed {failures} times in this conversation"),
            false,
            false,
            "change the arguments or the approach, or say what you need — the same call again is not tried",
        ));
    }
    let writes = !matches!(
        effect_of(&call.name),
        Some(Effect::ReadOnly) | Some(Effect::Pure)
    );
    if writes {
        let uncertain = attempts
            .iter()
            .filter_map(|(i, tc, r)| {
                let failure = r.and_then(ToolFailure::parse)?;
                (identical(tc) && failure.class == "unknown_outcome")
                    .then_some((*i, failure.retry_safe))
            })
            .max_by_key(|(i, _)| *i);
        // The tool's own read-back found nothing (`retry_safe`): one re-send
        // is safe, and only one — a second lost answer starts over.
        if let Some((at, true)) = uncertain {
            let resent_since = attempts.iter().any(|(i, tc, _)| *i > at && identical(tc));
            if !resent_since {
                return None;
            }
        }
        if let Some((at, false)) = uncertain {
            let read_since = attempts.iter().any(|(i, tc, r)| {
                *i > at
                    && matches!(
                        effect_of(&tc.name),
                        Some(Effect::ReadOnly) | Some(Effect::Pure)
                    )
                    && r.is_some_and(|c| !c.trim_start().starts_with("ERROR:"))
            });
            if !read_since {
                return Some(ToolFailure::new(
                    "reconcile_first",
                    &call.name,
                    "an identical write was sent earlier and its answer was lost — it may have happened",
                    false,
                    false,
                    "read the record back with a read-only tool first; send the write again only if it is not there",
                ));
            }
        }
    }
    None
}

// --------------------------------------------------------------------- //
// Result shaping: what the model is shown of a large result
// --------------------------------------------------------------------- //

/// One call a run must pause for: what it would do, and the effect id an
/// approval has to be scoped to. Serialized into the interrupt a person reads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalNeeded {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    /// The effect kind the tool declares (its name, for a connector tool).
    pub kind: String,
    /// The id a token must be minted against — this occurrence, no other.
    pub effect_id: String,
}

/// The most bytes of one tool result the model is shown whole. A result past
/// this is *shaped*: the model gets what it can act on — how big it was,
/// what it is (a list of N items, an object), the first items with their
/// most telling fields, every key that exists — and is told how to ask for
/// less. Without this a 160 KB record list lands in the context whole,
/// evicts everything else, and the agent concludes there was nothing there.
pub const TOOL_RESULT_INLINE_BYTES: usize = 24 * 1024;

/// Fields worth keeping first when an item has more than fit: identifiers,
/// titles, states, owners, times — the columns a person would put in a table.
const TELLING_KEYS: &[&str] = &[
    "number",
    "id",
    "key",
    "name",
    "title",
    "subject",
    "summary",
    "short_description",
    "description",
    "priority",
    "severity",
    "urgency",
    "state",
    "status",
    "category",
    "type",
    "kind",
    "assigned_to",
    "assignment_group",
    "owner",
    "requester",
    "caller_id",
    "author",
    "login",
    "opened_at",
    "created_at",
    "created_on",
    "sys_created_on",
    "updated_at",
    "sys_updated_on",
    "closed_at",
    "due_date",
    "url",
    "html_url",
    "link",
    "email",
    "count",
    "total",
    "active",
];
const MAX_ITEM_KEYS: usize = 12;
const MAX_VALUE_CHARS: usize = 160;

/// A text result the model can hold, or its head with an honest note.
pub fn shape_text(text: &str) -> String {
    if text.len() <= TOOL_RESULT_INLINE_BYTES {
        return text.to_owned();
    }
    let mut cut = TOOL_RESULT_INLINE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "SHAPED RESULT — {} bytes, too large to show whole; the first {} are below. Ask the tool for less (a limit, a page, specific fields) to see the rest.\n{}",
        text.len(),
        cut,
        &text[..cut]
    )
}

/// A JSON result the model can hold, or a digest of it: the list or object
/// it is, its size, the first items with their most telling fields, and the
/// keys that exist so the model can ask for exactly what it needs.
pub fn shape_result(value: &Value) -> String {
    let whole = value.to_string();
    if whole.len() <= TOOL_RESULT_INLINE_BYTES {
        return whole;
    }
    // The payload: a top-level list, or an envelope whose one collection
    // holds the list (`{"result": [...]}`, `{"items": [...], "total": 9}`).
    let (list, envelope): (Option<&Vec<Value>>, Option<String>) = match value {
        Value::Array(items) => (Some(items), None),
        Value::Object(map) => {
            let collections: Vec<(&String, &Vec<Value>)> = map
                .iter()
                .filter_map(|(k, v)| v.as_array().map(|a| (k, a)))
                .collect();
            // One collection that is most of the bytes is the payload; a
            // small list beside big scalars is just another field.
            match collections.as_slice() {
                [(key, items)]
                    if Value::Array((*items).clone()).to_string().len() * 2 >= whole.len() =>
                {
                    (Some(items), Some((*key).clone()))
                }
                _ => (None, None),
            }
        }
        _ => (None, None),
    };
    match list {
        Some(items) => {
            let keys = all_keys(items);
            let mut shown: Vec<Value> = Vec::new();
            let mut used = 0usize;
            for item in items {
                let trimmed = trim_item(item);
                let bytes = trimmed.to_string().len() + 2;
                if used + bytes > TOOL_RESULT_INLINE_BYTES - 1024 {
                    break;
                }
                used += bytes;
                shown.push(trimmed);
            }
            let fields = items
                .first()
                .and_then(Value::as_object)
                .map(|o| o.len())
                .unwrap_or(0);
            let per_item = shown
                .first()
                .and_then(Value::as_object)
                .map(|o| o.len())
                .unwrap_or(0);
            let siblings: Vec<String> = match value {
                Value::Object(map) => map
                    .iter()
                    .filter(|(_, v)| !v.is_array())
                    .map(|(k, v)| format!("{k}: {}", clip(&scalar_text(v))))
                    .collect(),
                _ => Vec::new(),
            };
            let mut note = format!(
                "SHAPED RESULT — {} bytes, too large to show whole. A list of {} items{}; showing the first {}{}.",
                whole.len(),
                items.len(),
                envelope.as_ref().map(|k| format!(" under `{k}`")).unwrap_or_default(),
                shown.len(),
                if fields > per_item && per_item > 0 { format!(" with {per_item} of {fields} fields each") } else { String::new() },
            );
            if !siblings.is_empty() {
                note.push_str(&format!(" Also: {}.", siblings.join(", ")));
            }
            // Counts over ALL items for the categorical fields, so a question
            // like "how many by priority" is answered from the whole list
            // and not invented from the part shown.
            let tallies = tally(items);
            if !tallies.is_empty() {
                note.push_str(&format!(
                    " Counts over all {} items — {}.",
                    items.len(),
                    tallies.join(" · ")
                ));
            }
            if !keys.is_empty() {
                note.push_str(&format!(" Every field an item has: {}.", keys.join(", ")));
            }
            note.push_str(" Ask the tool for fewer items or specific fields (a limit, a page, a fields argument) to see the rest — do not conclude the rest is empty, and do not count what is not shown except from the counts above.");
            format!("{note}\n{}", Value::Array(shown))
        }
        None => {
            let trimmed = trim_item(value);
            format!(
                "SHAPED RESULT — {} bytes, too large to show whole. Its fields, with long or nested values cut short: {}. Ask the tool for a narrower read to see any of them whole.\n{}",
                whole.len(),
                value.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                trimmed
            )
        }
    }
}

/// Value counts over every item for the telling fields that are
/// categorical — few distinct short values — rendered `priority: 3 ×31, 4 ×12`.
fn tally(items: &[Value]) -> Vec<String> {
    const MAX_DISTINCT: usize = 12;
    const MAX_FIELDS: usize = 6;
    let mut out = Vec::new();
    for key in TELLING_KEYS {
        if out.len() >= MAX_FIELDS {
            break;
        }
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut present = 0usize;
        for item in items {
            let Some(v) = item.as_object().and_then(|o| o.get(*key)) else {
                continue;
            };
            present += 1;
            let text = scalar_text(v);
            let label = if text.trim().is_empty() {
                "(blank)".to_owned()
            } else {
                clip_to(&text, 40)
            };
            match counts.iter_mut().find(|(l, _)| *l == label) {
                Some((_, n)) => *n += 1,
                None => counts.push((label, 1)),
            }
            if counts.len() > MAX_DISTINCT {
                break;
            }
        }
        if present < 2 || counts.len() > MAX_DISTINCT || counts.len() < 2 && present < items.len() {
            continue;
        }
        if counts.iter().any(|(l, _)| l.len() > 40) {
            continue;
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out.push(format!(
            "{key}: {}",
            counts
                .iter()
                .map(|(l, n)| format!("{l} ×{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

fn clip_to(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

fn all_keys(items: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for item in items.iter().take(20) {
        if let Some(map) = item.as_object() {
            for k in map.keys() {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    keys
}

/// One item with its most telling fields, values cut short, nested values
/// reduced to what a person would read off them.
fn trim_item(item: &Value) -> Value {
    let Some(map) = item.as_object() else {
        return Value::String(clip(&scalar_text(item)));
    };
    let mut chosen: Vec<&String> = Vec::new();
    for key in TELLING_KEYS {
        if let Some((k, v)) = map.get_key_value(*key) {
            if !is_blank(v) && chosen.len() < MAX_ITEM_KEYS {
                chosen.push(k);
            }
        }
    }
    for (k, v) in map {
        if chosen.len() >= MAX_ITEM_KEYS {
            break;
        }
        if !chosen.contains(&k) && !is_blank(v) {
            chosen.push(k);
        }
    }
    let mut out = serde_json::Map::new();
    for k in chosen {
        out.insert(k.clone(), Value::String(clip(&scalar_text(&map[k]))));
    }
    Value::Object(out)
}

fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => {
            o.is_empty() || (o.contains_key("display_value") && is_blank(&o["display_value"]))
        }
        _ => false,
    }
}

/// What a person would read off a value: a scalar as itself, a ServiceNow
/// reference by its display value, a link by its href, a collection by its size.
fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(_) | Value::Number(_) => v.to_string(),
        Value::Array(a) => format!("[{} items]", a.len()),
        Value::Object(o) => {
            for key in [
                "display_value",
                "name",
                "title",
                "value",
                "href",
                "url",
                "id",
            ] {
                if let Some(inner) = o.get(key) {
                    if !is_blank(inner) && !inner.is_object() && !inner.is_array() {
                        return scalar_text(inner);
                    }
                }
            }
            format!("{{{} fields}}", o.len())
        }
    }
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_VALUE_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_VALUE_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod shaping_tests {
    use super::*;
    use serde_json::json;

    fn incidents(n: usize) -> Value {
        let items: Vec<Value> = (0..n)
            .map(|i| {
                let mut o = serde_json::Map::new();
                o.insert("sys_id".into(), json!(format!("{i:032x}")));
                for f in 0..90 {
                    o.insert(format!("u_field_{f}"), json!("x".repeat(30)));
                }
                o.insert("number".into(), json!(format!("INC00{i:05}")));
                o.insert("priority".into(), json!("2"));
                o.insert(
                    "short_description".into(),
                    json!(format!("Printer {i} on fire")),
                );
                o.insert(
                    "assigned_to".into(),
                    json!({"display_value": "Ann", "link": "https://x/ann"}),
                );
                o.insert(
                    "assignment_group".into(),
                    json!({"display_value": "", "link": ""}),
                );
                o.insert("opened_at".into(), json!("2026-09-07 04:00:00"));
                Value::Object(o)
            })
            .collect();
        json!({"result": items})
    }

    #[test]
    fn a_small_result_is_shown_whole() {
        let v = json!({"result": [{"number": "INC1"}]});
        assert_eq!(shape_result(&v), v.to_string());
    }

    #[test]
    fn a_large_list_is_a_digest_that_keeps_the_telling_fields() {
        let v = incidents(60);
        assert!(v.to_string().len() > TOOL_RESULT_INLINE_BYTES);
        let shaped = shape_result(&v);
        assert!(
            shaped.len() <= TOOL_RESULT_INLINE_BYTES + 4096,
            "shaped is {} bytes",
            shaped.len()
        );
        assert!(shaped.starts_with("SHAPED RESULT — "));
        assert!(shaped.contains("A list of 60 items under `result`"));
        assert!(shaped.contains("do not conclude the rest is empty"));
        assert!(
            shaped.contains("INC0000000"),
            "the first item's number survives"
        );
        assert!(shaped.contains("\"priority\":\"2\""));
        assert!(
            shaped.contains("\"assigned_to\":\"Ann\""),
            "a reference reads by its display value"
        );
        assert!(
            !shaped.contains("\"assignment_group\":\""),
            "a blank reference is dropped from the items"
        );
        assert!(shaped.contains("Every field an item has: "));
        assert!(
            shaped.contains("u_field_89"),
            "every key is listed so the model can ask for it"
        );
        assert!(shaped.contains("Counts over all 60 items — "), "{shaped}");
        assert!(
            shaped.contains("priority: 2 ×60"),
            "a count over every item, not the shown ones"
        );
        assert!(shaped.contains("assigned_to: Ann ×60"));
    }

    #[test]
    fn a_large_object_is_its_fields_cut_short() {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), json!("big"));
        o.insert(
            "blob".into(),
            json!("y".repeat(TOOL_RESULT_INLINE_BYTES + 10)),
        );
        o.insert("parts".into(), json!([1, 2, 3]));
        let shaped = shape_result(&Value::Object(o));
        assert!(shaped.starts_with("SHAPED RESULT — "));
        assert!(shaped.contains("\"name\":\"big\""));
        assert!(shaped.contains("[3 items]"));
        assert!(shaped.len() < 2048);
    }

    #[test]
    fn long_text_keeps_its_head_and_says_so() {
        let text = "z".repeat(TOOL_RESULT_INLINE_BYTES + 100);
        let shaped = shape_text(&text);
        assert!(shaped.starts_with("SHAPED RESULT — "));
        assert!(shaped.ends_with(&"z".repeat(100)));
        assert_eq!(shape_text("short"), "short");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its input."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn call(&self, args: Value) -> Result<Value> {
            Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
        }
    }

    struct Fail;

    #[async_trait]
    impl Tool for Fail {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "Always fails."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Err(RustyError::Tool("boom".into()))
        }
    }

    #[test]
    fn registry_schemas_are_openai_shaped() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["type"], json!("function"));
        assert_eq!(schemas[0]["function"]["name"], json!("echo"));
        assert!(schemas[0]["function"]["parameters"]["properties"].is_object());
    }

    #[tokio::test]
    async fn batch_preserves_order_and_isolates_failures() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        registry.register(Fail);
        let executor = ToolExecutor::new(registry);

        let calls = vec![
            ToolCall::new("c1", "echo", json!({"text": "hello"})),
            ToolCall::new("c2", "fail", json!({})),
            ToolCall::new("c3", "missing", json!({})),
        ];
        let results = executor.execute_batch(&calls).await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(results[0].content.as_deref(), Some("hello"));
        assert_eq!(results[1].tool_call_id.as_deref(), Some("c2"));
        assert!(results[1].content.as_deref().unwrap().starts_with("ERROR:"));
        assert_eq!(results[2].tool_call_id.as_deref(), Some("c3"));
        assert!(results[2]
            .content
            .as_deref()
            .unwrap()
            .contains("unknown tool"));
    }

    struct Panic;

    #[tokio::test]
    async fn execute_one_returns_the_raw_result() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        registry.register(Fail);
        let executor = ToolExecutor::new(registry);

        // The singular form hands the driver the `Result` itself — no
        // string conversion, no `ERROR:` channel — so the code-mode
        // interpreter can fail or tolerate a step on it.
        let value = executor
            .execute_one(&ToolCall::new("c1", "echo", json!({"text": "hi"})))
            .await
            .unwrap();
        assert_eq!(value, json!("hi"));
        let error = executor
            .execute_one(&ToolCall::new("c2", "fail", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("boom"));
    }

    #[async_trait]
    impl Tool for Panic {
        fn name(&self) -> &str {
            "panic"
        }
        fn description(&self) -> &str {
            "Always panics."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            panic!("kaboom");
        }
    }

    #[tokio::test]
    async fn panicking_tool_is_contained_as_error_message() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        registry.register(Panic);
        let executor = ToolExecutor::new(registry);

        let calls = vec![
            ToolCall::new("c1", "panic", json!({})),
            ToolCall::new("c2", "echo", json!({"text": "still alive"})),
        ];
        let results = executor.execute_batch(&calls).await;

        // The panic joins the same ERROR: channel as ordinary failures, and
        // the rest of the batch completes normally.
        assert_eq!(results.len(), 2);
        let msg = results[0].content.as_deref().unwrap();
        assert!(msg.starts_with("ERROR:"), "got: {msg}");
        assert!(msg.contains("panicked"), "got: {msg}");
        assert!(msg.contains("kaboom"), "got: {msg}");
        assert_eq!(results[1].content.as_deref(), Some("still alive"));
    }

    // EP-05-S06 placement tests

    struct ReadTool;
    #[async_trait]
    impl Tool for ReadTool {
        fn name(&self) -> &str {
            "read_tool"
        }
        fn description(&self) -> &str {
            "A read-only tool."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    struct ExecuteTool;
    #[async_trait]
    impl Tool for ExecuteTool {
        fn name(&self) -> &str {
            "execute_tool"
        }
        fn description(&self) -> &str {
            "An execute tool."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect_class(&self) -> EffectClass {
            EffectClass::Execute
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    struct EgressTool;
    #[async_trait]
    impl Tool for EgressTool {
        fn name(&self) -> &str {
            "egress_tool"
        }
        fn description(&self) -> &str {
            "An egress tool."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect_class(&self) -> EffectClass {
            EffectClass::Egress
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    #[test]
    fn read_none_registers_and_executes() {
        let mut registry = ToolRegistry::new();
        registry.register(ReadTool);
        assert!(registry.contains("read_tool"));
    }

    #[test]
    #[should_panic(expected = "tool `execute_tool` declares an invalid placement")]
    fn execute_none_fails_at_registration() {
        let mut registry = ToolRegistry::new();
        registry.register(ExecuteTool);
    }

    #[test]
    #[should_panic(expected = "tool `egress_tool` declares an invalid placement")]
    fn egress_none_fails_at_registration() {
        let mut registry = ToolRegistry::new();
        registry.register(EgressTool);
    }

    #[tokio::test]
    async fn read_required_fails_at_dispatch_with_no_backend() {
        struct ReadRequired;
        #[async_trait]
        impl Tool for ReadRequired {
            fn name(&self) -> &str {
                "read_required"
            }
            fn description(&self) -> &str {
                "A read tool requiring sandbox."
            }
            fn parameters_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn sandbox_requirement(&self) -> SandboxRequirement {
                SandboxRequirement::Required
            }
            async fn call(&self, _args: Value) -> Result<Value> {
                Ok(Value::Null)
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(ReadRequired);
        let executor = ToolExecutor::new(registry);
        let result = executor
            .execute_one(&ToolCall::new("c1", "read_required", json!({})))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no sandbox backend is available"),
            "got: {msg}"
        );
    }
}

#[cfg(test)]
mod failure_policy_tests {
    use super::*;
    use crate::llm::ChatMessage;
    use serde_json::json;

    struct Fixed(&'static str, Effect);
    #[async_trait]
    impl Tool for Fixed {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "fixed"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect(&self) -> Effect {
            self.1
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Fixed("post", Effect::NonIdempotent));
        r.register(Fixed("read", Effect::ReadOnly));
        r
    }
    fn asked(id: &str, name: &str, args: Value) -> ChatMessage {
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(id, name, args)])
    }
    fn failed(id: &str, class: &str) -> ChatMessage {
        ChatMessage::tool_result(
            id,
            format!(
                "ERROR: {}",
                serde_json::to_string(&ToolFailure::new(class, "post", "x", true, false, "y"))
                    .unwrap()
            ),
        )
    }

    #[test]
    fn the_same_call_that_failed_its_bound_is_refused() {
        let r = registry();
        let call = ToolCall::new("c3", "read", json!({"table": "incdent"}));
        let mut history = vec![
            asked("c1", "read", json!({"table": "incdent"})),
            failed("c1", "invalid_arguments"),
        ];
        assert!(
            failure_policy(&history, &call, &r).is_none(),
            "one failure: try again"
        );
        history.push(asked("c2", "read", json!({"table": "incdent"})));
        history.push(failed("c2", "invalid_arguments"));
        let refusal = failure_policy(&history, &call, &r).expect("bounded");
        assert_eq!(refusal.class, "bounded");
        // Different arguments are a different call.
        assert!(failure_policy(
            &history,
            &ToolCall::new("c4", "read", json!({"table": "incident"})),
            &r
        )
        .is_none());
    }

    #[test]
    fn a_write_whose_answer_was_lost_is_not_sent_again_until_something_was_read() {
        let r = registry();
        let again = ToolCall::new("c2", "post", json!({"text": "hello"}));
        let mut history = vec![
            asked("c1", "post", json!({"text": "hello"})),
            failed("c1", "unknown_outcome"),
        ];
        let refusal = failure_policy(&history, &again, &r).expect("reconcile first");
        assert_eq!(refusal.class, "reconcile_first");
        // A read that answered clears the way.
        history.push(asked("r1", "read", json!({})));
        history.push(ChatMessage::tool_result("r1", "{\"posts\": []}"));
        assert!(failure_policy(&history, &again, &r).is_none());
        // A read-only call is never held back by the rule.
        let mut only_read = vec![
            asked("c1", "read", json!({})),
            failed("c1", "unknown_outcome"),
        ];
        assert!(failure_policy(&only_read, &ToolCall::new("c9", "read", json!({})), &r).is_none());
        only_read.clear();
    }

    #[test]
    fn after_a_read_back_found_nothing_one_identical_re_send_is_allowed_and_only_one() {
        let r = registry();
        let again = ToolCall::new("c2", "post", json!({"text": "hello"}));
        let lost_but_checked = |id: &str| {
            ChatMessage::tool_result(
                id,
                format!(
                    "ERROR: {}",
                    serde_json::to_string(&ToolFailure::new(
                        "unknown_outcome",
                        "post",
                        "sent; the read-back found no record",
                        true,
                        true,
                        "send it once more"
                    ))
                    .unwrap()
                ),
            )
        };
        let mut history = vec![
            asked("c1", "post", json!({"text": "hello"})),
            lost_but_checked("c1"),
        ];
        assert!(
            failure_policy(&history, &again, &r).is_none(),
            "the read-back found nothing: one re-send is safe"
        );
        // The re-send is made and its answer is lost again: the tool's own
        // read-back runs again; a second identical failure without a
        // read-back is refused as before.
        history.push(asked("c2", "post", json!({"text": "hello"})));
        history.push(failed("c2", "unknown_outcome"));
        let refusal = failure_policy(
            &history,
            &ToolCall::new("c3", "post", json!({"text": "hello"})),
            &r,
        )
        .expect("refused");
        // Two real failures of one call are also the bound; either way the
        // third identical write is not sent.
        assert!(
            matches!(refusal.class.as_str(), "reconcile_first" | "bounded"),
            "{refusal:?}"
        );
    }
}
