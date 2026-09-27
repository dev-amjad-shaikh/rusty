//! The skill plane's server half: durable, tenant-scoped storage for
//! governed `SKILL.md` packages plus the `/skills` HTTP surface.
//!
//! Core (`rusty_agent_runtime::skill`) owns the package contract — parsing,
//! the security scan, provenance, and immutable content-addressed versions.
//! This module owns what the server adds:
//!
//! - **Persistence.** One JSON file per registered version under
//!   `{store_path}/skills/`, path-keyed by tenancy exactly like memory:
//!   `skills/{name}/{revision:06}.json` for the default tenant,
//!   `skills/{tenant}/{name}/{revision:06}.json` for named tenants. Writes
//!   are atomic (temp file + rename — the `agents::persist_record`
//!   discipline): a crash mid-write never leaves a truncated version
//!   behind. The plane is file-backed under the store root regardless of
//!   the `ServerStore` backend — the receipt-keyring precedent
//!   (`{store_path}/keys/` lives on disk on Postgres deployments too) —
//!   because a `server_skills` table migration is only honest where it can
//!   be run, and this slice cannot run one.
//! - **Boot reload.** [`SkillPlane::load`] rebuilds every tenant's
//!   [`SkillRegistry`] from the file set: each stored version is
//!   re-parsed into a package and its recomputed content hash must match
//!   the recorded one (identity is integrity), then re-registered in
//!   revision order, so revisions and hashes survive restart bit-for-bit.
//!   Files that fail to parse or fail integrity are skipped with a warning
//!   (the agents loader's corrupt-tolerance rule); a skipped middle
//!   revision compacts that name's later revisions on reload, which the
//!   warning names.
//! - **Tenancy.** The plane holds one registry per tenant; handlers resolve
//!   the caller's [`TenantContext`] and only ever touch that tenant's
//!   registry, so a cross-tenant name is indistinguishable from an unknown
//!   one — the answer is `404`, never `403`.
//!
//! # The HTTP surface
//!
//! Progressive disclosure maps onto routes: `GET /skills` is the tier-1
//! metadata listing, `GET /skills/{name}/body` the tier-2 body (explicit
//! opt-in), and `GET /skills/{name}/files/{*path}` the tier-3 members.
//! Member-path hygiene is structural: the wildcard path is looked up
//! against the version's validated member maps, so a traversal string
//! simply matches nothing and answers `404` — there is no path to
//! canonicalize and escape through. `POST /skills` takes the raw
//! `SKILL.md` text (the server parses the exact bytes the author wrote,
//! never a re-serialization) plus `references` as text and `assets` as
//! hex; scan denials answer `422` with the structured findings, package
//! violations `400`, unknown names and revisions `404`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use rusty_agent_runtime::skill::{
    Registration, SkillError, SkillMetadata, SkillPackage, SkillPromotion, SkillPromotionStatus,
    SkillRegistry, SkillSource, SkillVersion, SkillVersionSelector,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::{scope_id, TenantContext, DEFAULT_TENANT};
use crate::error::ApiError;
use crate::routes::AppState;
use rusty_agent_runtime::gaps::{Citation, CitationKind, ClosureCriteria, GapOrigin, GapSubject};
use rusty_agent_runtime::repair::{
    RepairAction, RepairComponent, RepairLedger, RepairOutcome, RepairRecordBuilder, RepairRung,
    RepairTrigger,
};

/// The skills directory under the store root. `skills` is a reserved
/// layout name (see [`crate::RESERVED_NAMES`]): client-chosen tenant and
/// thread ids may not claim it.
fn dir(root: &Path) -> PathBuf {
    root.join("skills")
}

/// The file one version persists to: `skills/{scoped-name}/{revision:06}.json`,
/// the scoped name carrying the `{tenant}/` prefix for named tenants (the
/// memory layout rule — the default tenant stays unprefixed).
fn version_path(root: &Path, tenant: &str, name: &str, revision: u64) -> PathBuf {
    dir(root)
        .join(scope_id(tenant, name))
        .join(format!("{revision:06}.json"))
}

/// Persist one version atomically (temp file + rename) at its
/// revision-addressed path. Versions are immutable, so a rewrite of an
/// existing path is a byte-identical no-op by construction.
async fn persist_version(
    root: &Path,
    tenant: &str,
    version: &SkillVersion,
) -> Result<(), SkillError> {
    let path = version_path(root, tenant, version.name(), version.revision());
    let io = |message: String| SkillError::Io {
        path: path.display().to_string(),
        message,
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io(format!("create skills dir: {e}")))?;
    }
    let bytes = serde_json::to_vec_pretty(version).map_err(|e| io(format!("serialize: {e}")))?;
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| io(format!("write: {e}")))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| io(format!("rename: {e}")))
}

/// Rebuild the package a stored version was minted from. The canonical
/// hash covers parsed frontmatter values, the body, and the member bytes —
/// never the raw `SKILL.md` text — so a faithful re-serialization of the
/// version's own fields re-mints the same package, and the recomputed hash
/// is the integrity check: tampered bytes address differently.
fn package_of(version: &SkillVersion) -> Result<SkillPackage, SkillError> {
    let metadata = version.metadata();
    let mut frontmatter = format!(
        "name: {}\ndescription: {}",
        metadata.name, metadata.description
    );
    if let Some(license) = &metadata.license {
        frontmatter.push_str(&format!("\nlicense: {license}"));
    }
    if !metadata.allowed_tools.is_empty() {
        frontmatter.push_str(&format!(
            "\nallowed-tools: {}",
            metadata.allowed_tools.join(", ")
        ));
    }
    if let Some(compatibility) = &metadata.compatibility {
        frontmatter.push_str(&format!("\ncompatibility: {compatibility}"));
    }
    if let Some(eval_gate) = &metadata.eval_gate {
        frontmatter.push_str(&format!("\neval-gate: {eval_gate}"));
    }
    if !metadata.dependencies.is_empty() {
        let dep_list: Vec<String> = metadata
            .dependencies
            .iter()
            .map(|d| d.dependency_id())
            .collect();
        frontmatter.push_str(&format!("\ndependencies: {}", dep_list.join(", ")));
    }
    let skill_md = format!("---\n{frontmatter}\n---\n\n{}", version.body());
    let mut files = BTreeMap::new();
    files.insert("SKILL.md".to_owned(), skill_md.into_bytes());
    for path in version.reference_paths() {
        if let Some(bytes) = version.reference(path) {
            files.insert(path.to_owned(), bytes.to_vec());
        }
    }
    for path in version.asset_paths() {
        if let Some(bytes) = version.asset(path) {
            files.insert(path.to_owned(), bytes.to_vec());
        }
    }
    SkillPackage::from_files(files)
}

/// One loaded version with its tenant, recovered from its path and
/// verified against its own content address.
struct LoadedVersion {
    tenant: String,
    version: SkillVersion,
}

/// Load every stored version under `dir`, verifying each against its
/// content address. The tenant comes from the *path* (two components =
/// default tenant, three = named tenant), never from the record — the same
/// path-keyed tenancy the memory loader keeps. Unreadable, unparseable, or
/// hash-mismatched files are skipped with a warning, not fatal at boot.
fn load_versions(root: &Path) -> Vec<LoadedVersion> {
    let base = dir(root);
    let mut files = Vec::new();
    collect_json_files(&base, &mut files);
    let mut out = Vec::new();
    for path in files {
        let Some(relative) = path.strip_prefix(&base).ok() else {
            continue;
        };
        let components: Vec<String> = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        let tenant = match components.len() {
            2 => DEFAULT_TENANT.to_owned(),
            3 => components[0].clone(),
            _ => {
                tracing::warn!(path = %path.display(), "skipping skill file at an unexpected depth");
                continue;
            }
        };
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<SkillVersion>(&raw).ok());
        let Some(version) = parsed else {
            tracing::warn!(path = %path.display(), "skipping unreadable skill version file");
            continue;
        };
        match package_of(&version) {
            Ok(package) if package.content_hash() == version.content_hash() => {
                out.push(LoadedVersion { tenant, version });
            }
            Ok(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "skipping skill version whose bytes no longer address to its recorded hash"
                );
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping skill version that no longer parses");
            }
        }
    }
    out
}

/// Recursively collect `*.json` files under `root` (the memory loader's
/// walk).
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

/// Errors that can occur during promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromotionError {
    /// The skill was not found.
    NotFound,
    /// An I/O error occurred persisting the promotion record.
    Io(String),
}

impl std::fmt::Display for PromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromotionError::NotFound => write!(f, "skill not found"),
            PromotionError::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for PromotionError {}

/// One promotion history record per (tenant, skill_name) stored as JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PromotionHistory {
    promotions: Vec<SkillPromotion>,
}

/// Directory for promotion records under the store root.
fn promotions_dir(root: &Path) -> PathBuf {
    root.join("skill-promotions")
}

/// Path for one skill's promotion history.
fn promotion_history_path(root: &Path, tenant: &str, name: &str) -> PathBuf {
    promotions_dir(root)
        .join(scope_id(tenant, name))
        .with_extension("json")
}

/// Persist one promotion atomically (temp file + rename).
async fn persist_promotion(
    root: &Path,
    tenant: &str,
    name: &str,
    promotion: &SkillPromotion,
) -> Result<(), PromotionError> {
    let path = promotion_history_path(root, tenant, name);
    let io = |msg: String| PromotionError::Io(format!("{path}: {msg}", path = path.display()));

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io(format!("create dir: {e}")))?;
    }

    let mut history = match tokio::fs::read_to_string(&path).await {
        Ok(text) => serde_json::from_str::<PromotionHistory>(&text)
            .map_err(|e| io(format!("parse: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            PromotionHistory { promotions: vec![] }
        }
        Err(e) => return Err(io(format!("read: {e}"))),
    };

    history.promotions.push(promotion.clone());
    let bytes = serde_json::to_vec_pretty(&history).map_err(|e| io(format!("serialize: {e}")))?;
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| io(format!("write: {e}")))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| io(format!("rename: {e}")))
}

/// Load promotion history for one skill.
fn load_promotion_history(root: &Path, tenant: &str, name: &str) -> Vec<SkillPromotion> {
    let path = promotion_history_path(root, tenant, name);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<PromotionHistory>(&text)
            .map(|h| h.promotions)
            .unwrap_or_default(),
        Err(_) => vec![],
    }
}
/// durable file set under `{store_path}/skills/`.
///
/// The registry is the in-memory authority for reads; the file set is the
/// restart authority. Registration holds the tenant registry's lock across
/// the file write (the assistants convention), registry first and file
/// second: a crash between the two loses one acknowledged registration,
/// which the client re-applies idempotently — the rebuilt registry assigns
/// the same revision to the same content, because revisions are append-only
/// positions over a content-addressed history.
pub(crate) struct SkillPlane {
    root: PathBuf,
    tenants: Mutex<HashMap<String, SkillRegistry>>,
    /// Promotion history per (tenant, skill_name). Loaded at boot from
    /// `{store_path}/skill-promotions/`.
    promotions: Mutex<HashMap<(String, String), Vec<SkillPromotion>>>,
    /// The revision each skill's followers run: set by a promotion, or by a
    /// registration nothing could judge. Absent means the latest. Kept in
    /// `{root}/skills/current.json`.
    current: Mutex<HashMap<(String, String), u64>>,
    /// Skills a person took out of the library: who and when, by
    /// (tenant, name). The history stays — a run that used one still
    /// replays — but the library and its pickers no longer offer it.
    /// Registering the name again brings it back. Kept in
    /// `{root}/skill-removed.json`.
    removed: Mutex<HashMap<(String, String), Value>>,
}

fn removed_path(root: &Path) -> PathBuf {
    root.join("skill-removed.json")
}

fn load_removed(root: &Path) -> HashMap<(String, String), Value> {
    let Ok(bytes) = std::fs::read(removed_path(root)) else {
        return HashMap::new();
    };
    serde_json::from_slice::<HashMap<String, Value>>(&bytes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, mark)| {
            key.split_once('/')
                .map(|(t, n)| ((t.to_owned(), n.to_owned()), mark))
        })
        .collect()
}

fn current_path(root: &Path) -> PathBuf {
    root.join("skill-current.json")
}

fn load_current(root: &Path) -> HashMap<(String, String), u64> {
    let Ok(bytes) = std::fs::read(current_path(root)) else {
        return HashMap::new();
    };
    let Ok(map) = serde_json::from_slice::<HashMap<String, u64>>(&bytes) else {
        tracing::warn!("skills/current.json does not parse; every skill resolves to its latest until promoted again");
        return HashMap::new();
    };
    map.into_iter()
        .filter_map(|(key, rev)| {
            key.split_once('/')
                .map(|(t, n)| ((t.to_owned(), n.to_owned()), rev))
        })
        .collect()
}

impl SkillPlane {
    /// Rebuild the plane from the store root (boot path; synchronous like
    /// [`crate::server_store::JsonFileStore::load`]).
    pub(crate) fn load(root: &Path) -> Self {
        let current = load_current(root);
        let removed = load_removed(root);
        let mut loaded = load_versions(root);
        // Revision order within one (tenant, name) is the re-registration
        // order — registrations append, so sorted replay reproduces the
        // registered revisions exactly.
        loaded.sort_by_key(|entry| {
            (
                entry.tenant.clone(),
                entry.version.name().to_owned(),
                entry.version.revision(),
            )
        });
        let mut tenants: HashMap<String, SkillRegistry> = HashMap::new();
        for entry in loaded {
            let registry = tenants.entry(entry.tenant.clone()).or_default();
            let version = entry.version;
            if version.revision() as usize != registry.history(version.name()).len() + 1 {
                tracing::warn!(
                    tenant = %entry.tenant,
                    name = %version.name(),
                    revision = version.revision(),
                    "skill revision sequence is gapped (a skipped file compacts later revisions)"
                );
            }
            let provenance = version.provenance().clone();
            match package_of(&version) {
                Ok(package) => {
                    if let Err(error) =
                        registry.register(package, provenance.source, provenance.author)
                    {
                        tracing::warn!(%error, "skipping skill version that failed re-registration");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "skipping skill version that failed re-parsing");
                }
            }
        }

        // Load promotion histories.
        let mut promotions: HashMap<(String, String), Vec<SkillPromotion>> = HashMap::new();
        let promo_dir = promotions_dir(root);
        if promo_dir.exists() {
            let entries = match std::fs::read_dir(&promo_dir) {
                Ok(entries) => entries,
                Err(_) => {
                    tracing::warn!("cannot read promotion directory");
                    return Self {
                        root: root.to_path_buf(),
                        tenants: Mutex::new(tenants),
                        promotions: Mutex::new(promotions),
                        current: Mutex::new(current),
                        removed: Mutex::new(removed),
                    };
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                // stem is either "{name}" (default tenant) or "{tenant}/{name}" (named tenant)
                let (tenant, name) = if let Some(idx) = stem.find('/') {
                    (stem[..idx].to_owned(), stem[idx + 1..].to_owned())
                } else {
                    (DEFAULT_TENANT.to_owned(), stem.to_owned())
                };
                let history = load_promotion_history(root, &tenant, &name);
                if !history.is_empty() {
                    promotions.insert((tenant, name), history);
                }
            }
        }

        Self {
            root: root.to_path_buf(),
            tenants: Mutex::new(tenants),
            promotions: Mutex::new(promotions),
            current: Mutex::new(current),
            removed: Mutex::new(removed),
        }
    }

    /// The revision followers run: the current one when a promotion (or an
    /// unjudged registration) set it, else the latest.
    pub(crate) async fn resolve(&self, tenant: &str, name: &str) -> Option<Arc<SkillVersion>> {
        match self.current_revision(tenant, name).await {
            Some(revision) => {
                self.get_version(tenant, name, SkillVersionSelector::Revision(revision))
                    .await
            }
            None => self.get(tenant, name).await,
        }
    }

    pub(crate) async fn current_revision(&self, tenant: &str, name: &str) -> Option<u64> {
        self.current
            .lock()
            .await
            .get(&(tenant.to_owned(), name.to_owned()))
            .copied()
    }

    /// Whether a person took this skill out of the library (who and when).
    pub(crate) async fn removed(&self, tenant: &str, name: &str) -> Option<Value> {
        self.removed
            .lock()
            .await
            .get(&(tenant.to_owned(), name.to_owned()))
            .cloned()
    }

    /// Take a skill out of the library, or (`None`) put it back; kept.
    pub(crate) async fn set_removed(&self, tenant: &str, name: &str, mark: Option<Value>) {
        let mut removed = self.removed.lock().await;
        let key = (tenant.to_owned(), name.to_owned());
        let changed = match mark {
            Some(mark) => removed.insert(key, mark).is_none(),
            None => removed.remove(&key).is_some(),
        };
        if !changed {
            return;
        }
        let map: HashMap<String, Value> = removed
            .iter()
            .map(|((t, n), m)| (format!("{t}/{n}"), m.clone()))
            .collect();
        drop(removed);
        let path = removed_path(&self.root);
        if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Make `revision` the one followers run, and keep it.
    pub(crate) async fn set_current(&self, tenant: &str, name: &str, revision: u64) {
        let mut current = self.current.lock().await;
        current.insert((tenant.to_owned(), name.to_owned()), revision);
        let map: HashMap<String, u64> = current
            .iter()
            .map(|((t, n), r)| (format!("{t}/{n}"), *r))
            .collect();
        drop(current);
        let path = current_path(&self.root);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

impl SkillPlane {
    /// Demote a skill to Trial, appending a record to the promotion history
    /// (the dependency-invalidation route's half of the older promotion
    /// ledger; the gate itself is `after_registration` / `promote_skill`).
    pub(crate) async fn demote(
        &self,
        tenant: &str,
        name: &str,
        author: String,
    ) -> Result<SkillPromotion, PromotionError> {
        let tenants = self.tenants.lock().await;
        let version = tenants
            .get(tenant)
            .and_then(|registry| registry.get(name))
            .ok_or(PromotionError::NotFound)?;
        let revision = version.revision();
        let content_hash = version.content_hash().to_owned();
        drop(tenants);

        let promotion = SkillPromotion {
            name: name.to_owned(),
            revision,
            content_hash,
            status: SkillPromotionStatus::Trial,
            gate_run_id: None,
            author,
            created_at: chrono::Utc::now(),
        };

        persist_promotion(&self.root, tenant, name, &promotion).await?;
        let mut proms = self.promotions.lock().await;
        proms
            .entry((tenant.to_owned(), name.to_owned()))
            .or_default()
            .push(promotion.clone());

        Ok(promotion)
    }

    /// Get the promotion history for a skill.
    #[allow(dead_code)]
    pub(crate) async fn promotion_history(&self, tenant: &str, name: &str) -> Vec<SkillPromotion> {
        let promotions = self.promotions.lock().await;
        promotions
            .get(&(tenant.to_owned(), name.to_owned()))
            .cloned()
            .unwrap_or_default()
    }
    /// tenant's registry, then persist a fresh version's file. The scan
    /// runs inside the registry and denials fail closed as
    /// [`SkillError::ScanDenied`]; registration is idempotent on content.
    pub(crate) async fn register(
        &self,
        tenant: &str,
        package: SkillPackage,
        source: SkillSource,
        author: String,
    ) -> Result<Registration, SkillError> {
        let mut tenants = self.tenants.lock().await;
        let registry = tenants.entry(tenant.to_owned()).or_default();
        let registration = registry.register(package, source, author)?;
        if !registration.already_registered {
            persist_version(&self.root, tenant, &registration.version).await?;
        }
        drop(tenants);
        // Registering a removed skill's name is a person asking for it back.
        self.set_removed(tenant, registration.version.name(), None)
            .await;
        Ok(registration)
    }

    /// The tenant's tier-1 catalog (name-sorted latest metadata), without
    /// the skills a person took out of the library.
    pub(crate) async fn list(&self, tenant: &str) -> Vec<SkillMetadata> {
        let all = {
            let tenants = self.tenants.lock().await;
            tenants
                .get(tenant)
                .map(SkillRegistry::list)
                .unwrap_or_default()
        };
        let removed = self.removed.lock().await;
        all.into_iter()
            .filter(|meta| !removed.contains_key(&(tenant.to_owned(), meta.name.clone())))
            .collect()
    }

    /// The latest version of one skill in the caller's tenant.
    pub(crate) async fn get(&self, tenant: &str, name: &str) -> Option<Arc<SkillVersion>> {
        let tenants = self.tenants.lock().await;
        tenants.get(tenant)?.get(name)
    }

    /// One pinned version of a skill in the caller's tenant.
    pub(crate) async fn get_version(
        &self,
        tenant: &str,
        name: &str,
        selector: SkillVersionSelector,
    ) -> Option<Arc<SkillVersion>> {
        let tenants = self.tenants.lock().await;
        tenants.get(tenant)?.get_version(name, selector)
    }

    /// The tenant's revision history for one skill, ascending.
    pub(crate) async fn history(&self, tenant: &str, name: &str) -> Vec<SkillMetadata> {
        let tenants = self.tenants.lock().await;
        tenants
            .get(tenant)
            .map(|registry| registry.history(name))
            .unwrap_or_default()
    }
}

// --------------------------------------------------------------------- //
// The HTTP surface
// --------------------------------------------------------------------- //

/// `POST /skills` payload: the raw `SKILL.md` text plus its members.
/// References are markdown text; assets are hex-encoded bytes (no base64
/// codec in the dependency tree, and hex decodes with twenty lines of
/// obvious code). Member keys are paths *beneath* their directory —
/// `guide.md`, `nested/deep.md` — the server prefixes `references/` /
/// `assets/` and core's package validation enforces the hygiene rules.
#[derive(Debug, Deserialize)]
pub(crate) struct RegisterSkillPayload {
    /// The raw `SKILL.md` text, parsed byte-for-byte as authored.
    skill_md: String,
    /// Reference members (UTF-8 text), keyed by path beneath `references/`.
    #[serde(default)]
    references: BTreeMap<String, String>,
    /// Asset members (hex-encoded bytes), keyed by path beneath `assets/`.
    #[serde(default)]
    assets: BTreeMap<String, String>,
    /// Who registers the package. Provenance is mandatory, so this
    /// defaults to the signed-in principal; a program registering on its
    /// own behalf may name itself.
    #[serde(default)]
    author: Option<String>,
    /// Where the package came from; defaults to the server's own HTTP
    /// registry.
    #[serde(default)]
    source: Option<SkillSource>,
}

/// Decode one hex string (either case) into bytes.
fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("hex values must have an even length".to_owned());
    }
    let digit = |byte: u8| -> Result<u8, String> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            other => Err(format!("invalid hex digit `{}`", other as char)),
        }
    };
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect()
}

/// The `422` body for a scan denial: the [`ApiError`] shape plus the
/// structured findings — the report the caller acts on. Built directly
/// (not through [`ApiError`]) because its body is exactly `{error,
/// message}` and the findings are the point of the status.
fn scan_denied_response(denials: &[rusty_agent_runtime::skill::ScanFinding]) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({
            "error": "scan_denied",
            "message": format!("the security scan denied the package: {} denial(s)", denials.len()),
            "findings": denials,
        })),
    )
        .into_response()
}

/// The scan summary every version receipt carries: counts plus the full
/// findings (warnings and — on a stored version, by construction never —
/// denials).
fn scan_summary(version: &SkillVersion) -> Value {
    let report = version.scan();
    json!({
        "clean": report.is_clean(),
        "warnings": report.warnings().collect::<Vec<_>>(),
        "warning_count": report.warnings().count(),
    })
}

/// The version receipt: name, revision, content hash, provenance, and the
/// scan summary — the registration response and the detail read share it.
fn version_receipt(version: &SkillVersion) -> Value {
    json!({
        "metadata": version.metadata(),
        "name": version.name(),
        "revision": version.revision(),
        "content_hash": version.content_hash(),
        "provenance": version.provenance(),
        "scan": scan_summary(version),
    })
}

/// `POST /skills` — register a package → `201 {name, revision,
/// content_hash, already_registered, provenance, scan}`; `200` with
/// `already_registered: true` when the exact content is already registered
/// (content addressing makes re-registration idempotent by construction).
/// `422` + structured findings on a scan denial, `400` on any package
/// violation.
pub(crate) async fn register_skill(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<RegisterSkillPayload>,
) -> Response {
    let mut files = BTreeMap::new();
    files.insert("SKILL.md".to_owned(), payload.skill_md.into_bytes());
    for (path, text) in payload.references {
        files.insert(format!("references/{path}"), text.into_bytes());
    }
    for (path, hex) in &payload.assets {
        let bytes = match decode_hex(hex) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ApiError::bad_request(format!("asset `{path}`: {error}")).into_response();
            }
        };
        files.insert(format!("assets/{path}"), bytes);
    }
    let package = match SkillPackage::from_files(files) {
        Ok(package) => package,
        Err(error) => return ApiError::bad_request(error.to_string()).into_response(),
    };
    let source = payload.source.unwrap_or(SkillSource::Registry {
        name: "rusty-server".to_owned(),
    });
    // Provenance is mandatory: the signed-in principal unless a program
    // names itself. An empty name is a mistake, not a request to default.
    let author = match payload.author {
        None => tenant.principal().name.clone(),
        Some(author) if author.trim().is_empty() => {
            return ApiError::bad_request(
                "the author, when given, is not empty — leave it out to register as yourself"
                    .to_owned(),
            )
            .into_response();
        }
        Some(author) => author.trim().to_owned(),
    };
    match state
        .skills
        .register(tenant.tenant(), package, source, author)
        .await
    {
        Ok(registration) => {
            let status = if registration.already_registered {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            let mut receipt = version_receipt(&registration.version);
            receipt["already_registered"] = json!(registration.already_registered);
            if !registration.already_registered {
                let (gate, held, current) = after_registration(
                    &state,
                    &tenant,
                    registration.version.name(),
                    registration.version.revision(),
                )
                .await;
                receipt["gate"] = json!(gate);
                receipt["held"] = json!(held);
                receipt["current"] = json!(current);
            }
            (status, Json(receipt)).into_response()
        }
        Err(SkillError::ScanDenied { denials }) => scan_denied_response(&denials),
        Err(error) => ApiError::bad_request(error.to_string()).into_response(),
    }
}

/// A new revision of a skill reaches every agent that follows it on their
/// next run, and only their suites say whether it still holds — so the
/// suites run now, with nobody pressing anything: every dataset whose cases
/// were recorded from a follower's runs, at its newest version, against
/// that follower. What started (or could not) is the receipt's `gate`; the
/// verdicts arrive on each dataset's evaluations as the cases finish.
/// A new revision reaches its followers only through the gate: their
/// suites run against it, pinned to it; while any follower has a suite the
/// revision is *held* — the current one keeps running — until a promotion
/// with that evidence (or an admin's recorded word). A skill nothing can
/// judge becomes current at once. Returns (gate, held, current).
pub(crate) async fn after_registration(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    skill: &str,
    revision: u64,
) -> (Vec<Value>, bool, u64) {
    let gate = gate_followers(state, tenant, skill, revision).await;
    // Held when any follower has a suite — even one that could not start
    // (the gate fails closed; an admin's word is the way past it). A first
    // revision has nothing to hold back to.
    if !gate.is_empty() && revision > 1 {
        let previous = match state.skills.current_revision(tenant.tenant(), skill).await {
            Some(current) if current != revision => current,
            _ => revision.saturating_sub(1).max(1),
        };
        state
            .skills
            .set_current(tenant.tenant(), skill, previous)
            .await;
        tracing::info!(
            skill,
            revision,
            current = previous,
            "skill revision held: its followers' suites judge it first"
        );
        (gate, true, previous)
    } else {
        state
            .skills
            .set_current(tenant.tenant(), skill, revision)
            .await;
        (gate, false, revision)
    }
}

/// A follower a suite can judge: the agent, and the newest version of a
/// dataset with cases from it.
pub(crate) struct JudgedFollower {
    pub assistant_id: String,
    pub record: crate::assistants::AssistantRecord,
    pub dataset: String,
    pub version: String,
    pub cases: Vec<crate::evaluations::PublishedEvalCase>,
}

pub(crate) async fn judged_followers(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    skill: &str,
) -> Vec<JudgedFollower> {
    let mut out = Vec::new();
    let Ok(views) = state.server_store.list_assistants().await else {
        return out;
    };
    let Ok(catalog) = crate::evaluations::list_datasets(&state.server_store, tenant.tenant()).await
    else {
        return out;
    };
    // The catalog lists every version; a suite is its dataset's newest one.
    let mut newest: Vec<&crate::evaluations::DatasetVersionRecord> = Vec::new();
    for dataset in &catalog.datasets {
        match newest.iter().position(|n| n.name == dataset.name) {
            Some(at) if dataset.created_at > newest[at].created_at => newest[at] = dataset,
            Some(_) => {}
            None => newest.push(dataset),
        }
    }
    for view in views {
        let Some(external) = tenant.unscope(&view.assistant_id).map(str::to_owned) else {
            continue;
        };
        let Ok(Some(record)) = state.server_store.get_assistant(&view.assistant_id).await else {
            continue;
        };
        if record.archived_at.is_some()
            || !crate::routes::assistant_skills(&record.config)
                .iter()
                .any(|s| s == skill)
        {
            continue;
        }
        for dataset in &newest {
            let Ok(cases) = crate::evaluations::load_dataset_cases(
                &state.server_store,
                tenant.tenant(),
                &dataset.name,
                &dataset.version,
            )
            .await
            else {
                continue;
            };
            if !cases.iter().any(|case| case.source.agent_id == external) {
                continue;
            }
            out.push(JudgedFollower {
                assistant_id: external.clone(),
                record: record.clone(),
                dataset: dataset.name.clone(),
                version: dataset.version.clone(),
                cases,
            });
        }
    }
    out
}

/// Start every judged follower's suite against `revision` of `skill`
/// (the evaluations run pinned to it — see `dataset_runs::skill_pin`).
pub(crate) async fn gate_followers(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    skill: &str,
    revision: u64,
) -> Vec<Value> {
    let mut started = Vec::new();
    for follower in judged_followers(state, tenant, skill).await {
        let mut entry = json!({
            "assistant_id": follower.assistant_id,
            "assistant": follower.record.name,
            "dataset": follower.dataset,
            "version": follower.version,
            "cases": follower.cases.len(),
        });
        match crate::dataset_runs::start(
            Arc::clone(state),
            tenant.tenant().to_owned(),
            follower.dataset.clone(),
            follower.version.clone(),
            follower.record.clone(),
            follower.cases,
            Some(json!({ "kind": "skill", "name": skill, "revision": revision })),
            None,
        )
        .await
        {
            Ok(evaluation) => entry["evaluation_id"] = json!(evaluation.evaluation_id),
            Err(error) => entry["error"] = json!(format!("{error:?}")),
        }
        started.push(entry);
    }
    started
}

/// The gate's view of one revision: every judged follower's latest
/// evaluation pinned to it.
pub(crate) async fn skill_evidence(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    skill: &str,
    revision: u64,
) -> Value {
    let mut suites = Vec::new();
    for follower in judged_followers(state, tenant, skill).await {
        let evaluations =
            crate::dataset_runs::list(state, tenant.tenant(), &follower.dataset, &follower.version)
                .await
                .unwrap_or_default();
        let mine = evaluations.iter().find(|e| {
            e.assistant_id == follower.assistant_id
                && e.status != "error"
                && crate::dataset_runs::skill_pin(e.started_by.as_ref())
                    .is_some_and(|(n, r)| n == skill && r == revision)
        });
        let (state_word, evaluation_id, passed, total, at) = match mine {
            Some(e) if e.status == "running" => (
                "running",
                Some(e.evaluation_id.clone()),
                e.passed,
                e.total,
                None,
            ),
            Some(e) if e.total > 0 && e.passed == e.total => (
                "passed",
                Some(e.evaluation_id.clone()),
                e.passed,
                e.total,
                e.finished_at,
            ),
            Some(e) => (
                "failed",
                Some(e.evaluation_id.clone()),
                e.passed,
                e.total,
                e.finished_at,
            ),
            None => ("missing", None, 0, follower.cases.len(), None),
        };
        suites.push(json!({
            "assistant_id": follower.assistant_id,
            "assistant": follower.record.name,
            "dataset": follower.dataset,
            "version": follower.version,
            "cases": follower.cases.len(),
            "state": state_word,
            "evaluation_id": evaluation_id,
            "passed": passed,
            "total": total,
            "evaluated_at": at,
            "failures": match mine { Some(e) if state_word == "failed" => crate::dataset_runs::failures_of(e), _ => Vec::new() },
        }));
    }
    let unevaluated = suites.is_empty();
    let ok = suites.iter().all(|s| s["state"] == "passed");
    json!({"revision": revision, "suites": suites, "ok": ok, "unevaluated": unevaluated})
}

/// `GET /skills` — the tier-1 catalog: latest-version metadata for every
/// skill in the caller's tenant, name-sorted (core's registry order).
/// Metadata carries no body — the listing is the cheap tier by
/// construction.
pub(crate) async fn list_skills(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Json<Value> {
    let skills = state.skills.list(tenant.tenant()).await;
    Json(json!({ "skills": skills }))
}

/// `GET /skills/{name}` — the latest version's receipt plus the revision
/// count (`404` unknown/cross-tenant — the two are indistinguishable by
/// design).
pub(crate) async fn get_skill(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let version = state
        .skills
        .get(tenant.tenant(), &name)
        .await
        .ok_or_else(|| ApiError::not_found(format!("skill `{name}` not found")))?;
    let mut receipt = version_receipt(&version);
    receipt["revisions"] = json!(state.skills.history(tenant.tenant(), &name).await.len());
    let current = state
        .skills
        .current_revision(tenant.tenant(), &name)
        .await
        .unwrap_or(version.revision());
    receipt["current"] = json!(current);
    receipt["latest"] = json!(version.revision());
    receipt["candidate"] = json!((version.revision() > current).then_some(version.revision()));
    if let Some(mark) = state.skills.removed(tenant.tenant(), &name).await {
        receipt["removed"] = mark;
    }
    Ok(Json(receipt))
}

/// `DELETE /skills/{name}` — take a skill out of the library. Refused while
/// an agent uses it (its working copy or the version it serves), naming
/// them: taking a skill from under an agent is the builder's call on that
/// agent, not the library's. The history stays, so runs that used it still
/// replay; registering the name again brings it back.
pub(crate) async fn remove_skill(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    if state.skills.get(tenant.tenant(), &name).await.is_none()
        || state.skills.removed(tenant.tenant(), &name).await.is_some()
    {
        return Err(ApiError::not_found(format!("skill `{name}` not found")));
    }
    let users = agents_using(&state, &tenant, &name).await;
    if !users.is_empty() {
        let names = match users.as_slice() {
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            [] => unreachable!(),
        };
        return Err(ApiError::conflict(format!(
            "{names} still use{} this skill — take it off {} first",
            if users.len() == 1 { "s" } else { "" },
            if users.len() == 1 {
                "that agent"
            } else {
                "them"
            }
        )));
    }
    let mark = json!({"removed_by": tenant.principal().name, "removed_at": chrono::Utc::now()});
    state
        .skills
        .set_removed(tenant.tenant(), &name, Some(mark.clone()))
        .await;
    Ok(Json(json!({"name": name, "removed": mark})))
}

/// The agents (by name) whose working copy or served version names `skill`.
/// An archived agent does not count: it starts no work, and brought back it
/// still runs the skill's kept history.
async fn agents_using(state: &Arc<AppState>, tenant: &TenantContext, skill: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(views) = state.server_store.list_assistants().await else {
        return out;
    };
    for view in views {
        if tenant.unscope(&view.assistant_id).is_none() {
            continue;
        }
        let Ok(Some(record)) = state.server_store.get_assistant(&view.assistant_id).await else {
            continue;
        };
        if record.archived_at.is_some() {
            continue;
        }
        let names = |config: &Value| {
            crate::routes::assistant_skills(config)
                .iter()
                .any(|s| s == skill)
        };
        let served = record
            .active_version_id
            .as_deref()
            .and_then(|id| record.versions.iter().find(|v| v.version_id == id));
        if names(&record.config)
            || record.versions.last().is_some_and(|v| names(&v.config))
            || served.is_some_and(|v| names(&v.config))
        {
            out.push(record.name.clone());
        }
    }
    out.sort();
    out
}

/// `GET /skills/{name}/evidence?revision=` — what the gate knows about a
/// revision (the latest by default): every judged follower's evaluation
/// pinned to it, the current revision, the promotions.
pub(crate) async fn get_skill_evidence(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
    axum::extract::Query(query): axum::extract::Query<EvidenceQuery>,
) -> Result<Json<Value>, ApiError> {
    let latest = state
        .skills
        .get(tenant.tenant(), &name)
        .await
        .ok_or_else(|| ApiError::not_found(format!("skill `{name}` not found")))?;
    let revision = query.revision.unwrap_or(latest.revision());
    let current = state
        .skills
        .current_revision(tenant.tenant(), &name)
        .await
        .unwrap_or(latest.revision());
    let evidence = skill_evidence(&state, &tenant, &name, revision).await;
    let promotions = skill_promotions(&state, tenant.tenant(), &name).await;
    Ok(Json(
        json!({"name": name, "current": current, "latest": latest.revision(), "evidence": evidence, "promotions": promotions}),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvidenceQuery {
    #[serde(default)]
    revision: Option<u64>,
}

fn promotions_namespace(tenant: &str) -> String {
    format!("skill-promotions:{tenant}")
}

pub(crate) async fn skill_promotions(state: &AppState, tenant: &str, name: &str) -> Vec<Value> {
    state
        .server_store
        .kv_get(&promotions_namespace(tenant), name)
        .await
        .ok()
        .flatten()
        .and_then(|item| item.value.as_array().cloned())
        .unwrap_or_default()
}

async fn record_skill_promotion(state: &AppState, tenant: &str, name: &str, promotion: Value) {
    let mut all = skill_promotions(state, tenant, name).await;
    all.insert(0, promotion);
    all.truncate(50);
    if let Err(error) = state
        .server_store
        .kv_put(&promotions_namespace(tenant), name, Value::Array(all))
        .await
    {
        tracing::warn!(%error, skill = %name, "skill promotion not kept");
    }
}

/// `GET /skills/{name}/body` — the tier-2 disclosure unit: the `SKILL.md`
/// instructions of the latest revision, fetched on explicit demand.
pub(crate) async fn get_skill_body(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let version = state
        .skills
        .get(tenant.tenant(), &name)
        .await
        .ok_or_else(|| ApiError::not_found(format!("skill `{name}` not found")))?;
    Ok(Json(json!({
        "name": version.name(),
        "revision": version.revision(),
        "content_hash": version.content_hash(),
        "body": version.body(),
        "references": version.reference_paths().collect::<Vec<_>>(),
        "learns": crate::skill_learn::declared_plan(version.body()).map(|p| json!({"reference": p.reference, "reads": p.reads.len()})),
    })))
}

/// `GET /skills/{name}/history` — the append-only revision list as
/// metadata, ascending (`404` for an unknown name; the history *is* the
/// audit trail, so an empty one means the name never registered here).
pub(crate) async fn get_skill_history(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let history = state.skills.history(tenant.tenant(), &name).await;
    if history.is_empty() {
        return Err(ApiError::not_found(format!("skill `{name}` not found")));
    }
    Ok(Json(json!({ "name": name, "history": history })))
}

/// `GET /skills/{name}/versions/{revision}` — the pinned version's receipt
/// (`404` unknown name or revision).
pub(crate) async fn get_skill_version(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath((name, revision)): AxumPath<(String, u64)>,
) -> Result<Json<Value>, ApiError> {
    let version = state
        .skills
        .get_version(
            tenant.tenant(),
            &name,
            SkillVersionSelector::Revision(revision),
        )
        .await
        .ok_or_else(|| {
            ApiError::not_found(format!("skill `{name}` revision {revision} not found"))
        })?;
    Ok(Json(version_receipt(&version)))
}

/// `GET /skills/{name}/files/{*path}` — one tier-3 member of the latest
/// revision: references serve as `text/markdown`, assets as
/// `application/octet-stream`. Hygiene is structural — the wildcard path is
/// a lookup key into the version's validated member maps, so `..`,
/// absolute, and backslash forms simply match nothing and answer `404`.
pub(crate) async fn get_skill_file(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath((name, path)): AxumPath<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let version = state
        .skills
        .get(tenant.tenant(), &name)
        .await
        .ok_or_else(|| ApiError::not_found(format!("skill `{name}` not found")))?;
    let not_found = || ApiError::not_found(format!("skill `{name}` has no member `{path}`"));
    if let Some(bytes) = version.reference(&path) {
        return Ok((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            bytes.to_vec(),
        ));
    }
    if let Some(bytes) = version.asset(&path) {
        return Ok((
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes.to_vec(),
        ));
    }
    Err(not_found())
}

/// `POST /skills/{name}/promote` payload.
#[derive(Debug, Deserialize)]
pub(crate) struct PromoteSkillPayload {
    /// The revision to promote. Defaults to latest if omitted.
    #[serde(default)]
    revision: Option<u64>,
    /// An admin's word past the gate, kept with the promotion.
    #[serde(default)]
    override_reason: Option<String>,
}

/// `POST /skills/{name}/promote` — attempt promotion through the eval gate.
/// Returns `200` with the promotion record on success, `404` if the skill
/// or revision is unknown, `422` if no gate is declared, `403` if the gate
/// blocks (with diagnostics).
pub(crate) async fn promote_skill(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
    Json(payload): Json<PromoteSkillPayload>,
) -> Response {
    let revision = match payload.revision {
        Some(r) => r,
        None => match state.skills.get(tenant.tenant(), &name).await {
            Some(v) => v.revision(),
            None => {
                return ApiError::not_found(format!("skill `{name}` not found")).into_response();
            }
        },
    };

    // The gate: every judged follower's suite, evaluated pinned to this
    // revision, passed — or an admin's recorded word past it.
    if state
        .skills
        .get_version(
            tenant.tenant(),
            &name,
            SkillVersionSelector::Revision(revision),
        )
        .await
        .is_none()
    {
        return ApiError::not_found(format!("skill `{name}` revision {revision} not found"))
            .into_response();
    }
    let evidence = skill_evidence(&state, &tenant, &name, revision).await;
    let override_reason = payload
        .override_reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    if evidence["ok"] != json!(true) && override_reason.is_none() {
        let short: Vec<String> = evidence["suites"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["state"] != "passed")
            .map(|s| {
                format!(
                    "{} on {} ({})",
                    s["assistant"].as_str().unwrap_or("?"),
                    s["dataset"].as_str().unwrap_or("?"),
                    s["state"].as_str().unwrap_or("?")
                )
            })
            .collect();
        return ApiError::new(
            StatusCode::CONFLICT,
            "evidence_required",
            format!("revision {revision} of `{name}` has no passing evidence from its followers: {} — wait for their suites, or promote with an override_reason", short.join(", ")),
        )
        .with("evidence", evidence)
        .into_response();
    }
    state
        .skills
        .set_current(tenant.tenant(), &name, revision)
        .await;
    let promotion = json!({
        "revision": revision,
        "by": tenant.attribution(),
        "at": chrono::Utc::now(),
        "evidence": evidence,
        "override_reason": override_reason,
    });
    record_skill_promotion(&state, tenant.tenant(), &name, promotion.clone()).await;
    tracing::info!(skill = %name, revision, "skill revision promoted: its followers run it now");
    (
        StatusCode::OK,
        Json(json!({"name": name, "current": revision, "promoted": true, "promotion": promotion})),
    )
        .into_response()
}
/// `POST /skills/invalidate` payload.
#[derive(Debug, Deserialize)]
pub(crate) struct InvalidateSkillsPayload {
    /// The dependency id that changed (e.g. `tool:create_ticket`).
    dependency_id: String,
    /// The old SHA-256 fingerprint.
    old_fingerprint: String,
    /// The new SHA-256 fingerprint.
    new_fingerprint: String,
    /// What caused the change (e.g. `tool_reregistered`).
    change_source: String,
}

/// `POST /skills/invalidate` — event-driven invalidation of skills whose
/// declared dependencies changed. Every promoted dependent is demoted to
/// Trial and a revalidation gap entry is filed. One repair record covers
/// the whole episode.
pub(crate) async fn invalidate_skills(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<InvalidateSkillsPayload>,
) -> Response {
    let start_time = chrono::Utc::now();

    // Find all skills in the tenant that declare this dependency.
    let skills = state.skills.list(tenant.tenant()).await;
    let mut affected: Vec<String> = Vec::new();
    for meta in &skills {
        let dep_ids: Vec<String> = meta
            .dependencies
            .iter()
            .map(|d| d.dependency_id())
            .collect();
        if dep_ids.contains(&payload.dependency_id) {
            affected.push(meta.name.clone());
        }
    }

    let mut demotions: Vec<String> = Vec::new();
    let mut gap_ids: Vec<String> = Vec::new();

    for skill_name in &affected {
        let history = state
            .skills
            .promotion_history(tenant.tenant(), skill_name)
            .await;
        let current = history
            .last()
            .map(|p| p.status)
            .unwrap_or(SkillPromotionStatus::Trial);
        if current != SkillPromotionStatus::Promoted {
            continue;
        }
        match state
            .skills
            .demote(tenant.tenant(), skill_name, "invalidation".to_owned())
            .await
        {
            Ok(_) => {
                demotions.push(skill_name.clone());

                // File a revalidation gap entry for the demoted skill.
                let now = chrono::Utc::now();
                let skill_name_inner = skill_name.clone();
                let dep_id = payload.dependency_id.clone();
                let gap_result = crate::routes::mutate_gap_ledger(&state, &tenant, |ledger| {
                    let subject = GapSubject::question_shape(&format!(
                        "Revalidate skill {skill_name_inner} after dependency change"
                    ))?;
                    let statement = format!(
                        "Skill {skill_name_inner} was demoted after dependency {dep_id} changed"
                    );
                    let evidence = vec![
                        Citation::new(
                            CitationKind::MemoryRecord,
                            format!("dep-change:{}:{}", dep_id, payload.new_fingerprint),
                            Some("dependency change record".to_owned()),
                        )?,
                        Citation::new(
                            CitationKind::MemoryRecord,
                            format!("skill:{skill_name_inner}"),
                            Some("skill manifest hash at invalidation".to_owned()),
                        )?,
                    ];
                    let closure = ClosureCriteria::ArtifactPromoted {
                        candidate_id: format!("skill:{skill_name_inner}"),
                    };
                    ledger.file_gap(
                        subject,
                        statement,
                        evidence,
                        GapOrigin::RuntimeCorrection,
                        closure,
                        1,
                        10_000,
                        "invalidation",
                        now,
                    )
                })
                .await;

                match gap_result {
                    Ok(gap_id) => gap_ids.push(gap_id),
                    Err(e) => {
                        tracing::warn!(%e, "failed to file revalidation gap for skill {}", skill_name);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%e, "demotion failed for skill {}", skill_name);
            }
        }
    }

    // Emit one repair record for the episode.
    let outcome = if demotions.is_empty() {
        RepairOutcome::Repaired
    } else {
        RepairOutcome::Escalated
    };
    let record = RepairRecordBuilder::new()
        .component(RepairComponent::DependencyInvalidation)
        .trigger(RepairTrigger::DependencyChange {
            dependency_id: payload.dependency_id.clone(),
            old_fingerprint: payload.old_fingerprint,
            new_fingerprint: payload.new_fingerprint,
        })
        .action(RepairAction::DependencyInvalidation {
            rung: RepairRung::Knowledge,
        })
        .outcome(outcome)
        .start_time(start_time)
        .end_time(chrono::Utc::now())
        .citation(format!("change_source:{}", payload.change_source))
        .attempt_count(demotions.len() as u32)
        .build();
    let _ = state.repair_ledger.append(record);

    let status = if demotions.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (
        status,
        Json(json!({
            "dependency_id": payload.dependency_id,
            "affected": affected.len(),
            "demoted": demotions,
            "gap_ids": gap_ids,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::skill::SkillPackage;

    fn store_root() -> PathBuf {
        std::env::temp_dir().join(format!("rusty-skills-test-{}", uuid::Uuid::new_v4()))
    }

    fn skill_md(name: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\n{body}\n")
    }

    fn source() -> SkillSource {
        SkillSource::LocalPath {
            path: "/skills/test".to_owned(),
        }
    }

    #[tokio::test]
    async fn plane_round_trips_across_reload() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let package =
            SkillPackage::from_markdown(&skill_md("web-research", "Search, then summarize."))
                .unwrap();
        let registration = plane
            .register("default", package, source(), "operator:ada".to_owned())
            .await
            .unwrap();
        assert_eq!(registration.version.revision(), 1);
        let hash = registration.version.content_hash().to_owned();

        let reloaded = SkillPlane::load(&root);
        let version = reloaded.get("default", "web-research").await.unwrap();
        assert_eq!(version.revision(), 1);
        assert_eq!(version.content_hash(), hash);
        assert_eq!(version.body(), "Search, then summarize.\n");
        assert_eq!(version.provenance().author, "operator:ada");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reload_skips_corrupt_and_tampered_files() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        for (name, body) in [("a-skill", "One."), ("b-skill", "Two.")] {
            let package = SkillPackage::from_markdown(&skill_md(name, body)).unwrap();
            plane
                .register("default", package, source(), "operator:ada".to_owned())
                .await
                .unwrap();
        }
        // Corrupt JSON is skipped, not fatal.
        std::fs::write(dir(&root).join("broken.json"), b"{nope").unwrap();
        // Tampered bytes no longer address to the recorded hash.
        let tampered_path = version_path(&root, "default", "a-skill", 1);
        let mut tampered: Value =
            serde_json::from_slice(&std::fs::read(&tampered_path).unwrap()).unwrap();
        tampered["body"] = json!("Edited after the fact.");
        std::fs::write(&tampered_path, serde_json::to_vec(&tampered).unwrap()).unwrap();

        let reloaded = SkillPlane::load(&root);
        assert!(reloaded.get("default", "a-skill").await.is_none());
        assert!(reloaded.get("default", "b-skill").await.is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn registries_are_tenant_scoped() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let package = SkillPackage::from_markdown(&skill_md("a-skill", "Instructions.")).unwrap();
        plane
            .register("acme", package, source(), "operator:ada".to_owned())
            .await
            .unwrap();
        assert!(plane.get("acme", "a-skill").await.is_some());
        assert!(plane.get("globex", "a-skill").await.is_none());
        assert!(plane.list("globex").await.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hex_decoding_round_trips_and_rejects() {
        assert_eq!(
            decode_hex("89504e47").unwrap(),
            vec![0x89, 0x50, 0x4e, 0x47]
        );
        assert_eq!(
            decode_hex("89504E47").unwrap(),
            vec![0x89, 0x50, 0x4e, 0x47]
        );
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }

    // ----------------------------------------------------------------- //
    // Promotion gates
    // ----------------------------------------------------------------- //
}
