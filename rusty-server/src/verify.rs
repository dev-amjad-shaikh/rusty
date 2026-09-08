//! Journal verification for disaster-recovery confidence (EP-13-S10).
//!
//! [`verify_log`] checks the per-session invariants the spec calls out:
//! gap-free positions, paired turn events, and (when an artifact store is
//! wired) dangling artifact locators.  It is the engine behind
//! `rustyness verify-log` and behind the restore-rehearsal CI gate.

use rusty_agent_runtime::journal::{ArtifactStore, Clock, Journal, JournalSnapshot};
use rusty_agent_runtime::record::{ArtifactRef, PayloadRef, RunEvent, RunEventKind};
use serde::{Deserialize, Serialize};

/// One integrity problem found by the verifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntegrityFinding {
    /// A `seq` gap or duplication was detected.
    MissingPosition {
        /// The sequence index where the anomaly was found.
        index: usize,
        /// The `seq` value expected at this index.
        expected: u64,
        /// The `seq` value actually present.
        found: u64,
    },
    /// An event that opens a scope has no matching close event.
    UnpairedTurn {
        /// The event id of the opening event.
        open_event_id: String,
        /// The kind of the opening event.
        open_kind: String,
    },
    /// An artifact locator referenced by an event does not resolve in the
    /// artifact store or the snapshot's embedded artifact map.
    DanglingLocator {
        /// The locator string (sha256) that could not be resolved.
        locator: String,
        /// The event id that carried the reference.
        event_id: String,
    },
    /// The journal snapshot failed its cryptographic integrity check.
    IntegrityFailure {
        /// Human-readable description of the failure.
        reason: String,
    },
}

/// The outcome of running [`verify_log`] over a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationReport {
    /// `true` when no findings were emitted.
    pub passed: bool,
    /// Every anomaly detected, in discovery order.
    pub findings: Vec<IntegrityFinding>,
    /// Number of events inspected.
    pub event_count: usize,
}

impl VerificationReport {
    fn new(event_count: usize) -> Self {
        Self {
            passed: true,
            findings: Vec::new(),
            event_count,
        }
    }

    fn push(&mut self, finding: IntegrityFinding) {
        self.passed = false;
        self.findings.push(finding);
    }
}

/// Verify a [`JournalSnapshot`] for the invariants required by
/// `contracts:checkpoint` disaster recovery.
///
/// 1. **Integrity** – the snapshot loads through [`Journal::from_snapshot`];
///    a corrupted chain is rejected immediately.
/// 2. **Gap-free positions** – within the run, `event[i].seq == i as u64`;
///    no duplicates, no gaps.
/// 3. **Paired turn events** – every `SuperStepStart` is followed by a
///    `SuperStepEnd` before the journal ends; every `NodeInput` by a
///    `NodeOutput`; every `CoordinationStart` by a `CoordinationEnd`.
///    After crash repair an unclosed pair is reported as an integrity
///    finding (the run was interrupted mid-turn and will resume).
/// 4. **Dangling locators** – when an [`ArtifactStore`] is provided, every
///    `PayloadRef::Artifact` in the event stream is checked for existence
///    in the store or in the snapshot's embedded `artifacts` map.
///
/// Returns a [`VerificationReport`] that is `passed == true` only when
/// every check succeeds.
pub fn verify_log(snapshot: JournalSnapshot) -> VerificationReport {
    // Synchronous wrapper: no artifact store, so locator checks are skipped.
    let event_count = snapshot.events.len();
    let mut report = VerificationReport::new(event_count);
    run_core_checks(snapshot, &mut report);
    report
}

/// Async variant of [`verify_log`] that also verifies artifact references
/// against an optional [`ArtifactStore`].
///
/// When `store` is `Some`, every `PayloadRef::Artifact` in the event
/// stream is checked for existence.  Missing references are reported as
/// [`IntegrityFinding::DanglingLocator`].
pub async fn verify_log_with_store(
    snapshot: JournalSnapshot,
    store: Option<&dyn ArtifactStore>,
) -> VerificationReport {
    let event_count = snapshot.events.len();
    let mut report = VerificationReport::new(event_count);
    run_core_checks(snapshot.clone(), &mut report);

    if let Some(store) = store {
        verify_artifacts(&snapshot, store, &mut report).await;
    }

    report
}

/// Run the three core checks (integrity, positions, paired events).
fn run_core_checks(snapshot: JournalSnapshot, report: &mut VerificationReport) {
    // 1. Integrity check via Journal::from_snapshot.
    if let Err(e) = Journal::from_snapshot(snapshot.clone(), Clock::System) {
        report.push(IntegrityFinding::IntegrityFailure {
            reason: e.to_string(),
        });
        // Cannot proceed with event-level checks on a structurally invalid
        // snapshot; return early.
        return;
    }

    // 2. Gap-free positions.
    for (idx, event) in snapshot.events.iter().enumerate() {
        let expected = idx as u64;
        if event.seq != expected {
            report.push(IntegrityFinding::MissingPosition {
                index: idx,
                expected,
                found: event.seq,
            });
        }
    }

    // 3. Paired turn events.
    check_paired_events(&snapshot.events, report);
}

/// Verify that every `ArtifactRef` found in the event stream resolves.
async fn verify_artifacts(
    snapshot: &JournalSnapshot,
    store: &dyn ArtifactStore,
    report: &mut VerificationReport,
) {
    let refs = collect_artifact_refs(&snapshot.events);
    for (event_id, artifact_ref) in refs {
        let sha256 = &artifact_ref.sha256;
        // First check the snapshot's embedded artifact map.
        if snapshot.artifacts.contains_key(sha256) {
            continue;
        }
        // Then check the external artifact store.
        match store.contains(sha256).await {
            Ok(true) => continue,
            Ok(false) => {
                report.push(IntegrityFinding::DanglingLocator {
                    locator: sha256.clone(),
                    event_id: event_id.clone(),
                });
            }
            Err(e) => {
                report.push(IntegrityFinding::DanglingLocator {
                    locator: format!("{sha256} (store error: {e})"),
                    event_id: event_id.clone(),
                });
            }
        }
    }
}

/// Walk every event's `input` and `output` and collect all
/// `PayloadRef::Artifact` references.
fn collect_artifact_refs(events: &[RunEvent]) -> Vec<(String, ArtifactRef)> {
    let mut out = Vec::new();
    for event in events {
        if let Some(PayloadRef::Artifact(ref aref)) = event.input {
            out.push((event.id.clone(), aref.clone()));
        }
        if let Some(PayloadRef::Artifact(ref aref)) = event.output {
            out.push((event.id.clone(), aref.clone()));
        }
    }
    out
}

/// Check that opening/closing event pairs are balanced.
#[allow(clippy::collapsible_match)]
fn check_paired_events(events: &[RunEvent], report: &mut VerificationReport) {
    // Stack of currently-open pairs: (open_event_id, open_kind).
    let mut super_step_stack: Vec<(String, String)> = Vec::new();
    let mut node_stack: Vec<(String, String)> = Vec::new();
    let mut coord_stack: Vec<(String, String)> = Vec::new();

    for event in events {
        match event.kind {
            RunEventKind::SuperStepStart => {
                super_step_stack.push((event.id.clone(), "SuperStepStart".to_string()));
            }
            RunEventKind::SuperStepEnd => {
                if super_step_stack.pop().is_none() {
                    report.push(IntegrityFinding::UnpairedTurn {
                        open_event_id: event.id.clone(),
                        open_kind: "SuperStepEnd (orphan close)".to_string(),
                    });
                }
            }
            RunEventKind::NodeInput => {
                node_stack.push((event.id.clone(), "NodeInput".to_string()));
            }
            RunEventKind::NodeOutput => {
                if node_stack.pop().is_none() {
                    report.push(IntegrityFinding::UnpairedTurn {
                        open_event_id: event.id.clone(),
                        open_kind: "NodeOutput (orphan close)".to_string(),
                    });
                }
            }
            RunEventKind::CoordinationStart => {
                coord_stack.push((event.id.clone(), "CoordinationStart".to_string()));
            }
            RunEventKind::CoordinationEnd => {
                if coord_stack.pop().is_none() {
                    report.push(IntegrityFinding::UnpairedTurn {
                        open_event_id: event.id.clone(),
                        open_kind: "CoordinationEnd (orphan close)".to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    // Any remaining open events are unclosed pairs.
    for (id, kind) in super_step_stack {
        report.push(IntegrityFinding::UnpairedTurn {
            open_event_id: id,
            open_kind: kind,
        });
    }
    for (id, kind) in node_stack {
        report.push(IntegrityFinding::UnpairedTurn {
            open_event_id: id,
            open_kind: kind,
        });
    }
    for (id, kind) in coord_stack {
        report.push(IntegrityFinding::UnpairedTurn {
            open_event_id: id,
            open_kind: kind,
        });
    }
}

/// Load a [`JournalSnapshot`] from a JSON file path.
pub fn load_snapshot(path: &std::path::Path) -> Result<JournalSnapshot, String> {
    let bytes = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    serde_json::from_str(&bytes).map_err(|e| format!("parse {path:?}: {e}"))
}

/// Run verification over a file and print the report as JSON to stdout.
///
/// When `artifacts_dir` is `Some`, artifact references are verified
/// against a `FileArtifactStore` rooted at that path.
///
/// Returns `0` when the log passes, `1` when findings exist, `2` on I/O
/// or parse errors.
pub async fn verify_log_file(
    path: &std::path::Path,
    artifacts_dir: Option<&std::path::Path>,
) -> i32 {
    let snapshot = match load_snapshot(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("verify-log error: {e}");
            return 2;
        }
    };

    let store = artifacts_dir.map(|dir| rusty_agent_runtime::journal::FileArtifactStore::new(dir));

    let report = if let Some(ref store) = store {
        verify_log_with_store(snapshot, Some(store)).await
    } else {
        verify_log(snapshot)
    };

    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("verify-log serialization error: {e}");
            return 2;
        }
    }

    if report.passed {
        0
    } else {
        1
    }
}
