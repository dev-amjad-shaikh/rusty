//! Middleware / Interceptor SDK: ordered layers around execution.
//!
//! A [`Middleware`] is an async, object-safe interception layer with six
//! hooks — before/after at three points: **node run** ([`NodeCall`]),
//! **model call** ([`ModelCall`]), and **tool call** ([`ToolInvocation`]).
//! Every hook returns a [`Decision`]: continue (with any mutations applied
//! to the context in place), reject with a structured [`Rejection`], or
//! short-circuit with a substitute result.
//!
//! Layers compose into a [`MiddlewareChain`] with tower-style onion
//! semantics:
//!
//! - before-hooks run in **registration order** on the way in, after-hooks
//!   in **reverse order** on the way out;
//! - a before-hook short-circuit at layer *i* skips the remaining
//!   before-hooks and the operation itself; the after-hooks of the layers
//!   that already entered (`0..i`) unwind over the substitute result;
//! - a rejection is terminal: no after-hooks run, and the rejection surfaces
//!   through the crate's existing error taxonomy — node-run rejections as
//!   [`RustyError::Node`], model rejections as [`RustyError::Llm`], tool
//!   rejections as [`RustyError::Tool`] (which the
//!   [`crate::tool::ToolExecutor`] renders into an `ERROR:` tool message the
//!   model can observe). The message is the [`Rejection`]'s canonical
//!   [`Display`](std::fmt::Display) form; the typed `Rejection` itself is
//!   the contract at the middleware API. Never a panic;
//! - after-hooks run on the success path only; operation errors propagate
//!   untouched. An after-hook short-circuit replaces the result and skips
//!   the remaining (outer) after-hooks.
//!
//! Wiring:
//!
//! - **Node runs** — attach layers to the [`crate::executor::Executor`] with
//!   `.layer(...)`; the chain wraps every node invocation at the super-step
//!   boundary. The same chain reaches node code through
//!   [`crate::node::NodeContext::middleware`].
//! - **Tool calls** — hand the chain to
//!   [`crate::tool::ToolExecutor::with_middleware`]; every dispatched call is
//!   wrapped. A tool-level rejection follows the executor's
//!   failure-isolation contract and becomes an `ERROR:` tool message the
//!   model can observe and recover from.
//! - **Model calls** — wrap a [`crate::llm::ChatModel`] in
//!   [`MiddlewareChatModel`]; every `chat` and `chat_stream` is intercepted
//!   (streaming forwards token deltas live and runs the hooks around the
//!   stream).
//!
//! Two reference layers ship here: [`RequestLogger`] (tracing-based
//! observation) and [`ToolCallBlocklist`] (reject-by-policy).

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::llm::{ChatMessage, ChatModel, ChatResponse, TokenChunk, ToolCall};
use crate::node::NodeOutput;
use crate::state::State;

/// The interception point a [`Rejection`] originated at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterceptPoint {
    /// A node run at the super-step boundary.
    NodeRun,
    /// A [`crate::llm::ChatModel`] call.
    ModelCall,
    /// A tool call dispatched through [`crate::tool::ToolExecutor`].
    ToolCall,
}

impl std::fmt::Display for InterceptPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            InterceptPoint::NodeRun => "node_run",
            InterceptPoint::ModelCall => "model_call",
            InterceptPoint::ToolCall => "tool_call",
        };
        f.write_str(s)
    }
}

/// A structured middleware rejection: which layer, at which point, and why.
///
/// The typed contract at the middleware API. When a rejection leaves the
/// chain it maps onto the crate's string-payload error taxonomy by
/// interception point (node run → [`RustyError::Node`], model call →
/// [`RustyError::Llm`], tool call → [`RustyError::Tool`]), with this
/// struct's canonical [`Display`](std::fmt::Display) as the message —
/// keeping `RustyError` additive-free for downstream exhaustive matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    /// The rejecting layer ([`Middleware::name`]).
    pub middleware: String,
    /// The interception point that rejected.
    pub point: InterceptPoint,
    /// Machine-readable reason code (e.g. `"tool_blocked"`).
    pub reason: String,
    /// Optional human-readable detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Rejection {
    /// A rejection by `middleware` at `point` with reason code `reason`.
    pub fn new(
        middleware: impl Into<String>,
        point: InterceptPoint,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            middleware: middleware.into(),
            point,
            reason: reason.into(),
            detail: None,
        }
    }

    /// Builder-style: attach human-readable detail.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rejected by middleware `{}` at {}: {}",
            self.middleware, self.point, self.reason
        )
    }
}

impl Rejection {
    /// Map onto the crate's error taxonomy by interception point; see the
    /// struct docs.
    pub(crate) fn into_error(self) -> RustyError {
        let message = self.to_string();
        match self.point {
            InterceptPoint::NodeRun => RustyError::Node(message),
            InterceptPoint::ModelCall => RustyError::Llm(message),
            InterceptPoint::ToolCall => RustyError::Tool(message),
        }
    }
}

/// The verdict a middleware hook returns.
///
/// `R` is the result type of the intercepted operation: [`NodeOutput`] for
/// node runs, [`ChatResponse`] for model calls, [`Value`] for tool calls.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision<R> {
    /// Continue through the chain (any mutations applied to the context in
    /// place propagate to the next layer and the operation).
    Continue,
    /// Stop the chain and fail the operation with a structured reason.
    Reject(Rejection),
    /// Before-hook: skip the remaining before-hooks and the operation
    /// itself; `R` stands in for its result while the entered layers unwind.
    /// After-hook: replace the result with `R` and skip the remaining
    /// (outer) after-hooks.
    ShortCircuit(R),
}

/// Node-run interception context: the invocation a node is about to run with.
///
/// The `state` snapshot is the payload before-hooks may mutate; mutations
/// propagate into the [`crate::node::NodeContext`] the node receives.
#[derive(Debug, Clone)]
pub struct NodeCall {
    thread_id: String,
    node: String,
    step: usize,
    state: State,
}

impl NodeCall {
    /// A context for `node` at super-step `step` of thread `thread_id`.
    pub fn new(
        thread_id: impl Into<String>,
        node: impl Into<String>,
        step: usize,
        state: State,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            node: node.into(),
            step,
            state,
        }
    }

    /// The thread (run) this invocation belongs to.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The node about to run.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// The current super-step index.
    pub fn step(&self) -> usize {
        self.step
    }

    /// The invocation's state snapshot.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Mutable access to the snapshot the node will receive.
    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// Convenience: insert a channel into the snapshot.
    pub fn insert(&mut self, channel: impl Into<String>, value: Value) {
        self.state.insert(channel, value);
    }
}

/// Model-call interception context: the request a [`ChatModel`] is about to
/// receive. Mutations to the messages or tool schemas propagate to the model.
#[derive(Debug, Clone)]
pub struct ModelCall {
    thread_id: String,
    node: String,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
}

impl ModelCall {
    /// A context for a model call with `messages` and tool schemas `tools`.
    pub fn new(
        thread_id: impl Into<String>,
        node: impl Into<String>,
        messages: Vec<ChatMessage>,
        tools: Vec<Value>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            node: node.into(),
            messages,
            tools,
        }
    }

    /// The thread (run) this call belongs to, or empty when unknown.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The node making the call, or empty when unknown.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// The conversation about to be sent.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Mutable access to the conversation (inject, redact, rewrite).
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.messages
    }

    /// The OpenAI-format tool schemas about to be sent.
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// Mutable access to the tool schemas.
    pub fn tools_mut(&mut self) -> &mut Vec<Value> {
        &mut self.tools
    }
}

/// Tool-call interception context: one tool call about to be dispatched.
///
/// The registry lookup happens **after** before-hooks, so a layer may
/// rewrite the arguments — or the target tool name itself.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    thread_id: String,
    node: String,
    call: ToolCall,
}

impl ToolInvocation {
    /// A context wrapping one [`ToolCall`].
    pub fn new(thread_id: impl Into<String>, node: impl Into<String>, call: ToolCall) -> Self {
        Self {
            thread_id: thread_id.into(),
            node: node.into(),
            call,
        }
    }

    /// The thread (run) this call belongs to, or empty when unknown.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The node dispatching the call, or empty when unknown.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// The tool call about to be dispatched.
    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    /// Mutable access to the tool call (id, name, arguments).
    pub fn call_mut(&mut self) -> &mut ToolCall {
        &mut self.call
    }

    /// The provider-assigned call id.
    pub fn id(&self) -> &str {
        &self.call.id
    }

    /// The target tool name.
    pub fn name(&self) -> &str {
        &self.call.name
    }

    /// The model-supplied arguments.
    pub fn arguments(&self) -> &Value {
        &self.call.arguments
    }

    /// Convenience: replace the arguments.
    pub fn set_arguments(&mut self, arguments: Value) {
        self.call.arguments = arguments;
    }
}

/// The interception layer: async, object-safe hooks around execution.
///
/// Implement any subset of hooks; every default passes through unchanged.
/// `Send + Sync` because layers are shared across concurrent node
/// invocations and tool calls within a run.
#[async_trait]
pub trait Middleware: Send + Sync {
    /// Human/log-friendly name, recorded on rejections.
    fn name(&self) -> &str;

    /// Node run, inbound (registration order).
    async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
        let _ = call;
        Decision::Continue
    }

    /// Node run, outbound (reverse registration order). Success path only.
    async fn after_node(&self, call: &NodeCall, output: &mut NodeOutput) -> Decision<NodeOutput> {
        let _ = (call, output);
        Decision::Continue
    }

    /// Model call, inbound (registration order).
    async fn before_model(&self, call: &mut ModelCall) -> Decision<ChatResponse> {
        let _ = call;
        Decision::Continue
    }

    /// Model call, outbound (reverse registration order). Success path only.
    async fn after_model(
        &self,
        call: &ModelCall,
        response: &mut ChatResponse,
    ) -> Decision<ChatResponse> {
        let _ = (call, response);
        Decision::Continue
    }

    /// Tool call, inbound (registration order).
    async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
        let _ = call;
        Decision::Continue
    }

    /// Tool call, outbound (reverse registration order). Success path only.
    async fn after_tool(&self, call: &ToolInvocation, result: &mut Value) -> Decision<Value> {
        let _ = (call, result);
        Decision::Continue
    }
}

/// An ordered stack of middleware layers with onion semantics.
///
/// Cheap to clone (`Arc` layers); empty by default, and every consumer
/// (executor, tool executor, model wrapper) takes its original code path
/// when the chain is empty.
#[derive(Clone, Default)]
pub struct MiddlewareChain {
    layers: Vec<Arc<dyn Middleware>>,
}

impl std::fmt::Debug for MiddlewareChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareChain")
            .field(
                "layers",
                &self.layers.iter().map(|l| l.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl MiddlewareChain {
    /// An empty chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style: append a layer (runs after the already-added layers on
    /// the way in, before them on the way out).
    pub fn layer<M: Middleware + 'static>(mut self, middleware: M) -> Self {
        self.push(Arc::new(middleware));
        self
    }

    /// Append a pre-shared layer.
    pub fn push(&mut self, middleware: Arc<dyn Middleware>) -> &mut Self {
        self.layers.push(middleware);
        self
    }

    /// `true` if no layers are attached.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Number of attached layers.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// The layer names, in registration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.layers.iter().map(|l| l.name())
    }

    /// The layers, in registration order, as shared handles — the form
    /// the executor's `layer_shared` wiring takes them in when a resolved
    /// composition attaches to a run (R0.11 wave 4).
    pub fn layers(&self) -> &[Arc<dyn Middleware>] {
        &self.layers
    }

    /// Run the onion around a node invocation: before-hooks inward, `op`,
    /// after-hooks outward. See the module docs for the full contract.
    pub async fn run_node<F, Fut>(&self, call: &mut NodeCall, op: F) -> Result<NodeOutput>
    where
        F: FnOnce(&NodeCall) -> Fut + Send,
        Fut: Future<Output = Result<NodeOutput>> + Send,
    {
        let mut entered = 0;
        let mut substitute = None;
        for layer in &self.layers {
            match layer.before_node(call).await {
                Decision::Continue => entered += 1,
                Decision::Reject(rejection) => return Err(rejection.into_error()),
                Decision::ShortCircuit(output) => {
                    substitute = Some(output);
                    break;
                }
            }
        }
        let mut result = match substitute {
            Some(output) => output,
            None => {
                entered = self.layers.len();
                op(call).await?
            }
        };
        for layer in self.layers[..entered].iter().rev() {
            match layer.after_node(call, &mut result).await {
                Decision::Continue => {}
                Decision::Reject(rejection) => return Err(rejection.into_error()),
                Decision::ShortCircuit(output) => {
                    result = output;
                    break;
                }
            }
        }
        Ok(result)
    }

    /// Run the onion around a model call. Same contract as
    /// [`MiddlewareChain::run_node`].
    pub async fn run_model<F, Fut>(&self, call: &mut ModelCall, op: F) -> Result<ChatResponse>
    where
        F: FnOnce(&ModelCall) -> Fut + Send,
        Fut: Future<Output = Result<ChatResponse>> + Send,
    {
        let mut entered = 0;
        let mut substitute = None;
        for layer in &self.layers {
            match layer.before_model(call).await {
                Decision::Continue => entered += 1,
                Decision::Reject(rejection) => return Err(rejection.into_error()),
                Decision::ShortCircuit(response) => {
                    substitute = Some(response);
                    break;
                }
            }
        }
        let mut result = match substitute {
            Some(response) => response,
            None => {
                entered = self.layers.len();
                op(call).await?
            }
        };
        for layer in self.layers[..entered].iter().rev() {
            match layer.after_model(call, &mut result).await {
                Decision::Continue => {}
                Decision::Reject(rejection) => return Err(rejection.into_error()),
                Decision::ShortCircuit(response) => {
                    result = response;
                    break;
                }
            }
        }
        Ok(result)
    }

    /// Run the onion around a tool call. Same contract as
    /// [`MiddlewareChain::run_node`].
    pub async fn run_tool<F, Fut>(&self, call: &mut ToolInvocation, op: F) -> Result<Value>
    where
        F: FnOnce(&ToolInvocation) -> Fut + Send,
        Fut: Future<Output = Result<Value>> + Send,
    {
        let mut entered = 0;
        let mut substitute = None;
        for layer in &self.layers {
            match layer.before_tool(call).await {
                Decision::Continue => entered += 1,
                Decision::Reject(rejection) => return Err(rejection.into_error()),
                Decision::ShortCircuit(value) => {
                    substitute = Some(value);
                    break;
                }
            }
        }
        let mut result = match substitute {
            Some(value) => value,
            None => {
                entered = self.layers.len();
                op(call).await?
            }
        };
        for layer in self.layers[..entered].iter().rev() {
            match layer.after_tool(call, &mut result).await {
                Decision::Continue => {}
                Decision::Reject(rejection) => return Err(rejection.into_error()),
                Decision::ShortCircuit(value) => {
                    result = value;
                    break;
                }
            }
        }
        Ok(result)
    }
}

/// A [`ChatModel`] wrapper that runs every call through a
/// [`MiddlewareChain`]'s model hooks.
///
/// Construct per node (or per invocation) — typically with the chain from
/// [`crate::node::NodeContext::middleware`]. The optional `thread`/`node`
/// labels flow into the [`ModelCall`] context.
///
/// `chat_stream` is intercepted with the same onion as `chat`: before-hooks
/// run (and may reject) before the provider is called, the caller's token
/// callback forwards straight to the inner model's stream, and after-hooks
/// run on the accumulated final [`ChatResponse`]. Token deltas themselves
/// are not intercepted — interception is per-response, not per-token.
#[derive(Clone)]
pub struct MiddlewareChatModel {
    inner: Arc<dyn ChatModel>,
    chain: MiddlewareChain,
    thread_id: String,
    node: String,
}

impl MiddlewareChatModel {
    /// A middleware wrapper around `inner`, intercepting through `chain`.
    pub fn new(inner: Arc<dyn ChatModel>, chain: MiddlewareChain) -> Self {
        Self {
            inner,
            chain,
            thread_id: String::new(),
            node: String::new(),
        }
    }

    /// Builder-style: the thread (run) label for the [`ModelCall`] context.
    pub fn thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = thread_id.into();
        self
    }

    /// Builder-style: the node label for the [`ModelCall`] context.
    pub fn node(mut self, node: impl Into<String>) -> Self {
        self.node = node.into();
        self
    }
}

#[async_trait]
impl ChatModel for MiddlewareChatModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        if self.chain.is_empty() {
            return self.inner.chat(messages, tools).await;
        }
        let mut call = ModelCall::new(
            self.thread_id.clone(),
            self.node.clone(),
            messages.to_vec(),
            tools.to_vec(),
        );
        self.chain
            .run_model(&mut call, |call| {
                let inner = Arc::clone(&self.inner);
                let messages = call.messages().to_vec();
                let tools = call.tools().to_vec();
                async move { inner.chat(&messages, &tools).await }
            })
            .await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> Result<ChatResponse> {
        if self.chain.is_empty() {
            return self.inner.chat_stream(messages, tools, on_token).await;
        }
        let mut call = ModelCall::new(
            self.thread_id.clone(),
            self.node.clone(),
            messages.to_vec(),
            tools.to_vec(),
        );
        self.chain
            .run_model(&mut call, move |call| {
                let inner = Arc::clone(&self.inner);
                let messages = call.messages().to_vec();
                let tools = call.tools().to_vec();
                // The caller's callback passes straight through: deltas are
                // delivered live, and the after-hooks below see the
                // accumulated response the stream produced.
                async move { inner.chat_stream(&messages, &tools, on_token).await }
            })
            .await
    }

    fn effect(&self) -> crate::record::Effect {
        self.inner.effect()
    }

    fn pricing(&self) -> Option<crate::llm::ModelPricing> {
        self.inner.pricing()
    }
}

/// Tracing-based observation middleware: emits an INFO event at every
/// interception point and passes everything through unchanged.
///
/// Payloads are deliberately not logged — counts and ids only. Logging full
/// model or tool payloads is a data-leak footgun; compose a redaction layer
/// first if you need them.
#[derive(Debug, Default)]
pub struct RequestLogger;

impl RequestLogger {
    /// A logger layer.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Middleware for RequestLogger {
    fn name(&self) -> &str {
        "request_logger"
    }

    async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
        tracing::info!(
            thread_id = %call.thread_id(),
            node = %call.node(),
            step = call.step(),
            "middleware: node run started"
        );
        Decision::Continue
    }

    async fn after_node(&self, call: &NodeCall, output: &mut NodeOutput) -> Decision<NodeOutput> {
        tracing::info!(
            thread_id = %call.thread_id(),
            node = %call.node(),
            step = call.step(),
            channels = output.updates.len(),
            "middleware: node run completed"
        );
        Decision::Continue
    }

    async fn before_model(&self, call: &mut ModelCall) -> Decision<ChatResponse> {
        tracing::info!(
            thread_id = %call.thread_id(),
            node = %call.node(),
            messages = call.messages().len(),
            tools = call.tools().len(),
            "middleware: model call started"
        );
        Decision::Continue
    }

    async fn after_model(
        &self,
        call: &ModelCall,
        response: &mut ChatResponse,
    ) -> Decision<ChatResponse> {
        tracing::info!(
            thread_id = %call.thread_id(),
            node = %call.node(),
            total_tokens = response.usage.map(|u| u.total_tokens),
            "middleware: model call completed"
        );
        Decision::Continue
    }

    async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
        tracing::info!(
            thread_id = %call.thread_id(),
            node = %call.node(),
            tool = %call.name(),
            call_id = %call.id(),
            "middleware: tool call started"
        );
        Decision::Continue
    }

    async fn after_tool(&self, call: &ToolInvocation, result: &mut Value) -> Decision<Value> {
        let _ = result;
        tracing::info!(
            thread_id = %call.thread_id(),
            node = %call.node(),
            tool = %call.name(),
            call_id = %call.id(),
            "middleware: tool call completed"
        );
        Decision::Continue
    }
}

/// Reject-by-policy middleware: tool calls to blocklisted tools never
/// execute; each is rejected with a structured [`Rejection`] (reason
/// `"tool_blocked"` by default) that surfaces as an `ERROR:` tool message
/// under the [`crate::tool::ToolExecutor`] failure-isolation contract.
#[derive(Debug, Clone)]
pub struct ToolCallBlocklist {
    blocked: HashSet<String>,
    reason: String,
}

impl ToolCallBlocklist {
    /// Block the named tools with the default reason code.
    pub fn new<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            blocked: tools.into_iter().map(Into::into).collect(),
            reason: "tool_blocked".to_owned(),
        }
    }

    /// Builder-style: override the rejection reason code.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    /// `true` if calls to `tool` are rejected.
    pub fn contains(&self, tool: &str) -> bool {
        self.blocked.contains(tool)
    }
}

#[async_trait]
impl Middleware for ToolCallBlocklist {
    fn name(&self) -> &str {
        "tool_call_blocklist"
    }

    async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
        if self.blocked.contains(call.name()) {
            Decision::Reject(
                Rejection::new(self.name(), InterceptPoint::ToolCall, self.reason.clone())
                    .with_detail(format!("tool `{}` is blocked by policy", call.name())),
            )
        } else {
            Decision::Continue
        }
    }
}

/// Instantiate a journaled [`crate::learn::MiddlewareLayerConfig`] composition
/// into a live [`MiddlewareChain`].
///
/// The composition is evidence: a run pins its digest in the manifest and the
/// resolved layer order in the journal, so instantiation must be a pure,
/// deterministic function of the journaled value against the **compiled-in
/// layer vocabulary** — the same names [`MiddlewareChain::names`] reports.
/// Two layers are in the vocabulary today:
///
/// - `request_logger` — zero-config; a `config` payload is refused (a config
///   the layer ignores would be evidence the chain cannot honor);
/// - `tool_call_blocklist` — requires `{"blocked": ["tool.name", ...],
///   "reason": "..."?}`; `reason` defaults to the layer's own reason code.
///
/// An unknown layer name, a config on a config-free layer, or a malformed
/// config is an error naming the vocabulary — the set NEVER grows by
/// accepting unknown names at resolution time (R0.11 wave 4, closed-set
/// extension; design doc §Middleware).
pub fn instantiate_composition(
    layers: &[crate::learn::MiddlewareLayerConfig],
) -> Result<MiddlewareChain> {
    /// The `tool_call_blocklist` layer's config payload. Unknown keys are
    /// ignored (forward-compatible); the two governed keys are pinned by
    /// golden evidence.
    #[derive(Deserialize)]
    struct BlocklistConfig {
        blocked: Vec<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    let mut chain = MiddlewareChain::new();
    for entry in layers {
        match entry.layer.as_str() {
            "request_logger" => {
                if entry.config.is_some() {
                    return Err(RustyError::Graph(
                        "middleware layer `request_logger` takes no config payload".to_owned(),
                    ));
                }
                chain.push(Arc::new(RequestLogger::new()));
            }
            "tool_call_blocklist" => {
                let config = entry.config.as_ref().ok_or_else(|| {
                    RustyError::Graph(
                        "middleware layer `tool_call_blocklist` requires a config payload: \
                         {\"blocked\": [\"tool.name\"], \"reason\"?}"
                            .to_owned(),
                    )
                })?;
                let parsed: BlocklistConfig =
                    serde_json::from_value(config.clone()).map_err(|e| {
                        RustyError::Graph(format!(
                            "middleware layer `tool_call_blocklist` config is malformed: {e}"
                        ))
                    })?;
                let mut layer = ToolCallBlocklist::new(parsed.blocked);
                if let Some(reason) = parsed.reason {
                    layer = layer.with_reason(reason);
                }
                chain.push(Arc::new(layer));
            }
            other => {
                return Err(RustyError::Graph(format!(
                    "unknown middleware layer `{other}`: the compiled-in vocabulary is \
                     [request_logger, tool_call_blocklist]"
                )));
            }
        }
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::MiddlewareLayerConfig;
    use serde_json::json;
    use std::sync::Mutex;

    /// Shared, thread-safe record of hook invocations, for order proofs.
    #[derive(Clone, Default)]
    struct Trace(Arc<Mutex<Vec<String>>>);

    impl Trace {
        fn record(&self, entry: impl Into<String>) {
            self.0.lock().expect("trace lock").push(entry.into());
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().expect("trace lock").clone()
        }
    }

    /// Records before/after node hooks as `Lx:before|after:<node>`.
    struct Recorder {
        id: &'static str,
        trace: Trace,
    }

    #[async_trait]
    impl Middleware for Recorder {
        fn name(&self) -> &str {
            self.id
        }

        async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
            self.trace
                .record(format!("{}:before:{}", self.id, call.node()));
            Decision::Continue
        }

        async fn after_node(
            &self,
            call: &NodeCall,
            _output: &mut NodeOutput,
        ) -> Decision<NodeOutput> {
            self.trace
                .record(format!("{}:after:{}", self.id, call.node()));
            Decision::Continue
        }
    }

    #[tokio::test]
    async fn empty_chain_passes_through() {
        let chain = MiddlewareChain::new();
        assert!(chain.is_empty());
        let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
        let result = chain
            .run_node(&mut call, |_call| async {
                Ok(NodeOutput::update("x", json!(1)))
            })
            .await
            .unwrap();
        assert_eq!(result.updates.get("x"), Some(&json!(1)));
    }

    #[tokio::test]
    async fn before_hooks_run_in_registration_order_after_hooks_in_reverse() {
        let trace = Trace::default();
        let chain = MiddlewareChain::new()
            .layer(Recorder {
                id: "L1",
                trace: trace.clone(),
            })
            .layer(Recorder {
                id: "L2",
                trace: trace.clone(),
            })
            .layer(Recorder {
                id: "L3",
                trace: trace.clone(),
            });
        assert_eq!(chain.len(), 3);
        assert_eq!(chain.names().collect::<Vec<_>>(), ["L1", "L2", "L3"]);

        let op_trace = trace.clone();
        let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
        let result = chain
            .run_node(&mut call, |_call| {
                let op_trace = op_trace.clone();
                async move {
                    op_trace.record("op");
                    Ok(NodeOutput::update("x", json!(1)))
                }
            })
            .await
            .unwrap();

        assert_eq!(result.updates.get("x"), Some(&json!(1)));
        assert_eq!(
            trace.entries(),
            vec![
                "L1:before:node-a",
                "L2:before:node-a",
                "L3:before:node-a",
                "op",
                "L3:after:node-a",
                "L2:after:node-a",
                "L1:after:node-a",
            ]
        );
    }

    /// Injects state inbound, rewrites the output outbound.
    struct Mutate;

    #[async_trait]
    impl Middleware for Mutate {
        fn name(&self) -> &str {
            "mutate"
        }

        async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
            call.insert("injected", json!(true));
            Decision::Continue
        }

        async fn after_node(
            &self,
            _call: &NodeCall,
            output: &mut NodeOutput,
        ) -> Decision<NodeOutput> {
            output.updates.insert("x".into(), json!("rewritten"));
            Decision::Continue
        }
    }

    #[tokio::test]
    async fn mutations_propagate_inward_and_outward() {
        let chain = MiddlewareChain::new().layer(Mutate);
        let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
        let result = chain
            .run_node(&mut call, |call| {
                // The before-hook's state mutation is visible to the node.
                assert_eq!(call.state().get("injected"), Some(&json!(true)));
                async move { Ok(NodeOutput::update("x", json!("original"))) }
            })
            .await
            .unwrap();
        // The after-hook's rewrite is visible to the caller.
        assert_eq!(result.updates.get("x"), Some(&json!("rewritten")));
    }

    struct PolicyReject {
        reason: &'static str,
    }

    #[async_trait]
    impl Middleware for PolicyReject {
        fn name(&self) -> &str {
            "policy_reject"
        }

        async fn before_node(&self, _call: &mut NodeCall) -> Decision<NodeOutput> {
            Decision::Reject(
                Rejection::new(self.name(), InterceptPoint::NodeRun, self.reason)
                    .with_detail("denied by test policy"),
            )
        }
    }

    #[tokio::test]
    async fn reject_skips_operation_and_all_after_hooks() {
        let trace = Trace::default();
        let chain = MiddlewareChain::new()
            .layer(Recorder {
                id: "L1",
                trace: trace.clone(),
            })
            .layer(PolicyReject { reason: "policy" })
            .layer(Recorder {
                id: "L3",
                trace: trace.clone(),
            });

        let op_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let op_flag = op_ran.clone();
        let mut call = NodeCall::new("t-1", "node-b", 2, State::new());
        let err = chain
            .run_node(&mut call, |_call| {
                let op_flag = op_flag.clone();
                async move {
                    op_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(NodeOutput::empty())
                }
            })
            .await
            .unwrap_err();

        match err {
            RustyError::Node(message) => {
                // Node-run rejections map onto RustyError::Node carrying the
                // Rejection's canonical Display form.
                assert!(
                    message.contains("rejected by middleware `policy_reject` at node_run: policy"),
                    "got: {message}"
                );
            }
            other => panic!("expected RustyError::Node, got {other:?}"),
        }
        assert!(!op_ran.load(std::sync::atomic::Ordering::SeqCst));
        // L1 entered (before ran), L3 never ran at all, and no after-hooks
        // unwind a rejection.
        assert_eq!(trace.entries(), vec!["L1:before:node-b"]);
    }

    /// Short-circuits node `victim` with a substitute output.
    struct Skip;

    #[async_trait]
    impl Middleware for Skip {
        fn name(&self) -> &str {
            "skip"
        }

        async fn before_node(&self, call: &mut NodeCall) -> Decision<NodeOutput> {
            if call.node() == "victim" {
                Decision::ShortCircuit(NodeOutput::update("sub", json!("substitute")))
            } else {
                Decision::Continue
            }
        }
    }

    #[tokio::test]
    async fn short_circuit_returns_substitute_and_unwinds_entered_layers() {
        let trace = Trace::default();
        let chain = MiddlewareChain::new()
            .layer(Recorder {
                id: "L1",
                trace: trace.clone(),
            })
            .layer(Skip)
            .layer(Recorder {
                id: "L3",
                trace: trace.clone(),
            });

        let op_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let op_flag = op_ran.clone();
        let mut call = NodeCall::new("t-1", "victim", 1, State::new());
        let result = chain
            .run_node(&mut call, |_call| {
                let op_flag = op_flag.clone();
                async move {
                    op_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(NodeOutput::empty())
                }
            })
            .await
            .unwrap();

        assert_eq!(result.updates.get("sub"), Some(&json!("substitute")));
        assert!(!op_ran.load(std::sync::atomic::Ordering::SeqCst));
        // Only the layers that entered before the short-circuit unwind;
        // neither the short-circuiting layer nor the ones past it run
        // after-hooks.
        assert_eq!(trace.entries(), vec!["L1:before:victim", "L1:after:victim"]);
    }

    /// After-hook short-circuits with a replacement result.
    struct Replace;

    #[async_trait]
    impl Middleware for Replace {
        fn name(&self) -> &str {
            "replace"
        }

        async fn after_node(
            &self,
            _call: &NodeCall,
            _output: &mut NodeOutput,
        ) -> Decision<NodeOutput> {
            Decision::ShortCircuit(NodeOutput::update("x", json!("replaced")))
        }
    }

    #[tokio::test]
    async fn after_hook_short_circuit_replaces_result_and_skips_outer_layers() {
        let trace = Trace::default();
        let chain = MiddlewareChain::new()
            .layer(Recorder {
                id: "L1",
                trace: trace.clone(),
            })
            .layer(Replace);

        let mut call = NodeCall::new("t-1", "node-a", 0, State::new());
        let result = chain
            .run_node(&mut call, |_call| async {
                Ok(NodeOutput::update("x", json!("original")))
            })
            .await
            .unwrap();

        assert_eq!(result.updates.get("x"), Some(&json!("replaced")));
        // L2's after-hook replaced the result; L1's after-hook never ran.
        assert_eq!(trace.entries(), vec!["L1:before:node-a"]);
    }

    /// A mock model that reports how many messages it received.
    struct CountingModel {
        seen: Arc<Mutex<Option<usize>>>,
    }

    #[async_trait]
    impl ChatModel for CountingModel {
        async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
            *self.seen.lock().unwrap() = Some(messages.len());
            Ok(ChatResponse {
                message: ChatMessage::assistant(format!("saw {} messages", messages.len())),
                model: None,
                usage: None,
            })
        }
    }

    /// Injects a system message inbound, rewrites content outbound.
    struct ModelMutate;

    #[async_trait]
    impl Middleware for ModelMutate {
        fn name(&self) -> &str {
            "model_mutate"
        }

        async fn before_model(&self, call: &mut ModelCall) -> Decision<ChatResponse> {
            call.messages_mut().insert(0, ChatMessage::system("rules"));
            Decision::Continue
        }

        async fn after_model(
            &self,
            _call: &ModelCall,
            response: &mut ChatResponse,
        ) -> Decision<ChatResponse> {
            response.message.content = Some("intercepted".into());
            Decision::Continue
        }
    }

    #[tokio::test]
    async fn model_chain_mutates_request_and_response() {
        let seen = Arc::new(Mutex::new(None));
        let model = MiddlewareChatModel::new(
            Arc::new(CountingModel { seen: seen.clone() }),
            MiddlewareChain::new().layer(ModelMutate),
        )
        .thread("t-1")
        .node("agent");

        let response = model.chat(&[ChatMessage::user("hi")], &[]).await.unwrap();

        // The injected system message reached the inner model; the
        // after-hook rewrite reached the caller.
        assert_eq!(*seen.lock().unwrap(), Some(2));
        assert_eq!(response.message.content.as_deref(), Some("intercepted"));
    }

    /// Rejects every model call.
    struct ModelReject;

    #[async_trait]
    impl Middleware for ModelReject {
        fn name(&self) -> &str {
            "model_reject"
        }

        async fn before_model(&self, _call: &mut ModelCall) -> Decision<ChatResponse> {
            Decision::Reject(Rejection::new(
                self.name(),
                InterceptPoint::ModelCall,
                "model_blocked",
            ))
        }
    }

    #[tokio::test]
    async fn model_reject_skips_inner_model() {
        let seen = Arc::new(Mutex::new(None));
        let model = MiddlewareChatModel::new(
            Arc::new(CountingModel { seen: seen.clone() }),
            MiddlewareChain::new().layer(ModelReject),
        );

        let err = model.chat(&[], &[]).await.unwrap_err();
        match err {
            RustyError::Llm(message) => {
                assert!(
                    message.contains(
                        "rejected by middleware `model_reject` at model_call: model_blocked"
                    ),
                    "got: {message}"
                );
            }
            other => panic!("expected RustyError::Llm, got {other:?}"),
        }
        assert_eq!(*seen.lock().unwrap(), None);
    }

    /// Short-circuits every model call with a canned response.
    struct ModelCache;

    #[async_trait]
    impl Middleware for ModelCache {
        fn name(&self) -> &str {
            "model_cache"
        }

        async fn before_model(&self, _call: &mut ModelCall) -> Decision<ChatResponse> {
            Decision::ShortCircuit(ChatResponse {
                message: ChatMessage::assistant("cached"),
                model: None,
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn model_short_circuit_returns_substitute_response() {
        let seen = Arc::new(Mutex::new(None));
        let model = MiddlewareChatModel::new(
            Arc::new(CountingModel { seen: seen.clone() }),
            MiddlewareChain::new().layer(ModelCache),
        );

        let response = model.chat(&[], &[]).await.unwrap();
        assert_eq!(response.message.content.as_deref(), Some("cached"));
        assert_eq!(*seen.lock().unwrap(), None);
    }

    /// A wrapper with an empty chain must delegate untouched.
    #[tokio::test]
    async fn model_wrapper_with_empty_chain_delegates() {
        let seen = Arc::new(Mutex::new(None));
        let model = MiddlewareChatModel::new(
            Arc::new(CountingModel { seen: seen.clone() }),
            MiddlewareChain::new(),
        );
        let response = model.chat(&[ChatMessage::user("hi")], &[]).await.unwrap();
        assert_eq!(response.message.content.as_deref(), Some("saw 1 messages"));
        assert_eq!(*seen.lock().unwrap(), Some(1));
    }

    // ---------- streaming interception (provider layer) ----------

    /// A model with a genuine `chat_stream`: records the message count of
    /// every invocation and emits two delta chunks before the terminal one.
    struct ScriptedStreamModel {
        calls: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl ChatModel for ScriptedStreamModel {
        async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
            panic!("a streamed test must not degrade to the chat fallback");
        }

        async fn chat_stream(
            &self,
            messages: &[ChatMessage],
            _tools: &[Value],
            on_token: &mut (dyn FnMut(TokenChunk) + Send),
        ) -> Result<ChatResponse> {
            self.calls.lock().unwrap().push(messages.len());
            for delta in ["Hel", "lo"] {
                on_token(TokenChunk {
                    delta: delta.into(),
                    finish: false,
                    raw: None,
                });
            }
            on_token(TokenChunk {
                delta: String::new(),
                finish: true,
                raw: None,
            });
            Ok(ChatResponse {
                message: ChatMessage::assistant("Hello"),
                model: Some("scripted".into()),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn wrapped_streaming_model_still_streams_real_tokens() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let model = MiddlewareChatModel::new(
            Arc::new(ScriptedStreamModel {
                calls: calls.clone(),
            }),
            MiddlewareChain::new().layer(ModelMutate),
        );

        let mut chunks: Vec<TokenChunk> = Vec::new();
        let response = model
            .chat_stream(&[ChatMessage::user("hi")], &[], &mut |chunk| {
                chunks.push(chunk)
            })
            .await
            .unwrap();

        // The inner model's real deltas arrive live — before the override,
        // a wrapped model degraded to the single-chunk `chat` fallback.
        let deltas: Vec<&str> = chunks.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(deltas, ["Hel", "lo", ""]);
        assert!(chunks.last().unwrap().finish);
        // Request hooks ran inbound (ModelMutate injected a system
        // message)...
        assert_eq!(*calls.lock().unwrap(), vec![2]);
        // ...and the after-hook's rewrite of the accumulated response is
        // what returns.
        assert_eq!(response.message.content.as_deref(), Some("intercepted"));
    }

    #[tokio::test]
    async fn streaming_rejection_prevents_the_inner_call() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let model = MiddlewareChatModel::new(
            Arc::new(ScriptedStreamModel {
                calls: calls.clone(),
            }),
            MiddlewareChain::new().layer(ModelReject),
        );

        let mut chunks: Vec<TokenChunk> = Vec::new();
        let err = model
            .chat_stream(&[], &[], &mut |chunk| chunks.push(chunk))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("model_blocked"), "got: {err}");
        // The provider is never called and no tokens flow.
        assert!(calls.lock().unwrap().is_empty());
        assert!(chunks.is_empty());
    }

    #[tokio::test]
    async fn streaming_short_circuit_returns_the_substitute_response() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let model = MiddlewareChatModel::new(
            Arc::new(ScriptedStreamModel {
                calls: calls.clone(),
            }),
            MiddlewareChain::new().layer(ModelCache),
        );

        let mut chunks: Vec<TokenChunk> = Vec::new();
        let response = model
            .chat_stream(&[], &[], &mut |chunk| chunks.push(chunk))
            .await
            .unwrap();

        assert_eq!(response.message.content.as_deref(), Some("cached"));
        assert!(calls.lock().unwrap().is_empty());
        assert!(chunks.is_empty());
    }

    /// An empty chain forwards the stream untouched.
    #[tokio::test]
    async fn streaming_wrapper_with_empty_chain_delegates() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let model = MiddlewareChatModel::new(
            Arc::new(ScriptedStreamModel {
                calls: calls.clone(),
            }),
            MiddlewareChain::new(),
        );
        let mut chunks: Vec<TokenChunk> = Vec::new();
        let response = model
            .chat_stream(&[ChatMessage::user("hi")], &[], &mut |chunk| {
                chunks.push(chunk)
            })
            .await
            .unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(response.message.content.as_deref(), Some("Hello"));
        assert_eq!(*calls.lock().unwrap(), vec![1]);
    }

    struct EchoTool;

    #[async_trait]
    impl crate::tool::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its `text` argument."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn call(&self, args: Value) -> Result<Value> {
            Ok(json!(args.get("text").cloned().unwrap_or(Value::Null)))
        }
    }

    struct DangerTool;

    #[async_trait]
    impl crate::tool::Tool for DangerTool {
        fn name(&self) -> &str {
            "danger"
        }
        fn description(&self) -> &str {
            "Must never run."
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(&self, _args: Value) -> Result<Value> {
            panic!("blocklisted tool executed");
        }
    }

    fn echo_registry() -> crate::tool::ToolRegistry {
        let mut registry = crate::tool::ToolRegistry::new();
        registry.register(EchoTool);
        registry.register(DangerTool);
        registry
    }

    #[tokio::test]
    async fn blocklist_rejects_blocked_tool_and_passes_allowed() {
        let executor = crate::tool::ToolExecutor::new(echo_registry())
            .with_middleware(MiddlewareChain::new().layer(ToolCallBlocklist::new(["danger"])));

        let calls = vec![
            ToolCall::new("c1", "echo", json!({"text": "hello"})),
            ToolCall::new("c2", "danger", json!({})),
        ];
        let results = executor.execute_batch(&calls).await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content.as_deref(), Some("hello"));
        let blocked = results[1].content.as_deref().unwrap();
        assert!(blocked.starts_with("ERROR:"), "got: {blocked}");
        assert!(blocked.contains("tool_call_blocklist"), "got: {blocked}");
        assert!(blocked.contains("tool_blocked"), "got: {blocked}");
    }

    /// Rewrites tool arguments inbound.
    struct ArgsRewrite;

    #[async_trait]
    impl Middleware for ArgsRewrite {
        fn name(&self) -> &str {
            "args_rewrite"
        }

        async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
            call.set_arguments(json!({"text": "mutated"}));
            Decision::Continue
        }
    }

    #[tokio::test]
    async fn tool_argument_mutation_propagates_to_tool() {
        let executor = crate::tool::ToolExecutor::new(echo_registry())
            .with_middleware(MiddlewareChain::new().layer(ArgsRewrite));

        let calls = vec![ToolCall::new("c1", "echo", json!({"text": "original"}))];
        let results = executor.execute_batch(&calls).await;
        assert_eq!(results[0].content.as_deref(), Some("mutated"));
    }

    /// Answers tool calls from a cache without executing them.
    struct ToolCache;

    #[async_trait]
    impl Middleware for ToolCache {
        fn name(&self) -> &str {
            "tool_cache"
        }

        async fn before_tool(&self, _call: &mut ToolInvocation) -> Decision<Value> {
            Decision::ShortCircuit(json!("cached"))
        }
    }

    #[tokio::test]
    async fn tool_short_circuit_returns_substitute_result() {
        let executor = crate::tool::ToolExecutor::new(echo_registry())
            .with_middleware(MiddlewareChain::new().layer(ToolCache));

        let calls = vec![ToolCall::new("c1", "echo", json!({"text": "ignored"}))];
        let results = executor.execute_batch(&calls).await;
        assert_eq!(results[0].content.as_deref(), Some("cached"));
    }

    /// Rewrites the tool result outbound.
    struct ResultRewrite;

    #[async_trait]
    impl Middleware for ResultRewrite {
        fn name(&self) -> &str {
            "result_rewrite"
        }

        async fn after_tool(&self, _call: &ToolInvocation, result: &mut Value) -> Decision<Value> {
            *result = json!("rewritten");
            Decision::Continue
        }
    }

    #[tokio::test]
    async fn tool_result_mutation_propagates_to_caller() {
        let executor = crate::tool::ToolExecutor::new(echo_registry())
            .with_middleware(MiddlewareChain::new().layer(ResultRewrite));

        let calls = vec![ToolCall::new("c1", "echo", json!({"text": "original"}))];
        let results = executor.execute_batch(&calls).await;
        assert_eq!(results[0].content.as_deref(), Some("rewritten"));
    }

    /// Records tool hooks for order proofs.
    struct ToolRecorder {
        id: &'static str,
        trace: Trace,
    }

    #[async_trait]
    impl Middleware for ToolRecorder {
        fn name(&self) -> &str {
            self.id
        }

        async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
            self.trace
                .record(format!("{}:before:{}", self.id, call.name()));
            Decision::Continue
        }

        async fn after_tool(&self, call: &ToolInvocation, _result: &mut Value) -> Decision<Value> {
            self.trace
                .record(format!("{}:after:{}", self.id, call.name()));
            Decision::Continue
        }
    }

    #[tokio::test]
    async fn tool_hooks_follow_onion_order() {
        let trace = Trace::default();
        let executor = crate::tool::ToolExecutor::new(echo_registry()).with_middleware(
            MiddlewareChain::new()
                .layer(ToolRecorder {
                    id: "L1",
                    trace: trace.clone(),
                })
                .layer(ToolRecorder {
                    id: "L2",
                    trace: trace.clone(),
                }),
        );

        let calls = vec![ToolCall::new("c1", "echo", json!({"text": "hi"}))];
        let results = executor.execute_batch(&calls).await;
        assert_eq!(results[0].content.as_deref(), Some("hi"));
        assert_eq!(
            trace.entries(),
            vec![
                "L1:before:echo",
                "L2:before:echo",
                "L2:after:echo",
                "L1:after:echo"
            ]
        );
    }

    #[tokio::test]
    async fn request_logger_passes_everything_through() {
        let chain = MiddlewareChain::new().layer(RequestLogger::new());

        let mut node_call = NodeCall::new("t-1", "node-a", 0, State::new());
        let node_result = chain
            .run_node(&mut node_call, |_call| async {
                Ok(NodeOutput::update("x", json!(1)))
            })
            .await
            .unwrap();
        assert_eq!(node_result.updates.get("x"), Some(&json!(1)));

        let mut model_call = ModelCall::new("t-1", "agent", vec![ChatMessage::user("hi")], vec![]);
        let model_result = chain
            .run_model(&mut model_call, |_call| async {
                Ok(ChatResponse {
                    message: ChatMessage::assistant("hello"),
                    model: None,
                    usage: None,
                })
            })
            .await
            .unwrap();
        assert_eq!(model_result.message.content.as_deref(), Some("hello"));

        let mut tool_call =
            ToolInvocation::new("t-1", "tools", ToolCall::new("c1", "echo", json!({})));
        let tool_result = chain
            .run_tool(&mut tool_call, |_call| async { Ok(json!("done")) })
            .await
            .unwrap();
        assert_eq!(tool_result, json!("done"));
    }

    #[test]
    fn rejection_serde_roundtrip_display_and_error_mapping() {
        let rejection = Rejection::new(
            "tool_call_blocklist",
            InterceptPoint::ToolCall,
            "tool_blocked",
        )
        .with_detail("tool `rm_rf` is blocked by policy");
        let back: Rejection =
            serde_json::from_str(&serde_json::to_string(&rejection).unwrap()).unwrap();
        assert_eq!(rejection, back);

        let msg = rejection.to_string();
        assert!(msg.contains("tool_call_blocklist"), "got: {msg}");
        assert!(msg.contains("tool_call"), "got: {msg}");
        assert!(msg.contains("tool_blocked"), "got: {msg}");
        assert_eq!(format!("{}", rejection.point), "tool_call");

        // The taxonomy mapping: each interception point lands on its
        // existing error variant, carrying the canonical Display form.
        for (point, expect) in [
            (InterceptPoint::NodeRun, "node error"),
            (InterceptPoint::ModelCall, "llm error"),
            (InterceptPoint::ToolCall, "tool error"),
        ] {
            let err = Rejection::new("m", point, "r").into_error();
            assert!(
                err.to_string().starts_with(expect),
                "{point}: expected prefix `{expect}`, got: {err}"
            );
            assert!(err.to_string().contains("rejected by middleware `m`"));
        }
    }

    #[test]
    fn chain_debug_lists_layer_names() {
        let chain = MiddlewareChain::new()
            .layer(RequestLogger::new())
            .layer(ToolCallBlocklist::new(["danger"]));
        let debug = format!("{chain:?}");
        assert!(debug.contains("request_logger"), "got: {debug}");
        assert!(debug.contains("tool_call_blocklist"), "got: {debug}");
    }

    // ---------- instantiate_composition (R0.11 wave 4) ----------

    fn layer(name: &str, config: Option<Value>) -> MiddlewareLayerConfig {
        MiddlewareLayerConfig {
            layer: name.to_owned(),
            config,
        }
    }

    #[test]
    fn instantiate_builds_the_journaled_order() {
        let chain = instantiate_composition(&[
            layer("request_logger", None),
            layer(
                "tool_call_blocklist",
                Some(json!({"blocked": ["shell", "fs_write"], "reason": "policy_denied"})),
            ),
        ])
        .unwrap();
        assert_eq!(
            chain.names().collect::<Vec<_>>(),
            ["request_logger", "tool_call_blocklist"]
        );
        // The order *is* the artifact: the reversed composition builds the
        // reversed chain.
        let reversed = instantiate_composition(&[
            layer("tool_call_blocklist", Some(json!({"blocked": ["shell"]}))),
            layer("request_logger", None),
        ])
        .unwrap();
        assert_eq!(
            reversed.names().collect::<Vec<_>>(),
            ["tool_call_blocklist", "request_logger"]
        );
        // An empty composition instantiates an empty chain — pass-through,
        // not an error.
        assert!(instantiate_composition(&[]).unwrap().is_empty());
    }

    #[tokio::test]
    async fn instantiated_blocklist_enforces_its_config() {
        // The chain is not a name list: the instantiated layer enforces
        // the config the composition declared.
        let chain = instantiate_composition(&[layer(
            "tool_call_blocklist",
            Some(json!({"blocked": ["shell"]})),
        )])
        .unwrap();
        let mut call = ToolInvocation::new(
            "t-1",
            "node-a",
            ToolCall {
                id: "c-1".into(),
                name: "shell".into(),
                arguments: json!({}),
            },
        );
        let err = chain
            .run_tool(&mut call, |_call| async { Ok(json!("ran")) })
            .await
            .unwrap_err();
        // Tool rejections surface through the taxonomy as RustyError::Tool
        // carrying the Rejection's canonical Display (the typed Rejection's
        // `detail` stays at the middleware API).
        assert!(
            err.to_string().contains(
                "rejected by middleware `tool_call_blocklist` at tool_call: tool_blocked"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn instantiate_refuses_what_the_vocabulary_cannot_honor() {
        // Unknown layer: the closed set never grows by accepting unknown
        // names at resolution time.
        let err = instantiate_composition(&[layer("otel_tracer", None)]).unwrap_err();
        assert!(err.to_string().contains("otel_tracer"), "got: {err}");
        assert!(
            err.to_string().contains("compiled-in vocabulary"),
            "got: {err}"
        );

        // A config on the config-free layer: evidence the chain cannot
        // honor, refused rather than silently ignored.
        let err =
            instantiate_composition(&[layer("request_logger", Some(json!({"level": "debug"})))])
                .unwrap_err();
        assert!(err.to_string().contains("request_logger"), "got: {err}");

        // The blocklist without its config: the policy is the config, so
        // a config-free blocklist blocks nothing and means nothing.
        let err = instantiate_composition(&[layer("tool_call_blocklist", None)]).unwrap_err();
        assert!(err.to_string().contains("requires a config"), "got: {err}");

        // Malformed config: `blocked` must be a string list.
        let err = instantiate_composition(&[layer(
            "tool_call_blocklist",
            Some(json!({"blocked": "shell"})),
        )])
        .unwrap_err();
        assert!(err.to_string().contains("malformed"), "got: {err}");
    }
}
