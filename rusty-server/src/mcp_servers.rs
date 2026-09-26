//! MCP servers Rusty connects *to* — the inbound half of the bridge
//! (`mcp_bridge.rs` is the outbound half, where Rusty *is* an MCP server).
//!
//! A registered MCP server is a child process this server spawns over
//! stdio (the MCP stdio transport). Its `tools/list` becomes tools on every
//! graph that reads the live [`McpTools`] source, named `{server}.{tool}`
//! — the dotted form a connection's operations already use, so the agent
//! form, the run's allow-list and the journal all say the same name.
//!
//! Three rules hold the trust boundary:
//!
//! - **Spawning is an operator decision.** A deployment enables stdio
//!   servers explicitly ([`crate::ServerConfig::with_mcp_stdio`];
//!   `RUSTY_MCP_STDIO=1` on the demo), and registering one needs
//!   `mcp:admin`. A process this server spawns runs with this server's
//!   user and network — *outside* the egress policy that governs
//!   connectors — and the surface says so rather than pretending otherwise.
//! - **Effects are declared, never inferred.** MCP carries no effect. A
//!   server's `annotations` are advisory (the spec forbids trusting them
//!   for safety), so they only *suggest*; the registering admin declares
//!   each tool's effect, and an undeclared tool is `NonIdempotent` — the
//!   runtime's fail-closed default, which every gate treats as a write.
//! - **Secrets are sealed.** Environment values marked secret are sealed by
//!   the broker with the tenant-scoped server id as associated data, opened
//!   only to spawn the process, and served masked.
//!
//! A server saves only after it answers: `POST /mcp/servers` spawns,
//! initializes and lists before it persists anything. Records live under
//! `{store_path}/mcp-servers/`; the server remounts them at boot when stdio
//! servers are enabled. Only the default tenant's servers are mounted — the
//! registry is one per process, the limit `refresh_connection_tools` names.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{scope_id, tenant_of_internal, TenantContext, DEFAULT_TENANT};
use crate::connectors::{load_manifests, load_records, persist_json, remove_record};
use crate::error::ApiError;
use crate::routes::AppState;
use rusty_agent_runtime::broker::SealedCredential;
use rusty_agent_runtime::error::Result as CoreResult;
use rusty_agent_runtime::mcp::{InitializeResult, McpClient, McpStdioClient, McpToolAdapter, McpToolInfo};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::{Tool, ToolSource, MAX_TOOL_DESCRIPTION_BYTES, MAX_TOOL_SCHEMA_BYTES};

const ID_PREFIX: &str = "mcp-";
const MAX_SERVER_NAME_LEN: usize = 32;
/// How long one request to the child may take — `initialize` on a server
/// that installs itself on first run (`npx -y …`) can be slow.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

fn dir(root: &Path) -> PathBuf {
    root.join("mcp-servers")
}

/// One registered server, as persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct McpServerRecord {
    pub id: String,
    /// The tool prefix: kebab-case, so `{name}.{tool}` is a valid tool name.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Plain environment, served in full.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Sealed environment, served as names only.
    #[serde(default)]
    pub sealed_env: BTreeMap<String, SealedCredential>,
    /// The effect each tool was declared with. Absent means `NonIdempotent`.
    #[serde(default)]
    pub tool_effects: BTreeMap<String, Effect>,
    pub created_at: String,
    #[serde(default)]
    pub created_by: Value,
}

/// One tool as mounted.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MountedTool {
    /// The registry name: `{server}.{tool}`.
    pub name: String,
    /// The server's own name for it.
    pub tool: String,
    pub description: String,
    pub effect: Effect,
}

/// What a server holds right now.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MountStatus {
    /// `mounted`, `failed`, or `not_mounted`.
    pub state: &'static str,
    /// `{name} {version}` from the handshake.
    pub server: Option<String>,
    pub tools: Vec<MountedTool>,
    /// Tools the registry's contract refuses (an oversized or non-object
    /// schema) — named, not mounted.
    pub left_out: Vec<String>,
    pub error: Option<String>,
}

impl MountStatus {
    fn not_mounted(reason: Option<String>) -> Self {
        Self { state: "not_mounted", server: None, tools: Vec::new(), left_out: Vec::new(), error: reason }
    }
    fn failed(error: String) -> Self {
        Self { state: "failed", server: None, tools: Vec::new(), left_out: Vec::new(), error: Some(error) }
    }
}

struct Mount {
    /// Holding the client keeps the child alive; dropping it kills the child.
    client: Option<McpClient>,
    tools: Vec<Arc<dyn Tool>>,
    status: MountStatus,
}

/// The tools of every mounted MCP server, as a live [`ToolSource`] the
/// graph registries read. Empty until the server fills it.
#[derive(Default)]
pub struct McpTools {
    tools: std::sync::RwLock<Vec<Arc<dyn Tool>>>,
    mounts: std::sync::RwLock<HashMap<String, Mount>>,
}

impl std::fmt::Debug for McpTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.tools.read().map(|tools| tools.len()).unwrap_or(0);
        f.debug_struct("McpTools").field("tools", &count).finish()
    }
}

impl McpTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn set(&self, id: &str, mount: Mount) {
        if let Ok(mut mounts) = self.mounts.write() {
            mounts.insert(id.to_owned(), mount);
        }
        self.rebuild();
    }

    fn take(&self, id: &str) -> Option<McpClient> {
        let taken = self.mounts.write().ok().and_then(|mut mounts| mounts.remove(id));
        self.rebuild();
        taken.and_then(|mount| mount.client)
    }

    fn status(&self, id: &str) -> Option<MountStatus> {
        self.mounts.read().ok().and_then(|mounts| mounts.get(id).map(|m| m.status.clone()))
    }

    /// Every mounted server's tools, in one list, server by server.
    fn rebuild(&self) {
        let flat: Vec<Arc<dyn Tool>> = self
            .mounts
            .read()
            .map(|mounts| {
                let mut ids: Vec<&String> = mounts.keys().collect();
                ids.sort();
                ids.into_iter().flat_map(|id| mounts[id].tools.iter().cloned()).collect()
            })
            .unwrap_or_default();
        if let Ok(mut tools) = self.tools.write() {
            *tools = flat;
        }
    }
}

impl ToolSource for McpTools {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().map(|tools| tools.clone()).unwrap_or_default()
    }
}

/// An MCP tool under the name and effect the admin declared for it.
struct DeclaredTool {
    name: String,
    description: String,
    effect: Effect,
    inner: McpToolAdapter,
}

#[async_trait]
impl Tool for DeclaredTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }
    fn effect(&self) -> Effect {
        self.effect
    }
    async fn call(&self, args: Value) -> CoreResult<Value> {
        self.inner.call(args).await
    }
}

/// What a server's annotations suggest. Advisory: a host declares.
/// Anything short of an explicit read-only claim is a write, because the
/// gates read it that way and a wrong "read" is the expensive mistake.
pub(crate) fn suggested_effect(annotations: Option<&Value>) -> Effect {
    let Some(a) = annotations else {
        return Effect::NonIdempotent;
    };
    let flag = |key: &str| a.get(key).and_then(Value::as_bool);
    if flag("readOnlyHint") == Some(true) {
        return Effect::ReadOnly;
    }
    if flag("destructiveHint") == Some(false) && flag("idempotentHint") == Some(true) {
        return Effect::Idempotent;
    }
    Effect::NonIdempotent
}

/// A description the tool contract accepts: trimmed, control-free,
/// bounded, and never empty — an MCP server may send nothing.
fn contract_description(server: &str, tool: &str, raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned();
    let text = if cleaned.is_empty() {
        format!("{tool} on the {server} MCP server.")
    } else {
        cleaned
    };
    if text.len() <= MAX_TOOL_DESCRIPTION_BYTES {
        return text;
    }
    let mut cut = MAX_TOOL_DESCRIPTION_BYTES - 1;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", text[..cut].trim_end())
}

/// Why a server name would be refused, if it would.
pub(crate) fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a server needs a name".to_owned());
    }
    if name.len() > MAX_SERVER_NAME_LEN {
        return Some(format!("a server name is at most {MAX_SERVER_NAME_LEN} characters"));
    }
    let kebab = name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !kebab {
        return Some("a server name is kebab-case — it prefixes every tool name (`files.read_file`)".to_owned());
    }
    None
}

/// Spawn, shake hands, list.
async fn connect(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> CoreResult<(McpClient, InitializeResult, Vec<McpToolInfo>)> {
    let client = McpStdioClient::spawn_with_env(command, args.iter().map(String::as_str), env)?;
    client.set_request_timeout(REQUEST_TIMEOUT);
    let init = client.initialize().await?;
    let tools = client.list_tools().await?;
    Ok((client, init, tools))
}

/// The environment the process is spawned with: plain values plus the
/// sealed ones, opened for this spawn only.
async fn opened_env(state: &AppState, tenant: &str, record: &McpServerRecord) -> Result<Vec<(String, String)>, String> {
    let mut env: Vec<(String, String)> = record.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let owner = scope_id(tenant, &record.id);
    for (key, envelope) in &record.sealed_env {
        let bytes = state.broker.open_connector_secret(&owner, envelope).await?;
        let value = String::from_utf8(bytes).map_err(|_| format!("sealed value for `{key}` is not UTF-8"))?;
        env.push((key.clone(), value));
    }
    Ok(env)
}

/// Mount one server into the live cell; the status says what happened.
async fn mount(state: &AppState, tenant: &str, record: &McpServerRecord) -> MountStatus {
    let Some(cell) = &state.mcp_tools else {
        return MountStatus::not_mounted(Some("this server hosts no graph that reads MCP tools".to_owned()));
    };
    if tenant != DEFAULT_TENANT {
        return MountStatus::not_mounted(Some("only the default tenant's servers are mounted".to_owned()));
    }
    let env = match opened_env(state, tenant, record).await {
        Ok(env) => env,
        Err(error) => {
            let status = MountStatus::failed(format!("secrets could not be opened: {error}"));
            cell.set(&record.id, Mount { client: None, tools: Vec::new(), status: status.clone() });
            return status;
        }
    };
    let (client, init, infos) = match connect(&record.command, &record.args, &env).await {
        Ok(ok) => ok,
        Err(error) => {
            let status = MountStatus::failed(error.to_string());
            cell.set(&record.id, Mount { client: None, tools: Vec::new(), status: status.clone() });
            return status;
        }
    };
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut mounted = Vec::new();
    let mut left_out = Vec::new();
    for info in infos {
        let schema_ok = info.input_schema.is_object()
            && serde_json::to_vec(&info.input_schema).map(|b| b.len() <= MAX_TOOL_SCHEMA_BYTES).unwrap_or(false);
        let name_ok = !info.name.is_empty()
            && info.name.len() + record.name.len() < 128
            && info.name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
        if !schema_ok || !name_ok {
            left_out.push(info.name.clone());
            continue;
        }
        let name = format!("{}.{}", record.name, info.name);
        let effect = record.tool_effects.get(&info.name).copied().unwrap_or(Effect::NonIdempotent);
        let description = contract_description(&record.name, &info.name, &info.description);
        mounted.push(MountedTool { name: name.clone(), tool: info.name.clone(), description: description.clone(), effect });
        tools.push(Arc::new(DeclaredTool { name, description, effect, inner: McpToolAdapter::new(client.clone(), info) }));
    }
    let status = MountStatus {
        state: "mounted",
        server: Some(format!("{} {}", init.server_name, init.server_version)),
        tools: mounted,
        left_out,
        error: None,
    };
    cell.set(&record.id, Mount { client: Some(client), tools, status: status.clone() });
    status
}

/// Unmount one server: the child is told to stop, then dropped.
async fn unmount(state: &AppState, id: &str) {
    if let Some(client) = state.mcp_tools.as_ref().and_then(|cell| cell.take(id)) {
        let _ = client.shutdown().await;
    }
}

fn records(state: &AppState, tenant: &str) -> Vec<McpServerRecord> {
    load_records::<McpServerRecord>(&dir(&state.config.store_path))
        .into_iter()
        .filter(|(key, _)| tenant_of_internal(key) == tenant)
        .map(|(_, record)| record)
        .collect()
}

fn record(state: &AppState, tenant: &str, id: &str) -> Option<McpServerRecord> {
    records(state, tenant).into_iter().find(|r| r.id == id)
}

/// Remount every stored server of the default tenant — the boot half.
pub(crate) async fn remount_all(state: &AppState) {
    if !state.config.mcp_stdio {
        return;
    }
    for record in records(state, DEFAULT_TENANT) {
        let status = mount(state, DEFAULT_TENANT, &record).await;
        tracing::info!(server = %record.name, state = status.state, tools = status.tools.len(), "mcp server");
    }
}

fn serve(state: &AppState, record: &McpServerRecord, status: MountStatus) -> Value {
    let mut env: Vec<Value> = record
        .env
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value, "secret": false }))
        .collect();
    env.extend(record.sealed_env.keys().map(|name| json!({ "name": name, "secret": true })));
    json!({
        "id": record.id,
        "name": record.name,
        "command": record.command,
        "args": record.args,
        "env": env,
        "tool_effects": record.tool_effects,
        "status": status,
        "enabled": state.config.mcp_stdio,
        "created_at": record.created_at,
        "created_by": record.created_by,
    })
}

/// The servers and their mounted tools, for a reader that is not a route.
pub(crate) fn overview(state: &AppState) -> Value {
    let servers: Vec<Value> = records(state, DEFAULT_TENANT)
        .into_iter()
        .map(|record| {
            let status = state
                .mcp_tools
                .as_ref()
                .and_then(|cell| cell.status(&record.id))
                .unwrap_or_else(|| MountStatus::not_mounted(None));
            json!({
                "name": record.name,
                "state": status.state,
                "tools": status.tools.iter().map(|t| json!({"name": t.name, "effect": t.effect, "description": t.description})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "enabled": state.config.mcp_stdio, "servers": servers })
}

fn disabled() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "mcp_disabled",
        "stdio MCP servers are disabled on this deployment — the operator enables them with RUSTY_MCP_STDIO=1 (a registered server is a process this server spawns)".to_owned(),
    )
}

// --------------------------------------------------------------------- //
// The HTTP surface
// --------------------------------------------------------------------- //

/// `GET /mcp/servers`
pub(crate) async fn list_servers(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Json<Value> {
    let servers: Vec<Value> = records(&state, tenant.tenant())
        .into_iter()
        .map(|record| {
            let status = state
                .mcp_tools
                .as_ref()
                .and_then(|cell| cell.status(&record.id))
                .unwrap_or_else(|| MountStatus::not_mounted(None));
            serve(&state, &record, status)
        })
        .collect();
    Json(json!({ "enabled": state.config.mcp_stdio, "servers": servers }))
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EnvEntry {
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    secret: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProbePayload {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<EnvEntry>,
}

fn check_launch(command: &str, env: &[EnvEntry]) -> Result<(), ApiError> {
    if command.trim().is_empty() {
        return Err(ApiError::bad_request("a server needs a command to run".to_owned()));
    }
    for entry in env {
        let ok = !entry.name.is_empty() && entry.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            return Err(ApiError::bad_request(format!("`{}` is not an environment variable name", entry.name)));
        }
    }
    Ok(())
}

/// `POST /mcp/servers/probe` — spawn, shake hands, list, stop. Nothing is
/// stored; the answer is what the admin declares effects against.
pub(crate) async fn probe(
    AxumState(state): AxumState<Arc<AppState>>,
    Json(payload): Json<ProbePayload>,
) -> Result<Json<Value>, ApiError> {
    if !state.config.mcp_stdio {
        return Err(disabled());
    }
    check_launch(&payload.command, &payload.env)?;
    let env: Vec<(String, String)> = payload.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect();
    let (client, init, tools) = connect(payload.command.trim(), &payload.args, &env)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "mcp_failed", e.to_string()))?;
    let _ = client.shutdown().await;
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
                "annotations": t.annotations,
                "suggested_effect": suggested_effect(t.annotations.as_ref()),
            })
        })
        .collect();
    Ok(Json(json!({
        "server": { "name": init.server_name, "version": init.server_version, "protocol": init.protocol_version },
        "tools": tools,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePayload {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<EnvEntry>,
    #[serde(default)]
    tool_effects: BTreeMap<String, Effect>,
}

/// `POST /mcp/servers` — validate, seal, mount; persist only once the
/// server answered.
pub(crate) async fn create_server(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreatePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !state.config.mcp_stdio {
        return Err(disabled());
    }
    let name = payload.name.trim().to_owned();
    if let Some(problem) = name_problem(&name) {
        return Err(ApiError::bad_request(problem));
    }
    check_launch(&payload.command, &payload.env)?;
    if records(&state, tenant.tenant()).iter().any(|r| r.name == name) {
        return Err(ApiError::conflict(format!("an MCP server named `{name}` is already registered")));
    }
    let taken_by_connector = load_manifests(&state.config.store_path)
        .into_iter()
        .filter(|(key, _)| tenant_of_internal(key) == tenant.tenant())
        .any(|(_, manifest)| manifest.id == name);
    if taken_by_connector {
        return Err(ApiError::conflict(format!(
            "`{name}` is a connector in the library; its tools would collide — pick another name"
        )));
    }

    let id = format!("{ID_PREFIX}{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let owner = scope_id(tenant.tenant(), &id);
    let mut env = BTreeMap::new();
    let mut sealed_env = BTreeMap::new();
    for entry in &payload.env {
        if entry.secret {
            let envelope = state
                .broker
                .seal_connector_secret(&owner, entry.value.as_bytes())
                .await
                .map_err(|e| ApiError::internal(format!("seal `{}`: {e}", entry.name)))?;
            sealed_env.insert(entry.name.clone(), envelope);
        } else {
            env.insert(entry.name.clone(), entry.value.clone());
        }
    }
    let record = McpServerRecord {
        id: id.clone(),
        name,
        command: payload.command.trim().to_owned(),
        args: payload.args,
        env,
        sealed_env,
        tool_effects: payload.tool_effects,
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by: tenant.attribution(),
    };

    let status = mount(&state, tenant.tenant(), &record).await;
    if status.state != "mounted" {
        unmount(&state, &record.id).await;
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "mcp_failed",
            status.error.unwrap_or_else(|| "the server did not answer".to_owned()),
        ));
    }
    persist_json(&dir(&state.config.store_path), &owner, &record)
        .await
        .map_err(|e| ApiError::internal(format!("mcp server store: {e}")))?;
    Ok((StatusCode::CREATED, Json(serve(&state, &record, status))))
}

/// `DELETE /mcp/servers/{server_id}` — stop the process, forget the record.
pub(crate) async fn delete_server(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(server_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let Some(record) = record(&state, tenant.tenant(), &server_id) else {
        return Err(ApiError::not_found(format!("unknown MCP server `{server_id}`")));
    };
    unmount(&state, &record.id).await;
    remove_record(&dir(&state.config.store_path), &scope_id(tenant.tenant(), &record.id))
        .await
        .map_err(|e| ApiError::internal(format!("mcp server store: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /mcp/servers/{server_id}/remount` — try again, after a boot that
/// could not start it.
pub(crate) async fn remount_server(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(server_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    if !state.config.mcp_stdio {
        return Err(disabled());
    }
    let Some(record) = record(&state, tenant.tenant(), &server_id) else {
        return Err(ApiError::not_found(format!("unknown MCP server `{server_id}`")));
    };
    unmount(&state, &record.id).await;
    let status = mount(&state, tenant.tenant(), &record).await;
    Ok(Json(serve(&state, &record, status)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_only_ever_suggest_and_default_to_a_write() {
        assert_eq!(suggested_effect(None), Effect::NonIdempotent);
        assert_eq!(suggested_effect(Some(&json!({"readOnlyHint": true}))), Effect::ReadOnly);
        assert_eq!(
            suggested_effect(Some(&json!({"destructiveHint": false, "idempotentHint": true}))),
            Effect::Idempotent
        );
        assert_eq!(suggested_effect(Some(&json!({"destructiveHint": true}))), Effect::NonIdempotent);
        assert_eq!(suggested_effect(Some(&json!({"readOnlyHint": false}))), Effect::NonIdempotent);
    }

    #[test]
    fn names_are_tool_prefixes() {
        assert!(name_problem("files").is_none());
        assert!(name_problem("my-server-2").is_none());
        assert!(name_problem("").is_some());
        assert!(name_problem("Files").is_some());
        assert!(name_problem("a--b").is_some());
        assert!(name_problem(&"x".repeat(33)).is_some());
    }

    #[test]
    fn a_description_always_satisfies_the_contract() {
        assert_eq!(contract_description("files", "read_file", ""), "read_file on the files MCP server.");
        assert_eq!(contract_description("files", "read_file", "  Reads\ta file \n"), "Reads a file");
        let long = contract_description("s", "t", &"x".repeat(MAX_TOOL_DESCRIPTION_BYTES + 100));
        assert!(long.len() <= MAX_TOOL_DESCRIPTION_BYTES + 3);
        assert!(long.ends_with('…'));
    }
}
