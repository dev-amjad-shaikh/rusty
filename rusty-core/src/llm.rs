//! LLM abstraction and an OpenAI-compatible chat client.
//!
//! [`ChatModel`] is the minimal async chat-completion interface used by
//! agent nodes (the prebuilt ReAct agent only needs `chat`). Messages use
//! the OpenAI wire conventions: roles, assistant `tool_calls`, and tool
//! results carried by `role: "tool"` messages with `tool_call_id`.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};

use crate::error::{LlmErrorClass, Result, RustyError};

/// Maximum characters of an HTTP error body embedded in an error message:
/// enough for diagnosis, bounded so a verbose server cannot bloat logs.
/// Shared with `crate::remote`.
pub(crate) const ERROR_BODY_MAX_CHARS: usize = 512;

/// Truncate a response body for inclusion in an error message.
pub(crate) fn truncate_body(body: &str) -> String {
    body.chars().take(ERROR_BODY_MAX_CHARS).collect()
}

/// Exponential backoff `base * 2^attempt` with the exponent capped at 6 and
/// cheap time-based jitter (×[0.5, 1.5)) so concurrent retry loops
/// decorrelate without pulling in a `rand` dependency. Shared with
/// `crate::remote`.
pub(crate) fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let base = base.saturating_mul(2u32.saturating_pow(attempt.min(6)));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter = 0.5 + f64::from(nanos % 1000) / 1000.0; // [0.5, 1.5)
    let jittered = (base.as_nanos() as f64 * jitter) as u128;
    Duration::from_nanos(jittered.min(u64::MAX as u128) as u64)
}

/// Chat message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Provider-assigned call id (echoed back in the tool-result message).
    pub id: String,
    /// Tool name (must match a registered [`crate::tool::Tool::name`]).
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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
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
/// week it ships, so rates are operator configuration attached to the model
/// (see [`OpenAiCompatibleClient::with_pricing`]). The journaling path turns
/// `pricing × usage` into the `cost_usd` evidence the run aggregates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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

/// One incremental piece of a streamed completion (the LangGraph `messages`
/// stream-mode analog at the model level).
///
/// `on_token` callbacks of [`ChatModel::chat_stream`] receive a sequence of
/// `TokenChunk`s: zero or more with `finish: false` carrying text deltas,
/// terminated by exactly one with `finish: true` (whose `delta` is empty for
/// truly-streaming implementations).
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

/// The chat-model interface used by agent nodes.
///
/// `tools` are OpenAI-format tool schemas (`{"type": "function", "function":
/// {...}}`); pass an empty slice for a plain completion. See
/// [`crate::tool::ToolRegistry::schemas`].
///
/// # Streaming tokens into the executor's event channel
///
/// [`ChatModel::chat_stream`] is pull-based: the `on_token` callback fires
/// once per token delta. To surface those deltas as
/// [`crate::executor::GraphEvent::Token`]s (the LangGraph `messages` stream
/// mode), clone the run's event sender into the node closure and forward
/// each chunk — the executor's `event_tx` channel is the shared sink:
///
/// ```ignore
/// use rusty_agent_runtime::executor::{GraphEvent, RunConfig};
/// use rusty_agent_runtime::llm::{ChatModel, ChatMessage, TokenChunk};
///
/// let (tx, mut rx) = tokio::sync::mpsc::channel::<GraphEvent>(64);
/// let node_tx = tx.clone();                  // captured by the node closure
/// let config = RunConfig::new("t-1").with_event_tx(tx);
/// // Convenience handles for wiring the clone into nodes:
/// //   RunConfig::token_tx()  -> Option<mpsc::Sender<GraphEvent>>
/// //   Executor::with_token_tx(tx) / Executor::token_tx()
///
/// // ...inside the node:
/// // let response = model
/// //     .chat_stream(&messages, &tools, &mut |chunk: TokenChunk| {
/// //         if !chunk.delta.is_empty() {
/// //             let _ = node_tx.try_send(GraphEvent::Token {
/// //                 node: "agent".into(),
/// //                 delta: chunk.delta,
/// //             });
/// //         }
/// //     })
/// //     .await?;
/// ```
///
/// Forwarding uses `try_send` (best-effort), matching the executor's own
/// emission policy: a full or closed channel drops tokens but never aborts
/// the run.
///
/// The prebuilt ReAct agent comes in two flavors for exactly this wiring:
/// [`crate::react::create_react_agent`] never streams (it calls
/// [`ChatModel::chat`], so no `Token` events can fire), while
/// [`crate::react::create_react_agent_streaming`] performs the forwarding
/// above internally.
#[async_trait]
pub trait ChatModel: Send + Sync {
    /// Produce the next assistant message for the conversation.
    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse>;

    /// The declared effect classification of calling this model (Flight
    /// Recorder, R0.5): recorded on model-call journal events and used by
    /// retry/replay policy.
    ///
    /// The default is [`crate::record::Effect::NonIdempotent`]: a provider
    /// call is billable and unverifiable, so the restrictive class applies.
    /// Override only with justification (e.g. a local deterministic model
    /// could argue for `ReadOnly`; cached completions for `Idempotent`).
    fn effect(&self) -> crate::record::Effect {
        crate::record::Effect::NonIdempotent
    }

    /// The model's per-token pricing, when known.
    ///
    /// The journaling path multiplies this by each call's reported [`Usage`]
    /// to produce the `cost_usd` on model-call events — the field's only
    /// producer, and the input `rusty-eval`'s cost gates read. The default
    /// is `None`: a model that cannot price itself journals no cost and
    /// behaves exactly as before.
    fn pricing(&self) -> Option<ModelPricing> {
        None
    }

    /// Produce the next assistant message, streaming token deltas through
    /// `on_token` as they arrive.
    ///
    /// The default implementation falls back to [`ChatModel::chat`] and
    /// delivers the whole assistant text as a single [`TokenChunk`] with
    /// `finish: true`, so existing implementors remain source-compatible.
    /// Implementations with a streaming wire protocol (e.g.
    /// [`OpenAiCompatibleClient`]) override this to deliver real deltas.
    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> Result<ChatResponse> {
        let response = self.chat(messages, tools).await?;
        on_token(TokenChunk {
            delta: response.message.content.clone().unwrap_or_default(),
            finish: true,
            raw: None,
        });
        Ok(response)
    }
}

/// Default number of retries *after* the initial attempt for transient
/// failures (connect errors, timeouts, HTTP 5xx / 408 / 429).
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default base delay for exponential backoff between retries (exponent
/// capped, jittered; see [`OpenAiCompatibleClient::with_backoff`]).
pub const DEFAULT_BASE_BACKOFF: Duration = Duration::from_millis(100);

/// A client for any OpenAI-compatible `/chat/completions` endpoint (OpenAI,
/// Azure-OpenAI-compatible gateways, vLLM, Ollama, LM Studio, ...).
///
/// Uses `reqwest` with rustls; no default TLS features.
///
/// Transient failures are retried with capped, jittered exponential backoff
/// (same classification policy as `crate::remote::RemoteNode`): connect
/// errors, timeouts, and HTTP 5xx / 408 / 429 are retryable; a `Retry-After`
/// header (integer seconds) floors the delay. Other 4xx statuses and
/// request/response decode errors are fatal. Configure with
/// [`OpenAiCompatibleClient::with_retries`] /
/// [`OpenAiCompatibleClient::with_backoff`].
#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    base_url: String,
    api_key: Option<String>,
    model: String,
    client: reqwest::Client,
    /// Retries after the initial attempt.
    max_retries: u32,
    /// Base delay for exponential backoff.
    base_backoff: Duration,
    /// Operator-supplied pricing; `None` means cost is not computed.
    pricing: Option<ModelPricing>,
}

// Hand-written because the derived impl would print `api_key` in cleartext
// on any `{:?}` (logging middleware, panic messages, `dbg!`).
impl std::fmt::Debug for OpenAiCompatibleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("model", &self.model)
            .field("client", &self.client)
            .field("max_retries", &self.max_retries)
            .field("base_backoff", &self.base_backoff)
            .field("pricing", &self.pricing)
            .finish()
    }
}

impl OpenAiCompatibleClient {
    /// A client for `base_url` (e.g. `https://api.openai.com/v1`) serving
    /// `model`. Trailing slashes on `base_url` are trimmed.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
            client: reqwest::Client::new(),
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff: DEFAULT_BASE_BACKOFF,
            pricing: None,
        }
    }

    /// Read the API key from an environment variable.
    ///
    /// A missing variable maps to "no key" (requests go out unauthenticated
    /// and typically fail 401 far from the cause), so a warning is logged at
    /// construction time to keep the cause close to the effect.
    pub fn from_env(
        base_url: impl Into<String>,
        api_key_env: &str,
        model: impl Into<String>,
    ) -> Self {
        let api_key = std::env::var(api_key_env).ok();
        if api_key.is_none() {
            tracing::warn!(
                env_var = api_key_env,
                "API key environment variable is not set; sending requests unauthenticated"
            );
        }
        Self::new(base_url, api_key, model)
    }

    /// Override the underlying `reqwest::Client` (timeouts, proxies, ...).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Override the number of retries after the initial attempt
    /// (`0` = single attempt, no retries).
    pub fn with_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the base backoff delay (attempt *n* waits roughly
    /// `base * 2^n`, exponent capped and jittered).
    pub fn with_backoff(mut self, base_backoff: Duration) -> Self {
        self.base_backoff = base_backoff;
        self
    }

    /// Attach per-token pricing so journaled model calls carry `cost_usd`.
    /// Rates are operator configuration — the crate ships no price list.
    pub fn with_pricing(mut self, pricing: ModelPricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// The model this client requests.
    pub fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Send the request produced by `build()` with the configured retry
    /// policy. Only the initial request/response exchange is covered; once a
    /// 2xx response is returned, streaming-read failures are not retried.
    async fn send_with_retries(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut attempt: u32 = 0;
        loop {
            match self.try_send(build()).await {
                Ok(response) => return Ok(response),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Retryable { error, retry_after })
                    if attempt < self.max_retries =>
                {
                    let mut delay = backoff_delay(self.base_backoff, attempt);
                    if let Some(floor) = retry_after {
                        // A server that says Retry-After knows better than
                        // our backoff guess.
                        delay = delay.max(floor);
                    }
                    tracing::warn!(
                        url = %self.base_url,
                        attempt = attempt + 1,
                        max_retries = self.max_retries,
                        backoff_ms = delay.as_millis() as u64,
                        error = %error,
                        "chat completions attempt failed; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(AttemptError::Retryable { error, .. }) => return Err(error),
            }
        }
    }

    /// One HTTP attempt. `Ok` means a 2xx response (the body may still be
    /// streamed); `Err` is classified for retry.
    async fn try_send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> std::result::Result<reqwest::Response, AttemptError> {
        let response = request.send().await.map_err(|e| {
            // Connect failures and timeouts never produced a response, so
            // they are retryable; anything else at this layer (redirect,
            // body, builder errors) is treated as definitive.
            let class = if e.is_timeout() || e.is_connect() {
                LlmErrorClass::Timeout
            } else {
                LlmErrorClass::Unknown
            };
            let err = RustyError::LlmFailure {
                class,
                message: format!("request to {} failed: {e}", self.base_url),
            };
            if class == LlmErrorClass::Timeout {
                AttemptError::Retryable {
                    error: err,
                    retry_after: None,
                }
            } else {
                AttemptError::Fatal(err)
            }
        })?;

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response.text().await.unwrap_or_default();
        let err = RustyError::LlmFailure {
            class: classify_status(status),
            message: format!(
                "chat completions returned {status}: {}",
                truncate_body(&body)
            ),
        };
        // 5xx and 408/429 are transient by convention; other 4xx are
        // definitive (bad request, auth failure, ...).
        let retryable = status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        Err(if retryable {
            AttemptError::Retryable {
                error: err,
                retry_after,
            }
        } else {
            AttemptError::Fatal(err)
        })
    }
}

/// Map an HTTP failure status onto the LLM error taxonomy: 429 is a rate
/// limit, 408 and 5xx are the provider's own failure, 401/403 are
/// credentials, and every other 4xx is the request's fault.
fn classify_status(status: reqwest::StatusCode) -> LlmErrorClass {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        LlmErrorClass::RateLimited
    } else if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
        LlmErrorClass::Server
    } else if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        LlmErrorClass::Auth
    } else if status.is_client_error() {
        LlmErrorClass::InvalidRequest
    } else {
        LlmErrorClass::Unknown
    }
}

/// Internal classification of a failed HTTP attempt (mirrors
/// `crate::remote::AttemptError`).
#[derive(Debug)]
enum AttemptError {
    /// Transient failure eligible for retry (connect, timeout, 5xx/408/429),
    /// with an optional server-provided `Retry-After` floor.
    Retryable {
        error: RustyError,
        retry_after: Option<Duration>,
    },
    /// Definitive failure; never retried.
    Fatal(RustyError),
}

/// Wire shape of one completion choice.
#[derive(Deserialize)]
struct WireChoice {
    message: ChatMessage,
}

/// Wire shape of the completion response body.
#[derive(Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Wire shape of one streaming chunk (`stream: true`).
#[derive(Deserialize)]
struct WireStreamChunk {
    #[serde(default)]
    choices: Vec<WireStreamChoice>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Wire shape of one streaming choice.
///
/// Note: the wire's `finish_reason` field is deliberately not modeled here.
/// With `stream_options.include_usage`, OpenAI-compatible servers send the
/// terminal usage chunk *after* the chunk whose choice carries
/// `finish_reason: "stop"`, so terminating on `finish_reason` would drop
/// usage accounting. Stream termination is instead driven by the `[DONE]`
/// sentinel (with end-of-body as the fallback for providers that omit it).
#[derive(Deserialize)]
struct WireStreamChoice {
    delta: WireStreamDelta,
}

/// Wire shape of the incremental delta inside a streaming chunk.
#[derive(Deserialize)]
struct WireStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCallDelta>>,
}

/// Wire shape of an incremental tool-call delta (indexed slots; `id`,
/// `name`, and `arguments` arrive piecewise and are concatenated per index).
#[derive(Deserialize)]
struct WireToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunctionDelta>,
}

/// Wire shape of the function fragment inside a tool-call delta.
#[derive(Deserialize)]
struct WireFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Accumulates streaming deltas into a final [`ChatResponse`].
#[derive(Default)]
struct StreamAccumulator {
    content: String,
    tool_calls: Vec<ToolCallAccumulator>,
    model: Option<String>,
    usage: Option<Usage>,
}

/// Per-index accumulation of one streamed tool call.
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    fn into_response(self) -> Result<ChatResponse> {
        let mut tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (index, acc) in self.tool_calls.into_iter().enumerate() {
            let arguments = if acc.arguments.trim().is_empty() {
                // Note: a stream truncated mid-arguments also lands here —
                // the accumulator cannot distinguish "the model sent no
                // arguments" from "bytes were lost".
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&acc.arguments).map_err(|e| RustyError::LlmFailure {
                    class: LlmErrorClass::Decode,
                    message: format!(
                        "malformed tool-call arguments in stream (index {index}): {e}"
                    ),
                })?
            };
            tool_calls.push(ToolCall::new(acc.id, acc.name, arguments));
        }
        let content = if self.content.is_empty() {
            None
        } else {
            Some(self.content)
        };
        Ok(ChatResponse {
            message: ChatMessage {
                role: Role::Assistant,
                content,
                tool_calls,
                tool_call_id: None,
                name: None,
            },
            model: self.model,
            usage: self.usage,
        })
    }
}

/// A minimal hand-rolled Server-Sent-Events decoder.
///
/// SSE is a line protocol over an arbitrarily chunked byte stream: events
/// are separated by blank lines, each event's `data:` lines (possibly
/// several) join with `\n` into one payload, `:`-prefixed lines are
/// comments/heartbeats, and other fields (`event:`, `id:`, `retry:`) are
/// ignored. [`SseDecoder::feed_bytes`] buffers partial lines *and* partial
/// UTF-8 sequences across calls, so a `data:` line — or a single multi-byte
/// character — split across TCP chunks still decodes correctly. One leading
/// BOM is stripped, and a bare `data` line (no colon) is an empty-string
/// field, both per the SSE spec.
#[derive(Default)]
struct SseDecoder {
    /// Raw bytes held back because they end mid-UTF-8-sequence.
    pending: Vec<u8>,
    /// Decoded text received but not yet terminated by `\n`.
    buf: String,
    /// `data:` lines of the event currently being assembled.
    data_lines: Vec<String>,
    /// Whether any bytes have been consumed yet (for one-time BOM stripping).
    started: bool,
}

impl SseDecoder {
    fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes as they arrive from the transport; returns the payloads
    /// of all events completed by them (usually zero or one).
    ///
    /// Only complete UTF-8 is decoded: a trailing partial sequence is held in
    /// `pending` for the next feed, so a chunk boundary can never corrupt
    /// text into U+FFFD. Genuinely invalid bytes (not fixable by more data)
    /// are replaced with U+FFFD.
    fn feed_bytes(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        if !self.started {
            self.started = true;
            // Spec: a single leading U+FEFF is ignored. (If the BOM itself is
            // split across feeds, the tail decodes to U+FEFF in the first
            // line, which the JSON parse of real providers never produces.)
            if self.pending.starts_with(&[0xEF, 0xBB, 0xBF]) {
                self.pending.drain(..3);
            }
        }
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(s) => {
                    text.push_str(s);
                    consumed = self.pending.len();
                }
                Err(e) => {
                    let end = consumed + e.valid_up_to();
                    text.push_str(
                        std::str::from_utf8(&self.pending[consumed..end])
                            .expect("bytes up to valid_up_to are valid UTF-8"),
                    );
                    match e.error_len() {
                        Some(bad) => {
                            text.push('\u{FFFD}');
                            consumed = end + bad;
                        }
                        // Incomplete trailing sequence: wait for more bytes.
                        None => {
                            consumed = end;
                            break;
                        }
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        self.feed(&text)
    }

    /// Feed a text fragment; returns the payloads of all events completed by
    /// it (usually zero or one).
    fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            if let Some(payload) = self.process_line(line.trim_end_matches(['\n', '\r'])) {
                events.push(payload);
            }
        }
        events
    }

    /// Flush any undecoded trailing bytes (lossily), any unterminated
    /// trailing line, and any event that ended without its blank-line
    /// terminator (end of stream).
    fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            // Bytes that never completed a UTF-8 sequence before EOF.
            let tail = String::from_utf8_lossy(&self.pending).into_owned();
            self.pending.clear();
            events.extend(self.feed(&tail));
        }
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(payload) = self.process_line(line.trim_end_matches('\r')) {
                events.push(payload);
            }
        }
        if !self.data_lines.is_empty() {
            events.push(self.data_lines.join("\n"));
            self.data_lines.clear();
        }
        events
    }

    /// Process one complete line; returns the event payload when a blank
    /// line terminates an event that carried `data:`.
    fn process_line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            if self.data_lines.is_empty() {
                return None;
            }
            let payload = self.data_lines.join("\n");
            self.data_lines.clear();
            return Some(payload);
        }
        if line.starts_with(':') {
            return None; // comment / heartbeat
        }
        if let Some(data) = line.strip_prefix("data:") {
            // Per spec, a single leading space after the colon is stripped.
            self.data_lines
                .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        } else if line == "data" {
            // Per spec, a field with no colon has the empty string as value.
            self.data_lines.push(String::new());
        }
        None
    }
}

/// Apply one decoded SSE `data:` payload to the accumulator, invoking
/// `on_token` for text deltas. Returns `Ok(true)` on the terminal `[DONE]`
/// sentinel.
///
/// Fail-fast policy: one malformed payload aborts the whole stream
/// (discarding accumulated deltas) rather than guessing which bytes to skip
/// — after a desynchronized JSON parse, event boundaries can no longer be
/// trusted. Empty payloads (bare `data:` keep-alives) are skipped.
fn handle_sse_payload(
    payload: &str,
    acc: &mut StreamAccumulator,
    on_token: &mut (dyn FnMut(TokenChunk) + Send),
) -> Result<bool> {
    let trimmed = payload.trim();
    if trimmed == "[DONE]" {
        return Ok(true);
    }
    if trimmed.is_empty() {
        return Ok(false);
    }

    let malformed = |e: serde_json::Error| RustyError::LlmFailure {
        class: LlmErrorClass::Decode,
        message: format!("malformed stream chunk: {e}"),
    };
    let value: Value = serde_json::from_str(trimmed).map_err(malformed)?;
    let chunk: WireStreamChunk = serde_json::from_value(value.clone()).map_err(malformed)?;

    if chunk.model.is_some() {
        acc.model = chunk.model;
    }
    if chunk.usage.is_some() {
        acc.usage = chunk.usage;
    }

    if let Some(choice) = chunk.choices.into_iter().next() {
        let delta = choice.delta;
        if let Some(content) = delta.content {
            if !content.is_empty() {
                acc.content.push_str(&content);
                on_token(TokenChunk {
                    delta: content,
                    finish: false,
                    raw: Some(value),
                });
            }
        }
        if let Some(calls) = delta.tool_calls {
            for call in calls {
                if acc.tool_calls.len() <= call.index {
                    acc.tool_calls
                        .resize_with(call.index + 1, ToolCallAccumulator::default);
                }
                let slot = &mut acc.tool_calls[call.index];
                if let Some(id) = call.id {
                    slot.id.push_str(&id);
                }
                if let Some(function) = call.function {
                    if let Some(name) = function.name {
                        slot.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        slot.arguments.push_str(&arguments);
                    }
                }
            }
        }
    }
    Ok(false)
}

#[async_trait]
impl ChatModel for OpenAiCompatibleClient {
    fn pricing(&self) -> Option<ModelPricing> {
        self.pricing
    }

    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }

        let response = self
            .send_with_retries(|| {
                let mut request = self.client.post(self.endpoint()).json(&body);
                if let Some(key) = &self.api_key {
                    request = request.bearer_auth(key);
                }
                request
            })
            .await?;

        let wire: WireResponse = response.json().await.map_err(|e| RustyError::LlmFailure {
            class: LlmErrorClass::Decode,
            message: format!("malformed chat completions response: {e}"),
        })?;

        let choice = wire
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| RustyError::LlmFailure {
                class: LlmErrorClass::Decode,
                message: "chat completions returned zero choices".into(),
            })?;

        Ok(ChatResponse {
            message: choice.message,
            model: wire.model,
            usage: wire.usage,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> Result<ChatResponse> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // Ask for a final usage chunk (supported by OpenAI, vLLM, ...);
            // providers that ignore it simply omit `usage`.
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }

        let mut response = self
            .send_with_retries(|| {
                let mut request = self.client.post(self.endpoint()).json(&body);
                if let Some(key) = &self.api_key {
                    request = request.bearer_auth(key);
                }
                request
            })
            .await?;

        // Read the body as raw bytes and decode SSE manually (`chunk()` is
        // used because the `stream` feature of reqwest is not enabled; the
        // SseDecoder is byte-chunk agnostic either way).
        let mut decoder = SseDecoder::new();
        let mut acc = StreamAccumulator::default();
        let mut done = false;

        while !done {
            let bytes = match response.chunk().await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break, // end of body
                Err(e) => {
                    return Err(RustyError::LlmFailure {
                        class: if e.is_timeout() {
                            LlmErrorClass::Timeout
                        } else {
                            LlmErrorClass::Unknown
                        },
                        message: format!("stream read from {} failed: {e}", self.base_url),
                    })
                }
            };
            for payload in decoder.feed_bytes(&bytes) {
                if handle_sse_payload(&payload, &mut acc, on_token)? {
                    done = true;
                    break;
                }
            }
        }
        if !done {
            // Stream ended without `[DONE]`: flush whatever the decoder holds.
            for payload in decoder.finish() {
                if handle_sse_payload(&payload, &mut acc, on_token)? {
                    break;
                }
            }
        }

        on_token(TokenChunk {
            delta: String::new(),
            finish: true,
            raw: None,
        });
        acc.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_wire_roundtrip() {
        let call = ToolCall::new("call_1", "search", json!({"q": "rust"}));
        let serialized = serde_json::to_value(&call).unwrap();
        assert_eq!(
            serialized,
            json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}
            })
        );
        let back: ToolCall = serde_json::from_value(serialized).unwrap();
        assert_eq!(back, call);
    }

    #[test]
    fn message_builders() {
        let m = ChatMessage::tool_result("call_1", "42");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.tool_call_id.as_deref(), Some("call_1"));

        let m = ChatMessage::assistant_tool_calls(vec![ToolCall::new("c", "t", json!({}))]);
        assert!(m.has_tool_calls());
        assert_eq!(m.content, None);

        // Roles serialize to OpenAI lowercase strings.
        assert_eq!(
            serde_json::to_value(Role::Assistant).unwrap(),
            json!("assistant")
        );
    }

    #[test]
    fn sse_decoder_handles_multi_chunk_delivery() {
        let mut decoder = SseDecoder::new();
        // One event split across two arbitrary byte chunks (split inside a
        // `data:` line), followed by a comment and a blank-line terminator;
        // then a CRLF-framed event.
        assert!(decoder.feed("data: {\"hel").is_empty());
        assert_eq!(
            decoder.feed("lo\"}\n: heartbeat\n\nda"),
            vec!["{\"hello\"}".to_string()]
        );
        assert_eq!(decoder.feed("ta: world\r\n\r\n"), vec!["world".to_string()]);
    }

    #[test]
    fn sse_decoder_joins_multi_line_data_and_flushes_trailing_event() {
        let mut decoder = SseDecoder::new();
        // Multiple `data:` lines in one event join with `\n` per the SSE spec.
        assert_eq!(
            decoder.feed("data: a\ndata: b\n\n"),
            vec!["a\nb".to_string()]
        );
        // A final event with no blank-line terminator is flushed by finish().
        assert!(decoder.feed("data: tail").is_empty());
        assert_eq!(decoder.finish(), vec!["tail".to_string()]);
        // A blank line with no pending `data:` is not an event.
        assert!(SseDecoder::new().process_line("").is_none());
    }

    #[test]
    fn sse_done_sentinel_terminates_and_content_deltas_accumulate() {
        let mut acc = StreamAccumulator::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut on_token = |chunk: TokenChunk| {
            assert!(!chunk.finish);
            assert!(chunk.raw.is_some(), "wire chunks carry the raw JSON");
            deltas.push(chunk.delta);
        };

        let done = handle_sse_payload(
            r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#,
            &mut acc,
            &mut on_token,
        )
        .unwrap();
        assert!(!done);
        let done = handle_sse_payload(
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}],
                "model":"gpt-x","usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
            &mut acc,
            &mut on_token,
        )
        .unwrap();
        assert!(!done);
        // The [DONE] sentinel terminates the stream and is not parsed as JSON.
        assert!(handle_sse_payload("[DONE]", &mut acc, &mut on_token).unwrap());

        assert_eq!(deltas, ["Hel", "lo"]);
        let response = acc.into_response().unwrap();
        assert_eq!(response.message.content.as_deref(), Some("Hello"));
        assert_eq!(response.model.as_deref(), Some("gpt-x"));
        assert_eq!(response.usage.unwrap().total_tokens, 3);
    }

    #[test]
    fn sse_stream_accumulates_tool_call_deltas() {
        let mut acc = StreamAccumulator::default();
        let mut on_token = |_chunk: TokenChunk| {};
        // id/name/arguments arrive piecewise across chunks at the same index.
        for payload in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"search","arguments":"{\"q\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust\"}"}}]}}]}"#,
        ] {
            assert!(!handle_sse_payload(payload, &mut acc, &mut on_token).unwrap());
        }
        let response = acc.into_response().unwrap();
        assert_eq!(
            response.message.tool_calls,
            vec![ToolCall::new("call_1", "search", json!({"q": "rust"}))]
        );
        assert_eq!(response.message.content, None);
    }

    /// A model that only implements `chat` (the pre-streaming API surface).
    struct NonStreamingMock;

    #[async_trait]
    impl ChatModel for NonStreamingMock {
        async fn chat(&self, _messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
            Ok(ChatResponse {
                message: ChatMessage::assistant("full answer"),
                model: Some("mock".into()),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn chat_stream_default_falls_back_to_single_chunk() {
        let model = NonStreamingMock;
        let mut chunks: Vec<TokenChunk> = Vec::new();
        let response = model
            .chat_stream(&[], &[], &mut |chunk| chunks.push(chunk))
            .await
            .unwrap();

        // Exactly one terminal chunk carrying the whole message.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].delta, "full answer");
        assert!(chunks[0].finish);
        assert!(chunks[0].raw.is_none());
        assert_eq!(
            response.message.content.as_deref(),
            Some("full answer"),
            "the fallback returns the chat() response unchanged"
        );
    }

    #[test]
    fn feed_bytes_preserves_multibyte_chars_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        // "你" is E4 BD A0 in UTF-8; split the sequence across two feeds.
        let mut first = b"data: {\"c\":\"".to_vec();
        first.push(0xE4);
        assert!(decoder.feed_bytes(&first).is_empty());
        assert!(decoder.feed_bytes(&[0xBD, 0xA0]).is_empty());
        let events = decoder.feed_bytes(b"\"}\n\n");
        assert_eq!(events, vec!["{\"c\":\"你\"}".to_string()]);
    }

    #[test]
    fn sse_strips_leading_bom_and_handles_bare_data_line() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed_bytes(&[0xEF, 0xBB, 0xBF]).is_empty());
        // A bare `data` line (no colon) is an empty-string field per spec.
        assert_eq!(decoder.feed("data\n\n"), vec![String::new()]);
        assert_eq!(decoder.feed("data: x\n\n"), vec!["x".to_string()]);
    }

    #[test]
    fn empty_payload_is_skipped_not_parsed() {
        let mut acc = StreamAccumulator::default();
        let mut on_token = |_chunk: TokenChunk| {};
        // Bare `data:` keep-alive events decode to empty payloads and must
        // not abort the stream as "malformed JSON".
        assert!(!handle_sse_payload("", &mut acc, &mut on_token).unwrap());
    }

    #[test]
    fn client_debug_redacts_the_api_key() {
        let client =
            OpenAiCompatibleClient::new("http://localhost", Some("sk-secret-123".into()), "m");
        let debug = format!("{client:?}");
        assert!(!debug.contains("sk-secret-123"), "got: {debug}");
        assert!(debug.contains("***"), "got: {debug}");

        let no_key = OpenAiCompatibleClient::new("http://localhost", None, "m");
        assert!(format!("{no_key:?}").contains("api_key: None"));
    }

    // ---------- retry policy over a hand-rolled mock HTTP server ----------

    /// Start a minimal HTTP/1.1 server; `handler` maps the 1-based attempt
    /// number to a (status, body) response.
    fn start_http_mock(
        handler: impl Fn(usize) -> (u16, String) + Send + Sync + 'static,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = StdArc::new(AtomicUsize::new(0));
        let attempts2 = attempts.clone();
        let handler = StdArc::new(handler);
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let attempts = attempts2.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    // Read the request: headers up to \r\n\r\n, then the
                    // content-length bytes of body (contents irrelevant).
                    let mut buf: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            let len: usize = headers
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                            if buf.len() >= pos + 4 + len {
                                break;
                            }
                        }
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    let (status, body) = handler(n);
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        _ => "Status",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\n\
                         content-type: application/json\r\n\
                         content-length: {}\r\n\
                         connection: close\r\n\
                         \r\n\
                         {body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (addr, attempts)
    }

    #[tokio::test]
    async fn chat_retries_transient_5xx_then_succeeds() {
        let (addr, attempts) = start_http_mock(|n| {
            if n < 3 {
                (500, r#"{"error":"overloaded"}"#.to_string())
            } else {
                (
                    200,
                    r#"{"choices":[{"message":{"role":"assistant","content":"hi after retry"}}]}"#
                        .to_string(),
                )
            }
        });
        let client = OpenAiCompatibleClient::new(format!("http://{addr}"), None, "m")
            .with_backoff(Duration::from_millis(1));
        let response = client.chat(&[ChatMessage::user("hi")], &[]).await.unwrap();
        assert_eq!(response.message.content.as_deref(), Some("hi after retry"));
        // 1 initial + 2 retries = 3 attempts.
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn chat_does_not_retry_fatal_4xx() {
        let (addr, attempts) =
            start_http_mock(|_n| (400, r#"{"error":"bad request"}"#.to_string()));
        let client = OpenAiCompatibleClient::new(format!("http://{addr}"), None, "m")
            .with_backoff(Duration::from_millis(1));
        let err = client
            .chat(&[ChatMessage::user("hi")], &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"), "got: {err}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn chat_gives_up_after_retries_exhausted() {
        let (addr, attempts) =
            start_http_mock(|_n| (429, r#"{"error":"rate limited"}"#.to_string()));
        let client = OpenAiCompatibleClient::new(format!("http://{addr}"), None, "m")
            .with_retries(1)
            .with_backoff(Duration::from_millis(1));
        let err = client
            .chat(&[ChatMessage::user("hi")], &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("429"), "got: {err}");
        // 1 initial + 1 retry = 2 attempts.
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    // ---------- usage detail, pricing, classified errors (provider layer) ----------

    #[test]
    fn usage_detail_fields_are_absent_when_unset() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        // The pinned shape is exactly the three headline fields — nothing
        // new appears on the wire until a provider reports it.
        let value = serde_json::to_value(usage).unwrap();
        assert_eq!(
            value,
            json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15})
        );
        // Payloads written before the detail fields existed still decode.
        let back: Usage = serde_json::from_value(value).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn usage_detail_fields_round_trip_when_set() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: Some(4),
            reasoning_tokens: Some(2),
        };
        let value = serde_json::to_value(usage).unwrap();
        assert_eq!(
            value,
            json!({
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "cached_tokens": 4,
                "reasoning_tokens": 2,
            })
        );
        let back: Usage = serde_json::from_value(value).unwrap();
        assert_eq!(back, usage);
    }

    fn assert_cost(pricing: ModelPricing, usage: Usage, expected: f64) {
        let cost = pricing.cost_usd(&usage);
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected ${expected}, got ${cost}"
        );
    }

    #[test]
    fn pricing_cost_math_charges_input_and_output() {
        let pricing = ModelPricing::new(2.0, 8.0);
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            total_tokens: 1_500_000,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        assert_cost(pricing, usage, 2.0 + 4.0);
        // Zero usage costs zero.
        assert_cost(pricing, Usage::default(), 0.0);
    }

    #[test]
    fn cached_tokens_bill_at_the_cached_rate_never_twice() {
        let pricing = ModelPricing::new(2.0, 8.0).with_cached_input(0.5);
        // 1M prompt tokens, 400k of them cache-served: the cached subset
        // leaves the input-rate pool instead of being charged twice.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: Some(400_000),
            reasoning_tokens: None,
        };
        assert_cost(
            pricing,
            usage,
            600_000.0 * 2.0 / 1e6 + 400_000.0 * 0.5 / 1e6,
        );
        // Without a cached rate the subset bills at the full input rate —
        // still exactly once.
        let flat = ModelPricing::new(2.0, 8.0);
        assert_cost(flat, usage, 2.0);
        // A provider reporting more cached tokens than prompt tokens is
        // clamped, not trusted.
        let absurd = Usage {
            cached_tokens: Some(1_000_000),
            ..usage
        };
        assert_cost(pricing, absurd, 1_000_000.0 * 0.5 / 1e6);
    }

    #[test]
    fn client_pricing_is_operator_supplied() {
        let client = OpenAiCompatibleClient::new("http://localhost", None, "m");
        assert_eq!(client.pricing(), None);
        let pricing = ModelPricing::new(2.0, 8.0).with_cached_input(0.5);
        let client = client.with_pricing(pricing);
        assert_eq!(client.pricing(), Some(pricing));
    }

    #[tokio::test]
    async fn http_failures_carry_a_retry_relevant_class() {
        for (status, class) in [
            (429u16, LlmErrorClass::RateLimited),
            (500, LlmErrorClass::Server),
            (503, LlmErrorClass::Server),
            (401, LlmErrorClass::Auth),
            (403, LlmErrorClass::Auth),
            (400, LlmErrorClass::InvalidRequest),
            (422, LlmErrorClass::InvalidRequest),
        ] {
            let (addr, _attempts) =
                start_http_mock(move |_n| (status, r#"{"error":"x"}"#.to_string()));
            let client =
                OpenAiCompatibleClient::new(format!("http://{addr}"), None, "m").with_retries(0);
            let err = client
                .chat(&[ChatMessage::user("hi")], &[])
                .await
                .unwrap_err();
            // The helper accessor agrees with the variant's payload.
            assert_eq!(err.llm_class(), class, "status {status}");
            match err {
                RustyError::LlmFailure {
                    class: got,
                    message,
                } => {
                    assert_eq!(got, class, "status {status}");
                    assert!(message.contains(&status.to_string()), "got: {message}");
                }
                other => panic!("status {status}: expected LlmFailure, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn undecodable_response_classifies_as_decode() {
        let (addr, _attempts) = start_http_mock(|_n| (200, "not json".to_string()));
        let client =
            OpenAiCompatibleClient::new(format!("http://{addr}"), None, "m").with_retries(0);
        let err = client
            .chat(&[ChatMessage::user("hi")], &[])
            .await
            .unwrap_err();
        assert_eq!(err.llm_class(), LlmErrorClass::Decode);
        assert!(err.to_string().contains("malformed"), "got: {err}");
    }

    #[test]
    fn malformed_stream_chunk_classifies_as_decode() {
        let mut acc = StreamAccumulator::default();
        let mut on_token = |_chunk: TokenChunk| {};
        let err = handle_sse_payload("{not json", &mut acc, &mut on_token).unwrap_err();
        assert_eq!(err.llm_class(), LlmErrorClass::Decode);
    }
}
