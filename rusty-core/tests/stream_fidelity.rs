//! Stream fidelity tests (EP-01-S11).
//!
//! Verifies that streaming chunks are journaled as separate `AssistantChunk`
//! events before the assembled `ModelCall`, that chunk concatenation equals
//! the assembled message, and that replay reconstructs the exact chunk
//! sequence.

use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, TokenChunk};
use rusty_agent_runtime::record::{AssistantChunk, RunEventKind};
use rusty_agent_runtime::replay::RecordingChatModel;
use serde_json::Value;
use std::sync::Arc;

/// A mock model that emits a known multi-chunk sequence.
struct ChunkedMock {
    chunks: Vec<&'static str>,
}

#[async_trait::async_trait]
impl ChatModel for ChunkedMock {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> rusty_agent_runtime::error::Result<ChatResponse> {
        let assembled: String = self.chunks.iter().copied().collect();
        Ok(ChatResponse {
            message: ChatMessage::assistant(&assembled),
            model: None,
            usage: None,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        _tools: &[Value],
        on_token: &mut (dyn FnMut(TokenChunk) + Send),
    ) -> rusty_agent_runtime::error::Result<ChatResponse> {
        for (i, piece) in self.chunks.iter().enumerate() {
            on_token(TokenChunk {
                delta: (*piece).into(),
                finish: i + 1 == self.chunks.len(),
                raw: None,
            });
        }
        self.chat(messages, &[]).await
    }
}

/// Helper: create a fresh journal and a recording model wrapping `inner`.
fn recording_setup(inner: Arc<dyn ChatModel>) -> (Journal, RecordingChatModel) {
    let clock = Clock::logical(0, 1);
    let journal = Journal::new("run-test", "thread-test", clock);
    let model =
        RecordingChatModel::new(inner, journal.clone(), "parent-0").with_chunk_capture(true);
    (journal, model)
}

#[tokio::test]
async fn stream_chunks_are_journaled_separately() {
    let mock = Arc::new(ChunkedMock {
        chunks: vec!["Hel", "lo", " ", "world"],
    });
    let (journal, model) = recording_setup(mock);

    model.chat_stream(&[], &[], &mut |_chunk| {}).await.unwrap();

    let snapshot = journal.snapshot();
    let events = snapshot.events;

    // Each chunk becomes an AssistantChunk event.
    let chunk_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == RunEventKind::AssistantChunk)
        .collect();
    assert_eq!(chunk_events.len(), 4, "expected 4 chunk events");

    // stream_index is monotonic from 0.
    for (i, event) in chunk_events.iter().enumerate() {
        let chunk: AssistantChunk = serde_json::from_value(match event.output.clone().unwrap() {
            rusty_agent_runtime::record::PayloadRef::Inline(v) => v,
            other => panic!("unexpected payload ref: {:?}", other),
        })
        .unwrap();
        assert_eq!(chunk.stream_index, i as u64);
        assert_eq!(chunk.delta, ["Hel", "lo", " ", "world"][i]);
    }

    // The ModelCall event follows the chunks.
    let model_calls: Vec<_> = events
        .iter()
        .filter(|e| e.kind == RunEventKind::ModelCall)
        .collect();
    assert_eq!(model_calls.len(), 1);
    let model_call = &model_calls[0];
    let chunk_positions: Vec<u64> = chunk_events.iter().map(|e| e.seq).collect();
    assert!(
        model_call.seq > *chunk_positions.last().unwrap(),
        "ModelCall must come after all chunks"
    );
}

#[tokio::test]
async fn chunk_assembly_matches_response() {
    let mock = Arc::new(ChunkedMock {
        chunks: vec!["Hel", "lo", " ", "world"],
    });
    let (_journal, model) = recording_setup(mock);

    let response = model.chat_stream(&[], &[], &mut |_chunk| {}).await.unwrap();
    assert_eq!(response.message.content.as_deref().unwrap(), "Hello world");
}

#[tokio::test]
async fn chunk_mismatch_returns_typed_error() {
    // A mock whose chat_stream emits one set of chunks but whose chat()
    // returns a different message, simulating corruption.
    struct MismatchMock;

    #[async_trait::async_trait]
    impl ChatModel for MismatchMock {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[Value],
        ) -> rusty_agent_runtime::error::Result<ChatResponse> {
            Ok(ChatResponse {
                message: ChatMessage::assistant("WRONG"),
                model: None,
                usage: None,
            })
        }

        async fn chat_stream(
            &self,
            messages: &[ChatMessage],
            _tools: &[Value],
            on_token: &mut (dyn FnMut(TokenChunk) + Send),
        ) -> rusty_agent_runtime::error::Result<ChatResponse> {
            on_token(TokenChunk {
                delta: "right".into(),
                finish: true,
                raw: None,
            });
            self.chat(messages, &[]).await
        }
    }

    let mock = Arc::new(MismatchMock);
    let (_journal, model) = recording_setup(mock);

    let err = model
        .chat_stream(&[], &[], &mut |_chunk| {})
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("chunk assembly mismatch"),
        "expected ChunkAssemblyMismatch, got: {msg}"
    );
}

#[tokio::test]
async fn replay_reconstructs_exact_chunk_sequence() {
    let mock = Arc::new(ChunkedMock {
        chunks: vec!["alpha", " ", "beta"],
    });
    let (journal, model) = recording_setup(mock);

    model.chat_stream(&[], &[], &mut |_chunk| {}).await.unwrap();

    let snapshot = journal.snapshot();
    let events = snapshot.events;

    let chunks: Vec<AssistantChunk> = events
        .iter()
        .filter(|e| e.kind == RunEventKind::AssistantChunk)
        .map(|e| {
            serde_json::from_value(match e.output.clone().unwrap() {
                rusty_agent_runtime::record::PayloadRef::Inline(v) => v,
                other => panic!("unexpected payload ref: {:?}", other),
            })
            .unwrap()
        })
        .collect();

    let deltas: Vec<String> = chunks.iter().map(|c| c.delta.clone()).collect();
    assert_eq!(deltas, vec!["alpha", " ", "beta"]);
}
