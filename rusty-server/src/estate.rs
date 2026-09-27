//! The estate: everything this deployment holds — agents, skills, threads,
//! runs, people, connections, memories, the sealed keys — as one archive,
//! taken while the server runs and restored onto an empty store.
//!
//! A backup is a gzipped tar of the store directory (`store/…`) behind a
//! `manifest.json` that says when it was taken, by whom, from which server
//! version, and what it holds. The store writes every record atomically
//! (temp file, then rename), so each file in the archive is whole; a run
//! in flight at that moment is restored the way a restart recovers it.
//! The archive holds the broker master key with the rest: it is as secret
//! as the store it copies.
//!
//! A restore fills an *empty* store at boot (`ServerConfig::restore_from`,
//! `RUSTY_RESTORE_FROM`): a store that already holds anything is never
//! overwritten — the request is recorded as skipped and the estate says so.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

const MANIFEST: &str = "manifest.json";
const STORE_PREFIX: &str = "store/";
const RESTORED_MARKER: &str = "restored-from.json";

/// What an estate holds, counted from the store's own layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Counts {
    pub agents: u64,
    pub skill_revisions: u64,
    pub threads: u64,
    pub runs: u64,
    pub people: u64,
    pub connections: u64,
    pub memories: u64,
    pub approvals: u64,
    /// People forgotten: their key destroyed, a tombstone in its place.
    #[serde(default)]
    pub people_forgotten: u64,
}

/// The first member of every backup archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub taken_at: DateTime<Utc>,
    pub by: Value,
    pub server_version: String,
    pub store_path: String,
    pub counts: Counts,
    pub files: u64,
    pub bytes: u64,
    /// Person keys carried apart, in the sibling person-keys archive.
    #[serde(default)]
    pub person_keys: u64,
}

/// One archive on disk, as the estate lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    /// Absent when the archive has no readable manifest (not one of ours).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    /// The sibling archive holding the person keys, when there were any:
    /// kept apart so a data archive is ciphertext for every person whose
    /// key was destroyed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_keys: Option<String>,
}

/// What a boot-time restore request came to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreOutcome {
    Restored {
        archive: String,
        manifest: Option<Manifest>,
        files: u64,
    },
    Skipped {
        archive: String,
        reason: String,
    },
    Failed {
        archive: String,
        reason: String,
    },
}

/// The estate's own handle on the store and where its backups go.
pub struct EstatePlane {
    pub store_path: PathBuf,
    pub backup_dir: PathBuf,
    /// This boot's restore request, if there was one.
    pub restore: Option<RestoreOutcome>,
}

impl EstatePlane {
    pub fn new(
        store_path: &Path,
        backup_dir: Option<&Path>,
        restore: Option<RestoreOutcome>,
    ) -> Self {
        let backup_dir = backup_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_backup_dir(store_path));
        Self {
            store_path: store_path.to_path_buf(),
            backup_dir,
            restore,
        }
    }
}

/// `{parent}/{store dir}-backups`, beside the store — never inside it.
pub fn default_backup_dir(store_path: &Path) -> PathBuf {
    let name = store_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("store");
    store_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{name}-backups"))
}

/// The `.json` files directly in `dir`, none from its subdirectories.
fn count_files_here(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let p = e.path();
            p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("json")
        })
        .count() as u64
}

fn count_files(dir: &Path) -> u64 {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += count_files(&path);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            n += 1;
        }
    }
    n
}

/// Count what the store holds from its layout (see `server_store.rs`).
pub fn counts(store_path: &Path) -> Counts {
    let people = std::fs::read(store_path.join("users.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .map(|v| match v {
            Value::Array(list) => list.len() as u64,
            Value::Object(map) => map
                .get("users")
                .and_then(Value::as_array)
                .map(|l| l.len() as u64)
                .unwrap_or(map.len() as u64),
            _ => 0,
        })
        .unwrap_or(0);
    Counts {
        agents: count_files(&store_path.join("assistants")),
        skill_revisions: count_files(&store_path.join("skills")),
        threads: count_files(&store_path.join("threads")),
        // The journals directory holds one file per run and, since heads were
        // kept (Story 92), a `heads/` directory beside them: only the files count.
        runs: count_files_here(&store_path.join("journals")),
        people,
        connections: count_files(&store_path.join("connections")),
        memories: count_files(&store_path.join("memory")),
        approvals: count_files(&store_path.join("approvals")),
        people_forgotten: std::fs::read_dir(crate::receipts::keys_dir(store_path))
            .map(|d| {
                d.flatten()
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        n.starts_with("person.") && n.ends_with(".forgotten.json")
                    })
                    .count() as u64
            })
            .unwrap_or(0),
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) != Some("tmp") {
            out.push(path);
        }
    }
}

fn archive_name(at: DateTime<Utc>) -> String {
    format!("estate-{}.tar.gz", at.format("%Y%m%dT%H%M%SZ"))
}

/// Take a backup of the store into the backup directory. Blocking: run it
/// on a blocking thread.
pub fn take_backup(store_path: &Path, backup_dir: &Path, by: Value) -> Result<Backup, String> {
    let taken_at = Utc::now();
    let mut all = Vec::new();
    walk(store_path, &mut all);
    all.sort();
    // Person keys travel apart: the data archive holds a person's records
    // sealed, and only the sibling person-keys archive can open them.
    let (person_keys, files): (Vec<PathBuf>, Vec<PathBuf>) = all
        .into_iter()
        .partition(|f| crate::vault::PersonVault::is_key_file(f));
    let bytes: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    let manifest = Manifest {
        taken_at,
        by,
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        store_path: store_path.display().to_string(),
        counts: counts(store_path),
        files: files.len() as u64,
        bytes,
        person_keys: person_keys.len() as u64,
    };
    std::fs::create_dir_all(backup_dir)
        .map_err(|e| format!("the backup directory could not be made: {e}"))?;
    // Two backups in one second must not be one file: the second takes a
    // numbered name.
    let mut name = archive_name(taken_at);
    let mut nth = 1;
    while backup_dir.join(&name).exists() {
        nth += 1;
        name = format!(
            "{}.{nth}.tar.gz",
            archive_name(taken_at).trim_end_matches(".tar.gz")
        );
    }
    let path = backup_dir.join(&name);
    let tmp = backup_dir.join(format!("{name}.tmp"));
    {
        let file =
            File::create(&tmp).map_err(|e| format!("the archive could not be created: {e}"))?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o600);
        header.set_mtime(taken_at.timestamp() as u64);
        header.set_cksum();
        tar.append_data(&mut header, MANIFEST, manifest_bytes.as_slice())
            .map_err(|e| format!("manifest: {e}"))?;
        for file in &files {
            let Ok(relative) = file.strip_prefix(store_path) else {
                continue;
            };
            // A file that vanished since the walk (a rename in flight) is
            // simply not in this backup; the record it was replacing is.
            let Ok(mut handle) = File::open(file) else {
                continue;
            };
            let name = Path::new(STORE_PREFIX).join(relative);
            if let Err(error) = tar.append_file(&name, &mut handle) {
                return Err(format!("{}: {error}", relative.display()));
            }
        }
        let encoder = tar
            .into_inner()
            .map_err(|e| format!("the archive could not be finished: {e}"))?;
        let mut file = encoder
            .finish()
            .map_err(|e| format!("the archive could not be compressed: {e}"))?;
        file.flush().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("the archive could not be kept: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut keys_archive = None;
    if !person_keys.is_empty() {
        let keys_name = person_keys_name(&name);
        let keys_path = backup_dir.join(&keys_name);
        let tmp = backup_dir.join(format!("{keys_name}.tmp"));
        {
            let file = File::create(&tmp)
                .map_err(|e| format!("the person-keys archive could not be created: {e}"))?;
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(encoder);
            for file in &person_keys {
                let Ok(relative) = file.strip_prefix(store_path) else {
                    continue;
                };
                let Ok(mut handle) = File::open(file) else {
                    continue;
                };
                tar.append_file(Path::new(STORE_PREFIX).join(relative), &mut handle)
                    .map_err(|e| format!("{}: {e}", relative.display()))?;
            }
            let encoder = tar.into_inner().map_err(|e| e.to_string())?;
            let mut file = encoder.finish().map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &keys_path)
            .map_err(|e| format!("the person-keys archive could not be kept: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&keys_path, std::fs::Permissions::from_mode(0o600));
        }
        keys_archive = Some(keys_name);
    }
    Ok(Backup {
        name,
        path: path.display().to_string(),
        bytes,
        manifest: Some(manifest),
        person_keys: keys_archive,
    })
}

/// The person-keys archive that goes with a data archive:
/// `estate-….tar.gz` → `estate-…-person-keys.tar.gz`.
pub fn person_keys_name(archive_name: &str) -> String {
    format!(
        "{}-person-keys.tar.gz",
        archive_name.trim_end_matches(".tar.gz")
    )
}

fn read_manifest(path: &Path) -> Option<Manifest> {
    let file = File::open(path).ok()?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive.entries().ok()?;
    let mut first = entries.next()?.ok()?;
    if first.path().ok()?.to_str() != Some(MANIFEST) {
        return None;
    }
    let mut bytes = Vec::new();
    first.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Every `estate-*.tar.gz` in the backup directory, newest first.
pub fn list_backups(backup_dir: &Path) -> Vec<Backup> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(backup_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("estate-")
            || !name.ends_with(".tar.gz")
            || name.ends_with("-person-keys.tar.gz")
        {
            continue;
        }
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let keys = person_keys_name(&name);
        let person_keys = backup_dir.join(&keys).exists().then_some(keys);
        out.push(Backup {
            manifest: read_manifest(&path),
            name,
            path: path.display().to_string(),
            bytes,
            person_keys,
        });
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

fn store_is_empty(store_path: &Path) -> bool {
    match std::fs::read_dir(store_path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// Unpack `archive` into `store_path`. Only members under `store/` land,
/// each checked against the store root (no parent components).
fn unpack(archive: &Path, store_path: &Path) -> Result<(Option<Manifest>, u64), String> {
    let file = File::open(archive).map_err(|e| format!("the archive could not be opened: {e}"))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let mut manifest = None;
    let mut files = 0;
    std::fs::create_dir_all(store_path).map_err(|e| format!("the store could not be made: {e}"))?;
    for entry in tar
        .entries()
        .map_err(|e| format!("the archive does not read: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("a member does not read: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if path.to_str() == Some(MANIFEST) {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            manifest = serde_json::from_slice(&bytes).ok();
            continue;
        }
        let Ok(relative) = path.strip_prefix(STORE_PREFIX) else {
            continue;
        };
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(format!(
                "the archive names a path outside the store: {}",
                path.display()
            ));
        }
        let dest = store_path.join(relative);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry
            .unpack(&dest)
            .map_err(|e| format!("{}: {e}", relative.display()))?;
        files += 1;
    }
    Ok((manifest, files))
}

/// At boot, before the store loads: fill an empty store from the archive
/// the configuration names, and record what happened.
pub fn restore_if_asked(store_path: &Path, archive: Option<&Path>) -> Option<RestoreOutcome> {
    let archive = archive?;
    let name = archive.display().to_string();
    if !store_is_empty(store_path) {
        let reason = format!("the store at {} is not empty; a restore only fills an empty store — move the current one aside first", store_path.display());
        tracing::error!(archive = %name, %reason, "restore skipped");
        return Some(RestoreOutcome::Skipped {
            archive: name,
            reason,
        });
    }
    match unpack(archive, store_path) {
        Ok((manifest, mut files)) => {
            // The person keys, when their archive is beside the data one;
            // without it every person's records read as absent.
            let mut person_keys = None;
            if let Some(file_name) = archive.file_name().and_then(|n| n.to_str()) {
                let keys_path = archive.with_file_name(person_keys_name(file_name));
                if keys_path.exists() {
                    match unpack(&keys_path, store_path) {
                        Ok((_, n)) => {
                            files += n;
                            person_keys = Some(
                                json!({"archive": keys_path.display().to_string(), "keys": n}),
                            );
                        }
                        Err(reason) => {
                            tracing::error!(archive = %keys_path.display(), %reason, "person keys not restored")
                        }
                    }
                }
            }
            let marker = json!({"archive": name, "at": Utc::now(), "manifest": manifest, "files": files, "person_keys": person_keys});
            if let Ok(bytes) = serde_json::to_vec_pretty(&marker) {
                let _ = std::fs::write(store_path.join(RESTORED_MARKER), bytes);
            }
            tracing::info!(archive = %name, files, "estate restored from a backup");
            Some(RestoreOutcome::Restored {
                archive: name,
                manifest,
                files,
            })
        }
        Err(reason) => {
            tracing::error!(archive = %name, %reason, "restore failed");
            Some(RestoreOutcome::Failed {
                archive: name,
                reason,
            })
        }
    }
}

/// The marker the last restore left, when this store was filled from a
/// backup: the archive, when, and how long it took.
pub fn restored(store_path: &Path) -> Option<Value> {
    restored_from(store_path)
}

fn restored_from(store_path: &Path) -> Option<Value> {
    std::fs::read(store_path.join(RESTORED_MARKER))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// `GET /estate` — what this deployment holds, where its backups go, the
/// backups there, and whether this store came from one.
pub(crate) async fn get_estate(
    AxumState(state): AxumState<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let forgotten = state.vault.forgotten();
    let store_path = state.estate.store_path.clone();
    let backup_dir = state.estate.backup_dir.clone();
    let (counts, backups, restored, bytes) = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        walk(&store_path, &mut files);
        let bytes: u64 = files
            .iter()
            .filter_map(|f| std::fs::metadata(f).ok())
            .map(|m| m.len())
            .sum();
        (
            counts(&store_path),
            list_backups(&backup_dir),
            restored_from(&store_path),
            bytes,
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("estate: {e}")))?;
    Ok(Json(json!({
        "forgotten": forgotten,
        "store": {"kind": "files", "path": state.estate.store_path.display().to_string(), "bytes": bytes},
        "counts": counts,
        "backups_dir": state.estate.backup_dir.display().to_string(),
        "backups": backups,
        "restored_from": restored,
        "restore": state.estate.restore,
        "server_version": env!("CARGO_PKG_VERSION"),
    })))
}

/// `POST /estate/backups` — take a backup now, as the signed-in person.
pub(crate) async fn post_backup(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<(StatusCode, Json<Backup>), ApiError> {
    let store_path = state.estate.store_path.clone();
    let backup_dir = state.estate.backup_dir.clone();
    let by = tenant.attribution();
    let backup = tokio::task::spawn_blocking(move || take_backup(&store_path, &backup_dir, by))
        .await
        .map_err(|e| ApiError::internal(format!("backup: {e}")))?
        .map_err(ApiError::internal)?;
    tracing::info!(archive = %backup.path, bytes = backup.bytes, "estate backed up");
    Ok((StatusCode::CREATED, Json(backup)))
}

/// `GET /estate/roster?days=7` — the estate over days: what each day
/// wrote to the journals (files, bytes) and to memory (notes), how many
/// runs it finished, and how late each schedule fired against its
/// interval. The numbers an operator watches for growth and drift, read
/// from what the store already holds; nothing is sampled or estimated.
#[derive(Debug, Deserialize)]
pub(crate) struct RosterQuery {
    pub days: Option<u32>,
}

pub(crate) async fn get_roster(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    axum::extract::Query(query): axum::extract::Query<RosterQuery>,
) -> Result<Json<Value>, ApiError> {
    use std::collections::BTreeMap;
    let days = query.days.unwrap_or(7).clamp(1, 60) as i64;
    let now = Utc::now();
    let since = now - chrono::Duration::days(days);
    let day_of = |at: DateTime<Utc>| at.date_naive().to_string();
    // Every day in the window, oldest first, so a quiet day reads as zero.
    let mut by_day: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for d in (0..days).rev() {
        by_day.insert(day_of(now - chrono::Duration::days(d)), (0, 0, 0, 0));
    }
    // Journals: one file per run (and its payload spills), by write day.
    let journals_dir = state.estate.store_path.join("journals");
    let sizes = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        walk(&journals_dir, &mut files);
        let mut out: Vec<(DateTime<Utc>, u64)> = Vec::new();
        let mut total: u64 = 0;
        for f in files {
            if let Ok(meta) = std::fs::metadata(&f) {
                total += meta.len();
                if let Ok(modified) = meta.modified() {
                    out.push((DateTime::<Utc>::from(modified), meta.len()));
                }
            }
        }
        (out, total)
    })
    .await
    .map_err(|e| ApiError::internal(format!("roster: {e}")))?;
    let (journal_files, journal_bytes_total) = sizes;
    for (at, bytes) in &journal_files {
        if let Some(day) = by_day.get_mut(&day_of(*at)) {
            day.0 += 1;
            day.1 += bytes;
        }
    }
    // Memory: notes by the day they were written.
    let universe = crate::routes::memory_universe(&state, &tenant).await?;
    for r in &universe {
        if let Some(day) = by_day.get_mut(&day_of(r.created_at)) {
            day.2 += 1;
        }
    }
    // Runs: finished by day, and each schedule's firings for the drift.
    let recalled = crate::routes::recall_runs_for(&state, &tenant, 500, None).await?;
    let mut firings: BTreeMap<String, Vec<DateTime<Utc>>> = BTreeMap::new();
    for run in recalled
        .into_iter()
        .map(crate::routes::RecalledRun::into_wire)
    {
        let Some(at) = run
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|t| t.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        if at < since {
            continue;
        }
        if let Some(day) = by_day.get_mut(&day_of(at)) {
            day.3 += 1;
        }
        if let Some(cron_id) = run.pointer("/metadata/cron_id").and_then(Value::as_str) {
            firings.entry(cron_id.to_owned()).or_default().push(at);
        }
    }
    let crons = state
        .server_store
        .list_crons()
        .await
        .map_err(ApiError::internal)?;
    let mut schedules: Vec<Value> = Vec::new();
    for cron in crons.iter().filter(|c| c.assistant_id.is_some()) {
        let external = tenant
            .unscope(&cron.cron_id)
            .unwrap_or(&cron.cron_id)
            .to_owned();
        let mut fired: Vec<DateTime<Utc>> = firings
            .get(&external)
            .cloned()
            .or_else(|| firings.get(&cron.cron_id).cloned())
            .unwrap_or_default();
        fired.sort();
        let interval = cron.interval_secs;
        // Lateness: each gap between firings, past the interval; a cron
        // expression has no fixed interval, so its drift is not measured.
        let mut late: Vec<i64> = Vec::new();
        if let Some(every) = interval {
            for pair in fired.windows(2) {
                let gap = (pair[1] - pair[0]).num_seconds();
                late.push((gap - every as i64).max(0));
            }
        }
        late.sort();
        let agent = cron
            .assistant_id
            .as_deref()
            .map(|id| tenant.unscope(id).unwrap_or(id).to_owned());
        schedules.push(json!({
            "cron_id": external,
            "assistant_id": agent,
            "interval_secs": interval,
            "cron_expr": cron.cron_expr,
            "runs_fired": cron.runs_fired,
            "last_run_at": cron.last_run_at,
            "fired_in_window": fired.len(),
            "late_max_secs": late.last().copied(),
            "late_median_secs": if late.is_empty() { None } else { Some(late[late.len() / 2]) },
            "drift": if interval.is_none() { "not measured for a cron expression" } else if late.is_empty() { "fewer than two firings in the window" } else if late.last().copied().unwrap_or(0) <= 60 { "on time" } else { "late" },
        }));
    }
    let days_out: Vec<Value> = by_day.iter().map(|(day, (files, bytes, notes, runs))| json!({"day": day, "journal_files": files, "journal_bytes": bytes, "notes": notes, "runs": runs})).collect();
    Ok(Json(json!({
        "days": days,
        "since": since,
        "by_day": days_out,
        "journal_bytes_total": journal_bytes_total,
        "notes_total": universe.len(),
        "schedules": schedules,
        "note": "journals by the day their files were last written; notes by the day they were written; runs by the day they started, the newest 500; a schedule's lateness is each gap between its firings past its interval",
    })))
}
