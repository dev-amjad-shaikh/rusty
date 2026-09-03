//! The bounded exec-reviewer for the gray zone.
//!
//! A [`ExecReviewer`] is a [`Middleware`] layer that arbitrates gray-zone
//! tool calls — calls that static policy neither allows nor denies (allowed
//! tool, supervised autonomy, no standing approval). It issues a strictly
//! bounded model call with minimal context and fails closed to human review.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::capability::PermissionPreset;
use crate::error::{LlmErrorClass, Result, RustyError};
use crate::llm::{ChatMessage, ChatModel};
use crate::middleware::{Decision, InterceptPoint, Middleware, Rejection, ToolInvocation};
use crate::record::Effect;
use crate::tool::ToolRegistry;

/// Default output token cap for reviewer calls.
pub const REVIEWER_MAX_TOKENS: u32 = 360;
/// Default wall-clock timeout for reviewer calls.
pub const REVIEWER_TIMEOUT_SECS: u64 = 30;

/// The reviewer's decision for one gray-zone call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerDecision {
    /// Authorize this single call only (never sticky, never persisted).
    Allow,
    /// Escalate to human review.
    Ask,
}

/// The strict schema the reviewer model must return.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewerResponse {
    /// The decision: allow or ask.
    pub decision: ReviewerDecision,
    /// A short risk classification.
    pub risk: String,
    /// The rationale for the decision.
    pub rationale: String,
}

/// A bounded model reviewer for gray-zone tool calls.
///
/// When enabled and the call is in the gray zone, the reviewer issues a
/// bounded model call with minimal context. On "allow" the call proceeds;
/// on "ask" or any failure the call is rejected, escalating to human review.
///
/// # Example
///
/// ```ignore
/// use rusty_agent_runtime::reviewer::ExecReviewer;
/// use rusty_agent_runtime::llm::OpenAiCompatibleClient;
/// use rusty_agent_runtime::tool::ToolRegistry;
/// use std::sync::Arc;
///
/// let reviewer = ExecReviewer::new(
///     Arc::new(OpenAiCompatibleClient::new("https://api.openai.com/v1", Some("key".into()), "gpt-4o-mini")),
///     Arc::new(registry),
/// );
/// ```
#[derive(Clone)]
pub struct ExecReviewer {
    model: Arc<dyn ChatModel>,
    #[allow(dead_code)]
    registry: Arc<ToolRegistry>,
    preset: PermissionPreset,
    max_tokens: u32,
    timeout: Duration,
    workspace_root: Option<String>,
}

impl std::fmt::Debug for ExecReviewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecReviewer")
            .field("model", &"***")
            .field("preset", &self.preset)
            .field("max_tokens", &self.max_tokens)
            .field("timeout", &self.timeout)
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

impl ExecReviewer {
    /// Create a new exec-reviewer with the given model and registry.
    ///
    /// Defaults to [`PermissionPreset::WorkspaceAsk`] posture, 360 tokens,
    /// and 30-second timeout.
    pub fn new(model: Arc<dyn ChatModel>, registry: Arc<ToolRegistry>) -> Self {
        Self {
            model,
            registry,
            preset: PermissionPreset::WorkspaceAsk,
            max_tokens: REVIEWER_MAX_TOKENS,
            timeout: Duration::from_secs(REVIEWER_TIMEOUT_SECS),
            workspace_root: None,
        }
    }

    /// Set the permission preset the reviewer watches for.
    ///
    /// The reviewer is only active when the preset is [`PermissionPreset::WorkspaceAsk`].
    pub fn with_preset(mut self, preset: PermissionPreset) -> Self {
        self.preset = preset;
        self
    }

    /// Set the output token cap.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set the wall-clock timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the workspace root path included in reviewer context.
    pub fn with_workspace_root(mut self, root: impl Into<String>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    /// `true` when the given effect class is in the gray zone for the
    /// configured permission preset.
    fn is_gray_zone(&self, effect: Effect) -> bool {
        // Only active in WorkspaceAsk posture; freely repeatable effects are
        // not in the gray zone.
        self.preset == PermissionPreset::WorkspaceAsk && !effect.is_freely_repeatable()
    }

    /// Build the minimal prompt sent to the reviewer model.
    fn build_prompt(&self, call: &ToolInvocation, effect: Effect) -> Vec<ChatMessage> {
        let context = json!({
            "tool": call.name(),
            "arguments": call.arguments(),
            "effect_class": effect_name(effect),
            "permission_mode": self.preset.as_str(),
            "workspace_root": self.workspace_root.as_deref().unwrap_or(""),
        });
        vec![
            ChatMessage::system(
                "You are a strict security reviewer. Given a tool call context, \
                 decide whether to allow it once or ask for human review. \
                 Respond ONLY with valid JSON matching this schema: \
                 {\"decision\": \"allow\" | \"ask\", \"risk\": \"string\", \"rationale\": \"string\"}. \
                 On any uncertainty, respond \"ask\".",
            ),
            ChatMessage::user(context.to_string()),
        ]
    }

    /// Call the reviewer model and parse the response against the strict schema.
    async fn review(&self, call: &ToolInvocation, effect: Effect) -> Result<ReviewerResponse> {
        let messages = self.build_prompt(call, effect);
        let response = timeout(self.timeout, self.model.chat(&messages, &[]))
            .await
            .map_err(|_| RustyError::LlmFailure {
                class: LlmErrorClass::Timeout,
                message: "exec-reviewer timed out".to_owned(),
            })??;

        let content = response.message.content.as_deref().unwrap_or("");
        if content.is_empty() {
            return Err(RustyError::LlmFailure {
                class: LlmErrorClass::Decode,
                message: "exec-reviewer returned empty response".to_owned(),
            });
        }

        let parsed: ReviewerResponse =
            serde_json::from_str(content).map_err(|e| RustyError::LlmFailure {
                class: LlmErrorClass::Decode,
                message: format!("exec-reviewer returned malformed JSON: {e}"),
            })?;

        Ok(parsed)
    }
}

#[async_trait]
impl Middleware for ExecReviewer {
    fn name(&self) -> &str {
        "exec_reviewer"
    }

    async fn before_tool(&self, call: &mut ToolInvocation) -> Decision<Value> {
        let effect = match call.effect() {
            Some(e) => e,
            None => return Decision::Continue,
        };

        if !self.is_gray_zone(effect) {
            return Decision::Continue;
        }

        match self.review(call, effect).await {
            Ok(response) => match response.decision {
                ReviewerDecision::Allow => Decision::Continue,
                ReviewerDecision::Ask => Decision::Reject(
                    Rejection::new(self.name(), InterceptPoint::ToolCall, "ask").with_detail(
                        format!("risk: {}; rationale: {}", response.risk, response.rationale),
                    ),
                ),
            },
            Err(error) => {
                // Fail-closed: every failure path lands on human review.
                Decision::Reject(
                    Rejection::new(self.name(), InterceptPoint::ToolCall, "ask")
                        .with_detail(format!("reviewer failed: {error}")),
                )
            }
        }
    }
}

/// Wire name of an effect class, for prompts and guard reasons.
fn effect_name(effect: Effect) -> &'static str {
    match effect {
        Effect::Pure => "pure",
        Effect::ReadOnly => "read_only",
        Effect::Idempotent => "idempotent",
        Effect::Compensatable => "compensatable",
        Effect::NonIdempotent => "non_idempotent",
    }
}
