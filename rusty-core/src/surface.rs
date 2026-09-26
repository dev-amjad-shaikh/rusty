//! The conversation surface: a derived, rewriteable view over the immutable
//! Flight Recorder journal (R0.13 parity wave).
//!
//! The journal ([`crate::journal`]) is append-only truth: nothing recorded is
//! ever edited, and exact replay re-drives runs from it byte-for-byte. A
//! conversation, though, is not an append-only artifact — a long run's
//! context window needs *compaction* (old turns replaced by a summary), and a
//! live session appends turns that are not journaled yet. The surface
//! reconciles the two: **the log never lies, the surface mutates.**
//!
//! #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]Derivation model
//!
//! The journal has no message events; the conversation lives inside channel
//! evidence. [`Surface::derive`] reconstructs the `messages` channel
//! ([`crate::react::MESSAGES_CHANNEL`]) by folding exactly what is journaled:
//!
//! - **Seed.** The run's initial messages enter the journal exactly once — in
//!   the first [`RunEventKind::NodeInput`] event, whose input is the scoped
//!   state snapshot the first invocation observed. Those messages cite that
//!   event's seq.
//! - **Appends.** Every [`RunEventKind::NodeOutput`] whose `updates` carry
//!   `messages` contributes its message(s), folded with the channel's own
//!   [`crate::state::Reducer::AddMessages`] semantics (id-aware upsert, else
//!   append). Each appended message cites the node-output event plus the
//!   seqs of the conversational effects ([`RunEventKind::ModelCall`] /
//!   [`RunEventKind::ToolCall`]) recorded *in the same invocation* — found by
//!   causal parentage ([`crate::journal::PARENT_EVENT_KEY`]), never by
//!   content guessing.
//!
//! Model-call *requests* and super-step barrier values echo the channel but
//! append nothing, so they are not surface sources — deriving from them
//! would double-count. Oversized payloads resolve through the snapshot's
//! artifact map exactly as replay resolves them.
//!
//! #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]Surface ops
//!
//! Two ops, applied to the derived surface — never to the journal:
//!
//! - [`SurfaceOp::Append`] — a live turn not journaled yet
//!   ([`Provenance::Live`], no citations).
//! - [`SurfaceOp::Replace`] — positional span replacement: entries
//!   `[start, end)` fold into one entry, the compaction summary.
//!
//! Every applied op is recorded as a [`SurfaceRevision`] chained to its
//! parent, so the full compaction history is auditable and the pre-compaction
//! surface is always recoverable: [`Surface::view_at`]`(0)` replays zero
//! revisions — the journal-derived base — and any prefix replays history.
//!
//! #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]Honesty rules
//!
//! - A replacement's citations must be *exactly* the sorted union of the
//!   subsumed entries' citations: a seq invented is fabrication, a seq
//!   dropped is a gap leak; both are rejected.
//! - Every cited seq must be in range of the journal the surface derived
//!   from.
//! - A replacement entry must be marked [`Provenance::Compaction`] — a
//!   summary may never masquerade as journaled content.
//! - An appended entry must be [`Provenance::Live`] with no citations —
//!   unjournaled content may not pretend to evidence.
//!
//! Replay and fork paths are untouched: they read the raw journal and never
//! consult a surface.
//!
//! #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]Context-window use
//!
//! [`Surface::messages`] projects the current (possibly compacted) surface
//! into the `Vec<ChatMessage>` a model node consumes — journal-derived
//! entries verbatim, summaries as system messages. It is a pure function the
//! ReAct layer can adopt once both streams settle; nothing here is wired
//! into [`crate::react`] yet.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::journal::JournalSnapshot;
use crate::llm::{ChatMessage, Role};
use crate::react::MESSAGES_CHANNEL;
use crate::record::{EventStatus, PayloadRef, RunEvent, RunEventKind};

/// Where a surface entry's content comes from.
///
/// The provenance is the honesty marker: synthesized content is labeled at
/// construction and the op validation refuses to let it pose as journaled
/// evidence (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Folded out of journaled channel evidence; `source_seqs` cites the
    /// events it came from.
    Journal,

    /// Synthesized by a compaction — a summary standing in for the entries
    /// it subsumes. Never produced by the journal.
    Compaction,

    /// Appended to the surface but not journaled yet (a live turn).
    Live,
}

/// What kind of conversational turn an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceEntryKind {
    /// A user turn.
    User,
    /// An assistant turn (a final answer or a tool-call request).
    Assistant,
    /// A tool result answering an assistant tool call.
    ToolResult,
    /// A system message carried by the journaled channel.
    System,
    /// A compaction summary ([`Provenance::Compaction`]).
    Summary,
}

impl SurfaceEntryKind {
    fn of(message: &ChatMessage) -> Self {
        match message.role {
            Role::User => SurfaceEntryKind::User,
            Role::Assistant => SurfaceEntryKind::Assistant,
            Role::Tool => SurfaceEntryKind::ToolResult,
            Role::System => SurfaceEntryKind::System,
        }
    }
}

/// One turn of the conversation surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceEntry {
    /// The conversational kind.
    pub kind: SurfaceEntryKind,

    /// The turn as a chat message — verbatim for journaled entries, the
    /// summary text (as a system message) for compactions.
    pub message: ChatMessage,

    /// Where the content comes from.
    pub provenance: Provenance,

    /// The journal seqs this entry derives from, sorted and deduped. Empty
    /// for live (unjournaled) entries; exactly the subsumed union for
    /// compaction summaries.
    pub source_seqs: Vec<u64>,
}

impl SurfaceEntry {
    fn journaled(message: ChatMessage, source_seqs: Vec<u64>) -> Self {
        Self {
            kind: SurfaceEntryKind::of(&message),
            message,
            provenance: Provenance::Journal,
            source_seqs: normalized(source_seqs),
        }
    }

    /// A compaction summary: `text` as a system message citing `source_seqs`
    /// (the entries it subsumes), provenance [`Provenance::Compaction`].
    pub fn summary(text: impl Into<String>, source_seqs: Vec<u64>) -> Self {
        Self {
            kind: SurfaceEntryKind::Summary,
            message: ChatMessage::system(text),
            provenance: Provenance::Compaction,
            source_seqs: normalized(source_seqs),
        }
    }

    /// A live turn: appended to the surface but not journaled yet.
    pub fn live(message: ChatMessage) -> Self {
        Self {
            kind: SurfaceEntryKind::of(&message),
            message,
            provenance: Provenance::Live,
            source_seqs: Vec::new(),
        }
    }
}

/// Sorted and deduped — the canonical citation form validation compares.
fn normalized(mut seqs: Vec<u64>) -> Vec<u64> {
    seqs.sort_unstable();
    seqs.dedup();
    seqs
}

/// One edit to the surface. Ops apply to the surface only; the journal is
/// immutable and never sees them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SurfaceOp {
    /// Append a live turn to the end of the surface.
    Append {
        /// The turn to append; must be [`Provenance::Live`].
        entry: SurfaceEntry,
    },

    /// Replace the entries in `[start, end)` with a single entry — the
    /// compaction of that span.
    Replace {
        /// First subsumed entry (inclusive), positional in the current view.
        start: usize,
        /// Last subsumed entry (exclusive), positional in the current view.
        end: usize,
        /// The replacement; must be [`Provenance::Compaction`] and cite
        /// exactly the subsumed entries' citations.
        entry: SurfaceEntry,
    },
}

/// One applied op in the surface's history. Revisions chain by `parent`, so
/// the whole compaction history is auditable and every intermediate view is
/// recoverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceRevision {
    /// This revision's ordinal in the chain.
    pub id: u64,

    /// The revision it builds on (`None` for the first).
    pub parent: Option<u64>,

    /// The op that produced it.
    pub op: SurfaceOp,
}

/// A conversation surface derived from a journal snapshot, plus the revision
/// chain of ops applied on top of it.
///
/// Cheap to hold: the base entries are the derived conversation, and views
/// fold the (short) revision chain on demand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Surface {
    /// Number of events in the journal the base derived from — the citation
    /// range bound.
    journal_events: u64,

    /// The journal-derived conversation, before any op.
    base: Vec<SurfaceEntry>,

    /// The applied ops, in order; `revisions[i].id == i`.
    revisions: Vec<SurfaceRevision>,
}

impl Surface {
    /// Derive the conversation surface from a journal snapshot.
    ///
    /// Folds the `messages` channel evidence (see the module docs for the
    /// model). Payloads resolve through the snapshot's artifact map; an
    /// event whose payload cannot be resolved, a `messages` value that is
    /// not an array, or a message that is not a [`ChatMessage`] fails the
    /// derivation — a surface that silently dropped evidence would lie by
    /// omission.
    ///
    /// Integrity of the snapshot itself (chained head hash, artifact
    /// content hashes) is [`crate::journal::Journal::from_snapshot`]'s
    /// contract; derive trusts a snapshot the way replay trusts one that
    /// loaded.
    pub fn derive(snapshot: &JournalSnapshot) -> Result<Self> {
        // Causal index: a node invocation (its node-input event id) → the
        // seqs of the conversational effects recorded inside it. Parentage,
        // not content matching, is what ties an appended message to the
        // model or tool call that produced it.
        let mut invocation_effects: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for event in &snapshot.events {
            if matches!(event.kind, RunEventKind::ModelCall | RunEventKind::ToolCall) {
                if let Some(parent) = event.parent.as_deref() {
                    invocation_effects
                        .entry(parent)
                        .or_default()
                        .push(event.seq);
                }
            }
        }

        let mut fold = MessageFold::default();
        let mut seeded = false;
        for event in &snapshot.events {
            match event.kind {
                // The run's initial messages are journaled exactly once: in
                // the first invocation's input state. Later node inputs echo
                // the growing channel — they are observations, not appends.
                RunEventKind::NodeInput if !seeded => {
                    seeded = true;
                    if let Some(input) = resolve(snapshot, event, &event.input)? {
                        if let Some(messages) = input.get(MESSAGES_CHANNEL) {
                            for message in message_array(event, messages)? {
                                fold.upsert(message.clone(), &[event.seq]);
                            }
                        }
                    }
                }
                RunEventKind::NodeOutput if event.status == EventStatus::Ok => {
                    let Some(output) = resolve(snapshot, event, &event.output)? else {
                        continue;
                    };
                    let Some(update) = output
                        .get("updates")
                        .and_then(|updates| updates.get(MESSAGES_CHANNEL))
                    else {
                        continue;
                    };
                    // The entry's evidence: the node output that carried the
                    // message plus every conversational effect the same
                    // invocation recorded.
                    let mut seqs = vec![event.seq];
                    if let Some(parent) = event.parent.as_deref() {
                        if let Some(effects) = invocation_effects.get(parent) {
                            seqs.extend_from_slice(effects);
                        }
                    }
                    match update {
                        Value::Array(messages) => {
                            for message in messages {
                                fold.upsert(message.clone(), &seqs);
                            }
                        }
                        Value::Object(_) => fold.upsert(update.clone(), &seqs),
                        other => {
                            return Err(malformed(
                                event,
                                format!(
                                    "`updates.{MESSAGES_CHANNEL}` is neither a message nor an \
                                     array of messages: {other}"
                                ),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut base = Vec::with_capacity(fold.messages.len());
        for (message, seqs) in fold.messages.into_iter().zip(fold.seqs) {
            let message: ChatMessage = serde_json::from_value(message).map_err(|e| {
                RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("journaled `{MESSAGES_CHANNEL}` entry is not a chat message: {e}"),
                )))
            })?;
            base.push(SurfaceEntry::journaled(message, seqs));
        }

        Ok(Self {
            journal_events: snapshot.events.len() as u64,
            base,
            revisions: Vec::new(),
        })
    }

    /// Number of events in the journal this surface derived from.
    pub fn journal_events(&self) -> u64 {
        self.journal_events
    }

    /// The journal-derived conversation, before any op — the pre-compaction
    /// surface, always recoverable because it is stored, not recomputed.
    pub fn base(&self) -> &[SurfaceEntry] {
        &self.base
    }

    /// The revision chain, oldest first.
    pub fn revisions(&self) -> &[SurfaceRevision] {
        &self.revisions
    }

    /// The current surface: the base with every revision applied.
    pub fn entries(&self) -> Vec<SurfaceEntry> {
        self.view_at(self.revisions.len())
            .expect("the full revision chain always folds")
    }

    /// The surface after exactly `count` revisions (`0` is the derived
    /// base). `None` when `count` exceeds the chain.
    pub fn view_at(&self, count: usize) -> Option<Vec<SurfaceEntry>> {
        if count > self.revisions.len() {
            return None;
        }
        let mut view = self.base.clone();
        for revision in &self.revisions[..count] {
            match &revision.op {
                SurfaceOp::Append { entry } => view.push(entry.clone()),
                SurfaceOp::Replace { start, end, entry } => {
                    // Applied ops were validated against the view they
                    // replaced into, so the span is in range here by
                    // construction.
                    view.splice(*start..*end, [entry.clone()]);
                }
            }
        }
        Some(view)
    }

    /// The current surface as the message list a model node consumes:
    /// journaled entries verbatim, compaction summaries as their system
    /// message. Pure — the ReAct layer can adopt it when the integration
    /// lands; nothing calls it from the run path today.
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.entries()
            .into_iter()
            .map(|entry| entry.message)
            .collect()
    }

    /// Apply an op, validating it against the current view and the honesty
    /// rules (module docs). Returns the new revision's id.
    pub fn apply(&mut self, op: SurfaceOp) -> Result<u64> {
        let view = self.entries();
        match &op {
            SurfaceOp::Append { entry } => {
                if entry.provenance != Provenance::Live || !entry.source_seqs.is_empty() {
                    return Err(dishonest(
                        "an appended entry is not journaled evidence: it must be \
                         `Provenance::Live` with no citations",
                    ));
                }
            }
            SurfaceOp::Replace { start, end, entry } => {
                if start >= end || *end > view.len() {
                    return Err(dishonest(format!(
                        "replace span [{start}, {end}) is out of range for a surface of \
                         {} entries",
                        view.len()
                    )));
                }
                if entry.provenance != Provenance::Compaction {
                    return Err(dishonest(
                        "a replacement must be marked `Provenance::Compaction` — synthesized \
                         content may not masquerade as journaled",
                    ));
                }
                let subsumed: Vec<u64> = normalized(
                    view[*start..*end]
                        .iter()
                        .flat_map(|subsumed| subsumed.source_seqs.iter().copied())
                        .collect(),
                );
                let cited = normalized(entry.source_seqs.clone());
                if let Some(&seq) = cited.iter().find(|&&seq| seq >= self.journal_events) {
                    return Err(dishonest(format!(
                        "citation seq {seq} is out of range: the journal has {} events",
                        self.journal_events
                    )));
                }
                if cited != subsumed {
                    return Err(dishonest(format!(
                        "a replacement must cite exactly the seqs it subsumes: the span cites \
                         {subsumed:?} but the summary claims {cited:?} — a dropped seq leaks a \
                         gap, an invented one fabricates evidence",
                    )));
                }
            }
        }
        let id = self.revisions.len() as u64;
        self.revisions.push(SurfaceRevision {
            id,
            parent: id.checked_sub(1),
            op,
        });
        Ok(id)
    }

    /// Compact the span `[start, end)` of the current view into one summary
    /// entry, citing exactly what the span subsumes. Convenience over
    /// [`Surface::apply`] for the common case; the summarizer that produces
    /// `text` is the caller's concern (a model call, a heuristic — the
    /// surface only polices honesty, not prose).
    pub fn compact(&mut self, start: usize, end: usize, text: impl Into<String>) -> Result<u64> {
        let view = self.entries();
        if start >= end || end > view.len() {
            return Err(dishonest(format!(
                "compact span [{start}, {end}) is out of range for a surface of {} entries",
                view.len()
            )));
        }
        let seqs = normalized(
            view[start..end]
                .iter()
                .flat_map(|entry| entry.source_seqs.iter().copied())
                .collect(),
        );
        self.apply(SurfaceOp::Replace {
            start,
            end,
            entry: SurfaceEntry::summary(text, seqs),
        })
    }
}

/// The fold state of channel derivation: raw message values (so the
/// reducer's id-aware upsert sees the same JSON the reducer would) plus
/// each position's accumulated citations.
#[derive(Default)]
struct MessageFold {
    messages: Vec<Value>,
    seqs: Vec<Vec<u64>>,
}

impl MessageFold {
    /// [`crate::state::Reducer::AddMessages`] semantics: a message whose
    /// string `id` matches an existing entry replaces it in place (merging
    /// citations — both events evidenced this position); anything else
    /// appends.
    fn upsert(&mut self, message: Value, seqs: &[u64]) {
        let id = message.get("id").and_then(Value::as_str);
        if let Some(id) = id {
            if let Some(position) = self
                .messages
                .iter()
                .position(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
            {
                self.messages[position] = message;
                let merged = std::mem::take(&mut self.seqs[position]);
                self.seqs[position] = normalized(merged.iter().chain(seqs).copied().collect());
                return;
            }
        }
        self.messages.push(message);
        self.seqs.push(normalized(seqs.to_vec()));
    }
}

/// A `messages` payload that must be an array (the node-input seed).
fn message_array<'a>(event: &RunEvent, value: &'a Value) -> Result<&'a Vec<Value>> {
    value.as_array().ok_or_else(|| {
        malformed(
            event,
            format!("`{MESSAGES_CHANNEL}` in the input state is not an array: {value}"),
        )
    })
}

/// Resolve an event payload through the snapshot's artifact map — the same
/// lookup [`crate::journal::Journal::resolve`] performs, over the borrowed
/// snapshot. A missing payload or unresolvable artifact is a derivation
/// error, not a skipped entry.
fn resolve(
    snapshot: &JournalSnapshot,
    event: &RunEvent,
    payload: &Option<PayloadRef>,
) -> Result<Option<Value>> {
    match payload {
        None => Ok(None),
        Some(PayloadRef::Inline(value)) => Ok(Some(value.clone())),
        Some(PayloadRef::Artifact(reference)) => snapshot
            .artifacts
            .get(&reference.sha256)
            .cloned()
            .map(Some)
            .ok_or_else(|| {
                malformed(
                    event,
                    format!(
                        "payload is artifact {} but the snapshot's artifact map does not hold it \
                         (a truncated or externalized snapshot)",
                        reference.sha256
                    ),
                )
            }),
    }
}

/// A malformed-evidence derivation failure.
fn malformed(event: &RunEvent, detail: String) -> RustyError {
    RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "surface derivation: event {} ({:?}): {detail}",
            event.id, event.kind
        ),
    )))
}

/// A dishonest-op rejection.
fn dishonest(detail: impl Into<String>) -> RustyError {
    RustyError::InvalidUpdate(format!("surface: {}", detail.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Clock, EventDraft, Journal};
    use crate::record::Effect;
    use serde_json::json;

    /// Journal a minimal two-turn conversation in the executor's shapes:
    /// seeded user message, assistant output parented to a model call.
    fn conversation_journal() -> Journal {
        let journal = Journal::new("run-surface", "thread-surface", Clock::System);
        let input = journal.record(
            EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
                .node("agent")
                .input(json!({
                    "messages": [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
                })),
        );
        journal.record(
            EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
                .node("agent")
                .parent(input.clone()),
        );
        journal.record(
            EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
                .node("agent")
                .output(json!({
                    "updates": {
                        "messages": serde_json::to_value(ChatMessage::assistant("hello")).unwrap()
                    },
                    "command": null
                }))
                .parent(input),
        );
        journal
    }

    #[test]
    fn derivation_seeds_and_appends_with_causal_citations() {
        let journal = conversation_journal();
        let surface = Surface::derive(&journal.snapshot()).unwrap();

        assert_eq!(surface.journal_events(), 3);
        let base = surface.base();
        assert_eq!(base.len(), 2);
        assert_eq!(base[0].kind, SurfaceEntryKind::User);
        assert_eq!(base[0].provenance, Provenance::Journal);
        assert_eq!(base[0].source_seqs, vec![0]);
        assert_eq!(base[1].kind, SurfaceEntryKind::Assistant);
        // The node output and the same invocation's model call.
        assert_eq!(base[1].source_seqs, vec![1, 2]);
    }

    #[test]
    fn append_then_compact_chains_recoverable_revisions() {
        let journal = conversation_journal();
        let mut surface = Surface::derive(&journal.snapshot()).unwrap();

        let first = surface
            .apply(SurfaceOp::Append {
                entry: SurfaceEntry::live(ChatMessage::user("again")),
            })
            .unwrap();
        assert_eq!(first, 0);
        let second = surface.compact(0, 2, "greeting exchange").unwrap();
        assert_eq!(second, 1);
        assert_eq!(surface.revisions()[1].parent, Some(0));

        let entries = surface.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, SurfaceEntryKind::Summary);
        assert_eq!(entries[0].provenance, Provenance::Compaction);
        assert_eq!(entries[0].source_seqs, vec![0, 1, 2]);
        assert_eq!(entries[1].kind, SurfaceEntryKind::User);

        // Every prefix of history is recoverable.
        assert_eq!(surface.view_at(0).unwrap().len(), 2);
        assert_eq!(surface.view_at(1).unwrap().len(), 3);
        assert!(surface.view_at(3).is_none());
    }

    #[test]
    fn dishonest_replacements_are_rejected() {
        let journal = conversation_journal();
        let mut surface = Surface::derive(&journal.snapshot()).unwrap();

        // A summary masquerading as journaled content.
        let masquerade = SurfaceOp::Replace {
            start: 0,
            end: 2,
            entry: SurfaceEntry {
                source_seqs: vec![0, 1, 2],
                ..SurfaceEntry::journaled(ChatMessage::assistant("fake"), vec![0, 1, 2])
            },
        };
        assert!(surface.apply(masquerade).is_err());

        // A gap leak: the summary drops a subsumed seq.
        let leak = SurfaceOp::Replace {
            start: 0,
            end: 2,
            entry: SurfaceEntry::summary("x", vec![0, 2]),
        };
        assert!(surface.apply(leak).is_err());

        // Fabrication: the summary invents a citation.
        let fabricated = SurfaceOp::Replace {
            start: 0,
            end: 1,
            entry: SurfaceEntry::summary("x", vec![0, 99]),
        };
        let err = surface.apply(fabricated).unwrap_err().to_string();
        assert!(err.contains("out of range"), "got: {err}");

        // An out-of-range span.
        assert!(surface.compact(1, 9, "x").is_err());
        assert!(surface.compact(1, 1, "x").is_err());

        // An append pretending to evidence.
        let append = SurfaceOp::Append {
            entry: SurfaceEntry {
                provenance: Provenance::Journal,
                ..SurfaceEntry::live(ChatMessage::user("x"))
            },
        };
        assert!(surface.apply(append).is_err());

        // Nothing was recorded: the chain is still empty.
        assert!(surface.revisions().is_empty());
    }

    #[test]
    fn messages_projection_is_the_current_view() {
        let journal = conversation_journal();
        let mut surface = Surface::derive(&journal.snapshot()).unwrap();
        surface.compact(0, 2, "said hello").unwrap();

        let messages = surface.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].content.as_deref(), Some("said hello"));
    }

    #[test]
    fn artifact_referenced_payloads_resolve_into_the_surface() {
        let journal = Journal::new("run-big", "thread-big", Clock::System);
        let big = "x".repeat(crate::record::INLINE_PAYLOAD_MAX_BYTES);
        let input = journal.record(
            EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
                .node("agent")
                .input(json!({
                    "messages": [serde_json::to_value(ChatMessage::user(&big)).unwrap()]
                })),
        );
        journal.record(
            EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
                .node("agent")
                .output(json!({
                    "updates": {
                        "messages": serde_json::to_value(ChatMessage::assistant("ok")).unwrap()
                    },
                    "command": null
                }))
                .parent(input),
        );

        let snapshot = journal.snapshot();
        assert!(!snapshot.artifacts.is_empty(), "the big seed spilled");
        let surface = Surface::derive(&snapshot).unwrap();
        assert_eq!(
            surface.base()[0].message.content.as_deref(),
            Some(big.as_str())
        );
        assert_eq!(surface.base().len(), 2);
    }

    #[test]
    fn unresolvable_payloads_fail_derivation_loudly() {
        let journal = conversation_journal();
        let mut snapshot = journal.snapshot();
        snapshot.events[0].input = Some(PayloadRef::Artifact(crate::record::ArtifactRef {
            sha256: "0".repeat(64),
            bytes: 1,
        }));
        let err = Surface::derive(&snapshot).unwrap_err().to_string();
        assert!(err.contains("artifact"), "got: {err}");
    }
}
