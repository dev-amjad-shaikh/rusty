//! TeamTrace: cross-journal trace assembly for the Agent Fabric (R0.7
//! wave 3).
//!
//! A coordination pattern's evidence does not live in one journal. The
//! pattern's own journal holds the `CoordinationStart` → `MailboxSend` →
//! `MailboxReceive` → `CoordinationEnd` spine, and member agents write
//! their own run journals. Every event carries a `parent` link, so the
//! causal tree exists in the data — but each journal only stores its own
//! slice of it. TeamTrace is the read-side algorithm that stitches those
//! slices back into one tree: give it the snapshots (already integrity
//! verified by [`crate::journal::Journal::from_snapshot`]) and it returns
//! one deterministic, connected view.
//!
//! The module is deliberately read-only and pure: assembly never writes,
//! never mutates the journals, and never invents links. A node whose
//! parent is absent from the assembled set is a **root** — that is the
//! cross-journal stitch: the coordination journal's `MailboxSend` is the
//! parent of events in a member run's journal, and the stitch holds
//! because both sides record the same deterministic event ids.
//!
//! Determinism is the contract: the same snapshots always assemble into
//! the byte-identical trace (`run_ids` sorted, nodes sorted by
//! `(run_id, seq)`, children sorted by the child's `(run_id, seq)`). The
//! trace is evidence, and evidence that depends on iteration order is not
//! evidence.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::journal::JournalSnapshot;
use crate::record::RunEventKind;

/// One node in an assembled team trace: a single journaled event plus its
/// position in the cross-journal causal tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamTraceNode {
    /// The event's deterministic id (`{run_id}:{seq}`).
    pub event_id: String,

    /// The journal (run) this event was recorded in.
    pub run_id: String,

    /// The event's sequence number within its own journal.
    pub seq: u64,

    /// What happened.
    pub kind: RunEventKind,

    /// The parent event id as journaled. `None` for a causal root; an id
    /// outside the assembled set means the parent lives in a journal that
    /// was not part of this assembly (the node is then a root of this
    /// trace, and `parent` still names where it hangs in the wider tree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,

    /// The event ids of this node's children inside the assembled set,
    /// sorted by the child's `(run_id, seq)` — deterministic regardless of
    /// the order snapshots were supplied in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,

    /// The node's depth below a root (root = 0). `None` when the node is
    /// unreachable from every root — the signature of a parent cycle that
    /// no root can enter, which a hand-edited or corrupted journal could
    /// produce. [`TeamTrace::is_connected`] refuses such traces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
}

/// A cross-journal causal tree assembled from verified journal snapshots
/// (R0.7 wave 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamTrace {
    /// Every run id that contributed events, sorted — the journals this
    /// trace stitched together.
    pub run_ids: Vec<String>,

    /// The event ids of the trace's roots, sorted. A connected
    /// coordination has exactly one root: its `CoordinationStart` event.
    pub roots: Vec<String>,

    /// Every node, sorted by `(run_id, seq)` — a total, deterministic
    /// order over the whole assembled tree.
    pub nodes: Vec<TeamTraceNode>,
}

impl TeamTrace {
    /// Assemble one trace from verified journal snapshots.
    ///
    /// The algorithm:
    ///
    /// 1. Collect every event of every snapshot into nodes keyed by event
    ///    id. Duplicate ids (the same journal supplied twice) collapse —
    ///    the event is one fact.
    /// 2. Link children: an event whose `parent` names an event inside the
    ///    assembled set is that node's child. A parent outside the set
    ///    does not make the event parentless — `parent` keeps naming it —
    ///    but it does make the event a root *of this trace*.
    /// 3. Assign depths breadth-first from the roots, with a visited set
    ///    so a parent cycle can never loop the walk; cycle members that no
    ///    root reaches keep `depth: None`.
    pub fn assemble(snapshots: &[JournalSnapshot]) -> Self {
        // Pass 1: collect events. BTreeMap keyed by event id collapses
        // duplicate journals and gives a stable iteration order for free.
        let mut events: BTreeMap<String, (String, u64, RunEventKind, Option<String>)> =
            BTreeMap::new();
        for snapshot in snapshots {
            for event in &snapshot.events {
                events.entry(event.id.clone()).or_insert_with(|| {
                    (
                        event.run_id.clone(),
                        event.seq,
                        event.kind,
                        event.parent.clone(),
                    )
                });
            }
        }

        // Pass 2: children adjacency, keyed by parent event id.
        let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut roots: BTreeSet<String> = BTreeSet::new();
        for (id, (_, _, _, parent)) in &events {
            match parent {
                Some(parent_id) if events.contains_key(parent_id) => {
                    children_of
                        .entry(parent_id.clone())
                        .or_default()
                        .push(id.clone());
                }
                _ => {
                    roots.insert(id.clone());
                }
            }
        }
        // Sort each child list by the child's (run_id, seq), not the id
        // string — lexical order would rank "run:10" before "run:2".
        for children in children_of.values_mut() {
            children.sort_by_key(|a| node_order(&events, a));
        }

        // Pass 3: depths, breadth-first from every root, cycle-safe.
        let mut depth_of: HashMap<String, u32> = HashMap::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        for root in &roots {
            queue.push_back((root.clone(), 0));
        }
        while let Some((id, depth)) = queue.pop_front() {
            if !visited.insert(id.clone()) {
                continue;
            }
            depth_of.insert(id.clone(), depth);
            if let Some(children) = children_of.get(&id) {
                for child in children {
                    queue.push_back((child.clone(), depth + 1));
                }
            }
        }

        let mut run_ids: Vec<String> = events
            .values()
            .map(|(run_id, _, _, _)| run_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        run_ids.sort();

        let nodes: Vec<TeamTraceNode> = {
            let mut keyed: Vec<((String, u64), TeamTraceNode)> = events
                .into_iter()
                .map(|(id, (run_id, seq, kind, parent))| {
                    let node = TeamTraceNode {
                        children: children_of.remove(&id).unwrap_or_default(),
                        depth: depth_of.get(&id).copied(),
                        event_id: id,
                        run_id: run_id.clone(),
                        seq,
                        kind,
                        parent,
                    };
                    ((node.run_id.clone(), seq), node)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            keyed.into_iter().map(|(_, node)| node).collect()
        };

        Self {
            run_ids,
            roots: roots.into_iter().collect(),
            nodes,
        }
    }

    /// `true` when the assembled trace is one connected tree: exactly one
    /// root, and every node reachable from it (no detached cycles). A
    /// coordination's journal set should always assemble connected — the
    /// `CoordinationStart` event is the single causal root; anything else
    /// means journals are missing from the assembly or events were
    /// corrupted, and the caller should treat the trace as incomplete
    /// evidence rather than as a tree.
    pub fn is_connected(&self) -> bool {
        self.roots.len() == 1
            && !self.nodes.is_empty()
            && self.nodes.iter().all(|node| node.depth.is_some())
    }

    /// Look up one node by event id.
    pub fn node(&self, event_id: &str) -> Option<&TeamTraceNode> {
        self.nodes.iter().find(|node| node.event_id == event_id)
    }
}

/// The `(run_id, seq)` sort key of an event id already collected into the
/// events map. Assembly only calls this for ids known to be present.
fn node_order(
    events: &BTreeMap<String, (String, u64, RunEventKind, Option<String>)>,
    id: &str,
) -> (String, u64) {
    match events.get(id) {
        Some((run_id, seq, _, _)) => (run_id.clone(), *seq),
        None => (String::new(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::JournalSnapshot;
    use crate::record::{Effect, EventStatus, PayloadRef, RunEvent};
    use serde_json::json;

    /// Build a minimal journal snapshot with the given events. Assembly
    /// reads only `events`, so the hash chain fields are inert here — the
    /// integrity contract is upheld by `Journal::from_snapshot` before
    /// snapshots ever reach `assemble`.
    fn snapshot(run_id: &str, events: Vec<RunEvent>) -> JournalSnapshot {
        JournalSnapshot {
            run_id: run_id.to_string(),
            thread_id: format!("thread:{run_id}"),
            events,
            artifacts: BTreeMap::new(),
            head_hash: String::new(),
        }
    }

    fn event(run_id: &str, seq: u64, kind: RunEventKind, parent: Option<&str>) -> RunEvent {
        RunEvent {
            id: format!("{run_id}:{seq}"),
            run_id: run_id.to_string(),
            thread_id: format!("thread:{run_id}"),
            node_id: None,
            seq,
            kind,
            effect: Effect::Pure,
            input: None,
            output: Some(PayloadRef::inline(json!({"seq": seq}))),
            latency_ms: None,
            tokens: None,
            cost_usd: None,
            status: EventStatus::Ok,
            parent: parent.map(str::to_string),
            recorded_at: chrono::DateTime::from_timestamp_millis(1_800_000_000_000).unwrap(),
        }
    }

    /// The canonical coordination spine, stitched across two journals:
    /// the coordination journal holds start/send/receive/end, the member
    /// journal holds the member run's own events parented onto the send.
    fn stitched_snapshots() -> (JournalSnapshot, JournalSnapshot) {
        let coordination = snapshot(
            "coordination:acme:c1",
            vec![
                event(
                    "coordination:acme:c1",
                    1,
                    RunEventKind::CoordinationStart,
                    None,
                ),
                event(
                    "coordination:acme:c1",
                    2,
                    RunEventKind::MailboxSend,
                    Some("coordination:acme:c1:1"),
                ),
                event(
                    "coordination:acme:c1",
                    3,
                    RunEventKind::MailboxReceive,
                    Some("coordination:acme:c1:2"),
                ),
                event(
                    "coordination:acme:c1",
                    4,
                    RunEventKind::CoordinationEnd,
                    Some("coordination:acme:c1:1"),
                ),
            ],
        );
        let member = snapshot(
            "run:member-a",
            vec![
                event(
                    "run:member-a",
                    1,
                    RunEventKind::SuperStepStart,
                    Some("coordination:acme:c1:2"),
                ),
                event(
                    "run:member-a",
                    2,
                    RunEventKind::ModelCall,
                    Some("run:member-a:1"),
                ),
            ],
        );
        (coordination, member)
    }

    #[test]
    fn assembles_one_connected_tree_across_journals() {
        let (coordination, member) = stitched_snapshots();
        let trace = TeamTrace::assemble(&[coordination, member]);

        assert!(trace.is_connected());
        assert_eq!(trace.roots, vec!["coordination:acme:c1:1".to_string()]);
        assert_eq!(
            trace.run_ids,
            vec![
                "coordination:acme:c1".to_string(),
                "run:member-a".to_string()
            ]
        );

        // Depths: start 0 → send 1 → receive 2; end hangs off start at 1;
        // the member run stitches under the send at 2, its model call at 3.
        let depth = |id: &str| trace.node(id).and_then(|n| n.depth);
        assert_eq!(depth("coordination:acme:c1:1"), Some(0));
        assert_eq!(depth("coordination:acme:c1:2"), Some(1));
        assert_eq!(depth("coordination:acme:c1:3"), Some(2));
        assert_eq!(depth("coordination:acme:c1:4"), Some(1));
        assert_eq!(depth("run:member-a:1"), Some(2));
        assert_eq!(depth("run:member-a:2"), Some(3));

        // Children are deterministic and reflect the cross-journal stitch.
        let send = trace.node("coordination:acme:c1:2").unwrap();
        assert_eq!(
            send.children,
            vec![
                "coordination:acme:c1:3".to_string(),
                "run:member-a:1".to_string()
            ]
        );
    }

    #[test]
    fn assembly_is_deterministic_under_snapshot_order() {
        let (coordination, member) = stitched_snapshots();
        let forward = TeamTrace::assemble(&[coordination.clone(), member.clone()]);
        let reversed = TeamTrace::assemble(&[member, coordination]);
        assert_eq!(forward, reversed);
        // And byte-deterministic on the wire.
        assert_eq!(
            serde_json::to_string(&forward).unwrap(),
            serde_json::to_string(&reversed).unwrap()
        );
    }

    #[test]
    fn parent_outside_the_assembled_set_is_a_root() {
        // Only the member journal supplied: its RunStart parents onto a
        // send event that is not in the set, so it becomes a root — with
        // `parent` still naming the missing link.
        let (_, member) = stitched_snapshots();
        let trace = TeamTrace::assemble(&[member]);
        assert!(trace.is_connected());
        assert_eq!(trace.roots, vec!["run:member-a:1".to_string()]);
        let root = trace.node("run:member-a:1").unwrap();
        assert_eq!(root.parent.as_deref(), Some("coordination:acme:c1:2"));
        assert_eq!(root.depth, Some(0));
    }

    #[test]
    fn two_roots_are_not_connected() {
        let a = snapshot(
            "run:a",
            vec![event("run:a", 1, RunEventKind::SuperStepStart, None)],
        );
        let b = snapshot(
            "run:b",
            vec![event("run:b", 1, RunEventKind::SuperStepStart, None)],
        );
        let trace = TeamTrace::assemble(&[a, b]);
        assert!(!trace.is_connected());
        assert_eq!(trace.roots.len(), 2);
    }

    #[test]
    fn parent_cycle_neither_hangs_nor_connects() {
        // a:1 → a:2 → a:1: a cycle no root can enter. The walk must
        // terminate (visited set), the cycle nodes stay depth-less, and the
        // trace reports not-connected instead of inventing a tree.
        let cycled = snapshot(
            "run:a",
            vec![
                event("run:a", 1, RunEventKind::SuperStepStart, Some("run:a:2")),
                event("run:a", 2, RunEventKind::ModelCall, Some("run:a:1")),
            ],
        );
        let trace = TeamTrace::assemble(&[cycled]);
        assert!(trace.roots.is_empty());
        assert!(trace.nodes.iter().all(|node| node.depth.is_none()));
        assert!(!trace.is_connected());
    }

    #[test]
    fn child_sorting_uses_seq_not_lexical_id_order() {
        // "run:a:10" sorts before "run:a:2" lexically; the trace must order
        // children by sequence.
        let snap = snapshot(
            "run:a",
            vec![
                event("run:a", 1, RunEventKind::SuperStepStart, None),
                event("run:a", 2, RunEventKind::ModelCall, Some("run:a:1")),
                event("run:a", 10, RunEventKind::ModelCall, Some("run:a:1")),
            ],
        );
        let trace = TeamTrace::assemble(&[snap]);
        let root = trace.node("run:a:1").unwrap();
        assert_eq!(
            root.children,
            vec!["run:a:2".to_string(), "run:a:10".to_string()]
        );
    }

    #[test]
    fn empty_assembly_is_empty_and_not_connected() {
        let trace = TeamTrace::assemble(&[]);
        assert!(trace.run_ids.is_empty());
        assert!(trace.roots.is_empty());
        assert!(trace.nodes.is_empty());
        assert!(!trace.is_connected());
    }

    #[test]
    fn duplicate_journals_collapse_to_one_fact() {
        let (coordination, _) = stitched_snapshots();
        let trace = TeamTrace::assemble(&[coordination.clone(), coordination]);
        assert_eq!(trace.nodes.len(), 4);
        assert!(trace.is_connected());
    }
}
