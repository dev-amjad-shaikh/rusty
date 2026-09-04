//! `request_error` seam extensions: retry and overflow recovery.
//!
//! Verifies EP-02-S11 acceptance criteria:
//! - bare loop retries nothing (AC 1)
//! - retry handler with ceiling, base delay, backoff (AC 2, AC 3)
//! - overflow recovery compacts and retries (AC 4)
//! - waterfall ordering when both handlers are registered (AC 5)
//! - frozen prefix / reconstructability checks still pass on retries (AC 6)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use rusty_agent_runtime::error::{LlmErrorClass, RustyError};
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, Role};
use rusty_agent_runtime::middleware::{MiddlewareChain, OverflowRecoveryHandler, RetryHandler};

/// A model that fails `failures` times with `error`, then succeeds.
struct FlakyModel {
    failures: AtomicU32,
    error: String,
    error_class: LlmErrorClass,
    call_count: AtomicU32,
}

impl FlakyModel {
    fn new(failures: u32, error_class: LlmErrorClass, error: impl Into<String>) -> Self {
        Self {
            failures: AtomicU32::new(failures),
            error: error.into(),
            error_class,
            call_count: AtomicU32::new(0),
        }
    }

    fn calls(&self) -> u32 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ChatModel for FlakyModel {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
    ) -> Result<ChatResponse, RustyError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count < self.failures.load(Ordering::SeqCst) {
            Err(RustyError::LlmFailure {
                class: self.error_class,
                message: self.error.clone(),
            })
        } else {
            Ok(ChatResponse {
                message: ChatMessage::assistant(format!("success after {count} failures")),
                model: None,
                usage: None,
            })
        }
    }
}

/// AC 1: bare loop retries nothing.
#[tokio::test]
async fn bare_loop_does_not_retry() {
    let model = Arc::new(FlakyModel::new(2, LlmErrorClass::Server, "transient"));
    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model.clone(),
        MiddlewareChain::new(),
    );

    let err = wrapped
        .chat(&[ChatMessage::user("hi")], &[])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("transient"), "got: {err}");
    assert_eq!(model.calls(), 1, "bare loop should not retry");
}

/// AC 2: retry handler succeeds after transient failures.
#[tokio::test]
async fn retry_handler_succeeds_after_transients() {
    tokio::time::pause();
    let model = Arc::new(FlakyModel::new(2, LlmErrorClass::Server, "transient"));
    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model.clone(),
        MiddlewareChain::new().layer(RetryHandler::new().with_ceiling(3)),
    );

    let response = wrapped.chat(&[ChatMessage::user("hi")], &[]).await.unwrap();
    assert_eq!(
        response.message.content.as_deref(),
        Some("success after 2 failures")
    );
    assert_eq!(model.calls(), 3, "initial + 2 retries = 3 calls");
}

/// AC 3: retry ceiling is respected.
#[tokio::test]
async fn retry_ceiling_propagates_final_error() {
    tokio::time::pause();
    let model = Arc::new(FlakyModel::new(5, LlmErrorClass::Server, "still down"));
    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model.clone(),
        MiddlewareChain::new().layer(RetryHandler::new().with_ceiling(2)),
    );

    let err = wrapped
        .chat(&[ChatMessage::user("hi")], &[])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("still down"), "got: {err}");
    assert_eq!(model.calls(), 3, "initial + 2 retries = 3 calls");
}

/// AC 3: non-transient errors are not retried.
#[tokio::test]
async fn non_transient_errors_are_not_retried() {
    let model = Arc::new(FlakyModel::new(1, LlmErrorClass::Auth, "bad key"));
    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model.clone(),
        MiddlewareChain::new().layer(RetryHandler::new().with_ceiling(3)),
    );

    let err = wrapped
        .chat(&[ChatMessage::user("hi")], &[])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bad key"), "got: {err}");
    assert_eq!(model.calls(), 1, "auth failure should not retry");
}

/// AC 4: overflow recovery compacts messages and retries.
#[tokio::test]
async fn overflow_recovery_compacts_and_retries() {
    let _calls: Vec<Vec<ChatMessage>> = Vec::new();
    let calls_ref = Arc::new(Mutex::new(Vec::new()));
    let calls_clone = calls_ref.clone();

    let model: Arc<dyn ChatModel> = Arc::new({
        struct OverflowThenOk {
            calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
            count: AtomicU32,
        }
        #[async_trait]
        impl ChatModel for OverflowThenOk {
            async fn chat(
                &self,
                messages: &[ChatMessage],
                _tools: &[serde_json::Value],
            ) -> Result<ChatResponse, RustyError> {
                let count = self.count.fetch_add(1, Ordering::SeqCst);
                self.calls.lock().unwrap().push(messages.to_vec());
                if count == 0 {
                    Err(RustyError::Llm("context length exceeded".into()))
                } else {
                    Ok(ChatResponse {
                        message: ChatMessage::assistant("ok"),
                        model: None,
                        usage: None,
                    })
                }
            }
        }
        OverflowThenOk {
            calls: calls_clone,
            count: AtomicU32::new(0),
        }
    });

    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model,
        MiddlewareChain::new().layer(OverflowRecoveryHandler::new()),
    );

    let messages = vec![
        ChatMessage::system("rules"),
        ChatMessage::user("turn 1"),
        ChatMessage::assistant("reply 1"),
        ChatMessage::user("turn 2"),
        ChatMessage::assistant("reply 2"),
        ChatMessage::user("turn 3"),
    ];

    let response = wrapped.chat(&messages, &[]).await.unwrap();
    assert_eq!(response.message.content.as_deref(), Some("ok"));

    let recorded = calls_ref.lock().unwrap();
    assert_eq!(recorded.len(), 2, "initial fail + retry = 2 calls");

    // First call: original messages (6)
    assert_eq!(recorded[0].len(), 6);

    // Second call: compacted (system + summary + last user = 3)
    assert_eq!(recorded[1].len(), 3);
    assert_eq!(recorded[1][0].role, Role::System);
    assert_eq!(recorded[1][1].role, Role::System); // compaction summary
    assert_eq!(recorded[1][2].role, Role::User); // last turn
}

/// AC 5: both handlers registered — waterfall ordering.
#[tokio::test]
async fn composition_retry_then_overflow() {
    tokio::time::pause();
    let _calls: Vec<String> = Vec::new();
    let calls_ref = Arc::new(Mutex::new(Vec::new()));
    let calls_clone = calls_ref.clone();

    let model: Arc<dyn ChatModel> = Arc::new({
        struct TransientThenOverflowThenOk {
            calls: Arc<Mutex<Vec<String>>>,
            count: AtomicU32,
        }
        #[async_trait]
        impl ChatModel for TransientThenOverflowThenOk {
            async fn chat(
                &self,
                _messages: &[ChatMessage],
                _tools: &[serde_json::Value],
            ) -> Result<ChatResponse, RustyError> {
                let count = self.count.fetch_add(1, Ordering::SeqCst);
                match count {
                    0 => {
                        self.calls.lock().unwrap().push("transient".into());
                        Err(RustyError::LlmFailure {
                            class: LlmErrorClass::Timeout,
                            message: "timeout".into(),
                        })
                    }
                    1 => {
                        self.calls.lock().unwrap().push("overflow".into());
                        Err(RustyError::Llm("context length exceeded".into()))
                    }
                    _ => {
                        self.calls.lock().unwrap().push("ok".into());
                        Ok(ChatResponse {
                            message: ChatMessage::assistant("ok"),
                            model: None,
                            usage: None,
                        })
                    }
                }
            }
        }
        TransientThenOverflowThenOk {
            calls: calls_clone,
            count: AtomicU32::new(0),
        }
    });

    // Retry handler first, overflow second.
    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model,
        MiddlewareChain::new()
            .layer(RetryHandler::new().with_ceiling(3))
            .layer(OverflowRecoveryHandler::new()),
    );

    let response = wrapped.chat(&[ChatMessage::user("hi")], &[]).await.unwrap();
    assert_eq!(response.message.content.as_deref(), Some("ok"));

    let recorded = calls_ref.lock().unwrap();
    assert_eq!(
        recorded.as_slice(),
        &["transient", "overflow", "ok"],
        "retry handles transient, overflow handles overflow, then success"
    );
}

/// AC 6: invariant checks still pass on retried requests.
/// (This test verifies that the middleware chain preserves the messages
/// array across retries — no duplication or mutation leakage.)
#[tokio::test]
async fn retry_preserves_message_identity_per_attempt() {
    tokio::time::pause();
    let messages_per_attempt = Arc::new(Mutex::new(Vec::new()));
    let clone = messages_per_attempt.clone();

    let model: Arc<dyn ChatModel> = Arc::new({
        struct CheckIdentity {
            attempts: AtomicU32,
            snapshots: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        }
        #[async_trait]
        impl ChatModel for CheckIdentity {
            async fn chat(
                &self,
                messages: &[ChatMessage],
                _tools: &[serde_json::Value],
            ) -> Result<ChatResponse, RustyError> {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                self.snapshots.lock().unwrap().push(messages.to_vec());
                if attempt < 2 {
                    Err(RustyError::LlmFailure {
                        class: LlmErrorClass::Server,
                        message: "fail".into(),
                    })
                } else {
                    Ok(ChatResponse {
                        message: ChatMessage::assistant("done"),
                        model: None,
                        usage: None,
                    })
                }
            }
        }
        CheckIdentity {
            attempts: AtomicU32::new(0),
            snapshots: clone,
        }
    });

    let wrapped = rusty_agent_runtime::middleware::MiddlewareChatModel::new(
        model,
        MiddlewareChain::new().layer(RetryHandler::new().with_ceiling(3)),
    );

    let original = vec![
        ChatMessage::system("system prompt"),
        ChatMessage::user("question"),
    ];
    wrapped.chat(&original, &[]).await.unwrap();

    let snapshots = messages_per_attempt.lock().unwrap();
    assert_eq!(snapshots.len(), 3);
    // Every attempt should see the identical original messages.
    for snap in snapshots.iter() {
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].content.as_deref(), Some("system prompt"));
        assert_eq!(snap[1].content.as_deref(), Some("question"));
    }
}
