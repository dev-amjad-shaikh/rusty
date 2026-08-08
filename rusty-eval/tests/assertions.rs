//! Assertion unit tests against fabricated event streams: every assertion
//! kind, pass and fail, plus expectation-to-assertion conversion.

use serde_json::{json, Map, Value};

use rusty_eval::{
    Assertion, EvalCase, Expectation, ExpectedToolCall, RunEvidence, RunStatus, StatePredicate,
    ToolCallRecord,
};
use rusty_agent_runtime::record::EventStatus;

/// Fabricate evidence: tool names in order (args `{}`), plus final state,
/// latency, and cost.
fn evidence(names: &[&str], final_state: Value, latency_ms: u64, cost_usd: f64) -> RunEvidence {
    RunEvidence {
        status: RunStatus::Done,
        tool_calls: names
            .iter()
            .enumerate()
            .map(|(index, name)| ToolCallRecord {
                seq: index as u64,
                name: (*name).to_owned(),
                arguments: json!({}),
                latency_ms: None,
                cost_usd: None,
                status: EventStatus::Ok,
            })
            .collect(),
        final_state,
        latency_ms,
        cost_usd,
        total_tokens: 0,
    }
}

fn with_args(mut evidence: RunEvidence, index: usize, args: Value) -> RunEvidence {
    evidence.tool_calls[index].arguments = args;
    evidence
}

// ---------- tool_call_order ----------

#[test]
fn tool_call_order_matches_as_subsequence() {
    let ev = evidence(&["search", "calculator", "search", "email"], json!({}), 0, 0.0);
    let assertion = Assertion::ToolCallOrder {
        expected: vec![
            ExpectedToolCall::named("calculator"),
            ExpectedToolCall::named("email"),
        ],
    };
    let result = assertion.evaluate(&ev);
    assert!(result.passed, "{:?}", result.detail);
    assert_eq!(result.assertion, "tool_call_order");
    assert!(result.detail.is_none());
}

#[test]
fn tool_call_order_fails_on_wrong_order() {
    let ev = evidence(&["email", "calculator"], json!({}), 0, 0.0);
    let assertion = Assertion::ToolCallOrder {
        expected: vec![
            ExpectedToolCall::named("calculator"),
            ExpectedToolCall::named("email"),
        ],
    };
    let result = assertion.evaluate(&ev);
    assert!(!result.passed);
    let detail = result.detail.unwrap();
    assert!(detail.contains("`email`"), "{detail}");
    assert_eq!(result.observed, json!(["email", "calculator"]));
}

#[test]
fn tool_call_order_fails_on_missing_call() {
    let ev = evidence(&["calculator"], json!({}), 0, 0.0);
    let assertion = Assertion::ToolCallOrder {
        expected: vec![ExpectedToolCall::named("search")],
    };
    let result = assertion.evaluate(&ev);
    assert!(!result.passed);
    assert!(result.detail.unwrap().contains("never appears"));
}

#[test]
fn tool_call_order_honors_argument_matchers() {
    let ev = with_args(
        evidence(&["calculator"], json!({}), 0, 0.0),
        0,
        json!({"op": "add", "a": 2}),
    );
    let mut matcher = Map::new();
    matcher.insert("/op".to_owned(), json!("add"));
    let assertion = Assertion::ToolCallOrder {
        expected: vec![ExpectedToolCall {
            name: "calculator".to_owned(),
            args: matcher,
        }],
    };
    assert!(assertion.evaluate(&ev).passed);

    let mut wrong = Map::new();
    wrong.insert("/op".to_owned(), json!("mul"));
    let failing = Assertion::ToolCallOrder {
        expected: vec![ExpectedToolCall {
            name: "calculator".to_owned(),
            args: wrong,
        }],
    };
    let result = failing.evaluate(&ev);
    assert!(!result.passed);
    assert!(result.detail.unwrap().contains("arguments"));
}

#[test]
fn tool_call_order_skips_name_matches_with_wrong_args() {
    // First `calculator` has wrong args, second satisfies the matcher:
    // subsequence matching must keep scanning, not fail on the near-miss.
    let mut ev = evidence(&["calculator", "calculator"], json!({}), 0, 0.0);
    ev.tool_calls[0].arguments = json!({"op": "mul"});
    ev.tool_calls[1].arguments = json!({"op": "add"});
    let mut matcher = Map::new();
    matcher.insert("/op".to_owned(), json!("add"));
    let assertion = Assertion::ToolCallOrder {
        expected: vec![ExpectedToolCall {
            name: "calculator".to_owned(),
            args: matcher,
        }],
    };
    assert!(assertion.evaluate(&ev).passed);
}

// ---------- tool_call_count ----------

#[test]
fn tool_call_count_exact() {
    let ev = evidence(&["search", "calculator", "search"], json!({}), 0, 0.0);
    let assertion = Assertion::ToolCallCount {
        name: "search".to_owned(),
        expected: 2,
    };
    let result = assertion.evaluate(&ev);
    assert!(result.passed, "{:?}", result.detail);
    assert_eq!(result.assertion, "tool_call_count[search]");

    let failing = Assertion::ToolCallCount {
        name: "search".to_owned(),
        expected: 1,
    };
    let result = failing.evaluate(&ev);
    assert!(!result.passed);
    assert_eq!(result.observed, json!({ "count": 2 }));
}

// ---------- state predicate ----------

#[test]
fn state_predicate_matches_by_json_pointer() {
    let state = json!({"answer": 5, "messages": [{"role": "user"}, {"role": "assistant"}]});
    let ev = evidence(&[], state, 0, 0.0);

    let assertion = Assertion::StatePredicate {
        pointer: "/answer".to_owned(),
        expected: json!(5),
    };
    assert!(assertion.evaluate(&ev).passed);

    let nested = Assertion::StatePredicate {
        pointer: "/messages/1/role".to_owned(),
        expected: json!("assistant"),
    };
    assert!(nested.evaluate(&ev).passed);
}

#[test]
fn state_predicate_fails_on_mismatch_and_missing_pointer() {
    let ev = evidence(&[], json!({"answer": 5}), 0, 0.0);

    let mismatch = Assertion::StatePredicate {
        pointer: "/answer".to_owned(),
        expected: json!(6),
    };
    let result = mismatch.evaluate(&ev);
    assert!(!result.passed);
    assert_eq!(result.observed, json!(5));

    let missing = Assertion::StatePredicate {
        pointer: "/score".to_owned(),
        expected: json!(1),
    };
    let result = missing.evaluate(&ev);
    assert!(!result.passed);
    assert_eq!(result.observed, Value::Null);
    assert!(result.detail.unwrap().contains("not present"));
}

// ---------- no_tool_call ----------

#[test]
fn no_tool_call_blacklist() {
    let ev = evidence(&["search", "calculator"], json!({}), 0, 0.0);
    let clean = Assertion::NoToolCall {
        names: vec!["shell".to_owned()],
    };
    assert!(clean.evaluate(&ev).passed);

    let breached = Assertion::NoToolCall {
        names: vec!["shell".to_owned(), "calculator".to_owned()],
    };
    let result = breached.evaluate(&ev);
    assert!(!result.passed);
    assert_eq!(result.observed, json!({ "called": ["calculator"] }));
}

// ---------- cost / latency bounds ----------

#[test]
fn max_cost_bound() {
    let ev = evidence(&[], json!({}), 0, 0.0042);
    let assertion = Assertion::MaxCost { usd: 0.01 };
    assert!(assertion.evaluate(&ev).passed);

    let tight = Assertion::MaxCost { usd: 0.001 };
    let result = tight.evaluate(&ev);
    assert!(!result.passed);
    assert!(result.detail.unwrap().contains("exceeds"));
}

#[test]
fn max_latency_bound_is_inclusive() {
    let ev = evidence(&[], json!({}), 250, 0.0);
    let assertion = Assertion::MaxLatency { ms: 250 };
    assert!(assertion.evaluate(&ev).passed);

    let tight = Assertion::MaxLatency { ms: 249 };
    assert!(!tight.evaluate(&ev).passed);
}

// ---------- expectation conversion ----------

#[test]
fn expectation_converts_to_named_assertions() {
    let case = EvalCase {
        id: "c".to_owned(),
        input: json!({}),
        expect: Expectation {
            tool_trajectory: vec![ExpectedToolCall::named("calculator")],
            state: vec![
                StatePredicate {
                    pointer: "/a".to_owned(),
                    expected: json!(1),
                },
                StatePredicate {
                    pointer: "/b".to_owned(),
                    expected: json!(2),
                },
            ],
            forbid_tools: vec!["shell".to_owned()],
            max_cost_usd: Some(0.01),
            max_latency_ms: Some(1_000),
        },
        tags: vec![],
    };
    let names: Vec<String> = case
        .expect
        .assertions()
        .iter()
        .map(|assertion| assertion.name())
        .collect();
    assert_eq!(
        names,
        vec![
            "tool_call_order",
            "state[/a]",
            "state[/b]",
            "no_tool_call",
            "max_cost",
            "max_latency"
        ]
    );
}

#[test]
fn empty_expectation_yields_no_assertions() {
    assert!(Expectation::default().is_empty());
    assert!(Expectation::default().assertions().is_empty());
}
