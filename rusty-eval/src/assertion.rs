//! Deterministic assertions over run evidence.
//!
//! An [`Assertion`] is a closed set of checks — no heuristics, no model in
//! the loop — evaluated against a recorded run's [`RunEvidence`]. Every
//! evaluation returns an [`AssertionResult`] carrying the evidence of the
//! verdict: what was expected, what was observed, and a human-readable
//! detail on failure. Results are serializable, so reports preserve exactly
//! why a run failed, not just that it did.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::dataset::ExpectedToolCall;
use crate::evidence::RunEvidence;

/// One deterministic check over a run's evidence.
///
/// Serialized with an `assertion` tag so assertions can live in config
/// files and reports: `{"assertion":"tool_call_count","name":"calculator","expected":2}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "assertion", rename_all = "snake_case")]
pub enum Assertion {
    /// The expected tool calls appear in the observed trajectory as an
    /// ordered subsequence (extra calls in between are allowed), each
    /// satisfying its argument matchers.
    ToolCallOrder {
        /// The expected subsequence, in order.
        expected: Vec<ExpectedToolCall>,
    },

    /// The tool `name` was called exactly `expected` times.
    ToolCallCount {
        /// Tool name to count.
        name: String,
        /// Exact expected call count.
        expected: usize,
    },

    /// The run's final state has `expected` at `pointer` (JSON pointer,
    /// RFC 6901 — e.g. `/messages/3/content`).
    StatePredicate {
        /// JSON pointer into the final state object.
        pointer: String,
        /// Expected value, compared with `==`.
        expected: Value,
    },

    /// None of the blacklisted tools were called.
    NoToolCall {
        /// Forbidden tool names.
        names: Vec<String>,
    },

    /// The run's total journaled cost is at most `usd`.
    MaxCost {
        /// Cost ceiling in USD.
        usd: f64,
    },

    /// The run's wall latency is at most `ms`.
    MaxLatency {
        /// Latency ceiling in milliseconds.
        ms: u64,
    },
}

impl Assertion {
    /// The stable report key for this assertion. Aggregation groups results
    /// by it, so it includes the parameters that distinguish two assertions
    /// of the same kind (`state[/answer]` vs `state[/score]`).
    pub fn name(&self) -> String {
        match self {
            Assertion::ToolCallOrder { .. } => "tool_call_order".to_owned(),
            Assertion::ToolCallCount { name, .. } => format!("tool_call_count[{name}]"),
            Assertion::StatePredicate { pointer, .. } => format!("state[{pointer}]"),
            Assertion::NoToolCall { .. } => "no_tool_call".to_owned(),
            Assertion::MaxCost { .. } => "max_cost".to_owned(),
            Assertion::MaxLatency { .. } => "max_latency".to_owned(),
        }
    }

    /// Evaluate against one run's evidence. Pure: same evidence, same verdict.
    pub fn evaluate(&self, evidence: &RunEvidence) -> AssertionResult {
        match self {
            Assertion::ToolCallOrder { expected } => eval_tool_call_order(expected, evidence),
            Assertion::ToolCallCount { name, expected } => {
                let observed = evidence
                    .tool_calls
                    .iter()
                    .filter(|call| &call.name == name)
                    .count();
                AssertionResult {
                    assertion: self.name(),
                    passed: observed == *expected,
                    expected: json!({ "tool": name, "count": expected }),
                    observed: json!({ "count": observed }),
                    detail: (observed != *expected).then(|| {
                        format!("`{name}` was called {observed} time(s), expected {expected}")
                    }),
                }
            }
            Assertion::StatePredicate { pointer, expected } => {
                let observed = evidence.final_state.pointer(pointer);
                let passed = observed == Some(expected);
                AssertionResult {
                    assertion: self.name(),
                    passed,
                    expected: expected.clone(),
                    observed: observed.cloned().unwrap_or(Value::Null),
                    detail: (!passed).then(|| match observed {
                        None => format!("pointer `{pointer}` is not present in the final state"),
                        Some(value) => {
                            format!("value at `{pointer}` is {value}, expected {expected}")
                        }
                    }),
                }
            }
            Assertion::NoToolCall { names } => {
                let offending: Vec<&str> = evidence
                    .tool_calls
                    .iter()
                    .filter(|call| names.contains(&call.name))
                    .map(|call| call.name.as_str())
                    .collect();
                AssertionResult {
                    assertion: self.name(),
                    passed: offending.is_empty(),
                    expected: json!({ "forbidden": names }),
                    observed: json!({ "called": offending }),
                    detail: (!offending.is_empty())
                        .then(|| format!("blacklisted tool(s) called: {}", offending.join(", "))),
                }
            }
            Assertion::MaxCost { usd } => AssertionResult {
                assertion: self.name(),
                passed: evidence.cost_usd <= *usd,
                expected: json!({ "max_usd": usd }),
                observed: json!({ "usd": evidence.cost_usd }),
                detail: (evidence.cost_usd > *usd).then(|| {
                    format!(
                        "run cost ${:.6} exceeds the ${:.6} bound",
                        evidence.cost_usd, usd
                    )
                }),
            },
            Assertion::MaxLatency { ms } => AssertionResult {
                assertion: self.name(),
                passed: evidence.latency_ms <= *ms,
                expected: json!({ "max_ms": ms }),
                observed: json!({ "ms": evidence.latency_ms }),
                detail: (evidence.latency_ms > *ms).then(|| {
                    format!(
                        "run took {} ms, over the {ms} ms bound",
                        evidence.latency_ms
                    )
                }),
            },
        }
    }
}

/// The verdict of one [`Assertion`] on one run, with its evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionResult {
    /// The assertion's report key ([`Assertion::name`]).
    pub assertion: String,

    /// The verdict.
    pub passed: bool,

    /// What was expected (assertion parameters as JSON).
    pub expected: Value,

    /// What the run actually did (the observed quantity as JSON).
    pub observed: Value,

    /// Why it failed, in one sentence. Absent on passes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Subsequence matching for [`Assertion::ToolCallOrder`]: greedy
/// earliest-match, which is optimal for subsequence existence — taking the
/// earliest matching candidate leaves the longest possible suffix for the
/// rest of the pattern.
fn eval_tool_call_order(expected: &[ExpectedToolCall], evidence: &RunEvidence) -> AssertionResult {
    let observed_names = evidence.tool_names();
    let mut cursor = 0_usize;

    for (position, want) in expected.iter().enumerate() {
        let mut matched = false;
        let mut arg_mismatch: Option<String> = None;
        while cursor < evidence.tool_calls.len() {
            let call = &evidence.tool_calls[cursor];
            cursor += 1;
            if call.name == want.name {
                if want.matches_arguments(&call.arguments) {
                    matched = true;
                    break;
                }
                // Remember the nearest near-miss for the failure detail, but
                // keep scanning: a later same-name call may satisfy the args.
                arg_mismatch.get_or_insert_with(|| {
                    format!(
                        "`{}` was called at trajectory position {} but its arguments {} did not \
                         satisfy the matchers",
                        want.name,
                        cursor - 1,
                        call.arguments,
                    )
                });
            }
        }
        if !matched {
            let detail = arg_mismatch.unwrap_or_else(|| {
                format!(
                    "expected call {} (`{}`) never appears at or after trajectory position {}",
                    position + 1,
                    want.name,
                    cursor,
                )
            });
            return AssertionResult {
                assertion: Assertion::ToolCallOrder {
                    expected: expected.to_vec(),
                }
                .name(),
                passed: false,
                expected: json!(expected),
                observed: json!(observed_names),
                detail: Some(detail),
            };
        }
    }

    AssertionResult {
        assertion: Assertion::ToolCallOrder {
            expected: expected.to_vec(),
        }
        .name(),
        passed: true,
        expected: json!(expected),
        observed: json!(observed_names),
        detail: None,
    }
}
