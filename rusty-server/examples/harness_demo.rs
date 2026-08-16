//! Harness demo: four ReAct agents that walk the harness surfaces end to
//! end — connector service packs over stateful fixtures, per-run tool
//! allowlists, the composer lane, the self-improvement loop, and an
//! experiment evaluator — served on `127.0.0.1:8110`.
//!
//! The models are scripted and deterministic (no network, no credentials):
//! each is a small state machine over the run's message tail, so every run
//! produces exact model-call and tool-call evidence that
//! `rusty-server/tests/harness_flows.rs` asserts against. The Google
//! Calendar and ServiceNow "services" are in-process fixture transports
//! wired behind the real pack manifests — real request envelopes in
//! (path templates, percent-encoded query, auth headers), real-style
//! envelopes out (`calendar#event` resources, `{"result": …}` projections,
//! Google/SN-shaped 404 and 400 bodies) — so the demo exercises the
//! genuine provider path with state a test can rely on.
//!
//! The fixture day is 2026-02-09 (UTC). The calendar seeds a real conflict
//! pair ("Quarterly planning review" 10:00–11:00 vs "Design sync"
//! 10:30–11:30); the first free 30-minute slot is 09:30–10:00, and after a
//! booking there the next is 11:30–12:00. ServiceNow seeds two open
//! priority-1 VPN incidents, so `state=1^priority=1` deterministically
//! surfaces VPN as the top theme.
//!
//! Run with: `cargo run --example harness_demo`
//!
//! Test hooks (mirroring server_demo's `RUSTY_DEMO_*` discipline):
//! `RUSTY_HARNESS_ADDR` overrides the bind address and
//! `RUSTY_HARNESS_STORE` the JSON-file store directory.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use rusty_agent_runtime::composer::{
    publish_effect_id, ComposeSkillTool, ComposeToolDefinitionTool, ComposerSession,
    PublishComposedSkillTool,
};
use rusty_agent_runtime::connector::{
    packs, CredentialHandle, HttpApiProvider, HttpApiRequest, HttpApiTool, HttpApiTransport,
    HttpMethod, HttpResponse,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::learn::Candidate;
use rusty_agent_runtime::prelude::*;
use rusty_agent_runtime::self_improve::{
    BacklogEntry, BacklogProvenance, BacklogStatus, BacklogStore, BuildGapSkillTool,
    CapabilityInspection, InspectCapabilitiesTool, Plane, ProposeBacklogTool,
    FEATURE_CAPABILITY_SETS, HARNESS_PROVENANCE,
};
use rusty_agent_runtime::skill::{SkillPackage, SkillRegistry};
use rusty_agent_runtime::tool::builtins::cli::{CliPolicy, CliTool};
use rusty_agent_server::{
    serve, ExperimentOutcome, GraphRegistry, ServerConfig, StudioExperimentConfig,
    StudioExperimentEvaluator,
};
use rusty_eval::{Dataset, ExperimentReport};
use serde_json::{json, Value};

/// The fixture day every calendar journey runs against. Fixed so the slot
/// arithmetic above stays a documented, testable fact rather than a moving
/// target.
const DEMO_DAY: &str = "2026-02-09";

/// The summary the scripted calendar manager gives every booking it makes.
const BOOKING_SUMMARY: &str = "Requested 30-minute meeting";

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Mount adapter between the connector catalog and the advertised tool
/// contract. The pack spells its tools `<connector>/<operation>`
/// (`google-calendar/list-events`), but the contract charset every
/// advertised surface must pass excludes `/`, so `register_with_tools`
/// cannot carry the native spelling. The demo mounts each pack tool under
/// the `:` spelling — the same tool, schema, and effect under a name the
/// catalog (and the skill frontmatter, whose entry charset matches) can
/// carry.
#[derive(Debug)]
struct CatalogTool {
    inner: HttpApiTool,
    name: String,
}

impl CatalogTool {
    fn mount(inner: HttpApiTool) -> Self {
        let name = inner.name().replacen('/', ":", 1);
        Self { inner, name }
    }
}

#[async_trait]
impl Tool for CatalogTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }
    fn effect(&self) -> Effect {
        self.inner.effect()
    }
    fn effect_kind(&self) -> &str {
        self.inner.effect_kind()
    }
    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.inner.idempotency_key(args)
    }
    async fn call(&self, args: Value) -> Result<Value> {
        self.inner.call(args).await
    }
}

/// Serialize `value` into a response with `status`.
fn http_json(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_vec(&value).expect("a serde_json::Value always serializes"),
    }
}

/// Percent-decode one query or path segment. The provider percent-encodes
/// every scalar it renders, so the fixtures must decode before comparing —
/// this is the decoding half of the wire discipline, dependency-free like
/// the provider's encoding half.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(decoded) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(decoded);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a full request URL into its decoded path and decoded query map.
fn path_and_query(url: &str) -> (String, BTreeMap<String, String>) {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path_start = without_scheme.find('/').unwrap_or(0);
    let rest = &without_scheme[path_start..];
    match rest.split_once('?') {
        Some((path, query)) => {
            let params = query
                .split('&')
                .filter_map(|pair| {
                    pair.split_once('=')
                        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
                })
                .collect();
            (percent_decode(path), params)
        }
        None => (percent_decode(rest), BTreeMap::new()),
    }
}

/// Parse a request body as a JSON object, or answer with the service's own
/// 400 envelope shape.
fn parse_body(
    body: &[u8],
    bad_request: impl FnOnce(&str) -> HttpResponse,
) -> std::result::Result<Value, HttpResponse> {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) => Err(bad_request("the request body must be a JSON object")),
        Err(error) => Err(bad_request(&format!(
            "the request body is not JSON: {error}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Google Calendar fixture
// ---------------------------------------------------------------------------

/// A Google-shaped 404: the fixture's answer to unknown events and routes.
fn google_not_found(message: &str) -> HttpResponse {
    http_json(
        404,
        json!({
            "error": {
                "code": 404,
                "message": message,
                "errors": [{"domain": "calendar", "reason": "notFound", "message": message}]
            }
        }),
    )
}

/// A Google-shaped 400 for malformed request bodies.
fn google_bad_request(message: &str) -> HttpResponse {
    http_json(
        400,
        json!({
            "error": {
                "code": 400,
                "message": message,
                "errors": [{"domain": "global", "reason": "badRequest", "message": message}]
            }
        }),
    )
}

/// One `calendar#event` resource in the real wire shape.
fn calendar_event(id: &str, summary: &str, start: &str, end: &str) -> Value {
    json!({
        "kind": "calendar#event",
        "etag": format!("\"{id}\""),
        "id": id,
        "status": "confirmed",
        "htmlLink": format!("https://www.google.com/calendar/event?eid={id}"),
        "summary": summary,
        "start": {"dateTime": start, "timeZone": "UTC"},
        "end": {"dateTime": end, "timeZone": "UTC"},
    })
}

#[derive(Debug)]
struct CalendarState {
    events: BTreeMap<String, Value>,
    next_create: u32,
}

impl CalendarState {
    /// The seeded demo day: a standup, a real conflict pair, and a lunch.
    /// Created events allocate `evt-1001` upward so fixture and created ids
    /// never collide.
    fn seeded() -> Self {
        let events = [
            calendar_event(
                "evt-0001",
                "Standup",
                "2026-02-09T09:00:00Z",
                "2026-02-09T09:30:00Z",
            ),
            calendar_event(
                "evt-0002",
                "Quarterly planning review",
                "2026-02-09T10:00:00Z",
                "2026-02-09T11:00:00Z",
            ),
            calendar_event(
                "evt-0003",
                "Design sync",
                "2026-02-09T10:30:00Z",
                "2026-02-09T11:30:00Z",
            ),
            calendar_event(
                "evt-0004",
                "Lunch with Priya",
                "2026-02-09T12:30:00Z",
                "2026-02-09T13:30:00Z",
            ),
        ]
        .into_iter()
        .map(|event| (event["id"].as_str().expect("event id").to_owned(), event))
        .collect();
        Self {
            events,
            next_create: 1001,
        }
    }

    /// `events.list`: honor the time window, answer start-ordered like
    /// `singleEvents=true&orderBy=startTime` (which is how the model asks).
    fn list_events(&self, query: &BTreeMap<String, String>) -> HttpResponse {
        let mut items: Vec<Value> = self
            .events
            .values()
            .filter(|event| {
                let start = event
                    .pointer("/start/dateTime")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                query.get("timeMin").is_none_or(|min| start >= min.as_str())
                    && query.get("timeMax").is_none_or(|max| start < max.as_str())
            })
            .cloned()
            .collect();
        items.sort_by(|a, b| event_start(a).cmp(event_start(b)));
        http_json(200, json!({"kind": "calendar#events", "items": items}))
    }

    fn get_event(&self, event_id: &str) -> HttpResponse {
        match self.events.get(event_id) {
            Some(event) => http_json(200, event.clone()),
            None => google_not_found("Not Found"),
        }
    }

    /// `events.insert`: the request body carries the writable fields; the
    /// fixture stamps the server-owned ones, as the real API would.
    fn create_event(&mut self, body: &[u8]) -> HttpResponse {
        let mut event = match parse_body(body, google_bad_request) {
            Ok(event) => event,
            Err(response) => return response,
        };
        let id = format!("evt-{}", self.next_create);
        self.next_create += 1;
        let object = event.as_object_mut().expect("parse_body yields objects");
        object.insert("kind".to_owned(), json!("calendar#event"));
        object.insert("etag".to_owned(), json!(format!("\"{id}\"")));
        object.insert("id".to_owned(), json!(id.clone()));
        object.insert("status".to_owned(), json!("confirmed"));
        object.insert(
            "htmlLink".to_owned(),
            json!(format!("https://www.google.com/calendar/event?eid={id}")),
        );
        self.events.insert(id, event.clone());
        http_json(200, event)
    }

    /// `events.patch`: merge the writable fields over the stored resource.
    fn update_event(&mut self, event_id: &str, body: &[u8]) -> HttpResponse {
        let patch = match parse_body(body, google_bad_request) {
            Ok(patch) => patch,
            Err(response) => return response,
        };
        let Some(event) = self.events.get_mut(event_id) else {
            return google_not_found("Not Found");
        };
        let object = event.as_object_mut().expect("stored events are objects");
        for (key, value) in patch.as_object().expect("parse_body yields objects") {
            object.insert(key.clone(), value.clone());
        }
        http_json(200, event.clone())
    }

    fn delete_event(&mut self, event_id: &str) -> HttpResponse {
        match self.events.remove(event_id) {
            Some(_) => HttpResponse {
                status: 204,
                body: Vec::new(),
            },
            None => google_not_found("Not Found"),
        }
    }
}

fn event_start(event: &Value) -> &str {
    event
        .pointer("/start/dateTime")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// The calendar service: the real pack's request path (template rendering,
/// query encoding, bearer auth header) ends here instead of at Google.
/// Writes mutate, so follow-up runs observe earlier bookings.
#[derive(Debug, Clone)]
struct CalendarFixture {
    state: Arc<Mutex<CalendarState>>,
}

impl CalendarFixture {
    fn seeded() -> Self {
        Self {
            state: Arc::new(Mutex::new(CalendarState::seeded())),
        }
    }
}

#[async_trait]
impl HttpApiTransport for CalendarFixture {
    async fn send(&self, request: HttpApiRequest) -> Result<HttpResponse> {
        let (path, query) = path_and_query(&request.url);
        let segments: Vec<&str> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(match (request.method, segments.as_slice()) {
            (HttpMethod::Get, ["calendar", "v3", "users", "me", "calendarList"]) => http_json(
                200,
                json!({
                    "kind": "calendar#calendarList",
                    "items": [{
                        "kind": "calendar#calendarListEntry",
                        "id": "primary",
                        "summary": "Harness Demo",
                        "timeZone": "UTC",
                        "primary": true
                    }]
                }),
            ),
            (HttpMethod::Get, ["calendar", "v3", "calendars", _calendar, "events"]) => {
                state.list_events(&query)
            }
            (HttpMethod::Post, ["calendar", "v3", "calendars", _calendar, "events"]) => {
                state.create_event(&request.body)
            }
            (HttpMethod::Get, ["calendar", "v3", "calendars", _calendar, "events", event_id]) => {
                state.get_event(event_id)
            }
            (HttpMethod::Patch, ["calendar", "v3", "calendars", _calendar, "events", event_id]) => {
                state.update_event(event_id, &request.body)
            }
            (
                HttpMethod::Delete,
                ["calendar", "v3", "calendars", _calendar, "events", event_id],
            ) => state.delete_event(event_id),
            _ => google_not_found("The requested calendar resource was not found."),
        })
    }
}

// ---------------------------------------------------------------------------
// ServiceNow fixture
// ---------------------------------------------------------------------------

/// A ServiceNow-shaped failure envelope (the Table API's error body).
fn sn_failure(status: u16, message: &str) -> HttpResponse {
    http_json(
        status,
        json!({"error": {"message": message, "detail": null}, "status": "failure"}),
    )
}

/// Parse the fixture's `sysparm_query` subset: `field=value` conjunctions
/// joined by `^`. Anything richer (OR, operators, ORDERBY) is the real
/// API's job, not the fixture's — it answers 400 like ServiceNow answers
/// a malformed query.
fn sysparm_terms(query: &str) -> std::result::Result<Vec<(String, String)>, String> {
    let mut terms = Vec::new();
    for term in query.split('^') {
        let (field, value) = term
            .split_once('=')
            .ok_or_else(|| format!("query term `{term}` is not `field=value`"))?;
        if field.is_empty()
            || !field.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.')
            })
        {
            return Err(format!("query field `{field}` is not a plain field name"));
        }
        terms.push((field.to_owned(), value.to_owned()));
    }
    Ok(terms)
}

/// One record against parsed query terms. String and number fields compare
/// against the term's string value; a missing field simply does not match.
fn record_matches(record: &Value, terms: &[(String, String)]) -> bool {
    terms.iter().all(|(field, value)| match record.get(field) {
        Some(Value::String(actual)) => actual == value,
        Some(Value::Number(actual)) => actual.to_string() == *value,
        Some(Value::Bool(actual)) => actual.to_string() == *value,
        _ => false,
    })
}

#[derive(Debug)]
struct ServiceNowState {
    tables: BTreeMap<String, BTreeMap<String, Value>>,
    numbers: BTreeMap<String, u64>,
    next_sys_id: u64,
}

impl ServiceNowState {
    /// Two open priority-1 VPN incidents (the deterministic "top theme"),
    /// one in-progress battery issue, one open priority-2 SSO loop.
    /// `sys_id`s are the real 32-hex shape; allocation continues at 5.
    fn seeded() -> Self {
        let incidents = [
            (
                "1",
                "INC0001001",
                "VPN gateway flapping in DXB office",
                "1",
                "1",
                "network",
            ),
            (
                "2",
                "INC0001002",
                "VPN tunnel drops every ten minutes",
                "1",
                "1",
                "network",
            ),
            (
                "3",
                "INC0001003",
                "Laptop battery swelling",
                "2",
                "3",
                "hardware",
            ),
            (
                "4",
                "INC0001004",
                "SSO login loop for Okta users",
                "1",
                "2",
                "identity",
            ),
        ]
        .into_iter()
        .map(
            |(sys_n, number, short_description, state, priority, category)| {
                let sys_id = format!("{sys_n:0>32}");
                let record = json!({
                    "sys_id": sys_id,
                    "number": number,
                    "short_description": short_description,
                    "state": state,
                    "priority": priority,
                    "category": category,
                    "opened_by": "harness.demo",
                    "sys_created_on": "2026-02-09 08:00:00",
                    "sys_updated_on": "2026-02-09 08:00:00",
                });
                (sys_id, record)
            },
        )
        .collect();
        let mut tables = BTreeMap::new();
        tables.insert("incident".to_owned(), incidents);
        Self {
            tables,
            numbers: BTreeMap::new(),
            next_sys_id: 5,
        }
    }

    /// The per-table number sequence, continuing where the seeds left off.
    /// Tables without a number policy (most) simply get none, like a table
    /// without a number maintenance rule.
    fn allocate_number(&mut self, table: &str) -> Option<String> {
        let (prefix, first) = match table {
            "incident" => ("INC", 1005),
            "kb_knowledge" => ("KB", 1001),
            "sc_request" => ("REQ", 1001),
            _ => return None,
        };
        let next = self.numbers.entry(table.to_owned()).or_insert(first);
        let number = format!("{prefix}{next:07}");
        *next += 1;
        Some(number)
    }

    fn list_records(&self, table: &str, query: &BTreeMap<String, String>) -> HttpResponse {
        let Some(records) = self.tables.get(table) else {
            return sn_failure(404, &format!("Invalid or no table `{table}`"));
        };
        let terms = match query.get("sysparm_query") {
            Some(raw) => match sysparm_terms(raw) {
                Ok(terms) => terms,
                Err(message) => return sn_failure(400, &message),
            },
            None => Vec::new(),
        };
        let offset = query
            .get("sysparm_offset")
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = query
            .get("sysparm_limit")
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        let fields = query
            .get("sysparm_fields")
            .map(|raw| raw.split(',').map(str::to_owned).collect::<Vec<_>>());
        let result: Vec<Value> = records
            .values()
            .filter(|record| record_matches(record, &terms))
            .skip(offset)
            .take(limit)
            .map(|record| match &fields {
                Some(fields) => fields
                    .iter()
                    .filter_map(|field| {
                        record
                            .get(field)
                            .map(|value| (field.clone(), value.clone()))
                    })
                    .collect(),
                None => record.clone(),
            })
            .collect();
        http_json(200, json!({"result": result}))
    }

    fn get_record(&self, table: &str, sys_id: &str) -> HttpResponse {
        let record = self
            .tables
            .get(table)
            .and_then(|records| records.get(sys_id));
        match record {
            Some(record) => http_json(200, json!({"result": record})),
            None => sn_failure(404, "No record found"),
        }
    }

    /// Table API insert: the caller's fields plus the server-owned sys_id,
    /// number, and timestamps.
    fn create_record(&mut self, table: &str, body: &[u8]) -> HttpResponse {
        let mut record = match parse_body(body, |message| sn_failure(400, message)) {
            Ok(record) => record,
            Err(response) => return response,
        };
        let sys_id = format!("{:032x}", self.next_sys_id);
        self.next_sys_id += 1;
        let object = record.as_object_mut().expect("parse_body yields objects");
        object.insert("sys_id".to_owned(), json!(sys_id.clone()));
        if let Some(number) = self.allocate_number(table) {
            object.insert("number".to_owned(), json!(number));
        }
        object.insert("sys_created_on".to_owned(), json!("2026-02-09 09:00:00"));
        object.insert("sys_updated_on".to_owned(), json!("2026-02-09 09:00:00"));
        self.tables
            .entry(table.to_owned())
            .or_default()
            .insert(sys_id, record.clone());
        http_json(201, json!({"result": record}))
    }

    fn update_record(&mut self, table: &str, sys_id: &str, body: &[u8]) -> HttpResponse {
        let patch = match parse_body(body, |message| sn_failure(400, message)) {
            Ok(patch) => patch,
            Err(response) => return response,
        };
        let Some(record) = self
            .tables
            .get_mut(table)
            .and_then(|records| records.get_mut(sys_id))
        else {
            return sn_failure(404, "No record found");
        };
        let object = record.as_object_mut().expect("stored records are objects");
        for (key, value) in patch.as_object().expect("parse_body yields objects") {
            object.insert(key.clone(), value.clone());
        }
        object.insert("sys_updated_on".to_owned(), json!("2026-02-09 09:30:00"));
        http_json(200, json!({"result": record}))
    }

    fn delete_record(&mut self, table: &str, sys_id: &str) -> HttpResponse {
        let removed = self
            .tables
            .get_mut(table)
            .and_then(|records| records.remove(sys_id));
        match removed {
            Some(_) => HttpResponse {
                status: 204,
                body: Vec::new(),
            },
            None => sn_failure(404, "No record found"),
        }
    }
}

/// The ServiceNow tenant: basic-auth headers arrive resolved, the table
/// routes behave, and created records persist across runs.
#[derive(Debug, Clone)]
struct ServiceNowFixture {
    state: Arc<Mutex<ServiceNowState>>,
}

impl ServiceNowFixture {
    fn seeded() -> Self {
        Self {
            state: Arc::new(Mutex::new(ServiceNowState::seeded())),
        }
    }
}

#[async_trait]
impl HttpApiTransport for ServiceNowFixture {
    async fn send(&self, request: HttpApiRequest) -> Result<HttpResponse> {
        let (path, query) = path_and_query(&request.url);
        let segments: Vec<&str> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(match (request.method, segments.as_slice()) {
            (HttpMethod::Get, ["api", "now", "table", table]) => state.list_records(table, &query),
            (HttpMethod::Post, ["api", "now", "table", table]) => {
                state.create_record(table, &request.body)
            }
            (HttpMethod::Get, ["api", "now", "table", table, sys_id]) => {
                state.get_record(table, sys_id)
            }
            (HttpMethod::Patch, ["api", "now", "table", table, sys_id]) => {
                state.update_record(table, sys_id, &request.body)
            }
            (HttpMethod::Delete, ["api", "now", "table", table, sys_id]) => {
                state.delete_record(table, sys_id)
            }
            _ => sn_failure(404, "Unknown ServiceNow route"),
        })
    }
}

// ---------------------------------------------------------------------------
// Scripted models
// ---------------------------------------------------------------------------

/// Where a run's work begins. Threads accumulate messages across runs, so
/// rounds are counted from the tail — the tool replies that follow the last
/// user message — never from the head of the channel.
fn turn_progress(messages: &[ChatMessage]) -> (String, Vec<String>) {
    let last_user = messages
        .iter()
        .rposition(|message| message.role == Role::User);
    let user = last_user
        .and_then(|index| messages[index].content.clone())
        .unwrap_or_default();
    let replies = messages
        .iter()
        .skip(last_user.map_or(0, |index| index + 1))
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.content.clone())
        .collect();
    (user, replies)
}

/// A tool reply the executor already marked failed (`ERROR: …`).
fn is_tool_error(reply: &str) -> bool {
    reply.starts_with("ERROR:")
}

fn respond(message: ChatMessage, model: &str) -> Result<ChatResponse> {
    Ok(ChatResponse {
        message,
        model: Some(model.to_owned()),
        usage: None,
    })
}

fn format_instant(instant: DateTime<FixedOffset>) -> String {
    instant.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `HH:MM` out of the fixture's uniform RFC-3339 strings.
fn hhmm(date_time: &str) -> &str {
    date_time.get(11..16).unwrap_or(date_time)
}

/// First free 30-minute slot inside the 09:00–17:00 UTC work window, given
/// the day's events. Overlap is interval arithmetic (`a.start < b.end &&
/// b.start < a.end`); the cursor only ever moves forward past busy ends.
fn first_free_slot(items: &[Value]) -> Option<(String, String)> {
    let mut busy: Vec<(DateTime<FixedOffset>, DateTime<FixedOffset>)> = items
        .iter()
        .filter_map(|event| {
            let start =
                DateTime::parse_from_rfc3339(event.pointer("/start/dateTime")?.as_str()?).ok()?;
            let end =
                DateTime::parse_from_rfc3339(event.pointer("/end/dateTime")?.as_str()?).ok()?;
            Some((start, end))
        })
        .collect();
    busy.sort();
    let mut cursor = DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T09:00:00Z")).ok()?;
    let close = DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T17:00:00Z")).ok()?;
    let slot = chrono::Duration::minutes(30);
    for (start, end) in busy {
        if cursor + slot <= start {
            break;
        }
        if end > cursor {
            cursor = end;
        }
    }
    if cursor + slot <= close {
        Some((format_instant(cursor), format_instant(cursor + slot)))
    } else {
        None
    }
}

/// The prose day view: one line per event plus the conflict pairs, which is
/// what a calendar coach is for.
fn day_summary(items: &[Value]) -> String {
    let mut lines = vec![format!(
        "Your {DEMO_DAY} ({} event{}):",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    )];
    for event in items {
        lines.push(format!(
            "- {}–{} · {}",
            hhmm(event_start(event)),
            hhmm(
                event
                    .pointer("/end/dateTime")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            event["summary"].as_str().unwrap_or("(untitled)")
        ));
    }
    let mut conflicts = Vec::new();
    for (index, a) in items.iter().enumerate() {
        for b in &items[index + 1..] {
            let (a_start, a_end) = (
                event_start(a),
                a.pointer("/end/dateTime")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let (b_start, b_end) = (
                event_start(b),
                b.pointer("/end/dateTime")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            if a_start < b_end && b_start < a_end {
                conflicts.push(format!(
                    "\"{}\" overlaps \"{}\"",
                    a["summary"].as_str().unwrap_or("(untitled)"),
                    b["summary"].as_str().unwrap_or("(untitled)")
                ));
            }
        }
    }
    if !conflicts.is_empty() {
        lines.push(format!("Conflicts: {}.", conflicts.join("; ")));
    }
    lines.join("\n")
}

/// The calendar manager: list the fixture day, then either book the first
/// verified free slot or summarize. A refused call (per-run allowlist) is
/// reported honestly rather than retried or papered over.
struct CalendarModel;

impl CalendarModel {
    fn wants_booking(user: &str) -> bool {
        let lower = user.to_lowercase();
        lower.contains("book") || lower.contains("schedule")
    }

    fn list_call() -> ChatMessage {
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "call_list_events",
            "google-calendar:list-events",
            json!({
                "calendar_id": "primary",
                "timeMin": format!("{DEMO_DAY}T00:00:00Z"),
                "timeMax": "2026-02-10T00:00:00Z",
                "singleEvents": true,
                "orderBy": "startTime"
            }),
        )])
    }
}

#[async_trait]
impl ChatModel for CalendarModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let (user, replies) = turn_progress(messages);
        let message = match replies.len() {
            0 => Self::list_call(),
            1 => {
                let listing = &replies[0];
                if is_tool_error(listing) {
                    ChatMessage::assistant(format!(
                        "I couldn't list {DEMO_DAY}'s calendar — the listing call was refused ({listing}), so there is nothing to summarize or book."
                    ))
                } else {
                    let items = serde_json::from_str::<Value>(listing)
                        .ok()
                        .and_then(|envelope| envelope.get("items").cloned())
                        .and_then(|items| items.as_array().cloned())
                        .unwrap_or_default();
                    if Self::wants_booking(&user) {
                        match first_free_slot(&items) {
                            Some((start, end)) => ChatMessage::assistant_tool_calls(vec![
                                ToolCall::new(
                                    "call_create_event",
                                    "google-calendar:create-event",
                                    json!({
                                        "calendar_id": "primary",
                                        "summary": BOOKING_SUMMARY,
                                        "description": "Booked by the harness demo calendar manager.",
                                        "start": {"dateTime": start, "timeZone": "UTC"},
                                        "end": {"dateTime": end, "timeZone": "UTC"},
                                    }),
                                ),
                            ]),
                            None => ChatMessage::assistant(format!(
                                "No free 30-minute slot remains in the 09:00–17:00 window on {DEMO_DAY}; nothing was booked."
                            )),
                        }
                    } else {
                        ChatMessage::assistant(day_summary(&items))
                    }
                }
            }
            _ => {
                let reply = replies.last().expect("two or more replies");
                if is_tool_error(reply) {
                    ChatMessage::assistant(
                        "I found the free slot, but the booking call was refused — `google-calendar:create-event` is not in this run's tool allowlist. Nothing was booked.",
                    )
                } else {
                    let event = serde_json::from_str::<Value>(reply).unwrap_or(Value::Null);
                    ChatMessage::assistant(format!(
                        "Booked \"{}\" at {}–{} (event {}).",
                        event["summary"].as_str().unwrap_or(BOOKING_SUMMARY),
                        event
                            .pointer("/start/dateTime")
                            .and_then(Value::as_str)
                            .unwrap_or("?"),
                        event
                            .pointer("/end/dateTime")
                            .and_then(Value::as_str)
                            .unwrap_or("?"),
                        event["id"].as_str().unwrap_or("?")
                    ))
                }
            }
        };
        respond(message, "rusty-harness-calendar")
    }
}

/// The ServiceNow operator: list open high-priority incidents, distill the
/// top theme into a KB draft, or answer KB read-back requests read-only.
struct ServiceNowModel;

impl ServiceNowModel {
    fn mentions_kb(user: &str) -> bool {
        let lower = user.to_lowercase();
        lower.contains("kb") || lower.contains("knowledge") || lower.contains("article")
    }

    fn wants_kb_write(user: &str) -> bool {
        Self::mentions_kb(user)
            && ["file", "draft", "create", "report", "submit", "write"]
                .iter()
                .any(|word| user.to_lowercase().contains(word))
    }

    fn list_call(table: &str, with_query: bool) -> ChatMessage {
        let mut args = json!({"table": table, "sysparm_limit": 20});
        if with_query {
            args["sysparm_query"] = json!("state=1^priority=1");
        }
        ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "call_list_records",
            "servicenow:list-records",
            args,
        )])
    }

    /// The list reply is the projected `/result`: a bare array of records.
    fn parse_records(reply: &str) -> Vec<Value> {
        serde_json::from_str::<Value>(reply)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
    }

    fn record_numbers(records: &[Value]) -> Vec<String> {
        records
            .iter()
            .filter_map(|record| record["number"].as_str().map(str::to_owned))
            .collect()
    }

    /// The deterministic theme: any VPN-titled incident makes the theme VPN.
    fn theme(records: &[Value]) -> &'static str {
        let vpn = records.iter().any(|record| {
            record["short_description"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("vpn")
        });
        if vpn {
            "VPN connectivity"
        } else {
            "general service"
        }
    }
}

#[async_trait]
impl ChatModel for ServiceNowModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let (user, replies) = turn_progress(messages);
        let wants_kb_write = Self::wants_kb_write(&user);
        let mentions_kb = Self::mentions_kb(&user);
        let message = match replies.len() {
            0 => Self::list_call(
                if mentions_kb && !wants_kb_write {
                    "kb_knowledge"
                } else {
                    "incident"
                },
                !mentions_kb || wants_kb_write,
            ),
            1 => {
                let listing = &replies[0];
                if is_tool_error(listing) {
                    ChatMessage::assistant(format!(
                        "The table read was refused or failed ({listing}); I can't report or file anything this turn."
                    ))
                } else if wants_kb_write {
                    let incidents = Self::parse_records(listing);
                    let numbers = Self::record_numbers(&incidents);
                    let theme = Self::theme(&incidents);
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "call_create_kb",
                        "servicenow:create-record",
                        json!({
                            "table": "kb_knowledge",
                            "short_description": format!("{theme} troubleshooting"),
                            "description": format!(
                                "Troubleshooting guidance for {theme}, distilled from the open high-priority incidents {}.",
                                numbers.join(", ")
                            ),
                            "category": "troubleshooting",
                            "work_notes": format!(
                                "Drafted by the harness demo operator from incidents {}.",
                                numbers.join(", ")
                            ),
                        }),
                    )])
                } else {
                    let records = Self::parse_records(listing);
                    let what = if mentions_kb {
                        "KB articles"
                    } else {
                        "incidents"
                    };
                    let mut lines = vec![format!("{what} on file ({}):", records.len())];
                    for record in &records {
                        lines.push(format!(
                            "- {} — {}",
                            record["number"].as_str().unwrap_or("(no number)"),
                            record["short_description"].as_str().unwrap_or("(untitled)")
                        ));
                    }
                    ChatMessage::assistant(lines.join("\n"))
                }
            }
            _ => {
                let reply = replies.last().expect("two or more replies");
                if is_tool_error(reply) {
                    ChatMessage::assistant(format!(
                        "The KB write was refused or failed ({reply}); no article was filed."
                    ))
                } else {
                    let created = serde_json::from_str::<Value>(reply).unwrap_or(Value::Null);
                    let incidents = Self::parse_records(&replies[0]);
                    ChatMessage::assistant(format!(
                        "Filed KB article {} (sys_id {}) on {}, drawn from incidents {}.",
                        created["number"].as_str().unwrap_or("?"),
                        created["sys_id"].as_str().unwrap_or("?"),
                        Self::theme(&incidents),
                        Self::record_numbers(&incidents).join(", ")
                    ))
                }
            }
        };
        respond(message, "rusty-harness-servicenow")
    }
}

/// The composer studio: draft the standup-brief skill, publish it under the
/// pre-minted approval, then prove the CLI tool runs read-only. A
/// "disallowed" ask drives one refused `rm` call instead.
struct ComposerModel {
    /// The approval token for the exact draft this model composes, minted at
    /// startup against `publish_effect_id("composer-studio", hash)`.
    approval: Value,
}

/// The skill the composer drafts. Fixed so the publish approval can be
/// minted before any run starts.
const COMPOSED_NAME: &str = "daily-standup-brief";
const COMPOSED_DESCRIPTION: &str =
    "Turn a morning's calendar and inbox notes into a standup brief.";
const COMPOSED_BODY: &str =
    "# Standup Brief\n\nList yesterday, today, and blockers, one line each.\n";

/// The SKILL.md text `ComposeSkillTool` assembles for these exact args —
/// the hash must match byte for byte, so the demo builds it with the same
/// format string rather than approximating it.
fn composed_skill_md() -> String {
    format!(
        "---\nname: {COMPOSED_NAME}\ndescription: {COMPOSED_DESCRIPTION}\n---\n\n{COMPOSED_BODY}\n"
    )
}

/// Mint the publish approval the way an operator would: hash the exact
/// package, derive the scoped publish effect id, approve it by name.
fn precompute_publish_approval() -> Result<Value> {
    let mut files = BTreeMap::new();
    files.insert("SKILL.md".to_owned(), composed_skill_md().into_bytes());
    let package = SkillPackage::from_files(files).map_err(|error| {
        RustyError::Tool(format!("the composed demo skill must parse: {error}"))
    })?;
    let effect_id = publish_effect_id("composer-studio", &package.content_hash());
    let token = ApprovalToken::approve(effect_id, "ops:harness-demo");
    serde_json::to_value(token)
        .map_err(|error| RustyError::Tool(format!("the approval token must serialize: {error}")))
}

#[async_trait]
impl ChatModel for ComposerModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let (user, replies) = turn_progress(messages);
        let disallowed = user.to_lowercase().contains("disallowed");
        let message = match replies.len() {
            0 if disallowed => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_rm",
                "run_cli",
                json!({"program": "rm", "args": ["-rf", "."], "cwd": ".", "timeout_ms": 1000}),
            )]),
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_compose",
                "compose_skill",
                json!({
                    "name": COMPOSED_NAME,
                    "description": COMPOSED_DESCRIPTION,
                    "body": COMPOSED_BODY,
                    "author": "agent:rusty"
                }),
            )]),
            1 if disallowed => ChatMessage::assistant(format!(
                "Refused: `run_cli` declined the command ({}) — only allowlisted, read-only programs run from this graph.",
                replies[0]
            )),
            1 => {
                let receipt = serde_json::from_str::<Value>(&replies[0]).unwrap_or(Value::Null);
                ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                    "call_publish",
                    "publish_composed_skill",
                    json!({
                        "content_hash": receipt["content_hash"].as_str().unwrap_or(""),
                        "approval": self.approval.clone()
                    }),
                )])
            }
            2 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_ls",
                "run_cli",
                json!({"program": "ls"}),
            )]),
            _ => {
                let receipt = serde_json::from_str::<Value>(&replies[1]).unwrap_or(Value::Null);
                ChatMessage::assistant(format!(
                    "Composed and published `{}` at revision {} (approved by {}), and listed the skills directory read-only.",
                    receipt["name"].as_str().unwrap_or(COMPOSED_NAME),
                    receipt["revision"].as_i64().unwrap_or(1),
                    receipt["approved_by"].as_str().unwrap_or("ops:harness-demo")
                ))
            }
        };
        respond(message, "rusty-harness-composer")
    }
}

// ---------------------------------------------------------------------------
// The self-improvement loop
// ---------------------------------------------------------------------------

/// The gaps the scripted self-improver records backlog entries for — picked
/// by id (not report position) so the journey asserts intent, not accident.
const SELF_IMPROVE_GAPS: [&str; 3] = [
    "surface-compaction",
    "telemetry-ledger",
    "agent-session-query",
];

/// The runbook skill the loop drafts for its pre-approved entry. The gap
/// (`operator-runbooks`) flips to `Present` only once a `runbook-*` skill is
/// really registered, so a staged-but-unpublished draft honestly changes
/// nothing in the next inspection.
const RUNBOOK_NAME: &str = "runbook-incident-review";
const RUNBOOK_DESCRIPTION: &str =
    "Review the open high-priority incidents and file a theme summary.";
const RUNBOOK_BODY: &str = "# Incident Review\n\n1. List the open priority-1 incidents.\n2. Group them by category and name the top theme.\n3. File a KB draft summarizing the theme.\n";

/// The seeded entry the demo operator pre-approves at startup (title and
/// rationale are the entry's identity — keep them byte-stable so restarts
/// converge on the same content-derived id).
const RUNBOOK_ENTRY_TITLE: &str = "Ship the incident-review runbook skill";
const RUNBOOK_ENTRY_RATIONALE: &str =
    "operator-runbooks is Absent: no `runbook-*` skill is registered, and the incident-review \
     workflow recurs across sessions — it belongs in a governed, scanned package.";

/// The self-improver: introspect the demo's own registries, record backlog
/// entries for the top gaps, then draft the approved runbook entry's skill
/// through the composer and stage its publish. The loop never publishes —
/// the approval gate stays with the operator, and the final message says so.
struct SelfImproverModel;

#[async_trait]
impl ChatModel for SelfImproverModel {
    async fn chat(&self, messages: &[ChatMessage], _tools: &[Value]) -> Result<ChatResponse> {
        let (_user, replies) = turn_progress(messages);
        let message = match replies.len() {
            0 => ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "call_inspect",
                "inspect_capabilities",
                json!({}),
            )]),
            1 => {
                let report = &replies[0];
                if is_tool_error(report) {
                    ChatMessage::assistant(format!(
                        "I couldn't introspect the harness ({report}); without the gap report there is nothing honest to record."
                    ))
                } else {
                    let report = serde_json::from_str::<Value>(report).unwrap_or(Value::Null);
                    let entries: Vec<Value> = SELF_IMPROVE_GAPS
                        .iter()
                        .map(|gap| {
                            let description = report["assessments"]
                                .as_array()
                                .and_then(|assessments| {
                                    assessments.iter().find(|a| a["id"] == json!(gap))
                                })
                                .and_then(|a| a["description"].as_str())
                                .unwrap_or(gap);
                            json!({
                                "title": format!("Close the `{gap}` gap"),
                                "rationale": description,
                                "gap_ids": [gap]
                            })
                        })
                        .collect();
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "call_propose",
                        "propose_backlog_entries",
                        json!({"entries": entries}),
                    )])
                }
            }
            2 => {
                let recorded = &replies[1];
                if is_tool_error(recorded) {
                    ChatMessage::assistant(format!(
                        "The backlog refused my proposals ({recorded}); I won't draft against an unrecorded gap."
                    ))
                } else {
                    ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                        "call_build",
                        "build_gap_skill",
                        json!({
                            "gap_id": "operator-runbooks",
                            "name": RUNBOOK_NAME,
                            "description": RUNBOOK_DESCRIPTION,
                            "body": RUNBOOK_BODY,
                            "author": HARNESS_PROVENANCE
                        }),
                    )])
                }
            }
            _ => {
                let staged = &replies[2];
                if is_tool_error(staged) {
                    ChatMessage::assistant(format!(
                        "The runbook draft was refused ({staged}); nothing was staged, and the gap stays open."
                    ))
                } else {
                    let report = serde_json::from_str::<Value>(&replies[0]).unwrap_or(Value::Null);
                    let recorded =
                        serde_json::from_str::<Value>(&replies[1]).unwrap_or(Value::Null);
                    let staged = serde_json::from_str::<Value>(staged).unwrap_or(Value::Null);
                    ChatMessage::assistant(format!(
                        "Inspection: {} present, {} partial, {} absent. Recorded {} backlog entries \
                         (harness:self-improve, all `proposed`). Drafted `{RUNBOOK_NAME}` for the \
                         approved runbook entry — publish is staged behind effect id {} and awaits \
                         an operator approval; the gate stays with the operator.",
                        report["present"].as_u64().unwrap_or(0),
                        report["partial"].as_u64().unwrap_or(0),
                        report["absent"].as_u64().unwrap_or(0),
                        recorded["recorded"].as_array().map_or(0, Vec::len),
                        staged["publish_effect_id"].as_str().unwrap_or("(none)")
                    ))
                }
            }
        };
        respond(message, "rusty-harness-self-improver")
    }
}

// ---------------------------------------------------------------------------
// Experiment evaluator
// ---------------------------------------------------------------------------

/// A deterministic all-pass evaluator: the demo's experiment lane proves
/// the dataset → candidate → experiment → comparison plumbing end to end
/// without depending on model behavior. Both reports are identical, so a
/// regression verdict would be a comparison bug, not a flake.
#[derive(Debug)]
struct HarnessEvaluator;

/// The exact `ExperimentReport` wire shape, built as JSON and deserialized
/// so a schema drift fails here at startup instead of inside the lane.
fn harness_report(
    dataset: &Dataset,
    config: &StudioExperimentConfig,
    name: &str,
) -> ExperimentReport {
    let cases: Vec<Value> = dataset
        .cases()
        .iter()
        .map(|case| {
            let runs: Vec<Value> = (0..config.runs_per_case)
                .map(|repetition| {
                    json!({
                        "repetition": repetition,
                        "status": {"status": "done"},
                        "passed": true,
                        "assertions": [],
                        "judge": null,
                        "tool_calls": 0,
                        "latency_ms": 10,
                        "cost_usd": 0.001,
                        "total_tokens": 10
                    })
                })
                .collect();
            json!({
                "case_id": case.id,
                "tags": case.tags,
                "pass_rate": 1.0,
                "runs": runs
            })
        })
        .collect();
    let total_runs = dataset.cases().len() * config.runs_per_case;
    serde_json::from_value(json!({
        "format_version": 1,
        "name": name,
        "dataset_name": dataset.name(),
        "dataset_version": dataset.version(),
        "runs_per_case": config.runs_per_case,
        "max_concurrency": config.max_concurrency,
        "cases": cases,
        "summary": {
            "cases": dataset.cases().len(), "runs": total_runs, "runs_passed": total_runs,
            "run_pass_rate": 1.0, "case_pass_rate": 1.0,
            "assertions": [],
            "latency_ms": {"min": 10, "p50": 10, "p95": 10, "max": 10, "mean": 10.0},
            "total_cost_usd": total_runs as f64 * 0.001,
            "total_tokens": total_runs * 10
        }
    }))
    .expect("the harness report shape matches ExperimentReport")
}

#[async_trait]
impl StudioExperimentEvaluator for HarnessEvaluator {
    async fn evaluate(
        &self,
        _candidate: &Candidate,
        dataset: &Dataset,
        config: &StudioExperimentConfig,
    ) -> std::result::Result<ExperimentOutcome, String> {
        Ok(ExperimentOutcome {
            baseline_report: harness_report(dataset, config, "serving-baseline"),
            candidate_report: harness_report(dataset, config, "candidate"),
        })
    }
}

// ---------------------------------------------------------------------------
// Graph assembly
// ---------------------------------------------------------------------------

/// `calendar_manager`: the Google Calendar pack over the fixture transport.
fn build_calendar_graph() -> Result<(Graph, StateSpec, ToolRegistry)> {
    let manifest = packs::google_calendar()?;
    let provider = HttpApiProvider::from_manifest(&manifest)?;
    let transport: Arc<dyn HttpApiTransport> = Arc::new(CalendarFixture::seeded());
    let mut tools = ToolRegistry::new();
    for operation in [
        "list-calendars",
        "list-events",
        "get-event",
        "create-event",
        "update-event",
        "delete-event",
    ] {
        tools.register(CatalogTool::mount(HttpApiTool::new(
            "google-calendar",
            provider.clone(),
            operation,
            transport.clone(),
            vec![CredentialHandle::new(
                "harness-demo",
                "access_token",
                "fixture-calendar-token",
            )?],
            "harness-demo",
        )?));
    }
    let model: Arc<dyn ChatModel> = Arc::new(CalendarModel);
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// `servicenow_operator`: the ServiceNow pack over the fixture tenant.
fn build_servicenow_graph() -> Result<(Graph, StateSpec, ToolRegistry)> {
    let manifest = packs::servicenow("harness")?;
    let provider = HttpApiProvider::from_manifest(&manifest)?;
    let transport: Arc<dyn HttpApiTransport> = Arc::new(ServiceNowFixture::seeded());
    let credentials = || -> Result<Vec<CredentialHandle>> {
        Ok(vec![
            CredentialHandle::new("harness-demo", "username", "fixture-user")?,
            CredentialHandle::new("harness-demo", "password", "fixture-password")?,
        ])
    };
    let mut tools = ToolRegistry::new();
    for operation in [
        "list-records",
        "get-record",
        "create-record",
        "update-record",
        "delete-record",
    ] {
        tools.register(CatalogTool::mount(HttpApiTool::new(
            "servicenow",
            provider.clone(),
            operation,
            transport.clone(),
            credentials()?,
            "harness-demo",
        )?));
    }
    let model: Arc<dyn ChatModel> = Arc::new(ServiceNowModel);
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// `composer_studio`: the compose → approval-gated publish lane plus a
/// read-only, allowlisted `run_cli` jailed to the skills directory.
fn build_composer_graph(
    skills: Arc<Mutex<SkillRegistry>>,
) -> Result<(Graph, StateSpec, ToolRegistry)> {
    let session = ComposerSession::new("composer-studio");
    let mut tools = ToolRegistry::new();
    tools.register(ComposeSkillTool::new(session.clone()));
    tools.register(PublishComposedSkillTool::new(session, skills));
    tools.register(ComposeToolDefinitionTool::new(vec!["ls".to_owned()])?);
    let cli_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/harness_skills");
    tools.register(CliTool::new(
        CliPolicy::new(cli_root, ["ls"])?.with_read_only(true),
    ));
    let model: Arc<dyn ChatModel> = Arc::new(ComposerModel {
        approval: precompute_publish_approval()?,
    });
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// `self_improver`: the introspect → backlog → draft-and-stage loop. The
/// inspection closure is the honesty seam — it reads the demo's live skill
/// registry and the tool/manifest lists assembled from the demo's real
/// registries, so the gap report can never claim more than the demo wires.
/// The backlog store is seeded (in `main`) with one operator-approved
/// runbook entry; everything else the loop proposes lands as `proposed`.
fn build_self_improver_graph(
    backlog: Arc<BacklogStore>,
    skills: Arc<Mutex<SkillRegistry>>,
    host_tool_names: Vec<String>,
    connector_manifest_ids: Vec<String>,
) -> Result<(Graph, StateSpec, ToolRegistry)> {
    let session = ComposerSession::new("self-improver");
    let mut tool_names = host_tool_names;
    tool_names.extend(
        [
            "inspect_capabilities",
            "propose_backlog_entries",
            "build_gap_skill",
        ]
        .iter()
        .map(|name| (*name).to_owned()),
    );
    let inspect = Arc::new(move || CapabilityInspection {
        skill_names: skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .names()
            .map(str::to_owned)
            .collect(),
        connector_manifest_ids: connector_manifest_ids.clone(),
        tool_names: tool_names.clone(),
        // The demo binary wires these planes for real: the registries and
        // manifests above, the server's memory/knowledge endpoints, the
        // Flight Recorder journaling every run, and per-run tool
        // allowlists (journeys 3, 5, and 6 are the standing evidence).
        planes: vec![
            Plane::Skills,
            Plane::Connectors,
            Plane::Knowledge,
            Plane::Memory,
            Plane::Evidence,
            Plane::Tools,
        ],
        features: vec![FEATURE_CAPABILITY_SETS.to_owned()],
    });
    let mut tools = ToolRegistry::new();
    tools.register(InspectCapabilitiesTool::new(inspect));
    tools.register(ProposeBacklogTool::new(
        Arc::clone(&backlog),
        // A logical clock on the fixture day keeps the demo's journaled
        // evidence deterministic.
        Clock::logical(
            DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T09:00:00Z"))
                .expect("the fixture day parses")
                .timestamp_millis() as u64,
            60_000,
        ),
    ));
    tools.register(BuildGapSkillTool::new(backlog, session));
    let model: Arc<dyn ChatModel> = Arc::new(SelfImproverModel);
    let graph = create_react_agent(model, tools.clone())?;
    let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
    Ok((graph, spec, tools))
}

/// Seed the backlog with the demo operator's one pre-approved entry —
/// converging on restart (insertion is idempotent; an entry that already
/// moved on is left where it is).
async fn seed_backlog(backlog: &BacklogStore) -> Result<()> {
    let proposed = BacklogEntry::new(
        RUNBOOK_ENTRY_TITLE,
        RUNBOOK_ENTRY_RATIONALE,
        &["operator-runbooks".to_owned()],
        BacklogProvenance::operator("harness-demo")?,
        DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T08:00:00Z"))
            .expect("the fixture day parses")
            .with_timezone(&chrono::Utc),
    )?;
    if backlog.get(&proposed.id).is_none() {
        backlog.insert(proposed.clone()).await?;
        backlog
            .transition(
                &proposed.id,
                BacklogStatus::Approved,
                None,
                DateTime::parse_from_rfc3339(&format!("{DEMO_DAY}T08:05:00Z"))
                    .expect("the fixture day parses")
                    .with_timezone(&chrono::Utc),
            )
            .await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let (calendar, calendar_spec, calendar_tools) = build_calendar_graph()?;
    let (servicenow, servicenow_spec, servicenow_tools) = build_servicenow_graph()?;
    let skills = Arc::new(Mutex::new(SkillRegistry::new()));
    let (composer, composer_spec, composer_tools) = build_composer_graph(Arc::clone(&skills))?;

    // The self-improver's backlog lives under the same store root the
    // checkpointer uses; the operator's pre-approved runbook entry is
    // seeded before the graph is built.
    let store_root = std::env::var("RUSTY_HARNESS_STORE")
        .unwrap_or_else(|_| "./data/harness-demo-checkpoints".to_string());
    let backlog = Arc::new(
        BacklogStore::open(Path::new(&store_root).join("self-improve-backlog.json")).await?,
    );
    seed_backlog(&backlog).await?;
    let host_tool_names: Vec<String> = [&calendar_tools, &servicenow_tools, &composer_tools]
        .into_iter()
        .flat_map(|tools| tools.names().map(str::to_owned).collect::<Vec<_>>())
        .collect();
    let connector_manifest_ids = vec![
        packs::google_calendar()?.id,
        packs::servicenow("harness")?.id,
    ];
    let (self_improver, self_improver_spec, self_improver_tools) =
        build_self_improver_graph(backlog, skills, host_tool_names, connector_manifest_ids)?;

    let mut registry = GraphRegistry::new();
    registry.register_with_tools("calendar_manager", calendar, calendar_spec, &calendar_tools)?;
    registry.register_with_tools(
        "servicenow_operator",
        servicenow,
        servicenow_spec,
        &servicenow_tools,
    )?;
    registry.register_with_tools("composer_studio", composer, composer_spec, &composer_tools)?;
    registry.register_with_tools(
        "self_improver",
        self_improver,
        self_improver_spec,
        &self_improver_tools,
    )?;

    let config = ServerConfig::new(
        std::env::var("RUSTY_HARNESS_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8110".to_string())
            .parse()
            .expect("RUSTY_HARNESS_ADDR must be a socket address"),
        std::env::var("RUSTY_HARNESS_STORE")
            .unwrap_or_else(|_| "./data/harness-demo-checkpoints".to_string()),
    )
    .with_studio_experiment_evaluator(Arc::new(HarnessEvaluator));

    // The menu below is printed with the *actual* address so the test-hook
    // override stays honest when a human runs the demo with it set.
    let base = format!("localhost:{}", config.bind_addr.port());
    println!("\nrusty harness demo on http://{base}\n");
    println!("  Graphs: calendar_manager, servicenow_operator, composer_studio, self_improver");
    println!("  The models are scripted and the service tools run against in-process");
    println!("  fixtures (no network, no credentials); writes persist for the");
    println!("  process lifetime, so follow-up runs observe earlier bookings.\n");
    println!("  # liveness + registered graphs and their tool catalogs");
    println!("  curl {base}/ok");
    println!("  curl {base}/info | jq\n");
    println!("  # the calendar manager books the first verified free slot");
    println!("  THREAD=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"calendar_manager\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$THREAD/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Show my day and book a 30-minute slot.\"}}]}}}}' | jq\n");
    println!("  # the same booking under a per-run tool allowlist (create is refused,");
    println!("  # the run still completes, and the refusal is journaled)");
    println!("  curl -s -X POST {base}/threads/$THREAD/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Show my day and book a 30-minute slot.\"}}]}}, \"config\": {{\"tool_allowlist\": [\"google-calendar:list-events\"]}}}}' | jq\n");
    println!("  # the ServiceNow operator files a KB article from the open incidents");
    println!("  SN=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"servicenow_operator\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$SN/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Show open high-priority incidents and file a KB article about the top theme.\"}}]}}}}' | jq\n");
    println!("  # the composer drafts a skill, publishes it under its pre-minted");
    println!("  # approval, and proves run_cli is read-only and allowlisted");
    println!("  STUDIO=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"composer_studio\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$STUDIO/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Compose the standup brief skill, publish it, and list the skills directory.\"}}]}}}}' | jq\n");
    println!("  # the self-improver introspects its own capabilities, records backlog");
    println!("  # entries for the top gaps, and stages a runbook skill behind the");
    println!("  # composer's approval gate (publishing stays with the operator)");
    println!("  LOOP=$(curl -s -X POST {base}/threads \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"graph\": \"self_improver\"}}' | jq -r .thread_id)");
    println!("  curl -s -X POST {base}/threads/$LOOP/runs/wait \\");
    println!("    -H 'content-type: application/json' \\");
    println!("    -d '{{\"input\": {{\"messages\": [{{\"role\": \"user\", \"content\": \"Inspect your capabilities, record the top gaps, and stage the runbook skill.\"}}]}}}}' | jq\n");
    println!("  # every run's journaled evidence (run_id is in the terminal JSON)");
    println!("  curl -s {base}/runs/$RUN_ID/events | jq\n");
    println!("  # the governed surfaces the flow test drives: skills, memory,");
    println!("  # knowledge, datasets / candidates / experiments");
    println!("  curl -s {base}/skills | jq");
    println!("  curl -s -X POST {base}/memory/query \\");
    println!("    -H 'content-type: application/json' -d '{{}}' | jq");
    println!("  curl -s {base}/experiments | jq\n");

    serve(registry, config).await?;
    Ok(())
}
