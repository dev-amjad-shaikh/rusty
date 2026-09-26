//! Server-side exact-replay verification (`POST /runs/replay`).
//!
//! The handler re-drives the recorded run's registered graph against its
//! persisted journal (core's [`ExactReplay`]) and compares the replayed
//! journal against the recorded one. Server runs record under the system
//! clock and OS entropy, so byte-identical verification (core's
//! `ExactReplay::verify`) is unreachable here; the comparison is over the
//! **evidence axes** instead:
//!
//! - compared: `seq`, `kind`, `node_id`, effect class, `status`, and the
//!   resolved input/output payloads;
//! - excluded: event/run/thread identity and causal parentage (deterministic
//!   for a same-id re-drive), `recorded_at` and `latency_ms` (wall-clock
//!   measurements of each execution), token usage and cost (provider-reported
//!   per execution), and the `checkpoint_id` inside `checkpoint_written`
//!   outputs (minted per run through the run's RNG; the recorded run's ids
//!   are unrecoverable).
//!
//! What remains is exactly the question the endpoint answers: does the same
//! graph code, given the same input, reproduce the recorded run's decisions
//! and state transitions?

use rusty_agent_runtime::journal::JournalSnapshot;
use rusty_agent_runtime::record::{PayloadRef, RunEvent, RunEventKind};
use rusty_agent_runtime::state::State;
use serde_json::Value;

/// The outcome of comparing a recorded journal against its replay.
pub(crate) struct ReplayReport {
    /// `true` when the replayed journal reproduces the recorded one on every
    /// evidence axis, event for event (same length, no divergence).
    pub verified: bool,

    /// The `seq` of the first event where the two journals disagree, or of
    /// the first recorded event the replay never produced (`None` when the
    /// replay reproduces the recorded journal exactly).
    pub first_divergence: Option<u64>,
}

/// Compare `recorded` against `replayed` per the module docs' evidence axes.
pub(crate) fn compare_journals(
    recorded: &JournalSnapshot,
    replayed: &JournalSnapshot,
) -> ReplayReport {
    let divergence = recorded
        .events
        .iter()
        .zip(&replayed.events)
        .find(|(a, b)| !events_match(a, recorded, b, replayed))
        .map(|(a, _)| a.seq)
        .or_else(|| {
            // A shared prefix with different lengths: the divergence is the
            // first event one side never produced.
            if recorded.events.len() == replayed.events.len() {
                None
            } else {
                Some(recorded.events.len().min(replayed.events.len()) as u64)
            }
        });
    ReplayReport {
        verified: divergence.is_none(),
        first_divergence: divergence,
    }
}

/// The run's initial state, recovered from the journal: the first node-input
/// event's input payload is the entry invocation's state snapshot, which IS
/// the run's initial state. Defaults to empty (a journal that crashed before
/// its first node input replays from nothing, and the comparison reports the
/// divergence).
pub(crate) fn initial_state_from(snapshot: &JournalSnapshot) -> State {
    snapshot
        .events
        .iter()
        .find(|event| event.kind == RunEventKind::NodeInput)
        .and_then(|event| resolve(snapshot, event.input.as_ref()))
        .and_then(|value| State::from_value(value).ok())
        .unwrap_or_default()
}

/// One event pair on the evidence axes (see the module docs).
fn events_match(
    a: &RunEvent,
    snapshot_a: &JournalSnapshot,
    b: &RunEvent,
    snapshot_b: &JournalSnapshot,
) -> bool {
    a.seq == b.seq
        && a.kind == b.kind
        && a.node_id == b.node_id
        && a.effect == b.effect
        && a.status == b.status
        && comparable_payload(a, snapshot_a, a.input.as_ref())
            == comparable_payload(b, snapshot_b, b.input.as_ref())
        && comparable_payload(a, snapshot_a, a.output.as_ref())
            == comparable_payload(b, snapshot_b, b.output.as_ref())
}

/// A payload made comparable across runs: artifact references resolved, and —
/// for `checkpoint_written` outputs — the per-run minted `checkpoint_id`
/// removed (see the module docs).
fn comparable_payload(
    event: &RunEvent,
    snapshot: &JournalSnapshot,
    payload: Option<&PayloadRef>,
) -> Option<Value> {
    let mut value = resolve(snapshot, payload)?;
    if event.kind == RunEventKind::CheckpointWritten {
        if let Some(object) = value.as_object_mut() {
            object.remove("checkpoint_id");
        }
    }
    Some(value)
}

/// Resolve a payload reference against the snapshot's artifact map.
pub(crate) fn resolve(snapshot: &JournalSnapshot, payload: Option<&PayloadRef>) -> Option<Value> {
    match payload? {
        PayloadRef::Inline(value) => Some(value.clone()),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256).cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
    use rusty_agent_runtime::record::Effect;
    use serde_json::json;

    fn snapshot_with(run_id: &str, drafts: Vec<EventDraft>) -> JournalSnapshot {
        let journal = Journal::new(run_id, "thread-1", Clock::System);
        for draft in drafts {
            journal.record(draft);
        }
        journal.snapshot()
    }

    fn pipeline_drafts(checkpoint_id: &str) -> Vec<EventDraft> {
        vec![
            EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure)
                .input(json!({"step": 0, "active_nodes": ["first"]})),
            EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
                .node("first")
                .input(json!({"log": []})),
            EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
                .node("first")
                .output(json!({"updates": {"log": "first"}, "command": null}))
                .latency_ms(1),
            EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                .output(json!({"log": ["first"]})),
            EventDraft::new(RunEventKind::CheckpointWritten, Effect::Idempotent)
                .output(json!({"checkpoint_id": checkpoint_id, "step": 0, "suspension": false})),
        ]
    }

    #[test]
    fn a_faithful_replay_verifies_despite_fresh_ids_and_timestamps() {
        let recorded = snapshot_with("run-1", pipeline_drafts("cp-recorded"));
        // The replay mints its own checkpoint id and reads its own clock;
        // both are excluded from the evidence comparison.
        let replayed = snapshot_with("run-1", pipeline_drafts("cp-replayed"));
        let report = compare_journals(&recorded, &replayed);
        assert!(report.verified);
        assert_eq!(report.first_divergence, None);
    }

    #[test]
    fn a_changed_payload_is_reported_at_its_seq() {
        let recorded = snapshot_with("run-1", pipeline_drafts("cp-a"));
        let mut drafts = pipeline_drafts("cp-b");
        drafts[2] = EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
            .node("first")
            .output(json!({"updates": {"log": "CHANGED"}, "command": null}));
        let replayed = snapshot_with("run-1", drafts);
        let report = compare_journals(&recorded, &replayed);
        assert!(!report.verified);
        assert_eq!(report.first_divergence, Some(2));
    }

    #[test]
    fn a_short_replay_diverges_at_the_first_missing_seq() {
        let recorded = snapshot_with("run-1", pipeline_drafts("cp-a"));
        let replayed = snapshot_with(
            "run-1",
            pipeline_drafts("cp-a").into_iter().take(3).collect(),
        );
        let report = compare_journals(&recorded, &replayed);
        assert!(!report.verified);
        assert_eq!(report.first_divergence, Some(3));

        // A longer replay diverges at the first event the record never had.
        let longer = compare_journals(&replayed, &recorded);
        assert!(!longer.verified);
        assert_eq!(longer.first_divergence, Some(3));
    }

    #[test]
    fn initial_state_comes_from_the_first_node_input() {
        let snapshot = snapshot_with("run-1", pipeline_drafts("cp-a"));
        let state = initial_state_from(&snapshot);
        assert_eq!(state.to_value(), json!({"log": []}));

        let empty = snapshot_with("run-1", vec![]);
        assert_eq!(initial_state_from(&empty).to_value(), json!({}));
    }
}
