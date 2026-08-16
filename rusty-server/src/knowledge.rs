//! The knowledge plane's server half (capability-harness slice #4): the
//! file layout behind the persistence section in `server_store.rs`, the
//! adapter that serves core's [`ContentAddressedStore`] contract per
//! tenant, the `/knowledge/*` HTTP surface, and the governed
//! `search_knowledge` tool adapter.
//!
//! Layout under `{store_path}/knowledge/` (the assistants/memory
//! conventions exactly — one JSON file per record, tenant subdirectories
//! for named tenants, atomic temp-file-plus-rename writes, corrupt-tolerant
//! boot loads):
//!
//! ```text
//! knowledge/
//!   sources/{scoped_hash}.json      KnowledgeSource records (one per version)
//!   chunks/{scoped_hash}.json       the version's ChunkRecord list
//!   content/{scoped_hash}           content-addressed bytes (no extension)
//!   tombstones/{scoped_source_id}.json   SourceTombstone receipts
//! ```
//!
//! `scoped_*` keys are tenant-scoped (`{tenant}/{id}` for named tenants,
//! bare for the default tenant — [`crate::auth::scope_id`]), so the plane
//! is tenant-isolated at the storage layer: cross-tenant reads are
//! indistinguishable from absence, and the HTTP surface answers them `404`
//! — never `403`.
//!
//! The plane is file-backed in this slice on every deployment (a Postgres
//! backend is a later concern behind the same core trait); the boot path
//! reloads every index from disk, so the [`KnowledgeBase`] — stateless
//! over the store — is whole again after a restart by construction.
//!
//! **Journaled-query deferral.** Core's query is deterministic (injected
//! clocks, total-order ranking) and ready for a `RunEventKind` knowledge
//! read, the memory plane's journaled seam. That event kind lives in
//! `rusty-core/src/record.rs`, outside this slice's file scope, so the
//! server hook lands with the core contract — noted, not built.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, Query, State as AxumState};
use axum::http::StatusCode;
use axum::Json;
use axum::Extension;
use chrono::{DateTime, Utc};
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::knowledge::{
    Citation, ChunkRecord, ContentAddressedStore, KnowledgeBase, KnowledgeSource, QueryLimits,
    RetentionPolicy, SourceKind, SourceRegistration, SourceTombstone,
};
use rusty_agent_runtime::memory::{MemoryScope, ScopeAddress};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::builtins::{KnowledgeDocument, KnowledgeSearchTool};
use rusty_agent_runtime::tool::Tool;
use rusty_agent_runtime::error::RustyError;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::server_store::KnowledgePlane;
use crate::tasks;

fn invalid(message: impl Into<String>) -> RustyError {
    // The adapter crosses from the store's stringly errors into core's
    // taxonomy; contract-shaped failures keep the invalid-update class the
    // core module itself uses.
    RustyError::InvalidUpdate(message.into())
}

// --------------------------------------------------------------------- //
// File layout and IO (the server_store persistence section's helpers)
// --------------------------------------------------------------------- //

/// The knowledge directory under the store root. `knowledge` is a
/// reserved layout name (see [`crate::RESERVED_NAMES`]): client-chosen
/// thread ids may not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("knowledge")
}

fn sources_dir(root: &Path) -> PathBuf {
    dir(root).join("sources")
}

fn chunks_dir(root: &Path) -> PathBuf {
    dir(root).join("chunks")
}

fn content_dir(root: &Path) -> PathBuf {
    dir(root).join("content")
}

fn tombstones_dir(root: &Path) -> PathBuf {
    dir(root).join("tombstones")
}

/// Persist one JSON record atomically (temp file + rename — the
/// `agents::persist_record` discipline: a crash mid-write must never leave
/// a truncated record behind). The scoped id may carry a `{tenant}/`
/// prefix, so the parent directory is created, not just the flat dir.
async fn persist_json(
    dir: &Path,
    scoped_id: &str,
    record: &impl serde::Serialize,
) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| format!("serialize {scoped_id}: {e}"))?;
    let path = dir.join(format!("{scoped_id}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = dir.join(format!("{scoped_id}.tmp"));
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| format!("rename {}: {e}", path.display()))
}

/// Remove one record file; `false` when already gone — a missing file is
/// not an error, because the in-memory index is the authority on whether
/// the record was held at all (the memory plane's removal rule).
async fn remove_json(dir: &Path, scoped_id: &str) -> Result<bool, String> {
    let path = dir.join(format!("{scoped_id}.json"));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// Recursively collect files under `root`, mirroring the memory loader.
fn collect_files(root: &Path, extension: Option<&str>, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, extension, out);
        } else if extension.is_none() || path.extension().and_then(|e| e.to_str()) == extension {
            out.push(path);
        }
    }
}

/// The path-derived scoped id of a record file: `{tenant}/{name}` for
/// named tenants, the bare name for the default tenant — the record body
/// carries the bare id, so the key comes from where the file lives (the
/// memory loader's rule).
fn scoped_key(base: &Path, path: &Path, extension: &str) -> Option<String> {
    path.strip_prefix(base)
        .ok()
        .map(|relative| {
            let relative = relative.with_extension("");
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|key| !key.ends_with(extension))
}

pub(crate) async fn persist_source(
    root: &Path,
    scoped: &str,
    source: &KnowledgeSource,
) -> Result<(), String> {
    persist_json(&sources_dir(root), scoped, source).await
}

pub(crate) async fn remove_source(root: &Path, scoped: &str) -> Result<bool, String> {
    remove_json(&sources_dir(root), scoped).await
}

pub(crate) async fn persist_chunks(
    root: &Path,
    scoped: &str,
    chunks: &[ChunkRecord],
) -> Result<(), String> {
    persist_json(&chunks_dir(root), scoped, &chunks).await
}

pub(crate) async fn remove_chunks(root: &Path, scoped: &str) -> Result<bool, String> {
    remove_json(&chunks_dir(root), scoped).await
}

pub(crate) async fn persist_tombstone(
    root: &Path,
    scoped: &str,
    tombstone: &SourceTombstone,
) -> Result<(), String> {
    persist_json(&tombstones_dir(root), scoped, tombstone).await
}

/// Persist content-addressed bytes atomically under `content/`. Raw blob,
/// no extension — the record loaders above never mistake a blob for a
/// record (the `memory_artifacts` sibling discipline, kept one level down:
/// each has its own subdirectory here).
pub(crate) async fn persist_content(
    root: &Path,
    scoped: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let dir = content_dir(root);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(scoped);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = dir.join(format!("{scoped}.tmp"));
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| format!("rename {}: {e}", path.display()))
}

pub(crate) async fn remove_content(root: &Path, scoped: &str) -> Result<bool, String> {
    let path = content_dir(root).join(scoped);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// Boot-load the source-version index, path-keyed and corrupt-tolerant
/// (one bad file must not take the namespace down).
pub(crate) fn load_sources(root: &Path) -> Vec<(String, KnowledgeSource)> {
    let dir = sources_dir(root);
    let mut files = Vec::new();
    collect_files(&dir, Some("json"), &mut files);
    let mut out = Vec::new();
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<KnowledgeSource>(&raw).ok());
        match (scoped_key(&dir, &path, "json"), parsed) {
            (Some(key), Some(record)) => out.push((key, record)),
            _ => tracing::warn!(path = %path.display(), "skipping unreadable knowledge source file"),
        }
    }
    out
}

/// Boot-load the chunk indexes (one file per source version).
pub(crate) fn load_chunks(root: &Path) -> Vec<(String, Vec<ChunkRecord>)> {
    let dir = chunks_dir(root);
    let mut files = Vec::new();
    collect_files(&dir, Some("json"), &mut files);
    let mut out = Vec::new();
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<ChunkRecord>>(&raw).ok());
        match (scoped_key(&dir, &path, "json"), parsed) {
            (Some(key), Some(chunks)) => out.push((key, chunks)),
            _ => tracing::warn!(path = %path.display(), "skipping unreadable knowledge chunks file"),
        }
    }
    out
}

/// Boot-load the tombstone index, keyed by scoped source id.
pub(crate) fn load_tombstones(root: &Path) -> Vec<(String, SourceTombstone)> {
    let dir = tombstones_dir(root);
    let mut files = Vec::new();
    collect_files(&dir, Some("json"), &mut files);
    let mut out = Vec::new();
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<SourceTombstone>(&raw).ok());
        match (scoped_key(&dir, &path, "json"), parsed) {
            (Some(key), Some(tombstone)) => out.push((key, tombstone)),
            _ => tracing::warn!(path = %path.display(), "skipping unreadable knowledge tombstone"),
        }
    }
    out
}

/// Boot-load the content blobs into the in-memory index (the server's
/// index-everything convention; blobs are raw files without extension).
pub(crate) fn load_content(root: &Path) -> Vec<(String, Vec<u8>)> {
    let dir = content_dir(root);
    let mut files = Vec::new();
    collect_files(&dir, None, &mut files);
    let mut out = Vec::new();
    for path in files {
        // `.tmp` files are interrupted writes — never load them.
        if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
            continue;
        }
        let key = path.strip_prefix(&dir).ok().map(|relative| {
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        });
        match (key, std::fs::read(&path)) {
            (Some(key), Ok(bytes)) => out.push((key, bytes)),
            _ => tracing::warn!(path = %path.display(), "skipping unreadable knowledge content blob"),
        }
    }
    out
}

// --------------------------------------------------------------------- //
// The per-tenant core-store adapter
// --------------------------------------------------------------------- //

/// The tenant's knowledge namespace as a core [`ContentAddressedStore`] —
/// the adapter [`KnowledgeBase`] queries through, exactly the role
/// `ServerMemoryStore` plays for the memory plane. Every operation is one
/// [`KnowledgePlane`] call under this adapter's tenant, so a tenant sees
/// exactly its own namespace and nothing else.
pub(crate) struct ServerKnowledgeStore {
    plane: Arc<KnowledgePlane>,
    tenant: String,
}

// Manual: the plane handle carries no debug-relevant state the tenant does
// not already identify (the `ServerMemoryStore` precedent).
impl std::fmt::Debug for ServerKnowledgeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerKnowledgeStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl ServerKnowledgeStore {
    /// Adapt `plane` for `tenant`'s namespace.
    pub(crate) fn new(plane: Arc<KnowledgePlane>, tenant: impl Into<String>) -> Self {
        Self {
            plane,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl ContentAddressedStore for ServerKnowledgeStore {
    async fn put_content(&self, address: &str, bytes: &[u8]) -> RuntimeResult<bool> {
        self.plane
            .put_content(&self.tenant, address, bytes)
            .await
            .map_err(invalid)
    }

    async fn get_content(&self, address: &str) -> RuntimeResult<Option<Vec<u8>>> {
        self.plane
            .get_content(&self.tenant, address)
            .await
            .map_err(invalid)
    }

    async fn remove_content(&self, address: &str) -> RuntimeResult<bool> {
        self.plane
            .remove_content(&self.tenant, address)
            .await
            .map_err(invalid)
    }

    async fn put_source(&self, source: &KnowledgeSource) -> RuntimeResult<bool> {
        self.plane
            .put_source(&self.tenant, source)
            .await
            .map_err(invalid)
    }

    async fn get_source(&self, content_hash: &str) -> RuntimeResult<Option<KnowledgeSource>> {
        self.plane
            .get_source(&self.tenant, content_hash)
            .await
            .map_err(invalid)
    }

    async fn all_sources(&self) -> RuntimeResult<Vec<KnowledgeSource>> {
        self.plane.all_sources(&self.tenant).await.map_err(invalid)
    }

    async fn remove_source(&self, content_hash: &str) -> RuntimeResult<bool> {
        self.plane
            .remove_source(&self.tenant, content_hash)
            .await
            .map_err(invalid)
    }

    async fn put_chunks(&self, chunks: &[ChunkRecord]) -> RuntimeResult<()> {
        self.plane
            .put_chunks(&self.tenant, chunks)
            .await
            .map_err(invalid)
    }

    async fn chunks_of(&self, source_hash: &str) -> RuntimeResult<Vec<ChunkRecord>> {
        self.plane
            .chunks_of(&self.tenant, source_hash)
            .await
            .map_err(invalid)
    }

    async fn remove_chunks(&self, source_hash: &str) -> RuntimeResult<bool> {
        self.plane
            .remove_chunks(&self.tenant, source_hash)
            .await
            .map_err(invalid)
    }

    async fn source_of_chunk(&self, content_address: &str) -> RuntimeResult<Option<String>> {
        self.plane
            .source_of_chunk(&self.tenant, content_address)
            .await
            .map_err(invalid)
    }

    async fn put_tombstone(&self, tombstone: &SourceTombstone) -> RuntimeResult<()> {
        self.plane
            .put_tombstone(&self.tenant, tombstone)
            .await
            .map_err(invalid)
    }

    async fn tombstone_for(&self, source_id: &str) -> RuntimeResult<Option<SourceTombstone>> {
        self.plane
            .tombstone_for(&self.tenant, source_id)
            .await
            .map_err(invalid)
    }

    async fn all_tombstones(&self) -> RuntimeResult<Vec<SourceTombstone>> {
        self.plane.all_tombstones(&self.tenant).await.map_err(invalid)
    }
}

/// The tenant's [`KnowledgeBase`] — stateless over the store, so it is
/// constructed per request; the boot-rebuilt plane underneath carries all
/// state.
fn knowledge_base(state: &AppState, tenant: &TenantContext) -> KnowledgeBase {
    KnowledgeBase::new(Arc::new(ServerKnowledgeStore::new(
        Arc::clone(&state.knowledge),
        tenant.tenant(),
    )))
}

// --------------------------------------------------------------------- //
// The HTTP surface
// --------------------------------------------------------------------- //

/// The scope a knowledge write or read addresses. Default: the caller's
/// tenant scope — knowledge is a tenant asset. `run` scope is rejected
/// (`400` — the runtime has no knowledge writes on a run's behalf);
/// `tenant` scope must name the caller's own tenant, and a mismatch is
/// `404`, never `403` (cross-tenant is indistinguishable from absence by
/// design); `agent` / `team` / `user` scopes ride tenant namespacing
/// unchanged.
fn check_knowledge_scope_gate(tenant: &TenantContext, scope: &ScopeAddress) -> Result<(), ApiError> {
    tasks::validate_label("scope.id", &scope.id, 256).map_err(ApiError::bad_request)?;
    match scope.scope {
        MemoryScope::Run => Err(ApiError::bad_request(
            "`run`-scoped knowledge does not exist: sources outlive runs by design — the API \
             accepts `agent`, `team`, `user`, and `tenant` scopes"
                .to_string(),
        )),
        MemoryScope::Tenant if scope.id != tenant.tenant() => Err(ApiError::not_found(format!(
            "knowledge scope `{scope}` not found",
            scope = scope.as_address()
        ))),
        _ => Ok(()),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RegisterSourcePayload {
    /// The stable source name (correction chains share it).
    source_id: String,
    /// What the source is (text / markdown / json / csv).
    kind: SourceKind,
    /// The human-facing title citations render.
    title: String,
    /// The publisher's provenance string (`human:{id}` / `agent:{id}` /
    /// `system`). Mandatory — a source that cannot name its origin cannot
    /// be audited.
    author: String,
    /// The source body.
    body: String,
    /// The writer-declared confidence in `(0, 1]`. Defaults to `1.0` for
    /// `human:` authors (the claim is the person's, stated plainly — the
    /// memory plane's rule); required otherwise.
    #[serde(default)]
    confidence: Option<f64>,
    /// TTL or pinned (default: pinned).
    #[serde(default)]
    retention: Option<RetentionPolicy>,
    /// The scope the source lives at (default: the caller's tenant scope).
    #[serde(default)]
    scope: Option<ScopeAddress>,
}

/// `POST /knowledge/sources` — register a governed source → `201
/// {source_id, content_hash, version, chunk_count, created}`; `200` +
/// `created: false` when the body is already registered under this source
/// id (content addressing makes registration idempotent — a retried
/// submission converges). `400` on a contract violation (malformed id,
/// empty title/author/body, out-of-range confidence, oversize body, an
/// already-expired TTL, or a *different* body under an existing id — that
/// is a correction, never a silent overwrite).
pub(crate) async fn register_source(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<RegisterSourcePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let scope = payload
        .scope
        .unwrap_or_else(|| ScopeAddress::new(MemoryScope::Tenant, tenant.tenant()));
    check_knowledge_scope_gate(&tenant, &scope)?;
    let confidence = match payload.confidence {
        Some(confidence) => confidence,
        None if payload.author.starts_with("human:") => 1.0,
        None => {
            return Err(ApiError::bad_request(
                "`confidence` is required for non-human authors — `human:` authors default to \
                 1.0 (the claim is the person's); every other author must declare its confidence \
                 explicitly"
                    .to_string(),
            ));
        }
    };
    let registration = SourceRegistration {
        source_id: payload.source_id,
        scope,
        kind: payload.kind,
        title: payload.title,
        author: payload.author,
        confidence,
        retention: payload.retention.unwrap_or(RetentionPolicy::Pinned),
    };
    let base = knowledge_base(&state, &tenant);
    let prior = base
        .versions_of(&registration.source_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let source = base
        .register_source(registration, &payload.body, Utc::now())
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let created = !prior
        .iter()
        .any(|version| version.content_hash == source.content_hash);
    let chunk_count = state
        .knowledge
        .chunks_of(tenant.tenant(), &source.content_hash)
        .await
        .map_err(ApiError::internal)?
        .len();
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({
            "source_id": source.source_id,
            "content_hash": source.content_hash,
            "version": source.version,
            "chunk_count": chunk_count,
            "created": created,
        })),
    ))
}

/// `GET /knowledge/sources` — the tenant's sources, metadata only (bodies
/// never cross a listing — a listing is an audit view): the latest version
/// of each source with its chunk count, sorted by source id, plus the
/// tenant's tombstones (sorted) so purged citations stay resolvable from
/// the listing too.
pub(crate) async fn list_sources(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let sources = state
        .knowledge
        .all_sources(tenant.tenant())
        .await
        .map_err(ApiError::internal)?;
    // Latest version per source id; the map sorts by source id.
    let mut latest: std::collections::BTreeMap<String, KnowledgeSource> = Default::default();
    for source in sources {
        latest
            .entry(source.source_id.clone())
            .and_modify(|held| {
                if source.version > held.version {
                    *held = source.clone();
                }
            })
            .or_insert(source);
    }
    let mut listed = Vec::new();
    for source in latest.into_values() {
        let chunk_count = state
            .knowledge
            .chunks_of(tenant.tenant(), &source.content_hash)
            .await
            .map_err(ApiError::internal)?
            .len();
        listed.push(json!({
            "source_id": source.source_id,
            "scope": source.scope,
            "kind": source.kind,
            "title": source.title,
            "author": source.author,
            "confidence": source.confidence,
            "created_at": source.created_at,
            "retention": source.retention,
            "content_hash": source.content_hash,
            "content_bytes": source.content_bytes,
            "version": source.version,
            "supersedes": source.supersedes,
            "chunk_count": chunk_count,
        }));
    }
    let mut tombstones = state
        .knowledge
        .all_tombstones(tenant.tenant())
        .await
        .map_err(ApiError::internal)?;
    tombstones.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    Ok(Json(json!({
        "sources": listed,
        "tombstones": tombstones,
    })))
}

/// `GET /knowledge/sources/{source_id}` — one source's latest metadata plus
/// its chunk inventory (chunk records, no text). `404` unknown/cross-tenant;
/// a purged source answers `200 {"tombstone": …}` — the tombstone is the
/// metadata old citations resolve to.
pub(crate) async fn get_source(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(source_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let base = knowledge_base(&state, &tenant);
    let versions = base
        .versions_of(&source_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let Some(latest) = versions.last() else {
        if let Some(tombstone) = base
            .tombstone(&source_id)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
        {
            return Ok(Json(json!({ "tombstone": tombstone })));
        }
        return Err(ApiError::not_found(format!(
            "knowledge source `{source_id}` not found"
        )));
    };
    let chunks = state
        .knowledge
        .chunks_of(tenant.tenant(), &latest.content_hash)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "source": latest,
        "versions": versions.len(),
        "chunks": chunks,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkQuery {
    /// Pin the fetch to one version's content hash — the evidence path a
    /// citation in an old journal walks (default: the latest version).
    #[serde(default)]
    version: Option<String>,
}

/// `GET /knowledge/sources/{source_id}/chunks/{chunk_id}` — fetch one chunk
/// with its citation. `chunk_id` accepts the bare index (`3`) or the full
/// id (`doc#3`, URL-encoded); `?version={content_hash}` pins a superseded
/// version for evidence (superseded chunks stay addressable). `404`
/// unknown source, version, chunk, or cross-tenant.
pub(crate) async fn get_chunk(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath((source_id, chunk_id)): AxumPath<(String, String)>,
    Query(query): Query<ChunkQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = knowledge_base(&state, &tenant);
    let versions = base
        .versions_of(&source_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let version = match &query.version {
        Some(hash) => versions
            .iter()
            .find(|version| &version.content_hash == hash)
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "knowledge source `{source_id}` has no version `{hash}`"
                ))
            })?,
        None => versions.last().ok_or_else(|| {
            ApiError::not_found(format!("knowledge source `{source_id}` not found"))
        })?,
    };
    let chunks = state
        .knowledge
        .chunks_of(tenant.tenant(), &version.content_hash)
        .await
        .map_err(ApiError::internal)?;
    let chunk = match chunk_id.parse::<u32>() {
        Ok(index) => chunks.iter().find(|chunk| chunk.chunk_index == index),
        Err(_) => chunks.iter().find(|chunk| chunk.chunk_id == chunk_id),
    }
    .ok_or_else(|| {
        ApiError::not_found(format!(
            "knowledge source `{source_id}` has no chunk `{chunk_id}`"
        ))
    })?;
    let text = base
        .chunk_content(&chunk.content_address)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| {
            ApiError::internal(format!(
                "chunk {} of knowledge source `{source_id}` is indexed but its content is \
                 missing — the index and the content store disagree",
                chunk.chunk_id
            ))
        })?;
    Ok(Json(json!({
        "citation": Citation {
            source_id: chunk.source_id.clone(),
            source_hash: chunk.source_hash.clone(),
            title: version.title.clone(),
            chunk_id: chunk.chunk_id.clone(),
            chunk_index: chunk.chunk_index,
            content_address: chunk.content_address.clone(),
            byte_start: chunk.byte_start,
            byte_end: chunk.byte_end,
        },
        "text": text,
        "word_count": chunk.word_count,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CorrectSourcePayload {
    /// The corrector's provenance string. Mandatory — a correction that
    /// cannot name its corrector is indistinguishable from a rewrite.
    author: String,
    /// The corrected body (the new version's full content).
    body: String,
}

/// `POST /knowledge/sources/{source_id}/correct` — mint the superseding
/// version → `201 {source_id, content_hash, version, supersedes,
/// chunk_count}`. Retrieval stops serving the old version immediately; the
/// old version stays addressable (`?version=` on the chunk fetch, and the
/// evidence half of the store). `404` unknown/purged/cross-tenant source;
/// `400` on a contract violation (including a byte-identical "correction").
pub(crate) async fn correct_source(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(source_id): AxumPath<String>,
    Json(payload): Json<CorrectSourcePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let base = knowledge_base(&state, &tenant);
    if base
        .versions_of(&source_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .is_empty()
    {
        return Err(ApiError::not_found(format!(
            "knowledge source `{source_id}` not found"
        )));
    }
    let source = base
        .correct_source(&source_id, &payload.author, &payload.body, Utc::now())
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let chunk_count = state
        .knowledge
        .chunks_of(tenant.tenant(), &source.content_hash)
        .await
        .map_err(ApiError::internal)?
        .len();
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "source_id": source.source_id,
            "content_hash": source.content_hash,
            "version": source.version,
            "supersedes": source.supersedes,
            "chunk_count": chunk_count,
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryKnowledgePayload {
    /// The query text.
    text: String,
    /// Result ceilings (default: core's [`QueryLimits::default`]).
    #[serde(default)]
    limits: Option<QueryLimits>,
    /// The scope to search (default: the caller's tenant scope). Cross-
    /// tenant scopes are answered `404` by the gate; narrower scopes
    /// (`agent` / `team` / `user`) filter within the tenant.
    #[serde(default)]
    scope: Option<ScopeAddress>,
}

/// `POST /knowledge/query` — hybrid cited retrieval over the tenant's live
/// (unexpired, unsuperseded) sources → `200 {query, results}` where every
/// result is a `CitedChunk`: text *with* its citation, never bare text.
/// `400` on invalid limits, a termless query, or a rejected scope.
pub(crate) async fn query_knowledge(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<QueryKnowledgePayload>,
) -> Result<Json<Value>, ApiError> {
    let scope = payload
        .scope
        .unwrap_or_else(|| ScopeAddress::new(MemoryScope::Tenant, tenant.tenant()));
    check_knowledge_scope_gate(&tenant, &scope)?;
    let results = knowledge_base(&state, &tenant)
        .query(
            &scope,
            &payload.text,
            &payload.limits.unwrap_or_default(),
            Utc::now(),
        )
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({
        "query": payload.text,
        "results": results,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RetentionPayload {
    /// The instant the sweep evaluates expiry against (default: now). An
    /// explicit `as_of` is the operator-declared sweep instant — the
    /// injected clock at the HTTP boundary, mirroring core's determinism
    /// contract; the tombstones record it as `purged_at`.
    #[serde(default)]
    as_of: Option<DateTime<Utc>>,
}

/// `POST /knowledge/retention/plan` — the dry-run: what a sweep at `as_of`
/// (default now) *would* purge, computed before anything is deleted.
///
/// The server has no admin-role convention (tenant API keys are the whole
/// authorization model), so retention rides the tenant boundary the way
/// `/memory/forget` does: the tenant's own key is the operator gate.
pub(crate) async fn retention_plan(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<RetentionPayload>,
) -> Result<Json<Value>, ApiError> {
    let plan = knowledge_base(&state, &tenant)
        .plan_sweep(payload.as_of.unwrap_or_else(Utc::now))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    serde_json::to_value(&plan)
        .map(Json)
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// `POST /knowledge/retention/apply` — execute the plan exactly: chunks,
/// bodies, and source records are removed (an address dies with its last
/// reference), and every purged source id leaves a metadata-only
/// tombstone so citations in old journals stay resolvable. Same operator
/// gate as the plan (see its docs).
pub(crate) async fn retention_apply(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<RetentionPayload>,
) -> Result<Json<Value>, ApiError> {
    let receipt = knowledge_base(&state, &tenant)
        .apply_sweep(payload.as_of.unwrap_or_else(Utc::now))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    serde_json::to_value(&receipt)
        .map(Json)
        .map_err(|e| ApiError::internal(e.to_string()))
}

// --------------------------------------------------------------------- //
// The governed `search_knowledge` tool adapter
// --------------------------------------------------------------------- //

/// A `search_knowledge` tool backed by the governed knowledge plane when
/// one is configured, falling back to the built-in in-memory
/// [`KnowledgeSearchTool`] otherwise — the same name, schema, and effect
/// class as the built-in, so a graph registering it upgrades its backend
/// without a catalog change.
///
/// Results carry the governed shape: the built-in's `{id, title, score,
/// excerpt}` plus the full [`Citation`], so an agent quoting a result can
/// attribute it exactly. Embedders (the demo server, downstream
/// harnesses) construct it with the tenant's [`KnowledgeBase`] and the
/// scope the tool searches — the tool contract carries no tenant, so the
/// scope is fixed at registration, per graph.
pub struct GovernedKnowledgeSearchTool {
    base: Option<KnowledgeBase>,
    scope: Option<ScopeAddress>,
    fallback: KnowledgeSearchTool,
}

impl std::fmt::Debug for GovernedKnowledgeSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GovernedKnowledgeSearchTool")
            .field("governed", &self.base.is_some())
            .field("scope", &self.scope.as_ref().map(ScopeAddress::as_address))
            .finish_non_exhaustive()
    }
}

impl GovernedKnowledgeSearchTool {
    /// The governed configuration: query `base` at `scope`. `fallback` is
    /// the in-memory collection the tool serves when no base is configured
    /// ([`GovernedKnowledgeSearchTool::in_memory`]); with a base set, the
    /// plane is the answer — an empty governed result is the truth of the
    /// tenant's knowledge, not a reason to consult the fallback.
    pub fn governed(base: KnowledgeBase, scope: ScopeAddress, fallback: Vec<KnowledgeDocument>) -> rusty_agent_runtime::error::Result<Self> {
        Ok(Self {
            base: Some(base),
            scope: Some(scope),
            fallback: KnowledgeSearchTool::new(fallback)?,
        })
    }

    /// The in-memory configuration: exactly the built-in tool's behavior
    /// (the constructor an embedder uses before the plane is wired).
    pub fn in_memory(documents: Vec<KnowledgeDocument>) -> rusty_agent_runtime::error::Result<Self> {
        Ok(Self {
            base: None,
            scope: None,
            fallback: KnowledgeSearchTool::new(documents)?,
        })
    }
}

#[async_trait]
impl Tool for GovernedKnowledgeSearchTool {
    fn name(&self) -> &str {
        self.fallback.name()
    }

    fn description(&self) -> &str {
        self.fallback.description()
    }

    fn parameters_schema(&self) -> Value {
        self.fallback.parameters_schema()
    }

    fn effect(&self) -> Effect {
        self.fallback.effect()
    }

    async fn call(&self, args: Value) -> RuntimeResult<Value> {
        let Some(base) = &self.base else {
            return self.fallback.call(args).await;
        };
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| RustyError::Tool("`query` must be a string".into()))?
            .trim();
        if query.is_empty() || query.len() > rusty_agent_runtime::tool::builtins::MAX_SEARCH_QUERY_BYTES
        {
            return Err(RustyError::Tool(format!(
                "search query must contain 1..={} bytes",
                rusty_agent_runtime::tool::builtins::MAX_SEARCH_QUERY_BYTES
            )));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, rusty_agent_runtime::tool::builtins::MAX_SEARCH_RESULTS as u64) as usize;
        let limits = QueryLimits {
            max_results: limit,
            ..QueryLimits::default()
        };
        let scope = self
            .scope
            .clone()
            .expect("a governed tool carries its scope");
        let results = base
            .query(&scope, query, &limits, Utc::now())
            .await
            .map_err(|e| RustyError::Tool(format!("knowledge query failed: {e}")))?;
        let rendered: Vec<Value> = results
            .iter()
            .map(|result| {
                let mut excerpt = result.text.chars().take(480).collect::<String>();
                if result.text.chars().count() > 480 {
                    excerpt.push('…');
                }
                json!({
                    "id": result.citation.chunk_id,
                    "title": result.citation.title,
                    "score": result.score,
                    "excerpt": excerpt,
                    "citation": result.citation,
                })
            })
            .collect();
        Ok(json!({"query": query, "results": rendered}))
    }
}
