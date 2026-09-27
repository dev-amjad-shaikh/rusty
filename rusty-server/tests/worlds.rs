//! A world stands in for a connected system: a run in it has its connector
//! calls answered by the world, not the wire, and a suite whose case names
//! a world takes the create path every time — the world goes back to its
//! seed before each case.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::connector::{
    ConnectorManifest, ConnectorOperation, HttpMethod, OperationEffect,
};
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::Role as ChatRole;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, ConnectionTools, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

type Seen = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

/// A support desk that looks before it files: one search, one create, one
/// answer — the same every run, whatever the results say.
struct Desk {
    seen: Seen,
}

#[async_trait::async_trait]
impl ChatModel for Desk {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let answered = messages.iter().filter(|m| m.role == ChatRole::Tool).count();
        let message = match answered {
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "servicenow.list-records",
                json!({"table": "incident", "sysparm_query": "active=true^short_descriptionLIKEfog machine", "sysparm_fields": "number,state,short_description", "sysparm_limit": 5}),
            )]),
            1 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c2",
                "servicenow.create-incident",
                json!({"short_description": "Fog machine in the unicorn stables will not start", "description": "Reported by the stable hand; the machine hums and produces no fog.", "caller_id": "Abel Tuter"}),
            )]),
            _ => {
                let filed = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == ChatRole::Tool)
                    .and_then(|m| m.content.as_deref())
                    .and_then(|c| serde_json::from_str::<Value>(c).ok())
                    .and_then(|v| v["result"]["number"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "nothing".to_owned());
                ChatMessage::assistant(format!("No open incident matched, so I filed {filed}."))
            }
        };
        Ok(ChatResponse {
            message,
            model: Some("desk".into()),
            usage: None,
        })
    }
}

/// A ServiceNow-shaped connector on a host that does not exist: nothing a
/// run does here can be answered by the wire.
fn servicenow_manifest() -> ConnectorManifest {
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["instance"], "properties": {"instance": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    ConnectorManifest::new(
        "servicenow",
        "1",
        "ServiceNow",
        "The ServiceNow Table API.",
        "https://developer.servicenow.com/",
        "https://{instance}.service-now.com",
        spec,
        vec![
            op("whoami", HttpMethod::Get, "/api/now/table/sys_user?sysparm_limit=1", OperationEffect::ReadOnly, json!({"type": "object"})),
            op(
                "list-records",
                HttpMethod::Get,
                "/api/now/table/{table}?sysparm_display_value=true",
                OperationEffect::ReadOnly,
                json!({"type": "object", "required": ["table"], "properties": {"table": {"type": "string"}, "sysparm_query": {"type": "string"}, "sysparm_fields": {"type": "string"}, "sysparm_limit": {"type": "integer"}}}),
            ),
            op(
                "create-incident",
                HttpMethod::Post,
                "/api/now/table/incident",
                OperationEffect::Idempotent,
                json!({"type": "object", "required": ["short_description"], "properties": {"short_description": {"type": "string"}, "description": {"type": "string"}, "caller_id": {"type": "string"}}}),
            ),
        ],
        "whoami",
    )
    .expect("the manifest validates")
}

fn app_at(store: &std::path::Path) -> (Router, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let model: Arc<dyn ChatModel> = Arc::new(Desk {
        seen: Arc::clone(&seen),
    });
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
        .with_connection_tools(connection_tools);
    (router(registry, config), seen)
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
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn evaluation_done(app: &Router, name: &str, version: &str, id: &str) -> Value {
    for _ in 0..400 {
        let (_, list) = call(
            app,
            "GET",
            &format!("/datasets/{name}/versions/{version}/evaluations"),
            None,
        )
        .await;
        let evaluations = list["evaluations"]
            .as_array()
            .cloned()
            .or_else(|| list.as_array().cloned())
            .unwrap_or_default();
        if let Some(found) = evaluations.iter().find(|e| e["evaluation_id"] == id) {
            if found["status"] == "done" {
                return found.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("evaluation {id} never finished");
}

#[tokio::test]
async fn a_suite_in_a_world_takes_the_create_path_every_time_and_nothing_reaches_the_wire() {
    let store = std::env::temp_dir().join(format!("rusty-server-worlds-{}", uuid::Uuid::new_v4()));
    let (app, seen) = app_at(&store);

    // The connection, as a person makes one: the connector, then an
    // instance on a host nothing here can reach.
    let (status, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(servicenow_manifest()).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();
    assert!(
        instance["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "servicenow.create-incident"),
        "{instance}"
    );

    // The desk: allow-listed to the connection's tools, as the studio builds it.
    let (status, made) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "sn-desk", "name": "Support Desk", "graph": "react", "config": {"instructions": "You look before you file.", "studio_intent": {"tools": [{"name": "servicenow.list-records"}, {"name": "servicenow.create-incident"}]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    // A world standing in for that connection, from the dialect's starter.
    let (status, dialects) = call(&app, "GET", "/worlds/dialects", None).await;
    assert_eq!(status, StatusCode::OK, "{dialects}");
    assert_eq!(dialects["dialects"][0]["id"], "servicenow-table");
    // Every dialect says what its seed holds — the studio's hint for the
    // person editing one — including the faults any world can carry.
    for dialect in dialects["dialects"].as_array().unwrap() {
        let shape = dialect["seed_shape"].as_str().unwrap_or_default();
        assert!(
            shape.contains("faults") && shape.contains("drop_response"),
            "{dialect}"
        );
    }
    let (status, world) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-twin", "instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    assert_eq!(world["connector"], "servicenow");
    assert_eq!(
        world["stands_for"], "dev-twin.service-now.com",
        "the host the connection calls: {world}"
    );
    assert_eq!(world["dialect"], "servicenow-table");
    assert_eq!(
        world["records"]["incident"], 3,
        "the starter's incidents: {world}"
    );
    let world_id = world["world_id"].as_str().unwrap().to_owned();
    let (status, twice) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-twin", "instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{twice}");

    // A run in the world: the search finds nothing, the create is answered
    // by the world with a number, and the world holds the record.
    let input = json!({"messages": [{"role": "user", "content": "The fog machine in the unicorn stables will not start; please raise it."}]});
    let (status, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": input, "assistant_id": "sn-desk", "config": {"world": world_id}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");
    let said = run.to_string();
    assert!(
        said.contains("I filed INC0010004"),
        "the world numbered the incident after its seed: {said}"
    );
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, world_now) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(world_now["records"]["incident"], 4, "{world_now}");
    assert_eq!(world_now["calls_since_reset"], 2, "{world_now}");
    assert!(
        world_now["tables"]["incident"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["short_description"]
                .as_str()
                .unwrap_or("")
                .contains("Fog machine")),
        "{world_now}"
    );
    // The desk saw the world's empty search and the world's created record.
    let results: Vec<String> = seen
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .iter()
        .filter(|m| m.role == ChatRole::Tool)
        .filter_map(|m| m.content.clone())
        .collect();
    assert!(
        results[0].contains("\"result\":[]"),
        "the search in the world found nothing: {}",
        results[0]
    );
    assert!(
        results[1].contains("INC0010004"),
        "the world answered the create: {}",
        results[1]
    );

    // The same run, not in a world: the wire is asked and cannot answer.
    let (_, bare_thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let bare_thread_id = bare_thread["thread_id"].as_str().unwrap().to_owned();
    let (_, bare) = call(
        &app,
        "POST",
        &format!("/threads/{bare_thread_id}/runs/wait"),
        Some(json!({"input": input, "assistant_id": "sn-desk"})),
    )
    .await;
    let bare_results: Vec<String> = seen
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .iter()
        .filter(|m| m.role == ChatRole::Tool)
        .filter_map(|m| m.content.clone())
        .collect();
    assert!(
        bare_results.iter().all(|r| r.starts_with("ERROR:")),
        "without a world the calls go to the wire, which is not there: {bare_results:?} ({bare})"
    );

    // A suite whose case names the world: evaluated twice, the create path
    // both times, the same verdict, and the world reset before each. The
    // case carries the run's input as the run recorded it (the charter it
    // heard leads it), the way the studio publishes one.
    let (_, recorded) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let case_input = recorded["input"].clone();
    let case = json!({
        "id": "fog-machine",
        "input": case_input,
        "expect": {"tool_trajectory": [{"name": "servicenow.list-records"}, {"name": "servicenow.create-incident"}]},
        "tags": ["world:sn-twin", "campaign:F8"],
        "source": {"run_id": run_id, "thread_id": thread_id, "agent_id": "sn-desk", "captured_at": "2026-09-10T00:00:00Z"}
    });
    let (status, published) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "sn-desk-files", "version": "1", "cases": [case]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{published}");
    let mut verdicts = Vec::new();
    for _ in 0..2 {
        let (status, started) = call(
            &app,
            "POST",
            "/datasets/sn-desk-files/versions/1/evaluations",
            Some(json!({"assistant_id": "sn-desk"})),
        )
        .await;
        assert!(status.is_success(), "{status}: {started}");
        let id = started["evaluation_id"].as_str().unwrap().to_owned();
        let done = evaluation_done(&app, "sn-desk-files", "1", &id).await;
        assert_eq!(done["passed"], 1, "{done}");
        assert_eq!(done["cases"][0]["world"], "sn-twin", "{done}");
        assert!(
            done["cases"][0]["tool_calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "servicenow.create-incident"),
            "{done}"
        );
        verdicts.push(done["cases"][0]["passed"].clone());
    }
    assert_eq!(verdicts, vec![json!(true), json!(true)]);
    let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(after["reset_count"], 2, "one reset per case: {after}");
    assert_eq!(
        after["records"]["incident"], 4,
        "the seed's three and the last case's one — not five: {after}"
    );

    // A case naming a world the tenant does not hold fails by name.
    let stray = json!({
        "id": "stray",
        "input": case_input,
        "expect": {},
        "tags": ["world:nowhere"],
        "source": {"run_id": run_id, "thread_id": thread_id, "agent_id": "sn-desk", "captured_at": "2026-09-10T00:00:00Z"}
    });
    let (status, _) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "sn-desk-stray", "version": "1", "cases": [stray]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, started) = call(
        &app,
        "POST",
        "/datasets/sn-desk-stray/versions/1/evaluations",
        Some(json!({"assistant_id": "sn-desk"})),
    )
    .await;
    let done = evaluation_done(
        &app,
        "sn-desk-stray",
        "1",
        started["evaluation_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done["passed"], 0);
    assert!(
        done["cases"][0]["error"]
            .as_str()
            .unwrap()
            .contains("names world `nowhere`"),
        "{done}"
    );

    // Reset by hand, and gone.
    let (status, reset) = call(&app, "POST", "/worlds/sn-twin/reset", None).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert_eq!(reset["records"]["incident"], 3);
    assert_eq!(reset["reset_count"], 3);
    let (status, _) = call(&app, "DELETE", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(store);
}

/// A connection's check can be proved against a world before it is saved:
/// the check reaches the world and nothing else, the outcome names the
/// world, and a world standing in for another host is refused.
#[tokio::test]
async fn a_check_proved_against_a_world_reaches_the_world_and_nothing_else() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-check-{}",
        uuid::Uuid::new_v4()
    ));
    let (app, _seen) = app_at(&store);
    let (status, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(servicenow_manifest()).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();
    let (status, world) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-check", "instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{world}");

    let (status, proved) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(
            json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}, "world": "sn-check"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{proved}");
    assert_eq!(proved["status"], "succeeded", "{proved}");
    assert_eq!(proved["world"], "sn-check");

    let (status, live) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{live}");
    assert_ne!(live["status"], "succeeded", "{live}");
    assert!(live.get("world").is_none());

    let (status, other) = call(&app, "POST", "/connectors/check", Some(json!({"manifest_hash": hash, "config": {"instance": "elsewhere"}, "world": "sn-check"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{other}");
    assert!(other.to_string().contains("stands in for"), "{other}");

    let _ = std::fs::remove_dir_all(store);
}

/// A person puts a run in a world by the world's name, as the studio offers
/// it; the run is admitted under the world's id and the world answers. A
/// world nobody holds is refused at admission — the run never starts, so
/// nothing of it reaches the live system.
#[tokio::test]
async fn a_run_names_its_world_by_name_and_an_unknown_world_stops_the_run_before_it_starts() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-named-{}",
        uuid::Uuid::new_v4()
    ));
    let (app, seen) = app_at(&store);
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(servicenow_manifest()).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "sn-desk", "name": "Support Desk", "graph": "react", "config": {"instructions": "You look before you file.", "studio_intent": {"tools": [{"name": "servicenow.list-records"}, {"name": "servicenow.create-incident"}]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, world) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-twin", "instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();

    let input = json!({"messages": [{"role": "user", "content": "The fog machine in the unicorn stables will not start; please raise it."}]});
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();

    // By name: the world answers, and the journal declares the world by id.
    let (status, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"input": input, "assistant_id": "sn-desk", "config": {"world": "sn-twin"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");
    assert!(
        run.to_string().contains("I filed INC0010004"),
        "the world answered: {run}"
    );
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    let declared = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("run_config_declared"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        declared["output"]["value"]["world"],
        json!(world_id),
        "the journal says which world: {declared}"
    );
    let (_, world_now) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(world_now["calls_since_reset"], 2, "{world_now}");

    // A world nobody holds: refused before the run exists.
    let model_calls_before = seen.lock().unwrap().len();
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, refused) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": input, "assistant_id": "sn-desk", "config": {"world": "no-such-world"}}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("unknown world `no-such-world`"),
        "{refused}"
    );
    assert!(
        refused
            .to_string()
            .contains("nothing reached the live system"),
        "{refused}"
    );
    let (_, runs) = call(&app, "GET", "/runs", None).await;
    assert_eq!(
        runs.as_array().map(|r| r.len()).unwrap_or(0),
        1,
        "no second run was admitted: {runs}"
    );
    let model_calls = seen.lock().unwrap().len();
    assert_eq!(
        model_calls, model_calls_before,
        "the model never saw the refused run"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A system that is not there yet: its connector is described into the
/// library, and a world is made from the library entry — before any
/// connection exists — with the connector's operations and host. Then the
/// connect step proves a configuration against that world, and the
/// connection is made without the live host ever answering.
#[tokio::test]
async fn a_world_from_a_connector_in_the_library_stands_in_before_any_connection_exists() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-library-{}",
        uuid::Uuid::new_v4()
    ));
    let (app, _seen) = app_at(&store);
    // The connector as a builder describes it: a literal host, a read and a write.
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities",
        "1",
        "Facilities Desk",
        "The facilities desk's tickets.",
        "https://facilities.example.internal/docs",
        "https://facilities.example.internal",
        spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Idempotent, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (status, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");

    // From the library, by id: the host from the base URL, the operations
    // from the manifest, the connector's own dialect.
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities", "dialect": "manifest-rest"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    assert_eq!(
        world["stands_for"], "facilities.example.internal",
        "{world}"
    );
    assert_eq!(world["connector"], "facilities");
    assert!(
        world["records"].as_object().is_some_and(|t| !t.is_empty()),
        "tables from the operations: {world}"
    );

    // The connect step, proved against the world: the check reaches the
    // stand-in, and the connection is saved without the live host.
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, checked) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(
            json!({"manifest_hash": hash, "config": {"token": "any"}, "world": "facilities-twin"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{checked}");
    assert_eq!(checked["status"], "succeeded", "{checked}");
    assert_eq!(checked["world"], "facilities-twin");

    // Under a closed ceiling the live host is out of reach; a check against
    // the world still proves the shape, and says the ceiling stands between
    // the connection and the live system. Without a world, the ceiling
    // refuses the check as before.
    let (status, ceiling) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": false, "hosts": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ceiling}");
    let (status, checked) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(
            json!({"manifest_hash": hash, "config": {"token": "any"}, "world": "facilities-twin"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{checked}");
    assert_eq!(checked["status"], "succeeded", "{checked}");
    assert_eq!(
        checked["outside_ceiling"], "facilities.example.internal",
        "{checked}"
    );
    let (status, refused) = call(
        &app,
        "POST",
        "/connectors/check",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["error"], "egress_outside_ceiling", "{refused}");
    let (_, _) = call(
        &app,
        "PUT",
        "/egress/ceiling",
        Some(json!({"open": true, "hosts": []})),
    )
    .await;

    // A connector nobody described is a name and a host only, and needs
    // the host said.
    let (status, refused) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "ghost", "connector": "nobody"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(refused.to_string().contains("stands_for"), "{refused}");
    let _ = std::fs::remove_dir_all(store);
}

/// A desk whose job ends in an irreversible write: reads the tickets,
/// files one, answers with the number.
struct Filer;

#[async_trait::async_trait]
impl ChatModel for Filer {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let answered = messages.iter().filter(|m| m.role == ChatRole::Tool).count();
        let message = match answered {
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1",
                "facilities-desk.list-tickets",
                json!({"status": "open"}),
            )]),
            1 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c2",
                "facilities-desk.create-ticket",
                json!({"title": "Coffee machine broken", "room": "kitchen"}),
            )]),
            _ => {
                let filed = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == ChatRole::Tool)
                    .and_then(|m| m.content.as_deref())
                    .and_then(|c| serde_json::from_str::<Value>(c).ok())
                    .and_then(|v| v["number"].as_u64())
                    .unwrap_or(0);
                ChatMessage::assistant(format!("Filed ticket #{filed}."))
            }
        };
        Ok(ChatResponse {
            message,
            model: Some("filer".into()),
            usage: None,
        })
    }
}

/// A suite whose case ends in an irreversible write, in a world: the
/// evaluation approves the call for itself — the effect lands in the
/// stand-in — so the suite runs without a person, and the record says
/// the evaluation decided, and in which world.
#[tokio::test]
async fn in_a_world_the_evaluation_approves_the_irreversible_call_for_itself() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-approve-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Filer);
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools);
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk",
        "1",
        "Facilities Desk",
        "The facilities desk's tickets.",
        "https://facilities.example.internal/docs",
        "https://facilities.example.internal",
        spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (status, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [{"id": 1, "number": 1, "title": "Projector flickers", "room": "2A", "status": "open"}], "me": [{"id": 1}]}, "counters": {"tickets": 2}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();
    let (status, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");
    let (status, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED);

    // The live-shaped run a person would make first: it pauses at the
    // gate; the person approves; the case is made from it.
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let input = json!({"messages": [{"role": "user", "content": "The kitchen coffee machine is broken; file it if it is not filed."}]});
    let (_, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"input": input, "assistant_id": "triage", "config": {"world": "facilities-twin"}}))).await;
    assert_eq!(run["status"], "interrupted", "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{run_id}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let (_, recorded) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    // The case expects what the run leaves in the world — a ticket for the
    // kitchen, and nothing in `me` — not only what it calls. A second case
    // expects a room the agent never files for, and must fail saying so.
    let case = json!({
        "id": "coffee-machine",
        "input": recorded["input"].clone(),
        "expect": {"tool_trajectory": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}], "world_writes": [{"table": "tickets", "fields": {"/room": "kitchen"}, "like": {"/title": "COFFEE"}}], "no_world_writes": ["me"]},
        "tags": ["world:facilities-twin"],
        "source": {"run_id": run_id, "thread_id": thread_id, "agent_id": "triage", "captured_at": "2026-09-13T00:00:00Z"}
    });
    let mut wrong_room = case.clone();
    wrong_room["id"] = json!("coffee-machine-lobby");
    wrong_room["expect"] =
        json!({"world_writes": [{"table": "tickets", "fields": {"/room": "lobby"}}]});
    let (status, published) = call(
        &app,
        "POST",
        "/datasets",
        Some(json!({"name": "facilities-triage", "version": "1", "cases": [case, wrong_room]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{published}");

    // The suite: nobody approves anything, and it passes on the whole chain.
    let (status, started) = call(
        &app,
        "POST",
        "/datasets/facilities-triage/versions/1/evaluations",
        Some(json!({"assistant_id": "triage"})),
    )
    .await;
    assert!(status.is_success(), "{status}: {started}");
    let id = started["evaluation_id"].as_str().unwrap().to_owned();
    let done = evaluation_done(&app, "facilities-triage", "1", &id).await;
    assert_eq!(done["passed"], 1, "{done}");
    assert_eq!(done["total"], 2, "{done}");
    let case_done = done["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["case_id"] == "coffee-machine")
        .expect("the kitchen case");
    let assertions = case_done["assertions"].as_array().unwrap();
    let leaves = assertions
        .iter()
        .find(|a| a["assertion"] == "world_write[tickets]")
        .expect("the world-write assertion");
    assert_eq!(leaves["passed"], true, "{leaves}");
    assert_eq!(
        leaves["observed"]["rows"][0]["room"], "kitchen",
        "the row it wrote is the evidence: {leaves}"
    );
    assert_eq!(
        assertions
            .iter()
            .find(|a| a["assertion"] == "no_world_write")
            .unwrap()["passed"],
        true,
        "{assertions:?}"
    );
    let lobby = done["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["case_id"] == "coffee-machine-lobby")
        .expect("the lobby case");
    assert_eq!(lobby["passed"], false, "{lobby}");
    let missed = lobby["assertions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["assertion"] == "world_write[tickets]")
        .unwrap();
    assert_eq!(
        missed["detail"], "the run wrote 1 row(s) in `tickets`, none with room = \"lobby\"",
        "{missed}"
    );
    assert_eq!(case_done["approvals_in_world"], 1, "{case_done}");
    assert_eq!(
        case_done["resumed_run_ids"].as_array().map(Vec::len),
        Some(1),
        "{case_done}"
    );
    assert!(
        case_done["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "facilities-desk.create-ticket"),
        "{case_done}"
    );
    // The record of that approval says who decided, and where.
    let (_, approvals) = call(&app, "GET", "/approvals", None).await;
    let evaluated = approvals["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == case_done["run_id"])
        .cloned()
        .expect("the case's approval");
    assert_eq!(evaluated["status"], "approved");
    assert_eq!(
        evaluated["decided_by"]["name"], "the evaluation, in world facilities-twin",
        "{evaluated}"
    );
    assert_eq!(evaluated["decided_by"]["kind"], "service");
    assert!(
        evaluated["reason"].as_str().unwrap().contains("stand-in"),
        "{evaluated}"
    );
    // The listing says the paused run was continued, not that it needs anyone.
    let (_, listed) = call(&app, "GET", "/runs?limit=50", None).await;
    let paused_listed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == case_done["run_id"])
        .cloned()
        .expect("the paused run is listed");
    assert_eq!(paused_listed["status"], "interrupted", "{paused_listed}");
    assert_eq!(
        paused_listed["decision"]["status"], "approved",
        "{paused_listed}"
    );
    assert_eq!(
        paused_listed["decision"]["resumed_run_id"], case_done["resumed_run_ids"][0],
        "{paused_listed}"
    );
    // The run that went on says which pause it continues.
    let resumed_listed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == case_done["resumed_run_ids"][0])
        .cloned()
        .expect("the resumed run is listed");
    assert_eq!(
        resumed_listed["metadata"]["approval_of"], case_done["run_id"],
        "{resumed_listed}"
    );
    // Its own page says so too: a panel waiting on it follows the decision.
    let (_, paused_page) = call(
        &app,
        "GET",
        &format!("/runs/{}", case_done["run_id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        paused_page["decision"]["status"], "approved",
        "{paused_page}"
    );
    assert_eq!(
        paused_page["decision"]["resumed_run_id"], case_done["resumed_run_ids"][0],
        "{paused_page}"
    );
    // Nobody was told: the decision was the evaluation's, not a person's.
    let (_, told) = call(&app, "GET", "/notices", None).await;
    let keys: Vec<String> = told["notices"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|n| n["key"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !keys
            .iter()
            .any(|k| k == &format!("approval:{}", case_done["run_id"].as_str().unwrap())),
        "an evaluation's own decision is no notice: {keys:?}"
    );
    // The effect landed in the world, and only there: the seed's one and
    // the case's one, after the reset that preceded the case. The row says
    // which run wrote it — the run the decision continued.
    let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(after["records"]["tickets"], 2, "{after}");
    assert_eq!(
        after["reset_count"], 2,
        "a reset before each of the two cases"
    );
    let written = after["tables"]["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["_written_by"]["run_id"].is_string())
        .cloned()
        .expect("the created row says which run wrote it");
    assert_eq!(
        written["_written_by"]["run_id"], lobby["resumed_run_ids"][0],
        "the last case's run: {written}"
    );
    assert!(
        after["tables"]["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["number"] == json!(1) && r["_written_by"].is_null()),
        "the seed's row was written by nobody"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A front desk that routes a facilities problem to the triage agent, and
/// the triage agent that files it: one model, two behaviours, told apart
/// by the message. The desk's message starts with `ROUTE:`.
struct Team;

#[async_trait::async_trait]
impl ChatModel for Team {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let asked = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let answered = messages.iter().filter(|m| m.role == ChatRole::Tool).count();
        let message = if let Some(rest) = asked.strip_prefix("ROUTE:") {
            match answered {
                0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "d1",
                    "agents.ask",
                    json!({"agent": "Facilities Triage", "message": rest.trim()}),
                )]),
                _ => {
                    let said = messages
                        .iter()
                        .rev()
                        .find(|m| m.role == ChatRole::Tool)
                        .and_then(|m| m.content.clone())
                        .unwrap_or_default();
                    ChatMessage::assistant(format!("The triage desk says: {said}"))
                }
            }
        } else {
            match answered {
                0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c1",
                    "facilities-desk.list-tickets",
                    json!({"status": "open"}),
                )]),
                1 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "c2",
                    "facilities-desk.create-ticket",
                    json!({"title": "Coffee machine broken", "room": "kitchen"}),
                )]),
                _ => {
                    let last = messages
                        .iter()
                        .rev()
                        .find(|m| m.role == ChatRole::Tool)
                        .and_then(|m| m.content.clone())
                        .unwrap_or_default();
                    ChatMessage::assistant(format!("Done: {last}"))
                }
            }
        };
        Ok(ChatResponse {
            message,
            model: Some("team".into()),
            usage: None,
        })
    }
}

/// An agent asked by another runs where the asking one runs and under what
/// it must not do: the front desk in a world, forbidden to file, asks the
/// triage agent — the triage agent reads from the world and its filing is
/// refused, so nothing reaches the live desk and the constraint holds
/// across the ask.
#[tokio::test]
async fn an_agent_asked_from_a_world_answers_in_that_world_and_under_the_askers_constraints() {
    let store =
        std::env::temp_dir().join(format!("rusty-server-worlds-ask-{}", uuid::Uuid::new_v4()));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools);
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk",
        "1",
        "Facilities Desk",
        "The facilities desk's tickets.",
        "https://facilities.example.internal/docs",
        "https://facilities.example.internal",
        spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Idempotent, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (status, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [{"id": 1, "number": 1, "title": "Projector flickers", "room": "2A", "status": "open"}], "me": [{"id": 1}]}, "counters": {"tickets": 2}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "front", "name": "Front Desk", "graph": "react", "config": {"instructions": "Route facilities problems to the triage agent.", "studio_intent": {"tools": [{"name": "agents.ask"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED);

    // The desk runs in the world, forbidden to file; it asks the triage agent.
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"assistant_id": "front", "input": {"messages": [{"role": "user", "content": "ROUTE: the kitchen coffee machine is broken"}]}, "config": {"world": "facilities-twin", "forbidden_tools": ["facilities-desk.create-ticket"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "success", "{run}");
    let said = run.to_string();
    assert!(said.contains("The triage desk says"), "{said}");

    // The asked run, named in the ask's answer: in the world, under the
    // same constraint.
    let asked_id = run["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok())
        .find_map(|v| v["run_id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("the ask names the run it started: {said}"));
    let (_, events) = call(&app, "GET", &format!("/runs/{asked_id}/events"), None).await;
    let declared = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("run_config_declared"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        declared["output"]["value"]["world"],
        json!(world_id),
        "the asked agent runs in the asker's world: {declared}"
    );
    let tool_results: Vec<String> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == json!("tool_call"))
        .map(|e| e["output"].to_string())
        .collect();
    assert!(
        tool_results
            .iter()
            .any(|r| r.contains("Projector flickers")),
        "the read was answered by the world: {tool_results:?}"
    );
    // The refusal is journaled as the guard's own event, not a tool call.
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.to_string().contains("forbidden_tools")),
        "the filing was refused under the asker's constraint: {events}"
    );
    assert!(
        said.contains("must not be called in this work"),
        "the asker hears the refusal: {said}"
    );
    let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(
        after["records"]["tickets"], 1,
        "nothing was filed, in the world or anywhere: {after}"
    );
    assert!(after["calls_since_reset"].as_u64().unwrap() >= 1, "{after}");
    let _ = std::fs::remove_dir_all(store);
}

/// Standing work rehearsed in the stand-in: a schedule put in a world
/// fires every run in it, and the schedule says so. A world nobody holds
/// makes no schedule.
#[tokio::test]
async fn a_schedule_in_a_world_fires_its_runs_in_that_world() {
    let store =
        std::env::temp_dir().join(format!("rusty-server-worlds-cron-{}", uuid::Uuid::new_v4()));
    let (app, _seen) = app_at(&store);
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(servicenow_manifest()).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    let instance_id = instance["instance_id"].as_str().unwrap().to_owned();
    let (status, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "sn-desk", "name": "Support Desk", "graph": "react", "config": {"instructions": "You look before you file.", "studio_intent": {"tools": [{"name": "servicenow.list-records"}, {"name": "servicenow.create-incident"}]}}}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, world) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-twin", "instance_id": instance_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();

    let (status, refused) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "sn-desk", "interval_secs": 1, "world": "no-such-world", "input": {"messages": [{"role": "user", "content": "raise it"}]}}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    let (status, cron) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "sn-desk", "interval_secs": 1, "world": "sn-twin", "input": {"messages": [{"role": "user", "content": "The fog machine in the unicorn stables will not start; please raise it."}]}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    assert_eq!(
        cron["world"],
        json!(world_id),
        "the schedule names its world by id: {cron}"
    );
    assert_eq!(cron["world_name"], json!("sn-twin"));
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();
    let mut fired: Option<Value> = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, runs) = call(&app, "GET", "/runs", None).await;
        let hit = runs
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["metadata"]["cron_id"] == json!(cron_id) && r["status"] == json!("success"))
            .cloned();
        if hit.is_some() {
            fired = hit;
            break;
        }
    }
    let _ = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    let run = fired.unwrap_or_else(|| panic!("the schedule never fired a run that finished"));
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, events) = call(&app, "GET", &format!("/runs/{run_id}/events"), None).await;
    let declared = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!("run_config_declared"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        declared["output"]["value"]["world"],
        json!(world_id),
        "the fired run acts in the world: {declared}"
    );
    let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert!(
        after["calls_since_reset"].as_u64().unwrap() >= 2,
        "the world answered the fired run: {after}"
    );
    assert!(
        after["records"]["incident"].as_u64().unwrap() >= 4,
        "the create landed in the world: {after}"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A queued task the front desk works delegates to the triage agent, which
/// pauses at the gate: the leased task says whom it waits on and before
/// which call — the board can point at the decision — and once decided
/// the task settles with the outcome.
#[tokio::test]
async fn a_leased_task_whose_delegated_run_paused_says_it_waits_on_the_person() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-task-wait-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools);
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk", "1", "Facilities Desk", "The facilities desk's tickets.", "https://facilities.example.internal/docs", "https://facilities.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [{"id": 1, "number": 1, "title": "Projector flickers", "room": "2A", "status": "open"}], "me": [{"id": 1}]}, "counters": {"tickets": 2}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "front", "name": "Front Desk", "graph": "react", "config": {"instructions": "Route facilities problems to the triage agent.", "studio_intent": {"tools": [{"name": "agents.ask"}], "pool": "front-desk"}}}))).await;

    let (status, task) = call(&app, "POST", "/tasks", Some(json!({"kind": "report", "pool": "front-desk", "payload": {"message": "ROUTE: the kitchen coffee machine is broken", "world": "facilities-twin"}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_id = task["task_id"].as_str().unwrap().to_owned();

    // The board: the task is leased and, once the triage agent pauses,
    // says it waits on the person before the filing.
    let mut waiting = Value::Null;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, list) = call(&app, "GET", "/tasks?status=leased", None).await;
        if let Some(t) = list
            .as_array()
            .and_then(|a| a.iter().find(|t| t["task_id"] == json!(task_id)))
        {
            if !t["waiting_on"].is_null() {
                waiting = t.clone();
                break;
            }
        }
    }
    assert_eq!(
        waiting["status"],
        json!("leased"),
        "the task waits on a decision while leased: {waiting}"
    );
    assert_eq!(
        waiting["run_id"],
        Value::Null,
        "the task's own run id is stamped at settlement: {waiting}"
    );
    assert_eq!(
        waiting["waiting_on"]["agent"],
        json!("Facilities Triage"),
        "{waiting}"
    );
    assert_eq!(
        waiting["waiting_on"]["tools"],
        json!(["facilities-desk.create-ticket"]),
        "{waiting}"
    );
    let paused = waiting["waiting_on"]["run_id"].as_str().unwrap().to_owned();
    let (_, pending) = call(&app, "GET", "/approvals?status=pending", None).await;
    assert_eq!(
        pending["approvals"][0]["run_id"],
        json!(paused),
        "the board points at the decision the Inbox holds: {pending}"
    );

    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{paused}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let mut done = Value::Null;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, t) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
        if matches!(t["status"].as_str(), Some("completed" | "failed" | "dead")) {
            done = t;
            break;
        }
    }
    assert_eq!(done["status"], json!("completed"), "{done}");
    assert!(
        done["result"]["reply"]
            .as_str()
            .unwrap_or("")
            .contains("Coffee machine broken"),
        "the outcome reached the task: {done}"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// The ask's patience runs out before the person decides: the front desk
/// answers that the filing is undecided, but the task it works is not
/// done — it stays leased, waiting on the person, until the decision;
/// then it settles with what the asked agent did.
#[tokio::test]
async fn a_task_is_not_done_while_a_decision_it_caused_is_pending() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-task-undecided-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools)
        .with_ask_wait(std::time::Duration::from_secs(2));
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk", "1", "Facilities Desk", "The facilities desk's tickets.", "https://facilities.example.internal/docs", "https://facilities.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [{"id": 1, "number": 1, "title": "Projector flickers", "room": "2A", "status": "open"}], "me": [{"id": 1}]}, "counters": {"tickets": 2}}}))).await;
    let world_id = world["world_id"].as_str().unwrap().to_owned();
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "front", "name": "Front Desk", "graph": "react", "config": {"instructions": "Route facilities problems to the triage agent.", "studio_intent": {"tools": [{"name": "agents.ask"}], "pool": "front-desk"}}}))).await;

    let (_, task) = call(&app, "POST", "/tasks", Some(json!({"kind": "report", "pool": "front-desk", "payload": {"message": "ROUTE: the kitchen coffee machine is broken", "world": "facilities-twin"}}))).await;
    let task_id = task["task_id"].as_str().unwrap().to_owned();

    // The ask gives up after two seconds; the front desk's run ends saying
    // so. Well after that, the task is still leased and waiting.
    let mut paused = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, pending) = call(&app, "GET", "/approvals?status=pending", None).await;
        if let Some(run) = pending["approvals"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a["run_id"].as_str())
        {
            paused = Some(run.to_owned());
            break;
        }
    }
    let paused = paused.expect("the triage agent paused before filing");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let (_, list) = call(&app, "GET", "/tasks?status=leased", None).await;
    let leased = list
        .as_array()
        .and_then(|a| a.iter().find(|t| t["task_id"] == json!(task_id)))
        .cloned()
        .unwrap_or_else(|| panic!("the task is still leased after the ask gave up: {list}"));
    assert_eq!(
        leased["waiting_on"]["run_id"],
        json!(paused),
        "and it waits on the delegated decision: {leased}"
    );
    assert_eq!(leased["waiting_on"]["agent"], json!("Facilities Triage"));

    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{paused}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let mut done = Value::Null;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, t) = call(&app, "GET", &format!("/tasks/{task_id}"), None).await;
        if matches!(t["status"].as_str(), Some("completed" | "failed" | "dead")) {
            done = t;
            break;
        }
    }
    assert_eq!(done["status"], json!("completed"), "{done}");
    let outcome = &done["result"]["delegated"][0];
    assert_eq!(
        outcome["agent"],
        json!("Facilities Triage"),
        "the task carries what the asked agent did: {done}"
    );
    assert_eq!(outcome["decided"], json!("approved"), "{done}");
    assert!(
        outcome["reply"]
            .as_str()
            .unwrap_or("")
            .contains("Coffee machine broken"),
        "{done}"
    );
    let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(
        after["records"]["tickets"], 2,
        "the filing landed once: {after}"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A schedule whose run pauses for a decision holds: it does not fire
/// again while that decision is pending, its row says so, and once
/// decided it fires on.
#[tokio::test]
async fn a_schedule_holds_while_a_run_it_fired_waits_on_a_decision() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-cron-hold-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools);
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk", "1", "Facilities Desk", "The facilities desk's tickets.", "https://facilities.example.internal/docs", "https://facilities.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, _) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [], "me": [{"id": 1}]}, "counters": {"tickets": 1}}}))).await;
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;

    let (status, cron) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "triage", "interval_secs": 1, "world": "facilities-twin", "input": {"messages": [{"role": "user", "content": "The kitchen coffee machine is broken; file it."}]}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();

    // The first firing pauses before the filing.
    let mut paused = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, pending) = call(&app, "GET", "/approvals?status=pending", None).await;
        if let Some(run) = pending["approvals"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a["run_id"].as_str())
        {
            paused = Some(run.to_owned());
            break;
        }
    }
    let paused = paused.expect("the scheduled run paused before filing");
    // Three more seconds — three more due times — and still one pause.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let (_, pending) = call(&app, "GET", "/approvals?status=pending", None).await;
    assert_eq!(
        pending["approvals"].as_array().map(Vec::len),
        Some(1),
        "the schedule held instead of piling up pauses: {pending}"
    );
    let (_, listed) = call(&app, "GET", "/crons", None).await;
    let row = listed
        .as_array()
        .and_then(|c| c.iter().find(|c| c["cron_id"] == json!(cron_id)))
        .cloned()
        .unwrap_or(listed.clone());
    assert_eq!(
        row["held"]["run_id"],
        json!(paused),
        "the row says which run it waits on: {row}"
    );
    assert_eq!(row["runs_fired"], json!(1), "{row}");

    // Decided, the schedule fires on.
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{paused}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let mut fired_on = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, listed) = call(&app, "GET", "/crons", None).await;
        let row = listed
            .as_array()
            .and_then(|c| c.iter().find(|c| c["cron_id"] == json!(cron_id)))
            .cloned();
        if row
            .as_ref()
            .is_some_and(|r| r["runs_fired"].as_u64() >= Some(2) && r["held"].is_null())
        {
            fired_on = true;
            break;
        }
    }
    let _ = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert!(
        fired_on,
        "once decided the schedule fired again and is no longer held"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A listed run says what it was asked — the person's first message, as a
/// line — so a picker can tell one run from another without opening it.
#[tokio::test]
async fn a_listed_run_says_what_it_was_asked() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-asked-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools);
    let app = router(registry, config);
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "front", "name": "Front Desk", "graph": "react", "config": {"instructions": "Be brief.", "studio_intent": {"tools": []}}}))).await;
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let asked = "   Is there a ticket for the fog machine\n in the unicorn stables?  ";
    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "front", "input": {"messages": [{"role": "user", "content": asked}]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (_, listed) = call(&app, "GET", "/runs", None).await;
    let mine = listed
        .as_array()
        .and_then(|l| l.iter().find(|r| r["run_id"] == json!(run_id)))
        .cloned()
        .expect("the run is listed");
    assert_eq!(
        mine["asked"],
        json!("Is there a ticket for the fog machine in the unicorn stables?"),
        "one line, the person's words: {mine}"
    );
    let _ = std::fs::remove_dir_all(store);
}

/// A webhook put in a world fires every run in it: a signed event from a
/// sender is worked against the stand-in, and the filing lands there, not
/// in the live system.
#[tokio::test]
async fn a_webhook_in_a_world_fires_its_runs_there() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-webhook-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools);
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk", "1", "Facilities Desk", "The facilities desk's tickets.", "https://facilities.example.internal/docs", "https://facilities.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [], "me": [{"id": 1}]}, "counters": {"tickets": 1}}}))).await;
    let world_id = world["world_id"].as_str().unwrap().to_owned();
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;

    let (status, refused) = call(&app, "POST", "/triggers", Some(json!({"name": "Sensor alerts", "target": {"kind": "assistant", "id": "triage"}, "action": "start_run", "input_template": {"messages": [{"role": "user", "content": "{{event.text}}"}]}, "world": "no-such-world"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let (status, hook) = call(&app, "POST", "/triggers", Some(json!({"name": "Sensor alerts", "target": {"kind": "assistant", "id": "triage"}, "action": "start_run", "input_template": {"messages": [{"role": "user", "content": "{{event.text}}"}]}, "world": "facilities-twin"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{hook}");
    assert_eq!(
        hook["world"],
        json!(world_id),
        "the webhook names its world by id: {hook}"
    );
    assert_eq!(hook["world_name"], json!("facilities-twin"));
    let trigger_id = hook["trigger_id"].as_str().unwrap().to_owned();
    let secret = hook["secret"].as_str().unwrap().to_owned();

    // A signed event from the sender.
    let body = json!({"text": "The kitchen coffee machine is broken; file it."}).to_string();
    let signature = {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256={hex}")
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/triggers/{trigger_id}/webhook"))
                .header("content-type", "application/json")
                .header("x-rusty-signature", signature)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes: Bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let accepted: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");

    // The run pauses before the filing (the world does not lift the gate),
    // is approved, and the ticket lands in the world.
    let mut paused = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, pending) = call(&app, "GET", "/approvals?status=pending", None).await;
        if let Some(a) = pending["approvals"].as_array().and_then(|a| a.first()) {
            assert_eq!(
                a["world"]["name"],
                json!("facilities-twin"),
                "the pause says which world: {a}"
            );
            paused = Some(a["run_id"].as_str().unwrap().to_owned());
            break;
        }
    }
    let paused = paused.expect("the webhook's run paused before filing");
    let (status, decided) = call(
        &app,
        "POST",
        &format!("/approvals/{paused}/decide"),
        Some(json!({"decision": "approve"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    let mut filed = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
        if after["records"]["tickets"] == json!(1) {
            filed = true;
            break;
        }
    }
    assert!(filed, "the filing landed in the world");
    let (_, listed) = call(&app, "GET", "/runs", None).await;
    let mine = listed
        .as_array()
        .and_then(|l| l.iter().find(|r| r["run_id"] == json!(paused)))
        .cloned()
        .expect("listed");
    assert_eq!(mine["metadata"]["trigger_id"], json!(trigger_id), "{mine}");
    let _ = std::fs::remove_dir_all(store);
}

/// The seed a world of a connector starts from is made from the
/// connector's own operations: one example record per resource, the
/// fields the create operation takes, a counter for the next one.
#[tokio::test]
async fn a_world_starter_comes_from_the_connectors_own_operations() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-starter-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools);
    let app = router(registry, config);
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk", "1", "Facilities Desk", "The facilities desk's tickets.", "https://facilities.example.internal/docs", "https://facilities.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}, "priority": {"type": "integer"}, "urgent": {"type": "boolean"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, _) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    let (status, starter) = call(
        &app,
        "GET",
        "/worlds/starter?connector=facilities-desk",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{starter}");
    assert_eq!(starter["dialect"], json!("manifest-rest"));
    let ticket = &starter["starter"]["tables"]["tickets"][0];
    assert_eq!(ticket["title"], json!("Example title"), "{starter}");
    assert_eq!(ticket["room"], json!("Example room"));
    assert_eq!(ticket["priority"], json!(1));
    assert_eq!(ticket["urgent"], json!(false));
    assert_eq!(ticket["number"], json!(1));
    assert_eq!(starter["starter"]["counters"]["tickets"], json!(2));
    assert!(
        starter["starter"]["tables"]["me"].is_array(),
        "a read-only resource gets a table too: {starter}"
    );
    let (status, none) = call(&app, "GET", "/worlds/starter?connector=no-such", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{none}");
    let (status, neither) = call(&app, "GET", "/worlds/starter", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{neither}");
    // And a world made from the library with that starter has the record.
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": starter["starter"]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    assert_eq!(world["records"]["tickets"], json!(1), "{world}");
    let _ = std::fs::remove_dir_all(store);
}

/// A world made from the library's Slack connector speaks the Slack Web
/// API: channels to list, a message to post, the post read back.
#[tokio::test]
async fn a_slack_world_speaks_the_web_api() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-slack-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools);
    let app = router(registry, config);
    let manifest: Value =
        serde_json::from_str(include_str!("../../catalog/slack-connector/manifest.json"))
            .expect("the library's Slack manifest");
    let (status, receipt) = call(&app, "POST", "/connectors", Some(manifest)).await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let (status, starter) = call(&app, "GET", "/worlds/starter?connector=slack", None).await;
    assert_eq!(status, StatusCode::OK, "{starter}");
    assert_eq!(starter["dialect"], json!("slack-web"), "{starter}");
    assert_eq!(
        starter["starter"]["tables"]["channels"][0]["name"],
        json!("general")
    );
    let (status, world) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "slack-twin", "connector": "slack"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    assert_eq!(world["dialect"], json!("slack-web"), "{world}");
    assert_eq!(world["stands_for"], json!("slack.com"));
    assert_eq!(world["records"]["channels"], json!(2), "{world}");
    assert_eq!(world["records"]["messages"], json!(1));
    let _ = std::fs::remove_dir_all(store);
}

/// A morning brief: read one system, post to another. The run is put in
/// two worlds — one per system — and each call finds its own stand-in by
/// the host it addresses; nothing reaches the wire.
struct Brief;

#[async_trait::async_trait]
impl ChatModel for Brief {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        let answered = messages.iter().filter(|m| m.role == ChatRole::Tool).count();
        let message = match answered {
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "b1",
                "servicenow.list-records",
                json!({"table": "incident", "sysparm_limit": 10}),
            )]),
            1 => {
                let n = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == ChatRole::Tool)
                    .and_then(|m| m.content.as_deref())
                    .and_then(|c| serde_json::from_str::<Value>(c).ok())
                    .and_then(|v| v["result"].as_array().map(Vec::len))
                    .unwrap_or(0);
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "b2",
                    "chatter.post",
                    json!({"room": "ops", "text": format!("Morning brief: {n} incidents on the books.")}),
                )])
            }
            _ => ChatMessage::assistant("Posted the brief."),
        };
        Ok(ChatResponse {
            message,
            model: Some("brief".into()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn a_run_in_two_worlds_reads_one_and_posts_to_the_other() {
    let store =
        std::env::temp_dir().join(format!("rusty-server-worlds-two-{}", uuid::Uuid::new_v4()));
    let model: Arc<dyn ChatModel> = Arc::new(Brief);
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools);
    let app = router(registry, config);

    // ServiceNow, connected, with its twin.
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(servicenow_manifest()).unwrap()),
    )
    .await;
    let sn_hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": sn_hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    let sn_instance = instance["instance_id"].as_str().unwrap().to_owned();
    let (status, sn_twin) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-twin", "instance_id": sn_instance})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sn_twin}");
    let sn_twin_id = sn_twin["world_id"].as_str().unwrap().to_owned();

    // A chat system, connected, with its twin in the connector's own dialect.
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let chatter = ConnectorManifest::new(
        "chatter", "1", "Chatter", "A chat system's rooms.", "https://chat.example.internal/docs", "https://chat.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("post", HttpMethod::Post, "/rooms/{room}/messages", OperationEffect::Idempotent, json!({"type": "object", "required": ["room", "text"], "properties": {"room": {"type": "string"}, "text": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&chatter).unwrap()),
    )
    .await;
    let chat_hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": chat_hash, "config": {"token": "any"}})),
    )
    .await;
    let (status, chat_twin) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "chat-twin", "connector": "chatter", "dialect": "manifest-rest"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{chat_twin}");
    let chat_twin_id = chat_twin["world_id"].as_str().unwrap().to_owned();

    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "briefer", "name": "Morning Brief", "graph": "react", "config": {"instructions": "Read, then post.", "studio_intent": {"tools": [{"name": "servicenow.list-records"}, {"name": "chatter.post"}]}}}))).await;
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();

    // A world nobody holds refuses the run before it starts.
    let (status, refused) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "briefer", "input": {"messages": [{"role": "user", "content": "brief"}]}, "config": {"worlds": ["sn-twin", "no-such-twin"]}}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    let (status, run) = call(&app, "POST", &format!("/threads/{thread_id}/runs/wait"), Some(json!({"assistant_id": "briefer", "input": {"messages": [{"role": "user", "content": "brief"}]}, "config": {"worlds": ["sn-twin", "chat-twin"]}}))).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], json!("success"), "{run}");
    let said = run.to_string();
    assert!(
        said.contains("Morning brief: 3 incidents on the books."),
        "the brief counted the twin's incidents: {said}"
    );
    // Each system's twin answered its own calls.
    let (_, sn_after) = call(&app, "GET", &format!("/worlds/{sn_twin_id}"), None).await;
    assert_eq!(
        sn_after["calls_since_reset"],
        json!(1),
        "the read went to the ServiceNow twin: {sn_after}"
    );
    let (_, chat_after) = call(&app, "GET", &format!("/worlds/{chat_twin_id}"), None).await;
    assert_eq!(
        chat_after["calls_since_reset"],
        json!(1),
        "the post went to the chat twin: {chat_after}"
    );
    assert_eq!(chat_after["records"]["messages"], json!(1), "{chat_after}");
    let posted = chat_after["tables"]["messages"][0].clone();
    assert_eq!(
        posted["room"],
        json!("ops"),
        "the path's parameter is a field: {posted}"
    );
    assert_eq!(
        posted["_written_by"]["run_id"], run["run_id"],
        "the row says which run wrote it: {posted}"
    );
    // The run's record names both connections, each with the one call
    // its twin answered.
    let run_id = run["run_id"].as_str().unwrap();
    let (_, detail) = call(&app, "GET", &format!("/runs/{run_id}"), None).await;
    let connections = detail["connections"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(connections.len(), 2, "{detail}");
    assert!(
        connections
            .iter()
            .all(|c| c["tools"][0]["calls"] == json!(1)),
        "{detail}"
    );
    // And it says which worlds it was in, by name — on its page and in the list.
    assert_eq!(
        detail["worlds"],
        json!([{"world_id": sn_twin_id, "name": "sn-twin"}, {"world_id": chat_twin_id, "name": "chat-twin"}]),
        "{detail}"
    );
    let (_, listed) = call(&app, "GET", "/runs", None).await;
    let mine = listed
        .as_array()
        .and_then(|l| l.iter().find(|r| r["run_id"] == json!(run_id)))
        .cloned()
        .expect("listed");
    assert_eq!(mine["worlds"][1]["name"], json!("chat-twin"), "{mine}");
    let _ = std::fs::remove_dir_all(store);
}

/// The things that start runs on their own take several worlds too: a
/// schedule and a webhook in two worlds fire runs that act in both; a task
/// and a delegation keep the list for the runs they will start.
#[tokio::test]
async fn standing_things_take_several_worlds() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-standing-two-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Brief);
    let connection_tools = ConnectionTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools);
    let app = router(registry, config);

    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(servicenow_manifest()).unwrap()),
    )
    .await;
    let sn_hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, instance) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": sn_hash, "config": {"instance": "dev-twin"}})),
    )
    .await;
    let sn_instance = instance["instance_id"].as_str().unwrap().to_owned();
    let (_, sn_twin) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "sn-twin", "instance_id": sn_instance})),
    )
    .await;
    let sn_twin_id = sn_twin["world_id"].as_str().unwrap().to_owned();
    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let chatter = ConnectorManifest::new(
        "chatter", "1", "Chatter", "A chat system's rooms.", "https://chat.example.internal/docs", "https://chat.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("post", HttpMethod::Post, "/rooms/{room}/messages", OperationEffect::Idempotent, json!({"type": "object", "required": ["room", "text"], "properties": {"room": {"type": "string"}, "text": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&chatter).unwrap()),
    )
    .await;
    let chat_hash = receipt["hash"].as_str().unwrap().to_owned();
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": chat_hash, "config": {"token": "any"}})),
    )
    .await;
    let (_, chat_twin) = call(
        &app,
        "POST",
        "/worlds",
        Some(json!({"name": "chat-twin", "connector": "chatter", "dialect": "manifest-rest"})),
    )
    .await;
    let chat_twin_id = chat_twin["world_id"].as_str().unwrap().to_owned();
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "briefer", "name": "Morning Brief", "graph": "react", "config": {"instructions": "Read, then post.", "studio_intent": {"tools": [{"name": "servicenow.list-records"}, {"name": "chatter.post"}], "pool": "briefs"}}}))).await;

    // A schedule in two worlds: refused for a world nobody holds, made for
    // two, and its first firing posts the brief to the chat twin.
    let (status, refused) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "briefer", "interval_secs": 1, "worlds": ["sn-twin", "no-such"], "input": {"messages": [{"role": "user", "content": "brief"}]}}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let (status, cron) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "briefer", "interval_secs": 1, "worlds": ["sn-twin", "chat-twin"], "input": {"messages": [{"role": "user", "content": "brief"}]}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    assert_eq!(
        cron["world"],
        json!(sn_twin_id),
        "the first is the schedule's world: {cron}"
    );
    assert_eq!(cron["worlds"], json!([sn_twin_id, chat_twin_id]));
    assert_eq!(cron["world_names"], json!(["sn-twin", "chat-twin"]));
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();
    let mut posted = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, after) = call(&app, "GET", &format!("/worlds/{chat_twin_id}"), None).await;
        if after["records"]["messages"].as_u64().unwrap_or(0) >= 1 {
            posted = true;
            break;
        }
    }
    let _ = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    assert!(posted, "the scheduled brief landed in the chat twin");
    let (_, sn_after) = call(&app, "GET", &format!("/worlds/{sn_twin_id}"), None).await;
    assert!(
        sn_after["calls_since_reset"].as_u64().unwrap_or(0) >= 1,
        "and read the ServiceNow twin: {sn_after}"
    );

    // A webhook in two worlds keeps the list.
    let (status, hook) = call(&app, "POST", "/triggers", Some(json!({"name": "Brief on demand", "target": {"kind": "assistant", "id": "briefer"}, "action": "start_run", "input_template": {"messages": [{"role": "user", "content": "{{event.text}}"}]}, "worlds": ["sn-twin", "chat-twin"]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{hook}");
    assert_eq!(hook["worlds"], json!([sn_twin_id, chat_twin_id]), "{hook}");
    assert_eq!(hook["world"], json!(sn_twin_id));
    // A task and a delegation keep it for the runs they will start.
    let (status, task) = call(&app, "POST", "/tasks", Some(json!({"kind": "brief", "pool": "elsewhere", "payload": {"message": "brief", "worlds": ["sn-twin", "chat-twin"]}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let (_, task) = call(
        &app,
        "GET",
        &format!("/tasks/{}", task["task_id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(task["payload"]["world"], json!(sn_twin_id), "{task}");
    assert_eq!(task["payload"]["worlds"], json!([sn_twin_id, chat_twin_id]));
    assert_eq!(
        task["payload"]["world_names"],
        json!(["sn-twin", "chat-twin"])
    );
    let (status, delegated) = call(&app, "POST", "/assignments", Some(json!({"assistant_id": "briefer", "request": "Post the brief.", "max_rounds": 1, "worlds": ["sn-twin", "chat-twin"]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{delegated}");
    assert_eq!(
        delegated["worlds"],
        json!([sn_twin_id, chat_twin_id]),
        "{delegated}"
    );
    assert_eq!(delegated["world_name"], json!("sn-twin"));
    let _ = call(
        &app,
        "POST",
        &format!(
            "/assignments/{}/cancel",
            delegated["assignment_id"].as_str().unwrap()
        ),
        Some(json!({})),
    )
    .await;
    let _ = std::fs::remove_dir_all(store);
}

/// A schedule whose agent is archived cannot fire: its row says so, the
/// person who set it going is told once, and nothing is fired in silence.
#[tokio::test]
async fn a_schedule_whose_agent_is_archived_says_so_and_tells_its_maker() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-cron-stalled-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&rusty_agent_server::PlatformTools::new())
        as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let app = router(
        registry,
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone()),
    );
    let (_, made) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "front", "name": "Front Desk", "graph": "react", "config": {"instructions": "Be brief.", "studio_intent": {"tools": []}}}))).await;
    let (status, cron) = call(&app, "POST", "/crons", Some(json!({"assistant_id": "front", "interval_secs": 1, "input": {"messages": [{"role": "user", "content": "brief"}]}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{cron}");
    let cron_id = cron["cron_id"].as_str().unwrap().to_owned();
    let (status, archived) = call(
        &app,
        "POST",
        "/assistants/front/archive",
        Some(json!({"expected_active_version_id": made["active_version_id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{archived}");
    let mut row = Value::Null;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, listed) = call(&app, "GET", "/crons", None).await;
        if let Some(c) = listed
            .as_array()
            .and_then(|a| a.iter().find(|c| c["cron_id"] == json!(cron_id)))
        {
            if c["stalled"].is_string() {
                row = c.clone();
                break;
            }
        }
    }
    assert_eq!(
        row["stalled"],
        json!("its agent Front Desk is archived"),
        "the row says why: {row}"
    );
    let (_, notices) = call(&app, "GET", "/notices", None).await;
    let told = notices["notices"]
        .as_array()
        .and_then(|a| a.iter().find(|n| n["about"]["kind"] == json!("schedule")))
        .cloned()
        .expect("the maker is told");
    assert_eq!(
        told["title"],
        json!("Your schedule cannot run: its agent Front Desk is archived"),
        "{told}"
    );
    // Told once, however many due times pass.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (_, notices) = call(&app, "GET", "/notices", None).await;
    assert_eq!(
        notices["notices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["about"]["kind"] == json!("schedule"))
            .count(),
        1,
        "{notices}"
    );
    let _ = call(&app, "DELETE", &format!("/crons/{cron_id}"), None).await;
    let _ = std::fs::remove_dir_all(store);
}

/// An asked agent that must file (irreversible) pauses at the gate; the
/// ask waits for the person's decision and answers the asker with the
/// outcome — the asker's reply says what was filed, not that it paused.
#[tokio::test]
async fn an_ask_waits_for_the_decision_and_answers_with_the_outcome() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-worlds-ask-gate-{}",
        uuid::Uuid::new_v4()
    ));
    let model: Arc<dyn ChatModel> = Arc::new(Team);
    let connection_tools = ConnectionTools::new();
    let platform_tools = rusty_agent_server::PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&connection_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn rusty_agent_runtime::tool::ToolSource>);
    let graph = create_react_agent(model, tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_connection_tools(connection_tools)
        .with_platform_tools(platform_tools);
    let app = router(registry, config);

    let spec = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "required": ["token"], "properties": {"token": {"type": "string"}}, "additionalProperties": false});
    let op =
        |name: &str, method: HttpMethod, path: &str, effect: OperationEffect, params: Value| {
            ConnectorOperation {
                name: name.to_owned(),
                description: format!("The {name} operation."),
                method,
                path: path.to_owned(),
                effect,
                params_schema: params,
                headers: Vec::new(),
                auth: Vec::new(),
                max_response_bytes: None,
                reconcile: None,
            }
        };
    let manifest = ConnectorManifest::new(
        "facilities-desk", "1", "Facilities Desk", "The facilities desk's tickets.", "https://facilities.example.internal/docs", "https://facilities.example.internal", spec,
        vec![
            op("whoami", HttpMethod::Get, "/me", OperationEffect::ReadOnly, json!({"type": "object"})),
            op("list-tickets", HttpMethod::Get, "/tickets", OperationEffect::ReadOnly, json!({"type": "object", "properties": {"status": {"type": "string"}}})),
            op("create-ticket", HttpMethod::Post, "/tickets", OperationEffect::Irreversible, json!({"type": "object", "required": ["title"], "properties": {"title": {"type": "string"}, "room": {"type": "string"}}})),
        ],
        "whoami",
    )
    .expect("the manifest validates");
    let (_, receipt) = call(
        &app,
        "POST",
        "/connectors",
        Some(serde_json::to_value(&manifest).unwrap()),
    )
    .await;
    let hash = receipt["hash"].as_str().unwrap().to_owned();
    let (status, world) = call(&app, "POST", "/worlds", Some(json!({"name": "facilities-twin", "connector": "facilities-desk", "dialect": "manifest-rest", "seed": {"tables": {"tickets": [{"id": 1, "number": 1, "title": "Projector flickers", "room": "2A", "status": "open"}], "me": [{"id": 1}]}, "counters": {"tickets": 2}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{world}");
    let world_id = world["world_id"].as_str().unwrap().to_owned();
    let (_, _) = call(
        &app,
        "POST",
        "/connectors/instances",
        Some(json!({"manifest_hash": hash, "config": {"token": "any"}})),
    )
    .await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "triage", "name": "Facilities Triage", "graph": "react", "config": {"instructions": "Read before you file.", "studio_intent": {"tools": [{"name": "facilities-desk.list-tickets"}, {"name": "facilities-desk.create-ticket"}]}}}))).await;
    let (_, _) = call(&app, "POST", "/assistants", Some(json!({"assistant_id": "front", "name": "Front Desk", "graph": "react", "config": {"instructions": "Route facilities problems to the triage agent.", "studio_intent": {"tools": [{"name": "agents.ask"}]}}}))).await;

    // The person, deciding from the Inbox while the front desk waits.
    let deciding = {
        let app = app.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let (_, list) = call(&app, "GET", "/approvals?status=pending", None).await;
                if let Some(run_id) = list["approvals"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|a| a["run_id"].as_str())
                {
                    let (status, decided) = call(
                        &app,
                        "POST",
                        &format!("/approvals/{run_id}/decide"),
                        Some(json!({"decision": "approve"})),
                    )
                    .await;
                    return (status, decided);
                }
            }
            panic!("nothing paused for a decision");
        })
    };
    let (_, thread) = call(&app, "POST", "/threads", Some(json!({"graph": "react"}))).await;
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let (status, run) = call(
        &app,
        "POST",
        &format!("/threads/{thread_id}/runs/wait"),
        Some(json!({"assistant_id": "front", "input": {"messages": [{"role": "user", "content": "ROUTE: the kitchen coffee machine is broken"}]}, "config": {"world": "facilities-twin"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let (decided_status, decided) = deciding.await.unwrap();
    assert_eq!(decided_status, StatusCode::OK, "{decided}");
    assert_eq!(run["status"], "success", "{run}");
    let ask: Value = run["output"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok())
        .find(|v| v["asked"] == json!(true))
        .expect("the ask's answer");
    assert_eq!(
        ask["decided"],
        json!("approved"),
        "the ask waited for the decision: {ask}"
    );
    assert_eq!(ask["interrupted"], json!(false), "{ask}");
    assert!(ask["resumed_run_id"].is_string(), "{ask}");
    assert!(
        ask["reply"].as_str().unwrap_or("").contains("Done"),
        "the asker hears the outcome: {ask}"
    );
    assert!(ask["note"].is_null(), "{ask}");
    // The scripted desk quotes the whole answer; the outcome is in it.
    let said = run.to_string();
    assert!(
        said.contains("The triage desk says") && said.contains("Coffee machine broken"),
        "{said}"
    );
    let (_, after) = call(&app, "GET", &format!("/worlds/{world_id}"), None).await;
    assert_eq!(
        after["records"]["tickets"], 2,
        "the filing landed in the world once: {after}"
    );
    let _ = std::fs::remove_dir_all(store);
}
