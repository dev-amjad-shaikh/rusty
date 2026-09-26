//! Integration tests: end-to-end graph execution through the public API.
//!
//! Covers:
//! (a) linear 3-node graph executes in order and produces expected final state;
//! (b) diamond graph (fan-out + fan-in) merges parallel updates via
//!     Append/AddMessages reducers;
//! (c) conditional routing takes the correct branch based on state.
//!
//! No network access; all nodes are pure closures over shared state.

use std::sync::{Arc, Mutex};

use rusty_agent_runtime::prelude::*;
use serde_json::json;

/// Shared, thread-safe record of node invocations (`name@step`), used to
/// verify execution order across super-steps.
#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);

impl Trace {
    fn record(&self, entry: String) {
        self.0.lock().expect("trace lock").push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().expect("trace lock").clone()
    }
}

// ---------------------------------------------------------------------------
// (a) Linear 3-node graph: order + final state.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linear_graph_executes_in_order_and_produces_final_state() {
    let trace = Trace::default();
    let spec = StateSpec::new()
        .channel("trace", Reducer::Append)
        .channel("result", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    for name in ["a", "b", "c"] {
        let trace = trace.clone();
        builder.add_node(name, move |ctx: NodeContext| {
            let trace = trace.clone();
            async move {
                trace.record(format!("{name}@{}", ctx.step()));
                let out = NodeOutput::update("trace", json!(name));
                Ok(if name == "c" {
                    out.with_update("result", json!("done-by-c"))
                } else {
                    out
                })
            }
        });
    }
    builder.set_entry_point("a");
    builder.add_edge("a", "b");
    builder.add_edge("b", "c");
    let graph = builder.compile().expect("valid linear graph compiles");

    let outcome = Executor::new()
        .run(&graph, &spec, State::new(), RunConfig::new("t-linear"))
        .await
        .expect("linear run succeeds");

    assert!(
        matches!(outcome, ExecutionOutcome::Done(_)),
        "linear graph must terminate with Done, got {outcome:?}"
    );
    let state = outcome.state();

    // Nodes ran one per super-step, in order.
    assert_eq!(trace.entries(), vec!["a@0", "b@1", "c@2"]);

    // Sequential Append writes preserve order.
    assert_eq!(state.get("trace"), Some(&json!(["a", "b", "c"])));

    // Last node's Overwrite write survived.
    assert_eq!(state.get("result"), Some(&json!("done-by-c")));
}

// ---------------------------------------------------------------------------
// (b) Diamond graph: fan-out + fan-in through multi-write reducers.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diamond_graph_merges_parallel_updates_via_reducers() {
    let trace = Trace::default();
    let join_count = Arc::new(Mutex::new(0usize));

    let spec = StateSpec::new()
        .channel("log", Reducer::Append)
        .channel("messages", Reducer::AddMessages)
        .channel("joined", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();

    {
        let trace = trace.clone();
        builder.add_node("start", move |ctx: NodeContext| {
            let trace = trace.clone();
            async move {
                trace.record(format!("start@{}", ctx.step()));
                Ok(NodeOutput::update("log", json!("start")))
            }
        });
    }

    // Parallel branches: each writes to the Append channel AND the
    // AddMessages channel in the same super-step.
    for (name, msg_id) in [("b", "mb"), ("c", "mc")] {
        let trace = trace.clone();
        builder.add_node(name, move |ctx: NodeContext| {
            let trace = trace.clone();
            async move {
                trace.record(format!("{name}@{}", ctx.step()));
                Ok(NodeOutput::update("log", json!(name)).with_update(
                    "messages",
                    json!({"id": msg_id, "role": "assistant", "content": format!("from-{name}")}),
                ))
            }
        });
    }

    {
        let trace = trace.clone();
        let join_count = join_count.clone();
        builder.add_node("join", move |ctx: NodeContext| {
            let trace = trace.clone();
            let join_count = join_count.clone();
            async move {
                trace.record(format!("join@{}", ctx.step()));
                *join_count.lock().expect("join count lock") += 1;
                // The join node observes the merged post-barrier state.
                let log_len = ctx
                    .state()
                    .get("log")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                assert_eq!(log_len, 3, "join must see start + both branch writes");
                Ok(NodeOutput::update("joined", json!(true)).with_update("log", json!("join")))
            }
        });
    }

    builder.set_entry_point("start");
    builder.add_edge("start", "b");
    builder.add_edge("start", "c");
    builder.add_edge("b", "join");
    builder.add_edge("c", "join");
    let graph = builder.compile().expect("valid diamond graph compiles");

    let outcome = Executor::new()
        .run(&graph, &spec, State::new(), RunConfig::new("t-diamond"))
        .await
        .expect("diamond run succeeds");

    assert!(matches!(outcome, ExecutionOutcome::Done(_)));
    let state = outcome.state();

    // Both branches ran exactly once in the same super-step, after start and
    // before join (branch order between b and c is intentionally not fixed).
    let entries = trace.entries();
    assert_eq!(entries.len(), 4, "start + b + c + join, each once");
    assert_eq!(entries[0], "start@0");
    assert_eq!(entries[3], "join@2");
    let mut branches = entries[1..3].to_vec();
    branches.sort();
    assert_eq!(branches, vec!["b@1", "c@1"]);

    // The fan-in deduplicated to a single join invocation.
    assert_eq!(*join_count.lock().expect("join count lock"), 1);

    // Append merged every write (start, b, c, join).
    let log = state
        .get("log")
        .and_then(|v| v.as_array())
        .expect("log array");
    assert_eq!(log.len(), 4);
    assert_eq!(log.first(), Some(&json!("start")));
    assert_eq!(log.last(), Some(&json!("join")));
    let mut branch_writes = log[1..3].to_vec();
    branch_writes.sort_by_key(|v| v.as_str().unwrap_or_default().to_owned());
    assert_eq!(branch_writes, vec![json!("b"), json!("c")]);

    // AddMessages merged both parallel message writes, keyed by id.
    let messages = state
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(messages.len(), 2);
    let mut ids: Vec<&str> = messages
        .iter()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["mb", "mc"]);

    // Join node's Overwrite write survived.
    assert_eq!(state.get("joined"), Some(&json!(true)));
}

// ---------------------------------------------------------------------------
// (c) Conditional routing: the router's decision selects the branch.
// ---------------------------------------------------------------------------

/// Builds `classify -> (score >= 0.5 ? high : low)` and runs it with the
/// given score seeded into the initial state. Returns the final state and
/// the invocation trace.
async fn run_routed_graph(score: f64) -> (State, Vec<String>) {
    let trace = Trace::default();
    let spec = StateSpec::new()
        .channel("score", Reducer::Overwrite)
        .channel("classified", Reducer::Overwrite)
        .channel("result", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    {
        let trace = trace.clone();
        builder.add_node("classify", move |ctx: NodeContext| {
            let trace = trace.clone();
            async move {
                trace.record("classify".to_string());
                let seen = ctx.state().get("score").cloned().unwrap_or(json!(0.0));
                Ok(NodeOutput::update("classified", json!(true)).with_update("score", seen))
            }
        });
    }
    for name in ["high", "low"] {
        let trace = trace.clone();
        builder.add_node(name, move |_ctx: NodeContext| {
            let trace = trace.clone();
            async move {
                trace.record(name.to_string());
                Ok(NodeOutput::update("result", json!(name)))
            }
        });
    }
    builder.set_entry_point("classify");
    builder.add_conditional_edges("classify", |state: State| async move {
        let score = state.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        Ok(if score >= 0.5 {
            Route::Node("high".to_string())
        } else {
            Route::Node("low".to_string())
        })
    });
    let graph = builder.compile().expect("valid routed graph compiles");

    let initial = State::from_value(json!({"score": score})).expect("object state");
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            initial,
            RunConfig::new(format!("t-route-{score}")),
        )
        .await
        .expect("routed run succeeds");

    assert!(matches!(outcome, ExecutionOutcome::Done(_)));
    (outcome.state().clone(), trace.entries())
}

#[tokio::test]
async fn conditional_routing_takes_high_branch() {
    let (state, trace) = run_routed_graph(0.9).await;
    assert_eq!(state.get("result"), Some(&json!("high")));
    assert_eq!(trace, vec!["classify", "high"]);
}

#[tokio::test]
async fn conditional_routing_takes_low_branch() {
    let (state, trace) = run_routed_graph(0.1).await;
    assert_eq!(state.get("result"), Some(&json!("low")));
    assert_eq!(trace, vec!["classify", "low"]);
}
