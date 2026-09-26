//! Plugins: a pack the deployment ships installs whole, is listed with what
//! it brought, and refuses removal while an agent names any of it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{router, GraphRegistry, ServerConfig, ShippedPlugin};
use serde_json::{json, Value};
use tower::ServiceExt;

fn pipeline_graph() -> (Graph, StateSpec) {
    let spec = StateSpec::new().channel("log", Reducer::Append);
    let mut builder = GraphBuilder::new();
    builder.add_node("first", |_ctx: NodeContext| async { Ok(NodeOutput::update("log", json!("first"))) });
    builder.set_entry_point("first");
    (builder.compile().unwrap(), spec)
}

fn pack(id: &str, with_skill: bool) -> ShippedPlugin {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert(
        "plugin.json".into(),
        json!({
            "id": id, "name": "Cat Facts starter", "version": "1.0.0", "publisher": "Rusty",
            "description": "one connector, one skill",
            "connectors": ["connectors/cat-facts.json"],
            "skills": if with_skill { vec!["skills/cite-the-source"] } else { vec![] },
            "hosts": ["docs.catfact.ninja", "Developer.Example.com"],
            "knowledge": if with_skill { vec!["knowledge/cat-facts-notes.md"] } else { vec![] },
        })
        .to_string()
        .into_bytes(),
    );
    files.insert(
        "connectors/cat-facts.json".into(),
        json!({
            "id": "cat-facts", "version": "1", "display_name": "Cat Facts",
            "description": "catfact.ninja", "documentation_url": "https://catfact.ninja/",
            "base_url": "https://catfact.ninja",
            "connection_specification": {"type": "object", "additionalProperties": false, "properties": {}, "title": "Cat Facts"},
            "operations": [
                {"name": "check", "description": "answers", "method": "GET", "path": "/fact", "effect": "read_only", "params_schema": {"type": "object"}, "headers": [], "auth": [], "max_response_bytes": null},
                {"name": "fact", "description": "one fact", "method": "GET", "path": "/fact", "effect": "read_only", "params_schema": {"type": "object"}, "headers": [], "auth": [], "max_response_bytes": null}
            ],
            "check": "check"
        })
        .to_string()
        .into_bytes(),
    );
    if with_skill {
        files.insert(
            "knowledge/cat-facts-notes.md".into(),
            b"# Cat facts, the notes\n\nThe catfact.ninja service returns one random fact per call; the `length` field counts characters.\n".to_vec(),
        );
        files.insert(
            "skills/cite-the-source/SKILL.md".into(),
            b"---\nname: cite-the-source\ndescription: Use whenever an answer repeats a fact a tool returned.\n---\n\n# Cite the source\n\n## When to use\nAlways.\n\n## The method\n1. Quote it.\n\n## What done looks like\nTraceable.\n".to_vec(),
        );
    }
    ShippedPlugin { files }
}

fn app(packs: Vec<ShippedPlugin>) -> (Router, PathBuf) {
    let store = std::env::temp_dir().join(format!("rusty-server-plugins-{}", uuid::Uuid::new_v4()));
    let (graph, spec) = pipeline_graph();
    let mut registry = GraphRegistry::new();
    registry.register("pipeline", graph, spec);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()).with_plugin_packs(packs);
    (router(registry, config), store)
}

fn skill_names(v: &Value) -> Vec<String> {
    v.get("skills")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_owned)).collect())
        .unwrap_or_default()
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => { builder = builder.header("content-type", "application/json"); Body::from(v.to_string()) }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_pack_installs_whole_is_listed_and_refuses_removal_while_used() {
    let (app, store) = app(vec![pack("cat-facts-starter", true)]);

    // Offered, not installed.
    let (status, library) = call(&app, "GET", "/plugins/library", None).await;
    assert_eq!(status, StatusCode::OK, "{library}");
    assert_eq!(library["plugins"][0]["id"], "cat-facts-starter");
    assert_eq!(library["plugins"][0]["installed"], false);
    let (_, installed) = call(&app, "GET", "/plugins", None).await;
    assert!(installed["plugins"].as_array().unwrap().is_empty());

    // Install: the connector lands in the library, the skill in the registry.
    let (status, record) = call(&app, "POST", "/plugins/install", Some(json!({"library": "cat-facts-starter"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{record}");
    assert_eq!(record["connectors"][0]["id"], "cat-facts");
    assert_eq!(record["skills"][0]["name"], "cite-the-source");
    assert!(record["installed_by"].is_object(), "{record}");
    let (_, manifests) = call(&app, "GET", "/connectors", None).await;
    assert!(manifests["manifests"].as_array().unwrap().iter().any(|m| m["id"] == "cat-facts"), "{manifests}");
    let (_, skills) = call(&app, "GET", "/skills", None).await;
    assert!(skill_names(&skills).iter().any(|s| s == "cite-the-source"), "{skills}");
    let (_, library) = call(&app, "GET", "/plugins/library", None).await;
    assert_eq!(library["plugins"][0]["installed"], true);

    // Twice is a conflict, not a second copy.
    let (status, again) = call(&app, "POST", "/plugins/install", Some(json!({"library": "cat-facts-starter"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");

    // An agent that names its skill keeps it installed.
    let (status, agent) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"name": "Fact teller", "graph": "pipeline", "config": {"studio_intent": {"instructions": "Tell facts.", "tools": [], "skills": ["cite-the-source"]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let (status, refused) = call(&app, "DELETE", "/plugins/cat-facts-starter", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"], "plugin_in_use");
    assert!(refused["message"].as_str().unwrap().contains("Fact teller"), "{refused}");

    // Archive the agent — an archived agent holds nothing — and the plugin
    // goes; what it registered stays.
    let assistant_id = agent["assistant_id"].as_str().unwrap();
    let active = agent["active_version_id"].as_str().unwrap();
    let (status, archived) = call(
        &app,
        "POST",
        &format!("/assistants/{assistant_id}/archive"),
        Some(json!({"expected_active_version_id": active})),
    )
    .await;
    assert!(status.is_success(), "archive: {status} {archived}");
    let (status, gone) = call(&app, "DELETE", "/plugins/cat-facts-starter", None).await;
    assert_eq!(status, StatusCode::OK, "{gone}");
    let (_, skills) = call(&app, "GET", "/skills", None).await;
    assert!(skill_names(&skills).iter().any(|s| s == "cite-the-source"), "the skill stays: {skills}");
    let (status, _) = call(&app, "DELETE", "/plugins/cat-facts-starter", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_pack_with_a_bad_member_installs_nothing() {
    let mut bad = pack("broken", true);
    bad.files.insert("connectors/cat-facts.json".into(), b"{ not json".to_vec());
    let (app, store) = app(vec![bad]);
    let (status, err) = call(&app, "POST", "/plugins/install", Some(json!({"library": "broken"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err["message"].as_str().unwrap().contains("connectors/cat-facts.json"), "{err}");
    let (_, skills) = call(&app, "GET", "/skills", None).await;
    assert!(!skill_names(&skills).iter().any(|s| s == "cite-the-source"), "nothing registered: {skills}");
    let (_, installed) = call(&app, "GET", "/plugins", None).await;
    assert!(installed["plugins"].as_array().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_url_install_is_refused_before_any_fetch_when_it_cannot_be_read() {
    let (app, store) = app(vec![pack("cat-facts-starter", true)]);
    let (status, body) = call(&app, "POST", "/plugins/install", Some(json!({ "url": "http://github.com/acme/plugin" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("https://"), "{body}");

    let (status, body) = call(&app, "POST", "/plugins/install", Some(json!({ "url": "https://example.com/some/page" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("GitHub repository URL"), "{body}");

    // Neither a pack nor a URL, or both, is not a request.
    let (status, body) = call(&app, "POST", "/plugins/install", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("either a pack"), "{body}");
    let (status, _) = call(&app, "POST", "/plugins/install", Some(json!({ "library": "cat-facts-starter", "url": "https://github.com/acme/plugin" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert!(call(&app, "GET", "/plugins", None).await.1["plugins"].as_array().unwrap().is_empty(), "nothing installed");
    let _ = std::fs::remove_dir_all(store);
}

/// A plugin names the vendor hosts its skills read; a person allows them.
/// The record carries the hosts; against a closed ceiling the listing
/// says which are not admitted yet; allowing puts them on the ceiling
/// noted with the plugin, and the listing then says all are allowed.
#[tokio::test]
async fn a_plugins_vendor_hosts_are_named_and_a_person_allows_them() {
    let (app, store) = app(vec![pack("cat-facts-starter", true)]);
    let (status, record) = call(&app, "POST", "/plugins/install", Some(json!({"library": "cat-facts-starter"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{record}");
    assert_eq!(record["hosts"], json!(["docs.catfact.ninja", "developer.example.com"]), "named and normalized: {record}");
    // A closed ceiling that admits neither host.
    let (status, ceiling) = call(&app, "PUT", "/egress/ceiling", Some(json!({"open": false, "hosts": [{"host": "catfact.ninja"}]}))).await;
    assert_eq!(status, StatusCode::OK, "{ceiling}");
    let (_, listed) = call(&app, "GET", "/plugins", None).await;
    let plugin = &listed["plugins"][0];
    assert_eq!(plugin["ceiling_open"], false);
    assert_eq!(plugin["hosts_outside"], json!(["docs.catfact.ninja", "developer.example.com"]), "{plugin}");
    // Allowed by a person: on the ceiling, noted with the plugin.
    let (status, allowed) = call(&app, "POST", "/plugins/cat-facts-starter/hosts/allow", None).await;
    assert_eq!(status, StatusCode::OK, "{allowed}");
    assert_eq!(allowed["allowed"], json!(["docs.catfact.ninja", "developer.example.com"]));
    assert_eq!(allowed["plugin"]["hosts_outside"], json!([]));
    let (_, ceiling) = call(&app, "GET", "/egress/ceiling", None).await;
    let noted: Vec<&str> = ceiling["hosts"].as_array().unwrap().iter().filter(|h| h["note"] == "plugin:cat-facts-starter").filter_map(|h| h["host"].as_str()).collect();
    assert_eq!(noted, vec!["docs.catfact.ninja", "developer.example.com"], "{ceiling}");
    assert!(ceiling["hosts"].as_array().unwrap().iter().any(|h| h["host"] == "catfact.ninja"), "the ceiling's own host stays");
    // Allowed again: nothing more to admit.
    let (status, again) = call(&app, "POST", "/plugins/cat-facts-starter/hosts/allow", None).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["allowed"], json!([]));
    let _ = std::fs::remove_dir_all(store);
}

/// A plugin ships knowledge: registered as sources under the plugin's name
/// at install, listed on the record, found by a knowledge query; loading
/// again registers nothing new.
#[tokio::test]
async fn a_plugins_knowledge_is_registered_under_its_name_and_found() {
    let (app, store) = app(vec![pack("cat-facts-starter", true)]);
    let (status, record) = call(&app, "POST", "/plugins/install", Some(json!({"library": "cat-facts-starter"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{record}");
    assert_eq!(record["knowledge"][0]["source_id"], "plugin:cat-facts-starter:cat-facts-notes", "{record}");
    assert_eq!(record["knowledge"][0]["title"], "Cat facts, the notes");
    let (_, sources) = call(&app, "GET", "/knowledge/sources", None).await;
    let mine = sources["sources"].as_array().unwrap().iter().find(|s| s["source_id"] == "plugin:cat-facts-starter:cat-facts-notes").cloned();
    assert!(mine.is_some(), "{sources}");
    let mine = mine.unwrap();
    assert_eq!(mine["author"], "plugin:cat-facts-starter", "{mine}");
    let (status, found) = call(&app, "POST", "/knowledge/query", Some(json!({"text": "length field counts characters", "limit": 3}))).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert!(found.to_string().contains("plugin:cat-facts-starter:cat-facts-notes"), "the query finds the plugin's knowledge: {found}");
    let (_, listed) = call(&app, "GET", "/plugins", None).await;
    assert_eq!(listed["plugins"][0]["knowledge_shipped"], 1);
    let (status, again) = call(&app, "POST", "/plugins/cat-facts-starter/knowledge/load", None).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["loaded"].as_array().map(Vec::len), Some(1));
    let (_, sources) = call(&app, "GET", "/knowledge/sources", None).await;
    let versions = sources["sources"].as_array().unwrap().iter().filter(|s| s["source_id"] == "plugin:cat-facts-starter:cat-facts-notes").count();
    assert_eq!(versions, 1, "content-addressed: one source, not two: {sources}");
    let _ = std::fs::remove_dir_all(store);
}
