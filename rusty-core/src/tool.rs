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
use crate::llm::{ChatMessage, ToolCall};
use crate::middleware::{MiddlewareChain, ToolInvocation};

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
}

impl ToolCapability {
    fn from_tool(tool: &dyn Tool) -> Result<Self> {
        let name = tool.name();
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
        let description = tool.description();
        if description.is_empty()
            || description != description.trim()
            || description.len() > MAX_TOOL_DESCRIPTION_BYTES
            || description.chars().any(char::is_control)
        {
            return Err(RustyError::Tool(format!(
                "tool `{name}` description must be non-empty, trimmed, control-free, and at most {MAX_TOOL_DESCRIPTION_BYTES} bytes"
            )));
        }
        let parameters_schema = tool.parameters_schema();
        if !parameters_schema.is_object() {
            return Err(RustyError::Tool(format!(
                "tool `{name}` parameters schema must be a JSON object"
            )));
        }
        let schema_bytes = serde_json::to_vec(&parameters_schema).map_err(|error| {
            RustyError::Tool(format!(
                "tool `{name}` parameters schema did not serialize: {error}"
            ))
        })?;
        if schema_bytes.len() > MAX_TOOL_SCHEMA_BYTES {
            return Err(RustyError::Tool(format!(
                "tool `{name}` parameters schema exceeds {MAX_TOOL_SCHEMA_BYTES} bytes"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters_schema,
            effect: tool.effect(),
        })
    }
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

    /// Stable effect kind used for deterministic effect ids and compensation
    /// lookup. Defaults to the tool name.
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

/// A registry of tools, shared cheaply via `Arc<dyn Tool>`.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
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
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
        self
    }

    /// Register a pre-shared tool.
    pub fn register_shared(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.name().to_owned(), tool);
        self
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// `true` if a tool with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// All registered tool names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// `true` if no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// OpenAI-format tool schemas for the chat API, one per registered tool:
    /// `{"type": "function", "function": {"name", "description", "parameters"}}`.
    /// Pass directly as the `tools` argument of
    /// [`crate::llm::ChatModel::chat`].
    pub fn schemas(&self) -> Vec<Value> {
        self.tools
            .values()
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
            .tools
            .values()
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
#[derive(Debug, Clone, Default)]
pub struct ToolExecutor {
    registry: ToolRegistry,
    middleware: MiddlewareChain,
    effect_admission: Option<EffectAdmissionContext>,
    thread_id: String,
    node: String,
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

    /// The attached middleware chain (empty when none was added).
    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    /// The attached effect boundary, if enforcement is enabled.
    pub fn effect_admission(&self) -> Option<&EffectAdmissionContext> {
        self.effect_admission.as_ref()
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
        let futures = calls.iter().map(|call| {
            let registry = self.registry.clone();
            let chain = self.middleware.clone();
            let effect_admission = self.effect_admission.clone();
            let thread_id = self.thread_id.clone();
            let node = self.node.clone();
            async move {
                let result = std::panic::AssertUnwindSafe(async {
                    let value = if chain.is_empty() {
                        dispatch_tool(&registry, call, effect_admission.as_ref()).await?
                    } else {
                        let mut invocation = ToolInvocation::new(thread_id, node, call.clone());
                        chain
                            .run_tool(&mut invocation, |invocation| {
                                let registry = registry.clone();
                                let effect_admission = effect_admission.clone();
                                let call = invocation.call().clone();
                                async move {
                                    // The lookup happens after before-hooks,
                                    // so a layer may rewrite the arguments —
                                    // or the target tool name itself.
                                    dispatch_tool(&registry, &call, effect_admission.as_ref()).await
                                }
                            })
                            .await?
                    };
                    Ok::<String, RustyError>(match value {
                        Value::String(s) => s,
                        other => other.to_string(),
                    })
                })
                .catch_unwind()
                .await;
                match result {
                    Ok(Ok(content)) => ChatMessage::tool_result(&call.id, content),
                    Ok(Err(e)) => ChatMessage::tool_result(&call.id, format!("ERROR: {e}")),
                    Err(payload) => ChatMessage::tool_result(
                        &call.id,
                        format!(
                            "ERROR: tool `{}` panicked: {}",
                            call.name,
                            // `&*`: `&payload` would unsize-coerce the *Box*
                            // itself into `&dyn Any`, hiding the real payload.
                            panic_message(&*payload)
                        ),
                    ),
                }
            }
        });
        futures::future::join_all(futures).await
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
async fn dispatch_tool(
    registry: &ToolRegistry,
    call: &ToolCall,
    effect_admission: Option<&EffectAdmissionContext>,
) -> Result<Value> {
    let tool = registry
        .get(&call.name)
        .ok_or_else(|| RustyError::Tool(format!("unknown tool `{}`", call.name)))?;
    if let Some(context) = effect_admission {
        let request = tool.effect_request(call);
        if let Err(violation) = context.admit(&request) {
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
    tool.call(call.arguments.clone()).await
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
}
