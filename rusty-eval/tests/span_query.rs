//! Span-query integration tests (EP-12-S04): the structural assertion
//! language over execution span trees — the full constraint corpus against
//! synthetic trees (AC 1), golden failure reports (AC 2), one query
//! evaluated identically against a distilled journal tree and a fixture
//! tree of the same shape (AC 3), authoring-time vocabulary validation
//! (AC 4), and log-position determinism under repetition (AC 5).
//!
//! Goldens live under `tests/golden/span_*.json`; re-run with
//! `UPDATE_GOLDEN=1` to bless an intentional contract change and review
//! the diff.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};

use rusty_agent_runtime::executor::{Executor, RunConfig};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall, Usage};
use rusty_agent_runtime::prelude::{
    GraphBuilder, NodeContext, NodeOutput, Reducer, State, StateSpec,
};
use rusty_agent_runtime::react::{MESSAGES_CHANNEL, create_react_agent_with_recording};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};

use rusty_eval::span_query::{
    AttributePredicate, PredicateOp, SPAN_VOCABULARY, SPAN_VOCABULARY_VERSION, SpanConstraint,
    SpanQuery, SpanSelection, evaluate_query,
};
use rusty_eval::trace::{AttributeValue, SpanTree, TraceSpan};

// ---------- golden machinery ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- the fixture tree ----------

/// Build one span: `span-{start}-{end}` with the given attributes.
fn span(name: &str, seqs: (u64, u64), attrs: &[(&str, AttributeValue)]) -> TraceSpan {
    TraceSpan {
        span_id: format!("span-{}-{}", seqs.0, seqs.1),
        name: name.to_string(),
        parent: None,
        start_seq: seqs.0,
        end_seq: seqs.1,
        children: Vec::new(),
        attributes: attrs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn int(n: i64) -> AttributeValue {
    AttributeValue::Integer(n)
}

fn text(s: &str) -> AttributeValue {
    AttributeValue::Text(s.to_string())
}

/// The canonical fixture tree: a run with two super-steps — a planner
/// node with one model call, then a tools node running `search` and
/// `calc` concurrently (overlapping log intervals) followed by a
/// sequential `write_report`, and a closing summarize node.
fn fixture_tree() -> SpanTree {
    SpanTree::from_flat(
        "run-fixture",
        vec![
            span(
                "rusty.run",
                (1, 40),
                &[("thread", text("t-1")), ("events", int(40))],
            ),
            span("rusty.super_step", (2, 9), &[("step", int(0))]),
            span(
                "rusty.node",
                (3, 8),
                &[
                    ("node", text("planner")),
                    ("status", text("ok")),
                    ("latency_ms", int(500)),
                ],
            ),
            span(
                "rusty.model_call",
                (4, 4),
                &[
                    ("model", text("gpt-4o")),
                    ("tokens_total", int(120)),
                    ("latency_ms", int(300)),
                    ("cost_usd", AttributeValue::Float(0.002)),
                    ("status", text("ok")),
                    ("effect", text("read_only")),
                ],
            ),
            span("rusty.super_step", (10, 39), &[("step", int(1))]),
            span(
                "rusty.node",
                (11, 30),
                &[
                    ("node", text("tools")),
                    ("status", text("ok")),
                    ("latency_ms", int(1900)),
                ],
            ),
            span(
                "rusty.tool_call",
                (12, 18),
                &[
                    ("tool", text("search")),
                    ("latency_ms", int(400)),
                    ("status", text("ok")),
                    ("effect", text("read_only")),
                    ("has_receipt", AttributeValue::Bool(true)),
                ],
            ),
            span(
                "rusty.tool_call",
                (14, 20),
                &[
                    ("tool", text("calc")),
                    ("latency_ms", int(250)),
                    ("status", text("ok")),
                    ("effect", text("pure")),
                ],
            ),
            span(
                "rusty.tool_call",
                (25, 26),
                &[
                    ("tool", text("write_report")),
                    ("latency_ms", int(1200)),
                    ("status", text("ok")),
                    ("effect", text("idempotent")),
                    ("has_receipt", AttributeValue::Bool(true)),
                ],
            ),
            span(
                "rusty.node",
                (31, 38),
                &[
                    ("node", text("summarize")),
                    ("status", text("ok")),
                    ("latency_ms", int(800)),
                ],
            ),
            span(
                "rusty.model_call",
                (32, 32),
                &[
                    ("model", text("claude")),
                    ("tokens_total", int(80)),
                    ("status", text("ok")),
                ],
            ),
        ],
    )
}

fn query(select: SpanSelection, constraint: SpanConstraint) -> SpanQuery {
    SpanQuery { select, constraint }
}

fn pred(attribute: &str, op: PredicateOp, value: AttributeValue) -> AttributePredicate {
    AttributePredicate {
        attribute: attribute.to_string(),
        op,
        value,
    }
}

fn tool_named(name: &str) -> SpanSelection {
    SpanSelection {
        name: Some("rusty.tool_call".to_string()),
        predicates: vec![pred("tool", PredicateOp::Eq, text(name))],
    }
}

// ---------- AC 1: the constraint corpus ----------

#[test]
fn exists_and_absent() {
    let tree = fixture_tree();

    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.tool_call"),
            SpanConstraint::Exists,
        ),
    )
    .unwrap();
    assert!(verdict.passed);

    let verdict =
        evaluate_query(&tree, &query(tool_named("shell"), SpanConstraint::Exists)).unwrap();
    assert!(!verdict.passed);
    let failure = verdict.failure.unwrap();
    assert_eq!(failure.clause, "exists");
    assert!(
        !failure.nearest.is_empty(),
        "a miss names the nearest candidates"
    );

    let verdict =
        evaluate_query(&tree, &query(tool_named("shell"), SpanConstraint::Absent)).unwrap();
    assert!(verdict.passed);

    let verdict =
        evaluate_query(&tree, &query(tool_named("search"), SpanConstraint::Absent)).unwrap();
    assert!(!verdict.passed);
    let failure = verdict.failure.unwrap();
    assert_eq!(failure.clause, "absent");
    assert_eq!(failure.matched.len(), 1, "the violating span is listed");
    assert_eq!(failure.matched[0].attributes["tool"], text("search"));
}

#[test]
fn count_within_bounds() {
    let tree = fixture_tree();
    let selection = SpanSelection::named("rusty.tool_call");

    let verdict = evaluate_query(
        &tree,
        &query(
            selection.clone(),
            SpanConstraint::CountWithin { min: 1, max: 3 },
        ),
    )
    .unwrap();
    assert!(verdict.passed);

    let verdict = evaluate_query(
        &tree,
        &query(
            selection.clone(),
            SpanConstraint::CountWithin { min: 4, max: 9 },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    assert_eq!(verdict.failure.unwrap().clause, "count_within");

    let verdict = evaluate_query(
        &tree,
        &query(selection, SpanConstraint::CountWithin { min: 0, max: 2 }),
    )
    .unwrap();
    assert!(!verdict.passed);

    // Step count as a budget: exactly two super-steps ran.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.super_step"),
            SpanConstraint::CountWithin { min: 2, max: 2 },
        ),
    )
    .unwrap();
    assert!(verdict.passed, "step count reads from the tree");
}

#[test]
fn before_requires_unambiguous_log_order() {
    let tree = fixture_tree();

    // search [12,18] closes before write_report [25,26] opens.
    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("search"),
            SpanConstraint::Before {
                other: tool_named("write_report"),
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed);

    // search [12,18] and calc [14,20] overlap: the wall-clock order was
    // never established, so `before` fails — the deliberate case asserts
    // `concurrent_with`.
    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("search"),
            SpanConstraint::Before {
                other: tool_named("calc"),
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    let failure = verdict.failure.unwrap();
    assert_eq!(failure.clause, "before.ordering");
    assert_eq!(failure.matched.len(), 1, "the ambiguous span is named");

    // Ordering needs both sides present.
    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("shell"),
            SpanConstraint::Before {
                other: tool_named("calc"),
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    assert_eq!(verdict.failure.unwrap().clause, "before.selection");
}

#[test]
fn concurrent_with_is_the_deliberate_parallel_assertion() {
    let tree = fixture_tree();

    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("search"),
            SpanConstraint::ConcurrentWith {
                other: tool_named("calc"),
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed, "overlapping log intervals are concurrent");

    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("search"),
            SpanConstraint::ConcurrentWith {
                other: tool_named("write_report"),
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed, "strictly ordered spans are not concurrent");
}

#[test]
fn within_tracks_real_ancestry() {
    let tree = fixture_tree();

    // Every tool call descends from the tools node.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.tool_call"),
            SpanConstraint::Within {
                ancestor: SpanSelection {
                    name: Some("rusty.node".to_string()),
                    predicates: vec![pred("node", PredicateOp::Eq, text("tools"))],
                },
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed);

    // The write_report call is not under the planner node — the orphan
    // is named in the report.
    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("write_report"),
            SpanConstraint::Within {
                ancestor: SpanSelection {
                    name: Some("rusty.node".to_string()),
                    predicates: vec![pred("node", PredicateOp::Eq, text("planner"))],
                },
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    let failure = verdict.failure.unwrap();
    assert_eq!(failure.clause, "within.ancestry");
    assert_eq!(failure.matched.len(), 1);

    // An unknown ancestor selection fails its own clause.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.tool_call"),
            SpanConstraint::Within {
                ancestor: SpanSelection::named("rusty.wasm_call"),
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    assert_eq!(verdict.failure.unwrap().clause, "within.selection");
}

#[test]
fn budgets_aggregate_over_the_selection() {
    let tree = fixture_tree();

    // Total model tokens: 120 + 80 = 200.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.model_call"),
            SpanConstraint::TotalWithin {
                attribute: "tokens_total".to_string(),
                max: 200.0,
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed);

    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.model_call"),
            SpanConstraint::TotalWithin {
                attribute: "tokens_total".to_string(),
                max: 199.0,
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    let failure = verdict.failure.unwrap();
    assert_eq!(failure.clause, "total_within");
    assert!(
        failure.detail.contains("200"),
        "the observed total is stated"
    );

    // Total tool duration: 400 + 250 + 1200 = 1850.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.tool_call"),
            SpanConstraint::TotalWithin {
                attribute: "latency_ms".to_string(),
                max: 2000.0,
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed);
}

#[test]
fn predicates_filter_by_attribute() {
    let tree = fixture_tree();
    let selection = SpanSelection {
        name: Some("rusty.tool_call".to_string()),
        predicates: vec![
            pred("effect", PredicateOp::Eq, text("idempotent")),
            pred("has_receipt", PredicateOp::Eq, AttributeValue::Bool(true)),
        ],
    };
    let verdict = evaluate_query(&tree, &query(selection, SpanConstraint::Exists)).unwrap();
    assert!(verdict.passed, "the receipted idempotent call exists");

    let slow = SpanSelection {
        name: Some("rusty.tool_call".to_string()),
        predicates: vec![pred("latency_ms", PredicateOp::Gt, int(1000))],
    };
    let verdict = evaluate_query(
        &tree,
        &query(slow, SpanConstraint::CountWithin { min: 1, max: 1 }),
    )
    .unwrap();
    assert!(verdict.passed, "only write_report ran longer than 1s");

    let errored = SpanSelection {
        name: None,
        predicates: vec![pred("status", PredicateOp::Eq, text("error"))],
    };
    let verdict = evaluate_query(&tree, &query(errored, SpanConstraint::Absent)).unwrap();
    assert!(verdict.passed);
}

// ---------- AC 2: diagnosable failure reports ----------

#[test]
fn golden_failure_report_names_the_clause_and_the_nearest_spans() {
    let tree = fixture_tree();
    let verdict =
        evaluate_query(&tree, &query(tool_named("shell"), SpanConstraint::Exists)).unwrap();
    assert!(!verdict.passed);
    assert_golden("span_query_failure_exists.json", &verdict);
}

#[test]
fn golden_failure_report_for_an_ambiguous_ordering() {
    let tree = fixture_tree();
    let verdict = evaluate_query(
        &tree,
        &query(
            tool_named("search"),
            SpanConstraint::Before {
                other: tool_named("calc"),
            },
        ),
    )
    .unwrap();
    assert!(!verdict.passed);
    assert_golden("span_query_failure_before.json", &verdict);
}

// ---------- AC 4: authoring-time vocabulary validation ----------

#[test]
fn unknown_attributes_fail_at_authoring_time() {
    let q = query(
        SpanSelection {
            name: Some("rusty.tool_call".to_string()),
            predicates: vec![pred("tool_name", PredicateOp::Eq, text("search"))],
        },
        SpanConstraint::Exists,
    );
    let error = q.validate().unwrap_err().to_string();
    assert!(error.contains("tool_name"));
    assert!(
        error.contains(&format!("v{SPAN_VOCABULARY_VERSION}")),
        "the refusal names the vocabulary version: {error}"
    );

    // The vocabulary is published for suite authors.
    assert!(
        SPAN_VOCABULARY
            .iter()
            .any(|entry| entry.attribute == "tool")
    );
}

#[test]
fn unknown_span_names_fail_at_authoring_time() {
    let q = query(SpanSelection::named("tool.call"), SpanConstraint::Exists);
    let error = q.validate().unwrap_err().to_string();
    assert!(error.contains("tool.call"));
    assert!(
        error.contains("rusty.tool_call"),
        "the published names are listed"
    );
}

#[test]
fn malformed_predicates_and_constraints_fail_at_authoring_time() {
    // An ordering operator on a text attribute.
    let q = query(
        SpanSelection {
            name: Some("rusty.tool_call".to_string()),
            predicates: vec![pred("tool", PredicateOp::Gt, text("search"))],
        },
        SpanConstraint::Exists,
    );
    assert!(q.validate().is_err());

    // A text value against a numeric attribute.
    let q = query(
        SpanSelection {
            name: Some("rusty.tool_call".to_string()),
            predicates: vec![pred("latency_ms", PredicateOp::Eq, text("fast"))],
        },
        SpanConstraint::Exists,
    );
    assert!(q.validate().is_err());

    // A budget over a text attribute.
    let q = query(
        SpanSelection::named("rusty.tool_call"),
        SpanConstraint::TotalWithin {
            attribute: "tool".to_string(),
            max: 10.0,
        },
    );
    assert!(q.validate().is_err());

    // Inverted count bounds.
    let q = query(
        SpanSelection::named("rusty.tool_call"),
        SpanConstraint::CountWithin { min: 5, max: 2 },
    );
    assert!(q.validate().is_err());
}

// ---------- AC 5: determinism ----------

#[test]
fn ordering_verdicts_are_stable_across_repetitions() {
    let tree = fixture_tree();
    let concurrent = query(
        tool_named("search"),
        SpanConstraint::Before {
            other: tool_named("calc"),
        },
    );
    let ordered = query(
        tool_named("search"),
        SpanConstraint::Before {
            other: tool_named("write_report"),
        },
    );
    let baseline_concurrent = evaluate_query(&tree, &concurrent).unwrap();
    let baseline_ordered = evaluate_query(&tree, &ordered).unwrap();
    for _ in 0..20 {
        assert_eq!(
            evaluate_query(&tree, &concurrent).unwrap(),
            baseline_concurrent,
            "concurrent siblings never satisfy `before`"
        );
        assert_eq!(evaluate_query(&tree, &ordered).unwrap(), baseline_ordered);
    }
    assert!(!baseline_concurrent.passed);
    assert!(baseline_ordered.passed);
}

// ---------- AC 3: one language, offline and production shapes ----------

/// A scripted model: pops one canned response per call. Deterministic.
struct ScriptedModel {
    script: Mutex<VecDeque<ChatMessage>>,
}

impl ScriptedModel {
    fn new(script: Vec<ChatMessage>) -> Self {
        Self {
            script: Mutex::new(script.into()),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> rusty_agent_runtime::error::Result<ChatResponse> {
        let message = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("test script is exactly as long as the ReAct loop");
        Ok(ChatResponse {
            message,
            model: Some("scripted-eval-1".into()),
            usage: Some(Usage {
                prompt_tokens: 30,
                completion_tokens: 10,
                total_tokens: 40,
                ..Usage::default()
            }),
        })
    }
}

/// A trivial calculator tool (the experiment tests' fixture shape).
struct CalculatorTool;

#[async_trait::async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "add or multiply two numbers"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["add"]},
                "a": {"type": "number"},
                "b": {"type": "number"},
            },
            "required": ["op", "a", "b"],
        })
    }

    async fn call(&self, args: Value) -> rusty_agent_runtime::error::Result<Value> {
        let a = args["a"].as_f64().unwrap_or(0.0);
        let b = args["b"].as_f64().unwrap_or(0.0);
        let result = match args["op"].as_str() {
            Some("add") => a + b,
            other => panic!("test script only issues add, got {other:?}"),
        };
        Ok(json!(result))
    }
}

/// Run a scripted one-tool ReAct agent against a journaled executor and
/// distill the span tree from the run's journal.
async fn distilled_react_tree() -> SpanTree {
    let journal = Journal::new("run-1", "t-react", Clock::logical(1_000, 10));
    let model = Arc::new(ScriptedModel::new(vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c1",
            "calculator",
            json!({"op": "add", "a": 2, "b": 3}),
        )]),
        ChatMessage::assistant("the answer is 5"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(CalculatorTool);
    let graph = create_react_agent_with_recording(model, tools, journal.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let outcome = Executor::new()
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-react").with_journal(journal.clone()),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            rusty_agent_runtime::executor::ExecutionOutcome::Done(_)
        ),
        "the scripted run completes"
    );
    SpanTree::from_journal(&journal)
}

#[tokio::test]
async fn the_same_query_holds_on_a_distilled_journal_tree() {
    let tree = distilled_react_tree().await;

    // The distilled tree has the full taxonomy: run over super-steps over
    // nodes over the model and tool calls the script made.
    for name in [
        "rusty.run",
        "rusty.super_step",
        "rusty.node",
        "rusty.model_call",
        "rusty.tool_call",
    ] {
        let verdict = evaluate_query(
            &tree,
            &query(SpanSelection::named(name), SpanConstraint::Exists),
        )
        .unwrap();
        assert!(verdict.passed, "{name} span missing from distilled tree");
    }

    // The tool call is attributable, receipt-typed, and descends from a
    // node; the model call that requested it closed before it opened.
    let calculator = tool_named("calculator");
    let verdict =
        evaluate_query(&tree, &query(calculator.clone(), SpanConstraint::Exists)).unwrap();
    assert!(verdict.passed);
    let verdict = evaluate_query(
        &tree,
        &query(
            calculator,
            SpanConstraint::Within {
                ancestor: SpanSelection::named("rusty.node"),
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed, "the tool call runs inside a node span");
    // The loop made exactly two model calls and one tool call, all
    // attributable in the tree.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.model_call"),
            SpanConstraint::CountWithin { min: 2, max: 2 },
        ),
    )
    .unwrap();
    assert!(verdict.passed, "two scripted model calls are journaled");

    // The budget reads journaled usage: two scripted model calls, 40
    // tokens each.
    let verdict = evaluate_query(
        &tree,
        &query(
            SpanSelection::named("rusty.model_call"),
            SpanConstraint::TotalWithin {
                attribute: "tokens_total".to_string(),
                max: 80.0,
            },
        ),
    )
    .unwrap();
    assert!(verdict.passed);

    // AC 3's parity: every query above evaluates identically against a
    // fixture tree of the same shape — verdicts depend on structure, not
    // on which side of the seam the tree came from.
    let fixture = fixture_tree();
    let shape_queries = [
        query(
            SpanSelection::named("rusty.tool_call"),
            SpanConstraint::Exists,
        ),
        query(
            SpanSelection::named("rusty.tool_call"),
            SpanConstraint::Within {
                ancestor: SpanSelection::named("rusty.node"),
            },
        ),
        query(
            SpanSelection::named("rusty.model_call"),
            SpanConstraint::TotalWithin {
                attribute: "tokens_total".to_string(),
                max: 1_000.0,
            },
        ),
    ];
    for shape_query in &shape_queries {
        let on_distilled = evaluate_query(&tree, shape_query).unwrap();
        let on_fixture = evaluate_query(&fixture, shape_query).unwrap();
        assert_eq!(
            on_distilled.passed, on_fixture.passed,
            "query {shape_query:?} disagrees across trace shapes"
        );
    }
}

#[tokio::test]
async fn a_parallel_graph_distills_concurrent_node_spans_deterministically() {
    // Two parallel branches off one entry: the node intervals overlap in
    // log position, so `concurrent_with` holds and `before` refuses —
    // identically across twenty repetitions.
    let build_tree = || async {
        let spec = StateSpec::new().channel("log", Reducer::Append);
        let mut builder = GraphBuilder::new();
        builder.add_node("entry", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("entry")))
        });
        builder.add_node("left", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("left")))
        });
        builder.add_node("right", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("right")))
        });
        builder.set_entry_point("entry");
        builder.add_edge("entry", "left");
        builder.add_edge("entry", "right");
        let graph = builder.compile().unwrap();
        let journal = Journal::new("run-par", "t-par", Clock::logical(0, 1));
        let outcome = Executor::new()
            .run(
                &graph,
                &spec,
                State::new(),
                RunConfig::new("t-par").with_journal(journal.clone()),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                outcome,
                rusty_agent_runtime::executor::ExecutionOutcome::Done(_)
            ),
            "parallel run completes"
        );
        SpanTree::from_journal(&journal)
    };

    let tree = build_tree().await;
    let left = || SpanSelection {
        name: Some("rusty.node".to_string()),
        predicates: vec![pred("node", PredicateOp::Eq, text("left"))],
    };
    let right = || SpanSelection {
        name: Some("rusty.node".to_string()),
        predicates: vec![pred("node", PredicateOp::Eq, text("right"))],
    };
    let concurrent = query(left(), SpanConstraint::ConcurrentWith { other: right() });
    let ordered = query(left(), SpanConstraint::Before { other: right() });

    let baseline_concurrent = evaluate_query(&tree, &concurrent).unwrap();
    let baseline_ordered = evaluate_query(&tree, &ordered).unwrap();
    for _ in 0..20 {
        let fresh = build_tree().await;
        assert_eq!(
            evaluate_query(&fresh, &concurrent).unwrap(),
            baseline_concurrent
        );
        assert_eq!(evaluate_query(&fresh, &ordered).unwrap(), baseline_ordered);
    }
    assert!(
        baseline_concurrent.passed,
        "parallel branches overlap in log position: {:?}",
        baseline_concurrent.failure
    );
    assert!(
        !baseline_ordered.passed,
        "parallel siblings never satisfy `before`"
    );
}
