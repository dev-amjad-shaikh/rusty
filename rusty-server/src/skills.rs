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
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use rusty_agent_runtime::skill::{
    Registration, SkillError, SkillMetadata, SkillPackage, SkillPromotion, SkillPromotionStatus,
    SkillRegistry, SkillSource, SkillVersion, SkillVersionSelector,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::{DEFAULT_TENANT, TenantContext, scope_id};
use crate::error::ApiError;
use crate::routes::AppState;

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

/// The result of evaluating a skill's promotion gate.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateEvaluationResult {
    /// The gate passed: the suite run succeeded.
    Pass { run_id: String },
    /// The gate failed: the suite run produced failing cases.
    Fail {
        run_id: String,
        diagnostics: Vec<GateDiagnostic>,
    },
}

/// One failing case diagnostic for a gate refusal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct GateDiagnostic {
    pub case_id: String,
    pub reason: String,
}

/// A gate evaluator that always fails closed when no evaluator is
/// configured in the application state.
#[derive(Debug)]
pub(crate) struct UnconfiguredGateEvaluator;

#[async_trait::async_trait]
impl SkillGateEvaluator for UnconfiguredGateEvaluator {
    async fn evaluate(
        &self,
        _skill_name: &str,
        _revision: u64,
        _content_hash: &str,
        gate_name: &str,
    ) -> Result<GateEvaluationResult, String> {
        Err(format!(
            "no skill gate evaluator is configured; cannot evaluate gate `{gate_name}`"
        ))
    }
}

#[cfg(test)]
/// A test-double gate evaluator that returns a pre-configured result.
#[derive(Debug)]
pub(crate) struct ScriptedSkillGateEvaluator {
    pub result: Result<GateEvaluationResult, String>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl SkillGateEvaluator for ScriptedSkillGateEvaluator {
    async fn evaluate(
        &self,
        _skill_name: &str,
        _revision: u64,
        _content_hash: &str,
        _gate_name: &str,
    ) -> Result<GateEvaluationResult, String> {
        self.result.clone()
    }
}
/// Implementations run the eval suite named in `eval_gate` against the
/// candidate skill and return a pass/fail result.
#[async_trait::async_trait]
pub(crate) trait SkillGateEvaluator: Send + Sync + std::fmt::Debug {
    /// Evaluate the gate for `skill_name` revision `revision` with
    /// `content_hash` using the suite named `gate_name`.
    async fn evaluate(
        &self,
        skill_name: &str,
        revision: u64,
        content_hash: &str,
        gate_name: &str,
    ) -> Result<GateEvaluationResult, String>;
}

/// Errors that can occur during promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromotionError {
    /// The skill was not found.
    NotFound,
    /// The skill has no `eval_gate` declared.
    NoGateDeclared,
    /// The gate evaluation itself failed (infrastructure error, not a test failure).
    GateFailed(String),
    /// The gate blocked: tests failed.
    GateBlocked {
        run_id: String,
        diagnostics: Vec<GateDiagnostic>,
    },
    /// An I/O error occurred persisting the promotion record.
    Io(String),
}

impl std::fmt::Display for PromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromotionError::NotFound => write!(f, "skill not found"),
            PromotionError::NoGateDeclared => write!(f, "skill has no eval_gate declared"),
            PromotionError::GateFailed(msg) => write!(f, "gate evaluation failed: {msg}"),
            PromotionError::GateBlocked { run_id, .. } => {
                write!(f, "gate blocked by run {run_id}")
            }
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
}

impl SkillPlane {
    /// Rebuild the plane from the store root (boot path; synchronous like
    /// [`crate::server_store::JsonFileStore::load`]).
    pub(crate) fn load(root: &Path) -> Self {
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
        }
    }
}

impl SkillPlane {
    /// Register a validated package under `tenant`: version it through the
    /// tenant's registry, then persist a fresh version's file. The scan
    /// runs inside the registry and denials fail closed as
    /// [`SkillError::ScanDenied`]; registration is idempotent on content.
    /// Promote a skill revision through its eval gate.
    ///
    /// AC 1: A skill with `eval_gate` requires a passing suite run.
    /// AC 5: If content hash unchanged, reuse newest passing run;
    ///        if changed, demand new evaluation.
    pub(crate) async fn promote(
        &self,
        tenant: &str,
        name: &str,
        revision: u64,
        author: String,
        evaluator: &dyn SkillGateEvaluator,
    ) -> Result<SkillPromotion, PromotionError> {
        let tenants = self.tenants.lock().await;
        let version = tenants
            .get(tenant)
            .and_then(|registry| {
                registry.get_version(name, SkillVersionSelector::Revision(revision))
            })
            .ok_or(PromotionError::NotFound)?;

        let eval_gate = version.metadata().eval_gate.clone();
        let gate_name = eval_gate.ok_or(PromotionError::NoGateDeclared)?;
        let content_hash = version.content_hash().to_owned();
        drop(tenants); // release lock before async evaluation

        // AC 5: Check for existing passing run on same content hash.
        let promotions = self.promotions.lock().await;
        let existing = promotions
            .get(&(tenant.to_owned(), name.to_owned()))
            .and_then(|history| {
                history
                    .iter()
                    .rfind(|p| {
                        p.content_hash == content_hash && p.status == SkillPromotionStatus::Promoted
                    })
                    .cloned()
            });
        drop(promotions);

        if let Some(promotion) = existing {
            // Reuse the existing passing promotion for unchanged content.
            return Ok(promotion);
        }

        // Evaluate the gate.
        let result = evaluator
            .evaluate(name, revision, &content_hash, &gate_name)
            .await
            .map_err(PromotionError::GateFailed)?;

        let promotion = match result {
            GateEvaluationResult::Pass { run_id } => SkillPromotion {
                name: name.to_owned(),
                revision,
                content_hash,
                status: SkillPromotionStatus::Promoted,
                gate_run_id: Some(run_id),
                author,
                created_at: chrono::Utc::now(),
            },
            GateEvaluationResult::Fail {
                run_id,
                diagnostics,
            } => {
                // Record the failed attempt, then return error.
                let failed = SkillPromotion {
                    name: name.to_owned(),
                    revision,
                    content_hash,
                    status: SkillPromotionStatus::Trial,
                    gate_run_id: Some(run_id.clone()),
                    author: author.clone(),
                    created_at: chrono::Utc::now(),
                };
                persist_promotion(&self.root, tenant, name, &failed).await?;
                let mut proms = self.promotions.lock().await;
                proms
                    .entry((tenant.to_owned(), name.to_owned()))
                    .or_default()
                    .push(failed);
                return Err(PromotionError::GateBlocked {
                    run_id,
                    diagnostics,
                });
            }
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
        Ok(registration)
    }

    /// The tenant's tier-1 catalog (name-sorted latest metadata).
    pub(crate) async fn list(&self, tenant: &str) -> Vec<SkillMetadata> {
        let tenants = self.tenants.lock().await;
        tenants
            .get(tenant)
            .map(SkillRegistry::list)
            .unwrap_or_default()
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
    /// Who registers the package — provenance is mandatory.
    author: String,
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
    match state
        .skills
        .register(tenant.tenant(), package, source, payload.author)
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
            (status, Json(receipt)).into_response()
        }
        Err(SkillError::ScanDenied { denials }) => scan_denied_response(&denials),
        Err(error) => ApiError::bad_request(error.to_string()).into_response(),
    }
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
    Ok(Json(receipt))
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
    revision: Option<u64>,
    /// Who is attempting the promotion.
    author: String,
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

    // Use a default evaluator that fails closed when not configured.
    let default_evaluator = crate::skills::UnconfiguredGateEvaluator;
    let evaluator: &dyn crate::skills::SkillGateEvaluator = state
        .skill_gate_evaluator
        .as_deref()
        .map(|e| e as &dyn crate::skills::SkillGateEvaluator)
        .unwrap_or(&default_evaluator);

    match state
        .skills
        .promote(tenant.tenant(), &name, revision, payload.author, evaluator)
        .await
    {
        Ok(promotion) => {
            let receipt = json!({
                "name": promotion.name,
                "revision": promotion.revision,
                "content_hash": promotion.content_hash,
                "status": promotion.status,
                "gate_run_id": promotion.gate_run_id,
                "author": promotion.author,
                "created_at": promotion.created_at,
            });
            (StatusCode::OK, Json(receipt)).into_response()
        }
        Err(PromotionError::NotFound) => {
            ApiError::not_found(format!("skill `{name}` revision {revision} not found"))
                .into_response()
        }
        Err(PromotionError::NoGateDeclared) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "no_gate_declared",
                "message": "this skill has no eval_gate declared and cannot be promoted",
            })),
        )
            .into_response(),
        Err(PromotionError::GateBlocked {
            run_id,
            diagnostics,
        }) => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "gate_blocked",
                "message": format!("gate blocked by run {run_id}"),
                "run_id": run_id,
                "diagnostics": diagnostics,
            })),
        )
            .into_response(),
        Err(PromotionError::GateFailed(msg)) => {
            ApiError::internal(format!("gate evaluation failed: {msg}")).into_response()
        }
        Err(PromotionError::Io(msg)) => {
            ApiError::internal(format!("promotion persistence failed: {msg}")).into_response()
        }
    }
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

    fn gated_skill_md(name: &str, body: &str, gate: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: The {name} skill.\neval-gate: {gate}\n---\n\n{body}\n"
        )
    }

    #[tokio::test]
    async fn promotion_missing_gate_refuses() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let package = SkillPackage::from_markdown(&skill_md("no-gate", "Do something.")).unwrap();
        plane
            .register("default", package, source(), "op".to_owned())
            .await
            .unwrap();

        let evaluator = ScriptedSkillGateEvaluator {
            result: Ok(GateEvaluationResult::Pass {
                run_id: "run-1".to_owned(),
            }),
        };
        let result = plane
            .promote("default", "no-gate", 1, "op".to_string(), &evaluator)
            .await;
        assert_eq!(result, Err(PromotionError::NoGateDeclared));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn promotion_failing_gate_blocks() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let package =
            SkillPackage::from_markdown(&gated_skill_md("gated", "Do something.", "suite-a"))
                .unwrap();
        plane
            .register("default", package, source(), "op".to_owned())
            .await
            .unwrap();

        let evaluator = ScriptedSkillGateEvaluator {
            result: Ok(GateEvaluationResult::Fail {
                run_id: "run-fail".to_owned(),
                diagnostics: vec![GateDiagnostic {
                    case_id: "case-1".to_owned(),
                    reason: "assertion failed".to_owned(),
                }],
            }),
        };
        let result = plane
            .promote("default", "gated", 1, "op".to_string(), &evaluator)
            .await;
        assert!(
            matches!(result, Err(PromotionError::GateBlocked { run_id, .. }) if run_id == "run-fail")
        );
        // A failed promotion is recorded as Trial so the history shows the attempt.
        let history = plane.promotion_history("default", "gated").await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, SkillPromotionStatus::Trial);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn promotion_passing_gate_succeeds() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let package =
            SkillPackage::from_markdown(&gated_skill_md("gated", "Do something.", "suite-a"))
                .unwrap();
        plane
            .register("default", package, source(), "op".to_owned())
            .await
            .unwrap();

        let evaluator = ScriptedSkillGateEvaluator {
            result: Ok(GateEvaluationResult::Pass {
                run_id: "run-pass".to_owned(),
            }),
        };
        let promotion = plane
            .promote("default", "gated", 1, "op".to_string(), &evaluator)
            .await
            .unwrap();
        assert_eq!(promotion.status, SkillPromotionStatus::Promoted);
        assert_eq!(promotion.gate_run_id, Some("run-pass".to_owned()));

        // History reflects the promotion.
        let history = plane.promotion_history("default", "gated").await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, SkillPromotionStatus::Promoted);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn promotion_stale_hash_reuses_passing_run() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let package =
            SkillPackage::from_markdown(&gated_skill_md("gated", "Do something.", "suite-a"))
                .unwrap();
        plane
            .register("default", package, source(), "op".to_owned())
            .await
            .unwrap();

        let evaluator = ScriptedSkillGateEvaluator {
            result: Ok(GateEvaluationResult::Pass {
                run_id: "run-1".to_owned(),
            }),
        };
        let first = plane
            .promote("default", "gated", 1, "op".to_string(), &evaluator)
            .await
            .unwrap();
        assert_eq!(first.gate_run_id, Some("run-1".to_owned()));

        // Re-promote the same revision: should reuse without calling evaluator again.
        let evaluator_never_called = ScriptedSkillGateEvaluator {
            result: Err("should not be called".to_owned()),
        };
        let second = plane
            .promote(
                "default",
                "gated",
                1,
                "op".to_string(),
                &evaluator_never_called,
            )
            .await
            .unwrap();
        assert_eq!(second.gate_run_id, Some("run-1".to_owned()));
        assert_eq!(first.content_hash, second.content_hash);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn promotion_changed_hash_demands_new_evaluation() {
        let root = store_root();
        let plane = SkillPlane::load(&root);
        let pkg1 = SkillPackage::from_markdown(&gated_skill_md("gated", "Version one.", "suite-a"))
            .unwrap();
        plane
            .register("default", pkg1, source(), "op".to_owned())
            .await
            .unwrap();

        let evaluator1 = ScriptedSkillGateEvaluator {
            result: Ok(GateEvaluationResult::Pass {
                run_id: "run-1".to_owned(),
            }),
        };
        let first = plane
            .promote("default", "gated", 1, "op".to_string(), &evaluator1)
            .await
            .unwrap();

        // Register a new revision with different content.
        let pkg2 = SkillPackage::from_markdown(&gated_skill_md("gated", "Version two.", "suite-a"))
            .unwrap();
        plane
            .register("default", pkg2, source(), "op".to_owned())
            .await
            .unwrap();

        // Promote revision 2: new content hash means new evaluation required.
        let evaluator2 = ScriptedSkillGateEvaluator {
            result: Ok(GateEvaluationResult::Pass {
                run_id: "run-2".to_owned(),
            }),
        };
        let second = plane
            .promote("default", "gated", 2, "op".to_string(), &evaluator2)
            .await
            .unwrap();
        assert_ne!(first.content_hash, second.content_hash);
        assert_eq!(second.gate_run_id, Some("run-2".to_owned()));
        let _ = std::fs::remove_dir_all(root);
    }
}
