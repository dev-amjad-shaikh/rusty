//! A spawned agent idle for long enough is retired: archived, what it
//! learned folded into its maker's memory, the maker's owner told.

use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

fn registry() -> GraphRegistry {
    let spec = StateSpec::new().channel("done", Reducer::Overwrite);
    let mut builder = GraphBuilder::new();
    builder.add_node("work", |_ctx: NodeContext| async move {
        Ok(NodeOutput::update("done", json!(true)))
    });
    builder.set_entry_point("work");
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", builder.compile().unwrap(), spec);
    registry
}

fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-spawned-retire-{}", uuid::Uuid::new_v4()))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn an_idle_spawned_agent_is_retired_its_notes_folded_and_its_maker_told() {
    let store = temp_store();
    let app = router(
        registry(),
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
    );
    // The maker, owned by a person; the maintainer it spawned; a hand-made agent beside them.
    for (id, name, metadata) in [
        (
            "lead",
            "Research Lead",
            json!({"created_by": {"principal_id": "amjad", "name": "Amjad", "kind": "person"}}),
        ),
        (
            "mfa-maintainer",
            "MFA article maintainer",
            json!({"created_by": {"principal_id": "lead", "kind": "agent"}, "spawned_by": {"assistant_id": "lead", "run_id": "run-1"}}),
        ),
        (
            "desk",
            "Desk",
            json!({"created_by": {"principal_id": "amjad", "kind": "person"}}),
        ),
    ] {
        let (status, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": id, "name": name, "graph": "pipeline", "config": {}, "metadata": metadata}))).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
    }
    // The memory API writes into a runtime agent's private scope: both agents registered as such.
    for id in ["mfa-maintainer", "lead"] {
        let (status, v) = call(&app, "POST", "/agents", Some(json!({"agent_id": id, "manifest": {
            "agent_kind": "maintainer", "manifest_version": "maintainer/1.0.0",
            "accepts": {"work": {"kind": "application/json", "max_bytes": 65536, "schema": {"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object"}}},
            "scopes": ["private", "team"], "budget": {"max_tokens": 250000}
        }}))).await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
    }
    // What the maintainer learned, and a block it kept.
    let (status, note) = call(&app, "POST", "/memory", Some(json!({"kind": "fact", "scope": {"scope": "agent", "id": "mfa-maintainer"}, "content": {"text": "AADSTS50076 means a second factor is required"}, "author": {"type": "agent", "agent_id": "mfa-maintainer"}, "key": "aadsts50076", "priority": 6, "confidence": 0.8}))).await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    let (status, block) = call(&app, "POST", "/memory", Some(json!({"kind": "fact", "scope": {"scope": "agent", "id": "mfa-maintainer"}, "content": {"text": "draft v3 in progress"}, "author": {"type": "agent", "agent_id": "mfa-maintainer"}, "key": "block.working", "priority": 10, "confidence": 0.95}))).await;
    assert_eq!(status, StatusCode::CREATED, "{block}");

    // Its cadence: a daily schedule at six.
    let (status, cron) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "mfa-maintainer", "cron_expr": "0 6 * * *", "input": {"work": {}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");

    // The estate lists the spawned one alone, live, never run — with its
    // cadence, what it holds in memory and the gaps it left open.
    let (status, listed) = call(&app, "GET", "/estate/spawned", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed["spawned"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["assistant_id"], "mfa-maintainer");
    assert_eq!(rows[0]["idle_days"], 0);
    assert!(rows[0]["archived_at"].is_null());
    assert_eq!(
        rows[0]["schedules"][0]["cron_expr"], "0 6 * * *",
        "{listed}"
    );
    assert_eq!(rows[0]["notes"], 2, "{listed}");
    assert_eq!(rows[0]["open_gaps"], 0, "{listed}");
    assert_eq!(listed["retire_idle_days_default"], 14);
    assert_eq!(
        listed["retire_idle_days"], 14,
        "the workspace's nightly threshold starts at the default: {listed}"
    );

    // The workspace sets its own nightly threshold; zero switches the nightly retirement off.
    let (status, set) = call(
        &app,
        "PUT",
        "/estate/spawned/settings",
        Some(json!({"retire_idle_days": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    let (_, listed) = call(&app, "GET", "/estate/spawned", None).await;
    assert_eq!(listed["retire_idle_days"], 3, "{listed}");
    let (status, off) = call(
        &app,
        "PUT",
        "/estate/spawned/settings",
        Some(json!({"retire_idle_days": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{off}");
    let (_, listed) = call(&app, "GET", "/estate/spawned", None).await;
    assert_eq!(listed["retire_idle_days"], 0, "{listed}");
    let (status, refused) = call(
        &app,
        "PUT",
        "/estate/spawned/settings",
        Some(json!({"retire_idle_days": 99999})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // Not idle for a day yet: nothing retires at one day.
    let (status, nothing) = call(
        &app,
        "POST",
        "/estate/spawned/retire",
        Some(json!({"idle_days": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{nothing}");
    assert!(
        nothing["retired"].as_array().unwrap().is_empty(),
        "{nothing}"
    );

    // At zero days: retired, one note folded (the block stays with it), the maker's owner told.
    let (status, retired) = call(
        &app,
        "POST",
        "/estate/spawned/retire",
        Some(json!({"idle_days": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    let done = retired["retired"].as_array().unwrap();
    assert_eq!(done.len(), 1, "{retired}");
    assert_eq!(done[0]["assistant_id"], "mfa-maintainer");
    assert_eq!(done[0]["notes_folded"], 1, "{retired}");
    assert_eq!(
        done[0]["told"]["principal_id"], "dev",
        "the maker's owner — the caller who made the maker, as the server stamped it: {retired}"
    );
    let (_, agent) = call(&app, "GET", "/assistants/mfa-maintainer", None).await;
    assert!(agent["archived_at"].as_str().is_some(), "put away: {agent}");
    let (_, desk) = call(&app, "GET", "/assistants/desk", None).await;
    assert!(
        desk["archived_at"].is_null(),
        "a hand-made agent is nobody's to retire: {desk}"
    );

    // The maker's memory holds the fact, marked folded and where from; not the block.
    let (_, leads) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "agent", "id": "lead"}})),
    )
    .await;
    let records = leads["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{leads}");
    assert_eq!(
        records[0]["content"]["value"]["text"],
        "AADSTS50076 means a second factor is required"
    );
    assert_eq!(records[0]["key"], "folded.mfa-main.aadsts50076");
    assert!(
        records[0]["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "folded"),
        "{leads}"
    );
    assert!(
        records[0]["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "from:mfa-maintainer"),
        "{leads}"
    );
    assert_eq!(records[0]["priority"], 5);
    assert_eq!(
        records[0]["provenance"]["author"]["agent_id"], "mfa-maintainer",
        "the author stays: {leads}"
    );

    // Retiring again folds nothing twice and retires nothing already put away.
    let (_, again) = call(
        &app,
        "POST",
        "/estate/spawned/retire",
        Some(json!({"idle_days": 0})),
    )
    .await;
    assert!(again["retired"].as_array().unwrap().is_empty(), "{again}");
    let (_, leads) = call(
        &app,
        "POST",
        "/memory/query",
        Some(json!({"scope": {"scope": "agent", "id": "lead"}})),
    )
    .await;
    assert_eq!(leads["records"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(store);
}
