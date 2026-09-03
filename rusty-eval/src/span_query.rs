//! The span-query language: serializable structural assertions over a
//! run's [`SpanTree`].
//!
//! A [`SpanQuery`] is a selection (span name plus attribute predicates)
//! under one constraint — existence, absence, count bounds, ordering
//! (`before`), ancestry (`within`), deliberate concurrency
//! (`concurrent_with`), or a budget aggregate (`total_within`). The same
//! query serves offline suites and production spot-checks: pointed at any
//! tree in the published shape, it evaluates identically.
//!
//! Authoring-time validation is part of the contract: a query referencing
//! an attribute outside the versioned vocabulary ([`SPAN_VOCABULARY`]) or
//! a span name outside [`SPAN_NAMES`] fails
//! [`SpanQuery::validate`], never silently at run time.
//!
//! Failures are diagnosable without opening the raw trace: every verdict
//! names the query and the clause that failed, lists the matched spans,
//! and — when the selection matched nothing — the nearest candidate spans
//! by name, so "the tool span is called `rusty.tool_call`, not
//! `tool.call`" is one read away.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{EvalError, Result};
use crate::trace::{AttributeValue, SPAN_NAMES, SpanTree, TraceSpan};

/// The versioned attribute vocabulary's version. Bumping it is a
/// contract change; suites pin against it.
pub const SPAN_VOCABULARY_VERSION: u32 = 1;

/// An attribute's value kind, for authoring-time predicate checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeKind {
    /// True/false flags (`has_receipt`).
    Boolean,
    /// Numeric measurements (`latency_ms`, `tokens_total`, `cost_usd`).
    Number,
    /// Names and labels (`tool`, `model`, `effect`, `status`).
    Text,
}

/// One vocabulary entry: the attribute, its kind, what it means, and the
/// spans that carry it.
pub struct VocabularyEntry {
    /// The attribute name as queries reference it.
    pub attribute: &'static str,
    /// Its value kind.
    pub kind: AttributeKind,
    /// What the attribute records.
    pub doc: &'static str,
    /// The span names carrying it (empty = all spans).
    pub spans: &'static [&'static str],
}

/// The published attribute vocabulary, version 1. Generated from the
/// distillation in [`crate::trace`]: every attribute a span can carry is
/// listed here, and an assertion referencing anything else fails
/// validation at authoring time.
pub const SPAN_VOCABULARY: &[VocabularyEntry] = &[
    VocabularyEntry {
        attribute: "thread",
        kind: AttributeKind::Text,
        doc: "the thread (session) the run belongs to",
        spans: &["rusty.run"],
    },
    VocabularyEntry {
        attribute: "events",
        kind: AttributeKind::Number,
        doc: "how many events the run journaled",
        spans: &["rusty.run"],
    },
    VocabularyEntry {
        attribute: "step",
        kind: AttributeKind::Number,
        doc: "the super-step's index in the run, zero-based",
        spans: &["rusty.super_step"],
    },
    VocabularyEntry {
        attribute: "node",
        kind: AttributeKind::Text,
        doc: "the graph node the span executed",
        spans: &["rusty.node"],
    },
    VocabularyEntry {
        attribute: "tool",
        kind: AttributeKind::Text,
        doc: "the tool a call invoked",
        spans: &["rusty.tool_call"],
    },
    VocabularyEntry {
        attribute: "model",
        kind: AttributeKind::Text,
        doc: "the model ref a call was served by, when reported",
        spans: &["rusty.model_call"],
    },
    VocabularyEntry {
        attribute: "effect",
        kind: AttributeKind::Text,
        doc: "the declared effect class of what produced the span",
        spans: &[
            "rusty.model_call",
            "rusty.tool_call",
            "rusty.remote_call",
            "rusty.wasm_call",
        ],
    },
    VocabularyEntry {
        attribute: "status",
        kind: AttributeKind::Text,
        doc: "how the span's operation ended",
        spans: &[],
    },
    VocabularyEntry {
        attribute: "latency_ms",
        kind: AttributeKind::Number,
        doc: "the operation's measured latency in milliseconds",
        spans: &[],
    },
    VocabularyEntry {
        attribute: "cost_usd",
        kind: AttributeKind::Number,
        doc: "the operation's journaled cost in USD",
        spans: &[],
    },
    VocabularyEntry {
        attribute: "tokens_total",
        kind: AttributeKind::Number,
        doc: "total tokens the model call was billed",
        spans: &["rusty.model_call"],
    },
    VocabularyEntry {
        attribute: "has_receipt",
        kind: AttributeKind::Boolean,
        doc: "whether an effect receipt was journaled for the call",
        spans: &["rusty.tool_call", "rusty.remote_call"],
    },
];

/// Look an attribute up in the vocabulary.
fn vocabulary_entry(attribute: &str) -> Option<&'static VocabularyEntry> {
    SPAN_VOCABULARY
        .iter()
        .find(|entry| entry.attribute == attribute)
}

/// A comparison operator for attribute predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateOp {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Strictly less (numeric attributes only).
    Lt,
    /// Less or equal (numeric attributes only).
    Le,
    /// Strictly greater (numeric attributes only).
    Gt,
    /// Greater or equal (numeric attributes only).
    Ge,
}

impl PredicateOp {
    /// Whether the operator orders (and therefore needs numbers).
    pub fn is_ordering(&self) -> bool {
        matches!(
            self,
            PredicateOp::Lt | PredicateOp::Le | PredicateOp::Gt | PredicateOp::Ge
        )
    }
}

/// One attribute condition in a selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributePredicate {
    /// The vocabulary attribute to test.
    pub attribute: String,
    /// The comparison.
    pub op: PredicateOp,
    /// The value to compare against.
    pub value: AttributeValue,
}

impl AttributePredicate {
    /// Whether the predicate holds for a span.
    fn matches(&self, span: &TraceSpan) -> bool {
        let Some(actual) = span.attribute(&self.attribute) else {
            return false;
        };
        match self.op {
            PredicateOp::Eq => actual == &self.value,
            PredicateOp::Ne => actual != &self.value,
            PredicateOp::Lt | PredicateOp::Le | PredicateOp::Gt | PredicateOp::Ge => {
                match (actual.as_number(), self.value.as_number()) {
                    (Some(actual), Some(expected)) => match self.op {
                        PredicateOp::Lt => actual < expected,
                        PredicateOp::Le => actual <= expected,
                        PredicateOp::Gt => actual > expected,
                        PredicateOp::Ge => actual >= expected,
                        _ => unreachable!(),
                    },
                    _ => false,
                }
            }
        }
    }
}

/// Which spans a query talks about: an optional name from the published
/// taxonomy plus attribute predicates (all must hold).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanSelection {
    /// The span name to select (`None` matches every name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The attribute conditions, conjunctive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predicates: Vec<AttributePredicate>,
}

impl SpanSelection {
    /// A selection by name alone.
    pub fn named(name: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            predicates: Vec::new(),
        }
    }

    /// The spans of `tree` this selection matches, in tree order.
    pub fn select<'a>(&self, tree: &'a SpanTree) -> Vec<&'a TraceSpan> {
        tree.spans
            .iter()
            .filter(|span| self.name.as_deref().is_none_or(|name| span.name == name))
            .filter(|span| {
                self.predicates
                    .iter()
                    .all(|predicate| predicate.matches(span))
            })
            .collect()
    }
}

/// The constraint a selection is evaluated under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "constraint", rename_all = "snake_case")]
pub enum SpanConstraint {
    /// At least one selected span exists.
    Exists,
    /// No selected span exists.
    Absent,
    /// The selection's cardinality sits within `[min, max]`.
    CountWithin {
        /// At least this many.
        min: u64,
        /// At most this many.
        max: u64,
    },
    /// Every selected span closed before every `other` span opened —
    /// log-position order, unambiguous by construction. Concurrent
    /// spans fail: deliberate concurrency asserts with
    /// [`SpanConstraint::ConcurrentWith`].
    Before {
        /// The selection that must come after.
        other: SpanSelection,
    },
    /// Every selected span descends from an `ancestor`-selected span.
    Within {
        /// The selection that must contain.
        ancestor: SpanSelection,
    },
    /// At least one selected span overlaps an `other`-selected span in
    /// log position — the deliberate-concurrency assertion.
    ConcurrentWith {
        /// The selection to overlap.
        other: SpanSelection,
    },
    /// The sum of a numeric vocabulary attribute over the selection is
    /// at most `max` — the budget assertion (total tokens, total tool
    /// duration). Spans not carrying the attribute contribute zero.
    TotalWithin {
        /// The numeric attribute to aggregate.
        attribute: String,
        /// The inclusive ceiling.
        max: f64,
    },
}

/// One serializable structural assertion over a span tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanQuery {
    /// The spans the constraint quantifies over.
    pub select: SpanSelection,
    /// The constraint.
    #[serde(flatten)]
    pub constraint: SpanConstraint,
}

impl SpanQuery {
    /// Authoring-time validation: span names from the published
    /// taxonomy, attributes from the versioned vocabulary, ordering
    /// operators only on numeric attributes, budget aggregates only on
    /// numeric attributes. Anything else is an error now, not a silent
    /// misfire at scoring time.
    pub fn validate(&self) -> Result<()> {
        validate_selection(&self.select)?;
        match &self.constraint {
            SpanConstraint::Before { other } | SpanConstraint::ConcurrentWith { other } => {
                validate_selection(other)?
            }
            SpanConstraint::Within { ancestor } => validate_selection(ancestor)?,
            SpanConstraint::TotalWithin { attribute, max } => {
                let entry = vocabulary_entry(attribute).ok_or_else(|| {
                    EvalError::Dataset(format!(
                        "unknown span attribute `{attribute}` (vocabulary v{SPAN_VOCABULARY_VERSION})"
                    ))
                })?;
                if entry.kind != AttributeKind::Number {
                    return Err(EvalError::Dataset(format!(
                        "budget attribute `{attribute}` is not numeric"
                    )));
                }
                if !max.is_finite() {
                    return Err(EvalError::Dataset(
                        "budget ceiling must be finite".to_string(),
                    ));
                }
            }
            SpanConstraint::CountWithin { min, max } => {
                if min > max {
                    return Err(EvalError::Dataset(format!(
                        "count bounds are inverted: min {min} > max {max}"
                    )));
                }
            }
            SpanConstraint::Exists | SpanConstraint::Absent => {}
        }
        Ok(())
    }
}

/// Validate one selection against the taxonomy and vocabulary.
fn validate_selection(selection: &SpanSelection) -> Result<()> {
    if let Some(name) = &selection.name {
        if !SPAN_NAMES.contains(&name.as_str()) {
            return Err(EvalError::Dataset(format!(
                "unknown span name `{name}` (published: {})",
                SPAN_NAMES.join(", ")
            )));
        }
    }
    for predicate in &selection.predicates {
        let entry = vocabulary_entry(&predicate.attribute).ok_or_else(|| {
            EvalError::Dataset(format!(
                "unknown span attribute `{}` (vocabulary v{SPAN_VOCABULARY_VERSION})",
                predicate.attribute
            ))
        })?;
        if predicate.op.is_ordering() && entry.kind != AttributeKind::Number {
            return Err(EvalError::Dataset(format!(
                "ordering operator on non-numeric attribute `{}`",
                predicate.attribute
            )));
        }
        let kind_matches = matches!(
            (&entry.kind, &predicate.value),
            (AttributeKind::Boolean, AttributeValue::Bool(_))
                | (AttributeKind::Number, AttributeValue::Integer(_))
                | (AttributeKind::Number, AttributeValue::Float(_))
                | (AttributeKind::Text, AttributeValue::Text(_))
        );
        if !kind_matches {
            return Err(EvalError::Dataset(format!(
                "predicate value for `{}` does not match its {:?} kind",
                predicate.attribute, entry.kind
            )));
        }
    }
    Ok(())
}

/// The span as a failure report summarizes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanSummary {
    /// The span id.
    pub span_id: String,
    /// The published name.
    pub name: String,
    /// Journal interval.
    pub start_seq: u64,
    /// Journal interval end.
    pub end_seq: u64,
    /// The attributes that were on the span.
    pub attributes: std::collections::BTreeMap<String, AttributeValue>,
}

impl From<&TraceSpan> for SpanSummary {
    fn from(span: &TraceSpan) -> Self {
        Self {
            span_id: span.span_id.clone(),
            name: span.name.clone(),
            start_seq: span.start_seq,
            end_seq: span.end_seq,
            attributes: span.attributes.clone(),
        }
    }
}

/// Why a query failed: the query, the clause, the matched evidence, and
/// — when the selection matched nothing — the nearest candidate spans by
/// name, so the report alone diagnoses the miss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryFailure {
    /// The query that failed, echoed verbatim.
    pub query: Value,
    /// The clause that failed (`exists`, `before.ordering`, …).
    pub clause: String,
    /// What was expected vs observed, in one sentence.
    pub detail: String,
    /// The spans the selection matched (the violating evidence).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched: Vec<SpanSummary>,
    /// The nearest candidate spans when the selection matched nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nearest: Vec<SpanSummary>,
}

/// A query's verdict: pass, or a diagnosable failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryVerdict {
    /// Whether the query held.
    pub passed: bool,
    /// The failure report, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<QueryFailure>,
}

impl QueryVerdict {
    fn pass() -> Self {
        Self {
            passed: true,
            failure: None,
        }
    }

    fn fail(
        query: &SpanQuery,
        clause: &str,
        detail: String,
        matched: Vec<SpanSummary>,
        tree: &SpanTree,
        selection: &SpanSelection,
    ) -> Self {
        let nearest = if matched.is_empty() {
            nearest_spans(tree, selection)
        } else {
            Vec::new()
        };
        Self {
            passed: false,
            failure: Some(QueryFailure {
                query: serde_json::to_value(query).unwrap_or(Value::Null),
                clause: clause.to_string(),
                detail,
                matched,
                nearest,
            }),
        }
    }
}

/// The spans whose names are nearest to the selection's requested name:
/// sharing a prefix or substring, else the taxonomy the tree actually
/// contains. Deterministic, tree order, at most five.
fn nearest_spans(tree: &SpanTree, selection: &SpanSelection) -> Vec<SpanSummary> {
    let Some(name) = &selection.name else {
        return tree.spans.iter().take(5).map(SpanSummary::from).collect();
    };
    let prefix = name.split('.').next().unwrap_or(name);
    let close: Vec<&TraceSpan> = tree
        .spans
        .iter()
        .filter(|span| span.name.contains(prefix) || name.contains(&span.name))
        .collect();
    let candidates = if close.is_empty() {
        tree.spans.iter().collect()
    } else {
        close
    };
    candidates
        .into_iter()
        .take(5)
        .map(SpanSummary::from)
        .collect()
}

/// Evaluate one query against a span tree. Invalid queries are errors
/// (authoring-time mistakes); a valid query that does not hold is a
/// verdict, not an error.
pub fn evaluate_query(tree: &SpanTree, query: &SpanQuery) -> Result<QueryVerdict> {
    query.validate()?;
    let matched: Vec<&TraceSpan> = query.select.select(tree);
    let matched_summaries: Vec<SpanSummary> = matched
        .iter()
        .map(|span| SpanSummary::from(*span))
        .collect();
    let fail = |clause: &str, detail: String| {
        QueryVerdict::fail(
            query,
            clause,
            detail,
            matched_summaries.clone(),
            tree,
            &query.select,
        )
    };
    let verdict = match &query.constraint {
        SpanConstraint::Exists => {
            if matched.is_empty() {
                fail("exists", "selection matched no spans".to_string())
            } else {
                QueryVerdict::pass()
            }
        }
        SpanConstraint::Absent => {
            if matched.is_empty() {
                QueryVerdict::pass()
            } else {
                fail(
                    "absent",
                    format!(
                        "selection matched {} span(s) that must not exist",
                        matched.len()
                    ),
                )
            }
        }
        SpanConstraint::CountWithin { min, max } => {
            let count = matched.len() as u64;
            if count < *min || count > *max {
                fail(
                    "count_within",
                    format!("selection matched {count} span(s), outside [{min}, {max}]"),
                )
            } else {
                QueryVerdict::pass()
            }
        }
        SpanConstraint::TotalWithin { attribute, max } => {
            let total: f64 = matched
                .iter()
                .filter_map(|span| span.attribute(attribute))
                .filter_map(AttributeValue::as_number)
                .sum();
            if total <= *max {
                QueryVerdict::pass()
            } else {
                fail(
                    "total_within",
                    format!("total `{attribute}` is {total}, above the {max} ceiling"),
                )
            }
        }
        SpanConstraint::Before { other } => {
            let others: Vec<&TraceSpan> = other.select(tree);
            if matched.is_empty() || others.is_empty() {
                fail(
                    "before.selection",
                    "both selections must match spans to be ordered".to_string(),
                )
            } else {
                let latest_end = matched.iter().map(|span| span.end_seq).max().unwrap_or(0);
                let earliest_start = others.iter().map(|span| span.start_seq).min().unwrap_or(0);
                if latest_end < earliest_start {
                    QueryVerdict::pass()
                } else {
                    let offenders: Vec<SpanSummary> = matched
                        .iter()
                        .filter(|a| {
                            others
                                .iter()
                                .any(|b| a.overlaps(b) || a.end_seq >= b.start_seq)
                        })
                        .map(|span| SpanSummary::from(*span))
                        .collect();
                    QueryVerdict::fail(
                        query,
                        "before.ordering",
                        format!(
                            "order is ambiguous or inverted: latest selected close at seq {latest_end}, earliest other open at seq {earliest_start}"
                        ),
                        offenders,
                        tree,
                        &query.select,
                    )
                }
            }
        }
        SpanConstraint::ConcurrentWith { other } => {
            let others: Vec<&TraceSpan> = other.select(tree);
            let pair = matched.iter().any(|a| {
                others
                    .iter()
                    .any(|b| a.span_id != b.span_id && a.overlaps(b))
            });
            if pair {
                QueryVerdict::pass()
            } else {
                fail(
                    "concurrent_with",
                    "no selected span overlaps an other-selected span in log position".to_string(),
                )
            }
        }
        SpanConstraint::Within { ancestor } => {
            let ancestors: Vec<&TraceSpan> = ancestor.select(tree);
            if matched.is_empty() {
                fail("within.selection", "selection matched no spans".to_string())
            } else if ancestors.is_empty() {
                QueryVerdict::fail(
                    query,
                    "within.selection",
                    "the ancestor selection matched no spans".to_string(),
                    Vec::new(),
                    tree,
                    ancestor,
                )
            } else {
                let orphan = matched.iter().find(|span| {
                    !ancestors
                        .iter()
                        .any(|candidate| tree.is_ancestor(&candidate.span_id, &span.span_id))
                });
                match orphan {
                    None => QueryVerdict::pass(),
                    Some(span) => QueryVerdict::fail(
                        query,
                        "within.ancestry",
                        format!(
                            "span `{}` ({}) descends from no ancestor-selected span",
                            span.span_id, span.name
                        ),
                        vec![SpanSummary::from(*span)],
                        tree,
                        &query.select,
                    ),
                }
            }
        }
    };
    Ok(verdict)
}

/// Evaluate a set of queries against a tree; the verdicts arrive in
/// query order.
pub fn evaluate_all(tree: &SpanTree, queries: &[SpanQuery]) -> Result<Vec<QueryVerdict>> {
    queries
        .iter()
        .map(|query| evaluate_query(tree, query))
        .collect()
}
