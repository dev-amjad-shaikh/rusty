//! Graph definition, plus structural validation when you call
//! [`GraphBuilder::compile`].
//!
//! The public graph API is deliberately a thin builder ([`GraphBuilder`]);
//! `compile()` freezes the graph into an immutable, cheaply-clonable
//! [`Graph`] with `Arc`-wrapped internals and validates structure **before
//! any node (or LLM call) ever runs**:
//!
//! - an entry point must be set and must reference a known node;
//! - every edge endpoint must reference a known node;
//! - at least one node must be registered;
//! - node names must not collide with the reserved [`END`] sentinel;
//! - edges must be unambiguous: no duplicate `from → to` edges, at most one
//!   conditional edge per source node, and no source node mixing static and
//!   conditional edges.
//!
//! Edge types:
//!
//! - **Normal edges** ([`GraphBuilder::add_edge`]): `from → to`. Multiple
//!   outgoing normal edges from one node activate **all** destinations in
//!   parallel in the next super-step.
//! - **Conditional edges** ([`GraphBuilder::add_conditional_edges`]): an
//!   async routing function reads the post-barrier state and returns a
//!   [`Route`] — one node, dynamic fan-out via [`Send`], or the end of the
//!   run.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::node::Node;
use crate::state::State;

/// Reserved node name for the terminal route. Not a real node: routers
/// signal termination with [`Route::End`] instead of naming a node.
/// [`GraphBuilder::compile`] rejects registering a node under this name (or
/// any `__`-prefixed name) so the sentinel namespace can never collide with
/// user nodes.
pub const END: &str = "__end__";

/// The routing decision of a conditional edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Activate exactly one node next.
    Node(String),
    /// Dynamic fan-out (LangGraph `Send` API): activate one node invocation
    /// per item, each with its own scoped input state. The canonical
    /// map-reduce pattern: items are generated at runtime, each mapped
    /// through a node, results fan back in through multi-write reducers.
    Send(Vec<Send>),
    /// Terminate the run.
    End,
}

/// A single dynamic fan-out instruction: run `node` once with `state` as
/// its scoped input state.
///
/// Semantics for the executor: the scoped state is merged into the shared
/// state before the node runs (so the node sees its item), and the node's
/// updates merge back through the normal channel reducers — fan-in requires
/// multi-write reducers on the destination channels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Send {
    /// Target node name (must resolve to a known node at execution time).
    pub node: String,
    /// Scoped input state for this invocation (partial update applied to the
    /// shared state snapshot before the node runs).
    pub state: Value,
}

impl Send {
    /// Convenience constructor.
    pub fn new(node: impl Into<String>, state: Value) -> Self {
        Self {
            node: node.into(),
            state,
        }
    }
}

/// Async routing function for a conditional edge: reads the post-barrier
/// state and decides where the run goes next.
///
/// Produced by [`GraphBuilder::add_conditional_edges`] from any
/// `Fn(State) -> impl Future<Output = Result<Route>>`.
// NOTE: `Send` below refers to std's marker trait; the local `Send` struct
// shadows it in this module, hence the fully-qualified path.
pub type ConditionalRouter = Arc<
    dyn Fn(State) -> Pin<Box<dyn Future<Output = Result<Route>> + std::marker::Send>>
        + std::marker::Send
        + Sync,
>;

/// One edge in the graph.
#[derive(Clone)]
pub enum Edge {
    /// Static edge: after `from` completes, activate `to` in the next
    /// super-step. Multiple `Direct` edges from the same node activate all
    /// targets in parallel.
    Direct { from: String, to: String },
    /// Dynamic edge: after `from` completes, evaluate `router` against the
    /// post-barrier state to decide the next activation.
    Conditional {
        from: String,
        router: ConditionalRouter,
    },
}

impl Edge {
    /// The source node of this edge.
    pub fn from(&self) -> &str {
        match self {
            Edge::Direct { from, .. } | Edge::Conditional { from, .. } => from,
        }
    }
}

impl fmt::Debug for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Edge::Direct { from, to } => write!(f, "Edge::Direct({from} -> {to})"),
            Edge::Conditional { from, .. } => write!(f, "Edge::Conditional({from} -> ?)"),
        }
    }
}

/// Internal, immutable graph topology. Wrapped in `Arc` by [`Graph`].
struct GraphInner {
    nodes: HashMap<String, Arc<dyn Node>>,
    edges: Vec<Edge>,
    entry_point: String,
}

/// A compiled, immutable, thread-safe graph. Cheap to clone (Arc internals).
///
/// Obtain via [`GraphBuilder::compile`]. The executor drives a `Graph`;
/// graphs are frozen so topology can never drift mid-run.
pub struct Graph {
    inner: Arc<GraphInner>,
}

impl Clone for Graph {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl fmt::Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph")
            .field("nodes", &self.inner.nodes.keys().collect::<Vec<_>>())
            .field("edges", &self.inner.edges)
            .field("entry_point", &self.inner.entry_point)
            .finish()
    }
}

impl Graph {
    /// The entry-point node name.
    pub fn entry_point(&self) -> &str {
        &self.inner.entry_point
    }

    /// Look up a node by name.
    pub fn node(&self, name: &str) -> Option<Arc<dyn Node>> {
        self.inner.nodes.get(name).cloned()
    }

    /// `true` if a node with this name is registered.
    pub fn has_node(&self, name: &str) -> bool {
        self.inner.nodes.contains_key(name)
    }

    /// All registered node names.
    pub fn node_names(&self) -> impl Iterator<Item = &str> {
        self.inner.nodes.keys().map(String::as_str)
    }

    /// All edges originating at `from` (static and conditional).
    pub fn outgoing_edges(&self, from: &str) -> Vec<&Edge> {
        self.inner
            .edges
            .iter()
            .filter(|e| e.from() == from)
            .collect()
    }

    /// All edges in the graph.
    pub fn edges(&self) -> &[Edge] {
        &self.inner.edges
    }

    /// Number of registered nodes.
    pub fn node_count(&self) -> usize {
        self.inner.nodes.len()
    }

    /// SHA-256 content hash of the graph's topology: the sorted node names
    /// and the sorted edge descriptors (`from>to` for static edges,
    /// `from>?` for conditional routers).
    ///
    /// Stamped into [`crate::record::CheckpointHeader::graph_hash`] so replay
    /// can detect that a checkpoint and the graph about to resume it
    /// disagree structurally. It is deliberately a *topology* hash: node
    /// bodies are opaque closures/trait objects, so semantic changes inside
    /// a node cannot be detected here — that is what the application-level
    /// `graph_version` (set via `RunConfig::with_graph_version`) is for.
    pub fn topology_hash(&self) -> String {
        let mut lines: Vec<String> =
            Vec::with_capacity(self.inner.nodes.len() + self.inner.edges.len());
        lines.extend(self.inner.nodes.keys().map(|name| format!("node:{name}")));
        lines.extend(self.inner.edges.iter().map(|edge| match edge {
            Edge::Direct { from, to } => format!("edge:{from}>{to}"),
            Edge::Conditional { from, .. } => format!("edge:{from}>?"),
        }));
        lines.push(format!("entry:{}", self.inner.entry_point));
        lines.sort_unstable();
        crate::record::sha256_hex(lines.join("\n").as_bytes())
    }
}

/// Builder for a [`Graph`]. Register nodes and edges, set the entry point,
/// then [`GraphBuilder::compile`] to validate and freeze.
#[derive(Default)]
pub struct GraphBuilder {
    nodes: HashMap<String, Arc<dyn Node>>,
    edges: Vec<Edge>,
    entry_point: Option<String>,
}

impl GraphBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a node under `name`.
    ///
    /// Accepts any [`Node`] impl — including async closures
    /// (`Fn(NodeContext) -> impl Future<Output = Result<NodeOutput>>`) via
    /// the blanket impl. Re-registering the same name replaces the node.
    pub fn add_node<N>(&mut self, name: impl Into<String>, node: N) -> &mut Self
    where
        N: Node + 'static,
    {
        self.nodes.insert(name.into(), Arc::new(node));
        self
    }

    /// Add a static edge `from → to`. Multiple outgoing static edges from
    /// the same node activate all destinations in parallel in the next
    /// super-step.
    ///
    /// [`GraphBuilder::compile`] rejects a node that has both static and
    /// conditional edges — ambiguous routing fails when you call `compile()`,
    /// not as a runtime surprise. Dynamic routing via `Command::goto` from a node
    /// that also has static edges remains a runtime rule: both paths
    /// execute.
    pub fn add_edge(&mut self, from: impl Into<String>, to: impl Into<String>) -> &mut Self {
        self.edges.push(Edge::Direct {
            from: from.into(),
            to: to.into(),
        });
        self
    }

    /// Add a conditional edge: after `from` completes, run `router` against
    /// the post-barrier state and follow the returned [`Route`].
    ///
    /// ```
    /// use rusty_agent_runtime::graph::{GraphBuilder, Route};
    ///
    /// let mut builder = GraphBuilder::new();
    /// builder.add_conditional_edges("agent", |state| async move {
    ///     let needs_tools = state.get("messages")
    ///         .and_then(|m| m.as_array())
    ///         .and_then(|a| a.last())
    ///         .map(|m| m.get("tool_calls").is_some())
    ///         .unwrap_or(false);
    ///     Ok(if needs_tools { Route::Node("tools".into()) } else { Route::End })
    /// });
    /// ```
    pub fn add_conditional_edges<F, Fut>(&mut self, from: impl Into<String>, router: F) -> &mut Self
    where
        F: Fn(State) -> Fut + std::marker::Send + Sync + 'static,
        Fut: Future<Output = Result<Route>> + std::marker::Send + 'static,
    {
        let router: ConditionalRouter = Arc::new(move |state| Box::pin(router(state)));
        self.edges.push(Edge::Conditional {
            from: from.into(),
            router,
        });
        self
    }

    /// Set the entry-point node (the LangGraph `START` edge).
    pub fn set_entry_point(&mut self, name: impl Into<String>) -> &mut Self {
        self.entry_point = Some(name.into());
        self
    }

    /// Validate and freeze the graph.
    ///
    /// Validation (all failures are [`RustyError::Graph`]):
    ///
    /// - at least one node is registered;
    /// - the entry point is set and references a known node;
    /// - every edge endpoint (`from`, and `to` for static edges) references
    ///   a known node;
    /// - no node is registered under a reserved name ([`END`] or any
    ///   `__`-prefixed name);
    /// - no duplicate static `from → to` edge (a duplicate would activate
    ///   the target twice in one super-step, which surfaces as a spurious
    ///   double-write failure on single-write channels);
    /// - at most one conditional edge per source node;
    /// - no source node mixes static and conditional edges.
    ///
    /// Conditional router targets and [`Send`] node names are validated at
    /// execution time (they are data-dependent by design).
    pub fn compile(self) -> Result<Graph> {
        if self.nodes.is_empty() {
            return Err(RustyError::Graph(
                "cannot compile an empty graph: register at least one node".into(),
            ));
        }

        for name in self.nodes.keys() {
            if name == END || name.starts_with("__") {
                return Err(RustyError::Graph(format!(
                    "node name `{name}` is reserved (`{END}` and `__`-prefixed names \
                     form the engine's sentinel namespace)"
                )));
            }
        }

        let entry_point = self.entry_point.ok_or_else(|| {
            RustyError::Graph("no entry point set: call set_entry_point()".into())
        })?;
        if !self.nodes.contains_key(&entry_point) {
            return Err(RustyError::Graph(format!(
                "entry point `{entry_point}` does not reference a known node"
            )));
        }

        let mut direct_edges: HashSet<(&str, &str)> = HashSet::new();
        let mut direct_sources: HashSet<&str> = HashSet::new();
        let mut conditional_sources: HashSet<&str> = HashSet::new();

        for edge in &self.edges {
            let from = edge.from();
            if !self.nodes.contains_key(from) {
                return Err(RustyError::Graph(format!(
                    "edge source `{from}` does not reference a known node"
                )));
            }
            match edge {
                Edge::Direct { to, .. } => {
                    if !self.nodes.contains_key(to) {
                        return Err(RustyError::Graph(format!(
                            "edge target `{to}` (from `{from}`) does not reference a known node"
                        )));
                    }
                    if !direct_edges.insert((from, to.as_str())) {
                        return Err(RustyError::Graph(format!(
                            "duplicate edge `{from} -> {to}`: the target would be \
                             activated twice in one super-step"
                        )));
                    }
                    direct_sources.insert(from);
                }
                Edge::Conditional { .. } => {
                    if !conditional_sources.insert(from) {
                        return Err(RustyError::Graph(format!(
                            "node `{from}` has multiple conditional edges; only one \
                             router per source node is allowed"
                        )));
                    }
                }
            }
        }

        if let Some(from) = direct_sources.intersection(&conditional_sources).next() {
            return Err(RustyError::Graph(format!(
                "node `{from}` has both static and conditional edges; routing would \
                 be ambiguous — use one kind per source node"
            )));
        }

        Ok(Graph {
            inner: Arc::new(GraphInner {
                nodes: self.nodes,
                edges: self.edges,
                entry_point,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutput;
    use serde_json::json;

    async fn ok_node(_ctx: crate::node::NodeContext) -> Result<NodeOutput> {
        Ok(NodeOutput::empty())
    }

    #[test]
    fn compile_rejects_empty_graph() {
        let err = GraphBuilder::new().compile().unwrap_err();
        assert!(matches!(err, RustyError::Graph(_)));
    }

    #[test]
    fn compile_rejects_missing_entry_point() {
        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        let err = b.compile().unwrap_err();
        assert!(matches!(err, RustyError::Graph(_)));
    }

    #[test]
    fn compile_rejects_unknown_entry_point() {
        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        b.set_entry_point("nope");
        assert!(b.compile().is_err());
    }

    #[test]
    fn compile_rejects_dangling_edges() {
        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        b.set_entry_point("a");
        b.add_edge("a", "ghost");
        assert!(b.compile().is_err());

        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        b.set_entry_point("a");
        b.add_conditional_edges("ghost", |_s| async { Ok(Route::End) });
        assert!(b.compile().is_err());
    }

    #[tokio::test]
    async fn compile_accepts_valid_graph() {
        let mut b = GraphBuilder::new();
        b.add_node("agent", ok_node);
        b.add_node("tools", ok_node);
        b.set_entry_point("agent");
        b.add_edge("tools", "agent");
        b.add_conditional_edges("agent", |state| async move {
            Ok(if state.contains("done") {
                Route::End
            } else {
                Route::Node("tools".into())
            })
        });
        let graph = b.compile().unwrap();
        assert_eq!(graph.entry_point(), "agent");
        assert_eq!(graph.node_count(), 2);
        assert!(graph.has_node("tools"));
        assert_eq!(graph.outgoing_edges("agent").len(), 1);
        assert_eq!(graph.outgoing_edges("tools").len(), 1);

        // Clones share the same frozen internals.
        let g2 = graph.clone();
        assert!(g2.node("agent").is_some());

        // Router is callable.
        let edges = graph.outgoing_edges("agent");
        let route = match edges[0] {
            Edge::Conditional { router, .. } => {
                router(State::from_value(json!({"done": true})).unwrap()).await
            }
            _ => panic!("expected conditional edge"),
        };
        assert_eq!(route.unwrap(), Route::End);
    }

    #[test]
    fn compile_rejects_duplicate_direct_edges() {
        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        b.add_node("b", ok_node);
        b.set_entry_point("a");
        b.add_edge("a", "b");
        b.add_edge("a", "b");
        let err = b.compile().unwrap_err();
        assert!(matches!(err, RustyError::Graph(_)));
    }

    #[test]
    fn compile_rejects_duplicate_conditional_edges() {
        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        b.set_entry_point("a");
        b.add_conditional_edges("a", |_s| async { Ok(Route::End) });
        b.add_conditional_edges("a", |_s| async { Ok(Route::End) });
        let err = b.compile().unwrap_err();
        assert!(matches!(err, RustyError::Graph(_)));
    }

    #[test]
    fn compile_rejects_mixed_static_and_conditional_edges() {
        let mut b = GraphBuilder::new();
        b.add_node("a", ok_node);
        b.add_node("b", ok_node);
        b.set_entry_point("a");
        b.add_edge("a", "b");
        b.add_conditional_edges("a", |_s| async { Ok(Route::End) });
        let err = b.compile().unwrap_err();
        assert!(matches!(err, RustyError::Graph(_)));
    }

    #[test]
    fn compile_rejects_reserved_node_names() {
        for name in [END, "__internal"] {
            let mut b = GraphBuilder::new();
            b.add_node(name, ok_node);
            b.set_entry_point(name);
            let err = b.compile().unwrap_err();
            assert!(matches!(err, RustyError::Graph(_)), "name: {name}");
        }
    }
}
