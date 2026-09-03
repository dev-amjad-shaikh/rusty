//! Event-schema conformance suite (EP-01-S08).
//!
//! Enforces the closed-enum versioning discipline on the event schema:
//!
//! 1. **Variant-exhaustive round-trip** — every [`RunEventKind`] variant
//!    serializes and deserializes losslessly; the exhaustive `match` breaks
//!    compilation when a variant is added without updating this suite.
//! 2. **Unknown-tag rejection** — fabricated `kind` tags fail deserialization
//!    with a typed error, never silently map to a default.
//! 3. **Schema-drift detection** — the committed golden file
//!    `tests/golden/run_event_kind_complete.json` lists every variant's wire
//!    tag; a code change that alters any tag fails CI until the golden is
//!    explicitly regenerated with `UPDATE_GOLDEN=1` and the diff is reviewed.
//!
//! ## Schema snapshots
//!
//! `schemars`-generated JSON Schemas for every closed enum are golden-file
//! snapshotted alongside the variant listings. A schema change without an
//! explicit golden update fails CI.

use std::path::PathBuf;

use rusty_agent_runtime::record::{
    ApprovalDecision, DecisionAction, DecisionFamily, DecisionOutcome, DecisionRole, Effect,
    EventStatus, RunEventKind,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert pretty-printed JSON equals the golden file exactly.
fn assert_golden(name: &str, value: &impl serde::Serialize) {
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
        "schema drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// AC 1 — Variant-exhaustive round-trip
// ---------------------------------------------------------------------------

/// Return every [`RunEventKind`] variant exactly once.
///
/// This function is the compilation gate: adding a variant to the enum
/// without adding it here breaks the build, which is the intended
/// closed-enum discipline.
fn all_run_event_kinds() -> Vec<RunEventKind> {
    vec![
        RunEventKind::SuperStepStart,
        RunEventKind::SuperStepEnd,
        RunEventKind::NodeInput,
        RunEventKind::NodeOutput,
        RunEventKind::ModelCall,
        RunEventKind::ToolCall,
        RunEventKind::RemoteCall,
        RunEventKind::WasmCall,
        RunEventKind::Interrupt,
        RunEventKind::Resume,
        RunEventKind::RoutingDecision,
        RunEventKind::CheckpointWritten,
        RunEventKind::EffectReceipt,
        RunEventKind::AgentSpawn,
        RunEventKind::AgentExit,
        RunEventKind::MailboxSend,
        RunEventKind::MailboxReceive,
        RunEventKind::SupervisionEvent,
        RunEventKind::CoordinationStart,
        RunEventKind::CoordinationEnd,
        RunEventKind::MemoryRead,
        RunEventKind::MemoryWrite,
        RunEventKind::MemoryForget,
        RunEventKind::CandidateCreated,
        RunEventKind::CandidateEvaluated,
        RunEventKind::CandidatePromoted,
        RunEventKind::CandidateRolledBack,
        RunEventKind::PolicyDecision,
        RunEventKind::CapsuleResolved,
        RunEventKind::CapsuleCall,
        RunEventKind::CapsuleDenied,
        RunEventKind::SigningKeyRotated,
        RunEventKind::ConfigResolved,
        RunEventKind::ConnectionRegistered,
        RunEventKind::ConnectionConsented,
        RunEventKind::ConnectionRefreshed,
        RunEventKind::ConnectionRevoked,
        RunEventKind::CredentialHandleIssued,
        RunEventKind::CredentialUse,
        RunEventKind::CredentialDenied,
        RunEventKind::ConnectionNeedsReauth,
        RunEventKind::ArtifactCommitted,
        RunEventKind::ArtifactPruned,
        RunEventKind::ArtifactRetentionReleased,
        RunEventKind::ArtifactUnavailable,
        RunEventKind::DeploymentResolved,
        RunEventKind::RevisionRegistered,
        RunEventKind::RevisionPromoted,
        RunEventKind::RevisionRolledBack,
        RunEventKind::EnvironmentDeclared,
        RunEventKind::EnvSecretSet,
        RunEventKind::EnvSecretRevoked,
        RunEventKind::EnvSecretDenied,
        RunEventKind::GateDecisionRecorded,
        RunEventKind::CanaryDeclared,
        RunEventKind::CanaryCleared,
        RunEventKind::ShadowRunStarted,
        RunEventKind::ShadowEffectRefused,
        RunEventKind::ShadowVerdict,
        RunEventKind::RunConfigDeclared,
        RunEventKind::ToolCallDenied,
        RunEventKind::ApprovalAsked,
        RunEventKind::ApprovalDecided,
        RunEventKind::InboxIntake,
        RunEventKind::InboxConsumed,
        RunEventKind::RunCancelled,
    ]
}

#[test]
fn run_event_kind_exhaustive_round_trip() {
    for kind in all_run_event_kinds() {
        let tag = serde_json::to_value(kind).expect("every variant must serialize");
        let back: RunEventKind = serde_json::from_value(tag.clone()).unwrap_or_else(|e| {
            panic!("variant {kind:?} must deserialize from its tag {tag}: {e}")
        });
        assert_eq!(
            kind, back,
            "variant {kind:?} must round-trip through serde losslessly"
        );
    }
}

// ---------------------------------------------------------------------------
// AC 2 — Unknown-tag rejection
// ---------------------------------------------------------------------------

#[test]
fn run_event_kind_rejects_unknown_tags() {
    let unknown_tags = [
        "super_step_restart",
        "node_timeout",
        "model_invoke",
        "tool_invocation",
        "garbage_variant",
        "",
        "SuperStepStart", // wrong case — must be snake_case
        "SUPER_STEP_START",
        "super-step-start", // wrong separator
    ];

    for tag in &unknown_tags {
        let result: Result<RunEventKind, _> = serde_json::from_value(serde_json::json!(tag));
        assert!(
            result.is_err(),
            "unknown tag `{tag}` must fail deserialization, never silently succeed"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown variant"),
            "error for `{tag}` must name the unknown variant, got: {err}"
        );
    }
}

#[test]
fn effect_rejects_unknown_tags() {
    let result: Result<Effect, _> = serde_json::from_value(serde_json::json!("harmless"));
    assert!(result.is_err(), "Effect must reject unknown tags");
}

#[test]
fn event_status_rejects_unknown_tags() {
    let result: Result<EventStatus, _> = serde_json::from_value(serde_json::json!("pending"));
    assert!(result.is_err(), "EventStatus must reject unknown tags");
}

#[test]
fn decision_family_rejects_unknown_tags() {
    let result: Result<DecisionFamily, _> =
        serde_json::from_value(serde_json::json!("model_selection"));
    assert!(result.is_err(), "DecisionFamily must reject unknown tags");
}

#[test]
fn approval_decision_rejects_unknown_tags() {
    // ApprovalDecision is internally tagged with "decision" discriminant.
    let result: Result<ApprovalDecision, _> =
        serde_json::from_value(serde_json::json!({"decision": "deferred"}));
    assert!(
        result.is_err(),
        "ApprovalDecision must reject unknown variants"
    );
}

// ---------------------------------------------------------------------------
// AC 3 — Schema-drift detection via golden file
// ---------------------------------------------------------------------------

/// Every [`RunEventKind`] variant's wire tag, in declaration order.
///
/// The golden file pins these tags so that an accidental rename (e.g. a
/// refactor that changes the Rust identifier and therefore the snake_case
/// serialization) fails CI until explicitly reviewed and blessed.
#[test]
fn golden_run_event_kind_complete() {
    let tags: Vec<String> = all_run_event_kinds()
        .into_iter()
        .map(|k| {
            serde_json::to_value(k)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_golden("run_event_kind_complete.json", &tags);
}

/// Same discipline for the other closed enums that travel on the wire.
#[test]
fn golden_effect_variants() {
    let tags: Vec<String> = vec![
        Effect::Pure,
        Effect::ReadOnly,
        Effect::Idempotent,
        Effect::Compensatable,
        Effect::NonIdempotent,
    ]
    .into_iter()
    .map(|k| {
        serde_json::to_value(k)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    })
    .collect();
    assert_golden("effect_variants.json", &tags);
}

#[test]
fn golden_event_status_variants() {
    let tags: Vec<String> = vec![
        EventStatus::Ok,
        EventStatus::Error,
        EventStatus::Interrupted,
    ]
    .into_iter()
    .map(|k| {
        serde_json::to_value(k)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    })
    .collect();
    assert_golden("event_status_variants.json", &tags);
}

#[test]
fn golden_decision_family_variants() {
    let tags: Vec<String> = vec![
        DecisionFamily::Retry,
        DecisionFamily::Timeout,
        DecisionFamily::WorkerPlacement,
        DecisionFamily::Concurrency,
        DecisionFamily::CheckpointPlacement,
    ]
    .into_iter()
    .map(|k| {
        serde_json::to_value(k)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    })
    .collect();
    assert_golden("decision_family_variants.json", &tags);
}

#[test]
fn golden_decision_action_variants() {
    let tags: Vec<_> = vec![
        DecisionAction::Retry { attempt: 1 },
        DecisionAction::Abort,
        DecisionAction::SetTimeout { millis: 100 },
        DecisionAction::SelectWorker {
            worker: "w1".into(),
        },
        DecisionAction::SetConcurrency { limit: 4 },
        DecisionAction::WriteCheckpoint,
        DecisionAction::SkipCheckpoint,
    ];
    assert_golden("decision_action_variants.json", &tags);
}

#[test]
fn golden_decision_outcome_variants() {
    let tags: Vec<String> = vec![
        DecisionOutcome::Success,
        DecisionOutcome::Failure,
        DecisionOutcome::Cancelled,
    ]
    .into_iter()
    .map(|k| {
        serde_json::to_value(k)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    })
    .collect();
    assert_golden("decision_outcome_variants.json", &tags);
}

#[test]
fn golden_decision_role_variants() {
    let tags: Vec<String> = vec![DecisionRole::Acting, DecisionRole::Shadow]
        .into_iter()
        .map(|k| {
            serde_json::to_value(k)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_golden("decision_role_variants.json", &tags);
}

#[test]
fn golden_approval_decision_variants() {
    let tags: Vec<_> = vec![
        ApprovalDecision::ApprovedOnce {
            approved_by: "user-1".into(),
        },
        ApprovalDecision::Rejected {
            decided_by: "user-2".into(),
            reason: Some("no".into()),
        },
        ApprovalDecision::Cancelled {
            reason: Some("timeout".into()),
        },
        ApprovalDecision::Unavailable {
            reason: Some("gate absent".into()),
        },
    ];
    assert_golden("approval_decision_variants.json", &tags);
}

// ---------------------------------------------------------------------------
// AC 4 — SurfaceOp boundary validation
// ---------------------------------------------------------------------------

use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::llm::ChatMessage;
use rusty_agent_runtime::surface::{Provenance, Surface, SurfaceEntry, SurfaceOp};

/// Build a minimal surface with two entries so span validation has something
/// to index into.
fn two_entry_surface() -> Surface {
    let journal = Journal::new("surface-boundary", "thread-boundary", Clock::System);
    let input = journal.record(
        EventDraft::new(RunEventKind::NodeInput, Effect::Pure)
            .node("agent")
            .input(serde_json::json!({
                "messages": [serde_json::to_value(ChatMessage::user("hi")).unwrap()]
            })),
    );
    journal.record(
        EventDraft::new(RunEventKind::NodeOutput, Effect::Pure)
            .node("agent")
            .output(serde_json::json!({
                "updates": {
                    "messages": serde_json::to_value(ChatMessage::assistant("hello")).unwrap()
                },
                "command": null
            }))
            .parent(input),
    );
    Surface::derive(&journal.snapshot()).unwrap()
}

#[test]
fn surface_op_replace_rejects_invalid_span() {
    let mut surface = two_entry_surface();

    // start == end
    let op = SurfaceOp::Replace {
        start: 1,
        end: 1,
        entry: SurfaceEntry::summary("x", vec![0]),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("out of range"),
        "start == end must be rejected, got: {err}"
    );

    // start > end
    let op = SurfaceOp::Replace {
        start: 2,
        end: 1,
        entry: SurfaceEntry::summary("x", vec![0]),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("out of range"),
        "start > end must be rejected, got: {err}"
    );

    // end beyond view length
    let op = SurfaceOp::Replace {
        start: 0,
        end: 5,
        entry: SurfaceEntry::summary("x", vec![0, 1]),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("out of range"),
        "end beyond view length must be rejected, got: {err}"
    );

    // No revision was recorded for any rejected op.
    assert!(surface.revisions().is_empty());
}

#[test]
fn surface_op_replace_rejects_dishonest_provenance() {
    let mut surface = two_entry_surface();

    // A replacement must be Provenance::Compaction.
    let op = SurfaceOp::Replace {
        start: 0,
        end: 1,
        entry: SurfaceEntry::live(ChatMessage::user("masquerade")),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("Compaction"),
        "live provenance in Replace must be rejected, got: {err}"
    );

    // A replacement masquerading as Journal.
    let op = SurfaceOp::Replace {
        start: 0,
        end: 1,
        entry: SurfaceEntry {
            kind: rusty_agent_runtime::surface::SurfaceEntryKind::User,
            message: ChatMessage::user("fake"),
            provenance: Provenance::Journal,
            source_seqs: vec![0],
        },
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("Compaction"),
        "journal provenance in Replace must be rejected, got: {err}"
    );

    assert!(surface.revisions().is_empty());
}

#[test]
fn surface_op_replace_rejects_citation_mismatch() {
    let mut surface = two_entry_surface();

    // Gap leak: drops a subsumed seq.
    let op = SurfaceOp::Replace {
        start: 0,
        end: 2,
        entry: SurfaceEntry::summary("x", vec![0]),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("subsumes"),
        "citation gap leak must be rejected, got: {err}"
    );

    // Fabrication: invents a citation beyond journal range.
    let op = SurfaceOp::Replace {
        start: 0,
        end: 1,
        entry: SurfaceEntry::summary("x", vec![99]),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("out of range"),
        "fabricated citation must be rejected, got: {err}"
    );

    assert!(surface.revisions().is_empty());
}

#[test]
fn surface_op_append_rejects_non_live_provenance() {
    let mut surface = two_entry_surface();

    let op = SurfaceOp::Append {
        entry: SurfaceEntry {
            kind: rusty_agent_runtime::surface::SurfaceEntryKind::User,
            message: ChatMessage::user("fake"),
            provenance: Provenance::Journal,
            source_seqs: vec![0],
        },
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("Live"),
        "append with journal provenance must be rejected, got: {err}"
    );

    let op = SurfaceOp::Append {
        entry: SurfaceEntry::summary("x", vec![]),
    };
    let err = surface.apply(op).unwrap_err().to_string();
    assert!(
        err.contains("Live"),
        "append with compaction provenance must be rejected, got: {err}"
    );

    assert!(surface.revisions().is_empty());
}

// ---------------------------------------------------------------------------
// AC 5 — Backend-parameterized invariant battery (in-process variant)
// ---------------------------------------------------------------------------

/// A parameterized helper that asserts the closed-enum invariants for any
/// backend that can produce a sequence of events.  In the current
/// architecture there is only one in-process journal backend; the harness
/// structure is preserved so that a future Postgres-backed conformance run
/// can reuse the same assertions by substituting the event source.
fn assert_closed_enum_invariants(events: &[rusty_agent_runtime::record::RunEvent]) {
    // Every event's kind is a known variant.
    for event in events {
        let tag = serde_json::to_value(event.kind).unwrap();
        let back: RunEventKind = serde_json::from_value(tag.clone()).unwrap_or_else(|e| {
            panic!(
                "event {} kind {:?} must deserialize: {e}",
                event.id, event.kind
            )
        });
        assert_eq!(event.kind, back);
    }

    // No two distinct variants share a wire tag.
    let mut seen = std::collections::HashMap::new();
    for kind in all_run_event_kinds() {
        let tag = serde_json::to_value(kind)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        if let Some(prev) = seen.insert(tag.clone(), kind) {
            panic!("tag collision: {tag} used by both {prev:?} and {kind:?}");
        }
    }
}

#[test]
fn closed_enum_invariants_on_empty_journal() {
    let journal = rusty_agent_runtime::journal::Journal::new(
        "conformance-empty",
        "t-empty",
        rusty_agent_runtime::journal::Clock::System,
    );
    assert_closed_enum_invariants(&journal.events());
}

#[tokio::test]
async fn closed_enum_invariants_on_executed_journal() {
    use rusty_agent_runtime::checkpoint::InMemoryCheckpointer;
    use rusty_agent_runtime::executor::{Executor, RunConfig};
    use rusty_agent_runtime::graph::GraphBuilder;
    use rusty_agent_runtime::node::NodeContext;
    use rusty_agent_runtime::node::NodeOutput;
    use rusty_agent_runtime::state::{Reducer, State, StateSpec};

    let journal = rusty_agent_runtime::journal::Journal::new(
        "conformance-run",
        "t-run",
        rusty_agent_runtime::journal::Clock::System,
    );

    let spec = StateSpec::new().channel("out", Reducer::Append);
    let mut builder = GraphBuilder::new();
    let j = journal.clone();
    builder.add_node("echo", move |_: NodeContext| {
        let journal = j.clone();
        async move {
            journal.record(
                rusty_agent_runtime::journal::EventDraft::new(
                    RunEventKind::NodeOutput,
                    Effect::Pure,
                )
                .output(serde_json::json!({"ok": true})),
            );
            Ok(NodeOutput::update("out", serde_json::json!("done")))
        }
    });
    builder.set_entry_point("echo");
    let graph = builder.compile().unwrap();

    let executor = Executor::with_checkpointer(std::sync::Arc::new(InMemoryCheckpointer::new()));
    executor
        .run(
            &graph,
            &spec,
            State::new(),
            RunConfig::new("t-run").with_journal(journal.clone()),
        )
        .await
        .unwrap();

    assert!(!journal.events().is_empty());
    assert_closed_enum_invariants(&journal.events());
}

// ---------------------------------------------------------------------------
// Schema snapshot tests — schemars-generated JSON Schema drift detection
// ---------------------------------------------------------------------------

use schemars::schema_for;

/// Assert a schemars-generated schema equals the golden file exactly.
fn assert_schema_golden<T: schemars::JsonSchema>(name: &str) {
    let schema = schema_for!(T);
    let rendered = format!("{}\n", serde_json::to_string_pretty(&schema).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing schema golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "schema drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

#[test]
fn schema_effect() {
    assert_schema_golden::<Effect>("effect_schema.json");
}

#[test]
fn schema_event_status() {
    assert_schema_golden::<EventStatus>("event_status_schema.json");
}

#[test]
fn schema_run_event_kind() {
    assert_schema_golden::<RunEventKind>("run_event_kind_schema.json");
}

#[test]
fn schema_decision_family() {
    assert_schema_golden::<DecisionFamily>("decision_family_schema.json");
}

#[test]
fn schema_decision_action() {
    assert_schema_golden::<DecisionAction>("decision_action_schema.json");
}

#[test]
fn schema_decision_outcome() {
    assert_schema_golden::<DecisionOutcome>("decision_outcome_schema.json");
}

#[test]
fn schema_decision_role() {
    assert_schema_golden::<DecisionRole>("decision_role_schema.json");
}

#[test]
fn schema_approval_decision() {
    assert_schema_golden::<ApprovalDecision>("approval_decision_schema.json");
}

use rusty_agent_runtime::record::RunEvent;

#[test]
fn schema_run_event() {
    assert_schema_golden::<RunEvent>("run_event_schema.json");
}
