//! Pause envelope: versioned snapshot with tool-identity rebinding (EP-03-S06).
//!
//! Tests the [`PauseEnvelope`] contract: schema-version floor check,
//! minimal envelope semantics, tool-identity rebinding, and sticky approval
//! round-trips.

use rusty_agent_runtime::prelude::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// Schema-version floor check
// ---------------------------------------------------------------------------

#[test]
fn version_floor_fails_loudly() {
    let envelope = PauseEnvelope {
        schema_version: PauseSchemaVersion {
            major: 0,
            minor: 9,
            patch: 0,
        },
        run_id: "run-1".into(),
        session_id: "sess-1".into(),
        log_position: 0,
        obligations: vec![],
        sticky_approvals: vec![],
        tool_identities: vec![],
        checkpoint_id: "cp-1".into(),
        created_at: chrono::Utc::now(),
    };

    let err = envelope
        .check_version()
        .expect_err("envelope below minimum version must fail the floor check");
    let msg = err.to_string();
    assert!(
        msg.contains("0.9.0"),
        "error names the envelope's version: {msg}"
    );
    assert!(
        msg.contains(&PauseSchemaVersion::MINIMUM.as_string()),
        "error names the required minimum: {msg}"
    );
    assert!(
        msg.contains("pause-envelope"),
        "error names the feature: {msg}"
    );
}

#[test]
fn current_version_passes_floor() {
    let envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    envelope
        .check_version()
        .expect("current version passes floor");
}

// ---------------------------------------------------------------------------
// Envelope minimality: no transcript content inside the envelope
// ---------------------------------------------------------------------------

#[test]
fn envelope_is_minimal_and_complete() {
    let envelope = PauseEnvelope::new("run-1", "sess-1", 42, "cp-1");
    let json = serde_json::to_value(&envelope).unwrap();

    // The envelope must NOT carry transcript, memory, or scheduler state.
    assert!(
        json.get("transcript").is_none(),
        "envelope must not contain transcript"
    );
    assert!(
        json.get("memory").is_none(),
        "envelope must not contain memory"
    );
    assert!(
        json.get("state").is_none(),
        "envelope must not contain scheduler state"
    );

    // It must carry the required fields.
    assert_eq!(json["schema_version"]["major"], 1);
    assert_eq!(json["run_id"], "run-1");
    assert_eq!(json["session_id"], "sess-1");
    assert_eq!(json["log_position"], 42);
    assert_eq!(json["checkpoint_id"], "cp-1");
}

#[test]
fn envelope_round_trip() {
    let mut envelope = PauseEnvelope::new("run-1", "sess-1", 7, "cp-1");
    envelope.obligations.push(RunObligation {
        id: "obl-1".into(),
        tool_call_id: Some("tc-1".into()),
        kind: ObligationKind::Approval {
            scope: "operator".into(),
            sticky_allowed: true,
        },
        status: ObligationStatus::Open,
        expires_at: None,
        member_run_id: None,
    });
    envelope.tool_identities.push(ToolIdentityKey {
        agent_path: "main".into(),
        qualified_tool_name: "fs_read".into(),
    });

    let json = serde_json::to_string(&envelope).unwrap();
    let back: PauseEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(envelope, back);
}

// ---------------------------------------------------------------------------
// Tool-identity rebinding
// ---------------------------------------------------------------------------

#[test]
fn rebind_unchanged_toolset_succeeds() {
    let mut envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    envelope.tool_identities = vec![
        ToolIdentityKey {
            agent_path: "main".into(),
            qualified_tool_name: "fs_read".into(),
        },
        ToolIdentityKey {
            agent_path: "main".into(),
            qualified_tool_name: "shell_exec".into(),
        },
    ];

    let toolset = vec!["fs_read".into(), "shell_exec".into()];
    match rebind_tool_identities(&envelope, &toolset) {
        ToolRebindingResult::Bound { bindings } => {
            assert_eq!(bindings.len(), 2);
            assert_eq!(bindings.get("main/fs_read"), Some(&"fs_read".into()));
            assert_eq!(bindings.get("main/shell_exec"), Some(&"shell_exec".into()));
        }
        other => panic!("expected Bound, got {other:?}"),
    }
}

#[test]
fn rebind_rename_fails_loudly() {
    let mut envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    envelope.tool_identities = vec![ToolIdentityKey {
        agent_path: "main".into(),
        qualified_tool_name: "old_tool_name".into(),
    }];

    // The tool was renamed between pause and resume.
    let toolset = vec!["new_tool_name".into()];
    match rebind_tool_identities(&envelope, &toolset) {
        ToolRebindingResult::Unbindable { keys } => {
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].qualified_tool_name, "old_tool_name");
        }
        other => panic!("expected Unbindable, got {other:?}"),
    }
}

#[test]
fn rebind_duplicate_qualified_name_fails() {
    let mut envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    envelope.tool_identities = vec![ToolIdentityKey {
        agent_path: "main".into(),
        qualified_tool_name: "fs_read".into(),
    }];

    // Two tools share the same qualified name — ambiguous, must fail.
    let toolset = vec!["fs_read".into(), "fs_read".into()];
    match rebind_tool_identities(&envelope, &toolset) {
        ToolRebindingResult::Unbindable { keys } => {
            assert_eq!(keys.len(), 1);
        }
        other => panic!("expected Unbindable on ambiguous match, got {other:?}"),
    }
}

#[test]
fn rebind_empty_tool_identities_succeeds() {
    let envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    let toolset = vec!["fs_read".into()];
    match rebind_tool_identities(&envelope, &toolset) {
        ToolRebindingResult::Bound { bindings } => {
            assert!(bindings.is_empty());
        }
        other => panic!("expected Bound with empty bindings, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Sticky approval round-trip
// ---------------------------------------------------------------------------

#[test]
fn sticky_approval_round_trip() {
    let sticky = StickyApproval {
        tool_key: ToolIdentityKey {
            agent_path: "main".into(),
            qualified_tool_name: "fs_read".into(),
        },
        grants: true,
        set_by: "maya".into(),
        set_at: chrono::Utc::now(),
    };

    let json = serde_json::to_string(&sticky).unwrap();
    let back: StickyApproval = serde_json::from_str(&json).unwrap();
    assert_eq!(sticky, back);
}

#[test]
fn envelope_with_sticky_approval_serializes_sparse() {
    let mut envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    envelope.sticky_approvals.push(StickyApproval {
        tool_key: ToolIdentityKey {
            agent_path: "main".into(),
            qualified_tool_name: "fs_read".into(),
        },
        grants: true,
        set_by: "maya".into(),
        set_at: chrono::Utc::now(),
    });

    let json = serde_json::to_value(&envelope).unwrap();
    // sticky_approvals is present because we set one.
    assert!(json.get("sticky_approvals").is_some());
    // obligations is absent because empty (sparse wire shape).
    assert!(json.get("obligations").is_none());
}

// ---------------------------------------------------------------------------
// Obligation lifecycle
// ---------------------------------------------------------------------------

#[test]
fn obligation_status_serde_roundtrip() {
    for status in [
        ObligationStatus::Open,
        ObligationStatus::Satisfied,
        ObligationStatus::Rejected,
        ObligationStatus::Expired,
    ] {
        let json = serde_json::to_value(status).unwrap();
        let back: ObligationStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, back);
    }
}

#[test]
fn obligation_kind_serde_roundtrip() {
    let kinds = [
        ObligationKind::Approval {
            scope: "operator".into(),
            sticky_allowed: true,
        },
        ObligationKind::StructuredInput {
            input_schema: json!({"type": "object"}),
        },
        ObligationKind::Feedback {
            subject_event_id: "ev-1".into(),
        },
        ObligationKind::ExternalExecution {
            tool: "stripe_charge".into(),
            arguments: json!({"amount": 100}),
            result_schema: json!({"type": "object"}),
        },
    ];

    for kind in &kinds {
        let json = serde_json::to_value(kind).unwrap();
        let back: ObligationKind = serde_json::from_value(json).unwrap();
        assert_eq!(*kind, back);
    }
}

#[test]
fn envelope_is_resumable_when_no_open_obligations() {
    let mut envelope = PauseEnvelope::new("run-1", "sess-1", 0, "cp-1");
    assert!(envelope.is_resumable());

    envelope.obligations.push(RunObligation {
        id: "obl-1".into(),
        tool_call_id: None,
        kind: ObligationKind::Approval {
            scope: "operator".into(),
            sticky_allowed: false,
        },
        status: ObligationStatus::Open,
        expires_at: None,
        member_run_id: None,
    });
    assert!(!envelope.is_resumable());

    envelope.obligations[0].status = ObligationStatus::Satisfied;
    assert!(envelope.is_resumable());
}
