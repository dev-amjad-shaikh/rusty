//! Integration tests: the bounded exec-reviewer for the gray zone.
//!
//! Covers the decision matrix (allow / ask), exhaustive fault-injection
//! (timeout, malformed JSON, schema violation), and passthrough paths
//! (disabled mode, unknown tool, freely repeatable effect).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use rusty_agent_runtime::capability::PermissionPreset;
use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::middleware::{Decision, InterceptPoint, Middleware, ToolInvocation};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::reviewer::{
    ExecReviewer, REVIEWER_MAX_TOKENS, REVIEWER_TIMEOUT_SECS, ReviewerDecision, ReviewerResponse,
};
use rusty_agent_runtime::tool::ToolRegistry;

// ---------------------------------------------------------------------------
// Mock model that returns scripted responses.
// ---------------------------------------------------------------------------

struct ScriptedModel {
    script: Mutex<VecDeque<Result<ChatResponse>>>,
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(RustyError::Llm("script exhausted".into())))
    }
}

/// A model that sleeps longer than any reasonable timeout.
struct SlowModel;

#[async_trait]
impl ChatModel for SlowModel {
    async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        tokio::time::sleep(Duration::from_secs(600)).await;
        Ok(ChatResponse {
            message: ChatMessage::assistant("too late"),
            model: None,
            usage: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn reviewer_allow_response() -> ChatResponse {
    ChatResponse {
        message: ChatMessage::assistant(
            json!({
                "decision": "allow",
                "risk": "low",
                "rationale": "routine workspace write"
            })
            .to_string(),
        ),
        model: Some("reviewer-model".into()),
        usage: None,
    }
}

fn reviewer_ask_response() -> ChatResponse {
    ChatResponse {
        message: ChatMessage::assistant(
            json!({
                "decision": "ask",
                "risk": "high",
                "rationale": "irreversible effect"
            })
            .to_string(),
        ),
        model: Some("reviewer-model".into()),
        usage: None,
    }
}

fn make_invocation(effect: Option<Effect>) -> ToolInvocation {
    let mut call = ToolInvocation::new(
        "t-1",
        "node-a",
        ToolCall::new("c1", "write_file", json!({"path": "/tmp/test.txt"})),
    );
    call.set_effect(effect);
    call
}

// ---------------------------------------------------------------------------
// (1) Allow path: reviewer says allow → call proceeds.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn allow_path_proceeds() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::from([Ok(reviewer_allow_response())])),
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk);

    let mut call = make_invocation(Some(Effect::NonIdempotent));
    let decision = reviewer.before_tool(&mut call).await;

    assert_eq!(decision, Decision::Continue);
}

// ---------------------------------------------------------------------------
// (2) Ask path: reviewer says ask → structured rejection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ask_path_rejects() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::from([Ok(reviewer_ask_response())])),
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk);

    let mut call = make_invocation(Some(Effect::NonIdempotent));
    let decision = reviewer.before_tool(&mut call).await;

    match decision {
        Decision::Reject(rejection) => {
            assert_eq!(rejection.middleware, "exec_reviewer");
            assert_eq!(rejection.point, InterceptPoint::ToolCall);
            assert_eq!(rejection.reason, "ask");
            let detail = rejection.detail.expect("detail present");
            assert!(detail.contains("high"), "risk in detail: {detail}");
            assert!(
                detail.contains("irreversible effect"),
                "rationale in detail: {detail}"
            );
        }
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (3) Timeout failure → ask (fail-closed).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_failure_asks() {
    let model = Arc::new(SlowModel);
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk)
        .with_timeout(Duration::from_millis(50));

    let mut call = make_invocation(Some(Effect::NonIdempotent));
    let decision = reviewer.before_tool(&mut call).await;

    match decision {
        Decision::Reject(rejection) => {
            assert_eq!(rejection.reason, "ask");
            let detail = rejection.detail.expect("detail present");
            assert!(detail.contains("timed out"), "timeout in detail: {detail}");
        }
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (4) Malformed JSON → ask (fail-closed).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_json_asks() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::from([Ok(ChatResponse {
            message: ChatMessage::assistant("not json at all"),
            model: None,
            usage: None,
        })])),
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk);

    let mut call = make_invocation(Some(Effect::NonIdempotent));
    let decision = reviewer.before_tool(&mut call).await;

    match decision {
        Decision::Reject(rejection) => {
            assert_eq!(rejection.reason, "ask");
            let detail = rejection.detail.expect("detail present");
            assert!(
                detail.contains("malformed JSON"),
                "malformed in detail: {detail}"
            );
        }
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (5) Schema violation (missing required field) → ask (fail-closed).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schema_violation_asks() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::from([Ok(ChatResponse {
            message: ChatMessage::assistant(
                json!({"decision": "allow", "risk": "low"}).to_string(),
            ),
            model: None,
            usage: None,
        })])),
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk);

    let mut call = make_invocation(Some(Effect::NonIdempotent));
    let decision = reviewer.before_tool(&mut call).await;

    match decision {
        Decision::Reject(rejection) => {
            assert_eq!(rejection.reason, "ask");
            let detail = rejection.detail.expect("detail present");
            assert!(
                detail.contains("malformed JSON"),
                "schema violation in detail: {detail}"
            );
        }
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (6) Disabled mode (not WorkspaceAsk) → passthrough.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_mode_passthrough() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::new()), // should never be called
    });

    for preset in [
        PermissionPreset::ReadOnly,
        PermissionPreset::Workspace,
        PermissionPreset::FullAccess,
    ] {
        let reviewer =
            ExecReviewer::new(model.clone(), Arc::new(ToolRegistry::new())).with_preset(preset);

        let mut call = make_invocation(Some(Effect::NonIdempotent));
        let decision = reviewer.before_tool(&mut call).await;
        assert_eq!(
            decision,
            Decision::Continue,
            "preset {:?} should passthrough",
            preset
        );
    }
}

// ---------------------------------------------------------------------------
// (7) Unknown tool (effect is None) → passthrough.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_tool_passthrough() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::new()), // should never be called
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk);

    let mut call = make_invocation(None);
    let decision = reviewer.before_tool(&mut call).await;

    assert_eq!(decision, Decision::Continue);
}

// ---------------------------------------------------------------------------
// (8) Freely repeatable effect → passthrough.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn freely_repeatable_passthrough() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::new()), // should never be called
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()))
        .with_preset(PermissionPreset::WorkspaceAsk);

    for effect in [Effect::Pure, Effect::ReadOnly] {
        let mut call = make_invocation(Some(effect));
        let decision = reviewer.before_tool(&mut call).await;
        assert_eq!(
            decision,
            Decision::Continue,
            "effect {:?} should passthrough",
            effect
        );
    }
}

// ---------------------------------------------------------------------------
// (9) Constants are publicly visible.
// ---------------------------------------------------------------------------

#[test]
fn constants_are_sensible() {
    assert_eq!(REVIEWER_MAX_TOKENS, 360);
    assert_eq!(REVIEWER_TIMEOUT_SECS, 30);
}

// ---------------------------------------------------------------------------
// (10) Response round-trips through strict schema.
// ---------------------------------------------------------------------------

#[test]
fn reviewer_response_round_trip() {
    let raw = json!({
        "decision": "allow",
        "risk": "low",
        "rationale": "safe"
    });
    let parsed: ReviewerResponse = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(parsed.decision, ReviewerDecision::Allow);
    assert_eq!(parsed.risk, "low");
    assert_eq!(parsed.rationale, "safe");

    let encoded = serde_json::to_value(&parsed).unwrap();
    assert_eq!(encoded, raw);
}

// ---------------------------------------------------------------------------
// (11) Debug impl does not leak model internals.
// ---------------------------------------------------------------------------

#[test]
fn debug_does_not_leak_model() {
    let model = Arc::new(ScriptedModel {
        script: Mutex::new(VecDeque::new()),
    });
    let reviewer = ExecReviewer::new(model, Arc::new(ToolRegistry::new()));
    let s = format!("{:?}", reviewer);
    assert!(s.contains("ExecReviewer"));
    assert!(s.contains("***"), "model should be redacted");
}
