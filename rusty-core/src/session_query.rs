//! Session introspection: the [`JournalQuery`] seam and the agent-visible
//! tools over it (`session_search`, `session_trace`).
//!
//! The Flight Recorder journals everything; this module is how an agent (or
//! an operator driving one) *reads its own evidence* back — full-text search
//! over journaled payloads, causal traces along parent links, and bounded
//! event reads — without granting access to the store beneath. Every read is
//! bounded (result counts, trace depth, excerpt size): introspection is a
//! tool call, and a tool call must never be able to turn "search the
//! session" into an unbounded dump of the journal.
//!
//! The causal structure a trace walks is the journal's own: every event's
//! `parent` id was assigned at record time (node code parents its model and
//! tool calls onto the invocation's node-input event via
//! [`crate::journal::PARENT_EVENT_KEY`]), so the trace is a read of recorded
//! causality, never a reconstruction guess.
//!
//! Implementations:
//!
//! - [`InMemoryJournalQuery`] — over in-memory snapshots; the dev/test and
//!   fixture implementation.
//! - [`FileJournalQuery`] — over one JSON file per run
//!   (`{dir}/{run_id}.json`, a serialized
//!   [`crate::journal::JournalSnapshot`]). This is the read half of the
//!   layout `rusty-agent-server`'s journal store writes; core cannot depend
//!   on the server crate, so the layout is re-implemented here behind the
//!   seam and the server-side route wiring (`GET /runs/{id}/...` backed by
//!   these tools) is the documented follow-up.
//!
//! The tools ([`SessionSearchTool`], [`SessionTraceTool`]) follow the
//! built-in tool patterns (`crate::tool::builtins`): closed argument
//! schemas, declared [`Effect::ReadOnly`], bounded output.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Result, RustyError};
use crate::journal::JournalSnapshot;
use crate::record::{Effect, PayloadRef, RunEvent, RunEventKind};
use crate::tool::Tool;

/// Maximum query text a search accepts, in bytes.
pub const MAX_QUERY_BYTES: usize = 512;

/// Hard ceiling on search hits one call returns.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Hard ceiling on events one trace returns (ancestors + descendants + target).
pub const MAX_TRACE_EVENTS: usize = 128;

/// Hard ceiling on events one bounded read returns.
pub const MAX_READ_EVENTS: usize = 256;

/// Maximum excerpt length in a search hit, in characters.
pub const MAX_EXCERPT_CHARS: usize = 240;

/// Which payload a [`SearchHit`] matched in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchField {
    /// The event's input payload.
    Input,
    /// The event's output payload.
    Output,
}

/// One search hit: where the match is, plus a bounded excerpt around it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The run the matching event belongs to.
    pub run_id: String,

    /// The thread (session) the run belongs to.
    pub thread_id: String,

    /// The matching event's id (`{run_id}:{seq}`).
    pub event_id: String,

    /// The matching event's sequence number.
    pub seq: u64,

    /// What the matching event recorded.
    pub kind: RunEventKind,

    /// The payload the match was found in.
    pub field: SearchField,

    /// How many distinct query terms the payload contains (the rank).
    pub score: usize,

    /// A bounded excerpt around the first matched term.
    pub excerpt: String,
}

/// A search request: terms over journaled payloads, optionally scoped to one
/// run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearch {
    /// The query text: whitespace-separated terms, matched case-insensitively
    /// as substrings of the serialized payloads.
    pub text: String,

    /// Restrict the search to one run; `None` searches every run the
    /// implementation can see.
    pub run_id: Option<String>,

    /// Maximum hits to return (clamped to [`MAX_SEARCH_RESULTS`]).
    pub limit: usize,
}

/// A causal trace around one event: the target, its ancestor chain
/// (root-first), and its descendants (sequence order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventTrace {
    /// The event the trace was requested for.
    pub target: RunEvent,

    /// The target's causal ancestors, root first (the last entry is the
    /// target's direct parent).
    pub ancestors: Vec<RunEvent>,

    /// Events the target is a causal ancestor of, in sequence order.
    pub descendants: Vec<RunEvent>,

    /// `true` when the trace hit [`MAX_TRACE_EVENTS`] and was cut — a
    /// truncated trace says so rather than posing as complete.
    pub truncated: bool,
}

/// The introspection seam: how agents and tools read journaled evidence.
///
/// Implementations must honor the bounds in this module (clamping rather
/// than erroring on oversized limits) so the tools above the seam can state
/// their ceilings honestly.
#[async_trait]
pub trait JournalQuery: Send + Sync {
    /// Full-text search over journaled input/output payloads.
    async fn search(&self, query: &SessionSearch) -> Result<Vec<SearchHit>>;

    /// The causal trace around `event_id` in `run_id`'s journal: ancestors
    /// along `parent` links, descendants along the reverse links, bounded by
    /// [`MAX_TRACE_EVENTS`].
    async fn trace(&self, run_id: &str, event_id: &str) -> Result<EventTrace>;

    /// A bounded read of one run's events: those with `seq` greater than
    /// `after` (`None` reads from the start), in sequence order, at most
    /// `limit` (clamped to [`MAX_READ_EVENTS`]).
    async fn read_events(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunEvent>>;
}

/// Map an introspection IO failure into the module's error convention — the
/// `Serialization`-over-`io` shape the journal's artifact store uses.
fn query_io_error(context: String, e: std::io::Error) -> RustyError {
    RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
        e.kind(),
        format!("{context}: {e}"),
    )))
}

fn not_found(message: impl Into<String>) -> RustyError {
    // A query-plane contract failure ("no such run/event") reuses the
    // invalid-update class, the reading `crate::knowledge` takes for its own
    // contract violations, rather than growing the error taxonomy.
    RustyError::InvalidUpdate(message.into())
}

/// Resolve a payload against the snapshot's artifact map (inline payloads
/// resolve to themselves). `None` for a dangling artifact reference — a
/// truncated snapshot; the search simply does not match it.
fn resolve<'a>(snapshot: &'a JournalSnapshot, payload: &'a PayloadRef) -> Option<&'a Value> {
    match payload {
        PayloadRef::Inline(value) => Some(value),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256),
    }
}

/// The text a payload is searched as: its own string for string values (so
/// message content matches without JSON quoting noise), the compact
/// serialization otherwise.
fn searchable_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A bounded excerpt of `text` around the first occurrence of any `terms`
/// entry (both already lowercased by the caller's search path).
fn excerpt(text: &str, terms: &[String]) -> String {
    let first = terms
        .iter()
        .filter_map(|term| text.find(term))
        .min()
        .unwrap_or(0);
    // Back off half the window so the match sits mid-excerpt; char-boundary
    // safe via `char_indices`.
    let start = text
        .char_indices()
        .nth(first.saturating_sub(MAX_EXCERPT_CHARS / 2))
        .map(|(index, _)| index)
        .unwrap_or(0);
    let mut out: String = text[start..].chars().take(MAX_EXCERPT_CHARS).collect();
    if text[start..].chars().count() > MAX_EXCERPT_CHARS {
        out.push('…');
    }
    out
}

/// Search one snapshot's payloads for `terms` (already lowercased): every
/// event whose input or output contains at least one term, ranked by
/// distinct-term count.
fn search_snapshot(snapshot: &JournalSnapshot, terms: &[String]) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for event in &snapshot.events {
        for (field, payload) in [
            (SearchField::Input, event.input.as_ref()),
            (SearchField::Output, event.output.as_ref()),
        ] {
            let Some(text) = payload
                .and_then(|payload| resolve(snapshot, payload))
                .map(|value| searchable_text(value).to_lowercase())
            else {
                continue;
            };
            let score = terms
                .iter()
                .filter(|term| text.contains(term.as_str()))
                .count();
            if score > 0 {
                hits.push(SearchHit {
                    run_id: event.run_id.clone(),
                    thread_id: event.thread_id.clone(),
                    event_id: event.id.clone(),
                    seq: event.seq,
                    kind: event.kind,
                    field,
                    score,
                    excerpt: excerpt(&text, terms),
                });
            }
        }
    }
    // Total order: rank first, then the journal's own order, so equal-rank
    // hits are stable across implementations.
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.seq.cmp(&b.seq)));
    hits
}

/// Trace one event's causal neighborhood inside its snapshot. `None` when
/// the event id is not in the journal.
fn trace_snapshot(snapshot: &JournalSnapshot, event_id: &str) -> Option<EventTrace> {
    let by_id: BTreeMap<&str, &RunEvent> = snapshot
        .events
        .iter()
        .map(|event| (event.id.as_str(), event))
        .collect();
    let target = (*by_id.get(event_id)?).clone();

    // Ancestors: follow parent links, guarding against a cycle (a
    // hand-edited journal is corruption, not a reason to loop forever).
    let mut ancestors = Vec::new();
    let mut cursor = target.parent.clone();
    let mut truncated = false;
    while let Some(parent_id) = cursor {
        if ancestors.len() + 1 >= MAX_TRACE_EVENTS {
            truncated = true;
            break;
        }
        let Some(parent) = by_id.get(parent_id.as_str()) else {
            break; // parent outside the snapshot (a forked/tail fixture)
        };
        if ancestors
            .iter()
            .any(|event: &RunEvent| event.id == parent.id)
        {
            break; // cycle: stop at the repeated event
        }
        cursor = parent.parent.clone();
        ancestors.push((*parent).clone());
    }
    ancestors.reverse(); // root-first

    // Descendants: breadth-first over reverse parent links, sequence order
    // within one generation, bounded by the same budget.
    let mut descendants = Vec::new();
    let mut frontier = vec![target.id.clone()];
    while let Some(id) = frontier.pop() {
        let mut children: Vec<&RunEvent> = snapshot
            .events
            .iter()
            .filter(|event| event.parent.as_deref() == Some(id.as_str()))
            .collect();
        children.sort_by_key(|event| event.seq);
        for child in children {
            if ancestors.len() + descendants.len() + 1 >= MAX_TRACE_EVENTS {
                truncated = true;
                break;
            }
            frontier.push(child.id.clone());
            descendants.push(child.clone());
        }
        if truncated {
            break;
        }
    }
    descendants.sort_by_key(|event| event.seq);

    Some(EventTrace {
        target,
        ancestors,
        descendants,
        truncated,
    })
}

/// In-memory [`JournalQuery`] over named snapshots: the dev/test and fixture
/// implementation.
#[derive(Debug, Clone, Default)]
pub struct InMemoryJournalQuery {
    snapshots: BTreeMap<String, JournalSnapshot>,
}

impl InMemoryJournalQuery {
    /// An empty query surface.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) the snapshot queryable under its run id.
    pub fn add_snapshot(&mut self, snapshot: JournalSnapshot) -> &mut Self {
        self.snapshots.insert(snapshot.run_id.clone(), snapshot);
        self
    }
}

#[async_trait]
impl JournalQuery for InMemoryJournalQuery {
    async fn search(&self, query: &SessionSearch) -> Result<Vec<SearchHit>> {
        let terms = query_terms(&query.text)?;
        let limit = query.limit.clamp(1, MAX_SEARCH_RESULTS);
        let mut hits = Vec::new();
        let snapshots: Vec<&JournalSnapshot> = match &query.run_id {
            Some(run_id) => self.snapshots.get(run_id).into_iter().collect(),
            None => self.snapshots.values().collect(),
        };
        for snapshot in snapshots {
            hits.extend(search_snapshot(snapshot, &terms));
        }
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(a.run_id.cmp(&b.run_id))
                .then(a.seq.cmp(&b.seq))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn trace(&self, run_id: &str, event_id: &str) -> Result<EventTrace> {
        let snapshot = self
            .snapshots
            .get(run_id)
            .ok_or_else(|| not_found(format!("no journal for run `{run_id}`")))?;
        trace_snapshot(snapshot, event_id)
            .ok_or_else(|| not_found(format!("run `{run_id}` has no event `{event_id}`")))
    }

    async fn read_events(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let Some(snapshot) = self.snapshots.get(run_id) else {
            return Ok(Vec::new());
        };
        let from = after.map_or(0, |seq| seq.saturating_add(1));
        Ok(snapshot
            .events
            .iter()
            .filter(|event| event.seq >= from)
            .take(limit.clamp(1, MAX_READ_EVENTS))
            .cloned()
            .collect())
    }
}

/// File-backed [`JournalQuery`] over the server's journal layout: one
/// serialized [`JournalSnapshot`] per run at `{dir}/{run_id}.json`.
///
/// Reads load from disk on every call. The server rewrites a run's journal
/// file as the run grows, so a cached snapshot is stale evidence by
/// construction — the seam serves the store's current bytes or fails,
/// never last week's journal under today's question.
///
/// Listing (a search across all runs) skips an unparseable file with a
/// warning — one corrupt journal must not blind every other run (the
/// server store's own listing discipline). A directly named run's file
/// surfaces its parse error instead: asking about *that* run and being
/// shown nothing would be the silent miss the typed error exists to prevent.
#[derive(Debug, Clone)]
pub struct FileJournalQuery {
    dir: PathBuf,
}

impl FileJournalQuery {
    /// A query surface over the journal files in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory journals are read from.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("{run_id}.json"))
    }

    /// Load one run's snapshot; `None` when no journal was persisted for it
    /// (a queued run, or one that failed before its first checkpoint).
    async fn load(&self, run_id: &str) -> Result<Option<JournalSnapshot>> {
        let path = self.path(run_id);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(query_io_error(format!("read journal `{}`", run_id), e)),
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            query_io_error(
                format!("parse journal `{run_id}`"),
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            )
        })
    }

    /// Every parseable snapshot in the directory, in run-id order.
    async fn list(&self) -> Result<Vec<JournalSnapshot>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(query_io_error(
                    format!("read journal directory `{}`", self.dir.display()),
                    e,
                ))
            }
        };
        let mut snapshots = Vec::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => return Err(query_io_error("iterate journal directory".into(), e)),
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let bytes = tokio::fs::read(entry.path())
                .await
                .map_err(|e| query_io_error(format!("read journal file `{name}`"), e))?;
            match serde_json::from_slice::<JournalSnapshot>(&bytes) {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(e) => {
                    tracing::warn!(file = %name, error = %e, "skipping unparseable journal file")
                }
            }
        }
        snapshots.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(snapshots)
    }
}

#[async_trait]
impl JournalQuery for FileJournalQuery {
    async fn search(&self, query: &SessionSearch) -> Result<Vec<SearchHit>> {
        let terms = query_terms(&query.text)?;
        let limit = query.limit.clamp(1, MAX_SEARCH_RESULTS);
        let mut hits = Vec::new();
        match &query.run_id {
            Some(run_id) => {
                if let Some(snapshot) = self.load(run_id).await? {
                    hits.extend(search_snapshot(&snapshot, &terms));
                }
            }
            None => {
                for snapshot in self.list().await? {
                    hits.extend(search_snapshot(&snapshot, &terms));
                }
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(a.run_id.cmp(&b.run_id))
                .then(a.seq.cmp(&b.seq))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn trace(&self, run_id: &str, event_id: &str) -> Result<EventTrace> {
        let snapshot = self
            .load(run_id)
            .await?
            .ok_or_else(|| not_found(format!("no journal for run `{run_id}`")))?;
        trace_snapshot(&snapshot, event_id)
            .ok_or_else(|| not_found(format!("run `{run_id}` has no event `{event_id}`")))
    }

    async fn read_events(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let Some(snapshot) = self.load(run_id).await? else {
            return Ok(Vec::new());
        };
        let from = after.map_or(0, |seq| seq.saturating_add(1));
        Ok(snapshot
            .events
            .iter()
            .filter(|event| event.seq >= from)
            .take(limit.clamp(1, MAX_READ_EVENTS))
            .cloned()
            .collect())
    }
}

/// Split query text into lowercase terms, enforcing the byte bound.
fn query_terms(text: &str) -> Result<Vec<String>> {
    if text.is_empty() || text.len() > MAX_QUERY_BYTES {
        return Err(not_found(format!(
            "session search query must contain 1..={MAX_QUERY_BYTES} bytes"
        )));
    }
    let terms: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return Err(not_found("session search query must contain a term"));
    }
    Ok(terms)
}

/// The JSON summary of one event the tools render: identity and shape, no
/// payloads (payloads are what `session_search` excerpts are for).
fn event_summary(event: &RunEvent) -> Value {
    json!({
        "event_id": event.id,
        "seq": event.seq,
        "kind": event.kind,
        "effect": event.effect,
        "node_id": event.node_id,
        "status": event.status,
        "latency_ms": event.latency_ms,
        "parent": event.parent,
    })
}

/// `session_search`: full-text search over the agent's own journaled
/// session evidence.
///
/// [`Effect::ReadOnly`] — the tool reads journals, it never writes, and
/// exact replay may serve a journaled search instead of re-running it.
#[derive(Clone)]
pub struct SessionSearchTool {
    query: Arc<dyn JournalQuery>,
}

impl SessionSearchTool {
    /// The tool over a [`JournalQuery`] implementation.
    pub fn new(query: Arc<dyn JournalQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search the session's journaled run evidence (model calls, tool calls, interrupts) \
         for matching text and return bounded, cited excerpts."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_BYTES},
                "run_id": {"type": "string", "description": "restrict the search to one run"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS, "default": 5}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let text = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| RustyError::Tool("`query` must be a string".into()))?;
        let run_id = args
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
        let hits = self
            .query
            .search(&SessionSearch {
                text: text.to_owned(),
                run_id,
                limit,
            })
            .await
            .map_err(|e| RustyError::Tool(e.to_string()))?;
        let results: Vec<Value> = hits
            .into_iter()
            .map(|hit| {
                json!({
                    "run_id": hit.run_id,
                    "thread_id": hit.thread_id,
                    "event_id": hit.event_id,
                    "seq": hit.seq,
                    "kind": hit.kind,
                    "field": hit.field,
                    "score": hit.score,
                    "excerpt": hit.excerpt,
                })
            })
            .collect();
        Ok(json!({"results": results}))
    }
}

/// `session_trace`: walk the causal neighborhood of one journaled event —
/// its ancestor chain and its descendants.
#[derive(Clone)]
pub struct SessionTraceTool {
    query: Arc<dyn JournalQuery>,
}

impl SessionTraceTool {
    /// The tool over a [`JournalQuery`] implementation.
    pub fn new(query: Arc<dyn JournalQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionTraceTool {
    fn name(&self) -> &str {
        "session_trace"
    }

    fn description(&self) -> &str {
        "Trace one journaled event's causal chain: its ancestors (what caused it) and \
         descendants (what it caused), in the journal's own order."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "minLength": 1},
                "event_id": {"type": "string", "minLength": 1,
                    "description": "the event to trace, `{run_id}:{seq}`"}
            },
            "required": ["run_id", "event_id"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let run_id = args
            .get("run_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RustyError::Tool("`run_id` must be a string".into()))?;
        let event_id = args
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RustyError::Tool("`event_id` must be a string".into()))?;
        let trace = self
            .query
            .trace(run_id, event_id)
            .await
            .map_err(|e| RustyError::Tool(e.to_string()))?;
        Ok(json!({
            "target": event_summary(&trace.target),
            "ancestors": trace.ancestors.iter().map(event_summary).collect::<Vec<_>>(),
            "descendants": trace.descendants.iter().map(event_summary).collect::<Vec<_>>(),
            "truncated": trace.truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Clock, EventDraft, Journal};
    use crate::record::EventStatus;

    /// A small journaled run: step → node input → model call + tool call
    /// (children of the input), tool error child of the model call.
    fn fixture_snapshot() -> JournalSnapshot {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        let step = journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        let input = journal.record(
            EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
                .node("agent")
                .parent(step),
        );
        let model = journal.record(
            EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
                .node("agent")
                .parent(input)
                .input(
                    json!({"messages": [{"role": "user", "content": "deploy the staging build"}]}),
                )
                .output(json!({"message": {"content": "deploying now"}, "model": "mock"})),
        );
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::NonIdempotent)
                .parent(model)
                .input(json!({"tool": "deploy", "arguments": {"env": "staging"}}))
                .output(json!({"deployment_id": "dep-42"})),
        );
        journal.snapshot()
    }

    fn fixture_query() -> InMemoryJournalQuery {
        let mut query = InMemoryJournalQuery::new();
        query.add_snapshot(fixture_snapshot());
        query
    }

    #[tokio::test]
    async fn search_finds_terms_in_payloads_with_excerpts() {
        let hits = fixture_query()
            .search(&SessionSearch {
                text: "staging".into(),
                run_id: None,
                limit: 10,
            })
            .await
            .unwrap();
        // Model-call input and tool-call input/output all mention staging?
        // — the model input and tool input do; the output does not.
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.run_id == "run-1"));
        assert!(hits.iter().any(|hit| hit.field == SearchField::Input));
        assert!(hits
            .iter()
            .all(|hit| hit.excerpt.len() <= MAX_EXCERPT_CHARS + 1));
    }

    #[tokio::test]
    async fn search_ranks_by_distinct_terms_and_clamps_limit() {
        let hits = fixture_query()
            .search(&SessionSearch {
                text: "staging deploy".into(),
                run_id: Some("run-1".into()),
                limit: usize::MAX,
            })
            .await
            .unwrap();
        // Two-term hits (model input, tool input) outrank the one-term hit
        // (the model output's "deploying" contains "deploy"); equal ranks
        // fall back to journal order.
        assert_eq!(hits[0].score, 2);
        assert_eq!(hits[0].kind, RunEventKind::ModelCall);
        assert_eq!(hits[1].score, 2);
        assert_eq!(hits[1].kind, RunEventKind::ToolCall);
        assert_eq!(hits[2].score, 1);
        assert!(hits.len() <= MAX_SEARCH_RESULTS);
    }

    #[tokio::test]
    async fn search_rejects_empty_and_oversize_queries() {
        let query = fixture_query();
        for text in ["", "   ", &"x".repeat(MAX_QUERY_BYTES + 1)] {
            let err = query
                .search(&SessionSearch {
                    text: text.into(),
                    run_id: None,
                    limit: 5,
                })
                .await
                .unwrap_err();
            assert!(matches!(err, RustyError::InvalidUpdate(_)));
        }
    }

    #[tokio::test]
    async fn trace_walks_parent_links_both_ways() {
        let trace = fixture_query().trace("run-1", "run-1:3").await.unwrap();
        // The tool call's chain: step start → node input → model call.
        let ancestor_ids: Vec<&str> = trace
            .ancestors
            .iter()
            .map(|event| event.id.as_str())
            .collect();
        assert_eq!(ancestor_ids, ["run-1:0", "run-1:1", "run-1:2"]);
        assert_eq!(trace.target.kind, RunEventKind::ToolCall);
        assert!(trace.descendants.is_empty());
        assert!(!trace.truncated);

        // From the node input, the model call is the descendant.
        let trace = fixture_query().trace("run-1", "run-1:1").await.unwrap();
        let descendant_ids: Vec<&str> = trace
            .descendants
            .iter()
            .map(|event| event.id.as_str())
            .collect();
        assert_eq!(descendant_ids, ["run-1:2", "run-1:3"]);
    }

    #[tokio::test]
    async fn trace_names_unknown_runs_and_events() {
        let query = fixture_query();
        let err = query.trace("run-9", "run-9:0").await.unwrap_err();
        assert!(err.to_string().contains("run-9"));
        let err = query.trace("run-1", "run-1:99").await.unwrap_err();
        assert!(err.to_string().contains("run-1:99"));
    }

    #[tokio::test]
    async fn bounded_reads_page_by_sequence() {
        let query = fixture_query();
        let first = query.read_events("run-1", None, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].seq, 0);
        let rest = query
            .read_events("run-1", Some(first[1].seq), MAX_READ_EVENTS)
            .await
            .unwrap();
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].seq, 2);
        // A missing run reads as empty, never an error.
        assert!(query
            .read_events("run-9", None, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn file_backend_reads_the_server_layout() {
        let dir =
            std::env::temp_dir().join(format!("rusty-session-query-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let snapshot = fixture_snapshot();
        std::fs::write(
            dir.join("run-1.json"),
            serde_json::to_vec_pretty(&snapshot).unwrap(),
        )
        .unwrap();
        let query = FileJournalQuery::new(&dir);

        let hits = query
            .search(&SessionSearch {
                text: "dep-42".into(),
                run_id: None,
                limit: 5,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].field, SearchField::Output);

        let trace = query.trace("run-1", "run-1:3").await.unwrap();
        assert_eq!(trace.ancestors.len(), 3);

        // A named-but-absent run: empty search, typed miss on trace.
        assert!(query
            .search(&SessionSearch {
                text: "staging".into(),
                run_id: Some("run-9".into()),
                limit: 5,
            })
            .await
            .unwrap()
            .is_empty());
        assert!(query.trace("run-9", "run-9:0").await.is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn session_search_tool_validates_and_reports() {
        let tool = SessionSearchTool::new(Arc::new(fixture_query()));
        assert_eq!(tool.name(), "session_search");
        assert_eq!(tool.effect(), Effect::ReadOnly);

        let out = tool
            .call(json!({"query": "staging", "limit": 5}))
            .await
            .unwrap();
        assert_eq!(out["results"].as_array().unwrap().len(), 2);

        let err = tool.call(json!({"limit": 5})).await.unwrap_err();
        assert!(matches!(err, RustyError::Tool(_)));
    }

    #[tokio::test]
    async fn session_trace_tool_renders_the_chain() {
        let tool = SessionTraceTool::new(Arc::new(fixture_query()));
        assert_eq!(tool.name(), "session_trace");

        let out = tool
            .call(json!({"run_id": "run-1", "event_id": "run-1:3"}))
            .await
            .unwrap();
        assert_eq!(out["target"]["kind"], json!("tool_call"));
        assert_eq!(out["ancestors"].as_array().unwrap().len(), 3);
        assert_eq!(out["truncated"], json!(false));

        let err = tool
            .call(json!({"run_id": "run-1", "event_id": "run-1:99"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("run-1:99"));
    }

    #[tokio::test]
    async fn search_does_not_match_failed_or_absent_payloads() {
        let journal = Journal::new("run-2", "thread-2", Clock::System);
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::NonIdempotent)
                .status(EventStatus::Error),
        );
        let mut query = InMemoryJournalQuery::new();
        query.add_snapshot(journal.snapshot());
        let hits = query
            .search(&SessionSearch {
                text: "anything".into(),
                run_id: None,
                limit: 5,
            })
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
