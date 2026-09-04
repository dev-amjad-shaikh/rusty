//! The dependency-light trait ABI for the Rusty platform.
//!
//! This crate defines the shared types and traits that every implementation
//! crate depends on inward. It has no heavyweight dependencies so extensions
//! compile against it cheaply, and its versioning discipline is the platform's
//! plugin compatibility story.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Effect taxonomy
// ---------------------------------------------------------------------------

/// The effect taxonomy: what a journaled event did to the world outside the
/// run's own state.
///
/// The classification is declared by the producer (node/model/tool traits
/// carry a default with an override point) and recorded on every journaled
/// event. It is the input to three later policies:
///
/// - **Retry**: which failed effects may be re-attempted at all, and under
///   what key.
/// - **Replay**: which effects exact replay may serve from the journal versus
///   must re-execute.
/// - **Capsules**: which effects a sandboxed capsule may perform at all under
///   its capability grants.
///
/// The order of variants is a severity ladder: each class permits strictly
/// less automation freedom than the one before. The `Ord` derive is that
/// ladder made mechanical (declaration order), which is what capsule
/// manifests compare declared effects against grant-implied minima with.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// No observable effect beyond its return value: a deterministic function
    /// of its inputs. Re-execution is always safe and always equivalent, so
    /// replay may either re-run it or reuse the journaled output, and retries
    /// are unconstrained. Default for plain compute nodes.
    Pure,

    /// Reads external state but writes nothing (a GET, a file read, a
    /// lookup). Re-execution is safe but **not** necessarily equivalent — the
    /// world may have changed — so exact replay serves the journaled output
    /// while live replay re-reads. Retries are unconstrained.
    ReadOnly,

    /// Writes external state, but repeating the same call with the same
    /// idempotency key has the same effect as calling once (PUT semantics,
    /// upserts). Safe to retry under a stable key; exact replay may serve
    /// the journaled receipt instead of re-sending.
    Idempotent,

    /// Writes external state and repeating it duplicates the effect, but a
    /// declared compensating action can logically undo it (charge/refund).
    /// Retry only with care; replay and rollback policy must pair the effect
    /// with its compensation.
    Compensatable,

    /// Writes external state with no safe automatic repetition (send an
    /// email, charge a card, POST without a key). Never silently retried,
    /// never served from a journal in any replay mode that claims fidelity —
    /// re-execution is an explicit, caller-approved decision. Default for
    /// model and tool calls, which the runtime cannot prove otherwise.
    NonIdempotent,
}

impl Effect {
    /// Whether re-executing this effect during replay or retry is
    /// unconditionally safe (no duplication risk). `Compensatable` and
    /// `NonIdempotent` are the only classes requiring human or policy
    /// approval before re-execution.
    pub fn is_freely_repeatable(self) -> bool {
        matches!(self, Effect::Pure | Effect::ReadOnly | Effect::Idempotent)
    }
}

// ---------------------------------------------------------------------------
// Chat types
// ---------------------------------------------------------------------------

/// Chat message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System prompt / instructions.
    System,
    /// End-user input.
    User,
    /// Model output (may carry tool calls).
    Assistant,
    /// Tool execution result (must carry `tool_call_id`).
    Tool,
}

/// A single chat message.
///
/// Serialization follows the OpenAI chat-completions schema:
/// `content` may be null on assistant tool-call messages; `tool_calls` and
/// `tool_call_id` are omitted when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChatMessage {
    /// Who produced this message.
    pub role: Role,

    /// Text content. `None` is legal for assistant messages that only carry
    /// tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Tool calls requested by the assistant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,

    /// Required on `role: tool` messages: the tool call this answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Optional participant name (multi-agent disambiguation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// An assistant message with text content.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// An assistant message requesting tool calls (content may be empty).
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    /// A tool-result message answering `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: None,
        }
    }

    /// `true` if this is an assistant message requesting tool calls.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// A tool call requested by the model.
///
/// Wire format is the OpenAI function-calling shape:
/// `{"id": "...", "type": "function", "function": {"name": "...", "arguments": "<json string>"}}`.
/// The `arguments` field is exposed as a parsed [`Value`]; serialization
/// re-encodes it to the string form the API expects.
#[derive(Debug, Clone, PartialEq, JsonSchema)]
pub struct ToolCall {
    /// Provider-assigned call id (echoed back in the tool-result message).
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Parsed arguments.
    pub arguments: Value,
}

impl ToolCall {
    /// Convenience constructor.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

impl Serialize for ToolCall {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": self.arguments.to_string(),
            }
        })
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct FnArgs {
            name: String,
            arguments: Value,
        }
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            function: FnArgs,
        }
        let wire = Wire::deserialize(deserializer)?;
        let arguments = match wire.function.arguments {
            // Standard: arguments arrive as a JSON-encoded string.
            Value::String(s) => serde_json::from_str(&s).map_err(serde::de::Error::custom)?,
            // Lenient: some providers send a raw object.
            other => other,
        };
        Ok(ToolCall {
            id: wire.id,
            name: wire.function.name,
            arguments,
        })
    }
}

/// Token usage accounting from the provider.
///
/// `cached_tokens` and `reasoning_tokens` are the detail providers report
/// beyond the three headline counts (Anthropic's cache reads, OpenAI's
/// `prompt_tokens_details` / `completion_tokens_details`, Gemini's thoughts):
/// optional and absent on the wire when unset, so the pinned serde shape of
/// the headline fields is unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Tokens in the prompt.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Tokens in the completion.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Total tokens billed.
    #[serde(default)]
    pub total_tokens: u64,
    /// Prompt tokens served from the provider's cache. A *subset* of
    /// `prompt_tokens`, usually billed at a lower rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Completion tokens spent on reasoning rather than visible output.
    /// A *subset* of `completion_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// Per-million-token pricing for a model, supplied by whoever constructs it.
///
/// The crate ships no built-in price list: a vendor table would be stale the
/// week it ships, so rates are operator configuration attached to the model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPricing {
    /// USD per million uncached prompt tokens.
    pub input_per_million: f64,
    /// USD per million completion tokens.
    pub output_per_million: f64,
    /// USD per million cache-served prompt tokens. When absent, cached
    /// tokens bill at the full input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_million: Option<f64>,
}

impl ModelPricing {
    /// Pricing with uncached input and output rates only.
    pub fn new(input_per_million: f64, output_per_million: f64) -> Self {
        Self {
            input_per_million,
            output_per_million,
            cached_input_per_million: None,
        }
    }

    /// Builder-style: a distinct rate for cache-served prompt tokens.
    pub fn with_cached_input(mut self, cached_input_per_million: f64) -> Self {
        self.cached_input_per_million = Some(cached_input_per_million);
        self
    }

    /// The cost of `usage` in USD.
    ///
    /// Cached tokens are a subset of `prompt_tokens`, so they are charged at
    /// the cached rate *instead of* — never in addition to — the input rate.
    /// A provider reporting more cached than prompt tokens is clamped rather
    /// than trusted.
    pub fn cost_usd(&self, usage: &Usage) -> f64 {
        let cached = usage.cached_tokens.unwrap_or(0).min(usage.prompt_tokens);
        let uncached = usage.prompt_tokens - cached;
        let cached_rate = self
            .cached_input_per_million
            .unwrap_or(self.input_per_million);
        let million = 1_000_000.0;
        (uncached as f64 * self.input_per_million
            + cached as f64 * cached_rate
            + usage.completion_tokens as f64 * self.output_per_million)
            / million
    }
}

/// One chat-completion response (single choice; multi-choice responses are
/// not modeled).
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The assistant message (text and/or tool calls).
    pub message: ChatMessage,
    /// The model that produced the response, when reported.
    pub model: Option<String>,
    /// Token usage, when reported.
    pub usage: Option<Usage>,
}

/// One incremental piece of a streamed completion.
///
/// `on_token` callbacks receive a sequence of `TokenChunk`s: zero or more
/// with `finish: false` carrying text deltas, terminated by exactly one with
/// `finish: true` (whose `delta` is empty for truly-streaming
/// implementations).
#[derive(Debug, Clone)]
pub struct TokenChunk {
    /// The incremental text produced since the previous chunk. May be empty
    /// (e.g. on the terminal chunk, or on chunks that only carry tool-call
    /// deltas).
    pub delta: String,

    /// `true` on the final chunk of the stream.
    pub finish: bool,

    /// The raw provider chunk (the decoded SSE `data:` JSON), when the
    /// implementation streams from a wire protocol. `None` for synthetic
    /// chunks (default fallback, mocks).
    pub raw: Option<Value>,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

use thiserror::Error;

/// Errors arising from ABI-level operations in the Rusty platform.
#[derive(Debug, Error, Clone, PartialEq, JsonSchema)]
pub enum RustyApiError {
    /// A provider returned an error or the request could not be completed.
    #[error("provider error: {0}")]
    Provider(String),

    /// A capability or feature is not supported by the implementation.
    #[error("not supported: {0}")]
    NotSupported(String),

    /// The request was malformed or violated an invariant.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

// ---------------------------------------------------------------------------
// ModelProvider trait
// ---------------------------------------------------------------------------

/// Capability descriptors returned by [`ModelProvider::capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
pub struct ModelCapabilities {
    /// Whether the provider supports streaming responses.
    pub streaming: bool,
    /// Whether the provider supports function/tool calling.
    pub tool_calling: bool,
    /// Whether the provider supports reasoning tokens.
    pub reasoning: bool,
    /// Context-window size in tokens, if known.
    pub context_window: Option<u64>,
}

/// The engine-owned neutral model-provider trait.
///
/// Every model backend — OpenAI, Anthropic, local, gateway — implements this
/// trait so the kernel dispatches without knowing wire details.
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// Produce a complete response for the conversation.
    async fn get_response(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> std::result::Result<ChatResponse, RustyApiError>;

    /// Stream token deltas for the conversation.
    ///
    /// The default implementation falls back to [`ModelProvider::get_response`] and delivers
    /// the whole assistant text as a single [`TokenChunk`] with `finish: true`.
    async fn stream_response(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> std::result::Result<ChatResponse, RustyApiError> {
        let response = self.get_response(messages, tools).await?;
        on_token(TokenChunk {
            delta: response.message.content.clone().unwrap_or_default(),
            finish: true,
            raw: None,
        });
        Ok(response)
    }

    /// Query the capabilities of this provider/model combination.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    /// The declared effect classification of calling this model.
    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }

    /// Per-token pricing, when known.
    fn pricing(&self) -> Option<ModelPricing> {
        None
    }
}

// ---------------------------------------------------------------------------
// Channel trait
// ---------------------------------------------------------------------------

/// A normalized inbound message delivered to the kernel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboundMessage {
    /// Stable participant identity.
    pub from: String,
    /// The message payload.
    pub content: String,
    /// When the message was received.
    pub received_at: chrono::DateTime<chrono::Utc>,
}

/// A normalized outbound message produced by the kernel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutboundMessage {
    /// Target participant identity.
    pub to: String,
    /// The message payload.
    pub content: String,
}

/// The channel abstraction: send outbound messages and listen for inbound.
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// Send one outbound message.
    async fn send(&self, message: OutboundMessage) -> std::result::Result<(), RustyApiError>;

    /// Listen for inbound messages, returning a stream.
    ///
    /// The implementation produces inbound messages until the stream is
    /// dropped or an unrecoverable error occurs.
    fn listen(&self) -> std::pin::Pin<Box<dyn futures_core::Stream<Item = InboundMessage> + Send>>;
}

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

/// The dual-representation output of a tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    /// Human-readable text (rendered in chat, logged, shown to users).
    pub display: String,
    /// Machine-structured value (passed to downstream tools, stored, scored).
    pub structured: Value,
}

/// The effect class a tool declares, used for placement and sandboxing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// No side effects beyond computation.
    Read,
    /// May read and write, but execution is the expected path.
    Execute,
    /// Network egress.
    Egress,
}

/// The sandbox requirement a tool declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRequirement {
    /// No sandbox needed.
    None,
    /// Local process sandbox.
    LocalProcess,
    /// Container sandbox.
    Container,
    /// Remote sandbox.
    Remote,
}

/// The normative tool trait — every tool implements this surface.
///
/// Re-exported from `contracts:tool` semantics: a tool has a name, a JSON
/// Schema argument shape, an effect classification, and an async invoke
/// method returning dual-representation output.
pub trait Tool: Send + Sync {
    /// The tool's registered name (unique within its scope).
    fn name(&self) -> &str;

    /// A JSON Schema describing the arguments this tool accepts.
    fn schema(&self) -> Value;

    /// The effect class of this tool.
    fn effect_class(&self) -> EffectClass;

    /// The sandbox required to run this tool safely.
    fn sandbox_requirement(&self) -> SandboxRequirement;

    /// Invoke the tool with parsed arguments.
    fn invoke(
        &self,
        arguments: Value,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<ToolOutput, RustyApiError>>
                + Send
                + '_,
        >,
    >;
}

// ---------------------------------------------------------------------------
// Memory trait
// ---------------------------------------------------------------------------

/// One entry in episodic memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryEntry {
    /// Stable entry identity.
    pub id: uuid::Uuid,
    /// The serialized content.
    pub content: String,
    /// When the entry was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Optional provenance: who or what produced this entry.
    pub provenance: Option<String>,
}

/// A write request for a memory block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BlockWrite {
    /// Block identifier.
    pub block_id: String,
    /// The payload to store.
    pub payload: Value,
    /// Whether this write should overwrite an existing block.
    pub overwrite: bool,
}

/// The memory trait: recall query and entry/block write surface.
#[async_trait::async_trait]
pub trait Memory: Send + Sync {
    /// Recall entries matching `query`, up to `limit`.
    async fn recall(
        &self,
        query: &str,
        limit: usize,
    ) -> std::result::Result<Vec<MemoryEntry>, RustyApiError>;

    /// Write a batch of blocks.
    async fn write_blocks(&self, writes: Vec<BlockWrite>)
        -> std::result::Result<(), RustyApiError>;
}

// ---------------------------------------------------------------------------
// Observer trait
// ---------------------------------------------------------------------------

/// Lifecycle facts an [`Observer`] may be notified of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "event_kind")]
pub enum ObserverEvent {
    /// An event was appended to the journal.
    EventAppended { event_id: uuid::Uuid, kind: String },
    /// A phase was entered.
    PhaseEntered { phase: String, step: u32 },
    /// A phase was exited.
    PhaseExited {
        phase: String,
        step: u32,
        summary: Option<Value>,
    },
    /// A seam decision was taken.
    SeamDecision {
        seam: String,
        handlers: Vec<String>,
        outcome: String,
    },
    /// An invariant violation was detected.
    InvariantViolation { invariant: String, detail: String },
    /// A budget was consumed.
    BudgetConsumed {
        kind: String,
        limit: u64,
        consumed: u64,
    },
}

/// Typed, non-blocking notifications for lifecycle facts.
///
/// Observers are fire-and-forget: the kernel never awaits an observer.
pub trait Observer: Send + Sync {
    /// Notify the observer of a lifecycle event.
    fn notify(&self, event: ObserverEvent);
}

// ---------------------------------------------------------------------------
// RuntimeAdapter trait
// ---------------------------------------------------------------------------

/// The enforcement level reported by a runtime backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementLevel {
    /// Full policy enforcement.
    Full,
    /// Partial enforcement (degraded or best-effort).
    Partial,
}

/// The execution-backend seam: provision, execute-with-policy, teardown.
#[async_trait::async_trait]
pub trait RuntimeAdapter: Send + Sync {
    /// Provision the backend for an execution.
    async fn provision(&self, run_id: uuid::Uuid) -> std::result::Result<(), RustyApiError>;

    /// Execute a task under the given policy.
    async fn execute(
        &self,
        run_id: uuid::Uuid,
        task: Value,
        policy: Value,
    ) -> std::result::Result<ToolOutput, RustyApiError>;

    /// Tear down the provisioned resources for a run.
    async fn teardown(&self, run_id: uuid::Uuid) -> std::result::Result<(), RustyApiError>;

    /// Report the current enforcement level.
    fn enforcement(&self) -> EnforcementLevel;
}
