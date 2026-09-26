//! Durable Work contract tests (R0.6).
//!
//! Golden files pin the serialized shapes of `TaskEnvelope` and the
//! `ErrorClass` taxonomy against checked-in JSON under `tests/golden/`. Any
//! accidental contract drift fails here. To bless an intentional contract
//! change, re-run with `UPDATE_GOLDEN=1` and review the diff.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::durable::{
    ArtifactContract, ErrorClass, TaskBudget, TaskEnvelope, TASK_ENVELOPE_FORMAT_VERSION,
};
use rusty_agent_runtime::record::{ArtifactRef, Effect, PayloadRef};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
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
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

fn sample_task_envelope() -> TaskEnvelope {
    let mut envelope = TaskEnvelope::new(
        "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:task:7",
        "019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d",
        "pool-default",
        PayloadRef::Artifact(ArtifactRef {
            sha256: "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca7".into(),
            bytes: 8192,
        }),
    );
    envelope.parent = Some("019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:12".into());
    envelope.output_contract = Some(ArtifactContract {
        kind: "application/json".into(),
        max_bytes: Some(65_536),
        schema: None,
    });
    envelope.deadline = DateTime::<Utc>::from_timestamp_millis(1_750_000_300_000);
    envelope.budget = Some(TaskBudget {
        max_attempts: 5,
        timeout_ms: Some(30_000),
    });
    envelope.idempotency_key = Some("019157c4-6f1f-7a3b-8c2d-9e4f5a6b7c8d:charge:7".into());
    envelope.worker_version = Some("activity-worker/1.4.0".into());
    envelope.effect = Effect::Idempotent;
    envelope
}

#[test]
fn golden_task_envelope_shape() {
    let envelope = sample_task_envelope();
    assert_eq!(envelope.format_version, TASK_ENVELOPE_FORMAT_VERSION);
    assert_golden("task_envelope.json", &envelope);
}

#[test]
fn golden_error_class_shape() {
    // All variants in declaration order: the variant names are the contract.
    assert_golden(
        "error_class.json",
        &vec![
            ErrorClass::Transient,
            ErrorClass::RateLimited,
            ErrorClass::Timeout,
            ErrorClass::InvalidInput,
            ErrorClass::DependencyFailure,
            ErrorClass::ResourceExhausted,
            ErrorClass::Cancelled,
            ErrorClass::Unknown,
        ],
    );
}

/// A minimal v1 envelope — only the required fields, as the smallest client
/// writes it — must keep deserializing across future additive changes.
#[test]
fn minimal_envelope_json_still_loads() {
    let minimal = json!({
        "task_id": "task-1",
        "sender": "run-9",
        "recipient": "pool-default",
        "input": {"kind": "inline", "value": {"n": 1}},
    });
    let envelope: TaskEnvelope = serde_json::from_value(minimal).unwrap();
    assert_eq!(envelope.format_version, TASK_ENVELOPE_FORMAT_VERSION);
    assert_eq!(envelope.effect, Effect::NonIdempotent);
    assert!(envelope.idempotency_key.is_none());
}
