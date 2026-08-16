//! The connector plane's server slice: HTTP surface, vault bridge, real
//! transports, and durable records over the core contracts
//! (`rusty_agent_runtime::connector`).
//!
//! The core registry is the in-memory authority for manifests, instances,
//! and live sessions; this module is the server's half:
//!
//! - **Credential bridge.** The instantiate payload maps each declared
//!   credential slot to a tenant connection (`slot → connection_id`). The
//!   bridge resolves each pair through the deployment's vault
//!   ([`crate::broker::Broker`]): issue a handle, resolve it, and feed the
//!   opened access token into a throwaway
//!   [`InMemoryCredentialBroker`] that backs exactly one
//!   `ConnectorRegistry::instantiate` call. Secrets cross the vault
//!   boundary into the registry entry and nowhere else — no response, log
//!   line, or persisted record carries them (records persist the slot →
//!   connection-id binding, never the material).
//! - **Transports.** [`ReqwestTransport`] implements the core
//!   `HttpTransport` seam over reqwest, enforcing the response byte
//!   ceiling while streaming (a hostile endpoint cannot turn one search
//!   call into an unbounded allocation). MCP sessions spawn through the
//!   core provider at connect time.
//! - **Persistence.** Manifests (`{tenant}/{hash}`) and instance records
//!   (`{tenant}/{instance_id}`) ride the `ServerStore` backends — one JSON
//!   file per record under `{store_path}/connectors/…` on the file
//!   backend, one `server_connectors` row per record on Postgres. Live
//!   sessions are deliberately not durable: on boot every instance
//!   restores as `pending` (or `failed` when its credentials no longer
//!   resolve) and reconnects on demand.
//! - **Served catalog generations.** The registry's per-process
//!   generation counter resets on restart, so the durable authority is
//!   the instance record: [`fold_served_catalog`] advances the record's
//!   generation only when the derived catalog bytes change, monotone
//!   across restarts. Pins (`generation` query parameter) verify against
//!   the record, never against "latest" by accident — a mismatch is a
//!   409.
//!
//! Tenant isolation follows the store's indistinguishability rule: a
//! cross-tenant instance id is a 404, never a 403.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, OnceCell};

use rusty_agent_runtime::broker::{BrokerDenialReason, CredentialRequirement, IssueRequest};
use rusty_agent_runtime::connector::{
    default_provider, ConnectorInstance, ConnectorManifest, ConnectorRegistry, CredentialSlot,
    HttpTransport, InMemoryCredentialBroker, LifecycleState, SweepOutcome,
    MAX_SEARCH_RESPONSE_BYTES,
};
use rusty_agent_runtime::error::RustyError;
use rusty_agent_runtime::tool::ToolCapability;

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::server_store::ServerStore;

// --------------------------------------------------------------------- //
// Durable records
// --------------------------------------------------------------------- //

/// One durable catalog generation of an instance — the restart-surviving
/// authority the registry's in-memory counter reports into.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ServedCatalog {
    /// Monotone per-instance generation number (durable across restarts).
    pub generation: u64,
    /// SHA-256 over the canonical catalog serialization (core's digest).
    pub hash: String,
    /// The derived, namespaced tool catalog.
    pub tools: Vec<ToolCapability>,
}

/// The durable instance record. Secrets never appear here:
/// `credential_connections` binds slot names to connection ids — the
/// binding, not the material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConnectorInstanceRecord {
    /// The registry-minted instance id (`inst-NNNNNN`).
    pub instance_id: String,
    /// The connector id (denormalized from the manifest for listings).
    pub connector_id: String,
    /// The content hash of the pinned manifest.
    pub manifest_hash: String,
    /// Slot name → connection id, as declared at instantiation. Reused at
    /// boot to re-resolve credentials; carries no secret bytes.
    pub credential_connections: BTreeMap<String, String>,
    /// The lifecycle state name (`pending`, `healthy`, …).
    pub state: String,
    /// The bounded reason for `failed`/`degraded`, else `None`.
    pub state_reason: Option<String>,
    /// Consecutive health/connection failures since the last success.
    pub consecutive_failures: u32,
    /// Logical clock reading of the last health check.
    pub last_health_check_ms: Option<u64>,
    /// The last served catalog generation, when the instance has ever
    /// been healthy. Survives restart so pins stay verifiable.
    pub catalog: Option<ServedCatalog>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fold a freshly derived catalog into the record's generation chain:
/// equal bytes keep the generation, changed bytes advance it. This is the
/// durable counterpart of the registry's in-memory `adopt_catalog`.
fn fold_served_catalog(record: &mut ConnectorInstanceRecord, hash: &str, tools: Vec<ToolCapability>) {
    match &mut record.catalog {
        None => {
            record.catalog = Some(ServedCatalog {
                generation: 1,
                hash: hash.to_owned(),
                tools,
            });
        }
        Some(current) if current.hash == hash => {}
        Some(current) => {
            current.generation += 1;
            current.hash = hash.to_owned();
            current.tools = tools;
        }
    }
}

// --------------------------------------------------------------------- //
// File layout (the JsonFileStore backend's half; memory.rs conventions)
// --------------------------------------------------------------------- //

/// The connector manifest directory under the store root.
pub(crate) fn manifests_dir(root: &Path) -> PathBuf {
    root.join("connectors").join("manifests")
}

/// The connector instance directory under the store root.
pub(crate) fn instances_dir(root: &Path) -> PathBuf {
    root.join("connectors").join("instances")
}

/// Persist one record atomically (temp file + rename) under `dir`, named
/// by `scoped_key` — the shared file-backend durability discipline. The
/// key may carry a `{tenant}/` prefix, so the parent directory is
/// created, not just the flat dir.
async fn persist_record<T: Serialize>(
    dir: PathBuf,
    scoped_key: &str,
    record: &T,
    context: &str,
) -> io::Result<()> {
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{scoped_key}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dir.join(format!("{scoped_key}.{context}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Persist one manifest, keyed `{tenant}/{hash}`.
pub(crate) async fn persist_manifest(
    root: &Path,
    scoped_key: &str,
    manifest: &ConnectorManifest,
) -> io::Result<()> {
    persist_record(manifests_dir(root), scoped_key, manifest, "manifest").await
}

/// Persist one instance record, keyed `{tenant}/{instance_id}`.
pub(crate) async fn persist_instance(
    root: &Path,
    scoped_key: &str,
    record: &ConnectorInstanceRecord,
) -> io::Result<()> {
    persist_record(instances_dir(root), scoped_key, record, "instance").await
}

/// Recursively collect `*.json` files (tenant subdirectories hold that
/// tenant's records), the memory loader's walk.
fn collect_json_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Load all records under `dir`, keyed by their path-derived scoped key
/// (`{tenant}/{name}` for named tenants, bare for the default tenant).
/// Corrupt files are skipped with a warning: one bad record must not take
/// the plane down at boot.
fn load_records<T: for<'de> Deserialize<'de>>(dir: PathBuf) -> HashMap<String, T> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped_key = path
            .strip_prefix(&dir)
            .ok()
            .map(|relative| relative.with_extension(""))
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            });
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<T>(&raw).ok());
        match (scoped_key, parsed) {
            (Some(key), Some(record)) => {
                out.insert(key, record);
            }
            _ => tracing::warn!(path = %path.display(), "skipping unreadable connector file"),
        }
    }
    out
}

/// Load every persisted manifest (boot restore).
pub(crate) fn load_manifests(root: &Path) -> HashMap<String, ConnectorManifest> {
    load_records(manifests_dir(root))
}

/// Load every persisted instance record (boot restore).
pub(crate) fn load_instances(root: &Path) -> HashMap<String, ConnectorInstanceRecord> {
    load_records(instances_dir(root))
}

// --------------------------------------------------------------------- //
// Transports
// --------------------------------------------------------------------- //

/// The real HTTP transport: reqwest behind the core `HttpTransport` seam.
///
/// The response body is read as a stream with the provider's byte ceiling
/// enforced *during* the read — the ceiling trips before the allocation
/// grows past it, so a hostile or buggy endpoint cannot make the server
/// buffer unbounded bytes. The per-call timeout travels in the request
/// and is set on the reqwest call as well.
#[derive(Debug)]
pub(crate) struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub(crate) fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        request: rusty_agent_runtime::connector::HttpRequest,
    ) -> rusty_agent_runtime::error::Result<rusty_agent_runtime::connector::HttpResponse> {
        let transport_err = |e: reqwest::Error| {
            RustyError::Tool(format!("connector: search transport failed: {e}"))
        };
        let mut call = self
            .client
            .post(&request.url)
            .body(request.body)
            .timeout(request.timeout);
        for (name, value) in &request.headers {
            call = call.header(name, value);
        }
        let response = call.send().await.map_err(transport_err)?;
        let status = response.status().as_u16();

        use futures::StreamExt;
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_err)?;
            if body.len() + chunk.len() > MAX_SEARCH_RESPONSE_BYTES {
                return Err(RustyError::Tool(format!(
                    "connector: search response exceeds the {MAX_SEARCH_RESPONSE_BYTES}-byte ceiling"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(rusty_agent_runtime::connector::HttpResponse { status, body })
    }
}

// --------------------------------------------------------------------- //
// The plane
// --------------------------------------------------------------------- //

/// The server-side connector plane: the core registry (in-memory
/// authority over manifests, instances, and live sessions) plus the
/// durable records, the vault bridge, and the transport.
pub(crate) struct ConnectorPlane {
    registry: Mutex<ConnectorRegistry>,
    store: Arc<dyn ServerStore>,
    broker: Arc<crate::broker::Broker>,
    /// The real HTTP transport for search providers. Held by the plane so
    /// the tool-dispatch slice's search execution path wires one client —
    /// the pool outlives individual calls. No endpoint in this slice
    /// invokes search, so nothing reads it yet; the unit tests drive it
    /// directly.
    #[allow(dead_code)]
    transport: Arc<ReqwestTransport>,
    /// Boot restore runs exactly once, lazily on the first connector
    /// request (and deterministically before it): the store's records
    /// re-enter the registry before any handler answers. Lazy rather
    /// than spawned at startup so a restarted server never serves a
    /// half-restored plane.
    restored: OnceCell<()>,
}

impl ConnectorPlane {
    pub(crate) fn new(store: Arc<dyn ServerStore>, broker: Arc<crate::broker::Broker>) -> Self {
        Self {
            registry: Mutex::new(ConnectorRegistry::new()),
            store,
            broker,
            transport: Arc::new(ReqwestTransport::new()),
            restored: OnceCell::new(),
        }
    }

    /// Run boot restore exactly once.
    async fn ensure_restored(&self) -> Result<(), ApiError> {
        self.restored
            .get_or_try_init(|| self.restore())
            .await
            .map(|_| ())
            .map_err(|e: ApiError| e)
    }

    /// Re-register every persisted manifest, then re-instantiate every
    /// persisted instance in ascending id order — the order that
    /// reproduces the registry's `inst-NNNNNN` ids exactly (instances are
    /// never deleted, so mint order is stable). Restored instances hold
    /// no session: they come back `pending`, or `failed` when a
    /// credential slot no longer resolves, and reconnect on demand.
    async fn restore(&self) -> Result<(), ApiError> {
        let manifests = self
            .store
            .list_all_connector_manifests()
            .await
            .map_err(internal_err)?;
        let mut registry = self.registry.lock().await;
        for (_tenant, manifest) in manifests {
            match default_provider(&manifest) {
                Ok(provider) => {
                    // Idempotent by hash: a manifest several tenants
                    // registered enters the registry once.
                    if let Err(e) = registry.register_manifest(manifest, provider) {
                        tracing::warn!(error = %e, "connector manifest failed restore; skipping");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "connector manifest has no provider; skipping")
                }
            }
        }

        let mut instances = self
            .store
            .list_all_connector_instances()
            .await
            .map_err(internal_err)?;
        instances.sort_by(|left, right| left.1.instance_id.cmp(&right.1.instance_id));
        for (tenant, record) in instances {
            let credentials = self
                .resolve_slots_lenient(&tenant, &record.credential_connections)
                .await;
            match registry.instantiate(&record.manifest_hash, &tenant, &credentials) {
                Ok(minted) => {
                    if minted != record.instance_id {
                        // Mint order drifted from the persisted record —
                        // the id-alignment invariant is broken, and
                        // serving the wrong id would be worse than a gap.
                        tracing::warn!(
                            persisted = %record.instance_id,
                            minted = %minted,
                            "connector instance id drifted at restore"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(instance = %record.instance_id, error = %e, "connector instance failed restore; skipping");
                }
            }
        }
        drop(registry);

        // Mirror the restored states into the durable records (sessions
        // do not survive boot, so a record that said `healthy` now says
        // `pending`).
        let mut instances = self
            .store
            .list_all_connector_instances()
            .await
            .map_err(internal_err)?;
        instances.sort_by(|left, right| left.1.instance_id.cmp(&right.1.instance_id));
        for (tenant, record) in instances {
            let _ = self.sync_record(&tenant, &record.instance_id).await;
        }
        Ok(())
    }

    /// Strict slot resolution for `POST /connectors/instances`: every
    /// declared slot must name a connection, and every connection must
    /// issue and resolve — a vault denial is the client's 422, naming the
    /// slot and the denial's reason, never the material.
    async fn resolve_slots_strict(
        &self,
        tenant: &str,
        slots: &[CredentialSlot],
        connections: &BTreeMap<String, String>,
    ) -> Result<InMemoryCredentialBroker, ApiError> {
        let mut resolved = InMemoryCredentialBroker::new();
        for slot in slots {
            let connection_id = connections.get(&slot.name).ok_or_else(|| {
                ApiError::unprocessable(format!(
                    "credential slot `{}` requires a connection id in `credentials`",
                    slot.name
                ))
            })?;
            let secret = self
                .open_credential(tenant, &slot.name, connection_id)
                .await?;
            resolved.insert(tenant, slot.name.clone(), secret);
        }
        Ok(resolved)
    }

    /// Lenient slot resolution for boot restore: a slot that fails to
    /// resolve is simply absent from the broker, so the registry lands
    /// the instance in `failed` with its missing-slot reason rather than
    /// blocking the whole restore.
    async fn resolve_slots_lenient(
        &self,
        tenant: &str,
        connections: &BTreeMap<String, String>,
    ) -> InMemoryCredentialBroker {
        let mut resolved = InMemoryCredentialBroker::new();
        for (slot, connection_id) in connections {
            match self.open_credential(tenant, slot, connection_id).await {
                Ok(secret) => {
                    resolved.insert(tenant, slot.clone(), secret);
                }
                Err(e) => {
                    tracing::warn!(slot = %slot, error = %e, "connector credential unresolved at restore");
                }
            }
        }
        resolved
    }

    /// Open one credential: issue a handle against the connection, resolve
    /// it, and hand back the access token — the slot's secret. The vault
    /// journals both acts; this bridge adds no state of its own.
    async fn open_credential(
        &self,
        tenant: &str,
        slot: &str,
        connection_id: &str,
    ) -> Result<String, ApiError> {
        let request = IssueRequest {
            tenant: tenant.to_owned(),
            run_id: None,
            requirement: CredentialRequirement {
                connection_id: connection_id.to_owned(),
                scopes: Default::default(),
            },
        };
        let handle = self.broker.issue(&request).await.map_err(|denial| {
            ApiError::unprocessable(format!(
                "credential slot `{slot}` could not issue against connection `{connection_id}`: {}",
                denial_text(&denial.reason)
            ))
        })?;
        let credential = self
            .broker
            .resolve(&handle.token(), &Default::default())
            .await
            .map_err(|denial| {
                ApiError::unprocessable(format!(
                    "credential slot `{slot}` could not resolve against connection `{connection_id}`: {}",
                    denial_text(&denial.reason)
                ))
            })?;
        Ok(credential.material.access_token)
    }

    /// Mirror the registry's live state (and any newly derived catalog)
    /// into the durable record, then return the record for the response.
    async fn sync_record(
        &self,
        tenant: &str,
        instance_id: &str,
    ) -> Result<ConnectorInstanceRecord, ApiError> {
        let instance = {
            let registry = self.registry.lock().await;
            registry.instance(instance_id).cloned()
        }
        .ok_or_else(|| {
            ApiError::not_found(format!("connector instance `{instance_id}` not found"))
        })?;
        let mut record = self
            .store
            .get_connector_instance(tenant, instance_id)
            .await
            .map_err(internal_err)?
            .ok_or_else(|| {
                ApiError::internal(format!(
                    "connector instance `{instance_id}` has a registry entry but no record"
                ))
            })?;
        record.state = instance.state().name().to_owned();
        record.state_reason = match instance.state() {
            rusty_agent_runtime::connector::LifecycleState::Degraded { reason }
            | rusty_agent_runtime::connector::LifecycleState::Failed { reason } => {
                Some(reason.clone())
            }
            _ => None,
        };
        record.consecutive_failures = instance.consecutive_failures();
        record.last_health_check_ms = instance.last_health_check_ms();
        if let Some(generation) = instance.catalog() {
            fold_served_catalog(&mut record, &generation.hash, generation.tools.clone());
        }
        record.updated_at = Utc::now();
        self.store
            .upsert_connector_instance(tenant, &record)
            .await
            .map_err(internal_err)?;
        Ok(record)
    }

    /// The instance the caller's tenant owns — 404 for unknown and
    /// cross-tenant ids alike (the store's indistinguishability rule).
    async fn owned_instance(
        &self,
        tenant: &TenantContext,
        instance_id: &str,
    ) -> Result<ConnectorInstance, ApiError> {
        let instance = {
            let registry = self.registry.lock().await;
            registry.instance(instance_id).cloned()
        }
        .ok_or_else(|| {
            ApiError::not_found(format!("connector instance `{instance_id}` not found"))
        })?;
        if instance.tenant_id != tenant.tenant() {
            return Err(ApiError::not_found(format!(
                "connector instance `{instance_id}` not found"
            )));
        }
        Ok(instance)
    }
}

/// The plane off the app state.
fn plane(state: &AppState) -> &Arc<ConnectorPlane> {
    &state.connectors
}

/// The logical clock reading the lifecycle methods take: wall-clock
/// milliseconds at the server boundary (determinism lives in the core;
/// the server records real time).
fn now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

fn internal_err(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}

/// The API view of one instance: metadata and lifecycle, never
/// credentials.
fn instance_view(record: &ConnectorInstanceRecord) -> Value {
    json!({
        "instance_id": record.instance_id,
        "connector_id": record.connector_id,
        "manifest_hash": record.manifest_hash,
        "credential_slots": record.credential_connections.keys().collect::<Vec<_>>(),
        "state": record.state,
        "state_reason": record.state_reason,
        "consecutive_failures": record.consecutive_failures,
        "last_health_check_ms": record.last_health_check_ms,
        "catalog_generation": record.catalog.as_ref().map(|catalog| catalog.generation),
        "catalog_hash": record.catalog.as_ref().map(|catalog| catalog.hash.clone()),
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    })
}

/// The wire text of a vault denial: the reason serialized (it names
/// scopes, connections, and grants — never material). `BrokerDenialReason`
/// is the journaled evidence shape; the 422 answer carries the same
/// content.
fn denial_text(reason: &BrokerDenialReason) -> String {
    serde_json::to_string(reason).unwrap_or_else(|_| format!("{reason:?}"))
}

/// The API view of one lifecycle state: the name, plus the reason when
/// the state carries one.
fn lifecycle_view(state: &LifecycleState) -> Value {
    let reason = match state {
        LifecycleState::Degraded { reason } | LifecycleState::Failed { reason } => {
            Some(reason.clone())
        }
        _ => None,
    };
    json!({ "state": state.name(), "reason": reason })
}

/// The API view of one health/sweep outcome (core's `SweepOutcome` is
/// deliberately not `Serialize`; the wire shape is the server's
/// contract).
fn sweep_outcome_view(outcome: &SweepOutcome) -> Value {
    json!({
        "instance_id": outcome.instance_id,
        "previous_state": lifecycle_view(&outcome.previous),
        "current_state": lifecycle_view(&outcome.current),
        "catalog_bumped": outcome.catalog_bumped,
    })
}

// --------------------------------------------------------------------- //
// Handlers
// --------------------------------------------------------------------- //

/// `POST /connectors/manifests` body: the manifest content. `hash` is
/// optional — the server recomputes it from the canonical content and a
/// supplied hash that disagrees is a 422, not an override.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateManifestPayload {
    id: String,
    version: String,
    display_name: String,
    description: String,
    provider: rusty_agent_runtime::connector::ProviderKind,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    credential_slots: Vec<CredentialSlot>,
    #[serde(default)]
    hash: Option<String>,
}

/// `POST /connectors/manifests` — validate and register a manifest for
/// the caller's tenant. Idempotent by content hash: re-posting the same
/// bytes converges (`already_registered: true`), posting different bytes
/// registers a new entry — there is no update path. `201` with the
/// receipt; `422` when validation or the hash check refuses the content.
pub(crate) async fn create_connector_manifest(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateManifestPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    plane(&state).ensure_restored().await?;
    let manifest = ConnectorManifest::new(
        payload.id,
        payload.version,
        payload.display_name,
        payload.description,
        payload.provider,
        payload.capabilities,
        payload.credential_slots,
    )
    .map_err(|e| ApiError::unprocessable(e.to_string()))?;
    if let Some(declared) = &payload.hash {
        if *declared != manifest.hash {
            return Err(ApiError::unprocessable(format!(
                "declared hash `{declared}` does not match the manifest content ({})",
                manifest.hash
            )));
        }
    }

    let already = state
        .server_store
        .get_connector_manifest(tenant.tenant(), &manifest.hash)
        .await
        .map_err(internal_err)?
        .is_some();
    if !already {
        let provider = default_provider(&manifest).map_err(internal_err)?;
        plane(&state)
            .registry
            .lock()
            .await
            .register_manifest(manifest.clone(), provider)
            .map_err(internal_err)?;
        state
            .server_store
            .put_connector_manifest(tenant.tenant(), &manifest)
            .await
            .map_err(internal_err)?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "receipt": {
                "id": manifest.id,
                "version": manifest.version,
                "manifest_hash": manifest.hash,
                "already_registered": already,
            }
        })),
    ))
}

/// `GET /connectors/manifests` — the tenant's manifests, sorted by
/// `(id, hash)`.
pub(crate) async fn list_connector_manifests(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    plane(&state).ensure_restored().await?;
    let mut manifests = state
        .server_store
        .list_connector_manifests(tenant.tenant())
        .await
        .map_err(internal_err)?;
    manifests.sort_by(|left, right| left.id.cmp(&right.id).then(left.hash.cmp(&right.hash)));
    Ok(Json(json!({ "manifests": manifests })))
}

/// `POST /connectors/instances` body: the manifest pin plus the slot →
/// connection binding.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateInstancePayload {
    manifest_hash: String,
    /// Credential slot name → connection id. Every slot the manifest
    /// declares must appear; the material never crosses this boundary.
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

/// `POST /connectors/instances` — instantiate a manifest for the caller's
/// tenant. `404` when the tenant holds no manifest under the hash; `422`
/// when a declared slot is unbound or its connection refuses issuance
/// (the answer names the slot, never the secret). `201` with the instance
/// view; the instance starts `pending`.
pub(crate) async fn create_connector_instance(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CreateInstancePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    let manifest = state
        .server_store
        .get_connector_manifest(tenant.tenant(), &payload.manifest_hash)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no connector manifest registered under hash `{}`",
                payload.manifest_hash
            ))
        })?;
    let credentials = plane
        .resolve_slots_strict(tenant.tenant(), &manifest.credential_slots, &payload.credentials)
        .await?;

    let instance_id = plane
        .registry
        .lock()
        .await
        .instantiate(&manifest.hash, tenant.tenant(), &credentials)
        .map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let now = Utc::now();
    let record = ConnectorInstanceRecord {
        instance_id: instance_id.clone(),
        connector_id: manifest.id.clone(),
        manifest_hash: manifest.hash.clone(),
        credential_connections: payload.credentials,
        state: "pending".to_owned(),
        state_reason: None,
        consecutive_failures: 0,
        last_health_check_ms: None,
        catalog: None,
        created_at: now,
        updated_at: now,
    };
    state
        .server_store
        .upsert_connector_instance(tenant.tenant(), &record)
        .await
        .map_err(internal_err)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "instance": instance_view(&record) })),
    ))
}

/// `GET /connectors/instances` — the tenant's instances, sorted by
/// instance id, with lifecycle state and the current served catalog
/// generation.
pub(crate) async fn list_connector_instances(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    plane(&state).ensure_restored().await?;
    let mut records = state
        .server_store
        .list_connector_instances(tenant.tenant())
        .await
        .map_err(internal_err)?;
    records.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    let views: Vec<Value> = records.iter().map(instance_view).collect();
    Ok(Json(json!({ "instances": views })))
}

/// `POST /connectors/instances/{id}/connect` — spawn/attach the session
/// and derive the initial catalog. Provider failures land the instance in
/// `failed` and still answer `200` (the lifecycle is the answer); guard
/// violations (disabled, already healthy, already connecting) are `409`.
pub(crate) async fn connect_instance(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    plane.owned_instance(&tenant, &instance_id).await?;
    {
        let mut registry = plane.registry.lock().await;
        registry
            .connect(&instance_id, now_ms())
            .await
            .map_err(|e| ApiError::conflict(e.to_string()))?;
    }
    let record = plane.sync_record(tenant.tenant(), &instance_id).await?;
    Ok(Json(json!({ "instance": instance_view(&record) })))
}

/// `GET /connectors/instances/{id}/catalog` query: the generation pin.
#[derive(Debug, Deserialize)]
pub(crate) struct CatalogQuery {
    generation: Option<u64>,
}

/// `GET /connectors/instances/{id}/catalog?generation=N` — the served
/// catalog. Unpinned reads return the current generation; a pin that no
/// longer matches is a `409` naming the live generation, so a consumer
/// configured against generation N learns the world moved rather than
/// silently re-resolving. `404` for unknown/cross-tenant instances and
/// for instances that have never served a catalog.
pub(crate) async fn get_instance_catalog(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<Value>, ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    plane.owned_instance(&tenant, &instance_id).await?;
    let record = state
        .server_store
        .get_connector_instance(tenant.tenant(), &instance_id)
        .await
        .map_err(internal_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!("connector instance `{instance_id}` not found"))
        })?;
    let catalog = record.catalog.as_ref().ok_or_else(|| {
        ApiError::not_found(format!(
            "connector instance `{instance_id}` has served no catalog yet; connect it first"
        ))
    })?;
    if let Some(pinned) = query.generation {
        if pinned != catalog.generation {
            return Err(ApiError::conflict(format!(
                "catalog generation pin {pinned} does not match the live generation {}",
                catalog.generation
            )));
        }
    }
    Ok(Json(json!({
        "catalog": {
            "instance_id": record.instance_id,
            "generation": catalog.generation,
            "hash": catalog.hash,
            "tools": catalog.tools,
        }
    })))
}

/// `POST /connectors/instances/{id}/health` — an on-demand health check
/// against the live session. `409` when the instance is in a state health
/// checks do not apply to (`pending`, `connecting`, `failed`, `disabled`).
pub(crate) async fn check_instance_health(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    plane.owned_instance(&tenant, &instance_id).await?;
    let outcome = {
        let mut registry = plane.registry.lock().await;
        registry
            .check_health(&instance_id, now_ms())
            .await
            .map_err(|e| ApiError::conflict(e.to_string()))?
    };
    let record = plane.sync_record(tenant.tenant(), &instance_id).await?;
    Ok(Json(json!({
        "outcome": sweep_outcome_view(&outcome),
        "instance": instance_view(&record),
    })))
}

/// `POST /connectors/sweep` — re-check every `healthy`/`degraded`
/// instance and answer with the caller's tenant's outcomes (the sweep
/// itself is plane-wide: health is a global property, visibility is not).
pub(crate) async fn sweep_connectors(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    let outcomes = {
        let mut registry = plane.registry.lock().await;
        registry.health_sweep(now_ms()).await
    };
    let mut views = Vec::new();
    for outcome in &outcomes {
        let owned = {
            let registry = plane.registry.lock().await;
            registry
                .instance(&outcome.instance_id)
                .is_some_and(|instance| instance.tenant_id == tenant.tenant())
        };
        if owned {
            plane
                .sync_record(tenant.tenant(), &outcome.instance_id)
                .await?;
            views.push(sweep_outcome_view(outcome));
        }
    }
    Ok(Json(json!({ "outcomes": views })))
}

/// `POST /connectors/instances/{id}/disable` — shut the session down and
/// park the instance. `409` when already disabled.
pub(crate) async fn disable_instance(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    plane.owned_instance(&tenant, &instance_id).await?;
    {
        let mut registry = plane.registry.lock().await;
        registry
            .disable(&instance_id)
            .await
            .map_err(|e| ApiError::conflict(e.to_string()))?;
    }
    let record = plane.sync_record(tenant.tenant(), &instance_id).await?;
    Ok(Json(json!({ "instance": instance_view(&record) })))
}

/// `POST /connectors/instances/{id}/enable` — return a disabled instance
/// to `pending`; it reconnects through the usual path. `409` when the
/// instance is not disabled.
pub(crate) async fn enable_instance(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let plane = plane(&state);
    plane.ensure_restored().await?;
    plane.owned_instance(&tenant, &instance_id).await?;
    {
        let mut registry = plane.registry.lock().await;
        registry
            .enable(&instance_id)
            .map_err(|e| ApiError::conflict(e.to_string()))?;
    }
    let record = plane.sync_record(tenant.tenant(), &instance_id).await?;
    Ok(Json(json!({ "instance": instance_view(&record) })))
}

/// Tests for the pure halves (record folding, file layout); the HTTP
/// surface is covered in `tests/connectors.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    fn capability(name: &str) -> ToolCapability {
        ToolCapability {
            name: name.to_owned(),
            description: format!("The {name} tool."),
            parameters_schema: json!({"type": "object"}),
            effect: rusty_agent_runtime::record::Effect::NonIdempotent,
        }
    }

    fn record() -> ConnectorInstanceRecord {
        let now = Utc::now();
        ConnectorInstanceRecord {
            instance_id: "inst-000001".to_owned(),
            connector_id: "test-conn".to_owned(),
            manifest_hash: "ab".repeat(32),
            credential_connections: BTreeMap::new(),
            state: "healthy".to_owned(),
            state_reason: None,
            consecutive_failures: 0,
            last_health_check_ms: Some(1_000),
            catalog: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn served_catalog_advances_only_on_change() {
        let mut record = record();
        fold_served_catalog(&mut record, "hash-a", vec![capability("test-conn/a")]);
        assert_eq!(record.catalog.as_ref().unwrap().generation, 1);
        fold_served_catalog(&mut record, "hash-a", vec![capability("test-conn/a")]);
        assert_eq!(record.catalog.as_ref().unwrap().generation, 1);
        fold_served_catalog(&mut record, "hash-b", vec![capability("test-conn/b")]);
        let catalog = record.catalog.as_ref().unwrap();
        assert_eq!(catalog.generation, 2);
        assert_eq!(catalog.tools[0].name, "test-conn/b");
    }

    #[tokio::test]
    async fn file_layout_round_trips_scoped_records() {
        let root = std::env::temp_dir().join(format!("rusty-connectors-test-{}", uuid::Uuid::new_v4()));
        let manifest = ConnectorManifest::new(
            "test-conn",
            "1.0.0",
            "Test",
            "A test connector.",
            rusty_agent_runtime::connector::ProviderKind::HttpSearch(
                rusty_agent_runtime::connector::HttpSearchSpec {
                    base_url: "https://search.example.com".to_owned(),
                    auth: None,
                },
            ),
            vec![],
            vec![],
        )
        .unwrap();
        persist_manifest(&root, &manifest.hash, &manifest).await.unwrap();
        persist_manifest(&root, &format!("acme/{}", manifest.hash), &manifest)
            .await
            .unwrap();
        persist_instance(&root, "acme/inst-000001", &record())
            .await
            .unwrap();

        let manifests = load_manifests(&root);
        assert_eq!(manifests.len(), 2);
        assert!(manifests.contains_key(&manifest.hash));
        assert!(manifests.contains_key(&format!("acme/{}", manifest.hash)));
        let instances = load_instances(&root);
        assert_eq!(instances.len(), 1);
        assert_eq!(instances["acme/inst-000001"].instance_id, "inst-000001");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A canned loopback HTTP server: answers exactly one request with
    /// `status` and `body`, then yields the request bytes it saw.
    async fn canned_server(
        status: &str,
        body: Vec<u8>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_owned();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            // Headers end at the first blank line; the request is whole
            // once the declared content-length of body bytes followed.
            let total_len = |bytes: &[u8]| -> Option<usize> {
                let text = String::from_utf8_lossy(bytes);
                let split = text.find("\r\n\r\n")?;
                let length: usize = text[..split].lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        value.trim().parse().ok()
                    } else {
                        None
                    }
                })?;
                Some(split + 4 + length)
            };
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..n]);
                if total_len(&received).is_some_and(|total| received.len() >= total) {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            // The ceiling test's client errors mid-body and hangs up; a
            // failed write here is the point, not a flake.
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            String::from_utf8_lossy(&received).into_owned()
        });
        (addr, server)
    }

    fn http_request(addr: std::net::SocketAddr) -> rusty_agent_runtime::connector::HttpRequest {
        rusty_agent_runtime::connector::HttpRequest {
            url: format!("http://{addr}/search"),
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-test-marker".to_owned(), "yes".to_owned()),
            ],
            body: br#"{"query":"rust"}"#.to_vec(),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn reqwest_transport_round_trips_a_bounded_exchange() {
        let body =
            br#"{"results":[{"title":"t","url":"https://example.test","snippet":"s"}]}"#.to_vec();
        let (addr, server) = canned_server("200 OK", body.clone()).await;
        let reply = ReqwestTransport::new()
            .post(http_request(addr))
            .await
            .expect("post");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, body);
        let seen = server.await.unwrap();
        assert!(
            seen.starts_with("POST /search HTTP/1.1"),
            "request line: {seen}"
        );
        assert!(seen.contains("x-test-marker: yes"), "headers: {seen}");
        assert!(seen.contains(r#"{"query":"rust"}"#), "body: {seen}");
    }

    #[tokio::test]
    async fn reqwest_transport_trips_the_ceiling_during_the_read() {
        let oversized = vec![b'x'; MAX_SEARCH_RESPONSE_BYTES + 4096];
        let (addr, server) = canned_server("200 OK", oversized).await;
        let err = ReqwestTransport::new()
            .post(http_request(addr))
            .await
            .expect_err("an over-ceiling body is refused mid-stream");
        assert!(
            err.to_string().contains("byte ceiling"),
            "error names the ceiling: {err}"
        );
        let _ = server.await;
    }
}
