//! Integration tests: reducer merge semantics and the LastValue single-write
//! rule, exercised both directly through `StateSpec` and end-to-end through
//! the executor.
//!
//! Covers:
//! (d) LastValue-style single-write violation returns `InvalidUpdate`;
//!     plus the fan-in-friendly reducers (Append / DeepMerge / AddMessages)
//!     as the sanctioned alternatives.

use std::collections::HashMap;

use rusty_agent_runtime::prelude::*;
use serde_json::json;

fn updates(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// (d) LastValue single-write violation — spec level.
// ---------------------------------------------------------------------------

#[test]
fn last_value_double_write_in_one_super_step_is_invalid_update() {
    let mut state = State::new();
    let spec = StateSpec::new().channel("x", Reducer::Overwrite);
    let writes = vec![
        ("node_a".to_string(), updates(&[("x", json!(1))])),
        ("node_b".to_string(), updates(&[("x", json!(2))])),
    ];
    let err = spec
        .apply_super_step(&mut state, writes)
        .expect_err("second Overwrite write in one super-step must fail");
    assert!(matches!(err, RustyError::InvalidUpdate(_)));
    // The error message should name the channel to be actionable.
    assert!(err.to_string().contains('x'), "error names channel: {err}");
}

#[test]
fn write_to_undeclared_channel_is_invalid_update() {
    let mut state = State::new();
    let spec = StateSpec::new().channel("x", Reducer::Overwrite);
    let err = spec
        .apply_single(&mut state, "n", updates(&[("ghost", json!(1))]))
        .expect_err("undeclared channel write must fail");
    assert!(matches!(err, RustyError::InvalidUpdate(_)));
}

#[test]
fn sequential_overwrite_writes_across_super_steps_are_fine() {
    let mut state = State::new();
    let spec = StateSpec::new().channel("x", Reducer::Overwrite);
    // One write per super-step: legal, latest wins.
    spec.apply_single(&mut state, "a", updates(&[("x", json!(1))]))
        .unwrap();
    spec.apply_single(&mut state, "b", updates(&[("x", json!(2))]))
        .unwrap();
    assert_eq!(state.get("x"), Some(&json!(2)));
}

// ---------------------------------------------------------------------------
// (d) LastValue single-write violation — end-to-end through the executor.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn executor_surfaces_invalid_update_on_parallel_overwrite_conflict() {
    // Diamond where BOTH parallel branches write the same Overwrite channel
    // in the same super-step: the barrier merge must fail the run.
    let spec = StateSpec::new().channel("x", Reducer::Overwrite);

    let mut builder = GraphBuilder::new();
    builder.add_node("start", |_ctx: NodeContext| async {
        Ok(NodeOutput::empty())
    });
    for name in ["b", "c"] {
        builder.add_node(name, move |_ctx: NodeContext| async move {
            Ok(NodeOutput::update("x", json!(name)))
        });
    }
    builder.set_entry_point("start");
    builder.add_edge("start", "b");
    builder.add_edge("start", "c");
    let graph = builder.compile().expect("graph structure is valid");

    let err = Executor::new()
        .run(&graph, &spec, State::new(), RunConfig::new("t-conflict"))
        .await
        .expect_err("parallel Overwrite conflict must fail the run");
    assert!(
        matches!(err, RustyError::InvalidUpdate(_)),
        "expected InvalidUpdate, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Fan-in-friendly reducers (the fix for the LastValue failure mode).
// ---------------------------------------------------------------------------

#[test]
fn append_merges_parallel_writes_into_one_array() {
    let mut state = State::new();
    let spec = StateSpec::new().channel("xs", Reducer::Append);
    let writes = vec![
        ("a".to_string(), updates(&[("xs", json!([1, 2]))])),
        ("b".to_string(), updates(&[("xs", json!(3))])),
    ];
    spec.apply_super_step(&mut state, writes).unwrap();
    assert_eq!(state.get("xs"), Some(&json!([1, 2, 3])));
}

#[test]
fn deep_merge_combines_parallel_object_writes() {
    let mut state = State::from_value(json!({"cfg": {"base": true}})).unwrap();
    let spec = StateSpec::new().channel("cfg", Reducer::DeepMerge);
    let writes = vec![
        ("a".to_string(), updates(&[("cfg", json!({"from_a": 1}))])),
        ("b".to_string(), updates(&[("cfg", json!({"from_b": 2}))])),
    ];
    spec.apply_super_step(&mut state, writes).unwrap();
    assert_eq!(
        state.get("cfg"),
        Some(&json!({"base": true, "from_a": 1, "from_b": 2}))
    );
}

#[test]
fn add_messages_appends_and_upserts_by_id() {
    let mut state = State::from_value(json!({
        "messages": [{"id": "m1", "content": "old"}]
    }))
    .unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);

    // Parallel writes: one upserts m1, one appends a new message.
    let writes = vec![
        (
            "a".to_string(),
            updates(&[("messages", json!({"id": "m1", "content": "new"}))]),
        ),
        (
            "b".to_string(),
            updates(&[("messages", json!({"id": "m2", "content": "appended"}))]),
        ),
    ];
    spec.apply_super_step(&mut state, writes).unwrap();

    let messages = state
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(
        messages.len(),
        2,
        "upsert replaces in place, append adds one"
    );
    assert_eq!(messages[0], json!({"id": "m1", "content": "new"}));
    assert_eq!(messages[1], json!({"id": "m2", "content": "appended"}));
}
