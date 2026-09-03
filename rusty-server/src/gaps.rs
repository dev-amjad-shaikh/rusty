//! Gap-ledger persistence (demand-side learning, wave 2): the file
//! layout behind the gap-ledger store backends.
//!
//! One snapshot per tenant under `{store_path}/gaps/` (`gaps` is a
//! reserved layout name, see [`crate::RESERVED_NAMES`]): the default
//! tenant's ledger is `gaps/ledger.json`, a named tenant's is
//! `gaps/{tenant}/ledger.json` — the memory layout's path-keyed
//! tenancy rule, adapted to one file per tenant because the ledger is
//! a single versioned snapshot (core's
//! [`GapLedger::to_snapshot`](rusty_agent_runtime::gaps::GapLedger::to_snapshot)),
//! not a record collection. Writes are atomic (temp file + rename, the
//! durability discipline every file record in the server shares); loads
//! skip unparseable files with a warning — one corrupt ledger must not
//! take the plane down at boot, and the mutation chains inside a
//! healthy snapshot are the evidence an operator restores from.
//!
//! Postgres keeps the same snapshot-per-tenant shape column-mapped
//! (`server_gap_ledgers`: tenant primary key, snapshot JSONB,
//! `updated_at`). The ledger mutates as a whole under the route's
//! per-tenant lock, so neither backend ever merges — it stores exactly
//! what the lock serialized, and a crash between mutation and persist
//! replays from the last durable snapshot, never from a torn one.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::gaps::GapLedger;

/// The gap-ledger directory under the store root.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("gaps")
}

/// The tenant's snapshot file (`gaps/ledger.json` for the default
/// tenant, `gaps/{tenant}/ledger.json` for named tenants).
fn ledger_path(root: &Path, tenant: &str, default_tenant: &str) -> PathBuf {
    if tenant == default_tenant {
        dir(root).join("ledger.json")
    } else {
        dir(root).join(tenant).join("ledger.json")
    }
}

/// Persist one tenant's ledger snapshot atomically (temp file +
/// rename): a crash mid-write must never leave a truncated snapshot
/// behind — the last durable snapshot is always whole.
pub(crate) async fn persist(
    root: &Path,
    tenant: &str,
    default_tenant: &str,
    ledger: &GapLedger,
) -> io::Result<()> {
    let path = ledger_path(root, tenant, default_tenant);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(ledger)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Recursively collect `ledger.json` files under `root` (tenant
/// subdirectories hold that tenant's snapshot), mirroring the memory
/// loader's walk.
fn collect_ledger_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_ledger_files(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("ledger.json") {
            out.push(path);
        }
    }
}

/// Load all tenant ledgers under `dir`, keyed by tenant (the default
/// tenant's key is `default_tenant`, derived from the path — the
/// snapshot body is tenant-neutral, the same rule the memory loader
/// applies to content addresses). Files that fail to parse are skipped
/// with a warning (the corrupt-tolerance rule every loader here
/// shares).
pub(crate) fn load_ledgers(root: &Path, default_tenant: &str) -> HashMap<String, GapLedger> {
    let dir = dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_ledger_files(&dir, &mut files);
    for path in files {
        let tenant = path
            .parent()
            .and_then(|parent| parent.strip_prefix(&dir).ok())
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .filter(|relative| !relative.is_empty())
            .unwrap_or_else(|| default_tenant.to_string());
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|body| serde_json::from_str::<GapLedger>(&body).map_err(|e| e.to_string()));
        match parsed {
            Ok(ledger) => {
                out.insert(tenant, ledger);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unparseable gap-ledger snapshot");
            }
        }
    }
    out
}
