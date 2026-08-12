//! Multi-provider model access through the `genai` crate (feature `genai`).
//!
//! # Why genai, and why a feature
//!
//! [`OpenAiCompatibleClient`](crate::llm::OpenAiCompatibleClient) speaks one
//! wire protocol; Anthropic, Gemini, Ollama, and twenty more each differ in
//! roles, tool-call shapes, usage fields, and streaming grammar — more
//! adapters than this crate can hand-maintain. The `genai` crate
//! (`jeremychone/rust-genai`, MIT OR Apache-2.0) normalizes those protocols
//! behind one client, and this module maps its provider-neutral message
//! model onto ours. The full rationale — including why Rig was passed over —
//! is `docs/provider-layer-design.md`. The integration is an optional
//! feature so the zero-extra-dependency default stays put and the adapter
//! can be swapped (e.g. for `rig-core`) without touching the core crate.
//!
//! # What the translation boundary guarantees
//!
//! The evidence formats never leave our vocabulary: `ChatMessage`,
//! [`ToolCall`], and [`Usage`] are journaled verbatim and pinned by golden
//! files, so genai types exist only inside this module. Concretely:
//!
//! - **Messages.** Our system messages are hoisted into genai's request-level
//!   `system` field (genai's own convention; per-provider adapters place it
//!   correctly, e.g. Anthropic's top-level `system`). A `role: tool` message
//!   becomes genai's tool-result content with `tool_call_id` carried into
//!   `call_id`, and assistant `tool_calls` become genai tool-call parts with
//!   `id` ↔ `call_id` preserved exactly — exact replay's request hash and
//!   tool-result pairing depend on it. Two fields have no home in our
//!   vocabulary and are dropped at the boundary: the OpenAI participant
//!   `name`, and genai's reasoning/thought-signature parts (round-tripping
//!   those is a `ChatMessage` change with serde consequences — a post-1.0
//!   design of its own, per the design doc).
//! - **Tool schemas.** Schemas arrive in the OpenAI envelope
//!   (`{"type": "function", "function": {...}}`) and are unwrapped into
//!   genai's normalized `Tool` with the `parameters` value passed through
//!   unmodified, in the order received — never reordered, renamed, or
//!   re-serialized. The ReAct node sorts schemas by name to keep the replay
//!   request hash process-stable (`crate::replay`); this adapter must not
//!   disturb that order.
//! - **Usage.** Provider detail fields land in `Usage.cached_tokens` /
//!   `Usage.reasoning_tokens` whenever genai reports them.
//! - **Errors.** genai's error enum is classified onto [`LlmErrorClass`] so
//!   retry policy survives the boundary (429 → rate-limited, connect/timeout
//!   → timeout, 5xx → server, 401/403 → auth, other 4xx → invalid-request,
//!   decode failures → decode).
//!
//! All translation is pure functions (`messages_to_genai`,
//! `tool_schema_to_genai`, `response_from_genai`, `usage_from_genai`,
//! [`GenaiStreamFold`]) separate from the async client calls, so the whole
//! boundary is testable without network access.
//!
//! # TLS: pure-Rust ring, no C toolchain
//!
//! genai is taken without its default `rustls-tls` feature: in its reqwest
//! 0.13 line that feature selects the aws-lc-rs backend, a C/assembly build
//! (`aws-lc-sys`). The rest of this crate's HTTP stack already rides reqwest
//! 0.12's ring-based rustls, so the feature instead enables reqwest 0.13's
//! `rustls-no-provider` (genai documents the no-TLS-feature configuration as
//! its supported bring-your-own-TLS path) and installs rustls's ring crypto
//! provider as the process default before any genai client is built
//! ([`GenaiChatModel::new`]). Certificate verification uses the platform
//! trust store via `rustls-platform-verifier`, same as reqwest 0.13's
//! default rustls configuration.

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use genai::chat::{
    ChatMessage as GenaiChatMessage, ChatRequest as GenaiChatRequest,
    ChatResponse as GenaiChatResponse, ChatStreamEvent, ContentPart, MessageContent,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse as GenaiToolResponse,
    Usage as GenaiUsage,
};
use serde_json::Value;

use crate::error::{LlmErrorClass, Result, RustyError};
use crate::llm::{
    ChatMessage, ChatModel, ChatResponse, ModelPricing, Role, TokenChunk, ToolCall, Usage,
};
use crate::record::Effect;

/// A [`ChatModel`] over a `genai::Client`: one adapter, every provider genai
/// speaks natively (OpenAI, Anthropic, Gemini, Ollama, Groq, DeepSeek, ...).
///
/// The model string selects the provider through genai's inference/prefix
/// routing (`gpt-…` → OpenAI, `claude-…` → Anthropic, `gemini-…` → Gemini,
/// `ollama::…` → a local Ollama, ...). API keys resolve from the provider's
/// conventional environment variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
/// `GEMINI_API_KEY`, ...).
#[derive(Debug, Clone)]
pub struct GenaiChatModel {
    client: genai::Client,
    model: String,
    /// Operator-supplied pricing; `None` means cost is not computed.
    pricing: Option<ModelPricing>,
}

impl GenaiChatModel {
    /// A model over genai's default client (env-key resolution, default
    /// endpoints).
    ///
    /// Installs rustls's ring crypto provider as the process default first —
    /// the feature builds reqwest 0.13 without a bundled provider (see the
    /// module docs), and a reqwest client built with no provider installed
    /// panics. If the application already installed a provider, that choice
    /// stands.
    pub fn new(model: impl Into<String>) -> Self {
        install_ring_provider();
        Self::from_client(genai::Client::default(), model)
    }

    /// A model over a caller-configured `genai::Client` (custom endpoints,
    /// auth resolvers, chat options).
    ///
    /// Callers building their own client take over the TLS-provider
    /// responsibility described in [`GenaiChatModel::new`]; the install here
    /// is repeated defensively and is a no-op once a provider is set.
    pub fn from_client(client: genai::Client, model: impl Into<String>) -> Self {
        install_ring_provider();
        Self {
            client,
            model: model.into(),
            pricing: None,
        }
    }

    /// Attach per-token pricing so journaled model calls carry `cost_usd`.
    /// Rates are operator configuration — the crate ships no price list.
    pub fn with_pricing(mut self, pricing: ModelPricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// The model string this client requests (provider-selecting, per
    /// genai's routing).
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Install rustls's ring provider as the process default, unless the
/// application (or a prior call) already installed one. `install_default`
/// reports an error in the "already set" case; that outcome is success for
/// our purposes — reqwest will find *a* provider, which is all the
/// no-provider build requires.
fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[async_trait]
impl ChatModel for GenaiChatModel {
    fn pricing(&self) -> Option<ModelPricing> {
        self.pricing
    }

    fn effect(&self) -> Effect {
        // The safe default: a provider call is billable and its outcome
        // unverifiable, so nothing about it is safely retryable at the trait
        // level. Transient-failure retries belong to the caller's policy,
        // keyed off the classified error, not to effect admission.
        Effect::NonIdempotent
    }

    async fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatResponse> {
        let request = chat_request_to_genai(messages, tools)?;
        let response = self
            .client
            .exec_chat(self.model.as_str(), request, None)
            .await
            .map_err(|e| map_genai_error("chat request", e))?;
        Ok(response_from_genai(response))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> Result<ChatResponse> {
        let request = chat_request_to_genai(messages, tools)?;
        // Ask genai to capture terminal usage into the End event; providers
        // that never report usage simply leave it `None`.
        let options = genai::chat::ChatOptions::default().with_capture_usage(true);
        let response = self
            .client
            .exec_chat_stream(self.model.as_str(), request, Some(&options))
            .await
            .map_err(|e| map_genai_error("streaming chat request", e))?;
        let model = response.model_iden.model_name.to_string();
        drive_genai_stream(response.stream, Some(model), on_token).await
    }
}

// ---------------------------------------------------------------------------
// Translation: ours → genai (pure; no client state involved)
// ---------------------------------------------------------------------------

/// Build genai's chat request from our messages and tool schemas.
///
/// System messages are hoisted into the request-level `system` field
/// (several are joined with a blank line, preserving order); every other
/// message keeps its position. Tool schemas pass through in the order given.
pub fn chat_request_to_genai(
    messages: &[ChatMessage],
    tools: &[Value],
) -> Result<GenaiChatRequest> {
    let (system, messages) = messages_to_genai(messages)?;
    let mut request = GenaiChatRequest::from_messages(messages);
    if let Some(system) = system {
        request = request.with_system(system);
    }
    if !tools.is_empty() {
        let genai_tools = tools
            .iter()
            .map(tool_schema_to_genai)
            .collect::<Result<Vec<_>>>()?;
        request = request.with_tools(genai_tools);
    }
    Ok(request)
}

/// Translate our message list into genai's, returning the hoisted system
/// text separately.
///
/// Assistant tool calls become genai tool-call parts with `id` carried into
/// `call_id`; `role: tool` messages become genai tool-result content keyed
/// by the same id — the pairing exact replay depends on. A tool message
/// without `tool_call_id` is rejected as `InvalidRequest`: forwarding an
/// empty correlation id would fail later, farther from the cause, inside the
/// provider.
pub fn messages_to_genai(
    messages: &[ChatMessage],
) -> Result<(Option<String>, Vec<GenaiChatMessage>)> {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role {
            Role::System => {
                if let Some(content) = &message.content {
                    system_parts.push(content);
                }
            }
            Role::User => {
                out.push(GenaiChatMessage::user(
                    message.content.clone().unwrap_or_default(),
                ));
            }
            Role::Assistant => {
                let mut parts: Vec<ContentPart> = Vec::new();
                if let Some(content) = &message.content {
                    if !content.is_empty() {
                        parts.push(ContentPart::Text(content.clone()));
                    }
                }
                parts.extend(message.tool_calls.iter().map(|call| {
                    ContentPart::ToolCall(GenaiToolCall {
                        call_id: call.id.clone(),
                        fn_name: call.name.clone(),
                        fn_arguments: call.arguments.clone(),
                        // Our vocabulary has no home for provider thought
                        // signatures; see the module docs.
                        thought_signatures: None,
                    })
                }));
                out.push(GenaiChatMessage::assistant(MessageContent::from_parts(
                    parts,
                )));
            }
            Role::Tool => {
                let call_id =
                    message
                        .tool_call_id
                        .as_deref()
                        .ok_or_else(|| RustyError::LlmFailure {
                            class: LlmErrorClass::InvalidRequest,
                            message: "role: tool message without tool_call_id cannot be \
                                  translated for genai"
                                .into(),
                        })?;
                out.push(GenaiChatMessage::tool(GenaiToolResponse::new(
                    call_id,
                    message.content.clone().unwrap_or_default(),
                )));
            }
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    Ok((system, out))
}

/// Unwrap one OpenAI-envelope tool schema (`{"type": "function",
/// "function": {"name", "description", "parameters", ...}}`) into genai's
/// normalized `Tool`.
///
/// The `parameters` value is moved across unmodified, and callers iterate
/// schemas in the order received: exact replay hashes the tool list in our
/// serde form with schemas sorted by name upstream (`crate::replay`), so
/// this boundary must never reorder, rename, or re-serialize them. Anything
/// that is not a well-formed function envelope is the request's fault —
/// `InvalidRequest`.
pub fn tool_schema_to_genai(schema: &Value) -> Result<GenaiTool> {
    let invalid = |detail: &str| RustyError::LlmFailure {
        class: LlmErrorClass::InvalidRequest,
        message: format!("tool schema is not an OpenAI function envelope: {detail}"),
    };
    let function = schema
        .get("function")
        .ok_or_else(|| invalid("missing \"function\" object"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing \"function.name\" string"))?;
    let mut tool = GenaiTool::new(name);
    if let Some(description) = function.get("description").and_then(Value::as_str) {
        tool = tool.with_description(description);
    }
    if let Some(parameters) = function.get("parameters") {
        tool = tool.with_schema(parameters.clone());
    }
    if let Some(strict) = function.get("strict").and_then(Value::as_bool) {
        tool = tool.with_strict(strict);
    }
    Ok(tool)
}

// ---------------------------------------------------------------------------
// Translation: genai → ours (pure)
// ---------------------------------------------------------------------------

/// Translate a non-streaming genai response into our [`ChatResponse`].
///
/// Text parts concatenate into `ChatMessage.content` (the same accumulation
/// the streaming path produces); tool-call parts become our [`ToolCall`]s
/// with `call_id` carried back into `id`; the provider-reported model name
/// is preferred over the requested one; usage maps through
/// [`usage_from_genai`].
pub fn response_from_genai(response: GenaiChatResponse) -> ChatResponse {
    let model = response.provider_model_iden.model_name.to_string();
    let usage = usage_from_genai(&response.usage);
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for part in response.content.into_parts() {
        match part {
            ContentPart::Text(text) => content.push_str(&text),
            ContentPart::ToolCall(call) => tool_calls.push(tool_call_from_genai(call)),
            // Reasoning, thought signatures, binary and custom parts have no
            // home in our vocabulary; see the module docs.
            _ => {}
        }
    }
    ChatResponse {
        message: assistant_message(content, tool_calls),
        model: Some(model),
        usage,
    }
}

/// Map genai's normalized usage into ours, detail fields included.
///
/// Returns `None` when genai reports nothing at all, so an unreported usage
/// stays distinguishable from a zero one. Counts arrive as `Option<i32>`;
/// negatives (a provider bug, not a signal) clamp to zero, and a missing
/// total falls back to prompt + completion.
pub fn usage_from_genai(usage: &GenaiUsage) -> Option<Usage> {
    let clamp =
        |value: Option<i32>| -> u64 { value.and_then(|v| u64::try_from(v).ok()).unwrap_or(0) };
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .and_then(|v| u64::try_from(v).ok());
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .and_then(|v| u64::try_from(v).ok());
    if usage.prompt_tokens.is_none()
        && usage.completion_tokens.is_none()
        && usage.total_tokens.is_none()
        && cached.is_none()
        && reasoning.is_none()
    {
        return None;
    }
    let prompt_tokens = clamp(usage.prompt_tokens);
    let completion_tokens = clamp(usage.completion_tokens);
    let total_tokens = usage
        .total_tokens
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(prompt_tokens + completion_tokens);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens: cached,
        reasoning_tokens: reasoning,
    })
}

/// One genai tool call into ours: `call_id` ↔ `id`, name, and the arguments
/// payload (already a parsed `Value` on genai's side).
fn tool_call_from_genai(call: GenaiToolCall) -> ToolCall {
    ToolCall::new(call.call_id, call.fn_name, call.fn_arguments)
}

/// Assemble the accumulated assistant message: empty text is `None` (an
/// assistant message that only carries tool calls), matching the OpenAI
/// client's convention.
fn assistant_message(content: String, tool_calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage {
        role: Role::Assistant,
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        tool_calls,
        tool_call_id: None,
        name: None,
    }
}

// ---------------------------------------------------------------------------
// Error classification (pure)
// ---------------------------------------------------------------------------

/// Classify a genai error onto the retry-relevant taxonomy.
///
/// HTTP statuses map exactly as the OpenAI-compatible client classifies
/// them (429 → rate-limited, 408 and 5xx → server, 401/403 → auth, other
/// 4xx → invalid-request); transport failures that never produced a
/// response are timeouts; auth-resolution failures are `Auth` before any
/// bytes fly; request-shape and serde failures are `InvalidRequest` /
/// `Decode`. Everything else is honestly `Unknown` rather than guessed.
pub fn classify_genai_error(error: &genai::Error) -> LlmErrorClass {
    use genai::Error as E;
    match error {
        E::HttpError { status, .. } => classify_status_code(status.as_u16()),
        E::WebAdapterCall { webc_error, .. } | E::WebModelCall { webc_error, .. } => {
            classify_webc_error(webc_error)
        }
        E::StreamParse { .. } | E::SerdeJson(_) | E::JsonValueExt(_) => LlmErrorClass::Decode,
        E::RequiresApiKey { .. } | E::NoAuthResolver { .. } | E::NoAuthData { .. } => {
            LlmErrorClass::Auth
        }
        E::ChatReqHasNoMessages { .. }
        | E::LastChatMessageIsNotUser { .. }
        | E::MessageRoleNotSupported { .. }
        | E::MessageContentTypeNotSupported { .. }
        | E::JsonModeWithoutInstruction
        | E::AdapterKindMismatch { .. }
        | E::AdapterNotSupported { .. } => LlmErrorClass::InvalidRequest,
        _ => LlmErrorClass::Unknown,
    }
}

/// Classify genai's transport-layer error (the failure modes of its thin
/// reqwest wrapper).
fn classify_webc_error(error: &genai::webc::Error) -> LlmErrorClass {
    use genai::webc::Error as E;
    match error {
        E::ResponseFailedStatus { status, .. } => classify_status_code(status.as_u16()),
        // Connect failures and timeouts never produced a response; anything
        // else at this layer is treated as definitive.
        E::Reqwest(e) => {
            if e.is_timeout() || e.is_connect() {
                LlmErrorClass::Timeout
            } else {
                LlmErrorClass::Unknown
            }
        }
        E::ResponseFailedNotJson { .. }
        | E::ResponseFailedInvalidJson { .. }
        | E::JsonValueExt(_) => LlmErrorClass::Decode,
    }
}

/// The HTTP-status arm of the taxonomy, shared by genai's two error
/// surfaces (`HttpError` and the transport layer's `ResponseFailedStatus`).
fn classify_status_code(status: u16) -> LlmErrorClass {
    match status {
        429 => LlmErrorClass::RateLimited,
        408 | 500..=599 => LlmErrorClass::Server,
        401 | 403 => LlmErrorClass::Auth,
        400..=499 => LlmErrorClass::InvalidRequest,
        _ => LlmErrorClass::Unknown,
    }
}

/// Wrap a genai failure in our classified LLM error, keeping genai's
/// display text (status and truncated body) as the detail.
fn map_genai_error(context: &str, error: genai::Error) -> RustyError {
    let class = classify_genai_error(&error);
    RustyError::LlmFailure {
        class,
        message: format!("genai {context} failed: {error}"),
    }
}

// ---------------------------------------------------------------------------
// Stream folding (pure) and driving (async, still network-free)
// ---------------------------------------------------------------------------

/// Accumulates genai's normalized stream events into a final
/// [`ChatResponse`].
///
/// Text deltas fire the token callback; tool calls arrive from genai
/// already assembled and accumulate silently; reasoning and
/// thought-signature deltas are dropped (no home in our vocabulary — the
/// module docs explain why); terminal usage is captured from the `End`
/// event. Mirrors the OpenAI-compatible client's streaming contract.
#[derive(Default)]
pub struct GenaiStreamFold {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
}

impl GenaiStreamFold {
    /// Fold one stream event, firing `on_token` for text deltas only.
    pub fn apply(
        &mut self,
        event: &ChatStreamEvent,
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) {
        match event {
            ChatStreamEvent::Start => {}
            ChatStreamEvent::Chunk(chunk) => {
                if !chunk.content.is_empty() {
                    self.content.push_str(&chunk.content);
                    on_token(TokenChunk {
                        delta: chunk.content.clone(),
                        finish: false,
                        // genai normalizes provider wire chunks away, so
                        // there is no raw payload to carry.
                        raw: None,
                    });
                }
            }
            ChatStreamEvent::ReasoningChunk(_) | ChatStreamEvent::ThoughtSignatureChunk(_) => {}
            ChatStreamEvent::ToolCallChunk(chunk) => {
                self.tool_calls
                    .push(tool_call_from_genai(chunk.tool_call.clone()));
            }
            ChatStreamEvent::End(end) => {
                if let Some(usage) = &end.captured_usage {
                    self.usage = usage_from_genai(usage);
                }
            }
        }
    }

    /// The accumulated response (usage `None` when the stream never
    /// reported any).
    pub fn into_response(self) -> ChatResponse {
        ChatResponse {
            message: assistant_message(self.content, self.tool_calls),
            model: None,
            usage: self.usage,
        }
    }
}

/// Drive a genai event stream to completion, fold it, and emit exactly one
/// terminal `finish: true` chunk — the [`ChatModel::chat_stream`] contract.
///
/// A stream that ends without an `End` event still terminates cleanly with
/// whatever accumulated (usage `None`), mirroring the OpenAI client's
/// end-of-body fallback. A mid-stream error aborts with its classified
/// failure, discarding partial output rather than returning it as complete.
///
/// Public so the folding contract is exercisable from integration tests
/// over scripted event streams, with no network involved.
pub async fn drive_genai_stream<S>(
    mut stream: S,
    model: Option<String>,
    on_token: &mut (dyn FnMut(TokenChunk) + Send),
) -> Result<ChatResponse>
where
    S: Stream<Item = genai::Result<ChatStreamEvent>> + Unpin,
{
    let mut fold = GenaiStreamFold::default();
    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| map_genai_error("stream", e))?;
        fold.apply(&event, on_token);
    }
    on_token(TokenChunk {
        delta: String::new(),
        finish: true,
        raw: None,
    });
    let mut response = fold.into_response();
    response.model = model;
    Ok(response)
}
