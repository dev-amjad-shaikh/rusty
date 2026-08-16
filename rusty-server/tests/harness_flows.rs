//! Harness flows — the harness demo's end-to-end proof, automated.
//!
//! `rusty-server/examples/harness_demo.rs` serves four scripted ReAct
//! agents (calendar manager, ServiceNow operator, composer studio,
//! self-improver) over stateful in-process fixtures. This suite spawns
//! that example as a real process (the crash-recovery pattern: own port,
//! own store, SIGKILL guard) and drives nine journeys over plain HTTP,
//! asserting on the real responses and on `GET /runs/{id}/events` — the
//! Flight Recorder journal is the evidence, not the model's say-so:
//!
//! 1. calendar: list the fixture day, book the first verified free slot
//!    (09:30–10:00), then a follow-up summary that observes the booking;
//! 2. servicenow: open high-priority incidents → KB article, then read the
//!    KB back;
//! 3. per-run tool allowlists: the same booking with `create-event` blocked
//!    (the refusal is journaled and reported, the run still succeeds), and
//!    with it allowed (books 11:30–12:00 — the fixture kept journey 1's
//!    booking);
//! 4. skills: both `examples/harness_skills` packages upload clean;
//! 5. memory: write, read, query, correct, and re-query (the correction
//!    supersedes);
//! 6. knowledge: register a source and get a cited chunk back;
//! 7. evals: a dataset case sourced from journey 1's run evidence, a
//!    candidate, and an experiment that completes without regression;
//! 8. composer: compose → approval-gated publish → read-only `run_cli`,
//!    then a disallowed command refused by the policy;
//! 9. self-improver: introspect the demo's own registries → record backlog
//!    entries for the top gaps → draft the approved runbook entry's skill
//!    through the composer and stage its publish (the gate stays shut).
//!
//! One sequential test, one server: journeys 1 and 3 share the calendar
//! fixture by design, and the dataset case must match journey 1's run
//! exactly, so the order below is load-bearing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusty_agent_runtime::learn::{Candidate, CandidateContent, EvidenceSpan};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::skill::{scan_package, SkillPackage};
use serde_json::{json, Value};

/// The fixture day the demo's calendar is seeded with.
const DEMO_DAY: &str = "2026-02-09";

/// Journey 1's exact run input — the journey-7 dataset case must equal it
/// byte for byte (the server rejects sourced cases whose input does not
/// match the run's recorded input).
fn booking_input() -> Value {
    json!({"messages": [{"role": "user", "content": "Show my day and book a 30-minute slot."}]})
}

fn example_binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("test executable lives under <target>/<profile>/deps");
    let path = profile_dir
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "example binary `{name}` not found at {} — build the examples first \
         (`cargo test -p rusty-agent-server` does this for you)",
        path.display()
    );
    path
}

/// A free TCP port: bind, read, release (the shutdown suite's discipline).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A spawned demo server, SIGKILLed on drop so a panicking assertion never
/// leaks a process.
struct ChildGuard {
    child: Option<tokio::process::Child>,
}

impl ChildGuard {
    fn spawn(command: &mut tokio::process::Command) -> Self {
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn harness_demo");
        Self { child: Some(child) }
    }

    async fn sigkill(mut self) {
        let mut child = self.child.take().expect("process already reaped");
        child.kill().await.expect("failed to kill harness_demo");
        child.wait().await.expect("failed to reap harness_demo");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

async fn wait_ready(client: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client.get(format!("{base}/ok")).send().await {
            if response.status() == reqwest::StatusCode::OK {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "harness demo at {base} never became ready"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn post(client: &reqwest::Client, url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    let status = response.status();
    let value = response.json().await.unwrap_or(Value::Null);
    (status, value)
}

async fn get(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Value) {
    let response = client
        .get(url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = response.status();
    let value = response.json().await.unwrap_or(Value::Null);
    (status, value)
}

/// Create a thread on `graph`, then a blocking run over it; asserts both
/// steps and returns `(thread_id, terminal run JSON)`.
async fn run_on(
    client: &reqwest::Client,
    base: &str,
    graph: &str,
    payload: Value,
) -> (String, Value) {
    let (status, thread) = post(client, &format!("{base}/threads"), json!({"graph": graph})).await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "thread on {graph}: {thread}"
    );
    let thread_id = thread["thread_id"].as_str().unwrap().to_owned();
    let run = run_on_thread(client, base, &thread_id, payload).await;
    (thread_id, run)
}

async fn run_on_thread(
    client: &reqwest::Client,
    base: &str,
    thread_id: &str,
    payload: Value,
) -> Value {
    let (status, run) = post(
        client,
        &format!("{base}/threads/{thread_id}/runs/wait"),
        payload,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "run: {run}");
    assert_eq!(run["status"], "success", "run did not succeed: {run}");
    run
}

/// One journaled tool call: name, arguments, declared effect, status.
#[derive(Debug)]
struct ToolEvidence {
    name: String,
    arguments: Value,
    effect: String,
    status: String,
}

/// The run's journaled tool calls in seq order — the Flight Recorder's
/// account of what the run actually did.
async fn tool_calls(client: &reqwest::Client, base: &str, run_id: &str) -> Vec<ToolEvidence> {
    let (status, journal) = get(client, &format!("{base}/runs/{run_id}/events")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "events: {journal}");
    assert_eq!(journal["complete"], true, "journal incomplete: {journal}");
    journal["events"]
        .as_array()
        .expect("events is an array")
        .iter()
        .filter(|event| event["kind"] == "tool_call")
        .map(|event| ToolEvidence {
            name: event
                .pointer("/input/value/tool")
                .and_then(Value::as_str)
                .expect("tool_call input names the tool")
                .to_owned(),
            arguments: event
                .pointer("/input/value/arguments")
                .cloned()
                .expect("tool_call input carries arguments"),
            effect: event["effect"].as_str().expect("effect").to_owned(),
            status: event["status"].as_str().expect("status").to_owned(),
        })
        .collect()
}

/// The terminal assistant text of a run (the last assistant message in the
/// run's output channel).
fn final_assistant_text(run: &Value) -> &str {
    run.pointer("/output/messages")
        .and_then(Value::as_array)
        .expect("run output carries messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant")
        .and_then(|message| message["content"].as_str())
        .expect("the run ends with assistant text")
}

/// The run's tool messages' contents, in order.
fn tool_message_contents(run: &Value) -> Vec<&str> {
    run.pointer("/output/messages")
        .and_then(Value::as_array)
        .expect("run output carries messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .collect()
}

#[tokio::test]
async fn harness_flows_end_to_end() {
    let port = free_port();
    let store = std::env::temp_dir().join(format!("rusty-harness-flows-{}", uuid::Uuid::new_v4()));
    let base = format!("http://127.0.0.1:{port}");
    let server = ChildGuard::spawn(
        tokio::process::Command::new(example_binary("harness_demo"))
            .env("RUSTY_HARNESS_ADDR", format!("127.0.0.1:{port}"))
            .env("RUSTY_HARNESS_STORE", &store),
    );
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    // -- Journey 1: the calendar manager lists the day and books the first
    //    verified free slot; a follow-up summary observes the booking.
    let (status, assistant) = post(
        &client,
        &format!("{base}/assistants"),
        json!({
            "assistant_id": "calendar-coach",
            "name": "Calendar Coach",
            "graph": "calendar_manager",
            "config": {},
            "metadata": {}
        }),
    )
    .await;
    assert!(
        status == reqwest::StatusCode::CREATED || status == reqwest::StatusCode::OK,
        "assistant: {assistant}"
    );

    let (calendar_thread, booking_run) = run_on(
        &client,
        &base,
        "calendar_manager",
        json!({"assistant_id": "calendar-coach", "input": booking_input()}),
    )
    .await;
    let booking_run_id = booking_run["run_id"].as_str().unwrap().to_owned();
    let calls = tool_calls(&client, &base, &booking_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["google-calendar:list-events", "google-calendar:create-event"],
        "journey 1 journal"
    );
    assert_eq!(calls[0].effect, "read_only");
    assert_eq!(calls[1].effect, "compensatable");
    assert!(calls.iter().all(|call| call.status == "ok"));
    assert_eq!(
        calls[1].arguments.pointer("/start/dateTime"),
        Some(&json!(format!("{DEMO_DAY}T09:30:00Z"))),
        "the first free 30-minute slot on {DEMO_DAY} is 09:30–10:00"
    );
    assert_eq!(
        calls[1].arguments.pointer("/end/dateTime"),
        Some(&json!(format!("{DEMO_DAY}T10:00:00Z")))
    );
    assert!(final_assistant_text(&booking_run).contains("Requested 30-minute meeting"));

    let summary_run = run_on_thread(
        &client,
        &base,
        &calendar_thread,
        json!({"input": {"messages": [{"role": "user", "content": "Summarize my day."}]}}),
    )
    .await;
    let summary = final_assistant_text(&summary_run);
    assert!(
        summary.contains("Requested 30-minute meeting"),
        "the follow-up summary must observe journey 1's booking: {summary}"
    );
    assert!(
        summary.contains("Quarterly planning review") && summary.contains("overlaps"),
        "the seeded conflict pair must surface in the summary: {summary}"
    );

    // -- Journey 2: the ServiceNow operator distills the open high-priority
    //    incidents into a KB article; a follow-up reads the KB back.
    let (servicenow_thread, kb_run) = run_on(
        &client,
        &base,
        "servicenow_operator",
        json!({"input": {"messages": [{"role": "user", "content": "Show open high-priority incidents and file a KB article about the top theme."}]}}),
    )
    .await;
    let kb_run_id = kb_run["run_id"].as_str().unwrap().to_owned();
    let calls = tool_calls(&client, &base, &kb_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["servicenow:list-records", "servicenow:create-record"],
        "journey 2 journal"
    );
    assert_eq!(
        calls[0].arguments["sysparm_query"],
        json!("state=1^priority=1")
    );
    assert_eq!(calls[0].effect, "read_only");
    assert_eq!(calls[1].arguments["table"], json!("kb_knowledge"));
    assert_eq!(calls[1].effect, "compensatable");
    assert!(
        final_assistant_text(&kb_run).contains("KB0001001"),
        "the filed article's number: {}",
        final_assistant_text(&kb_run)
    );

    let kb_readback = run_on_thread(
        &client,
        &base,
        &servicenow_thread,
        json!({"input": {"messages": [{"role": "user", "content": "List the KB articles."}]}}),
    )
    .await;
    let readback = final_assistant_text(&kb_readback);
    assert!(
        readback.contains("KB0001001") && readback.contains("VPN connectivity"),
        "the KB read-back observes journey 2's article: {readback}"
    );

    // -- Journey 3: per-run tool allowlists. Blocked: create-event is
    //    refused, the refusal is the run's tool message, and the journal
    //    holds only the allowed listing. Allowed: the booking lands at
    //    11:30–12:00 because the fixture kept journey 1's.
    let (_blocked_thread, blocked_run) = run_on(
        &client,
        &base,
        "calendar_manager",
        json!({
            "input": booking_input(),
            "config": {"tool_allowlist": ["google-calendar:list-events"]}
        }),
    )
    .await;
    let blocked_run_id = blocked_run["run_id"].as_str().unwrap().to_owned();
    let tool_messages = tool_message_contents(&blocked_run);
    assert!(
        tool_messages
            .iter()
            .any(|content| content.contains("unknown tool `google-calendar:create-event`")),
        "the blocked booking must surface as a tool error: {tool_messages:?}"
    );
    let calls = tool_calls(&client, &base, &blocked_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["google-calendar:list-events"],
        "a refused call never reaches a tool, so the journal holds the allowed listing only"
    );
    assert!(
        final_assistant_text(&blocked_run).contains("Nothing was booked"),
        "the model must report the refusal honestly: {}",
        final_assistant_text(&blocked_run)
    );
    let (status, blocked_status) = get(&client, &format!("{base}/runs/{blocked_run_id}")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        blocked_status["capability_tools"],
        json!(["google-calendar:list-events"]),
        "the run status view surfaces the admitted capability selection"
    );

    let blocked_summary = run_on_thread(
        &client,
        &base,
        &_blocked_thread,
        json!({"input": {"messages": [{"role": "user", "content": "Summarize my day."}]}}),
    )
    .await;
    let text = final_assistant_text(&blocked_summary);
    assert_eq!(
        text.matches("Requested 30-minute meeting").count(),
        1,
        "only journey 1's booking exists; the blocked run created none: {text}"
    );

    let (_allowed_thread, allowed_run) = run_on(
        &client,
        &base,
        "calendar_manager",
        json!({
            "input": booking_input(),
            "config": {"tool_allowlist": ["google-calendar:list-events", "google-calendar:create-event"]}
        }),
    )
    .await;
    let allowed_run_id = allowed_run["run_id"].as_str().unwrap().to_owned();
    let calls = tool_calls(&client, &base, &allowed_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["google-calendar:list-events", "google-calendar:create-event"]
    );
    assert_eq!(
        calls[1].arguments.pointer("/start/dateTime"),
        Some(&json!(format!("{DEMO_DAY}T11:30:00Z"))),
        "with 09:30–10:00 taken, the next free slot is 11:30–12:00"
    );

    // -- Journey 4: both skill packages upload clean, list, and read back.
    let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/harness_skills");
    for name in ["calendar-management", "servicenow-operations"] {
        // The packages must pass core's own validation and scan before the
        // server ever sees them — the upload below is the wire half of the
        // same contract.
        let package = SkillPackage::from_dir(&skills_dir.join(name))
            .unwrap_or_else(|e| panic!("{name} must parse as a skill package: {e}"));
        assert!(
            scan_package(&package).is_clean(),
            "{name} must scan clean"
        );

        let skill_md = std::fs::read_to_string(skills_dir.join(name).join("SKILL.md"))
            .expect("read SKILL.md");
        let (status, receipt) = post(
            &client,
            &format!("{base}/skills"),
            json!({"skill_md": skill_md, "author": "operator:harness-demo"}),
        )
        .await;
        assert_eq!(
            status,
            reqwest::StatusCode::CREATED,
            "register {name}: {receipt}"
        );
        assert_eq!(receipt["name"], json!(name));
        assert_eq!(receipt["revision"], 1);
        assert_eq!(receipt["already_registered"], false);
        assert_eq!(receipt["content_hash"].as_str().unwrap().len(), 64);
        assert_eq!(receipt["scan"]["clean"], true, "scan: {receipt}");
    }
    let (status, list) = get(&client, &format!("{base}/skills")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "list skills: {list}");
    let names: Vec<&str> = list["skills"]
        .as_array()
        .expect("skills list")
        .iter()
        .filter_map(|skill| skill["name"].as_str())
        .collect();
    assert!(
        names.contains(&"calendar-management") && names.contains(&"servicenow-operations"),
        "both packages registered: {names:?}"
    );
    let (status, detail) = get(&client, &format!("{base}/skills/calendar-management")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "skill detail: {detail}");
    assert_eq!(
        detail["metadata"]["allowed_tools"],
        json!([
            "google-calendar:list-calendars",
            "google-calendar:list-events",
            "google-calendar:get-event",
            "google-calendar:create-event",
            "google-calendar:update-event",
            "google-calendar:delete-event"
        ]),
        "frontmatter allowed-tools (`:`-spelled — the validator's charset excludes `/`)"
    );
    let (status, body) = get(&client, &format!("{base}/skills/calendar-management/body")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "skill body: {body}");
    assert!(
        body["body"]
            .as_str()
            .expect("skill body text")
            .contains("verified free slot"),
        "the stored body is the authored guidance"
    );

    // -- Journey 5: memory write, read, query, correct, re-query.
    let (status, written) = post(
        &client,
        &format!("{base}/memory"),
        json!({
            "kind": "fact",
            "scope": {"scope": "user", "id": "user-7"},
            "content": {"timezone": "Asia/Dubai"},
            "author": {"type": "human", "human_id": "amjad"},
            "key": "timezone",
            "tags": ["prefs"]
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CREATED, "memory write: {written}");
    assert_eq!(written["created"], true);
    let memory_id = written["memory_id"].as_str().unwrap().to_owned();
    assert_eq!(memory_id.len(), 64, "memory ids are content addresses");

    let (status, fetched) = get(&client, &format!("{base}/memory/{memory_id}")).await;
    assert_eq!(status, reqwest::StatusCode::OK, "memory read: {fetched}");

    let query = json!({"scope": {"scope": "user", "id": "user-7"}, "key": "timezone"});
    let (status, found) = post(&client, &format!("{base}/memory/query"), query.clone()).await;
    assert_eq!(status, reqwest::StatusCode::OK, "memory query: {found}");
    assert_eq!(
        found["records"].as_array().expect("records").len(),
        1,
        "one live timezone record: {found}"
    );

    let (status, correction) = post(
        &client,
        &format!("{base}/memory/corrections"),
        json!({
            "correction_id": "corr-timezone-1",
            "author": "amjad",
            "target": {"type": "memory", "memory_id": memory_id},
            "corrected": {"timezone": "Asia/Dubai (GMT+4)"},
            "scope": {"scope": "user", "id": "user-7"}
        }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "memory correction: {correction}"
    );
    assert_eq!(correction["superseded"], json!(memory_id));

    let (status, after) = post(&client, &format!("{base}/memory/query"), query).await;
    assert_eq!(status, reqwest::StatusCode::OK, "memory re-query: {after}");
    let records = after["records"].as_array().expect("records");
    assert_eq!(records.len(), 1, "the correction serves alone: {after}");
    assert_eq!(
        records[0]["content"]["value"],
        json!({"timezone": "Asia/Dubai (GMT+4)"}),
        "the corrected content is what retrieval serves"
    );

    // -- Journey 6: knowledge ingest and cited retrieval.
    let (status, source) = post(
        &client,
        &format!("{base}/knowledge/sources"),
        json!({
            "source_id": "harness-notes",
            "kind": "text",
            "title": "Harness demo operator notes",
            "author": "human:curator",
            "body": "Flight recorder evidence is the run's journal of model calls, tool calls, \
                     and node transitions, each with its causal parent.\n\n\
                     Every journaled effect carries its declared effect class, so replay and \
                     rollback policy can reason about what re-execution would do."
        }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "knowledge source: {source}"
    );
    assert_eq!(source["version"], 1);
    assert!(source["chunk_count"].as_u64().unwrap() >= 1);

    let (status, answer) = post(
        &client,
        &format!("{base}/knowledge/query"),
        json!({"text": "flight recorder evidence"}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "knowledge query: {answer}");
    let results = answer["results"].as_array().expect("results");
    assert!(!results.is_empty(), "retrieval must cite the source: {answer}");
    assert_eq!(results[0]["citation"]["source_id"], json!("harness-notes"));
    assert_eq!(
        results[0]["citation"]["content_address"]
            .as_str()
            .unwrap()
            .len(),
        64,
        "citations carry the chunk's content address"
    );

    // -- Journey 7: a dataset case sourced from journey 1's run evidence,
    //    a candidate over it, and an experiment that completes clean.
    let dataset = json!({
        "name": "calendar-day-flow",
        "version": DEMO_DAY,
        "cases": [{
            "id": "book-30",
            "input": booking_input(),
            "expect": {"tool_trajectory": [
                {"name": "google-calendar:list-events"},
                {"name": "google-calendar:create-event"}
            ]},
            "tags": ["calendar"],
            "source": {
                "run_id": booking_run_id,
                "thread_id": calendar_thread,
                "agent_id": "calendar-coach",
                "captured_at": "2020-01-01T00:00:00Z"
            }
        }]
    });
    let (status, created) = post(&client, &format!("{base}/datasets"), dataset).await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "dataset from run evidence: {created}"
    );
    assert_eq!(created["case_count"], 1);
    assert_eq!(created["digest"].as_str().unwrap().len(), 64);

    let candidate = Candidate::new(
        CandidateContent::Prompt {
            name: "calendar_manager".into(),
            prompt: "Answer precisely.".into(),
        },
        ProvenanceAuthor::Distiller {
            name: "harness-demo".into(),
        },
        EvidenceSpan::default(),
        DateTime::<Utc>::from_timestamp_millis(1_754_953_200_000).unwrap(),
    )
    .expect("candidate");
    let candidate_id = candidate.candidate_id.to_string();
    let (status, registered) = post(
        &client,
        &format!("{base}/learn/candidates"),
        json!({"candidate": candidate, "run_id": booking_run_id}),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "candidate: {registered}"
    );

    let (status, started) = post(
        &client,
        &format!("{base}/experiments"),
        json!({
            "experiment_id": "exp-calendar",
            "candidate_id": candidate_id,
            "dataset_name": "calendar-day-flow",
            "dataset_version": DEMO_DAY,
            "runs_per_case": 1,
            "max_concurrency": 1,
            "target_metric": "case_pass_rate",
            "thresholds": {"max_pass_rate_drop": 0.05, "max_latency_p95_ratio": 1.25}
        }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "experiment: {started}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let settled = loop {
        let (status, experiment) = get(&client, &format!("{base}/experiments/exp-calendar")).await;
        assert_eq!(status, reqwest::StatusCode::OK, "experiment: {experiment}");
        if experiment["status"]["phase"] == "complete"
            || experiment["status"]["phase"] == "failed"
        {
            break experiment;
        }
        assert!(Instant::now() < deadline, "experiment never settled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(settled["status"]["phase"], "complete", "settled: {settled}");
    assert_eq!(
        settled["comparison"]["regressed"], false,
        "identical all-pass reports cannot regress: {settled}"
    );

    // -- Journey 8: the composer drafts, publishes under its pre-minted
    //    approval, and lists the skills directory read-only; a disallowed
    //    command is refused by the CLI policy.
    let (composer_thread, compose_run) = run_on(
        &client,
        &base,
        "composer_studio",
        json!({"input": {"messages": [{"role": "user", "content": "Compose the standup brief skill, publish it, and list the skills directory."}]}}),
    )
    .await;
    let compose_run_id = compose_run["run_id"].as_str().unwrap().to_owned();
    let calls = tool_calls(&client, &base, &compose_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["compose_skill", "publish_composed_skill", "run_cli"],
        "journey 8 journal"
    );
    assert_eq!(calls[0].effect, "pure");
    assert_eq!(calls[1].effect, "non_idempotent");
    assert_eq!(calls[2].effect, "read_only");
    assert!(
        calls.iter().all(|call| call.status == "ok"),
        "all three calls succeed: {calls:?}"
    );
    let (status, journal) = get(&client, &format!("{base}/runs/{compose_run_id}/events")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let publish = journal["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event.pointer("/input/value/tool") == Some(&json!("publish_composed_skill")))
        .expect("the publish event");
    assert_eq!(
        publish.pointer("/output/value/name"),
        Some(&json!("daily-standup-brief"))
    );
    assert_eq!(publish.pointer("/output/value/revision"), Some(&json!(1)));
    assert_eq!(
        publish.pointer("/output/value/approved_by"),
        Some(&json!("ops:harness-demo")),
        "the approval-bound publisher is on the receipt"
    );

    let refused_run = run_on_thread(
        &client,
        &base,
        &composer_thread,
        json!({"input": {"messages": [{"role": "user", "content": "Try a disallowed command."}]}}),
    )
    .await;
    let tool_messages = tool_message_contents(&refused_run);
    assert!(
        tool_messages
            .iter()
            .any(|content| content.contains("not in the policy allowlist")),
        "the CLI policy refuses `rm`: {tool_messages:?}"
    );
    let refused_run_id = refused_run["run_id"].as_str().unwrap().to_owned();
    let calls = tool_calls(&client, &base, &refused_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["run_cli"]
    );
    assert_eq!(
        calls[0].status, "error",
        "the refusal is journaled as an errored call"
    );

    // -- Journey 9: the self-improver introspects the demo's own
    //    registries, records backlog entries for the top gaps, then drafts
    //    the pre-approved runbook entry's skill through the composer and
    //    stages its publish — without crossing the approval gate.
    let (_loop_thread, loop_run) = run_on(
        &client,
        &base,
        "self_improver",
        json!({"input": {"messages": [{"role": "user", "content": "Inspect your capabilities, record the top gaps, and stage the runbook skill."}]}}),
    )
    .await;
    let loop_run_id = loop_run["run_id"].as_str().unwrap().to_owned();
    let calls = tool_calls(&client, &base, &loop_run_id).await;
    assert_eq!(
        calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
        ["inspect_capabilities", "propose_backlog_entries", "build_gap_skill"],
        "journey 9 journal: introspect → backlog → draft-and-stage"
    );
    assert_eq!(calls[0].effect, "read_only");
    assert_eq!(calls[1].effect, "idempotent");
    assert_eq!(calls[2].effect, "pure");
    assert!(
        calls.iter().all(|call| call.status == "ok"),
        "all three loop steps succeed: {calls:?}"
    );

    let (status, journal) = get(&client, &format!("{base}/runs/{loop_run_id}/events")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let events = journal["events"].as_array().unwrap();
    let tool_event = |name: &str| {
        events
            .iter()
            .find(|event| event.pointer("/input/value/tool") == Some(&json!(name)))
            .unwrap_or_else(|| panic!("the journal holds a {name} call"))
            .clone()
    };

    // The inspection is honest about the demo's own wiring: the real
    // planes are present, run_cli-without-confinement is exactly partial,
    // and every dsh-parity gap stays absent until its stream lands. The
    // full report exceeds the journal's inline-payload ceiling, so the
    // journaled event carries it as a content-addressed artifact
    // reference — evidence by hash — while the run's tool message holds
    // the inline JSON to assert against.
    let inspect = tool_event("inspect_capabilities");
    assert_eq!(inspect["output"]["kind"], json!("artifact"));
    let reference = &inspect["output"]["value"];
    assert_eq!(reference["sha256"].as_str().unwrap().len(), 64);
    assert!(
        reference["bytes"].as_u64().unwrap() > 4096,
        "the report spills above the inline ceiling: {reference}"
    );
    let report_text = tool_message_contents(&loop_run)[0].to_owned();
    let report: Value = serde_json::from_str(&report_text).unwrap();
    let present = report["present"].as_u64().unwrap();
    let partial = report["partial"].as_u64().unwrap();
    let absent = report["absent"].as_u64().unwrap();
    assert_eq!(
        present + partial + absent,
        report["assessments"].as_array().unwrap().len() as u64,
        "the counts are derived, not claimed: {report}"
    );
    assert!(present >= 8, "the demo's real planes are present: {report}");
    let status_of = |id: &str| {
        report["assessments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == json!(id))
            .unwrap_or_else(|| panic!("the report assesses {id}"))
            .pointer("/status/status")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned()
    };
    assert_eq!(status_of("skill-plane"), "present");
    assert_eq!(status_of("approval-gated-publish"), "present");
    assert_eq!(status_of("os-sandbox-confinement"), "partial");
    assert_eq!(status_of("telemetry-ledger"), "absent");
    // A staged-but-unpublished runbook honestly changes nothing.
    assert_eq!(status_of("operator-runbooks"), "absent");

    // The proposals land as harness-proposed, operator-unapproved work.
    let propose = tool_event("propose_backlog_entries");
    let recorded = propose["output"]["value"]["recorded"].as_array().unwrap();
    assert_eq!(recorded.len(), 3, "one entry per top gap: {propose}");
    let proposed_gaps: Vec<&str> = recorded
        .iter()
        .map(|entry| entry["gap_ids"][0].as_str().unwrap())
        .collect();
    assert_eq!(
        proposed_gaps,
        ["surface-compaction", "telemetry-ledger", "agent-session-query"]
    );
    for entry in recorded {
        assert_eq!(entry["provenance"], json!("harness:self-improve"));
        assert_eq!(entry["status"], json!("proposed"));
        assert_eq!(entry["inserted"], json!(true));
        assert!(entry["id"].as_str().unwrap().starts_with("bl-"));
    }

    // The draft is staged behind the gate: an approved entry, a content
    // hash, the publish effect id — and no publish call anywhere in the
    // journal.
    let build = tool_event("build_gap_skill");
    let staged = &build["output"]["value"];
    assert_eq!(staged["entry_status"], json!("approved"));
    assert_eq!(staged["content_hash"].as_str().unwrap().len(), 64);
    assert!(
        staged["publish_effect_id"].as_str().unwrap().len() >= 64,
        "the staged publish names its scoped effect id: {staged}"
    );
    assert!(
        events
            .iter()
            .all(|event| event.pointer("/input/value/tool") != Some(&json!("publish_composed_skill"))),
        "the loop never publishes: the gate stays with the operator"
    );
    let closing = final_assistant_text(&loop_run);
    assert!(
        closing.contains("runbook-incident-review") && closing.contains("awaits an operator approval"),
        "the closing message reports the staged gate honestly: {closing}"
    );

    server.sigkill().await;
    let _ = std::fs::remove_dir_all(&store);
}
