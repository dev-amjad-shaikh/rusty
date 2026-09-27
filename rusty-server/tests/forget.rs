//! Forgetting a person: their run records are sealed under their key from
//! the start, a backup carries the records without the key, and forgetting
//! destroys the key — so what a restore brings back of them is ciphertext
//! that reads as absent, while everyone else's history stands.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, Principal, PrincipalKind, Role, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Brief;

#[async_trait::async_trait]
impl ChatModel for Brief {
    async fn chat(&self, _messages: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("noted, and kept"),
            model: Some("brief".into()),
            usage: None,
        })
    }
}

fn person(id: &str, role: Role) -> Principal {
    Principal {
        id: id.to_owned(),
        name: id.to_owned(),
        kind: PrincipalKind::User,
        roles: vec![role],
    }
}

fn app_at(
    store: &std::path::Path,
    backups: &std::path::Path,
    restore_from: Option<&std::path::Path>,
) -> Router {
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(Brief), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
        .with_principal("default", person("ada", Role::Admin), "ada-key")
        .with_principal("default", person("ana", Role::Builder), "ana-key")
        .with_principal("default", person("bob", Role::Builder), "bob-key")
        .with_backup_dir(backups.to_path_buf());
    if let Some(archive) = restore_from {
        config = config.with_restore_from(archive.to_path_buf());
    }
    router(registry, config)
}

async fn call(
    app: &Router,
    key: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Api-Key", key);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn converse(app: &Router, key: &str, words: &str) -> (String, String) {
    let (status, thread) = call(
        app,
        key,
        "POST",
        "/threads",
        Some(json!({"graph": "react_agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        app,
        key,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": {"messages": [{"role": "user", "content": words}]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    (thread_id, run["run_id"].as_str().unwrap().to_owned())
}

async fn listed(app: &Router, key: &str) -> Vec<String> {
    let (status, runs) = call(app, key, "GET", "/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{runs}");
    runs.as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn forgetting_a_person_destroys_their_key_and_a_restored_backup_holds_only_ciphertext_of_them(
) {
    let base = std::env::temp_dir().join(format!("rusty-forget-{}", uuid::Uuid::new_v4()));
    let store = base.join("store");
    let backups = base.join("backups");
    let app = app_at(&store, &backups, None);

    // Ana and Bob each talk to the agent; the admin sees both.
    let (ana_thread, ana_run) = converse(
        &app,
        "ana-key",
        "Ana here: my badge number is 4471, please remember it.",
    )
    .await;
    let (_, bob_run) = converse(
        &app,
        "bob-key",
        "Bob here: the printer on floor 3 is out of toner.",
    )
    .await;
    let admin_sees = listed(&app, "ada-key").await;
    assert!(
        admin_sees.contains(&ana_run) && admin_sees.contains(&bob_run),
        "{admin_sees:?}"
    );

    // On disk, Ana's run is sealed under her key: her words are not in the
    // file; Bob's — nobody's key, a plain record — are in his.
    let ana_journal =
        std::fs::read_to_string(store.join("journals").join(format!("{ana_run}.json"))).unwrap();
    assert!(
        ana_journal.contains("\"sealed_for\""),
        "Ana's journal is sealed: {}",
        &ana_journal[..200.min(ana_journal.len())]
    );
    assert!(
        !ana_journal.contains("badge number is 4471"),
        "Ana's words are not on disk in the clear"
    );
    let ana_record =
        std::fs::read_to_string(store.join("accepted_runs").join(format!("{ana_run}.json")))
            .unwrap();
    assert!(ana_record.contains("\"sealed_for\"") && !ana_record.contains("badge number is 4471"));
    let bob_journal =
        std::fs::read_to_string(store.join("journals").join(format!("{bob_run}.json"))).unwrap();
    assert!(
        bob_journal.contains("sealed_for"),
        "Bob's run acts for Bob: sealed under his key too: {}",
        &bob_journal[..120]
    );
    let key_file = store.join("keys").join("person.default.ana.secret");
    assert!(key_file.exists(), "Ana's key is on the box");
    // And through the API, sealed reads as the record it is.
    let (status, run) = call(&app, "ada-key", "GET", &format!("/runs/{ana_run}"), None).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert!(run.to_string().contains("badge number is 4471"), "{run}");

    // A backup taken before the forget: the data archive holds the sealed
    // records and no person key; the person keys travel in their own.
    let (status, backup) = call(&app, "ada-key", "POST", "/estate/backups", None).await;
    assert_eq!(status, StatusCode::CREATED, "{backup}");
    let archive = backups.join(backup["name"].as_str().unwrap());
    let keys_archive = backup["person_keys"]
        .as_str()
        .map(|n| backups.join(n))
        .expect("a person-keys archive beside the data archive");
    assert!(archive.exists() && keys_archive.exists());
    assert_eq!(
        backup["manifest"]["person_keys"], 2,
        "Ana's and Bob's keys: {backup}"
    );
    let (_, estate) = call(&app, "ada-key", "GET", "/estate", None).await;
    assert_eq!(estate["counts"]["people_forgotten"], 0, "{estate}");

    // Forget Ana: an admin, with a reason. Ana is refused from then on; her
    // run is gone from the box; Bob's stands; the tombstone remains.
    // An agent's curated block names Ana beside Bob: erasure reaches it.
    let (status, desk) = call(
        &app,
        "ada-key",
        "POST",
        "/assistants",
        Some(json!({
            "assistant_id": "desk", "name": "Desk", "graph": "react_agent",
            "config": {"studio_intent": {"instructions": "Help.", "tools": []}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{desk}");
    let (status, block) = call(&app, "ada-key", "PUT", "/assistants/desk/memory/blocks/person", Some(json!({"text": "Ana sits on floor 2; badge 4471.\nBob keeps the floor-3 printer stocked."}))).await;
    assert_eq!(status, StatusCode::OK, "{block}");

    let (status, refused) = call(
        &app,
        "ada-key",
        "POST",
        "/users/ana/forget",
        Some(json!({"reason": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, forgotten) = call(
        &app,
        "ada-key",
        "POST",
        "/users/ana/forget",
        Some(json!({"reason": "left the company; erasure requested 2026-09-10"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{forgotten}");
    assert_eq!(forgotten["forgotten"]["principal"], "ana");
    assert_eq!(forgotten["runs_removed"], 1, "{forgotten}");
    assert_eq!(forgotten["threads_removed"], 1, "{forgotten}");
    assert_eq!(forgotten["key_destroyed"], true);
    assert_eq!(
        forgotten["blocks_scrubbed"],
        json!([{"agent_id": "desk", "label": "person", "lines_removed": 1, "versions_removed": 1}]),
        "{forgotten}"
    );
    // The version that named Ana is gone from the block's history too: nothing to restore her from.
    let (status, history) = call(
        &app,
        "ada-key",
        "GET",
        "/assistants/desk/memory/blocks/person/history",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert!(
        history["versions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| !v["text"].as_str().unwrap_or("").contains("Ana")),
        "{history}"
    );
    assert_eq!(
        history["versions"].as_array().unwrap().len(),
        1,
        "{history}"
    );
    let (status, blocks) = call(
        &app,
        "ada-key",
        "GET",
        "/assistants/desk/memory/blocks",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{blocks}");
    let person_block = blocks
        .as_array()
        .cloned()
        .or_else(|| blocks["blocks"].as_array().cloned())
        .unwrap()
        .into_iter()
        .find(|b| b["label"] == "person")
        .expect("the person block stands");
    assert_eq!(
        person_block["text"],
        json!("Bob keeps the floor-3 printer stocked."),
        "Ana's line is gone, Bob's stays: {person_block}"
    );
    assert!(!key_file.exists(), "the key is destroyed");
    assert!(
        store
            .join("keys")
            .join("person.default.ana.forgotten.json")
            .exists(),
        "the tombstone stands"
    );
    let (status, _) = call(&app, "ana-key", "GET", "/runs", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Ana's key names nobody now"
    );
    let after = listed(&app, "ada-key").await;
    assert!(
        !after.contains(&ana_run) && after.contains(&bob_run),
        "{after:?}"
    );
    let (status, _) = call(&app, "ada-key", "GET", &format!("/runs/{ana_run}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        "ada-key",
        "GET",
        &format!("/threads/{ana_thread}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, twice) = call(
        &app,
        "ada-key",
        "POST",
        "/users/ana/forget",
        Some(json!({"reason": "again"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{twice}");
    let (_, estate) = call(&app, "ada-key", "GET", "/estate", None).await;
    assert_eq!(estate["counts"]["people_forgotten"], 1, "{estate}");
    assert_eq!(estate["forgotten"][0]["principal"], "ana");
    assert_eq!(
        estate["forgotten"][0]["reason"],
        "left the company; erasure requested 2026-09-10"
    );

    // A restore of the backup taken before the forget, with the person keys
    // of *after* it beside the data archive: Ana's records come back as
    // ciphertext and read as absent; Bob's come back whole.
    let (_, later) = call(&app, "ada-key", "POST", "/estate/backups", None).await;
    let later_keys = backups.join(
        later["person_keys"]
            .as_str()
            .expect("Bob's key still travels"),
    );
    assert_eq!(
        later["manifest"]["person_keys"], 1,
        "only Bob's key now: {later}"
    );
    let restore_dir = base.join("restore");
    std::fs::create_dir_all(&restore_dir).unwrap();
    std::fs::copy(&archive, restore_dir.join(archive.file_name().unwrap())).unwrap();
    std::fs::copy(
        &later_keys,
        restore_dir.join(rusty_agent_server::estate::person_keys_name(
            archive.file_name().unwrap().to_str().unwrap(),
        )),
    )
    .unwrap();
    let fresh = base.join("fresh");
    let restored = app_at(
        &fresh,
        &base.join("fresh-backups"),
        Some(&restore_dir.join(archive.file_name().unwrap())),
    );
    let (status, estate) = call(&restored, "ada-key", "GET", "/estate", None).await;
    assert_eq!(status, StatusCode::OK, "{estate}");
    assert_eq!(
        estate["restored_from"]["archive"]
            .as_str()
            .map(|a| a.ends_with(archive.file_name().unwrap().to_str().unwrap())),
        Some(true),
        "{estate}"
    );
    assert!(
        fresh
            .join("journals")
            .join(format!("{ana_run}.json"))
            .exists(),
        "Ana's sealed journal came back with the data archive"
    );
    let raw =
        std::fs::read_to_string(fresh.join("journals").join(format!("{ana_run}.json"))).unwrap();
    assert!(
        raw.contains("\"sealed_for\"") && !raw.contains("badge number is 4471"),
        "ciphertext, still"
    );
    let seen = listed(&restored, "ada-key").await;
    assert!(
        seen.contains(&bob_run),
        "Bob's run reads on the restored box: {seen:?}"
    );
    assert!(
        !seen.contains(&ana_run),
        "Ana's run is ciphertext without a key: {seen:?}"
    );
    let (status, _) = call(
        &restored,
        "ada-key",
        "GET",
        &format!("/runs/{ana_run}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, bob) = call(
        &restored,
        "ada-key",
        "GET",
        &format!("/runs/{bob_run}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bob}");
    assert!(bob.to_string().contains("toner"), "{bob}");

    let _ = std::fs::remove_dir_all(base);
}
