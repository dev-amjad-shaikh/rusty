//! A learned reference's freshness, through the API: a skill that has
//! learned nothing says so, a check with nothing to re-read refuses with
//! the reason, and the comparison that decides staleness is the one the
//! module publishes.
//!
//! The drift itself needs a system that moves under a live connection, so
//! it is proven against the real ServiceNow instance and recorded in the
//! handover; the egress preflight refuses loopback by design, and this
//! suite does not weaken that to fake one.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::freshness::{moved_since, notice, ReadStamp, Stamp};
use rusty_agent_server::{router, ConnectionTools, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Brief;

#[async_trait::async_trait]
impl ChatModel for Brief {
    async fn chat(&self, _m: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse { message: ChatMessage::assistant("noted"), model: Some("brief".into()), usage: None })
    }
}

fn app(store: &std::path::Path) -> Router {
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(Arc::new(Brief), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry.register_with_tools("react_agent", graph, spec, &tools).unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf()).with_connection_tools(connection_tools);
    router(registry, config)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_skill_that_has_learned_nothing_says_so_and_a_check_refuses_with_the_reason() {
    let store = std::env::temp_dir().join(format!("rusty-freshness-{}", uuid::Uuid::new_v4()));
    let app = app(&store);
    let skill_md = "---\nname: count-well\ndescription: How to count and answer.\n---\n\n# Count well\n\nAnswer with the count.\n";
    let (status, made) = call(&app, "POST", "/skills", Some(json!({"skill_md": skill_md}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // Nothing learned: the page says so rather than claiming freshness.
    let (status, freshness) = call(&app, "GET", "/skills/count-well/freshness", None).await;
    assert_eq!(status, StatusCode::OK, "{freshness}");
    assert_eq!(freshness["learned"], false);
    assert!(freshness["note"].as_str().unwrap().contains("learned nothing"), "{freshness}");

    // And a check has nothing to compare against, said in words.
    let (status, refused) = call(&app, "POST", "/skills/count-well/freshness", Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert!(refused.to_string().contains("learned nothing yet"), "{refused}");

    // A skill nobody has is not a freshness question either.
    let (status, _) = call(&app, "GET", "/skills/no-such-skill/freshness", None).await;
    assert_eq!(status, StatusCode::OK, "an unlearned skill and an unknown one read the same way");

    let _ = std::fs::remove_dir_all(store);
}

fn stamp(title: &str, records: usize, newest: Option<&str>, digest: &str) -> ReadStamp {
    ReadStamp {
        title: title.to_owned(),
        tool: "servicenow.list-records".to_owned(),
        arguments: json!({"table": "incident"}),
        records,
        digest: digest.to_owned(),
        version_field: newest.map(|_| "sys_updated_on".to_owned()),
        newest: newest.map(str::to_owned),
        groups: None,
    }
}

#[test]
fn what_the_platform_says_moved_is_what_a_person_would_say() {
    let then = vec![stamp("Resolutions", 12, Some("2026-09-01 10:00:00"), "aaa")];

    // Nothing moved: nothing said, and nothing is marked stale.
    assert!(moved_since(&then, &then).is_empty());

    // A count and a watermark: both named, in the words the studio shows
    // and the agent reads.
    let now = vec![stamp("Resolutions", 18, Some("2026-09-11 09:00:00"), "bbb")];
    assert_eq!(
        moved_since(&then, &now),
        vec![
            "Resolutions: 12 records → 18".to_owned(),
            "Resolutions: newest sys_updated_on 2026-09-01 10:00:00 → 2026-09-11 09:00:00".to_owned(),
        ]
    );

    // An edit in place moves neither count nor watermark; the digest is
    // what catches it, and it is said plainly rather than silently missed.
    let edited = vec![stamp("Resolutions", 12, Some("2026-09-01 10:00:00"), "ccc")];
    assert_eq!(moved_since(&then, &edited), vec!["Resolutions: the answer changed".to_owned()]);

    // A stale reference still reads, with what moved said above it.
    let marked = Stamp {
        skill: "servicenow-resolutions".to_owned(),
        revision: 4,
        reference: "learned/resolutions.md".to_owned(),
        learned_at: "2026-09-01T10:00:00Z".parse().unwrap(),
        reads: then,
        checked_at: Some("2026-09-11T02:00:00Z".parse().unwrap()),
        stale: true,
        because: moved_since(&[stamp("Resolutions", 12, Some("2026-09-01 10:00:00"), "aaa")], &now),
    };
    let said = notice(&marked);
    assert!(said.contains("STALE"), "{said}");
    assert!(said.contains("12 records → 18"), "{said}");
    assert!(said.contains("read the system for anything that matters"), "{said}");
}
