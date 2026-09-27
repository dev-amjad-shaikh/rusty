//! Worlds — a stand-in for a connected system that a suite can reset.
//!
//! A suite cannot take a write's create path against a live system more
//! than once: the first evaluation leaves the record the next one finds.
//! A *world* is the server's own twin of one connection: it speaks the
//! system's wire dialect (ServiceNow's Table API today), holds the records
//! a suite seeds it with, and goes back to that seed on reset. A run whose
//! execution names a world has its connector calls answered by the world,
//! in-process, before egress — the desk's tools keep their names, the
//! connection keeps its credential, and nothing reaches the live system.
//! A case tagged `world:<name>` resets the world, then runs in it.
//!
//! What the world is not: a judge of realism. It answers the calls the
//! connector's operations make, with the shapes the live system uses, and
//! nothing a suite did not seed.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use rusty_agent_runtime::connector::{CheckRequest, CheckResponse, HttpMethod};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::server_store::ServerStore;

const NAMESPACE_PREFIX: &str = "worlds";
/// Rows a world keeps per table at most: a suite's seed, not a data lake.
const MAX_ROWS_PER_TABLE: usize = 5_000;
/// Rows the record view returns per table.
const SHOWN_ROWS: usize = 50;

/// The dialects a world can speak: the wire shape of one system family.
pub const DIALECTS: &[Dialect] = &[
    Dialect {
        id: "servicenow-table",
        name: "ServiceNow Table API",
        fits: &["servicenow"],
        summary: "GET/POST/PATCH /api/now/table/{table} with sysparm_query, sysparm_fields, sysparm_limit; /api/now/stats counts; the service catalog's items and order_now. Records get sys_id, number (INC…, PRB…, REQ…), opened_at, sys_created_on.",
        seed_shape: "{\"tables\": {<table>: [<record>, …]}}, beside `counters` giving the next number each record type is filed under. Optional `faults`: [{\"drop_response\": \"POST /api/now/table/incident\", \"times\": 1}, {\"delay\": \"POST /api/now/table/problem\", \"seconds\": 30}] — the world applies each matching call and then loses its answer, or holds it that many seconds, that many times per reset, the way a wire does.",
    },
    Dialect {
        id: "ledger-api",
        name: "Ledger API (paged, versioned schema)",
        fits: &["ledger"],
        summary: "GET /v1/schema names the fields this version has; GET /v1/entries pages with cursor and limit and answers next_cursor until the last page. A call naming a field the current schema does not have is refused with the fields that exist, so an agent can discover the change and repair.",
        seed_shape: "{\"tables\": {\"entries\": [<entry>, …]}} beside three knobs: `schema_version` (\"v1\" or \"v2\" — four fields are renamed between them), `page_size` (the most entries one call may take, however large a limit it asks for), and `schema_changes_after: <n>` (the world moves from v1 to v2 once it has answered n listing calls, so the schema changes under a run; leave it out and the version never moves). Optional `faults` as for any world: [{\"drop_response\": \"GET /v1/entries\", \"times\": 1}] loses the answer to a matching call after it was served; {\"delay\": …, \"seconds\": 30} holds it.",
    },
    Dialect {
        id: "slack-web",
        name: "Slack Web API",
        fits: &["slack"],
        summary: "GET /api/auth.test, /api/conversations.list, /api/users.list, /api/conversations.history?channel=…; POST /api/chat.postMessage with channel (a #name or a C… id) and text. Answers the way Slack does: 200 with ok true, or ok false and an error word (channel_not_found, no_text). A posted message lands in `messages` with a ts, the channel's id and name, and which run wrote it.",
        seed_shape: "{\"tables\": {\"channels\": [{\"id\": \"C…\", \"name\": \"general\", \"is_member\": true}, …], \"users\": [{\"id\": \"U…\", \"name\": …}], \"messages\": [{\"channel\": \"C…\", \"text\": …, \"ts\": …}]}}. Optional `faults` as for any world: [{\"drop_response\": \"POST /api/chat.postMessage\", \"times\": 1}] loses the answer to a post after it landed; {\"delay\": …, \"seconds\": 30} holds it.",
    },
    Dialect {
        id: "manifest-rest",
        name: "The connector's own operations (REST)",
        fits: &[],
        summary: "Stands in for any connector by its manifest: each operation's method and path answer from a table named after the resource the path addresses (…/issues → issues; a {table} parameter names it). A read of a collection answers its rows, filtered by the path's other parameters and any query parameter that names a field; a read with a trailing parameter answers the one record it names (by that field, or by number or id); a POST files a new record with a number and an id; PATCH and PUT change one; DELETE removes one. Made from a connection, so the operations are the connector's own.",
        seed_shape: "{\"tables\": {<resource>: [<record>, …]}} — one table per resource the connector's operations address (issues, pulls, repos…), each created empty when missing; a record carries the fields the API would return, and the path's parameters (owner, repo) as fields to be found by. Optional `counters` per resource for the next number, and `faults` as for any world ([{\"drop_response\": \"POST /repos/{owner}/{repo}/issues\"}] loses a write's answer once).",
    },
];

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Dialect {
    pub id: &'static str,
    pub name: &'static str,
    /// Connector ids (manifest ids) this dialect stands in for.
    pub fits: &'static [&'static str],
    pub summary: &'static str,
    /// What a seed of this dialect holds, for the person editing one. The
    /// starter shows the shape; this says what every part of it does,
    /// including the settings no starter can demonstrate by example.
    pub seed_shape: &'static str,
}

impl Dialect {
    fn by_id(id: &str) -> Option<&'static Dialect> {
        DIALECTS.iter().find(|d| d.id == id)
    }

    /// The dialect for a connector: the one written for it, else the one
    /// that speaks any manifest.
    fn for_connector(connector: &str) -> Option<&'static Dialect> {
        DIALECTS
            .iter()
            .find(|d| !d.fits.is_empty() && d.fits.iter().any(|f| connector.starts_with(f)))
            .or_else(|| DIALECTS.iter().find(|d| d.fits.is_empty()))
    }

    /// A seed a world of this dialect starts from when the person supplies
    /// none: enough for a desk to find something, and file something.
    pub fn starter(&self) -> Value {
        match self.id {
            "slack-web" => json!({
                "tables": {
                    "channels": [
                        {"id": "C0GENERAL1", "name": "general", "is_channel": true, "is_member": true, "topic": "Company-wide announcements"},
                        {"id": "C0OPSROOM1", "name": "ops", "is_channel": true, "is_member": true, "topic": "Incidents and the morning brief"}
                    ],
                    "users": [
                        {"id": "U0ADMIN001", "name": "admin", "real_name": "System Administrator", "is_bot": false},
                        {"id": "U0RUSTYBOT", "name": "rusty", "real_name": "Rusty", "is_bot": true}
                    ],
                    "messages": [
                        {"ts": "1757746800.000100", "channel": "C0OPSROOM1", "channel_name": "ops", "user": "U0ADMIN001", "text": "Reminder: the morning brief goes out at 09:00."}
                    ]
                }
            }),
            "servicenow-table" => json!({
                "tables": {
                    "sys_user": [
                        {"sys_id": "6816f79cc0a8016401c5a33be04be441", "user_name": "admin", "name": "System Administrator", "email": "admin@example.com", "active": "true"},
                        {"sys_id": "62826bf03710200044e0bfc8bcbe5df1", "user_name": "abel.tuter", "name": "Abel Tuter", "email": "abel.tuter@example.com", "active": "true"}
                    ],
                    "sys_user_group": [
                        {"sys_id": "8a5055c9c61122780043563ef53438e3", "name": "Hardware", "active": "true"},
                        {"sys_id": "287ebd7da9fe198100f92cc8d1d2154e", "name": "Network", "active": "true"},
                        {"sys_id": "d625dccec0a8016700a222a0f7900d06", "name": "Service Desk", "active": "true"}
                    ],
                    "incident": [
                        {"sys_id": "a83820b58f723300e7e16c7827bdeed2", "number": "INC0010001", "short_description": "Badge reader at the north entrance rejects every card", "description": "Since 07:30 the badge reader at the north entrance rejects every card; staff are being let in by security.", "category": "hardware", "priority": "2", "impact": "2", "urgency": "2", "state": "2", "active": "true", "assignment_group": "Hardware", "assigned_to": "", "caller_id": "Abel Tuter", "opened_at": "2026-09-01 07:41:12", "sys_created_on": "2026-09-01 07:41:12", "sys_updated_on": "2026-09-01 08:02:00"},
                        {"sys_id": "c83820b58f723300e7e16c7827bdeed3", "number": "INC0010002", "short_description": "VPN drops every few minutes from home", "description": "The VPN client disconnects every 5-10 minutes when working from home; office is fine.", "category": "network", "priority": "3", "impact": "3", "urgency": "3", "state": "1", "active": "true", "assignment_group": "Network", "assigned_to": "", "caller_id": "Abel Tuter", "opened_at": "2026-09-02 09:15:40", "sys_created_on": "2026-09-02 09:15:40", "sys_updated_on": "2026-09-02 09:15:40"},
                        {"sys_id": "e83820b58f723300e7e16c7827bdeed4", "number": "INC0010003", "short_description": "Printer on floor 3 prints blank pages", "description": "The printer on floor 3 prints blank pages; toner shows 40%.", "category": "hardware", "priority": "4", "impact": "3", "urgency": "3", "state": "6", "active": "false", "assignment_group": "Hardware", "assigned_to": "System Administrator", "caller_id": "Abel Tuter", "opened_at": "2026-08-28 14:03:10", "sys_created_on": "2026-08-28 14:03:10", "sys_updated_on": "2026-08-29 10:00:00", "resolved_by": "System Administrator", "close_code": "Solved (Permanently)", "close_notes": "Replaced the drum unit."}
                    ],
                    "sc_cat_item": [
                        {"sys_id": "04b7e94b4f7b4200086eeed18110c7fd", "name": "Standard Laptop", "short_description": "A standard-issue laptop for new hires and replacements.", "price": "1200", "active": "true"},
                        {"sys_id": "e28b4e5f4f7b4200086eeed18110c7a2", "name": "VPN Access", "short_description": "Remote access to the corporate network.", "price": "0", "active": "true"}
                    ]
                },
                "counters": {"incident": 10004, "problem": 40001, "sc_request": 10001, "change_request": 30001}
            }),
            "ledger-api" => {
                // Sixty entries over three pages at the default size, and a
                // schema the world can be seeded at either version of.
                let parties = [
                    "Northwind Freight",
                    "Acme Supplies",
                    "Kestrel Media",
                    "Blue Harbour Ltd",
                    "Orsted Tooling",
                    "Vantage Analytics",
                ];
                let states = [
                    "posted", "pending", "posted", "reversed", "posted", "pending",
                ];
                let entries: Vec<Value> = (1..=60)
                    .map(|n: u64| {
                        json!({
                            "id": format!("LE-{n:05}"),
                            "date": format!("2026-{:02}-{:02}", 7 + (n % 3), 1 + (n % 28)),
                            "party": parties[(n as usize) % parties.len()],
                            "amount": format!("{}.{:02}", 40 + (n * 17) % 900, (n * 7) % 100),
                            "currency": if n.is_multiple_of(5) { "EUR" } else { "GBP" },
                            "status": states[(n as usize) % states.len()],
                        })
                    })
                    .collect();
                json!({"schema_version": "v1", "page_size": 25, "tables": {"entries": entries}})
            }
            _ => json!({"tables": {}}),
        }
    }
}

/// One world, as the store keeps it: what it stands in for, its seed, and
/// its live state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldRecord {
    pub world_id: String,
    pub tenant: String,
    /// The person's name for it, unique in the tenant (`world:<name>` on a case).
    pub name: String,
    /// The connector (manifest id) whose calls it answers.
    pub connector: String,
    /// The connection it stands in for, when made from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// The live system's host the connection calls — the requests the world
    /// answers are the ones addressed there.
    pub stands_for: String,
    pub dialect: String,
    /// The connector's operations, for a dialect that answers by them —
    /// name, method and path template, read from the manifest at creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<generic::OpShape>,
    pub seed: Value,
    #[serde(default)]
    pub state: Value,
    pub created_by: Value,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub reset_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reset_at: Option<DateTime<Utc>>,
    /// Calls the world answered since its last reset.
    #[serde(default)]
    pub calls_since_reset: u64,
}

fn namespace(tenant: &str) -> String {
    format!("{NAMESPACE_PREFIX}:{tenant}")
}

/// The worlds of every tenant, kept in the store and answered in-process.
pub struct WorldPlane {
    store: Arc<dyn ServerStore>,
    /// One lock per call: a world's state changes under it, and a reset
    /// never interleaves with a call.
    lock: Mutex<()>,
}

impl std::fmt::Debug for WorldPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorldPlane").finish_non_exhaustive()
    }
}

impl WorldPlane {
    pub(crate) fn new(store: Arc<dyn ServerStore>) -> Self {
        Self {
            store,
            lock: Mutex::new(()),
        }
    }

    async fn load(&self, tenant: &str, world_id: &str) -> Result<Option<WorldRecord>, String> {
        let item = self
            .store
            .kv_get(&namespace(tenant), world_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(item.and_then(|i| serde_json::from_value(i.value).ok()))
    }

    async fn keep(&self, record: &WorldRecord) -> Result<(), String> {
        let value = serde_json::to_value(record).map_err(|e| e.to_string())?;
        self.store
            .kv_put(&namespace(&record.tenant), &record.world_id, value)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub async fn list(&self, tenant: &str) -> Result<Vec<WorldRecord>, String> {
        let items = self
            .store
            .kv_list(&namespace(tenant))
            .await
            .map_err(|e| e.to_string())?;
        let mut worlds: Vec<WorldRecord> = items
            .into_iter()
            .filter_map(|i| serde_json::from_value(i.value).ok())
            .collect();
        worlds.sort_by_key(|a| a.created_at);
        Ok(worlds)
    }

    /// A world by its id or its name.
    pub async fn find(
        &self,
        tenant: &str,
        id_or_name: &str,
    ) -> Result<Option<WorldRecord>, String> {
        if let Some(found) = self.load(tenant, id_or_name).await? {
            return Ok(Some(found));
        }
        Ok(self
            .list(tenant)
            .await?
            .into_iter()
            .find(|w| w.name == id_or_name))
    }

    /// Back to the seed. Returns the record after.
    pub async fn reset(&self, tenant: &str, world_id: &str) -> Result<WorldRecord, String> {
        let _guard = self.lock.lock().await;
        let mut record = self
            .load(tenant, world_id)
            .await?
            .ok_or_else(|| format!("unknown world `{world_id}`"))?;
        record.state = record.seed.clone();
        record.reset_count += 1;
        record.last_reset_at = Some(Utc::now());
        record.calls_since_reset = 0;
        self.keep(&record).await?;
        Ok(record)
    }

    /// Answer `request` from world `world_id` when the request is addressed
    /// to the host it stands in for; `None` when it is not this world's to
    /// answer (the request goes to the wire as usual). A world seeded with
    /// `faults` applies a matching call and then loses its answer, the
    /// error the wire gives when a response never comes back — so a suite
    /// can put a lost write in front of an agent on purpose, and again
    /// after every reset.
    pub async fn answer(
        &self,
        tenant: &str,
        world_id: &str,
        request: &CheckRequest,
    ) -> Option<rusty_agent_runtime::error::Result<CheckResponse>> {
        let guard = self.lock.lock().await;
        let mut record = match self.load(tenant, world_id).await {
            Ok(Some(record)) => record,
            // A run that names a world it cannot be answered by is refused
            // the call, never handed to the wire: what was meant for a
            // stand-in must not land in the system it stands in for.
            Ok(None) => {
                tracing::warn!(world = %world_id, "a run names a world the tenant does not hold; the call is refused");
                return Some(Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                    "the run is in world `{world_id}`, which no longer exists; the call was not sent — the world was deleted or the run was put in one nobody holds"
                ))));
            }
            Err(error) => {
                tracing::warn!(world = %world_id, %error, "world could not be read; the call is refused");
                return Some(Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                    "the run is in world `{world_id}`, which could not be read ({error}); the call was not sent"
                ))));
            }
        };
        let url = reqwest::Url::parse(&request.url).ok()?;
        if !url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&record.stands_for))
        {
            return None;
        }
        let path = url.path().to_owned();
        let query: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let body = request
            .body
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Value>(b).ok());
        // The run this call belongs to, so a row it creates can say so.
        let written_by = rusty_agent_runtime::tool::current_run().map(|ctx| json!({"run_id": ctx.run_id, "at": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()}));
        let (status, answer) = match record.dialect.as_str() {
            "servicenow-table" => servicenow::serve(
                &mut record.state,
                request.method,
                &path,
                &query,
                body.as_ref(),
            ),
            "ledger-api" => ledger::serve(&mut record.state, request.method, &path, &query),
            "slack-web" => slack::serve(
                &mut record.state,
                request.method,
                &path,
                &query,
                body.as_ref(),
                written_by.as_ref(),
            ),
            "manifest-rest" => generic::serve(
                &mut record.state,
                &record.operations,
                request.method,
                &path,
                &query,
                body.as_ref(),
                written_by.as_ref(),
            ),
            other => (
                501,
                json!({"error": {"message": format!("the world speaks no dialect `{other}`")}}),
            ),
        };
        record.calls_since_reset += 1;
        let fault = spend_fault(&mut record.state, method_word(request.method), &path);
        if let Err(error) = self.keep(&record).await {
            tracing::warn!(world = %world_id, %error, "world state could not be kept");
        }
        // The call is applied and kept; what follows is the wire's
        // misbehaviour, and no other world call waits on it.
        drop(guard);
        match fault {
            Some(FaultKind::DropResponse) => {
                tracing::info!(world = %record.name, method = ?request.method, %path, status, "world applied the call and lost its answer, as seeded");
                return Some(Err(rusty_agent_runtime::error::RustyError::Transport {
                    sent: true,
                    detail: format!(
                        "connector transport: (world `{}` fault) the answer was lost after the request was sent",
                        record.name
                    ),
                }));
            }
            Some(FaultKind::Delay(seconds)) => {
                tracing::info!(world = %record.name, method = ?request.method, %path, status, seconds, "world applied the call and holds its answer, as seeded");
                tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            }
            None => {}
        }
        tracing::debug!(world = %record.name, method = ?request.method, %path, status, "world answered");
        Some(Ok(CheckResponse {
            status,
            body: serde_json::to_vec(&answer).unwrap_or_default(),
        }))
    }
}

/// What a seeded fault does to a matching call, after the world has
/// applied it: lose the answer, or hold it for that many seconds — the
/// two ways a wire misbehaves after a request was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    DropResponse,
    Delay(u64),
}

fn method_word(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
    }
}

/// One seeded fault: `{"drop_response": "POST /api/now/table/incident",
/// "times": 1}` loses the answer; `{"delay": "POST /api/now/table/problem",
/// "seconds": 30}` holds it. The method, the path prefix, the kind, and
/// how many matching calls it applies to before the world answers
/// normally (one when unsaid).
fn parse_fault(fault: &Value) -> Option<(FaultKind, String, String, u64)> {
    let (kind, spec) = if let Some(spec) = fault.get("drop_response").and_then(Value::as_str) {
        (FaultKind::DropResponse, spec)
    } else {
        let spec = fault.get("delay").and_then(Value::as_str)?;
        let seconds = fault.get("seconds").map_or(Some(10), Value::as_u64)?;
        (FaultKind::Delay(seconds.clamp(1, 300)), spec)
    };
    let (method, path) = spec.trim().split_once(char::is_whitespace)?;
    let path = path.trim();
    if method.is_empty() || !path.starts_with('/') {
        return None;
    }
    let times = fault.get("times").map_or(Some(1), Value::as_u64)?;
    Some((kind, method.to_ascii_uppercase(), path.to_owned(), times))
}

/// The fault this call meets, if any: the first seeded one matching the
/// method and path with uses left is spent and its kind answered.
fn spend_fault(state: &mut Value, method: &str, path: &str) -> Option<FaultKind> {
    let faults = state.get_mut("faults").and_then(Value::as_array_mut)?;
    for fault in faults.iter_mut() {
        let Some((kind, m, p, times)) = parse_fault(fault) else {
            continue;
        };
        if times > 0 && m == method && path.starts_with(&p) {
            fault["times"] = json!(times - 1);
            return Some(kind);
        }
    }
    None
}

/// Uses a world's faults have left, all kinds together.
fn faults_left(state: &Value) -> u64 {
    state
        .get("faults")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_fault)
        .map(|(_, _, _, n)| n)
        .sum()
}

fn row_count(state: &Value) -> BTreeMap<String, usize> {
    state
        .get("tables")
        .and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .map(|(k, v)| (k.clone(), v.as_array().map_or(0, Vec::len)))
                .collect()
        })
        .unwrap_or_default()
}

fn served(record: &WorldRecord, with_rows: bool) -> Value {
    let mut value = json!({
        "world_id": record.world_id,
        "name": record.name,
        "connector": record.connector,
        "instance_id": record.instance_id,
        "stands_for": record.stands_for,
        "dialect": record.dialect,
        "created_by": record.created_by,
        "created_at": record.created_at,
        "reset_count": record.reset_count,
        "last_reset_at": record.last_reset_at,
        "calls_since_reset": record.calls_since_reset,
        "records": row_count(&record.state),
        "seed_records": row_count(&record.seed),
        "faults_left": faults_left(&record.state),
        "seed_faults": faults_left(&record.seed),
    });
    if with_rows {
        let tables: Map<String, Value> = record
            .state
            .get("tables")
            .and_then(Value::as_object)
            .map(|t| {
                t.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            json!(v
                                .as_array()
                                .map(|rows| rows
                                    .iter()
                                    .rev()
                                    .take(SHOWN_ROWS)
                                    .cloned()
                                    .collect::<Vec<_>>())
                                .unwrap_or_default()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        value["tables"] = Value::Object(tables);
    }
    value
}

fn validate_seed(seed: &Value) -> Result<(), ApiError> {
    let Some(tables) = seed.get("tables").and_then(Value::as_object) else {
        return Err(ApiError::bad_request(
            "a seed is `{\"tables\": {<table>: [<record>, …]}}`".to_owned(),
        ));
    };
    for (table, rows) in tables {
        let Some(rows) = rows.as_array() else {
            return Err(ApiError::bad_request(format!(
                "seed table `{table}` must be a list of records"
            )));
        };
        if rows.len() > MAX_ROWS_PER_TABLE {
            return Err(ApiError::bad_request(format!(
                "seed table `{table}` holds {} records; a world keeps at most {MAX_ROWS_PER_TABLE} per table",
                rows.len()
            )));
        }
        if rows.iter().any(|r| !r.is_object()) {
            return Err(ApiError::bad_request(format!(
                "seed table `{table}` holds something that is not a record"
            )));
        }
    }
    if let Some(faults) = seed.get("faults") {
        let Some(faults) = faults.as_array() else {
            return Err(ApiError::bad_request("`faults` is a list: [{\"drop_response\": \"POST /api/now/table/incident\", \"times\": 1}, {\"delay\": \"POST /api/now/table/problem\", \"seconds\": 30}]".to_owned()));
        };
        for fault in faults {
            if parse_fault(fault).is_none() {
                return Err(ApiError::bad_request(format!(
                    "fault {fault} is neither `{{\"drop_response\": \"<METHOD> </path>\", \"times\": <n>}}` nor `{{\"delay\": \"<METHOD> </path>\", \"seconds\": <s>, \"times\": <n>}}`"
                )));
            }
        }
    }
    Ok(())
}

// ── Routes ──────────────────────────────────────────────────────────────────

/// Every dialect as the struct serializes — a field added to `Dialect` is
/// served without anyone remembering to list it — plus its starter seed.
pub(crate) async fn list_dialects() -> Json<Value> {
    let dialects: Vec<Value> = DIALECTS
        .iter()
        .map(|d| {
            let mut value = serde_json::to_value(d).unwrap_or_else(|_| json!({"id": d.id}));
            value["starter"] = d.starter();
            value
        })
        .collect();
    Json(json!({ "dialects": dialects }))
}

/// A world a standing thing (a schedule, a webhook, a task, a delegation)
/// was put in, resolved: its id and its name.
#[derive(Debug, Clone)]
pub(crate) struct WorldRef {
    pub world_id: String,
    pub name: String,
}

/// The worlds named for a standing thing — `world` (one) and `worlds`
/// (several, one per system) — each by name or id, resolved to ids in
/// order and without repeats; refused (422) when the tenant holds no such
/// world, with `what` naming the thing that was not made.
pub(crate) async fn resolve_worlds(
    state: &AppState,
    tenant: &str,
    one: Option<&str>,
    more: &[String],
    what: &str,
) -> Result<Vec<WorldRef>, ApiError> {
    let mut named: Vec<String> = Vec::new();
    for w in one
        .into_iter()
        .map(str::to_owned)
        .chain(more.iter().cloned())
    {
        let w = w.trim().to_owned();
        if !w.is_empty() && !named.contains(&w) {
            named.push(w);
        }
    }
    let mut out: Vec<WorldRef> = Vec::new();
    for w in named {
        match state.worlds.find(tenant, &w).await {
            Ok(Some(world)) => {
                if !out.iter().any(|r| r.world_id == world.world_id) {
                    out.push(WorldRef {
                        world_id: world.world_id,
                        name: world.name,
                    });
                }
            }
            Ok(None) => {
                return Err(ApiError::unprocessable(format!(
                    "unknown world `{w}`: {what} was not made; make the world under Evals → Worlds or leave the world empty"
                )));
            }
            Err(error) => return Err(ApiError::internal(error)),
        }
    }
    Ok(out)
}

/// The run config a standing thing's worlds give a fired run: the first
/// as `world`, all as `worlds` when there are several.
pub(crate) fn run_config_for(
    world: Option<&String>,
    worlds: &[String],
) -> Option<crate::runs::RunConfigPayload> {
    let first = world.cloned().or_else(|| worlds.first().cloned())?;
    let all: Vec<String> = if worlds.len() > 1 {
        worlds.to_vec()
    } else {
        Vec::new()
    };
    Some(crate::runs::RunConfigPayload {
        world: Some(first),
        worlds: if all.len() > 1 { Some(all) } else { None },
        ..crate::runs::RunConfigPayload::default()
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct StarterQuery {
    #[serde(default)]
    connector: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
}

/// `GET /worlds/starter?connector=<id|hash>` or `?instance_id=<id>` — the
/// seed a new world of that system would start from: the dialect's own
/// starter when one is written for the connector, else one made from the
/// connector's operations.
pub(crate) async fn world_starter(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Query(query): Query<StarterQuery>,
) -> Result<Json<Value>, ApiError> {
    let manifest = match (&query.instance_id, &query.connector) {
        (Some(instance_id), _) => {
            let instance = state
                .connectors
                .get_instance(tenant.tenant(), instance_id)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .ok_or_else(|| {
                    ApiError::not_found(format!("unknown connection `{instance_id}`"))
                })?;
            state
                .connectors
                .get_manifest(tenant.tenant(), &instance.manifest_hash)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .ok_or_else(|| {
                    ApiError::not_found("the connection's connector is gone".to_owned())
                })?
        }
        (None, Some(connector)) => state
            .connectors
            .list_manifests(tenant.tenant())
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .into_iter()
            .filter(|m| m.id == *connector || m.hash == *connector)
            .max_by(|a, b| a.version.cmp(&b.version))
            .ok_or_else(|| {
                ApiError::not_found(format!("no connector `{connector}` in the library"))
            })?,
        (None, None) => return Err(ApiError::bad_request(
            "say which system: `connector` (a library id or hash) or `instance_id` (a connection)"
                .to_owned(),
        )),
    };
    let dialect = Dialect::for_connector(&manifest.id)
        .unwrap_or_else(|| Dialect::by_id("manifest-rest").expect("the generic dialect exists"));
    let starter = if dialect.id == "manifest-rest" {
        generic::starter(&manifest)
    } else {
        dialect.starter()
    };
    Ok(Json(
        json!({ "connector": manifest.id, "dialect": dialect.id, "starter": starter }),
    ))
}

pub(crate) async fn list_worlds(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let worlds = state
        .worlds
        .list(tenant.tenant())
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(
        json!({"worlds": worlds.iter().map(|w| served(w, false)).collect::<Vec<_>>()}),
    ))
}

pub(crate) async fn get_world(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let world = state
        .worlds
        .find(tenant.tenant(), &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("unknown world `{id}`")))?;
    Ok(Json(served(&world, true)))
}

#[derive(Debug, Deserialize)]
pub struct CreateWorld {
    pub name: String,
    /// The connection it stands in for: its connector and host are read
    /// from it.
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Without a connection: the connector id and the host it would call.
    #[serde(default)]
    pub connector: Option<String>,
    #[serde(default)]
    pub stands_for: Option<String>,
    #[serde(default)]
    pub dialect: Option<String>,
    /// `{"tables": {<table>: [<record>, …]}}`; the dialect's starter when absent.
    #[serde(default)]
    pub seed: Option<Value>,
}

pub(crate) async fn create_world(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(input): Json<CreateWorld>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let name = input.name.trim().to_owned();
    if name.is_empty()
        || name.len() > 60
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ApiError::bad_request("a world's name is 1–60 characters of letters, digits, `-`, `_` or `.` — it is what a case's `world:` tag names".to_owned()));
    }
    let existing = state
        .worlds
        .list(tenant.tenant())
        .await
        .map_err(ApiError::internal)?;
    if existing.iter().any(|w| w.name == name) {
        return Err(ApiError::conflict(format!("a world named `{name}` exists")));
    }
    // What it stands in for: the connection's connector and host, or the
    // caller's own words.
    let (connector, stands_for, instance_id, operations) = match &input.instance_id {
        Some(instance_id) => {
            let instance = state
                .connectors
                .get_instance(tenant.tenant(), instance_id)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .ok_or_else(|| {
                    ApiError::not_found(format!("unknown connection `{instance_id}`"))
                })?;
            let manifest = state
                .connectors
                .get_manifest(tenant.tenant(), &instance.manifest_hash)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .ok_or_else(|| {
                    ApiError::not_found("the connection's connector is gone".to_owned())
                })?;
            let config = crate::connectors::opened_config(&state, &tenant, &instance).await?;
            let host = rusty_agent_runtime::connector::render_template(&manifest.base_url, &config)
                .ok()
                .and_then(|url| crate::connectors::host_of(&url))
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "the connection's base URL names no host to stand in for".to_owned(),
                    )
                })?;
            (
                manifest.id.clone(),
                host,
                Some(instance.instance_id.clone()),
                generic::shapes(&manifest),
            )
        }
        // Without a connection: a connector in the library (by id or hash)
        // gives the operations and, when its base URL names one host, the
        // host — so a system that is not there yet has a stand-in before
        // anyone can connect to it, and the connect step proves against it.
        // A connector the library does not hold is a name and a host only.
        None => {
            let connector = input.connector.as_deref().map(str::trim).filter(|c| !c.is_empty()).ok_or_else(|| ApiError::bad_request("name the connection the world stands in for (`instance_id`), or the connector and host".to_owned()))?;
            let in_library = state
                .connectors
                .list_manifests(tenant.tenant())
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .into_iter()
                .filter(|m| m.id == connector || m.hash == connector)
                .max_by(|a, b| a.version.cmp(&b.version));
            let literal_host = in_library
                .as_ref()
                .filter(|m| !m.base_url.contains('{'))
                .and_then(|m| crate::connectors::host_of(&m.base_url));
            let host = match input.stands_for.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
                Some(host) => host.to_owned(),
                None => literal_host.ok_or_else(|| ApiError::bad_request("`stands_for` names the host whose calls the world answers (the connector's base URL names none, or names it per connection)".to_owned()))?,
            };
            match in_library {
                Some(manifest) => (
                    manifest.id.clone(),
                    host.to_ascii_lowercase(),
                    None,
                    generic::shapes(&manifest),
                ),
                None => (
                    connector.to_owned(),
                    host.to_ascii_lowercase(),
                    None,
                    Vec::new(),
                ),
            }
        }
    };
    let dialect = match input
        .dialect
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(id) => Dialect::by_id(id).ok_or_else(|| {
            ApiError::bad_request(format!(
                "no dialect `{id}`; the worlds speak {}",
                DIALECTS.iter().map(|d| d.id).collect::<Vec<_>>().join(", ")
            ))
        })?,
        None => Dialect::for_connector(&connector).ok_or_else(|| {
            ApiError::bad_request(format!(
                "no dialect fits connector `{connector}` — name one; the worlds speak {}",
                DIALECTS.iter().map(|d| d.id).collect::<Vec<_>>().join(", ")
            ))
        })?,
    };
    let mut seed = match input.seed {
        Some(seed) if !seed.is_null() => {
            validate_seed(&seed)?;
            seed
        }
        _ => dialect.starter(),
    };
    // A world by the manifest needs the operations, and a table for each
    // resource they address — empty when the seed has none.
    if dialect.id == "manifest-rest" {
        if operations.is_empty() {
            return Err(ApiError::bad_request("a world in the connector's own dialect is made from a connection, or from a connector in the library, so it knows the operations to answer".to_owned()));
        }
        generic::ensure_tables(&mut seed, &operations);
    }
    let now = Utc::now();
    let record = WorldRecord {
        world_id: uuid::Uuid::new_v4().to_string(),
        tenant: tenant.tenant().to_owned(),
        name,
        connector,
        instance_id,
        stands_for,
        dialect: dialect.id.to_owned(),
        operations,
        state: seed.clone(),
        seed,
        created_by: tenant.attribution(),
        created_at: now,
        reset_count: 0,
        last_reset_at: None,
        calls_since_reset: 0,
    };
    state
        .worlds
        .keep(&record)
        .await
        .map_err(ApiError::internal)?;
    tracing::info!(world = %record.name, connector = %record.connector, stands_for = %record.stands_for, "world created");
    Ok((StatusCode::CREATED, Json(served(&record, true))))
}

pub(crate) async fn reset_world(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let world = state
        .worlds
        .find(tenant.tenant(), &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("unknown world `{id}`")))?;
    let after = state
        .worlds
        .reset(tenant.tenant(), &world.world_id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(served(&after, true)))
}

pub(crate) async fn delete_world(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let world = state
        .worlds
        .find(tenant.tenant(), &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("unknown world `{id}`")))?;
    state
        .worlds
        .store
        .kv_delete(&namespace(tenant.tenant()), &world.world_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(
        json!({"deleted": true, "world_id": world.world_id, "name": world.name}),
    ))
}

// ── The ServiceNow Table API, as a world speaks it ─────────────────────────

pub mod servicenow {
    use super::*;

    fn now_text() -> String {
        Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    fn param<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn tables(state: &mut Value) -> &mut Map<String, Value> {
        if !state.is_object() {
            *state = json!({});
        }
        let object = state.as_object_mut().expect("object");
        if !object.get("tables").is_some_and(Value::is_object) {
            object.insert("tables".to_owned(), json!({}));
        }
        object
            .get_mut("tables")
            .and_then(Value::as_object_mut)
            .expect("tables")
    }

    fn rows<'a>(state: &'a mut Value, table: &str) -> &'a mut Vec<Value> {
        let tables = tables(state);
        if !tables.get(table).is_some_and(Value::is_array) {
            tables.insert(table.to_owned(), json!([]));
        }
        tables
            .get_mut(table)
            .and_then(Value::as_array_mut)
            .expect("rows")
    }

    fn next_number(state: &mut Value, table: &str) -> Option<String> {
        let prefix = match table {
            "incident" => "INC",
            "problem" => "PRB",
            "change_request" => "CHG",
            "sc_request" => "REQ",
            "sc_req_item" => "RITM",
            "sc_task" => "SCTASK",
            "kb_knowledge" => "KB",
            _ => return None,
        };
        let object = state.as_object_mut()?;
        if !object.get("counters").is_some_and(Value::is_object) {
            object.insert("counters".to_owned(), json!({}));
        }
        let counters = object.get_mut("counters").and_then(Value::as_object_mut)?;
        let next = counters.get(table).and_then(Value::as_u64).unwrap_or(10001);
        counters.insert(table.to_owned(), json!(next + 1));
        Some(format!("{prefix}{next:07}"))
    }

    fn field_text(row: &Value, field: &str, all: &Map<String, Value>) -> String {
        let direct = row.get(field);
        if let Some(value) = direct {
            return text_of(value);
        }
        // `cmdb_ci.name`: the referenced record's field, by sys_id or by
        // the reference's own text.
        if let Some((head, rest)) = field.split_once('.') {
            let reference = row.get(head).cloned().unwrap_or(Value::Null);
            if let Some(nested) = reference.get(rest) {
                return text_of(nested);
            }
            let key = text_of(&reference);
            if let Some(target) = all.get(head).and_then(Value::as_array) {
                if let Some(found) = target.iter().find(|r| {
                    text_of(r.get("sys_id").unwrap_or(&Value::Null)) == key
                        || text_of(r.get("name").unwrap_or(&Value::Null)) == key
                }) {
                    return field_text(found, rest, all);
                }
            }
            return if rest == "name" || rest == "display_value" {
                key
            } else {
                String::new()
            };
        }
        String::new()
    }

    fn text_of(value: &Value) -> String {
        match value {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("value")
                .or_else(|| o.get("display_value"))
                .map(text_of)
                .unwrap_or_default(),
            other => other.to_string(),
        }
    }

    /// One condition of an encoded query.
    #[derive(Debug)]
    struct Condition {
        field: String,
        op: String,
        value: String,
    }

    fn parse_condition(text: &str) -> Option<Condition> {
        // Longest operators first, so `!=` is not read as `=`; the word
        // operators as ServiceNow encodes them (no spaces around them).
        const OPS: &[&str] = &[
            "ISNOTEMPTY",
            "ISEMPTY",
            "NOT LIKE",
            "NOTLIKE",
            "STARTSWITH",
            "ENDSWITH",
            "RELATIVEGT",
            "RELATIVELT",
            "RELATIVEGE",
            "RELATIVELE",
            "NOT IN",
            "DYNAMIC",
            "ANYTHING",
            "LIKE",
            "IN",
            "!=",
            ">=",
            "<=",
            "=",
            ">",
            "<",
        ];
        for op in OPS {
            if let Some(at) = text.find(op) {
                if at == 0 {
                    continue;
                }
                let field = text[..at].trim().to_owned();
                let value = text[at + op.len()..].to_owned();
                if field.is_empty()
                    || !field
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
                {
                    continue;
                }
                return Some(Condition {
                    field,
                    op: (*op).to_owned(),
                    value,
                });
            }
        }
        None
    }

    fn relative_bound(value: &str) -> Option<String> {
        // `@hour@ago@24`, `@dayofweek@ago@7`, `@minute@ahead@30`
        let mut parts = value.trim_start_matches('@').split('@');
        let unit = parts.next()?;
        let direction = parts.next()?;
        let count: i64 = parts.next()?.parse().ok()?;
        let span = match unit {
            "minute" => Duration::minutes(count),
            "hour" => Duration::hours(count),
            "dayofweek" | "day" => Duration::days(count),
            "week" => Duration::weeks(count),
            "month" => Duration::days(30 * count),
            "year" => Duration::days(365 * count),
            _ => return None,
        };
        let bound = if direction == "ago" {
            Utc::now() - span
        } else {
            Utc::now() + span
        };
        Some(bound.format("%Y-%m-%d %H:%M:%S").to_string())
    }

    fn compare(actual: &str, expected: &str) -> std::cmp::Ordering {
        match (actual.trim().parse::<f64>(), expected.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            _ => actual.cmp(expected),
        }
    }

    fn holds(condition: &Condition, row: &Value, all: &Map<String, Value>) -> bool {
        let actual = field_text(row, &condition.field, all);
        let wanted = condition.value.as_str();
        let lower = actual.to_ascii_lowercase();
        match condition.op.as_str() {
            "=" => {
                if wanted.starts_with("javascript:") {
                    return false;
                }
                actual == wanted || lower == wanted.to_ascii_lowercase()
            }
            "!=" => actual != wanted && lower != wanted.to_ascii_lowercase(),
            "LIKE" => lower.contains(&wanted.to_ascii_lowercase()),
            "NOT LIKE" | "NOTLIKE" => !lower.contains(&wanted.to_ascii_lowercase()),
            "STARTSWITH" => lower.starts_with(&wanted.to_ascii_lowercase()),
            "ENDSWITH" => lower.ends_with(&wanted.to_ascii_lowercase()),
            "ISEMPTY" => actual.is_empty(),
            "ISNOTEMPTY" => !actual.is_empty(),
            "IN" => wanted
                .split(',')
                .any(|w| w.trim().eq_ignore_ascii_case(&actual)),
            "NOT IN" => !wanted
                .split(',')
                .any(|w| w.trim().eq_ignore_ascii_case(&actual)),
            ">" => compare(&actual, wanted) == std::cmp::Ordering::Greater,
            "<" => compare(&actual, wanted) == std::cmp::Ordering::Less,
            ">=" => compare(&actual, wanted) != std::cmp::Ordering::Less,
            "<=" => compare(&actual, wanted) != std::cmp::Ordering::Greater,
            "RELATIVEGT" | "RELATIVEGE" => relative_bound(wanted)
                .is_some_and(|bound| !actual.is_empty() && actual.as_str() >= bound.as_str()),
            "RELATIVELT" | "RELATIVELE" => relative_bound(wanted)
                .is_some_and(|bound| !actual.is_empty() && actual.as_str() <= bound.as_str()),
            "ANYTHING" => true,
            _ => false,
        }
    }

    /// The order an encoded query asks for, if any.
    struct Order {
        field: String,
        descending: bool,
    }

    /// `a^b^ORc^ORDERBYDESCd` — groups joined by AND, alternatives inside a
    /// group joined by OR (ServiceNow's `^OR` binds to the condition before
    /// it), the order clauses aside. `^NQ` starts a new query whose rows are
    /// added to the first's.
    fn parse_query(text: &str) -> (Vec<Vec<Vec<Condition>>>, Vec<Order>) {
        let mut queries: Vec<Vec<Vec<Condition>>> = Vec::new();
        let mut orders = Vec::new();
        for query in text.split("^NQ") {
            let mut groups: Vec<Vec<Condition>> = Vec::new();
            for token in query.split('^') {
                let token = token.trim();
                if token.is_empty() || token == "EQ" {
                    continue;
                }
                if let Some(field) = token.strip_prefix("ORDERBYDESC") {
                    orders.push(Order {
                        field: field.to_owned(),
                        descending: true,
                    });
                    continue;
                }
                if let Some(field) = token.strip_prefix("ORDERBY") {
                    orders.push(Order {
                        field: field.to_owned(),
                        descending: false,
                    });
                    continue;
                }
                if token.starts_with("GROUPBY") {
                    continue;
                }
                let (alternative, body) = match token.strip_prefix("OR") {
                    Some(rest) if !rest.starts_with("DERBY") => (true, rest),
                    _ => (false, token),
                };
                let Some(condition) = parse_condition(body) else {
                    // Something the world does not read: nothing matches it,
                    // rather than everything.
                    groups.push(vec![Condition {
                        field: "sys_id".to_owned(),
                        op: "NEVER".to_owned(),
                        value: String::new(),
                    }]);
                    continue;
                };
                if alternative {
                    if let Some(last) = groups.last_mut() {
                        last.push(condition);
                        continue;
                    }
                }
                groups.push(vec![condition]);
            }
            queries.push(groups);
        }
        (queries, orders)
    }

    fn matches(queries: &[Vec<Vec<Condition>>], row: &Value, all: &Map<String, Value>) -> bool {
        queries.iter().any(|groups| {
            groups
                .iter()
                .all(|group| group.iter().any(|c| holds(c, row, all)))
        })
    }

    fn project(row: &Value, fields: Option<&str>, all: &Map<String, Value>) -> Value {
        let Some(fields) = fields.map(str::trim).filter(|f| !f.is_empty()) else {
            return row.clone();
        };
        let mut out = Map::new();
        for field in fields.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            out.insert(field.to_owned(), Value::String(field_text(row, field, all)));
        }
        Value::Object(out)
    }

    fn not_found() -> (u16, Value) {
        (
            404,
            json!({"error": {"message": "No Record found", "detail": "Record doesn't exist or ACL restricts access"}, "status": "failure"}),
        )
    }

    /// Answer one request against `state`.
    pub fn serve(
        state: &mut Value,
        method: HttpMethod,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> (u16, Value) {
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        match (method, segments.as_slice()) {
            (HttpMethod::Get, ["api", "now", "table", table]) => list(state, table, query),
            (HttpMethod::Get, ["api", "now", "table", table, sys_id]) => {
                get(state, table, sys_id, query)
            }
            (HttpMethod::Post, ["api", "now", "table", table]) => create(state, table, body),
            (HttpMethod::Patch | HttpMethod::Put, ["api", "now", "table", table, sys_id]) => {
                update(state, table, sys_id, body)
            }
            (HttpMethod::Delete, ["api", "now", "table", table, sys_id]) => {
                delete(state, table, sys_id)
            }
            (HttpMethod::Get, ["api", "now", "stats", table]) => stats(state, table, query),
            (HttpMethod::Get, ["api", "sn_sc", "servicecatalog", "items"]) => {
                catalog_items(state, query)
            }
            (
                HttpMethod::Post,
                ["api", "sn_sc", "servicecatalog", "items", sys_id, "order_now"],
            ) => order_now(state, sys_id, body),
            _ => (
                404,
                json!({"error": {"message": "Requested URI does not represent any resource", "detail": path}, "status": "failure"}),
            ),
        }
    }

    fn list(state: &mut Value, table: &str, query: &[(String, String)]) -> (u16, Value) {
        let all = tables(state).clone();
        let rows = all
            .get(table)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (queries, orders) = parse_query(param(query, "sysparm_query").unwrap_or(""));
        let mut found: Vec<Value> = rows
            .into_iter()
            .filter(|r| queries.is_empty() || matches(&queries, r, &all))
            .collect();
        for order in orders.iter().rev() {
            found.sort_by(|a, b| {
                let ord = compare(
                    &field_text(a, &order.field, &all),
                    &field_text(b, &order.field, &all),
                );
                if order.descending {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        let offset = param(query, "sysparm_offset")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = param(query, "sysparm_limit")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(10_000);
        let fields = param(query, "sysparm_fields");
        let page: Vec<Value> = found
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|r| project(&r, fields, &all))
            .collect();
        (200, json!({"result": page}))
    }

    fn get(
        state: &mut Value,
        table: &str,
        sys_id: &str,
        query: &[(String, String)],
    ) -> (u16, Value) {
        let all = tables(state).clone();
        let Some(row) = all.get(table).and_then(Value::as_array).and_then(|rows| {
            rows.iter()
                .find(|r| text_of(r.get("sys_id").unwrap_or(&Value::Null)) == sys_id)
        }) else {
            return not_found();
        };
        (
            200,
            json!({"result": project(row, param(query, "sysparm_fields"), &all)}),
        )
    }

    fn create(state: &mut Value, table: &str, body: Option<&Value>) -> (u16, Value) {
        let Some(Value::Object(fields)) = body else {
            return (
                400,
                json!({"error": {"message": "Invalid request body", "detail": "a JSON object of fields"}, "status": "failure"}),
            );
        };
        if rows(state, table).len() >= MAX_ROWS_PER_TABLE {
            return (
                507,
                json!({"error": {"message": format!("the world keeps at most {MAX_ROWS_PER_TABLE} records per table")}, "status": "failure"}),
            );
        }
        let mut record = fields.clone();
        let now = now_text();
        let sys_id = uuid::Uuid::new_v4().simple().to_string();
        record.insert("sys_id".to_owned(), json!(sys_id));
        if let Some(number) = next_number(state, table) {
            record.entry("number".to_owned()).or_insert(json!(number));
        }
        for (key, value) in [
            ("sys_created_on", now.clone()),
            ("sys_updated_on", now.clone()),
            ("opened_at", now.clone()),
        ] {
            if table == "incident" || table == "problem" || key != "opened_at" {
                record.entry(key.to_owned()).or_insert(json!(value));
            }
        }
        if table == "incident" || table == "problem" || table == "sc_request" {
            record.entry("state".to_owned()).or_insert(json!("1"));
            record.entry("active".to_owned()).or_insert(json!("true"));
        }
        let created = Value::Object(record);
        rows(state, table).push(created.clone());
        (201, json!({"result": created}))
    }

    fn update(state: &mut Value, table: &str, sys_id: &str, body: Option<&Value>) -> (u16, Value) {
        let Some(Value::Object(fields)) = body else {
            return (
                400,
                json!({"error": {"message": "Invalid request body", "detail": "a JSON object of fields"}, "status": "failure"}),
            );
        };
        let now = now_text();
        let rows = rows(state, table);
        let Some(row) = rows
            .iter_mut()
            .find(|r| text_of(r.get("sys_id").unwrap_or(&Value::Null)) == sys_id)
        else {
            return not_found();
        };
        if let Some(object) = row.as_object_mut() {
            for (k, v) in fields {
                object.insert(k.clone(), v.clone());
            }
            object.insert("sys_updated_on".to_owned(), json!(now));
            if matches!(
                object.get("state").map(text_of).as_deref(),
                Some("6") | Some("7")
            ) {
                object.insert("active".to_owned(), json!("false"));
            }
        }
        (200, json!({"result": row.clone()}))
    }

    fn delete(state: &mut Value, table: &str, sys_id: &str) -> (u16, Value) {
        let rows = rows(state, table);
        let before = rows.len();
        rows.retain(|r| text_of(r.get("sys_id").unwrap_or(&Value::Null)) != sys_id);
        if rows.len() == before {
            return not_found();
        }
        (204, Value::Null)
    }

    fn stats(state: &mut Value, table: &str, query: &[(String, String)]) -> (u16, Value) {
        let all = tables(state).clone();
        let rows = all
            .get(table)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (queries, _) = parse_query(param(query, "sysparm_query").unwrap_or(""));
        let found: Vec<Value> = rows
            .into_iter()
            .filter(|r| queries.is_empty() || matches(&queries, r, &all))
            .collect();
        match param(query, "sysparm_group_by")
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            Some(group_by) => {
                let mut counts: BTreeMap<String, usize> = BTreeMap::new();
                for row in &found {
                    *counts.entry(field_text(row, group_by, &all)).or_default() += 1;
                }
                let result: Vec<Value> = counts
                    .into_iter()
                    .map(|(value, count)| json!({"groupby_fields": [{"field": group_by, "value": value}], "stats": {"count": count.to_string()}}))
                    .collect();
                (200, json!({"result": result}))
            }
            None => (
                200,
                json!({"result": {"stats": {"count": found.len().to_string()}}}),
            ),
        }
    }

    fn catalog_items(state: &mut Value, query: &[(String, String)]) -> (u16, Value) {
        let all = tables(state).clone();
        let items = all
            .get("sc_cat_item")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = param(query, "sysparm_text")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let limit = param(query, "sysparm_limit")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20);
        let words: Vec<&str> = text.split_whitespace().collect();
        let found: Vec<Value> = items
            .into_iter()
            .filter(|item| {
                if words.is_empty() {
                    return true;
                }
                let hay = format!(
                    "{} {}",
                    field_text(item, "name", &all),
                    field_text(item, "short_description", &all)
                )
                .to_ascii_lowercase();
                words.iter().any(|w| hay.contains(w))
            })
            .take(limit)
            .collect();
        (200, json!({"result": found}))
    }

    fn order_now(state: &mut Value, sys_id: &str, body: Option<&Value>) -> (u16, Value) {
        let all = tables(state).clone();
        let Some(item) = all
            .get("sc_cat_item")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|i| text_of(i.get("sys_id").unwrap_or(&Value::Null)) == sys_id)
            })
            .cloned()
        else {
            return not_found();
        };
        let quantity = body
            .and_then(|b| b.get("sysparm_quantity"))
            .map(text_of)
            .unwrap_or_else(|| "1".to_owned());
        let number = next_number(state, "sc_request").unwrap_or_else(|| "REQ0010001".to_owned());
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let now = now_text();
        rows(state, "sc_request").push(json!({
            "sys_id": request_id,
            "number": number,
            "cat_item": item.get("sys_id").cloned().unwrap_or(Value::Null),
            "short_description": item.get("name").cloned().unwrap_or(Value::Null),
            "quantity": quantity,
            "state": "1",
            "active": "true",
            "opened_at": now,
            "sys_created_on": now,
            "sys_updated_on": now,
        }));
        (
            200,
            json!({"result": {"sys_id": request_id, "number": number, "request_number": number, "request_id": request_id, "table": "sc_request"}}),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn world() -> Value {
            Dialect::by_id("servicenow-table").unwrap().starter()
        }

        fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        }

        #[test]
        fn an_encoded_query_filters_orders_and_projects() {
            let mut state = world();
            let (status, answer) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[
                    (
                        "sysparm_query",
                        "active=true^short_descriptionLIKEvpn^ORshort_descriptionLIKEbadge^ORDERBYDESCopened_at",
                    ),
                    ("sysparm_fields", "number,state"),
                    ("sysparm_limit", "10"),
                ]),
                None,
            );
            assert_eq!(status, 200);
            let rows = answer["result"].as_array().unwrap();
            assert_eq!(
                rows.iter()
                    .map(|r| r["number"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["INC0010002", "INC0010001"]
            );
            assert_eq!(
                rows[0].as_object().unwrap().len(),
                2,
                "only the named fields: {rows:?}"
            );
        }

        #[test]
        fn a_created_incident_is_numbered_and_found_again_and_a_reset_forgets_it() {
            let mut state = world();
            let (status, answer) = serve(
                &mut state,
                HttpMethod::Post,
                "/api/now/table/incident",
                &[],
                Some(
                    &json!({"short_description": "Fog machine in the unicorn stables", "caller_id": "Abel Tuter"}),
                ),
            );
            assert_eq!(status, 201, "{answer}");
            assert_eq!(answer["result"]["number"], "INC0010004");
            assert_eq!(answer["result"]["state"], "1");
            let sys_id = answer["result"]["sys_id"].as_str().unwrap().to_owned();
            let (status, again) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[
                    ("sysparm_query", "short_descriptionLIKEfog machine"),
                    ("sysparm_limit", "3"),
                ]),
                None,
            );
            assert_eq!(status, 200);
            assert_eq!(again["result"][0]["sys_id"], sys_id);
            let (status, one) = serve(
                &mut state,
                HttpMethod::Get,
                &format!("/api/now/table/incident/{sys_id}"),
                &[],
                None,
            );
            assert_eq!(status, 200);
            assert_eq!(one["result"]["number"], "INC0010004");
            let (status, _) = serve(
                &mut state,
                HttpMethod::Patch,
                &format!("/api/now/table/incident/{sys_id}"),
                &[],
                Some(&json!({"state": "6", "close_notes": "not real"})),
            );
            assert_eq!(status, 200);
            let (_, closed) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[
                    ("sysparm_query", "number=INC0010004"),
                    ("sysparm_fields", "active,state"),
                ]),
                None,
            );
            assert_eq!(closed["result"][0]["active"], "false");
            // The seed is what a reset restores.
            let mut fresh = world();
            let (_, none) = serve(
                &mut fresh,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[("sysparm_query", "short_descriptionLIKEfog machine")]),
                None,
            );
            assert_eq!(none["result"].as_array().unwrap().len(), 0);
            let (_, next) = serve(
                &mut fresh,
                HttpMethod::Post,
                "/api/now/table/incident",
                &[],
                Some(&json!({"short_description": "again"})),
            );
            assert_eq!(
                next["result"]["number"], "INC0010004",
                "the counter is part of the seed"
            );
        }

        #[test]
        fn stats_count_and_group_and_the_catalog_orders() {
            let mut state = world();
            let (_, count) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/stats/incident",
                &q(&[("sysparm_count", "true"), ("sysparm_query", "active=true")]),
                None,
            );
            assert_eq!(count["result"]["stats"]["count"], "2");
            let (_, grouped) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/stats/incident",
                &q(&[("sysparm_count", "true"), ("sysparm_group_by", "category")]),
                None,
            );
            let groups = grouped["result"].as_array().unwrap();
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0]["groupby_fields"][0]["value"], "hardware");
            assert_eq!(groups[0]["stats"]["count"], "2");
            let (_, items) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/sn_sc/servicecatalog/items",
                &q(&[("sysparm_text", "laptop")]),
                None,
            );
            let item = items["result"][0]["sys_id"].as_str().unwrap().to_owned();
            let (status, ordered) = serve(
                &mut state,
                HttpMethod::Post,
                &format!("/api/sn_sc/servicecatalog/items/{item}/order_now"),
                &[],
                Some(&json!({"sysparm_quantity": "1"})),
            );
            assert_eq!(status, 200);
            assert_eq!(ordered["result"]["request_number"], "REQ0010001");
            let (status, missing) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident/nope",
                &[],
                None,
            );
            assert_eq!(status, 404);
            assert_eq!(missing["error"]["message"], "No Record found");
        }

        #[test]
        fn what_the_world_cannot_read_matches_nothing() {
            let mut state = world();
            let (_, js) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[(
                    "sysparm_query",
                    "opened_at=javascript:gs.beginningOfToday()",
                )]),
                None,
            );
            assert_eq!(js["result"].as_array().unwrap().len(), 0);
            let (_, recent) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[("sysparm_query", "opened_atRELATIVEGT@year@ago@50")]),
                None,
            );
            assert_eq!(recent["result"].as_array().unwrap().len(), 3);
            let (_, referenced) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/now/table/incident",
                &q(&[
                    ("sysparm_query", "assignment_group.name=Network"),
                    ("sysparm_fields", "number"),
                ]),
                None,
            );
            assert_eq!(referenced["result"][0]["number"], "INC0010002");
        }
    }
}

// ── A ledger API: paged, and its schema has versions ────────────────────────

/// A records API of the ordinary kind: it pages, and between versions it
/// renames fields. An agent built against v1 and pointed at a v2 world
/// makes an invalid call, is told which fields exist, reads the schema,
/// repairs the call and pages to the end. That is the whole of family F2
/// in one system.
pub mod ledger {
    use super::*;

    /// The four fields that change name between versions, v1 → v2.
    const RENAMED: [(&str, &str); 4] = [
        ("date", "posted_on"),
        ("party", "counterparty"),
        ("amount", "amount_minor"),
        ("status", "state"),
    ];

    fn version(state: &Value) -> String {
        state
            .get("schema_version")
            .and_then(Value::as_str)
            .unwrap_or("v1")
            .to_owned()
    }

    fn page_size(state: &Value) -> usize {
        state
            .get("page_size")
            .and_then(Value::as_u64)
            .unwrap_or(25)
            .clamp(1, 100) as usize
    }

    /// The field names this version answers with.
    fn fields_of(version: &str) -> Vec<&'static str> {
        if version == "v2" {
            vec![
                "id",
                "posted_on",
                "counterparty",
                "amount_minor",
                "currency",
                "state",
            ]
        } else {
            vec!["id", "date", "party", "amount", "currency", "status"]
        }
    }

    /// One seeded row, as this version names it. v2 also carries the amount
    /// in minor units, because a rename that only renames teaches nothing.
    fn row_as(version: &str, row: &Value) -> Value {
        if version != "v2" {
            return row.clone();
        }
        let mut out = serde_json::Map::new();
        out.insert(
            "id".to_owned(),
            row.get("id").cloned().unwrap_or(Value::Null),
        );
        for (v1, v2) in RENAMED {
            let value = row.get(v1).cloned().unwrap_or(Value::Null);
            let value = if v1 == "amount" {
                // "123.45" becomes 12345 minor units.
                let minor = value
                    .as_str()
                    .and_then(|s| s.replace('.', "").parse::<i64>().ok())
                    .unwrap_or(0);
                json!(minor)
            } else {
                value
            };
            out.insert(v2.to_owned(), value);
        }
        out.insert(
            "currency".to_owned(),
            row.get("currency").cloned().unwrap_or(Value::Null),
        );
        Value::Object(out)
    }

    fn param<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// A refusal that names what exists, so the caller can repair rather
    /// than guess.
    fn unknown_field(field: &str, version: &str) -> (u16, Value) {
        (
            400,
            json!({"error": {
                "code": "unknown_field",
                "message": format!("`{field}` is not a field of schema {version}; this version has {}. Read /v1/schema and call again with the names it gives.", fields_of(version).join(", ")),
                "schema_version": version,
                "schema_url": "/v1/schema",
                "fields": fields_of(version),
            }}),
        )
    }

    pub fn serve(
        state: &mut Value,
        method: HttpMethod,
        path: &str,
        query: &[(String, String)],
    ) -> (u16, Value) {
        let version = version(state);
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        match (method, segments.as_slice()) {
            (HttpMethod::Get, ["v1", "schema"]) => (
                200,
                json!({
                    "schema_version": version,
                    "fields": fields_of(&version).iter().map(|f| json!({"name": f, "type": if *f == "amount_minor" { "integer (minor units)" } else { "string" }})).collect::<Vec<_>>(),
                    "note": "Field names change between schema versions. Call /v1/entries with the names this version gives.",
                }),
            ),
            (HttpMethod::Get, ["v1", "entries"]) => {
                // `schema_changes_after: n` means the call after the nth
                // meets the new schema, so the change lands before this call
                // is answered.
                advance_schema(state);
                let answered_under = state
                    .get("schema_version")
                    .and_then(Value::as_str)
                    .unwrap_or("v1")
                    .to_owned();
                entries(state, &answered_under, query)
            }
            _ => (
                404,
                json!({"error": {"code": "no_such_route", "message": format!("{path} is not a route of this API; it answers /v1/schema and /v1/entries.")}}),
            ),
        }
    }

    /// A ledger seeded with `schema_changes_after: n` moves to v2 once it
    /// has answered n entry calls: the schema changes *under* a run, which
    /// is the way an agent meets one it cannot read ahead of.
    fn advance_schema(state: &mut Value) {
        let Some(after) = state.get("schema_changes_after").and_then(Value::as_u64) else {
            return;
        };
        let calls = state
            .get("entry_calls")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        if let Some(object) = state.as_object_mut() {
            object.insert("entry_calls".to_owned(), json!(calls));
            if calls > after && object.get("schema_version").and_then(Value::as_str) == Some("v1") {
                object.insert("schema_version".to_owned(), json!("v2"));
                tracing::info!(after, "a ledger world moved to schema v2 under a run");
            }
        }
    }

    fn entries(state: &mut Value, version: &str, query: &[(String, String)]) -> (u16, Value) {
        let known = fields_of(version);
        // Every field the caller names, in `fields` or as a filter, must be
        // one this version has.
        if let Some(wanted) = param(query, "fields") {
            for field in wanted.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                if !known.contains(&field) {
                    return unknown_field(field, version);
                }
            }
        }
        for (key, _) in query
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "fields" | "cursor" | "limit"))
        {
            if !known.contains(&key.as_str()) {
                return unknown_field(key, version);
            }
        }
        let rows: Vec<Value> = state
            .get("tables")
            .and_then(|t| t.get("entries"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|row| row_as(version, row))
            .collect();
        // Filters: any known field, matched exactly on its text.
        let filtered: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                query
                    .iter()
                    .filter(|(k, _)| known.contains(&k.as_str()))
                    .all(|(k, v)| {
                        row.get(k).map(|found| match found {
                            Value::String(s) => s == v,
                            // Non-string scalars match by their JSON
                            // serialization ("true", "42"); serde_json offers
                            // no allocation-free way to spell that, so the
                            // owned comparison stays.
                            #[allow(clippy::cmp_owned)]
                            other => other.to_string() == *v,
                        }) == Some(true)
                    })
            })
            .collect();
        // `page_size` is this API's maximum, not merely its default: asking
        // for more does not make paging go away.
        let most = page_size(state);
        let limit = param(query, "limit")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(most)
            .clamp(1, most);
        let from = param(query, "cursor")
            .and_then(|c| c.strip_prefix("e"))
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        let page: Vec<Value> = filtered.iter().skip(from).take(limit).cloned().collect();
        let next = from + page.len();
        (
            200,
            json!({
                "schema_version": version,
                "entries": page,
                "total": filtered.len(),
                "next_cursor": if next < filtered.len() { Some(format!("e{next}")) } else { None },
            }),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn world(version: &str) -> Value {
            let mut state = Dialect::by_id("ledger-api").expect("the dialect").starter();
            state["schema_version"] = json!(version);
            state
        }

        fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        }

        #[test]
        fn the_schema_says_which_names_this_version_answers_to() {
            let (status, v1) = serve(&mut world("v1"), HttpMethod::Get, "/v1/schema", &[]);
            assert_eq!(status, 200);
            assert_eq!(v1["schema_version"], "v1");
            let names: Vec<String> = v1["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_owned())
                .collect();
            assert!(
                names.contains(&"party".to_owned()) && !names.contains(&"counterparty".to_owned()),
                "{names:?}"
            );
            let (_, v2) = serve(&mut world("v2"), HttpMethod::Get, "/v1/schema", &[]);
            let names: Vec<String> = v2["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_owned())
                .collect();
            assert!(
                names.contains(&"counterparty".to_owned()) && !names.contains(&"party".to_owned()),
                "{names:?}"
            );
        }

        #[test]
        fn a_call_in_the_old_names_is_refused_with_the_names_that_exist() {
            let mut v2 = world("v2");
            let (status, refused) = serve(
                &mut v2,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("fields", "id,party,amount")]),
            );
            assert_eq!(status, 400);
            assert_eq!(refused["error"]["code"], "unknown_field");
            let said = refused["error"]["message"].as_str().unwrap();
            assert!(
                said.contains("`party` is not a field of schema v2"),
                "{said}"
            );
            assert!(
                said.contains("counterparty") && said.contains("amount_minor"),
                "the refusal names what exists: {said}"
            );
            assert_eq!(refused["error"]["schema_url"], "/v1/schema");
            // A filter on a renamed field is refused the same way.
            let (status, _) = serve(
                &mut v2,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("status", "posted")]),
            );
            assert_eq!(status, 400);
            // And the repaired call goes through.
            let (status, ok) = serve(
                &mut v2,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[
                    ("fields", "id,counterparty,amount_minor"),
                    ("state", "posted"),
                ]),
            );
            assert_eq!(status, 200, "{ok}");
            assert!(ok["total"].as_u64().unwrap() > 0);
        }

        #[test]
        fn every_page_is_answered_until_the_cursor_runs_out() {
            let mut v1 = world("v1");
            let mut seen = 0usize;
            let mut cursor: Option<String> = None;
            let mut pages = 0;
            loop {
                let mut args = vec![("limit".to_owned(), "25".to_owned())];
                if let Some(c) = &cursor {
                    args.push(("cursor".to_owned(), c.clone()));
                }
                let (status, page) = serve(&mut v1, HttpMethod::Get, "/v1/entries", &args);
                assert_eq!(status, 200, "{page}");
                seen += page["entries"].as_array().unwrap().len();
                pages += 1;
                match page["next_cursor"].as_str() {
                    Some(next) => cursor = Some(next.to_owned()),
                    None => break,
                }
                assert!(pages < 10, "the cursor never ran out");
            }
            assert_eq!(pages, 3, "sixty entries at twenty-five a page");
            assert_eq!(seen, 60);
        }

        #[test]
        fn the_page_size_is_a_maximum_and_asking_for_more_does_not_avoid_paging() {
            let mut v1 = world("v1");
            let (_, page) = serve(
                &mut v1,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("limit", "1000")]),
            );
            assert_eq!(
                page["entries"].as_array().unwrap().len(),
                25,
                "twenty-five is this ledger's maximum page"
            );
            assert_eq!(page["next_cursor"], "e25");
        }

        #[test]
        fn a_ledger_can_change_its_schema_under_a_run() {
            let mut w = world("v1");
            w["schema_changes_after"] = json!(1);
            // The first call is answered under v1, in the old names.
            let (status, first) = serve(
                &mut w,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("fields", "id,party,status")]),
            );
            assert_eq!(status, 200, "{first}");
            assert_eq!(first["schema_version"], "v1");
            let cursor = first["next_cursor"]
                .as_str()
                .expect("more pages")
                .to_owned();
            // The next one meets v2, and the same names are now refused with
            // the names that exist.
            let (status, refused) = serve(
                &mut w,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("fields", "id,party,status"), ("cursor", &cursor)]),
            );
            assert_eq!(status, 400, "{refused}");
            assert!(
                refused["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("counterparty"),
                "{refused}"
            );
            // Repaired, the same cursor carries on where v1 left off.
            let (status, next) = serve(
                &mut w,
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("fields", "id,counterparty,state"), ("cursor", &cursor)]),
            );
            assert_eq!(status, 200, "{next}");
            assert_eq!(next["schema_version"], "v2");
            assert_eq!(next["entries"].as_array().unwrap().len(), 25);
            // And a reset puts the ledger back on v1 for the next case.
            let fresh = {
                let mut f = world("v1");
                f["schema_changes_after"] = json!(1);
                f
            };
            assert_eq!(fresh["schema_version"], "v1");
            assert!(fresh.get("entry_calls").is_none());
        }

        #[test]
        fn v2_restates_the_amount_in_minor_units_so_a_rename_is_not_the_whole_change() {
            let (_, v1) = serve(
                &mut world("v1"),
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("limit", "1")]),
            );
            let (_, v2) = serve(
                &mut world("v2"),
                HttpMethod::Get,
                "/v1/entries",
                &q(&[("limit", "1")]),
            );
            let major = v1["entries"][0]["amount"]
                .as_str()
                .expect("v1 amount is text");
            let minor = v2["entries"][0]["amount_minor"]
                .as_i64()
                .expect("v2 amount is an integer");
            assert_eq!(minor, major.replace('.', "").parse::<i64>().unwrap());
            assert!(
                v2["entries"][0].get("amount").is_none(),
                "the old name is gone in v2"
            );
        }
    }
}

/// A dialect that speaks any manifest: the connector's own operations,
/// answered from tables named after the resources their paths address.
pub mod generic {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use rusty_agent_runtime::connector::{ConnectorManifest, HttpMethod};
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Map, Value};

    /// One operation as the dialect needs it: name, method, path template.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct OpShape {
        pub name: String,
        pub method: String,
        pub path: String,
    }

    /// A starter seed from the connector's own operations: one example
    /// record per resource, its fields from the operation that creates
    /// one (a string field reads `Example <field>`, a number 1, a flag
    /// false; the path's parameters as fields to be found by), an `id`
    /// and a `number`, and a counter for the next one — so a world made
    /// from the library has something a case can expect on day one.
    pub fn starter(manifest: &ConnectorManifest) -> Value {
        let mut tables = serde_json::Map::new();
        let mut counters = serde_json::Map::new();
        for op in &manifest.operations {
            let path = op.path.split('?').next().unwrap_or(&op.path);
            if segments(path).contains(&"{table}") {
                continue;
            }
            let resource = resource_of(path, &BTreeMap::new());
            let row = tables
                .entry(resource.clone())
                .or_insert_with(|| json!([{"id": 1, "number": 1}]));
            let Some(record) = row
                .as_array_mut()
                .and_then(|rows| rows.first_mut())
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            for segment in segments(path).iter().filter(|s| is_param(s)) {
                let param = segment.trim_matches(|c| c == '{' || c == '}');
                if !matches!(param, "id" | "number") {
                    record
                        .entry(param.to_owned())
                        .or_insert_with(|| json!(format!("example-{param}")));
                }
            }
            if super::method_word(op.method) == "POST" {
                if let Some(props) = op
                    .params_schema
                    .get("properties")
                    .and_then(Value::as_object)
                {
                    for (field, schema) in props {
                        let example = match schema
                            .get("enum")
                            .and_then(Value::as_array)
                            .and_then(|e| e.first())
                        {
                            Some(first) => first.clone(),
                            None => match schema.get("type").and_then(Value::as_str) {
                                Some("integer") | Some("number") => json!(1),
                                Some("boolean") => json!(false),
                                Some("array") => json!([]),
                                Some("object") => json!({}),
                                _ => json!(format!("Example {}", field.replace('_', " "))),
                            },
                        };
                        record.entry(field.clone()).or_insert(example);
                    }
                }
            }
            counters.entry(resource).or_insert(json!(2));
        }
        json!({"tables": tables, "counters": counters})
    }

    pub fn shapes(manifest: &ConnectorManifest) -> Vec<OpShape> {
        manifest
            .operations
            .iter()
            .map(|op| OpShape {
                name: op.name.clone(),
                method: super::method_word(op.method).to_owned(),
                path: op.path.split('?').next().unwrap_or(&op.path).to_owned(),
            })
            .collect()
    }

    fn segments(path: &str) -> Vec<&str> {
        path.split('?')
            .next()
            .unwrap_or(path)
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn is_param(segment: &str) -> bool {
        segment.starts_with('{') && segment.ends_with('}')
    }

    /// The path's captured parameters when it matches the template.
    fn matches(template: &str, path: &str) -> Option<BTreeMap<String, String>> {
        let want = segments(template);
        let have = segments(path);
        if want.len() != have.len() {
            return None;
        }
        let mut captured = BTreeMap::new();
        for (w, h) in want.iter().zip(have.iter()) {
            if is_param(w) {
                captured.insert(
                    w.trim_matches(|c| c == '{' || c == '}').to_owned(),
                    (*h).to_owned(),
                );
            } else if !w.eq_ignore_ascii_case(h) {
                return None;
            }
        }
        Some(captured)
    }

    /// The resource a template addresses: a `{table}` parameter's value,
    /// else the last static segment.
    pub fn resource_of(template: &str, captured: &BTreeMap<String, String>) -> String {
        if let Some(table) = captured.get("table") {
            return table.clone();
        }
        segments(template)
            .iter()
            .rev()
            .find(|s| !is_param(s))
            .map(|s| (*s).to_owned())
            .unwrap_or_else(|| "records".to_owned())
    }

    /// The parameter that names one record: the template's trailing one,
    /// when there is one and it is not the table.
    fn identity(template: &str) -> Option<String> {
        segments(template)
            .last()
            .filter(|s| is_param(s))
            .map(|s| s.trim_matches(|c| c == '{' || c == '}').to_owned())
            .filter(|p| p != "table")
    }

    /// A table for every resource the operations address, empty when the
    /// seed has none.
    pub fn ensure_tables(seed: &mut Value, ops: &[OpShape]) {
        if !seed.is_object() {
            *seed = json!({});
        }
        if seed.get("tables").and_then(Value::as_object).is_none() {
            seed["tables"] = json!({});
        }
        for op in ops {
            let resource = resource_of(&op.path, &BTreeMap::new());
            if segments(&op.path).contains(&"{table}") {
                continue;
            }
            if seed["tables"].get(&resource).is_none() {
                seed["tables"][&resource] = json!([]);
            }
        }
    }

    fn same(field: &Value, wanted: &str) -> bool {
        match field {
            Value::String(s) => s == wanted,
            Value::Number(n) => n.to_string() == wanted,
            Value::Bool(b) => b.to_string() == wanted,
            _ => false,
        }
    }

    fn is_record(row: &Value, key: &str, wanted: &str) -> bool {
        [key, "number", "id", "sys_id"]
            .iter()
            .any(|k| row.get(k).is_some_and(|v| same(v, wanted)))
    }

    pub fn serve(
        state: &mut Value,
        ops: &[OpShape],
        method: HttpMethod,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        written_by: Option<&Value>,
    ) -> (u16, Value) {
        let verb = super::method_word(method);
        let matched = ops
            .iter()
            .filter(|op| op.method == verb)
            .filter_map(|op| matches(&op.path, path).map(|captured| (op, captured)))
            .max_by_key(|(op, _)| segments(&op.path).iter().filter(|s| !is_param(s)).count());
        let Some((op, captured)) = matched else {
            let known: Vec<String> = ops
                .iter()
                .map(|o| format!("{} {} ({})", o.method, o.path, o.name))
                .collect();
            return (
                404,
                json!({"error": {"message": format!("no operation of this connector answers {verb} {path}; it knows: {}", known.join(", "))}}),
            );
        };
        let resource = resource_of(&op.path, &captured);
        if state.get("tables").and_then(Value::as_object).is_none() {
            state["tables"] = json!({});
        }
        if state["tables"].get(&resource).is_none() {
            state["tables"][&resource] = json!([]);
        }
        let identity = identity(&op.path);
        let scope: Vec<(&String, &String)> = captured
            .iter()
            .filter(|(k, _)| Some(k.as_str()) != identity.as_deref() && k.as_str() != "table")
            .collect();
        let in_scope = |row: &Value| {
            scope
                .iter()
                .all(|(k, v)| row.get(k.as_str()).is_none_or(|f| same(f, v)))
        };
        let seeded_next = state
            .get("counters")
            .and_then(|c| c.get(&resource))
            .and_then(Value::as_u64);
        let mut bump: Option<u64> = None;
        let rows = state["tables"][&resource]
            .as_array_mut()
            .expect("a table is a list");
        let answer = match (verb, identity) {
            ("GET", Some(key)) => {
                let wanted = &captured[&key];
                match rows
                    .iter()
                    .find(|r| in_scope(r) && is_record(r, &key, wanted))
                {
                    Some(row) => (200, row.clone()),
                    None => (
                        404,
                        json!({"error": {"message": format!("no {resource} record `{wanted}`")}}),
                    ),
                }
            }
            ("GET", None) => {
                let found: Vec<Value> = rows
                    .iter()
                    .filter(|r| in_scope(r))
                    .filter(|r| {
                        query
                            .iter()
                            .all(|(k, v)| r.get(k).is_none_or(|f| same(f, v)))
                    })
                    .cloned()
                    .collect();
                (200, Value::Array(found))
            }
            ("POST", _) => {
                let mut row: Map<String, Value> =
                    body.and_then(Value::as_object).cloned().unwrap_or_default();
                for (k, v) in &scope {
                    row.entry((*k).clone())
                        .or_insert_with(|| Value::String((*v).clone()));
                }
                let next = seeded_next.unwrap_or(rows.len() as u64 + 1).max(1);
                row.entry("number".to_owned())
                    .or_insert_with(|| json!(next));
                row.entry("id".to_owned()).or_insert_with(|| json!(next));
                row.entry("created_at".to_owned())
                    .or_insert_with(|| json!(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()));
                // The row says which run wrote it: the stand-in's own receipt,
                // read from the world's page into Observe.
                if let Some(by) = written_by {
                    row.insert("_written_by".to_owned(), by.clone());
                }
                let made = Value::Object(row);
                rows.push(made.clone());
                bump = Some(next + 1);
                (201, made)
            }
            ("PATCH" | "PUT", Some(key)) => {
                let wanted = captured[&key].clone();
                match rows
                    .iter_mut()
                    .find(|r| in_scope(r) && is_record(r, &key, &wanted))
                {
                    Some(row) => {
                        if let (Some(target), Some(changes)) =
                            (row.as_object_mut(), body.and_then(Value::as_object))
                        {
                            for (k, v) in changes {
                                target.insert(k.clone(), v.clone());
                            }
                        }
                        (200, row.clone())
                    }
                    None => (
                        404,
                        json!({"error": {"message": format!("no {resource} record `{wanted}`")}}),
                    ),
                }
            }
            ("DELETE", Some(key)) => {
                let wanted = captured[&key].clone();
                let before = rows.len();
                rows.retain(|r| !(in_scope(r) && is_record(r, &key, &wanted)));
                if rows.len() < before {
                    (204, json!({}))
                } else {
                    (
                        404,
                        json!({"error": {"message": format!("no {resource} record `{wanted}`")}}),
                    )
                }
            }
            _ => (
                405,
                json!({"error": {"message": format!("{verb} on a collection of {resource} is not something this dialect does")}}),
            ),
        };
        if let Some(next) = bump {
            if state.get("counters").and_then(Value::as_object).is_none() {
                state["counters"] = json!({});
            }
            state["counters"][&resource] = json!(next);
        }
        answer
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn github_ops() -> Vec<OpShape> {
            let op = |name: &str, method: &str, path: &str| OpShape {
                name: name.into(),
                method: method.into(),
                path: path.into(),
            };
            vec![
                op("list-repos", "GET", "/user/repos"),
                op(
                    "get-issue",
                    "GET",
                    "/repos/{owner}/{repo}/issues/{issue_number}",
                ),
                op("list-issues", "GET", "/repos/{owner}/{repo}/issues"),
                op("create-issue", "POST", "/repos/{owner}/{repo}/issues"),
                op(
                    "update-issue",
                    "PATCH",
                    "/repos/{owner}/{repo}/issues/{issue_number}",
                ),
                op("check", "GET", "/user"),
            ]
        }

        fn seed() -> Value {
            json!({"tables": {"issues": [
                {"number": 1, "owner": "acme", "repo": "api", "title": "Login times out", "state": "open"},
                {"number": 2, "owner": "acme", "repo": "api", "title": "Typo on the docs page", "state": "closed"},
                {"number": 7, "owner": "acme", "repo": "web", "title": "Menu overlaps", "state": "open"}
            ]}})
        }

        fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        }

        #[test]
        fn the_resource_is_the_table_parameter_or_the_last_static_segment() {
            assert_eq!(
                resource_of("/repos/{owner}/{repo}/issues", &BTreeMap::new()),
                "issues"
            );
            assert_eq!(
                resource_of(
                    "/repos/{owner}/{repo}/issues/{issue_number}",
                    &BTreeMap::new()
                ),
                "issues"
            );
            assert_eq!(
                resource_of(
                    "/api/now/table/{table}",
                    &[("table".to_owned(), "problem".to_owned())]
                        .into_iter()
                        .collect()
                ),
                "problem"
            );
            assert_eq!(resource_of("/user", &BTreeMap::new()), "user");
        }

        #[test]
        fn a_collection_read_is_scoped_by_the_path_and_filtered_by_query_fields() {
            let mut state = seed();
            let (status, all) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/repos/acme/api/issues",
                &[],
                None,
                None,
            );
            assert_eq!(status, 200);
            assert_eq!(all.as_array().unwrap().len(), 2, "{all}");
            let (_, open) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/repos/acme/api/issues",
                &q(&[("state", "open")]),
                None,
                None,
            );
            assert_eq!(open.as_array().unwrap().len(), 1);
            assert_eq!(open[0]["number"], 1);
            // A query parameter no record carries filters nothing.
            let (_, sorted) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/repos/acme/api/issues",
                &q(&[("sort", "updated")]),
                None,
                None,
            );
            assert_eq!(sorted.as_array().unwrap().len(), 2);
        }

        #[test]
        fn a_record_is_read_by_its_trailing_parameter_and_filed_with_a_number() {
            let mut state = seed();
            let (status, one) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/repos/acme/web/issues/7",
                &[],
                None,
                None,
            );
            assert_eq!(status, 200, "{one}");
            assert_eq!(one["title"], "Menu overlaps");
            let (status, missing) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/repos/acme/api/issues/7",
                &[],
                None,
                None,
            );
            assert_eq!(status, 404, "the record is in another repo: {missing}");
            let (status, made) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Post,
                "/repos/acme/api/issues",
                &[],
                Some(&json!({"title": "Badge reader", "body": "rejects every card"})),
                None,
            );
            assert!(made["_written_by"].is_null(), "no run, no receipt: {made}");
            let (_, receipted) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Post,
                "/repos/acme/api/issues",
                &[],
                Some(&json!({"title": "Coffee machine"})),
                Some(&json!({"run_id": "run-1"})),
            );
            assert_eq!(
                receipted["_written_by"]["run_id"],
                json!("run-1"),
                "the row says which run wrote it: {receipted}"
            );
            assert_eq!(status, 201, "{made}");
            assert_eq!(made["number"], 4, "one past the seeded three: {made}");
            assert_eq!(made["owner"], "acme");
            assert_eq!(made["repo"], "api");
            let (status, again) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/repos/acme/api/issues/4",
                &[],
                None,
                None,
            );
            assert_eq!(status, 200, "{again}");
            assert_eq!(again["title"], "Badge reader");
            let (status, _) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Patch,
                "/repos/acme/api/issues/4",
                &[],
                Some(&json!({"state": "closed"})),
                None,
            );
            assert_eq!(status, 200);
            assert_eq!(state["tables"]["issues"][3]["state"], "closed");
            // An operation the connector does not declare is not answered.
            let (status, refused) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Delete,
                "/repos/acme/api/issues/4",
                &[],
                None,
                None,
            );
            assert_eq!(status, 404, "{refused}");
        }

        #[test]
        fn an_unknown_path_names_what_the_connector_knows_and_tables_are_made_for_every_resource() {
            let mut state = seed();
            let (status, refused) = serve(
                &mut state,
                &github_ops(),
                HttpMethod::Get,
                "/orgs/acme/members",
                &[],
                None,
                None,
            );
            assert_eq!(status, 404);
            assert!(
                refused["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("list-issues"),
                "{refused}"
            );
            let mut fresh = json!({"tables": {}});
            ensure_tables(&mut fresh, &github_ops());
            let tables: Vec<&String> = fresh["tables"].as_object().unwrap().keys().collect();
            assert_eq!(tables, vec!["issues", "repos", "user"]);
        }
    }
}

/// The Slack Web API, as far as an agent that reads channels and posts a
/// message goes: the four operations the library's Slack connector
/// declares, plus a channel's history so a case can read back what was
/// posted. Slack answers 200 with `ok`, true or false, and an error word.
mod slack {
    use super::*;

    fn param<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn tables(state: &mut Value) -> &mut Map<String, Value> {
        if !state.is_object() {
            *state = json!({});
        }
        let object = state.as_object_mut().expect("an object");
        if !object.get("tables").is_some_and(Value::is_object) {
            object.insert("tables".to_owned(), json!({}));
        }
        object
            .get_mut("tables")
            .and_then(Value::as_object_mut)
            .expect("tables")
    }

    fn rows<'a>(state: &'a mut Value, table: &str) -> &'a mut Vec<Value> {
        let tables = tables(state);
        if !tables.get(table).is_some_and(Value::is_array) {
            tables.insert(table.to_owned(), json!([]));
        }
        tables
            .get_mut(table)
            .and_then(Value::as_array_mut)
            .expect("rows")
    }

    fn err(word: &str) -> (u16, Value) {
        (200, json!({"ok": false, "error": word}))
    }

    /// A channel by `#name`, `name` or id.
    fn channel(state: &mut Value, wanted: &str) -> Option<Value> {
        let wanted = wanted.trim();
        let name = wanted.trim_start_matches('#');
        rows(state, "channels")
            .iter()
            .find(|c| {
                c.get("id").and_then(Value::as_str) == Some(wanted)
                    || c.get("name").and_then(Value::as_str) == Some(name)
            })
            .cloned()
    }

    /// Answer one request against `state`.
    pub fn serve(
        state: &mut Value,
        method: HttpMethod,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        written_by: Option<&Value>,
    ) -> (u16, Value) {
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        match (method, segments.as_slice()) {
            (HttpMethod::Get | HttpMethod::Post, ["api", "auth.test"]) => (
                200,
                json!({"ok": true, "url": "https://rusty-twin.slack.com/", "team": "Rusty Twin", "user": "rusty", "team_id": "T0RUSTYTWIN", "user_id": "U0RUSTYBOT"}),
            ),
            (HttpMethod::Get | HttpMethod::Post, ["api", "conversations.list"]) => {
                let channels = rows(state, "channels").clone();
                (
                    200,
                    json!({"ok": true, "channels": channels, "response_metadata": {"next_cursor": ""}}),
                )
            }
            (HttpMethod::Get | HttpMethod::Post, ["api", "users.list"]) => {
                let members = rows(state, "users").clone();
                (
                    200,
                    json!({"ok": true, "members": members, "response_metadata": {"next_cursor": ""}}),
                )
            }
            (HttpMethod::Get | HttpMethod::Post, ["api", "conversations.history"]) => {
                let Some(wanted) = param(query, "channel").map(str::to_owned).or_else(|| {
                    body.and_then(|b| b.get("channel"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }) else {
                    return err("channel_not_found");
                };
                let Some(found) = channel(state, &wanted) else {
                    return err("channel_not_found");
                };
                let id = found
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let mut messages: Vec<Value> = rows(state, "messages")
                    .iter()
                    .filter(|m| m.get("channel").and_then(Value::as_str) == Some(id.as_str()))
                    .cloned()
                    .collect();
                messages.sort_by(|a, b| {
                    b.get("ts")
                        .and_then(Value::as_str)
                        .cmp(&a.get("ts").and_then(Value::as_str))
                });
                (
                    200,
                    json!({"ok": true, "messages": messages, "has_more": false}),
                )
            }
            (HttpMethod::Post, ["api", "chat.postMessage"]) => {
                let Some(Value::Object(fields)) = body else {
                    return err("invalid_json");
                };
                let Some(wanted) = fields
                    .get("channel")
                    .and_then(Value::as_str)
                    .filter(|c| !c.trim().is_empty())
                else {
                    return err("channel_not_found");
                };
                let Some(text) = fields
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                else {
                    return err("no_text");
                };
                let Some(found) = channel(state, wanted) else {
                    return err("channel_not_found");
                };
                if rows(state, "messages").len() >= MAX_ROWS_PER_TABLE {
                    return err("ratelimited");
                }
                let now = Utc::now();
                let ts = format!("{}.{:06}", now.timestamp(), now.timestamp_subsec_micros());
                let id = found.get("id").cloned().unwrap_or(Value::Null);
                let mut message = json!({
                    "type": "message",
                    "ts": ts,
                    "channel": id,
                    "channel_name": found.get("name").cloned().unwrap_or(Value::Null),
                    "user": "U0RUSTYBOT",
                    "text": text,
                });
                if let Some(by) = written_by {
                    message["_written_by"] = by.clone();
                }
                rows(state, "messages").push(message.clone());
                (
                    200,
                    json!({"ok": true, "channel": message["channel"], "ts": message["ts"], "message": {"type": "message", "text": text, "user": "U0RUSTYBOT", "ts": message["ts"]}}),
                )
            }
            _ => err("unknown_method"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_post_lands_in_the_channel_and_reads_back_in_its_history() {
            let mut state = Dialect::by_id("slack-web").unwrap().starter();
            let by = json!({"run_id": "run-1", "at": "2026-09-13T09:00:00Z"});
            let (status, list) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/conversations.list",
                &[],
                None,
                None,
            );
            assert_eq!(status, 200);
            assert_eq!(list["ok"], json!(true));
            assert_eq!(list["channels"].as_array().map(Vec::len), Some(2));
            let (_, posted) = serve(
                &mut state,
                HttpMethod::Post,
                "/api/chat.postMessage",
                &[],
                Some(&json!({"channel": "#ops", "text": "Morning brief: 3 open incidents."})),
                Some(&by),
            );
            assert_eq!(posted["ok"], json!(true), "{posted}");
            assert_eq!(
                posted["channel"],
                json!("C0OPSROOM1"),
                "#ops is resolved to its id: {posted}"
            );
            let (_, history) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/conversations.history",
                &[("channel".to_owned(), "C0OPSROOM1".to_owned())],
                None,
                None,
            );
            assert_eq!(
                history["messages"][0]["text"],
                json!("Morning brief: 3 open incidents."),
                "newest first: {history}"
            );
            assert_eq!(
                history["messages"][0]["_written_by"]["run_id"],
                json!("run-1"),
                "the row says which run wrote it"
            );
            let (_, missing) = serve(
                &mut state,
                HttpMethod::Post,
                "/api/chat.postMessage",
                &[],
                Some(&json!({"channel": "#nowhere", "text": "hi"})),
                None,
            );
            assert_eq!(missing, json!({"ok": false, "error": "channel_not_found"}));
            let (_, empty) = serve(
                &mut state,
                HttpMethod::Post,
                "/api/chat.postMessage",
                &[],
                Some(&json!({"channel": "#ops", "text": ""})),
                None,
            );
            assert_eq!(empty["error"], json!("no_text"));
            let (_, me) = serve(
                &mut state,
                HttpMethod::Get,
                "/api/auth.test",
                &[],
                None,
                None,
            );
            assert_eq!(me["ok"], json!(true));
        }
    }
}

#[cfg(test)]
mod fault_tests {
    use super::*;

    #[test]
    fn a_seeded_fault_loses_that_many_matching_answers_and_a_reset_restores_them() {
        let seed = json!({"tables": {}, "faults": [{"drop_response": "POST /api/now/table/problem", "times": 1}, {"delay": "PATCH /api/now/table/problem", "seconds": 30}]});
        let mut state = seed.clone();
        assert_eq!(faults_left(&state), 2);
        // A read is not the fault's; neither is a write to another table.
        assert_eq!(
            spend_fault(&mut state, "GET", "/api/now/table/problem"),
            None
        );
        assert_eq!(
            spend_fault(&mut state, "POST", "/api/now/table/incident"),
            None
        );
        // The matching write loses its answer once, then answers; the
        // update is held once, then answers.
        assert_eq!(
            spend_fault(&mut state, "POST", "/api/now/table/problem"),
            Some(FaultKind::DropResponse)
        );
        assert_eq!(
            spend_fault(&mut state, "POST", "/api/now/table/problem"),
            None
        );
        assert_eq!(
            spend_fault(&mut state, "PATCH", "/api/now/table/problem/abc"),
            Some(FaultKind::Delay(30))
        );
        assert_eq!(
            spend_fault(&mut state, "PATCH", "/api/now/table/problem/abc"),
            None
        );
        assert_eq!(faults_left(&state), 0);
        // Back to the seed, the fault is armed again.
        let state = seed.clone();
        assert_eq!(faults_left(&state), 2);
    }

    #[test]
    fn a_fault_is_read_as_written_and_refused_when_malformed() {
        assert_eq!(
            parse_fault(&json!({"drop_response": "post /v1/entries"})),
            Some((
                FaultKind::DropResponse,
                "POST".to_owned(),
                "/v1/entries".to_owned(),
                1
            ))
        );
        assert_eq!(
            parse_fault(&json!({"drop_response": "POST /x", "times": 3})).map(|f| f.3),
            Some(3)
        );
        assert_eq!(
            parse_fault(&json!({"delay": "POST /x", "seconds": 25})).map(|f| f.0),
            Some(FaultKind::Delay(25))
        );
        assert_eq!(
            parse_fault(&json!({"delay": "POST /x"})).map(|f| f.0),
            Some(FaultKind::Delay(10)),
            "ten seconds when unsaid"
        );
        assert!(
            parse_fault(&json!({"drop_response": "POST"})).is_none(),
            "no path"
        );
        assert!(
            parse_fault(&json!({"drop_response": "POST x"})).is_none(),
            "a path starts with /"
        );
        assert!(
            validate_seed(&json!({"tables": {}, "faults": [{"drop_response": "POST /a"}]})).is_ok()
        );
        assert!(
            validate_seed(&json!({"tables": {}, "faults": {"drop_response": "POST /a"}})).is_err(),
            "a list"
        );
        assert!(
            validate_seed(&json!({"tables": {}, "faults": [{"times": 1}]})).is_err(),
            "names what to drop"
        );
    }
}
