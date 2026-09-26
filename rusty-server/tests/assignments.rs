//! Durable assignments — an outcome a person delegates and comes back to.
//! Two rounds through the real run path: the agent reads the ASSIGNMENT
//! checkpoint, records its progress through `assignment.progress`, the
//! driver starts the next round from that record, and the agent finishes
//! it. Then the record outlives a restart; and a round the restart caught
//! running is closed from its journal and the work continues.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::Role as ChatRole;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse, ToolCall};
use rusty_agent_runtime::react::create_react_agent;
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::{ToolRegistry, ToolSource};
use rusty_agent_server::{GraphRegistry, PlatformTools, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

type Seen = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

/// An investigator that reads the ASSIGNMENT block it is given: round one
/// counts and records a next step; any later round finishes and says so.
struct Investigator {
    seen: Seen,
}

fn system_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .filter(|m| m.role == ChatRole::System)
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait::async_trait]
impl ChatModel for Investigator {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let system = system_text(messages);
        let id = system
            .split("ASSIGNMENT ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .unwrap_or("")
            .to_owned();
        let round: u64 = system
            .split("— round ")
            .nth(1)
            .map(|s| {
                s.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        let recorded = messages.iter().any(|m| m.role == ChatRole::Tool);
        let asked_to_record = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .and_then(|m| m.content.as_deref())
            .is_some_and(|c| c.starts_with("Record your progress now"));
        // Round one answers in prose and never records by itself — the
        // closing turn has to ask; later rounds record first.
        let message = if round == 1 && !asked_to_record && !recorded {
            ChatMessage::assistant(
                "Round one: 7 P1 incidents this week against 2 last week. Next I group them by category.",
            )
        } else if !recorded {
            let args = if round == 1 {
                json!({"assignment_id": id, "done": ["counted 7 P1 incidents this week, 2 last week"], "unresolved": ["whether the 7 share a cause"], "next_step": "group the 7 by category and assignment group"})
            } else {
                json!({"assignment_id": id, "done": ["counted 7 P1 incidents this week, 2 last week", "5 of 7 are Network/VPN, all opened after the 09:00 outage"], "unresolved": [], "next_step": "", "complete": true})
            };
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                format!("c{round}"),
                "assignment.progress",
                args,
            )])
        } else if round == 1 {
            ChatMessage::assistant("Recorded.")
        } else {
            ChatMessage::assistant(
                "The spike is the 09:00 VPN outage: 5 of the 7 P1s are Network/VPN opened after it. Done.",
            )
        };
        Ok(ChatResponse {
            message,
            model: Some("investigator".into()),
            usage: None,
        })
    }
}

fn app_at(store: &std::path::Path, seen: &Seen) -> (Router, Seen) {
    let seen = Arc::clone(seen);
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(
        Arc::new(Investigator {
            seen: Arc::clone(&seen),
        }),
        tools.clone(),
    )
    .unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
        .with_platform_tools(platform_tools);
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

async fn settled(app: &Router, id: &str, rounds: usize) -> Value {
    let mut last = Value::Null;
    for _ in 0..1200 {
        let (status, a) = call(app, "GET", &format!("/assignments/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{a}");
        let done = a["rounds"]
            .as_array()
            .map(|r| r.len() >= rounds && r.iter().all(|x| x["status"] != "running"))
            .unwrap_or(false);
        if done && a["state"] != "working" {
            return a;
        }
        last = a;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("assignment {id} never settled: {last}");
}

async fn make_agent(app: &Router) {
    let (status, made) = call(
        app,
        "POST",
        "/assistants",
        // An allow-listed agent, as every desk built in the studio is: its
        // own tools only. The round must still hand it the progress door.
        Some(json!({"assistant_id": "investigator", "name": "Incident Investigator", "graph": "react_agent", "config": {"instructions": "You investigate incidents with numbers.", "studio_intent": {"tools": [{"name": "catalog.tools"}]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
}

#[tokio::test]
async fn a_delegated_investigation_runs_in_rounds_from_its_own_record_and_outlives_a_restart() {
    let store =
        std::env::temp_dir().join(format!("rusty-server-assignments-{}", uuid::Uuid::new_v4()));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (app, seen) = app_at(&store, &seen);
    make_agent(&app).await;

    let (status, a) = call(
        &app,
        "POST",
        "/assignments",
        Some(json!({"assistant_id": "investigator", "request": "Investigate why P1 incidents spiked this week and say whether they share a cause.", "success": "a cause, with the numbers behind it", "max_rounds": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{a}");
    let id = a["assignment_id"].as_str().unwrap().to_owned();
    assert_eq!(a["state"], "working");
    assert_eq!(a["rounds"][0]["round"], 1);
    assert_eq!(a["rounds"][0]["status"], "running");
    assert_eq!(a["goal"]["phase"], "active");
    assert_eq!(a["goal"]["provenance"]["type"], "operator");

    let a = settled(&app, &id, 2).await;
    assert_eq!(a["state"], "done", "{a}");
    assert_eq!(a["rounds_used"], 2);
    assert_eq!(a["rounds"][0]["status"], "success");
    assert_eq!(a["rounds"][1]["status"], "success");
    assert!(
        a["rounds"][0]["summary"].as_str().unwrap().contains("7 P1"),
        "{a}"
    );
    assert!(
        a["rounds"][1]["summary"].as_str().unwrap().contains("VPN"),
        "{a}"
    );
    assert!(
        a["rounds"][0]["closing_run_id"].is_string(),
        "round one answered without recording; the closing turn asked: {a}"
    );
    assert_eq!(
        a["rounds"][0]["actions"][0]["tool"], "assignment.progress",
        "the record's actions come from the journal, the closing turn's too: {a}"
    );
    assert!(
        a["rounds"][1]["closing_run_id"].is_null(),
        "round two recorded by itself: {a}"
    );
    assert_eq!(a["progress"]["complete"], true);
    assert_eq!(a["progress"]["done"].as_array().unwrap().len(), 2);
    assert_eq!(a["goal"]["phase"], "complete");
    assert_eq!(
        a["request"],
        "Investigate why P1 incidents spiked this week and say whether they share a cause."
    );

    // Round two started from round one's record, not from scratch.
    let rounds: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|m| system_text(m))
        .collect();
    let second = rounds
        .iter()
        .find(|s| s.contains("— round 2;"))
        .expect("a second round was asked");
    assert!(second.contains("counted 7 P1 incidents"), "{second}");
    assert!(
        second.contains("group the 7 by category"),
        "the next step carried over: {second}"
    );
    assert!(
        second.contains("round 1: assignment.progress"),
        "the actions already taken are listed: {second}"
    );
    assert!(
        second.contains("You investigate incidents with numbers."),
        "the charter follows the checkpoint: {second}"
    );
    let (_, list) = call(&app, "GET", "/assignments", None).await;
    assert_eq!(list["assignments"][0]["assignment_id"], json!(id));

    // A restart: the record is what it was.
    drop(app);
    let (app, _) = app_at(&store, &seen);
    let (status, again) = call(&app, "GET", &format!("/assignments/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["state"], "done");
    assert_eq!(again["rounds"].as_array().unwrap().len(), 2);
    assert_eq!(again["progress"]["done"], a["progress"]["done"]);

    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn a_round_a_restart_caught_running_is_closed_from_its_journal_and_the_work_continues() {
    let store = std::env::temp_dir().join(format!(
        "rusty-server-assignments-restart-{}",
        uuid::Uuid::new_v4()
    ));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (app, _) = app_at(&store, &seen);
    make_agent(&app).await;
    let (_, a) = call(
        &app,
        "POST",
        "/assignments",
        Some(json!({"assistant_id": "investigator", "request": "Investigate why P1 incidents spiked this week.", "max_rounds": 3})),
    )
    .await;
    let id = a["assignment_id"].as_str().unwrap().to_owned();
    // Let the assignment run to its end (so nothing writes the record any
    // more), then rewrite it as a restart would find it: round two marked
    // running, its run gone with the process.
    let mut a = Value::Null;
    for _ in 0..1200 {
        let (_, now) = call(&app, "GET", &format!("/assignments/{id}"), None).await;
        if now["rounds"]
            .as_array()
            .map(|r| r.len() >= 2 && r.iter().all(|x| x["status"] != "running"))
            .unwrap_or(false)
            && now["state"] != "working"
        {
            a = now;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(app);
    let path = store.join("assignments").join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["state"] = json!("working");
    record["state_reason"] = Value::Null;
    record["progress"]["complete"] = json!(false);
    record["progress"]["next_step"] = json!("group the 7 by category and assignment group");
    let rounds = record["rounds"].as_array_mut().unwrap();
    rounds.truncate(2);
    rounds[1]["status"] = json!("running");
    rounds[1]["run_id"] = json!("run-lost-with-the-process");
    rounds[1]["ended_at"] = Value::Null;
    std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    let _ = a;

    // Boot: the lost round is closed as a restart and the next one runs.
    seen.lock().unwrap().clear();
    let (app, seen) = app_at(&store, &seen);
    let mut settled_record = Value::Null;
    for _ in 0..1200 {
        let (_, now) = call(&app, "GET", &format!("/assignments/{id}"), None).await;
        if now["rounds"]
            .as_array()
            .map(|r| r.len() >= 3 && r[2]["status"] != "running")
            .unwrap_or(false)
        {
            settled_record = now;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        settled_record["rounds"][1]["status"], "restart",
        "{settled_record}"
    );
    assert!(
        settled_record["rounds"][1]["error"]
            .as_str()
            .unwrap()
            .contains("restarted")
    );
    assert_eq!(settled_record["rounds"][2]["status"], "success");
    assert_eq!(settled_record["state"], "done", "{settled_record}");
    let asked: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|m| system_text(m))
        .collect();
    assert!(
        asked
            .iter()
            .any(|s| s.contains("— round 3;") && s.contains("group the 7 by category")),
        "the continued round carried the record: {asked:?}"
    );

    let _ = std::fs::remove_dir_all(store);
}

// ── A round that reads and reads without learning anything ends early ──────

/// A read that answers the same whatever it is asked: the world as a desk
/// sees it while it polls a record that is not changing.
struct SameAnswer;

#[async_trait::async_trait]
impl rusty_agent_runtime::tool::Tool for SameAnswer {
    fn name(&self) -> &str {
        "record.read"
    }
    fn description(&self) -> &str {
        "Reads a record."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }
    fn effect(&self) -> rusty_agent_runtime::record::Effect {
        rusty_agent_runtime::record::Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        Ok(json!({"number": "INC0010104", "state": "New"}))
    }
}

/// A desk that polls: round one asks the record a fresh question every turn
/// and never records; once steered, it records what it waits for and stops.
struct Poller {
    seen: Seen,
}

#[async_trait::async_trait]
impl ChatModel for Poller {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> RustyResult<ChatResponse> {
        self.seen.lock().unwrap().push(messages.to_vec());
        let system = system_text(messages);
        let id = system
            .split("ASSIGNMENT ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .unwrap_or("")
            .to_owned();
        let round: u64 = system
            .split("— round ")
            .nth(1)
            .map(|s| {
                s.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        let answered = messages.iter().filter(|m| m.role == ChatRole::Tool).count();
        let asked_to_record = messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .and_then(|m| m.content.as_deref())
            .is_some_and(|c| c.starts_with("Record your progress now"));
        let message = if round == 1 && !asked_to_record {
            // A different query each time — never the same call twice — and
            // the same answer every time.
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                format!("q{answered}"),
                "record.read",
                json!({"query": format!("INC0010104 check {answered}")}),
            )])
        } else if answered == 0 || (round == 1 && asked_to_record) {
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                format!("p{round}"),
                "assignment.progress",
                json!({"assignment_id": id, "done": ["INC0010104 is New, unassigned"], "unresolved": [], "next_step": "", "complete": true}),
            )])
        } else {
            ChatMessage::assistant(
                "INC0010104 is still New and unassigned; recorded, nothing more to read until it changes.",
            )
        };
        Ok(ChatResponse {
            message,
            model: Some("poller".into()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn a_round_whose_reads_answer_nothing_new_stops_early_and_the_next_round_is_steered_with_the_reason()
 {
    let store =
        std::env::temp_dir().join(format!("rusty-server-assignments-{}", uuid::Uuid::new_v4()));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let platform_tools = PlatformTools::new();
    let mut tools = ToolRegistry::new();
    tools.register(SameAnswer);
    tools.attach(Arc::clone(&platform_tools) as Arc<dyn ToolSource>);
    let graph = create_react_agent(
        Arc::new(Poller {
            seen: Arc::clone(&seen),
        }),
        tools.clone(),
    )
    .unwrap();
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_platform_tools(platform_tools);
    let app = router(registry, config);
    let (status, made) = call(
        &app,
        "POST",
        "/assistants",
        Some(json!({"assistant_id": "watcher", "name": "Incident Watcher", "graph": "react_agent", "config": {"instructions": "You watch incidents.", "studio_intent": {"tools": [{"name": "record.read"}]}}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    let (status, a) = call(
        &app,
        "POST",
        "/assignments",
        Some(json!({"assistant_id": "watcher", "request": "Watch INC0010104 until it is resolved and say who resolved it.", "success": "the resolver named", "max_rounds": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{a}");
    let id = a["assignment_id"].as_str().unwrap().to_owned();

    let a = settled(&app, &id, 2).await;
    assert_eq!(a["state"], "done", "{a}");
    // Round one: four reads answered, the fifth refused with the notice, the
    // sixth ended the round — not the budget, not the rounds.
    assert_eq!(a["rounds"][0]["status"], "error", "{a}");
    let why = a["rounds"][0]["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        why.contains("no progress: 3 reads in a row answered nothing new"),
        "{a}"
    );
    let reads = a["rounds"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["tool"] == "record.read")
        .count();
    assert_eq!(reads, 4, "the reads that ran, not the refused one: {a}");
    // Round two carried the reason as its steer and finished from it.
    let steer = a["rounds"][1]["steer"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        steer.contains("the previous round stopped — no progress"),
        "{a}"
    );
    assert!(steer.contains("Do not read the same way again"), "{steer}");
    assert_eq!(a["rounds"][1]["status"], "success", "{a}");
    assert_eq!(a["rounds_used"], 2, "{a}");
    assert_eq!(a["progress"]["complete"], true, "{a}");
    assert_eq!(
        a["max_tokens_per_round"], 150_000,
        "a round is bounded by default: {a}"
    );
    let second = seen
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|m| system_text(m).contains("— round 2;"))
        .cloned()
        .expect("a second round was asked");
    assert!(
        system_text(&second).contains("one bounded run of at most 150000 tokens"),
        "the agent is told the bound: {}",
        system_text(&second)
    );
    let opening = second
        .iter()
        .find(|m| m.role == ChatRole::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert!(
        opening.contains("The owner says: the previous round stopped — no progress"),
        "the model was told why: {opening}"
    );

    let _ = std::fs::remove_dir_all(store);
}
