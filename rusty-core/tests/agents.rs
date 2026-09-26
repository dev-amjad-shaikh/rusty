//! Agent Fabric contract tests (R0.7 wave 1).
//!
//! Golden files pin the serialized shapes of `CapabilityManifest`,
//! `StateScope`, the schema-carrying `ArtifactContract`, and the extended
//! `RunEventKind` set against checked-in JSON under `tests/golden/`. Any
//! accidental contract drift fails here. To bless an intentional contract
//! change, re-run with `UPDATE_GOLDEN=1` and review the diff.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::agents::{
    AgentBudget, CapabilityManifest, EscalationNotice, RestartPolicy, StateScope,
    SupervisionAttempt, SupervisionPolicy, SupervisionTrigger,
};
use rusty_agent_runtime::durable::{ArtifactContract, ErrorClass};
use rusty_agent_runtime::record::RunEventKind;

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

fn sample_manifest() -> CapabilityManifest {
    let mut accepts = BTreeMap::new();
    accepts.insert(
        "summarize".to_string(),
        ArtifactContract {
            kind: "application/json".into(),
            max_bytes: Some(65_536),
            schema: Some(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"topic": {"type": "string"}},
                "required": ["topic"],
            })),
        },
    );
    accepts.insert(
        "search".to_string(),
        ArtifactContract {
            kind: "application/json".into(),
            max_bytes: None,
            schema: None,
        },
    );
    CapabilityManifest {
        agent_kind: "researcher".into(),
        manifest_version: "researcher/1.4.0".into(),
        accepts,
        scopes: vec![StateScope::Private, StateScope::Team, StateScope::User],
        budget: Some(AgentBudget {
            max_tokens: Some(250_000),
            max_cost_usd: Some(1.50),
            deadline: DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000),
        }),
        // Wave-2 field deliberately unset here: the pinned golden must
        // stay byte-identical across the additive change — the proof that
        // pre-wave-2 readers see no shape drift.
        supervision: None,
    }
}

#[test]
fn golden_capability_manifest_shape() {
    assert_golden("capability_manifest.json", &sample_manifest());
}

#[test]
fn golden_state_scope_shape() {
    // All variants in declaration order: the variant names are the contract.
    assert_golden(
        "state_scope.json",
        &vec![
            StateScope::Private,
            StateScope::Team,
            StateScope::User,
            StateScope::Tenant,
        ],
    );
}

#[test]
fn golden_artifact_contract_with_schema_shape() {
    // The R0.7 contract carrying its optional JSON Schema (draft 2020-12).
    assert_golden(
        "artifact_contract.json",
        &ArtifactContract {
            kind: "application/json".into(),
            max_bytes: Some(65_536),
            schema: Some(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"topic": {"type": "string"}},
                "required": ["topic"],
            })),
        },
    );
}

#[test]
fn golden_run_event_kind_shape() {
    // The full closed enum in declaration order: the pre-R0.7 names are
    // unchanged (old journals keep deserializing) and the R0.7 agent-fabric
    // variants append after `effect_receipt` — the same additive evolution
    // rule the EffectReceipt variant followed in R0.6.
    assert_golden(
        "run_event_kind.json",
        &vec![
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
        ],
    );
}

/// A minimal manifest — only the required fields, as the smallest client
/// writes it — must keep deserializing across future additive changes.
#[test]
fn minimal_manifest_json_still_loads() {
    let minimal = json!({
        "agent_kind": "researcher",
        "manifest_version": "researcher/1.4.0",
    });
    let manifest: CapabilityManifest = serde_json::from_value(minimal).unwrap();
    assert!(manifest.accepts.is_empty());
    assert!(manifest.scopes.is_empty());
    assert!(manifest.budget.is_none());
    assert!(manifest.supervision.is_none());
}

#[test]
fn golden_supervision_policy_shape() {
    // The wave-2 contract: OTP restart vocabulary + intensity/period +
    // the supervisor address.
    assert_golden(
        "supervision_policy.json",
        &SupervisionPolicy {
            restart: RestartPolicy::Permanent,
            intensity: 3,
            period_ms: 60_000,
            supervisor: Some("boss".into()),
        },
    );
}

#[test]
fn golden_escalation_notice_shape() {
    // The escalation message payload: failed agent, the exhausted policy
    // verbatim, and the full attempt history as evidence.
    let t0 = DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000).unwrap();
    assert_golden(
        "escalation_notice.json",
        &EscalationNotice {
            agent_id: "looper".into(),
            policy: SupervisionPolicy {
                restart: RestartPolicy::Permanent,
                intensity: 2,
                period_ms: 60_000,
                supervisor: Some("boss".into()),
            },
            attempts: vec![
                SupervisionAttempt {
                    ordinal: 1,
                    trigger: SupervisionTrigger::TurnFailed,
                    error_class: Some(ErrorClass::Transient),
                    message: "model timed out".into(),
                    task_id: Some("task-1".into()),
                    at: t0,
                },
                SupervisionAttempt {
                    ordinal: 2,
                    trigger: SupervisionTrigger::TurnFailed,
                    error_class: Some(ErrorClass::Timeout),
                    message: "turn lease lapsed mid-effect".into(),
                    task_id: Some("task-1".into()),
                    at: t0 + chrono::Duration::seconds(4),
                },
            ],
            escalated_at: t0 + chrono::Duration::seconds(9),
        },
    );
}

/// A pre-R0.7 artifact contract (no `schema` key — the exact R0.6 golden
/// shape) must keep deserializing, with the schema unset.
#[test]
fn r06_artifact_contract_json_still_loads() {
    let r06_shape = json!({"kind": "application/json", "max_bytes": 65536});
    let contract: ArtifactContract = serde_json::from_value(r06_shape).unwrap();
    assert_eq!(contract.kind, "application/json");
    assert_eq!(contract.max_bytes, Some(65_536));
    assert_eq!(contract.schema, None);
}

/// A pre-R0.7 journal event kind string resolves exactly as before; the
/// enum's extension must not move existing names.
#[test]
fn pre_r07_event_kinds_still_load() {
    assert_eq!(
        serde_json::from_value::<RunEventKind>(json!("effect_receipt")).unwrap(),
        RunEventKind::EffectReceipt
    );
    assert_eq!(
        serde_json::from_value::<RunEventKind>(json!("super_step_start")).unwrap(),
        RunEventKind::SuperStepStart
    );
}
