//! Coordination pattern contract tests (R0.7 wave 3).
//!
//! Golden files pin the serialized shapes of the four pattern contracts
//! (`CoordinationContract` with its `pattern` tag), the member-task payload
//! (`CoordinationMessage`), the settled outcome (`CoordinationOutcome`), and
//! the assembled cross-journal trace (`TeamTrace`) against checked-in JSON
//! under `tests/golden/`. Any accidental contract drift fails here. To bless
//! an intentional contract change, re-run with `UPDATE_GOLDEN=1` and review
//! the diff.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::agents::{
    ContextGrant, CoordinationContract, CoordinationKind, CoordinationMessage, CoordinationOutcome,
    CoordinationStatus, DelegateContract, Delegation, FanOutContract, MemberDisposition,
    MemberFailurePolicy, MemberSettlement, QuorumContract, QuorumResolver, QuorumResolverRecord,
    RaceContract, StateScope,
};
use rusty_agent_runtime::durable::{ArtifactContract, ErrorClass};
use rusty_agent_runtime::llm::Usage;
use rusty_agent_runtime::record::{Effect, PayloadRef, RunEventKind};
use rusty_agent_runtime::team_trace::{TeamTrace, TeamTraceNode};

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

fn fixed_now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000).unwrap()
}

fn delegation(member: &str, effect: Effect) -> Delegation {
    Delegation {
        member: member.into(),
        agent_id: format!("{member}-agent"),
        manifest_version: "researcher/1.4.0".into(),
        kind: "summarize".into(),
        input: PayloadRef::inline(json!({"topic": member})),
        effect,
        deadline: None,
    }
}

#[test]
fn golden_delegate_contract_shape() {
    let mut delegate = delegation("writer", Effect::NonIdempotent);
    delegate.deadline = Some(fixed_now());
    let contract = CoordinationContract::Delegate(Box::new(DelegateContract {
        delegate,
        context: Some(ContextGrant {
            scopes: vec![StateScope::Private, StateScope::Team],
            channels: vec!["thread:team-7".into()],
        }),
        result_contract: Some(ArtifactContract {
            kind: "application/json".into(),
            max_bytes: Some(65_536),
            schema: None,
        }),
        handoff: true,
    }));
    assert_golden("coordination_delegate_contract.json", &contract);
}

#[test]
fn golden_fan_out_contract_shape() {
    let contract = CoordinationContract::FanOut(FanOutContract {
        members: vec![
            delegation("alpha", Effect::Pure),
            delegation("beta", Effect::ReadOnly),
        ],
        max_in_flight: 2,
        on_member_failure: MemberFailurePolicy::Partial,
    });
    assert_golden("coordination_fan_out_contract.json", &contract);
}

#[test]
fn golden_race_contract_shape() {
    let contract = CoordinationContract::Race(RaceContract {
        candidates: vec![
            delegation("fast", Effect::Idempotent),
            delegation("slow", Effect::Idempotent),
        ],
    });
    assert_golden("coordination_race_contract.json", &contract);
}

#[test]
fn golden_quorum_contract_shape() {
    // The Custom resolver shape is pinned too: wave 3 rejects it at
    // submission, but the wire shape is the future registry's contract.
    let contract = CoordinationContract::Quorum(QuorumContract {
        members: vec![
            delegation("juror-a", Effect::Pure),
            delegation("juror-b", Effect::Pure),
            delegation("juror-c", Effect::Pure),
        ],
        threshold: 2,
        resolver: QuorumResolver::MajorityEqual,
    });
    assert_golden("coordination_quorum_contract.json", &contract);
    assert_golden(
        "coordination_quorum_custom_resolver.json",
        &QuorumResolver::Custom {
            name: "semantic_vote".into(),
        },
    );
}

#[test]
fn golden_coordination_message_shape() {
    let message = CoordinationMessage {
        coordination_id: "c-42".into(),
        member: "alpha".into(),
        pattern: CoordinationKind::FanOut,
        input: PayloadRef::inline(json!({"topic": "alpha"})),
        context: Some(ContextGrant {
            scopes: vec![StateScope::Team],
            channels: vec![],
        }),
    };
    assert_golden("coordination_message.json", &message);
}

#[test]
fn golden_coordination_outcome_shape() {
    // A quorum outcome exercises every field at once: resolver record,
    // per-member dispositions with results, failure evidence, and waste
    // accounting.
    let outcome = CoordinationOutcome {
        coordination_id: "c-42".into(),
        pattern: CoordinationKind::Quorum,
        status: CoordinationStatus::Completed,
        result: Some(PayloadRef::inline(json!({"answer": "X"}))),
        members: vec![
            MemberDisposition {
                member: "juror-a".into(),
                task_id: "acme--c-42--juror-a".into(),
                settlement: MemberSettlement::Completed,
                result: Some(PayloadRef::inline(json!({"answer": "X"}))),
                error_class: None,
                error: None,
                tokens: Some(Usage {
                    prompt_tokens: 120,
                    completion_tokens: 30,
                    total_tokens: 150,
                    cached_tokens: None,
                    reasoning_tokens: None,
                }),
                cost_usd: Some(0.0042),
            },
            MemberDisposition {
                member: "juror-b".into(),
                task_id: "acme--c-42--juror-b".into(),
                settlement: MemberSettlement::Failed,
                result: None,
                error_class: Some(ErrorClass::Transient),
                error: Some("model timed out".into()),
                tokens: None,
                cost_usd: None,
            },
            MemberDisposition {
                member: "juror-c".into(),
                task_id: "acme--c-42--juror-c".into(),
                settlement: MemberSettlement::Cancelled,
                result: None,
                error_class: None,
                error: None,
                tokens: None,
                cost_usd: None,
            },
        ],
        wasted_tokens: Some(150),
        wasted_cost_usd: Some(0.0042),
        resolver: Some(QuorumResolverRecord {
            resolver: QuorumResolver::MajorityEqual,
            inputs: vec![json!({"answer": "X"}), json!({"answer": "X"})],
            output: Some(json!({"answer": "X"})),
            decided: true,
        }),
    };
    assert_golden("coordination_outcome.json", &outcome);
}

#[test]
fn golden_team_trace_shape() {
    // A minimal assembled trace: the coordination spine with one member
    // journal stitched under the send.
    let trace = TeamTrace {
        run_ids: vec!["coordination:acme:c-42".into(), "run:juror-a".into()],
        roots: vec!["coordination:acme:c-42:1".into()],
        nodes: vec![
            TeamTraceNode {
                event_id: "coordination:acme:c-42:1".into(),
                run_id: "coordination:acme:c-42".into(),
                seq: 1,
                kind: RunEventKind::CoordinationStart,
                parent: None,
                children: vec!["coordination:acme:c-42:2".into()],
                depth: Some(0),
            },
            TeamTraceNode {
                event_id: "coordination:acme:c-42:2".into(),
                run_id: "coordination:acme:c-42".into(),
                seq: 2,
                kind: RunEventKind::MailboxSend,
                parent: Some("coordination:acme:c-42:1".into()),
                children: vec!["coordination:acme:c-42:3".into(), "run:juror-a:1".into()],
                depth: Some(1),
            },
            TeamTraceNode {
                event_id: "coordination:acme:c-42:3".into(),
                run_id: "coordination:acme:c-42".into(),
                seq: 3,
                kind: RunEventKind::CoordinationEnd,
                parent: Some("coordination:acme:c-42:1".into()),
                children: vec![],
                depth: Some(1),
            },
            TeamTraceNode {
                event_id: "run:juror-a:1".into(),
                run_id: "run:juror-a".into(),
                seq: 1,
                kind: RunEventKind::SuperStepStart,
                parent: Some("coordination:acme:c-42:2".into()),
                children: vec![],
                depth: Some(2),
            },
        ],
    };
    assert_golden("team_trace.json", &trace);
}
