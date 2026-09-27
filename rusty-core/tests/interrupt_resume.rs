//! Integration tests: interrupts, resume, checkpoint recovery, step limits,
//! and the streaming event stream.
//!
//! Covers:
//! (e) interrupt -> `ExecutionOutcome::Interrupted`, then resume via
//!     `RunConfig::resume` + same `thread_id` completes the run
//!     (`InMemoryCheckpointer`);
//! (f) checkpoint recovery: kill-and-resume simulation with
//!     `JsonFileCheckpointer` under a temp dir;
//! (g) `max_steps` guard returns an error on a cyclic graph;
//! (h) `GraphEvent` stream receives NodeStart/NodeEnd/SuperStep events in
//!     order.

use std::path::PathBuf;
use std::sync::Arc;

use rusty_agent_runtime::prelude::*;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// `start -> human`, where `human` interrupts until a resume value arrives.
/// Returns the compiled graph and its state spec.
fn human_in_the_loop_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new()
        .channel("greeting", Reducer::Overwrite)
        .channel("answer", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("start", |_ctx: NodeContext| async {
        Ok(NodeOutput::update("greeting", json!("hello")))
    });
    builder.add_node("human", |ctx: NodeContext| async move {
        match ctx.resume_value() {
            // Resumed: interrupt() logically "returns" the caller's value.
            Some(v) => Ok(NodeOutput::update("answer", v.clone())),
            // First pass: suspend the whole run with a payload.
            None => Err(ctx.interrupt(json!({"question": "approve?"}))),
        }
    });
    builder.set_entry_point("start");
    builder.add_edge("start", "human");
    let graph = builder.compile().expect("HITL graph compiles");
    (graph, spec)
}

// ---------------------------------------------------------------------------
// (e) Interrupt -> resume with InMemoryCheckpointer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_then_resume_completes_run_in_memory() {
    let (graph, spec) = human_in_the_loop_graph();
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());

    // First run: suspends at the `human` node.
    let outcome = executor
        .run(&graph, &spec, State::new(), RunConfig::new("t-hitl"))
        .await
        .expect("interrupt surfaces as Ok(Interrupted), not an error");

    let checkpoint_id = match &outcome {
        ExecutionOutcome::Interrupted {
            value,
            state,
            checkpoint_id,
        } => {
            assert_eq!(value, &json!({"question": "approve?"}));
            // The completed super-step (start) is visible in the state.
            assert_eq!(state.get("greeting"), Some(&json!("hello")));
            assert!(!state.contains("answer"));
            checkpoint_id.clone()
        }
        other => panic!("expected Interrupted, got {other:?}"),
    };
    assert!(outcome.is_interrupted());
    assert!(!checkpoint_id.is_empty());

    // The suspension checkpoint is persisted and schedules the interrupted node.
    let latest = checkpointer
        .get_latest("t-hitl")
        .await
        .expect("get_latest succeeds")
        .expect("a checkpoint exists at the suspension point");
    assert_eq!(latest.thread_id, "t-hitl");
    assert_eq!(latest.next_nodes, vec!["human".to_string()]);

    // Resume: same thread_id + resume value. The interrupted node re-executes
    // from its start and completes the run.
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(), // ignored on resume: checkpoint state wins
            RunConfig::new("t-hitl").with_resume(json!({"approved": true})),
        )
        .await
        .expect("resume run succeeds");

    match &outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(state.get("greeting"), Some(&json!("hello")));
            assert_eq!(state.get("answer"), Some(&json!({"approved": true})));
        }
        other => panic!("expected Done after resume, got {other:?}"),
    }
}

#[tokio::test]
async fn resume_without_checkpoint_is_an_error() {
    let (graph, spec) = human_in_the_loop_graph();
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));

    let err = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-never-ran").with_resume(json!(true)),
        )
        .await
        .expect_err("resume with no prior checkpoint must fail");
    assert!(
        matches!(err, RustyError::Checkpoint(_) | RustyError::Graph(_)),
        "expected Checkpoint/Graph error, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// (f) Kill-and-resume with JsonFileCheckpointer under a temp dir.
// ---------------------------------------------------------------------------

fn fresh_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rusty-test-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn json_file_checkpointer_roundtrip_direct() {
    let dir = fresh_temp_dir("roundtrip");
    let store = JsonFileCheckpointer::new(&dir);

    assert!(store.get_latest("t1").await.unwrap().is_none());
    assert!(store.list("t1").await.unwrap().is_empty());

    let state0 = State::from_value(json!({"step": 0})).unwrap();
    let state1 = State::from_value(json!({"step": 1})).unwrap();
    store
        .put(Checkpoint::new("t1", 0, state0, vec!["a".to_string()]))
        .await
        .unwrap();
    store
        .put(Checkpoint::new("t1", 1, state1, vec!["b".to_string()]))
        .await
        .unwrap();

    let latest = store
        .get_latest("t1")
        .await
        .unwrap()
        .expect("latest exists");
    assert_eq!(latest.step, 1);
    assert_eq!(latest.next_nodes, vec!["b".to_string()]);
    assert_eq!(latest.state.get("step"), Some(&json!(1)));

    let all = store.list("t1").await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].step, 0);
    assert_eq!(all[1].step, 1);

    // One JSON file per checkpoint on disk.
    let files = std::fs::read_dir(dir.join("t1"))
        .expect("thread dir exists")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(files, 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn kill_and_resume_via_json_file_checkpointer() {
    let (graph, spec) = human_in_the_loop_graph();
    let dir = fresh_temp_dir("kill-resume");

    // "Process 1": run until the interrupt, then drop everything (simulated kill).
    {
        let checkpointer = Arc::new(JsonFileCheckpointer::new(&dir));
        let executor = Executor::with_checkpointer(checkpointer);
        let outcome = executor
            .run(&graph, &spec, State::new(), RunConfig::new("t-durable"))
            .await
            .expect("first run suspends cleanly");
        assert!(outcome.is_interrupted(), "first run must interrupt");
        // executor + checkpointer dropped here: only the files on disk remain.
    }

    // "Process 2": brand-new checkpointer instance over the same directory.
    let checkpointer = Arc::new(JsonFileCheckpointer::new(&dir));
    let executor = Executor::with_checkpointer(checkpointer.clone());

    // The durable checkpoint survived the simulated restart.
    let latest = checkpointer
        .get_latest("t-durable")
        .await
        .expect("get_latest succeeds after restart")
        .expect("checkpoint recovered from disk");
    assert_eq!(latest.next_nodes, vec!["human".to_string()]);
    assert_eq!(latest.state.get("greeting"), Some(&json!("hello")));

    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-durable").with_resume(json!({"approved": "after-restart"})),
        )
        .await
        .expect("resume after restart succeeds");

    match &outcome {
        ExecutionOutcome::Done(state) => {
            assert_eq!(state.get("greeting"), Some(&json!("hello")));
            assert_eq!(
                state.get("answer"),
                Some(&json!({"approved": "after-restart"}))
            );
        }
        other => panic!("expected Done after durable resume, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// (g) max_steps guard on a cyclic graph.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_steps_guard_aborts_cyclic_graph() {
    let spec = StateSpec::new().channel("trace", Reducer::Append);

    let mut builder = GraphBuilder::new();
    for name in ["ping", "pong"] {
        builder.add_node(name, move |_ctx: NodeContext| async move {
            Ok(NodeOutput::update("trace", json!(name)))
        });
    }
    builder.set_entry_point("ping");
    builder.add_edge("ping", "pong");
    builder.add_edge("pong", "ping"); // cycle: never terminates on its own
    let graph = builder.compile().expect("cyclic graph compiles");

    // The step ceiling halts the run with the work it has done, resumable
    // from its checkpoint: an interrupt that names the ceiling, not an
    // error that loses the steps.
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-loop").with_max_steps(5),
        )
        .await;
    match outcome {
        Ok(ExecutionOutcome::Interrupted { value, state, .. }) => {
            assert_eq!(
                value["rusty.halted"]["reason"],
                json!("step_ceiling"),
                "{value}"
            );
            assert_eq!(value["rusty.halted"]["steps_run"], json!(5), "{value}");
            assert_eq!(value["rusty.halted"]["limit"], json!(5), "{value}");
            let trace = state
                .get_as::<Value>("trace")
                .ok()
                .flatten()
                .unwrap_or(json!([]));
            assert_eq!(
                trace.as_array().map(|t| t.len()),
                Some(5),
                "five steps ran before the ceiling: {trace}"
            );
        }
        other => panic!("the cyclic graph must halt at the step ceiling, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (h) GraphEvent stream: NodeStart / NodeEnd / SuperStep ordering.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_stream_emits_ordered_lifecycle_events() {
    let spec = StateSpec::new().channel("trace", Reducer::Append);

    let mut builder = GraphBuilder::new();
    for name in ["a", "b"] {
        builder.add_node(name, move |_ctx: NodeContext| async move {
            Ok(NodeOutput::update("trace", json!(name)))
        });
    }
    builder.set_entry_point("a");
    builder.add_edge("a", "b");
    let graph = builder.compile().expect("linear graph compiles");

    let (tx, mut rx) = mpsc::channel(64);
    let executor = Executor::with_checkpointer(Arc::new(InMemoryCheckpointer::new()));
    let outcome = executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-events").with_event_tx(tx),
        )
        .await
        .expect("run succeeds");
    assert!(matches!(outcome, ExecutionOutcome::Done(_)));

    // Drain the stream (sender was dropped when the run finished).
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    assert!(!events.is_empty(), "executor emitted events");

    // The run must open with SuperStep 0 activating the entry point.
    match &events[0] {
        GraphEvent::SuperStep { step, active_nodes } => {
            assert_eq!(*step, 0);
            assert_eq!(active_nodes, &vec!["a".to_string()]);
        }
        other => panic!("first event must be SuperStep, got {other:?}"),
    }

    // SuperStep boundaries arrive in order, one per step.
    let super_steps: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            GraphEvent::SuperStep { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(super_steps, vec![0, 1]);

    // Helper: position of the first event matching a predicate.
    let pos = |pred: &dyn Fn(&GraphEvent) -> bool| -> usize {
        events.iter().position(pred).expect("event present")
    };

    let start_a =
        pos(&|e| matches!(e, GraphEvent::NodeStart { node, step } if node == "a" && *step == 0));
    let end_a =
        pos(&|e| matches!(e, GraphEvent::NodeEnd { node, step } if node == "a" && *step == 0));
    let start_b =
        pos(&|e| matches!(e, GraphEvent::NodeStart { node, step } if node == "b" && *step == 1));
    let end_b =
        pos(&|e| matches!(e, GraphEvent::NodeEnd { node, step } if node == "b" && *step == 1));

    // Lifecycle ordering: each node starts before it ends, and the first
    // super-step fully completes before the second node's lifecycle begins.
    assert!(start_a < end_a, "NodeStart(a) precedes NodeEnd(a)");
    assert!(start_b < end_b, "NodeStart(b) precedes NodeEnd(b)");
    assert!(end_a < start_b, "super-step 0 completes before step 1 runs");

    // Barrier merge produced a StateUpdate for each step that wrote state.
    let update_steps: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            GraphEvent::StateUpdate { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(update_steps, vec![0, 1]);

    // A checkpointer was configured: at least one CheckpointSaved, with
    // non-decreasing step indexes.
    let checkpoint_steps: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            GraphEvent::CheckpointSaved {
                step,
                checkpoint_id,
            } => {
                assert!(!checkpoint_id.is_empty());
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert!(!checkpoint_steps.is_empty(), "checkpoints were announced");
    assert!(
        checkpoint_steps.windows(2).all(|w| w[0] <= w[1]),
        "checkpoint steps are non-decreasing: {checkpoint_steps:?}"
    );
}
