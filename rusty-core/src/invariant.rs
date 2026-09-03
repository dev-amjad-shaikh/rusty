//! The model-visible-means-logged invariant (EP-01-S05): nothing reaches a
//! provider that the log cannot reconstruct.
//!
//! The substrate's central promise — every byte the model saw is
//! reconstructable from the journal — is only worth what enforces it. This
//! module is that enforcement: a checker on the [`ChatModel`] dispatch seam
//! that, immediately before a request would leave the process, recomputes
//! the message window the log says this invocation must see and compares
//! the outgoing history against it byte-for-byte over the canonical
//! serialization. Divergence — an injected string, a mutated message, a
//! reordered transcript, content a seam handler added without a logged
//! event — fails the call with
//! [`InvariantViolation::UnloggedContent`] before any bytes reach the
//! provider.
//!
//! The recomputation anchors on the invocation's own journaled
//! [`RunEventKind::NodeInput`] event: its input payload is the exact scoped
//! state snapshot the executor handed the node, which is the folded
//! persisted window by construction. The durable-inbox batch delivered at
//! the step's boundary rides into the model call but is not yet in the
//! channel snapshot (the node appends it only after the call), so the
//! derivation extends the window with the journaled
//! [`RunEventKind::InboxConsumed`] batches recorded since the last
//! super-step barrier — converted to user messages exactly as the ReAct
//! agent converts them.
//!
//! The check is read-only: a passing request proceeds and nothing is
//! journaled ([`RunEventKind`] gains no verification event), and a rejected
//! request pollutes neither the provider nor the log. Rejections surface
//! through `tracing` with the violation's structured details, so the
//! observer pipeline (rusty-otel) sees every refusal.
//!
//! Wire the checker by wrapping the dispatch model in
//! [`CheckingChatModel`]; the prebuilt ReAct constructors do this
//! automatically whenever the run journals. Additional request assertions —
//! the `TurnStamp` requirement of EP-02-S10 is the slated second one —
//! register through [`InvariantChecker::with_assertion`] and run after the
//! reconstructability check, on the same seam.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::inbox::InboxConsumption;
use crate::journal::Journal;
use crate::llm::{ChatMessage, ChatModel, ChatResponse, TokenChunk};
use crate::record::RunEventKind;

/// The channel whose contents the model's history segment must
/// reconstruct: the prebuilt agents' conversation channel.
const HISTORY_CHANNEL: &str = crate::react::MESSAGES_CHANNEL;

/// How an outgoing provider request failed the model-visible-means-logged
/// invariant.
///
/// The violation is typed and serializable: the step's outcome, the
/// `tracing` notification, and any caller-side handling all read the same
/// shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "violation", rename_all = "snake_case")]
pub enum InvariantViolation {
    /// The request's history segment diverges from what the log
    /// reconstructs for this invocation. `index` is the first divergent
    /// message position — the length boundary when one side is a strict
    /// prefix of the other.
    UnloggedContent {
        /// First divergent message index.
        index: usize,
        /// A human-readable summary of the divergence.
        detail: String,
        /// The expected message at `index`, when one exists.
        #[serde(skip_serializing_if = "Option::is_none")]
        expected: Option<Value>,
        /// The actual message at `index`, when one exists.
        #[serde(skip_serializing_if = "Option::is_none")]
        actual: Option<Value>,
    },
}

impl fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnloggedContent { index, detail, .. } => {
                write!(f, "unlogged content at message {index}: {detail}")
            }
        }
    }
}

impl std::error::Error for InvariantViolation {}

/// The request a registered [`RequestAssertion`] inspects. Everything an
/// assertion may consult is borrowed; assertions are read-only by
/// contract.
pub struct AssertionRequest<'a> {
    /// The thread (session) the call belongs to.
    pub thread_id: &'a str,
    /// The node dispatching the call.
    pub node: &'a str,
    /// The outgoing history segment, after all seam rewrites.
    pub messages: &'a [ChatMessage],
    /// The outgoing tool schemas.
    pub tools: &'a [Value],
    /// The run's journal, for assertions that consult the evidence.
    pub journal: &'a Journal,
}

/// A request assertion registered on the checker alongside the built-in
/// reconstructability check (EP-01-S05 AC 5's registration point; the
/// `TurnStamp` requirement of EP-02-S10 plugs in here).
pub trait RequestAssertion: Send + Sync {
    /// A stable name, recorded on violations.
    fn name(&self) -> &str;
    /// Inspect the request; return the violation on rejection.
    fn check(&self, request: &AssertionRequest<'_>) -> std::result::Result<(), InvariantViolation>;
}

/// The longest JSON fragment carried into a violation's diff summary.
const DIFF_SNIPPET: usize = 160;

/// Truncate a serialized message for the diff summary.
fn snippet(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= DIFF_SNIPPET {
        text
    } else {
        format!("{}…", &text[..DIFF_SNIPPET])
    }
}

/// The first position where `actual` diverges from `expected`, with both
/// sides' messages at that position (whichever exists).
fn first_divergence(expected: &[Value], actual: &[Value]) -> (usize, Option<Value>, Option<Value>) {
    let shared = expected.len().min(actual.len());
    for i in 0..shared {
        if expected[i] != actual[i] {
            return (i, Some(expected[i].clone()), Some(actual[i].clone()));
        }
    }
    let boundary = shared;
    (
        boundary,
        expected.get(boundary).cloned(),
        actual.get(boundary).cloned(),
    )
}

/// Recompute the message window the log says the invocation journaled as
/// `node_input_event` must see: the scoped state snapshot the executor
/// handed the node (the folded persisted window), extended with the
/// durable-inbox batch consumed at this step's boundary, converted to user
/// messages exactly as the ReAct agent converts them.
///
/// This is the checker's independent recomputation; the outgoing request
/// is compared against its result, never the other way around.
pub fn derive_expected_messages(
    journal: &Journal,
    node_input_event: &str,
) -> Result<Vec<ChatMessage>> {
    let events = journal.events();
    let parent = events
        .iter()
        .find(|event| event.id == node_input_event)
        .ok_or_else(|| {
            RustyError::InvariantViolation(InvariantViolation::UnloggedContent {
                index: 0,
                detail: format!(
                    "the invocation's node-input event `{node_input_event}` is not in the journal"
                ),
                expected: None,
                actual: None,
            })
        })?;
    if parent.kind != RunEventKind::NodeInput {
        return Err(RustyError::InvariantViolation(
            InvariantViolation::UnloggedContent {
                index: 0,
                detail: format!(
                    "event `{node_input_event}` is a {:?}, not the invocation's node input",
                    parent.kind
                ),
                expected: None,
                actual: None,
            },
        ));
    }
    let snapshot = parent
        .input
        .as_ref()
        .and_then(|payload| journal.resolve(payload))
        .ok_or_else(|| {
            RustyError::InvariantViolation(InvariantViolation::UnloggedContent {
                index: 0,
                detail: format!(
                    "the node-input event `{node_input_event}` carries no resolvable state snapshot"
                ),
                expected: None,
                actual: None,
            })
        })?;
    let mut window: Vec<ChatMessage> = match snapshot.get(HISTORY_CHANNEL) {
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            RustyError::InvariantViolation(InvariantViolation::UnloggedContent {
                index: 0,
                detail: format!(
                    "the journaled `{HISTORY_CHANNEL}` channel does not deserialize: {error}"
                ),
                expected: None,
                actual: None,
            })
        })?,
        None => Vec::new(),
    };

    // The inbox batch this step's boundary delivered is journaled between
    // the last barrier and this invocation's node input; the agent appends
    // it to the window it sends (and only later to the channel).
    let last_barrier = events
        .iter()
        .filter(|event| event.kind == RunEventKind::SuperStepEnd && event.seq < parent.seq)
        .map(|event| event.seq)
        .max();
    for event in events.iter().filter(|event| {
        event.kind == RunEventKind::InboxConsumed
            && event.seq < parent.seq
            && last_barrier.is_none_or(|barrier| event.seq > barrier)
    }) {
        let consumption: InboxConsumption = event
            .output
            .as_ref()
            .and_then(|payload| journal.resolve(payload))
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                RustyError::InvariantViolation(InvariantViolation::UnloggedContent {
                    index: window.len(),
                    detail: "an inbox-consumption event does not resolve".to_owned(),
                    expected: None,
                    actual: None,
                })
            })?;
        window.extend(consumption.messages.iter().map(|message| {
            ChatMessage::user(match &message.content {
                Value::String(text) => text.clone(),
                other => serde_json::to_string(other).expect("a JSON value always serializes"),
            })
        }));
    }
    Ok(window)
}

/// The invariant checker: recomputes the log-derived expectation for an
/// outgoing request and rejects divergence, then runs any registered
/// assertions. Cheap to clone (the journal is shared).
#[derive(Clone)]
pub struct InvariantChecker {
    journal: Journal,
    assertions: Vec<Arc<dyn RequestAssertion>>,
}

impl InvariantChecker {
    /// A checker over `journal` with only the built-in reconstructability
    /// check.
    pub fn new(journal: Journal) -> Self {
        Self {
            journal,
            assertions: Vec::new(),
        }
    }

    /// Register an additional request assertion (EP-01-S05 AC 5). Runs
    /// after the reconstructability check, in registration order.
    pub fn with_assertion(mut self, assertion: Arc<dyn RequestAssertion>) -> Self {
        self.assertions.push(assertion);
        self
    }

    /// Verify that `messages` — the history segment about to be sent by
    /// `node` in the invocation journaled as `node_input_event` — is
    /// exactly what the log reconstructs, then run the registered
    /// assertions. Read-only: a pass journals nothing, and a rejection
    /// journals nothing either; the refusal is announced through `tracing`
    /// for the observer pipeline.
    pub fn check_request(
        &self,
        thread_id: &str,
        node: &str,
        node_input_event: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> std::result::Result<(), InvariantViolation> {
        let expected = derive_expected_messages(&self.journal, node_input_event).map_err(
            |error| match error {
                RustyError::InvariantViolation(violation) => violation,
                other => InvariantViolation::UnloggedContent {
                    index: 0,
                    detail: format!("the log-derived recomputation failed: {other}"),
                    expected: None,
                    actual: None,
                },
            },
        )?;
        let expected_values: Vec<Value> = expected
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()
            .expect("a chat message always serializes");
        let actual_values: Vec<Value> = messages
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()
            .expect("a chat message always serializes");
        // Byte-for-byte over the canonical serialization: struct field
        // order is fixed, so string equality is structural equality.
        if expected_values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            != actual_values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
        {
            let (index, expected_at, actual_at) =
                first_divergence(&expected_values, &actual_values);
            let detail = match (&expected_at, &actual_at) {
                (Some(expected), Some(actual)) => {
                    format!("expected {}, got {}", snippet(expected), snippet(actual))
                }
                (Some(expected), None) => format!(
                    "the request dropped the logged message {}",
                    snippet(expected)
                ),
                (None, Some(actual)) => format!(
                    "the request carries content the log never saw: {}",
                    snippet(actual)
                ),
                (None, None) => unreachable!("equal-length divergence is caught above"),
            };
            let violation = InvariantViolation::UnloggedContent {
                index,
                detail,
                expected: expected_at,
                actual: actual_at,
            };
            tracing::warn!(
                thread_id,
                node,
                violation = %violation,
                "model-visible-means-logged invariant rejected a provider request"
            );
            return Err(violation);
        }
        let request = AssertionRequest {
            thread_id,
            node,
            messages,
            tools,
            journal: &self.journal,
        };
        for assertion in &self.assertions {
            if let Err(violation) = assertion.check(&request) {
                tracing::warn!(
                    thread_id,
                    node,
                    assertion = assertion.name(),
                    violation = %violation,
                    "a registered request assertion rejected a provider request"
                );
                return Err(violation);
            }
        }
        Ok(())
    }
}

/// A [`ChatModel`] decorator enforcing the model-visible-means-logged
/// invariant on every dispatch: the check runs before the wrapped model is
/// touched, in release builds included — this is correctness, not a debug
/// assertion. A rejection returns
/// [`RustyError::InvariantViolation`] and the provider mock's call count
/// stays at zero: no bytes leave the process.
///
/// Construct one per invocation (the prebuilt ReAct agents do): the
/// `parent_event` anchor is the invocation's journaled node-input event,
/// which only exists once the executor has scheduled it.
pub struct CheckingChatModel {
    inner: Arc<dyn ChatModel>,
    checker: InvariantChecker,
    thread_id: String,
    node: String,
    parent_event: String,
}

impl CheckingChatModel {
    /// Wrap `inner` so every call is checked against `checker`, anchored
    /// on the journaled node-input event `parent_event`.
    pub fn new(
        inner: Arc<dyn ChatModel>,
        checker: InvariantChecker,
        thread_id: impl Into<String>,
        node: impl Into<String>,
        parent_event: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            checker,
            thread_id: thread_id.into(),
            node: node.into(),
            parent_event: parent_event.into(),
        }
    }

    fn check(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<()> {
        self.checker
            .check_request(
                &self.thread_id,
                &self.node,
                &self.parent_event,
                messages,
                tools,
            )
            .map_err(RustyError::InvariantViolation)
    }
}

#[async_trait]
impl ChatModel for CheckingChatModel {
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        self.check(messages, tools)?;
        self.inner.chat(messages, tools).await
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> Result<ChatResponse> {
        self.check(messages, tools)?;
        self.inner.chat_stream(messages, tools, on_token).await
    }

    fn effect(&self) -> crate::record::Effect {
        self.inner.effect()
    }

    fn pricing(&self) -> Option<crate::llm::ModelPricing> {
        self.inner.pricing()
    }
}
