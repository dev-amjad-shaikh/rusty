//! Node abstractions: the unit of computation in the graph.
//!
//! A node is an async computation that:
//!
//! 1. Receives a [`NodeContext`] — an **immutable snapshot** of the shared
//!    state as of the start of the current super-step, plus run configuration
//!    and interrupt/resume helpers.
//! 2. Returns a [`NodeOutput`] — a *partial* state update (never the whole
//!    state) plus an optional [`Command`] for dynamic routing.
//!
//! Because nodes only ever see the snapshot from the super-step start, nodes
//! running in the same super-step can never observe each other's writes —
//! the barrier is what makes shared-state parallelism safe.
//!
//! Any async closure `Fn(NodeContext) -> impl Future<Output = Result<NodeOutput>>`
//! automatically implements [`Node`] via a blanket impl.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::effects::EffectAdmissionContext;
use crate::error::{Result, RustyError};
use crate::middleware::MiddlewareChain;
use crate::state::State;

/// Per-run, per-node configuration handed to every node invocation.
///
/// This is a deliberately small, cloneable value (the LangGraph analog is
/// `RunnableConfig` + `Runtime`). It is constructed fresh by the executor at
/// each super-step and embedded in the [`NodeContext`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeConfig {
    /// The thread (session) this run belongs to. Stable across interrupts
    /// and resumes; namespacing for checkpoints.
    pub thread_id: String,

    /// Zero-based index of the current super-step. Useful for logging,
    /// `remaining_steps`-style degradation, and idempotency keys.
    pub step: usize,

    /// The resume value supplied by the caller when resuming from an
    /// interrupt (`RunConfig::resume` / `Command::resume`).
    ///
    /// Protocol (mirrors LangGraph): a node that previously called
    /// `interrupt(payload)` is **re-executed from its start**; on the resume
    /// run, this field carries the caller's value and
    /// [`NodeContext::resume_value`] returns `Some`. Nodes must therefore be
    /// idempotent with respect to side effects performed before the
    /// `interrupt()` call.
    pub resume: Option<Value>,

    /// Free-form extension point (tags, tracing metadata, user config).
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

/// The input to every node invocation.
///
/// The state snapshot is cloned per node invocation, so snapshot isolation
/// is structural, not conventional: nodes running in the same super-step
/// each hold their own owned snapshot and can never observe each other's
/// writes.
#[derive(Debug, Clone)]
pub struct NodeContext {
    state: State,
    config: NodeConfig,
    middleware: MiddlewareChain,
    effect_admission: Option<EffectAdmissionContext>,
}

impl NodeContext {
    /// Build a context from a state snapshot and config. Primarily used by
    /// the executor; tests may construct it directly. Carries no middleware
    /// chain — see [`NodeContext::with_middleware`].
    pub fn new(state: State, config: NodeConfig) -> Self {
        Self {
            state,
            config,
            middleware: MiddlewareChain::new(),
            effect_admission: None,
        }
    }

    /// The immutable state snapshot as of the start of this super-step.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The run configuration for this invocation.
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Builder-style: attach the run's middleware chain. The executor sets
    /// this when layers are registered; empty otherwise.
    pub fn with_middleware(mut self, chain: MiddlewareChain) -> Self {
        self.middleware = chain;
        self
    }

    /// The middleware chain attached to the run's executor (Middleware /
    /// Interceptor SDK). Node code uses it to propagate interception into
    /// the boundaries it controls: hand it to
    /// [`crate::tool::ToolExecutor::with_middleware`] or wrap models in
    /// [`crate::middleware::MiddlewareChatModel`]. Empty when the executor
    /// has no layers.
    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    /// The run-scoped effect boundary attached by an executor that enabled
    /// admission enforcement. Nodes dispatching tools should pass this to
    /// [`crate::tool::ToolExecutor::with_effect_admission`]. The prebuilt
    /// ReAct tools node does so automatically.
    pub fn effect_admission(&self) -> Option<&EffectAdmissionContext> {
        self.effect_admission.as_ref()
    }

    /// Builder-style: attach a run-scoped effect admission boundary.
    pub fn with_effect_admission(mut self, context: EffectAdmissionContext) -> Self {
        self.effect_admission = Some(context);
        self
    }

    pub(crate) fn with_optional_effect_admission(
        mut self,
        context: Option<EffectAdmissionContext>,
    ) -> Self {
        self.effect_admission = context;
        self
    }

    /// The current thread id (shortcut for `config().thread_id`).
    pub fn thread_id(&self) -> &str {
        &self.config.thread_id
    }

    /// The current super-step index (shortcut for `config().step`).
    pub fn step(&self) -> usize {
        self.config.step
    }

    /// The resume value, if this run is resuming from an interrupt.
    ///
    /// Typical pattern inside a resumable node:
    ///
    /// ```
    /// use rusty_agent_runtime::node::{NodeConfig, NodeContext};
    /// use rusty_agent_runtime::state::State;
    /// use serde_json::{json, Value};
    ///
    /// # fn pattern(ctx: NodeContext) -> rusty_agent_runtime::error::Result<Value> {
    /// let approved = match ctx.resume_value() {
    ///     Some(v) => v.clone(),            // resumed: interrupt() "returns" v
    ///     None => return Err(ctx.interrupt(json!({"question": "approve?"}))),
    /// };
    /// # Ok(approved)
    /// # }
    /// # let ctx = NodeContext::new(State::new(), NodeConfig::default());
    /// # assert!(pattern(ctx).unwrap_err().is_interrupt());
    /// ```
    pub fn resume_value(&self) -> Option<&Value> {
        self.config.resume.as_ref()
    }

    /// Build the interrupt error for a payload.
    ///
    /// Returning `Err(ctx.interrupt(payload))` from a node suspends the whole
    /// run — not just this node. The super-step is transactional: no write of
    /// the in-flight step survives, not even from sibling nodes that already
    /// completed. The executor therefore persists a checkpoint that
    /// re-schedules the **entire active set** of the suspended step — the
    /// interrupting node plus all of its siblings — and surfaces `payload` in
    /// [`crate::executor::ExecutionOutcome::Interrupted`]. On resume, every
    /// node of that set re-executes from its start (node logic must be
    /// idempotent), with [`NodeContext::resume_value`] set for the first
    /// super-step.
    pub fn interrupt(&self, value: Value) -> RustyError {
        RustyError::Interrupt { value }
    }
}

/// The output of a node: partial state updates plus optional dynamic routing.
#[derive(Debug, Clone, Default)]
pub struct NodeOutput {
    /// Partial updates to merge into shared state at the barrier, keyed by
    /// channel name. Merge semantics come from the channel's
    /// [`crate::state::Reducer`]. Nodes **must not** return whole state.
    pub updates: HashMap<String, Value>,

    /// Optional dynamic routing decision (overrides static edges).
    ///
    /// `Some(Command::default())` is a no-op in disguise (see
    /// [`Command::is_noop`]); prefer `None` for "no override".
    pub command: Option<Command>,
}

impl NodeOutput {
    /// An output with no updates and no routing override.
    pub fn empty() -> Self {
        Self::default()
    }

    /// An output carrying a single channel update.
    pub fn update(channel: impl Into<String>, value: Value) -> Self {
        let mut updates = HashMap::new();
        updates.insert(channel.into(), value);
        Self {
            updates,
            command: None,
        }
    }

    /// An output carrying multiple channel updates.
    pub fn updates(updates: HashMap<String, Value>) -> Self {
        Self {
            updates,
            command: None,
        }
    }

    /// Alias for [`NodeOutput::updates`] that reads better next to
    /// [`NodeOutput::update`] / [`NodeOutput::with_update`].
    pub fn from_updates(updates: HashMap<String, Value>) -> Self {
        Self::updates(updates)
    }

    /// An output that only routes (no state updates).
    ///
    /// Note that [`Command::default`] is a no-op ([`Command::is_noop`]);
    /// wrapping one in `Some` here is accepted but has no effect.
    pub fn route(command: Command) -> Self {
        Self {
            updates: HashMap::new(),
            command: Some(command),
        }
    }

    /// Builder-style: attach a routing command.
    pub fn with_command(mut self, command: Command) -> Self {
        self.command = Some(command);
        self
    }

    /// Builder-style: add one channel update.
    pub fn with_update(mut self, channel: impl Into<String>, value: Value) -> Self {
        self.updates.insert(channel.into(), value);
        self
    }
}

/// Dynamic routing + resume directive, unifying state transition and control
/// flow (the LangGraph `Command`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Command {
    /// Nodes to activate in the next super-step, overriding the static edge
    /// set. Names must resolve to graph nodes (validated by the executor).
    ///
    /// An empty `goto` with no other information means "no routing override".
    #[serde(default)]
    pub goto: Vec<String>,

    /// A resume value. As **input** (`RunConfig::resume`) this is the only
    /// valid Command pattern: it supplies the value an interrupted node's
    /// `interrupt()` logically returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Value>,
}

impl Command {
    /// Route to a single node next.
    pub fn goto(node: impl Into<String>) -> Self {
        Self {
            goto: vec![node.into()],
            resume: None,
        }
    }

    /// Route to multiple nodes (parallel activation) next.
    pub fn goto_many<I, S>(nodes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            goto: nodes.into_iter().map(Into::into).collect(),
            resume: None,
        }
    }

    /// A resume directive (used as run input after an interrupt).
    pub fn resume(value: Value) -> Self {
        Self {
            goto: Vec::new(),
            resume: Some(value),
        }
    }

    /// `true` if this command carries no routing override and no resume
    /// value — i.e. `Command::default()`. Consumers can use this to treat
    /// `Some(noop)` the same as `None`.
    pub fn is_noop(&self) -> bool {
        self.goto.is_empty() && self.resume.is_none()
    }
}

/// The node trait: an async unit of computation over shared state.
///
/// Implement this directly for stateful nodes, or just pass an async closure
/// to [`crate::graph::GraphBuilder::add_node`] — a blanket impl covers any
/// `Fn(NodeContext) -> impl Future<Output = Result<NodeOutput>> + Send + Sync`.
///
/// **Contract** (enforced by the executor, documented here):
///
/// - Nodes read state **only** from [`NodeContext::state`] (the super-step
///   snapshot) and express all state changes via [`NodeOutput::updates`].
/// - Node logic must be **idempotent**: on interrupt-resume or
///   partial-failure recovery the node re-executes from its start.
#[async_trait]
pub trait Node: Send + Sync {
    /// Human/log-friendly name. Closure nodes default to `"anonymous"`;
    /// the graph tracks nodes by the name given to `add_node`, which is the
    /// authoritative identity.
    fn name(&self) -> &str {
        "anonymous"
    }

    /// The declared default effect classification of this node (Flight
    /// Recorder, R0.5): recorded on the node's journal events and used by
    /// retry/replay policy.
    ///
    /// The default is [`crate::record::Effect::Pure`] — a plain compute node
    /// over its state snapshot. **Override honestly**: a node that performs
    /// I/O or side effects inside `run` is not `Pure`, and misdeclaring it
    /// weakens replay fidelity (the journal will claim an effect was
    /// re-derivable when it was not). [`crate::remote::RemoteNode`] and the
    /// `wasm`-feature `WasmNode` override to `NonIdempotent`.
    fn effect(&self) -> crate::record::Effect {
        crate::record::Effect::Pure
    }

    /// Execute the node against a super-step state snapshot.
    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput>;
}

/// Blanket implementation for async closures/functions:
/// `Fn(NodeContext) -> impl Future<Output = Result<NodeOutput>>`.
#[async_trait]
impl<F, Fut> Node for F
where
    F: Fn(NodeContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<NodeOutput>> + Send,
{
    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput> {
        (self)(ctx).await
    }
}

/// Allow `Arc<dyn Node>` itself to be registered as a node (useful for
/// sharing one node implementation across graphs).
#[async_trait]
impl Node for Arc<dyn Node> {
    fn name(&self) -> &str {
        self.as_ref().name()
    }

    async fn run(&self, ctx: NodeContext) -> Result<NodeOutput> {
        self.as_ref().run(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn closure_implements_node() {
        let node = |ctx: NodeContext| async move {
            assert_eq!(ctx.thread_id(), "t1");
            Ok(NodeOutput::update("x", json!(ctx.step())))
        };
        let ctx = NodeContext::new(
            State::new(),
            NodeConfig {
                thread_id: "t1".into(),
                step: 3,
                resume: None,
                extra: HashMap::new(),
            },
        );
        let out = Node::run(&node, ctx).await.unwrap();
        assert_eq!(out.updates.get("x"), Some(&json!(3)));
        assert_eq!(node.name(), "anonymous");
    }

    #[tokio::test]
    async fn interrupt_helper_produces_interrupt_error() {
        let node = |ctx: NodeContext| async move {
            match ctx.resume_value() {
                Some(v) => Ok(NodeOutput::update("answer", v.clone())),
                None => Err(ctx.interrupt(json!({"question": "approve?"}))),
            }
        };

        let ctx = NodeContext::new(State::new(), NodeConfig::default());
        let err = Node::run(&node, ctx).await.unwrap_err();
        assert!(err.is_interrupt());
        assert_eq!(
            err.interrupt_value(),
            Some(&json!({"question": "approve?"}))
        );

        let resumed = NodeContext::new(
            State::new(),
            NodeConfig {
                resume: Some(json!(true)),
                ..NodeConfig::default()
            },
        );
        let out = Node::run(&node, resumed).await.unwrap();
        assert_eq!(out.updates.get("answer"), Some(&json!(true)));
    }
}
