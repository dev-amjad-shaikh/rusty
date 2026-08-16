//! The durable inbox: a per-run, two-queue message intake with typed
//! cancellation (R0.13 parity wave).
//!
//! One inbox per run carries everything the outside world has to say to a
//! live execution, in two ordered queues:
//!
//! - **next-step steering** — consumed at the next super-step boundary and
//!   delivered to the step's nodes as user-role input before their next
//!   model call ([`Inbox::steer`]);
//! - **next-turn follow-ups** — consumed when the current turn would
//!   otherwise end: instead of idling, the run re-activates the graph's
//!   entry point with the follow-ups delivered as user-role input
//!   ([`Inbox::followup`]).
//!
//! A third intake, [`Inbox::inject`], stages context without waking the
//! loop: staged messages ride along with the next *wake* (a steering batch,
//! a follow-up turn extension, a fresh run start, or a resume) and never
//! cause one on their own. [`Inbox::send`] is the general form the three
//! aliases resolve to: `send(sender, target, wakeup, content)` maps
//! `(NextStep, wake)` → steering, `(NextStep, no wake)` → staged injection,
//! `(NextTurn, _)` → follow-up.
//!
//! # Durability
//!
//! Every mutation the run observes is a journaled event. Sends land in a
//! pending buffer first; the executor **settles** pending sends at defined
//! points — every super-step boundary, the turn-end check, the cancellation
//! check — journaling one [`RunEventKind::InboxIntake`] per message as it
//! enters a queue. Consumption journals [`RunEventKind::InboxConsumed`] with
//! the exact batch and the point it left the inbox. Typed cancellation
//! ([`Inbox::cancel`]) journals [`RunEventKind::RunCancelled`] with its
//! [`CancelCause`] and `keep_inbox` disposition.
//!
//! Settlement-time journaling is what makes the inbox exactly replayable:
//! the journal — not wall-clock arrival — fixes each message's position in
//! the run's evidence, and a message that never reached a settlement point
//! is not part of the durable record. [`Inbox::replaying`] rebuilds the
//! intake schedule from a recorded journal, releasing each message (and each
//! cancellation) when the replayed journal reaches the seq its intake was
//! recorded at, so a re-driven run settles the same messages at the same
//! points and journals byte-identical events.
//!
//! The queue contents themselves cross checkpoints: every checkpoint header
//! stamps an [`InboxSnapshot`] (additively — absent when the inbox was never
//! used, so pre-inbox checkpoints and inbox-free runs keep their exact
//! bytes), and resume seeds a fresh inbox from it. A cancellation with
//! `keep_inbox: false` clears the queues and rewrites the boundary
//! checkpoint, so the durable record and the resumed run agree that the
//! messages are gone; `keep_inbox: true` leaves them queued for the resume
//! that follows.
//!
//! # Empty means absent
//!
//! An attached but untouched inbox journals nothing, delivers nothing,
//! stamps nothing, and changes no checkpoint byte: zero-inbox runs are
//! byte-identical to runs without an inbox.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::journal::{EventDraft, Journal, JournalSnapshot};
use crate::record::{Effect, PayloadRef, RunEventKind};

/// The [`crate::node::NodeConfig`] extra key under which the executor hands
/// a super-step's drained inbox batch to that step's node invocations.
///
/// The value is the JSON serialization of `Vec<InboxMessage>` — the drained
/// steering (plus any staged messages and turn-extension follow-ups riding
/// the same wake), in intake order. Nodes that speak the inbox protocol
/// (the prebuilt ReAct agent does) parse it; every other node ignores it.
/// The key is absent whenever the batch is empty.
pub const INBOX_DELIVERY_KEY: &str = "rusty.inbox_delivery";

/// The default sender provenance for the convenience aliases
/// ([`Inbox::steer`] / [`Inbox::followup`] / [`Inbox::inject`]).
pub const DEFAULT_SENDER: &str = "user";

/// Which queue a [`Inbox::send`] targets: the next super-step boundary
/// (steering / staged injection, selected by `wakeup`) or the next turn
/// (follow-ups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxTarget {
    /// Consume at the next super-step boundary.
    NextStep,
    /// Consume when the current turn would otherwise end.
    NextTurn,
}

/// The queue an inbox message lives in, fixed at intake by the
/// target/wakeup pair it was sent with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    /// Steering: delivered at the next super-step boundary.
    Steering,
    /// Follow-up: delivered when the turn would otherwise end; extends the
    /// run into another turn instead of idling.
    FollowUp,
    /// Staged context: delivered with the next wake (a steering batch, a
    /// follow-up extension, a run start, a resume); never wakes on its own.
    Injected,
}

/// Why a run was cancelled through the inbox. Closed set; the executor
/// matches exhaustively and the journaled [`RunEventKind::RunCancelled`]
/// event carries it, so an audit reads *who* ended the run, not just *that*
/// it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelCause {
    /// The human (or the client acting for them) asked to stop.
    User,
    /// A supervising parent run or agent cancelled its child.
    Parent,
    /// A hook / policy gate cancelled the run it observed.
    Hook,
    /// The run's owner was dropped: teardown, not a decision about the run.
    Disposed,
}

impl std::fmt::Display for CancelCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            CancelCause::User => "user",
            CancelCause::Parent => "parent",
            CancelCause::Hook => "hook",
            CancelCause::Disposed => "disposed",
        };
        f.write_str(name)
    }
}

/// One message held by (or settled into) an inbox.
///
/// `seq` is the inbox's own monotonic intake sequence, assigned at send time
/// and preserved across checkpoints and replay — the total order of "what
/// arrived", distinct from the journal's `seq` that fixes when the run
/// *observed* it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Monotonic intake sequence within the inbox.
    pub seq: u64,
    /// Which queue the message lives in.
    pub kind: InboxKind,
    /// Provenance: who sent it (a user id, `parent:{run_id}`, a hook name).
    pub sender: String,
    /// The message payload. A JSON string is delivered to chat models as its
    /// text; anything else is delivered as its canonical JSON encoding.
    pub content: Value,
}

/// Where a drained batch left the inbox, journaled on every
/// [`RunEventKind::InboxConsumed`] event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumptionPoint {
    /// A super-step boundary mid-turn (steering, plus staged messages riding
    /// the wake).
    StepBoundary,
    /// The first super-step of a turn: a fresh run start, a resume, or the
    /// first step after a follow-up extension.
    TurnStart,
    /// The turn-end check: follow-ups (plus staged messages) consumed to
    /// extend the run into another turn instead of finishing.
    TurnExtension,
}

/// The journaled payload of a [`RunEventKind::InboxConsumed`] event: the
/// exact batch that left the inbox, where, and at which super-step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxConsumption {
    /// Where the batch was consumed.
    pub point: ConsumptionPoint,
    /// The super-step the consumption belongs to (the upcoming step for
    /// boundary/turn-start batches, the ended step for extensions).
    pub step: usize,
    /// The drained messages, in intake order.
    pub messages: Vec<InboxMessage>,
}

/// How many messages a `keep_inbox: false` cancellation dropped, journaled
/// on the [`RunEventKind::RunCancelled`] event so the erasure is evidence,
/// not silence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedMessages {
    /// Settled steering messages dropped.
    pub steering: usize,
    /// Settled follow-ups dropped.
    pub followups: usize,
    /// Staged injections dropped.
    pub staged: usize,
    /// Sent but never settled (never journaled) messages dropped.
    pub pending: usize,
}

/// The journaled payload of a [`RunEventKind::RunCancelled`] event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCancellation {
    /// Who cancelled the run.
    pub cause: CancelCause,
    /// Whether queued messages survive for the resume that follows.
    pub keep_inbox: bool,
    /// What a `keep_inbox: false` cancellation dropped (all zero when
    /// keeping).
    pub dropped: DroppedMessages,
}

/// The durable queue state of an inbox, stamped into every checkpoint
/// header of an inbox-using run ([`crate::record::CheckpointHeader::inbox`])
/// and restored on resume.
///
/// Pending (sent but unsettled) messages are deliberately absent: a send is
/// not part of the durable record until the run settles and journals it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxSnapshot {
    /// Queued steering messages, in intake order.
    pub steering: Vec<InboxMessage>,
    /// Queued follow-ups, in intake order.
    pub followups: Vec<InboxMessage>,
    /// Staged injections, in intake order.
    pub staged: Vec<InboxMessage>,
    /// The next intake sequence number, so a resumed inbox never reissues a
    /// sequence a journaled message already carries.
    pub next_seq: u64,
    /// The delivery batch the run had already consumed (and journaled) but
    /// not yet delivered when the checkpoint was taken: a follow-up turn
    /// extension's batch riding to the next super-step, or an interrupted
    /// step's whole batch restored so the re-executed step sees exactly
    /// what the discarded attempt saw. Empty in the common case; absent
    /// from the wire then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_delivery: Vec<InboxMessage>,
}

/// Queue capacities. A send that would overflow its queue (counting both
/// settled and pending messages of its kind) fails at send time rather than
/// dropping silently later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxBounds {
    /// Sent but not yet settled messages.
    pub max_pending: usize,
    /// Settled steering messages.
    pub max_steering: usize,
    /// Settled follow-ups.
    pub max_followups: usize,
    /// Staged injections.
    pub max_staged: usize,
}

impl Default for InboxBounds {
    fn default() -> Self {
        Self {
            max_pending: 1024,
            max_steering: 256,
            max_followups: 256,
            max_staged: 256,
        }
    }
}

/// Resolve a journaled payload against a snapshot's embedded artifacts (the
/// [`JournalSnapshot::find_effect_receipt`] pattern): inline values return
/// as stored, artifact references look the bytes up by content hash.
fn resolve_payload(snapshot: &JournalSnapshot, payload: &PayloadRef) -> Option<Value> {
    match payload {
        PayloadRef::Inline(value) => Some(value.clone()),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256).cloned(),
    }
}

/// One scheduled replay point: a mutation the recorded journal attests,
/// released when the replayed journal reaches the seq it was recorded at.
#[derive(Debug, Clone)]
enum ScheduledPoint {
    /// An intake recorded at the attached journal seq.
    Intake(u64, InboxMessage),
    /// A cancellation recorded at the attached journal seq.
    Cancel(u64, CancelCause, bool),
}

/// Where settled messages come from: live sends (recording / normal runs)
/// or the recorded schedule (exact replay).
#[derive(Debug, Default)]
enum IntakeSource {
    /// Live sends buffered by [`Inbox::send`] and friends.
    #[default]
    Live,
    /// The recorded journal's inbox mutations, in seq order.
    Replay(VecDeque<ScheduledPoint>),
}

#[derive(Debug, Default)]
struct InboxInner {
    /// Sent, not yet settled into a queue (live mode only).
    pending: VecDeque<InboxMessage>,
    steering: VecDeque<InboxMessage>,
    followups: VecDeque<InboxMessage>,
    staged: VecDeque<InboxMessage>,
    /// The next intake sequence number.
    next_seq: u64,
    /// A requested cancellation, latched until the executor observes it.
    cancel: Option<(CancelCause, bool)>,
    source: IntakeSource,
    bounds: InboxBounds,
}

/// A durable per-run inbox. Cheap to clone: clones share one inbox (one
/// `Arc` inside), which is how a run's owner, its hooks, and its parent all
/// send into the same queues. Thread-safe; `Send + Sync`.
///
/// Attach to a run with [`crate::executor::RunConfig::with_inbox`]. The same
/// handle is passed to the resume that follows a suspension or a
/// `keep_inbox` cancellation; pass a *fresh* inbox instead to seed from the
/// checkpoint's stamped [`InboxSnapshot`] (a fresh inbox is one that has
/// never accepted a send).
#[derive(Debug, Clone, Default)]
pub struct Inbox {
    inner: Arc<Mutex<InboxInner>>,
}

impl Inbox {
    /// An empty inbox with the default bounds.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty inbox with explicit queue capacities.
    pub fn with_bounds(bounds: InboxBounds) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InboxInner {
                bounds,
                ..InboxInner::default()
            })),
        }
    }

    /// An inbox that re-delivers the inbox mutations of a recorded journal
    /// for exact replay: every [`RunEventKind::InboxIntake`] and
    /// [`RunEventKind::RunCancelled`] event becomes a scheduled point,
    /// released when the replaying run's journal reaches the seq the
    /// mutation was recorded at.
    ///
    /// Sending into a replaying inbox is rejected: replay evidence must be a
    /// pure function of the record, and a live send is not in the record.
    pub fn replaying(snapshot: &JournalSnapshot) -> Result<Self> {
        let mut points: VecDeque<ScheduledPoint> = VecDeque::new();
        for event in &snapshot.events {
            match event.kind {
                RunEventKind::InboxIntake => {
                    let value = event
                        .output
                        .as_ref()
                        .and_then(|payload| resolve_payload(snapshot, payload))
                        .ok_or_else(|| {
                            RustyError::Replay(format!(
                                "inbox replay: intake event {} carries no resolvable message",
                                event.id
                            ))
                        })?;
                    let message: InboxMessage = serde_json::from_value(value).map_err(|error| {
                        RustyError::Replay(format!(
                            "inbox replay: intake event {} holds a malformed message: {error}",
                            event.id
                        ))
                    })?;
                    points.push_back(ScheduledPoint::Intake(event.seq, message));
                }
                RunEventKind::RunCancelled => {
                    let value = event
                        .output
                        .as_ref()
                        .and_then(|payload| resolve_payload(snapshot, payload))
                        .ok_or_else(|| {
                            RustyError::Replay(format!(
                                "inbox replay: cancellation event {} carries no resolvable payload",
                                event.id
                            ))
                        })?;
                    let cancellation: RunCancellation =
                        serde_json::from_value(value).map_err(|error| {
                            RustyError::Replay(format!(
                                "inbox replay: cancellation event {} is malformed: {error}",
                                event.id
                            ))
                        })?;
                    points.push_back(ScheduledPoint::Cancel(
                        event.seq,
                        cancellation.cause,
                        cancellation.keep_inbox,
                    ));
                }
                _ => {}
            }
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(InboxInner {
                source: IntakeSource::Replay(points),
                ..InboxInner::default()
            })),
        })
    }

    fn lock(&self) -> MutexGuard<'_, InboxInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The general intake: route `content` from `sender` to `target`,
    /// waking the loop when `wakeup` is set. `(NextStep, wake)` steers,
    /// `(NextStep, no wake)` stages an injection, `(NextTurn, _)` follows
    /// up. The fixed aliases below cover the three meaningful combinations.
    pub fn send(
        &self,
        sender: impl Into<String>,
        target: InboxTarget,
        wakeup: bool,
        content: impl Into<Value>,
    ) -> Result<u64> {
        let kind = match (target, wakeup) {
            (InboxTarget::NextStep, true) => InboxKind::Steering,
            (InboxTarget::NextStep, false) => InboxKind::Injected,
            (InboxTarget::NextTurn, _) => InboxKind::FollowUp,
        };
        let mut inner = self.lock();
        if matches!(inner.source, IntakeSource::Replay(..)) {
            return Err(RustyError::Replay(
                "cannot send into a replaying inbox: replay evidence is a pure function of \
                 the recorded journal — build the run's inbox with Inbox::replaying and leave \
                 it untouched"
                    .into(),
            ));
        }
        let occupied = inner.pending.iter().filter(|m| m.kind == kind).count()
            + match kind {
                InboxKind::Steering => inner.steering.len(),
                InboxKind::FollowUp => inner.followups.len(),
                InboxKind::Injected => inner.staged.len(),
            };
        let bound = match kind {
            InboxKind::Steering => inner.bounds.max_steering,
            InboxKind::FollowUp => inner.bounds.max_followups,
            InboxKind::Injected => inner.bounds.max_staged,
        };
        if occupied >= bound {
            return Err(RustyError::Graph(format!(
                "inbox {kind:?} queue is full ({bound} messages); the send was refused, \
                 nothing was dropped"
            )));
        }
        if inner.pending.len() >= inner.bounds.max_pending {
            return Err(RustyError::Graph(format!(
                "inbox has {} unsettled sends (bound {}); the send was refused",
                inner.pending.len(),
                inner.bounds.max_pending
            )));
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.pending.push_back(InboxMessage {
            seq,
            kind,
            sender: sender.into(),
            content: content.into(),
        });
        Ok(seq)
    }

    /// Steer the run: `content` is delivered as user-role input at the next
    /// super-step boundary. Alias for `send("user", NextStep, wake, _)`.
    pub fn steer(&self, content: impl Into<Value>) -> Result<u64> {
        self.send(DEFAULT_SENDER, InboxTarget::NextStep, true, content)
    }

    /// Queue a follow-up: when the current turn would otherwise end, the run
    /// re-activates the graph's entry point with `content` delivered as
    /// user-role input. Alias for `send("user", NextTurn, _, _)`.
    pub fn followup(&self, content: impl Into<Value>) -> Result<u64> {
        self.send(DEFAULT_SENDER, InboxTarget::NextTurn, false, content)
    }

    /// Stage context without waking the loop: `content` rides along with the
    /// next wake (a steering batch, a follow-up extension, a run start, a
    /// resume). Alias for `send("user", NextStep, no wake, _)`.
    pub fn inject(&self, content: impl Into<Value>) -> Result<u64> {
        self.send(DEFAULT_SENDER, InboxTarget::NextStep, false, content)
    }

    /// Cancel the run with a typed cause. The executor observes the request
    /// at the next super-step boundary — the same transactional granularity
    /// as [`crate::executor::RunConfig::cancellation`] — journals
    /// [`RunEventKind::RunCancelled`], and returns
    /// [`RustyError::Cancelled`]. `keep_inbox` preserves the queued messages
    /// for the resume that follows; without it the queues are cleared and
    /// the boundary checkpoint is rewritten so the drop is durable.
    ///
    /// The request latches: a second cancel before observation keeps the
    /// first cause, matching "the run can only be cancelled once".
    pub fn cancel(&self, cause: CancelCause, keep_inbox: bool) {
        let mut inner = self.lock();
        if inner.cancel.is_none() {
            inner.cancel = Some((cause, keep_inbox));
        }
    }

    /// Total queued (settled) messages across all three queues.
    pub fn len(&self) -> usize {
        let inner = self.lock();
        inner.steering.len() + inner.followups.len() + inner.staged.len()
    }

    /// Whether all three queues are empty (pending sends and a latched
    /// cancellation do not count — they are not queued yet).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The durable queue state for checkpoint stamping. `None` while the
    /// inbox has never accepted a send — the "empty means absent" rule that
    /// keeps untouched inboxes out of checkpoint bytes entirely.
    pub fn snapshot(&self) -> Option<InboxSnapshot> {
        let inner = self.lock();
        if inner.next_seq == 0 {
            return None;
        }
        Some(InboxSnapshot {
            steering: inner.steering.iter().cloned().collect(),
            followups: inner.followups.iter().cloned().collect(),
            staged: inner.staged.iter().cloned().collect(),
            next_seq: inner.next_seq,
            pending_delivery: Vec::new(),
        })
    }

    /// Whether the inbox has never accepted a send: the condition under
    /// which resume seeds it from a checkpoint's stamped snapshot.
    pub(crate) fn is_fresh(&self) -> bool {
        let inner = self.lock();
        inner.next_seq == 0 && inner.pending.is_empty()
    }

    /// Seed a fresh inbox from a checkpoint's stamped snapshot (resume).
    /// A inbox that has already accepted sends keeps its own state: the same
    /// handle carried across an in-process suspension/resume must not have
    /// its queues duplicated by the checkpoint it helped stamp.
    pub(crate) fn seed(&self, snapshot: InboxSnapshot) {
        if !self.is_fresh() {
            return;
        }
        let mut inner = self.lock();
        inner.steering = snapshot.steering.into_iter().collect();
        inner.followups = snapshot.followups.into_iter().collect();
        inner.staged = snapshot.staged.into_iter().collect();
        inner.next_seq = snapshot.next_seq;
    }

    /// Move every releasable arrival into its queue, journaling one
    /// [`RunEventKind::InboxIntake`] per message. Live mode releases the
    /// pending buffer; replay mode releases scheduled intakes whose recorded
    /// seq the journal has reached. Returns the last journaled event id (the
    /// causal chain's new head), or `None` when nothing settled.
    ///
    /// The release rule — one message at a time, re-checking the journal
    /// length after each record — is what places a batch of intakes at
    /// consecutive seqs identically in recording and replay.
    pub(crate) fn settle(&self, journal: &Journal, parent: Option<String>) -> Option<String> {
        let mut inner = self.lock();
        let mut last_event: Option<String> = None;
        loop {
            let message = match &mut inner.source {
                IntakeSource::Live => inner.pending.pop_front(),
                IntakeSource::Replay(points) => match points.front() {
                    Some(ScheduledPoint::Intake(seq, _)) if *seq <= journal.len() as u64 => {
                        match points.pop_front() {
                            Some(ScheduledPoint::Intake(_, message)) => Some(message),
                            _ => unreachable!("front matched an intake point"),
                        }
                    }
                    _ => None,
                },
            };
            let Some(message) = message else { break };
            match message.kind {
                InboxKind::Steering => inner.steering.push_back(message.clone()),
                InboxKind::FollowUp => inner.followups.push_back(message.clone()),
                InboxKind::Injected => inner.staged.push_back(message.clone()),
            }
            let mut draft = EventDraft::new(RunEventKind::InboxIntake, Effect::Pure)
                .output(serde_json::to_value(&message).expect("an InboxMessage always serializes"));
            if let Some(parent) = last_event.clone().or(parent.clone()) {
                draft = draft.parent(parent);
            }
            last_event = Some(journal.record(draft));
        }
        last_event
    }

    /// Take the latched cancellation request, if one is releasable. Live
    /// mode releases whatever [`Inbox::cancel`] latched; replay mode
    /// releases the schedule's front point when it is a cancellation whose
    /// recorded seq the journal has reached.
    pub(crate) fn take_cancel(&self, journal_len: u64) -> Option<(CancelCause, bool)> {
        let mut inner = self.lock();
        match &mut inner.source {
            IntakeSource::Live => inner.cancel.take(),
            IntakeSource::Replay(points) => match points.front() {
                Some(ScheduledPoint::Cancel(seq, ..)) if *seq <= journal_len => {
                    match points.pop_front() {
                        Some(ScheduledPoint::Cancel(_, cause, keep_inbox)) => {
                            Some((cause, keep_inbox))
                        }
                        _ => unreachable!("front matched a cancel point"),
                    }
                }
                _ => None,
            },
        }
    }

    /// Drain the steering queue (next-step consumption).
    pub(crate) fn drain_steering(&self) -> Vec<InboxMessage> {
        self.lock().steering.drain(..).collect()
    }

    /// Drain the follow-up queue (next-turn consumption).
    pub(crate) fn drain_followups(&self) -> Vec<InboxMessage> {
        self.lock().followups.drain(..).collect()
    }

    /// Drain the staged injection queue (consumed with a wake).
    pub(crate) fn drain_staged(&self) -> Vec<InboxMessage> {
        self.lock().staged.drain(..).collect()
    }

    /// Drop every queued and pending message (a `keep_inbox: false`
    /// cancellation), returning what was dropped for the journaled record.
    pub(crate) fn clear(&self) -> DroppedMessages {
        let mut inner = self.lock();
        let dropped = DroppedMessages {
            steering: inner.steering.len(),
            followups: inner.followups.len(),
            staged: inner.staged.len(),
            pending: inner.pending.len(),
        };
        inner.steering.clear();
        inner.followups.clear();
        inner.staged.clear();
        inner.pending.clear();
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Clock;
    use serde_json::json;

    #[test]
    fn send_routes_target_and_wakeup_to_the_fixed_kinds() {
        let inbox = Inbox::new();
        inbox
            .send("hook", InboxTarget::NextStep, true, json!("s"))
            .unwrap();
        inbox
            .send("hook", InboxTarget::NextStep, false, json!("i"))
            .unwrap();
        inbox
            .send("hook", InboxTarget::NextTurn, true, json!("f1"))
            .unwrap();
        inbox
            .send("hook", InboxTarget::NextTurn, false, json!("f2"))
            .unwrap();

        let journal = Journal::new("run", "thread", Clock::System);
        inbox.settle(&journal, None);

        let steering = inbox.drain_steering();
        let staged = inbox.drain_staged();
        let followups = inbox.drain_followups();
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].kind, InboxKind::Steering);
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].kind, InboxKind::Injected);
        assert_eq!(followups.len(), 2);
        assert!(followups.iter().all(|m| m.kind == InboxKind::FollowUp));
        assert!(followups.iter().all(|m| m.sender == "hook"));
    }

    #[test]
    fn aliases_default_to_the_user_sender() {
        let inbox = Inbox::new();
        inbox.steer(json!("s")).unwrap();
        inbox.followup(json!("f")).unwrap();
        inbox.inject(json!("i")).unwrap();
        let journal = Journal::new("run", "thread", Clock::System);
        inbox.settle(&journal, None);
        assert_eq!(inbox.drain_steering()[0].sender, DEFAULT_SENDER);
        assert_eq!(inbox.drain_followups()[0].sender, DEFAULT_SENDER);
        assert_eq!(inbox.drain_staged()[0].sender, DEFAULT_SENDER);
    }

    #[test]
    fn intake_sequences_are_monotonic_and_survive_snapshot_restore() {
        let inbox = Inbox::new();
        let first = inbox.steer(json!("a")).unwrap();
        let second = inbox.followup(json!("b")).unwrap();
        assert_eq!((first, second), (0, 1));

        let snapshot = inbox.snapshot().expect("touched inbox snapshots");
        assert_eq!(snapshot.next_seq, 2);

        let restored = Inbox::new();
        restored.seed(snapshot);
        // The restored inbox continues the sequence instead of reissuing 0.
        let journal = Journal::new("run", "thread", Clock::System);
        restored.settle(&journal, None);
        let third = restored.steer(json!("c")).unwrap();
        assert_eq!(third, 2);
    }

    #[test]
    fn seed_does_not_disturb_a_live_inbox() {
        let inbox = Inbox::new();
        inbox.steer(json!("mine")).unwrap();
        inbox.seed(InboxSnapshot {
            steering: vec![InboxMessage {
                seq: 0,
                kind: InboxKind::Steering,
                sender: "other".into(),
                content: json!("theirs"),
            }],
            followups: vec![],
            staged: vec![],
            next_seq: 1,
            pending_delivery: vec![],
        });
        // The live pending send survived; the checkpoint's copy was ignored.
        let journal = Journal::new("run", "thread", Clock::System);
        inbox.settle(&journal, None);
        let steering = inbox.drain_steering();
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].content, json!("mine"));
    }

    #[test]
    fn bounds_refuse_overflowing_sends() {
        let inbox = Inbox::with_bounds(InboxBounds {
            max_pending: 8,
            max_steering: 1,
            max_followups: 1,
            max_staged: 1,
        });
        inbox.steer(json!("a")).unwrap();
        let error = inbox.steer(json!("b")).unwrap_err();
        assert!(matches!(error, RustyError::Graph(_)), "got: {error}");
        // Pending messages count against the queue bound even before
        // settlement, so a flush cannot overflow later.
    }

    #[test]
    fn cancel_latches_the_first_cause() {
        let inbox = Inbox::new();
        inbox.cancel(CancelCause::User, false);
        inbox.cancel(CancelCause::Disposed, true);
        assert_eq!(
            inbox.take_cancel(0),
            Some((CancelCause::User, false)),
            "the first cancellation wins; the run can only be cancelled once"
        );
        assert_eq!(inbox.take_cancel(0), None, "observation consumes it");
    }

    #[test]
    fn clear_reports_everything_it_dropped() {
        let inbox = Inbox::new();
        inbox.steer(json!("s")).unwrap();
        inbox.followup(json!("f")).unwrap();
        inbox.inject(json!("i")).unwrap();
        let journal = Journal::new("run", "thread", Clock::System);
        inbox.settle(&journal, None);
        inbox.steer(json!("still pending")).unwrap();

        let dropped = inbox.clear();
        assert_eq!(
            dropped,
            DroppedMessages {
                steering: 1,
                followups: 1,
                staged: 1,
                pending: 1,
            }
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn untouched_inbox_has_no_snapshot() {
        assert_eq!(Inbox::new().snapshot(), None);
    }

    #[test]
    fn replaying_inbox_rejects_live_sends() {
        let journal = Journal::new("run", "thread", Clock::System);
        let snapshot = journal.snapshot();
        let inbox = Inbox::replaying(&snapshot).unwrap();
        let error = inbox.steer(json!("nope")).unwrap_err();
        assert!(matches!(error, RustyError::Replay(_)), "got: {error}");
    }
}
