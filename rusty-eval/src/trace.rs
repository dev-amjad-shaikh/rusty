//! A run's execution as a queryable span tree, distilled from the Flight
//! Recorder journal.
//!
//! [`SpanTree`] is the span-query language's input: one rooted tree of
//! [`TraceSpan`]s — `rusty.run` over `rusty.super_step` over `rusty.node`
//! over the leaf calls (model, tool, remote, WASM) — with every span
//! carrying the attributes assertions predicate over (tool name, effect
//! class, model ref, token counts, duration, receipt presence). Ordering
//! and ancestry derive from journal positions and the events' causal
//! parentage, never from wall-clock races, so a query's verdict is stable
//! across repetitions (the determinism contract of EP-12-S04 AC 5).
//!
//! The tree is plain data — serializable, clonable, free of runtime
//! handles — so the same query language serves offline suites (a journal
//! distilled here) and online spot-checks (a production trace exported in
//! the same shape), which is what makes gate assertions reusable as
//! production scorers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use rusty_agent_runtime::journal::Journal;
use rusty_agent_runtime::record::{RunEvent, RunEventKind};

/// The published span names, mirroring the runtime's tracing taxonomy.
/// Selection by name validates against this list at authoring time.
pub const SPAN_NAMES: &[&str] = &[
    "rusty.run",
    "rusty.super_step",
    "rusty.node",
    "rusty.model_call",
    "rusty.tool_call",
    "rusty.remote_call",
    "rusty.wasm_call",
];

/// A typed span attribute value. Numbers keep their int/float
/// distinction so predicates compare without string coercion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    /// A boolean flag (receipt presence).
    Bool(bool),
    /// A whole-number measurement (token counts, seq positions).
    Integer(i64),
    /// A fractional measurement (cost in USD).
    Float(f64),
    /// A name or label (tool, model, effect class, status).
    Text(String),
}

impl AttributeValue {
    /// The numeric reading, when the value is one.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            AttributeValue::Integer(n) => Some(*n as f64),
            AttributeValue::Float(n) => Some(*n),
            _ => None,
        }
    }
}

/// One span of the execution tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceSpan {
    /// `span-{start_seq}-{end_seq}` — content-derived from the journal
    /// positions, so the same run distills the same ids.
    pub span_id: String,
    /// The span's published name (see [`SPAN_NAMES`]).
    pub name: String,
    /// The enclosing span's id (`None` on the root). Ancestry is real
    /// structure, not interval coincidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The journal sequence number the span opened at.
    pub start_seq: u64,
    /// The journal sequence number the span closed at (equal to
    /// `start_seq` for point events — a leaf call).
    pub end_seq: u64,
    /// The span's direct children, in journal order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    /// The attributes assertions predicate over.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, AttributeValue>,
}

impl TraceSpan {
    /// Read one attribute.
    pub fn attribute(&self, name: &str) -> Option<&AttributeValue> {
        self.attributes.get(name)
    }

    /// Whether `[start_seq, end_seq]` overlaps `other`'s interval —
    /// the concurrency test. Overlapping log ranges mean the wall-clock
    /// order between the two spans was never established.
    pub fn overlaps(&self, other: &TraceSpan) -> bool {
        self.start_seq <= other.end_seq && other.start_seq <= self.end_seq
    }
}

/// A run's execution tree, distilled from its journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanTree {
    /// The run the tree was distilled from.
    pub run_id: String,
    /// Every span, in `start_seq` order (ties broken by `end_seq`, then
    /// id) — deterministic for a given journal.
    pub spans: Vec<TraceSpan>,
}

impl SpanTree {
    /// A hand-built tree (fixtures, production traces imported in the
    /// same shape). Spans are sorted into canonical order.
    pub fn new(run_id: impl Into<String>, mut spans: Vec<TraceSpan>) -> Self {
        spans.sort_by(|a, b| {
            a.start_seq
                .cmp(&b.start_seq)
                .then_with(|| a.end_seq.cmp(&b.end_seq))
                .then_with(|| a.span_id.cmp(&b.span_id))
        });
        Self {
            run_id: run_id.into(),
            spans,
        }
    }

    /// A flat span list with parents derived by interval containment —
    /// the same rule the journal distillation applies, so a fixture tree
    /// and a distilled tree behave identically under ancestry queries.
    pub fn from_flat(run_id: impl Into<String>, spans: Vec<TraceSpan>) -> Self {
        let mut spans = spans;
        assign_parents(&mut spans);
        Self::new(run_id, spans)
    }

    /// Look a span up by id.
    pub fn get(&self, span_id: &str) -> Option<&TraceSpan> {
        self.spans.iter().find(|span| span.span_id == span_id)
    }

    /// The span's ancestor ids, nearest first.
    pub fn ancestors(&self, span_id: &str) -> Vec<&str> {
        let mut chain = Vec::new();
        let mut current = self.get(span_id);
        while let Some(span) = current {
            match &span.parent {
                Some(parent) => {
                    chain.push(parent.as_str());
                    current = self.get(parent);
                }
                None => break,
            }
        }
        chain
    }

    /// Whether `ancestor` is on `span`'s parent chain.
    pub fn is_ancestor(&self, ancestor: &str, span: &str) -> bool {
        self.ancestors(span).contains(&ancestor)
    }

    /// Distill a run's journal into the span tree.
    ///
    /// Pairing is by event kind and FIFO order per scope: a
    /// `SuperStepStart` pairs with the next `SuperStepEnd`, a `NodeInput`
    /// with the next `NodeOutput` for the same node. Leaf calls are point
    /// spans at their event's sequence number. Parentage is interval
    /// containment — the smallest enclosing span wins — so the tree is
    /// determined by the journal alone.
    pub fn from_journal(journal: &Journal) -> Self {
        let events = journal.events();
        let run_id = events
            .first()
            .map(|event| event.run_id.clone())
            .unwrap_or_default();

        // Receipt presence: an EffectReceipt's parent is the effect's own
        // event, so the set of receipted event ids comes first.
        let receipted: std::collections::BTreeSet<&str> = events
            .iter()
            .filter(|event| event.kind == RunEventKind::EffectReceipt)
            .filter_map(|event| event.parent.as_deref())
            .collect();

        let mut spans: Vec<TraceSpan> = Vec::new();
        // Pair begin/end events per scope, in journal order.
        let mut open_steps: Vec<u64> = Vec::new();
        let mut open_nodes: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut step_index = 0_i64;
        for event in &events {
            match event.kind {
                RunEventKind::SuperStepStart => open_steps.push(event.seq),
                RunEventKind::SuperStepEnd => {
                    if let Some(start) = open_steps.pop() {
                        let mut span = interval_span("rusty.super_step", start, event.seq);
                        span.attributes
                            .insert("step".into(), AttributeValue::Integer(step_index));
                        insert_status(&mut span, event);
                        step_index += 1;
                        spans.push(span);
                    }
                }
                RunEventKind::NodeInput => {
                    if let Some(node) = &event.node_id {
                        open_nodes.entry(node.clone()).or_default().push(event.seq);
                    }
                }
                RunEventKind::NodeOutput => {
                    let start = event
                        .node_id
                        .as_ref()
                        .and_then(|node| open_nodes.get_mut(node)?.pop());
                    if let Some(start) = start {
                        let mut span = interval_span("rusty.node", start, event.seq);
                        if let Some(node) = &event.node_id {
                            span.attributes
                                .insert("node".into(), AttributeValue::Text(node.clone()));
                        }
                        insert_latency(&mut span, event);
                        insert_status(&mut span, event);
                        spans.push(span);
                    }
                }
                RunEventKind::ModelCall => {
                    let mut span = leaf_span("rusty.model_call", event);
                    if let Some(usage) = &event.tokens {
                        span.attributes.insert(
                            "tokens_total".into(),
                            AttributeValue::Integer(usage.total_tokens as i64),
                        );
                    }
                    if let Some(model) = payload_text(journal, event, "model") {
                        span.attributes
                            .insert("model".into(), AttributeValue::Text(model));
                    }
                    insert_effect(&mut span, event);
                    spans.push(span);
                }
                RunEventKind::ToolCall => {
                    let mut span = leaf_span("rusty.tool_call", event);
                    let tool = payload_text(journal, event, "tool")
                        .or_else(|| event.node_id.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    span.attributes
                        .insert("tool".into(), AttributeValue::Text(tool));
                    insert_effect(&mut span, event);
                    span.attributes.insert(
                        "has_receipt".into(),
                        AttributeValue::Bool(receipted.contains(event.id.as_str())),
                    );
                    spans.push(span);
                }
                RunEventKind::RemoteCall => {
                    let mut span = leaf_span("rusty.remote_call", event);
                    insert_effect(&mut span, event);
                    span.attributes.insert(
                        "has_receipt".into(),
                        AttributeValue::Bool(receipted.contains(event.id.as_str())),
                    );
                    spans.push(span);
                }
                RunEventKind::WasmCall => {
                    let mut span = leaf_span("rusty.wasm_call", event);
                    insert_effect(&mut span, event);
                    spans.push(span);
                }
                _ => {}
            }
        }

        // The root encloses everything the run journaled.
        if let (Some(first), Some(last)) = (events.first(), events.last()) {
            let mut root = interval_span("rusty.run", first.seq, last.seq);
            root.attributes.insert(
                "thread".into(),
                AttributeValue::Text(first.thread_id.clone()),
            );
            root.attributes.insert(
                "events".into(),
                AttributeValue::Integer(events.len() as i64),
            );
            spans.push(root);
        }

        assign_parents(&mut spans);
        Self::new(run_id, spans)
    }
}

/// A span over a journal interval, id derived from its positions.
fn interval_span(name: &str, start_seq: u64, end_seq: u64) -> TraceSpan {
    TraceSpan {
        span_id: format!("span-{start_seq}-{end_seq}"),
        name: name.to_string(),
        parent: None,
        start_seq,
        end_seq,
        children: Vec::new(),
        attributes: BTreeMap::new(),
    }
}

/// A point span for a single-event leaf call.
fn leaf_span(name: &str, event: &RunEvent) -> TraceSpan {
    let mut span = interval_span(name, event.seq, event.seq);
    insert_latency(&mut span, event);
    insert_status(&mut span, event);
    if let Some(cost) = event.cost_usd {
        span.attributes
            .insert("cost_usd".into(), AttributeValue::Float(cost));
    }
    span
}

fn insert_latency(span: &mut TraceSpan, event: &RunEvent) {
    if let Some(latency) = event.latency_ms {
        span.attributes
            .insert("latency_ms".into(), AttributeValue::Integer(latency as i64));
    }
}

fn insert_status(span: &mut TraceSpan, event: &RunEvent) {
    if let Ok(serde_json::Value::String(status)) = serde_json::to_value(event.status) {
        span.attributes
            .insert("status".into(), AttributeValue::Text(status));
    }
}

fn insert_effect(span: &mut TraceSpan, event: &RunEvent) {
    if let Ok(serde_json::Value::String(effect)) = serde_json::to_value(event.effect) {
        span.attributes
            .insert("effect".into(), AttributeValue::Text(effect));
    }
}

/// Read a string field from the event's resolved input payload, then its
/// output payload.
fn payload_text(journal: &Journal, event: &RunEvent, field: &str) -> Option<String> {
    for reference in [&event.input, &event.output].into_iter().flatten() {
        if let Some(text) = journal
            .resolve(reference)
            .and_then(|payload| payload.get(field)?.as_str().map(str::to_string))
        {
            return Some(text);
        }
    }
    None
}

/// Parent every span to its smallest strict container — ancestry decided
/// by the journal intervals alone, deterministic by construction.
fn assign_parents(spans: &mut [TraceSpan]) {
    let intervals: Vec<(String, u64, u64)> = spans
        .iter()
        .map(|span| (span.span_id.clone(), span.start_seq, span.end_seq))
        .collect();
    let mut parents: Vec<(usize, Option<String>)> = Vec::with_capacity(spans.len());
    for (index, (id, start, end)) in intervals.iter().enumerate() {
        // The parent is the narrowest span that strictly contains this
        // one; ties break to the earliest-starting container.
        let mut best: Option<(u64, u64, String)> = None;
        for (other_id, other_start, other_end) in &intervals {
            if other_id == id {
                continue;
            }
            let contains = *other_start <= *start
                && *end <= *other_end
                && (*other_start, *other_end) != (*start, *end);
            if !contains {
                continue;
            }
            let width = *other_end - *other_start;
            let narrower = match &best {
                None => true,
                Some((best_width, best_start, _)) => {
                    width < *best_width || (width == *best_width && *other_start < *best_start)
                }
            };
            if narrower {
                best = Some((width, *other_start, other_id.clone()));
            }
        }
        parents.push((index, best.map(|(_, _, parent_id)| parent_id)));
    }
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (index, parent) in parents {
        let id = spans[index].span_id.clone();
        if let Some(parent_id) = &parent {
            children.entry(parent_id.clone()).or_default().push(id);
        }
        spans[index].parent = parent;
    }
    for span in spans.iter_mut() {
        if let Some(mut kids) = children.remove(&span.span_id) {
            kids.sort();
            span.children = kids;
        }
    }
}
