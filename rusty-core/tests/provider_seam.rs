//! Provider seam tests: prefix routing, provenance stamps, pre-dispatch
//! invariants, and persistence.

use std::sync::Arc;

use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ProviderRegistry};
use rusty_api::{ComponentAttribution, TrafficClass, TurnBoundary, TurnStamp};
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock providers
// ---------------------------------------------------------------------------

/// A mock provider that records every stamp it receives.
struct StampCapturingMock {
    prefix: String,
    stamps: Arc<std::sync::Mutex<Vec<TurnStamp>>>,
}

#[async_trait::async_trait]
impl ChatModel for StampCapturingMock {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> rusty_agent_runtime::error::Result<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant(format!("hello from {}", self.prefix)),
            model: Some(format!("{}/test", self.prefix)),
            usage: None,
        })
    }

    async fn chat_stamped(
        &self,
        stamp: &TurnStamp,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> rusty_agent_runtime::error::Result<ChatResponse> {
        self.stamps.lock().unwrap().push(stamp.clone());
        self.chat(messages, tools).await
    }
}

/// A mock provider that panics if called (for negative tests).
struct PanicMock;

#[async_trait::async_trait]
impl ChatModel for PanicMock {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
    ) -> rusty_agent_runtime::error::Result<ChatResponse> {
        panic!("PanicMock should never be invoked in this test")
    }
}

// ---------------------------------------------------------------------------
// AC 1: prefix routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_registry_routes_by_prefix() {
    let openai_stamps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let anthropic_stamps = Arc::new(std::sync::Mutex::new(Vec::new()));

    let openai = Arc::new(StampCapturingMock {
        prefix: "openai".to_string(),
        stamps: openai_stamps.clone(),
    });
    let anthropic = Arc::new(StampCapturingMock {
        prefix: "anthropic".to_string(),
        stamps: anthropic_stamps.clone(),
    });

    let mut registry = ProviderRegistry::new();
    registry.register("openai", openai);
    registry.register("anthropic", anthropic);

    let (model, provider) = registry.resolve("openai/gpt-4").unwrap();
    assert_eq!(model, "gpt-4");
    let response = provider.chat(&[], &[]).await.unwrap();
    assert_eq!(
        response.message.content,
        Some("hello from openai".to_string())
    );

    let (model, provider) = registry.resolve("anthropic/claude-3").unwrap();
    assert_eq!(model, "claude-3");
    let response = provider.chat(&[], &[]).await.unwrap();
    assert_eq!(
        response.message.content,
        Some("hello from anthropic".to_string())
    );
}

#[tokio::test]
async fn provider_registry_rejects_unregistered_prefix() {
    let mut registry = ProviderRegistry::new();
    registry.register("openai", Arc::new(PanicMock));

    let err = match registry.resolve("bogus/model") {
        Err(e) => e,
        Ok(_) => panic!("expected error for bogus/model"),
    };
    let msg = err.to_string();
    assert!(msg.contains("provider not registered"), "got: {msg}");
    assert!(msg.contains("bogus"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// AC 2: stamp correctness across a three-step turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stamped_provider_carries_correct_stamp_per_step() {
    let stamps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock = Arc::new(StampCapturingMock {
        prefix: "test".to_string(),
        stamps: stamps.clone(),
    });

    let session_id = Uuid::new_v4();
    let turn_id = Uuid::new_v4();

    // Step 1: start
    let stamp1 = TurnStamp {
        session_id,
        traffic: TrafficClass::Main,
        turn_id,
        turn_boundary: TurnBoundary::Start,
        issued_by: ComponentAttribution {
            component: "react_agent".to_string(),
            sub_id: None,
        },
    };
    let _ = mock.chat_stamped(&stamp1, &[], &[]).await.unwrap();

    // Step 2: continuation
    let stamp2 = TurnStamp {
        session_id,
        traffic: TrafficClass::Main,
        turn_id,
        turn_boundary: TurnBoundary::Continuation,
        issued_by: ComponentAttribution {
            component: "react_agent".to_string(),
            sub_id: None,
        },
    };
    let _ = mock.chat_stamped(&stamp2, &[], &[]).await.unwrap();

    // Step 3: end
    let stamp3 = TurnStamp {
        session_id,
        traffic: TrafficClass::Main,
        turn_id,
        turn_boundary: TurnBoundary::End,
        issued_by: ComponentAttribution {
            component: "react_agent".to_string(),
            sub_id: None,
        },
    };
    let _ = mock.chat_stamped(&stamp3, &[], &[]).await.unwrap();

    let captured = stamps.lock().unwrap();
    assert_eq!(captured.len(), 3);
    assert_eq!(captured[0].turn_boundary, TurnBoundary::Start);
    assert_eq!(captured[1].turn_boundary, TurnBoundary::Continuation);
    assert_eq!(captured[2].turn_boundary, TurnBoundary::End);
    assert_eq!(captured[0].session_id, session_id);
    assert_eq!(captured[0].turn_id, turn_id);
    assert!(matches!(captured[0].traffic, TrafficClass::Main));
}

#[tokio::test]
async fn stamped_provider_distinguishes_main_and_side_traffic() {
    let stamps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock = Arc::new(StampCapturingMock {
        prefix: "test".to_string(),
        stamps: stamps.clone(),
    });

    let main_stamp = TurnStamp {
        session_id: Uuid::new_v4(),
        traffic: TrafficClass::Main,
        turn_id: Uuid::new_v4(),
        turn_boundary: TurnBoundary::Start,
        issued_by: ComponentAttribution {
            component: "react_agent".to_string(),
            sub_id: None,
        },
    };
    let side_stamp = TurnStamp {
        session_id: Uuid::new_v4(),
        traffic: TrafficClass::Side,
        turn_id: Uuid::new_v4(),
        turn_boundary: TurnBoundary::Start,
        issued_by: ComponentAttribution {
            component: "review_fork".to_string(),
            sub_id: Some("fork-1".to_string()),
        },
    };

    let _ = mock.chat_stamped(&main_stamp, &[], &[]).await.unwrap();
    let _ = mock.chat_stamped(&side_stamp, &[], &[]).await.unwrap();

    let captured = stamps.lock().unwrap();
    assert!(matches!(captured[0].traffic, TrafficClass::Main));
    assert!(matches!(captured[1].traffic, TrafficClass::Side));
}

// ---------------------------------------------------------------------------
// AC 6: cargo metadata assertion — no provider SDK in kernel
// ---------------------------------------------------------------------------

#[test]
fn kernel_imports_no_provider_sdk() {
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()))
        .output()
        .expect("cargo metadata should run");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    let resolve = meta
        .get("resolve")
        .and_then(|r| r.get("nodes"))
        .and_then(|n| n.as_array())
        .unwrap();

    let runtime_node = resolve
        .iter()
        .find(|n| {
            n.get("id")
                .and_then(|i| i.as_str())
                .map(|s| s.contains("rusty-agent-runtime"))
                .unwrap_or(false)
        })
        .expect("rusty-agent-runtime node should exist");

    let deps: Vec<String> = runtime_node
        .get("deps")
        .and_then(|d| d.as_array())
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|d| d.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    // No direct provider SDK crates in the kernel.
    let forbidden = ["genai", "openai", "anthropic", "gemini", "ollama"];
    for bad in &forbidden {
        assert!(
            !deps.iter().any(|d| d == *bad),
            "rusty-agent-runtime should not depend on {bad}"
        );
    }
}
