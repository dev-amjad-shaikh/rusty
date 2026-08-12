//! Translation-boundary tests for the genai adapter (feature `genai`).
//!
//! No network: every test exercises the adapter's pure translation layer —
//! message/request mapping, tool-schema pass-through, response and usage
//! mapping, error classification, and stream folding over scripted event
//! streams. genai-side fixtures are built either from genai's constructors
//! or by deserializing realistic provider-shaped JSON into genai's types.

#![cfg(feature = "genai")]

use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage as GenaiChatMessage, ChatRequest as GenaiChatRequest,
    ChatResponse as GenaiChatResponse, ChatRole, ChatStreamEvent, ToolCall as GenaiToolCall,
};
use genai::{webc, ModelIden};
use reqwest_tls::header::HeaderMap;
use reqwest_tls::StatusCode;
use rusty_agent_runtime::error::{LlmErrorClass, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel as _, ModelPricing, Role, ToolCall, Usage};
use rusty_agent_runtime::provider_genai::{
    chat_request_to_genai, classify_genai_error, drive_genai_stream, messages_to_genai,
    response_from_genai, tool_schema_to_genai, usage_from_genai, GenaiChatModel, GenaiStreamFold,
};
use rusty_agent_runtime::record::Effect;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn model_iden(name: &str) -> ModelIden {
    ModelIden::new(AdapterKind::OpenAI, name)
}

fn openai_tool_schema(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

/// A genai chat response decoded from a realistic normalized payload, the
/// way genai's adapters produce it. Parsed from text (not a `Value`) because
/// genai's `ModelName` deserializer borrows from the input.
fn genai_response(payload: &str) -> GenaiChatResponse {
    serde_json::from_str(payload).expect("fixture must decode as genai ChatResponse")
}

/// A genai stream event decoded from its serialized form.
fn stream_event(payload: Value) -> ChatStreamEvent {
    serde_json::from_value(payload).expect("fixture must decode as genai ChatStreamEvent")
}

// ---------------------------------------------------------------------------
// Message translation: system hoisting, roles, tool-call pairing
// ---------------------------------------------------------------------------

#[test]
fn system_messages_hoist_into_the_request_system_field() {
    let messages = vec![
        ChatMessage::system("You are precise."),
        ChatMessage::user("hi"),
        ChatMessage::system("Answer in French."),
    ];
    let (system, genai_messages) = messages_to_genai(&messages).unwrap();
    // Several system messages join in order with a blank line; the remaining
    // messages keep their positions.
    assert_eq!(
        system.as_deref(),
        Some("You are precise.\n\nAnswer in French.")
    );
    assert_eq!(genai_messages.len(), 1);
    assert_eq!(genai_messages[0].role, ChatRole::User);
    assert_eq!(genai_messages[0].content.first_text(), Some("hi"));

    // No system message at all means no system field, not an empty one.
    let (system, _) = messages_to_genai(&[ChatMessage::user("hi")]).unwrap();
    assert_eq!(system, None);
}

#[test]
fn assistant_tool_calls_carry_id_name_and_arguments() {
    let messages = vec![ChatMessage::assistant_tool_calls(vec![
        ToolCall::new("call_1", "search", json!({"q": "rust"})),
        ToolCall::new("call_2", "calc", json!({"a": 1, "b": 2})),
    ])];
    let (system, genai_messages) = messages_to_genai(&messages).unwrap();
    assert_eq!(system, None);
    assert_eq!(genai_messages.len(), 1);
    let message = &genai_messages[0];
    assert_eq!(message.role, ChatRole::Assistant);
    let calls = message.content.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call_id, "call_1");
    assert_eq!(calls[0].fn_name, "search");
    assert_eq!(calls[0].fn_arguments, json!({"q": "rust"}));
    assert_eq!(calls[1].call_id, "call_2");

    // Text alongside tool calls survives as a leading text part.
    let mut with_text = ChatMessage::assistant_tool_calls(vec![ToolCall::new("c", "t", json!({}))]);
    with_text.content = Some("let me check".into());
    let (_, genai_messages) = messages_to_genai(&[with_text]).unwrap();
    let content = &genai_messages[0].content;
    assert_eq!(content.first_text(), Some("let me check"));
    assert_eq!(content.tool_calls().len(), 1);
}

#[test]
fn tool_results_map_to_tool_response_parts_keyed_by_call_id() {
    let messages = vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("call_9", "search", json!({}))]),
        ChatMessage::tool_result("call_9", "42 results"),
    ];
    let (_, genai_messages) = messages_to_genai(&messages).unwrap();
    let tool_message = &genai_messages[1];
    assert_eq!(tool_message.role, ChatRole::Tool);
    let responses = tool_message.content.tool_responses();
    assert_eq!(responses.len(), 1);
    // The id pairing exact replay matches on survives the boundary verbatim.
    assert_eq!(responses[0].call_id, "call_9");
    assert_eq!(responses[0].content, "42 results");
}

#[test]
fn tool_message_without_call_id_is_an_invalid_request() {
    let orphan = ChatMessage {
        role: Role::Tool,
        content: Some("result".into()),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
    };
    let err = messages_to_genai(&[orphan]).unwrap_err();
    assert_eq!(err.llm_class(), LlmErrorClass::InvalidRequest);
}

#[test]
fn full_conversation_round_trips_with_order_and_pairing_intact() {
    let messages = vec![
        ChatMessage::system("Be terse."),
        ChatMessage::user("search for rust"),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "call_1",
            "search",
            json!({"q": "rust"}),
        )]),
        ChatMessage::tool_result("call_1", "rust: a language"),
        ChatMessage::user("thanks"),
    ];
    let request: GenaiChatRequest = chat_request_to_genai(&messages, &[]).unwrap();
    assert_eq!(request.system.as_deref(), Some("Be terse."));
    let roles: Vec<ChatRole> = request.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::User
        ]
    );
    // The assistant's call id and the tool result's call id are the same string.
    let call_id = &request.messages[1].content.tool_calls()[0].call_id;
    let response_id = &request.messages[2].content.tool_responses()[0].call_id;
    assert_eq!(call_id, response_id);
}

#[test]
fn translation_is_deterministic_across_runs() {
    // Exact replay hashes the request in *our* serde form upstream; the
    // adapter must be a deterministic function of that input, or recording
    // and replay could diverge across processes.
    let messages = vec![
        ChatMessage::system("s"),
        ChatMessage::user("u"),
        ChatMessage::assistant_tool_calls(vec![ToolCall::new("c", "t", json!({"b": 1, "a": 2}))]),
        ChatMessage::tool_result("c", "r"),
    ];
    let tools = vec![
        openai_tool_schema("beta", "second", json!({"type": "object"})),
        openai_tool_schema("alpha", "first", json!({"type": "object"})),
    ];
    let first = serde_json::to_value(chat_request_to_genai(&messages, &tools).unwrap()).unwrap();
    let second = serde_json::to_value(chat_request_to_genai(&messages, &tools).unwrap()).unwrap();
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// Tool schema pass-through
// ---------------------------------------------------------------------------

#[test]
fn tool_schemas_pass_through_in_order_without_rewrites() {
    // Deliberately unsorted: the order the caller gave is the order genai
    // gets (the ReAct node already sorted for replay-hash stability; the
    // adapter must not second-guess it).
    let parameters = json!({
        "type": "object",
        "properties": {"q": {"type": "string"}},
        "required": ["q"]
    });
    let tools = vec![
        openai_tool_schema("zeta", "last alphabetically", parameters.clone()),
        json!({
            "type": "function",
            "function": {
                "name": "alpha",
                "description": "first alphabetically",
                "parameters": {"type": "object"},
                "strict": true,
            }
        }),
    ];
    let request = chat_request_to_genai(&[ChatMessage::user("hi")], &tools).unwrap();
    let genai_tools = request.tools.expect("tools present");
    assert_eq!(genai_tools.len(), 2);
    assert_eq!(genai_tools[0].name.as_str(), "zeta");
    assert_eq!(genai_tools[1].name.as_str(), "alpha");
    // The parameters value crosses the boundary unmodified.
    assert_eq!(genai_tools[0].schema.as_ref(), Some(&parameters));
    assert_eq!(genai_tools[1].strict, Some(true));
    assert_eq!(
        genai_tools[0].description.as_deref(),
        Some("last alphabetically")
    );
}

#[test]
fn malformed_tool_schemas_are_invalid_requests() {
    for bad in [
        json!({"type": "function"}),                      // no function object
        json!({"function": {"description": "nameless"}}), // no name
        json!({"function": {"name": 42}}),                // name not a string
    ] {
        let err = tool_schema_to_genai(&bad).unwrap_err();
        assert_eq!(
            err.llm_class(),
            LlmErrorClass::InvalidRequest,
            "schema {bad}"
        );
    }
}

// ---------------------------------------------------------------------------
// Response translation
// ---------------------------------------------------------------------------

#[test]
fn response_translation_maps_text_tool_calls_model_and_usage() {
    let response = genai_response(
        r#"{
        "content": [
            {"Text": "The answer is "},
            {"Text": "42."},
            {"ToolCall": {"call_id": "call_1", "fn_name": "calc", "fn_arguments": {"a": 40, "b": 2}}}
        ],
        "model_iden": {"adapter_kind": "OpenAI", "model_name": "gpt-4o"},
        "provider_model_iden": {"adapter_kind": "OpenAI", "model_name": "gpt-4o-2024-08-06"},
        "usage": {
            "prompt_tokens": 12,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens": 7,
            "completion_tokens_details": {"reasoning_tokens": 3},
            "total_tokens": 19
        }
    }"#,
    );
    let response = response_from_genai(response);
    // Text parts concatenate the way streamed deltas accumulate.
    assert_eq!(
        response.message.content.as_deref(),
        Some("The answer is 42.")
    );
    assert_eq!(response.message.role, Role::Assistant);
    assert_eq!(
        response.message.tool_calls,
        vec![ToolCall::new("call_1", "calc", json!({"a": 40, "b": 2}))]
    );
    // The provider-reported name is the one journaled.
    assert_eq!(response.model.as_deref(), Some("gpt-4o-2024-08-06"));
    let usage = response.usage.expect("usage mapped");
    assert_eq!(
        usage,
        Usage {
            prompt_tokens: 12,
            completion_tokens: 7,
            total_tokens: 19,
            cached_tokens: Some(4),
            reasoning_tokens: Some(3),
        }
    );
}

#[test]
fn usage_mapping_handles_absent_computed_and_clamped_counts() {
    // Nothing reported stays unreported (distinguishable from zero).
    let bare: genai::chat::Usage = serde_json::from_value(json!({})).unwrap();
    assert_eq!(usage_from_genai(&bare), None);

    // A missing total falls back to prompt + completion.
    let no_total: genai::chat::Usage =
        serde_json::from_value(json!({"prompt_tokens": 5, "completion_tokens": 7})).unwrap();
    let usage = usage_from_genai(&no_total).unwrap();
    assert_eq!(usage.total_tokens, 12);
    assert_eq!(usage.cached_tokens, None);

    // genai reports zero-as-none already; a negative count (provider bug)
    // clamps to zero rather than wrapping.
    let negative: genai::chat::Usage =
        serde_json::from_value(json!({"prompt_tokens": -3, "total_tokens": -9})).unwrap();
    let usage = usage_from_genai(&negative).unwrap();
    assert_eq!(usage.prompt_tokens, 0);
    assert_eq!(usage.total_tokens, 0);
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

#[test]
fn http_statuses_classify_like_the_openai_client() {
    for (status, class) in [
        (StatusCode::TOO_MANY_REQUESTS, LlmErrorClass::RateLimited),
        (StatusCode::INTERNAL_SERVER_ERROR, LlmErrorClass::Server),
        (StatusCode::SERVICE_UNAVAILABLE, LlmErrorClass::Server),
        (StatusCode::REQUEST_TIMEOUT, LlmErrorClass::Server),
        (StatusCode::UNAUTHORIZED, LlmErrorClass::Auth),
        (StatusCode::FORBIDDEN, LlmErrorClass::Auth),
        (StatusCode::BAD_REQUEST, LlmErrorClass::InvalidRequest),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            LlmErrorClass::InvalidRequest,
        ),
    ] {
        // Both of genai's HTTP error surfaces agree.
        let direct = genai::Error::HttpError {
            status,
            canonical_reason: status.canonical_reason().unwrap_or("?").to_string(),
            body: "{}".into(),
        };
        assert_eq!(classify_genai_error(&direct), class, "status {status}");

        let wrapped = genai::Error::WebModelCall {
            model_iden: model_iden("gpt-x"),
            webc_error: webc::Error::ResponseFailedStatus {
                status,
                body: "{}".into(),
                headers: Box::new(HeaderMap::new()),
            },
        };
        assert_eq!(classify_genai_error(&wrapped), class, "status {status}");
    }
}

#[test]
fn non_status_failures_classify_by_cause() {
    for (error, class) in [
        (
            genai::Error::StreamParse {
                model_iden: model_iden("gpt-x"),
                serde_error: serde_json::from_str::<Value>("{").unwrap_err(),
            },
            LlmErrorClass::Decode,
        ),
        (
            genai::Error::SerdeJson(serde_json::from_str::<Value>("{").unwrap_err()),
            LlmErrorClass::Decode,
        ),
        (
            genai::Error::RequiresApiKey {
                model_iden: model_iden("gpt-x"),
            },
            LlmErrorClass::Auth,
        ),
        (
            genai::Error::NoAuthData {
                model_iden: model_iden("gpt-x"),
            },
            LlmErrorClass::Auth,
        ),
        (
            genai::Error::ChatReqHasNoMessages {
                model_iden: model_iden("gpt-x"),
            },
            LlmErrorClass::InvalidRequest,
        ),
        (
            genai::Error::AdapterNotSupported {
                adapter_kind: AdapterKind::OpenAI,
                feature: "embeddings".into(),
            },
            LlmErrorClass::InvalidRequest,
        ),
        (
            genai::Error::WebStream {
                model_iden: model_iden("gpt-x"),
                cause: "connection reset mid-stream".into(),
                error: Box::new(std::io::Error::other("reset")),
            },
            LlmErrorClass::Unknown,
        ),
    ] {
        assert_eq!(classify_genai_error(&error), class, "error: {error}");
    }

    // Transport decode failures classify as Decode.
    let not_json = genai::Error::WebAdapterCall {
        adapter_kind: AdapterKind::OpenAI,
        webc_error: webc::Error::ResponseFailedNotJson {
            content_type: "text/html".into(),
            body: "<html>".into(),
        },
    };
    assert_eq!(classify_genai_error(&not_json), LlmErrorClass::Decode);
}

// ---------------------------------------------------------------------------
// Stream folding and driving
// ---------------------------------------------------------------------------

/// A scripted genai event stream: greeting deltas, a reasoning delta, a
/// tool call, then End with terminal usage.
fn scripted_events() -> Vec<genai::Result<ChatStreamEvent>> {
    vec![
        Ok(stream_event(json!("Start"))),
        Ok(stream_event(json!({"Chunk": {"content": "Hel"}}))),
        Ok(stream_event(
            json!({"ReasoningChunk": {"content": "thinking..."}}),
        )),
        Ok(stream_event(json!({"Chunk": {"content": "lo"}}))),
        Ok(stream_event(json!({"ToolCallChunk": {"tool_call": {
            "call_id": "call_1", "fn_name": "search", "fn_arguments": {"q": "rust"}
        }}}))),
        Ok(stream_event(json!({"End": {
            "captured_usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "completion_tokens_details": {"reasoning_tokens": 2}
            }
        }}))),
    ]
}

#[tokio::test]
async fn stream_drives_text_deltas_to_callback_and_folds_the_rest() {
    let mut chunks: Vec<(String, bool)> = Vec::new();
    let response = drive_genai_stream(
        futures::stream::iter(scripted_events()),
        Some("gpt-4o".into()),
        &mut |chunk| chunks.push((chunk.delta.clone(), chunk.finish)),
    )
    .await
    .unwrap();

    // Text deltas — and only text deltas — fired the callback, followed by
    // exactly one terminal chunk with an empty delta.
    assert_eq!(
        chunks,
        vec![
            ("Hel".to_string(), false),
            ("lo".to_string(), false),
            (String::new(), true),
        ]
    );
    assert_eq!(response.message.content.as_deref(), Some("Hello"));
    // Reasoning deltas never became answer text.
    assert!(!response.message.content.unwrap().contains("thinking"));
    // The tool call arrived whole and accumulated silently.
    assert_eq!(
        response.message.tool_calls,
        vec![ToolCall::new("call_1", "search", json!({"q": "rust"}))]
    );
    // Terminal usage, detail fields included.
    let usage = response.usage.expect("terminal usage captured");
    assert_eq!(usage.total_tokens, 15);
    assert_eq!(usage.reasoning_tokens, Some(2));
    assert_eq!(response.model.as_deref(), Some("gpt-4o"));
}

#[tokio::test]
async fn stream_ending_without_an_end_event_still_finishes_cleanly() {
    let events = vec![Ok(stream_event(json!({"Chunk": {"content": "partial"}})))];
    let mut finishes = 0;
    let response = drive_genai_stream(futures::stream::iter(events), None, &mut |chunk| {
        if chunk.finish {
            finishes += 1;
        }
    })
    .await
    .unwrap();
    assert_eq!(finishes, 1, "exactly one terminal chunk");
    assert_eq!(response.message.content.as_deref(), Some("partial"));
    assert_eq!(response.usage, None);
    assert_eq!(response.model, None);
}

#[tokio::test]
async fn mid_stream_error_aborts_with_a_classified_failure() {
    let mut events = scripted_events();
    events.truncate(2);
    events.push(Err(genai::Error::HttpError {
        status: StatusCode::TOO_MANY_REQUESTS,
        canonical_reason: "Too Many Requests".into(),
        body: "slow down".into(),
    }));
    let mut chunks = 0;
    let err = drive_genai_stream(futures::stream::iter(events), None, &mut |_| chunks += 1)
        .await
        .unwrap_err();
    assert_eq!(err.llm_class(), LlmErrorClass::RateLimited);
    assert!(err.to_string().contains("429"), "got: {err}");
}

#[test]
fn fold_ignores_empty_text_deltas() {
    let mut fold = GenaiStreamFold::default();
    let mut fired = 0;
    fold.apply(
        &stream_event(json!({"Chunk": {"content": ""}})),
        &mut |_| fired += 1,
    );
    assert_eq!(fired, 0, "empty deltas do not fire the callback");
    let response = fold.into_response();
    assert_eq!(response.message.content, None);
}

// ---------------------------------------------------------------------------
// The model itself (construction is network-free)
// ---------------------------------------------------------------------------

#[test]
fn model_construction_pricing_and_effect() {
    let model = GenaiChatModel::new("gpt-4o-mini");
    assert_eq!(model.model(), "gpt-4o-mini");
    assert_eq!(model.pricing(), None);
    // A provider call is billable and unverifiable: the restrictive class.
    assert_eq!(model.effect(), Effect::NonIdempotent);

    let pricing = ModelPricing::new(0.15, 0.6);
    let model = model.with_pricing(pricing);
    assert_eq!(model.pricing(), Some(pricing));

    let from_client = GenaiChatModel::from_client(genai::Client::default(), "claude-haiku-4-5");
    assert_eq!(from_client.model(), "claude-haiku-4-5");
}

#[test]
fn errors_surface_as_classified_llm_failures() {
    // The adapter's error wrapper preserves the class and embeds genai's
    // detail text.
    let error = genai::Error::HttpError {
        status: StatusCode::UNAUTHORIZED,
        canonical_reason: "Unauthorized".into(),
        body: "bad key".into(),
    };
    let class = classify_genai_error(&error);
    let wrapped = RustyError::LlmFailure {
        class,
        message: format!("genai chat request failed: {error}"),
    };
    assert_eq!(wrapped.llm_class(), LlmErrorClass::Auth);
    assert!(wrapped.to_string().contains("bad key"));
}

// ---------------------------------------------------------------------------
// Fixture sanity: the genai-side JSON shapes these tests assume
// ---------------------------------------------------------------------------

#[test]
fn genai_fixtures_decode_the_shapes_the_adapter_consumes() {
    // Guard against a silent genai schema drift making every other test in
    // this file vacuous: a genai message decoded from JSON round-trips the
    // parts the adapter reads.
    let message: GenaiChatMessage = serde_json::from_value(json!({
        "role": "Assistant",
        "content": [
            {"Text": "checking"},
            {"ToolCall": {"call_id": "c1", "fn_name": "t", "fn_arguments": {}}},
        ],
    }))
    .unwrap();
    assert_eq!(message.role, ChatRole::Assistant);
    assert_eq!(message.content.first_text(), Some("checking"));
    let call: &GenaiToolCall = message.content.tool_calls()[0];
    assert_eq!(call.call_id, "c1");
}
