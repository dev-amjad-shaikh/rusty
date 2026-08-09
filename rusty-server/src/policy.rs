//! The executor policy plane (R0.8 Rusty Learn, wave 4): the registry's
//! serde types, the JSON-file layout, epoch derivation, and the admission
//! binding decorator.
//!
//! Three pieces:
//!
//! - **Registry types** ([`PolicyRecord`], [`PolicyActivation`],
//!   [`PolicyBinding`]) — server-side records, not frozen core contracts:
//!   the golden-pinned half of the plane (`ExecutorPolicy`,
//!   `derive_policy_version`, `DecisionEvent`) lives in
//!   `rusty_agent_runtime::record`. The registry stores immutable policy
//!   bodies under content-derived versions; promotions and rollbacks move
//!   *activations*, never edit bodies.
//! - **The file layout** — three directories under `{store_path}/policy/`
//!   (`policy` is a reserved layout name, see [`crate::RESERVED_NAMES`]),
//!   one JSON file per record, temp-write-then-rename, path-keyed tenancy
//!   (`{tenant}/{name}.json` for named tenants, bare names for the default
//!   tenant) — the `learn` layout's discipline applied to a third entity
//!   set. Postgres keeps the same entities column-mapped
//!   (`server_policies` / `server_policy_activations` /
//!   `server_policy_bindings`).
//! - **The admission decorator** ([`PolicyBindingCheckpointer`]) — the seam
//!   where runs bind the active policy version. The executor stamps every
//!   checkpoint header with its `RunConfig`'s policy version (the static
//!   floor when unpinned), and the run-construction path is not the policy
//!   plane's to change; the checkpointer `put` is the one funnel every
//!   checkpoint of every run passes through, so the binding happens there.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::checkpoint::{Checkpoint, Checkpointer};
use rusty_agent_runtime::error::{Result as RuntimeResult, RustyError};
use rusty_agent_runtime::record::{ExecutorPolicy, PolicyVersion};
use serde::{Deserialize, Serialize};

use crate::server_store::{ServerStore, StoreResult};

/// Where a registered policy body came from. Provenance, not identity —
/// the version (a content address) is the identity; the source answers
/// "who put this parameter set in front of traffic."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub(crate) enum PolicySource {
    /// Registered directly through `POST /policy/versions` (an operator's
    /// hand-authored parameter set).
    Api,
    /// Derived from a promoted policy candidate: the promotion hook
    /// overlaid the candidate's family parameters onto the policy that was
    /// active and registered the result under its derived version.
    Candidate {
        /// The candidate whose promotion produced this body.
        candidate_id: String,
    },
}

/// One immutable policy body in the registry. Immutability is enforced at
/// the write seam ([`crate::server_store::ServerStore::put_policy`]): the
/// same version with the same body converges (idempotent create), the same
/// version with a different body conflicts — a version string is a
/// commitment to one exact parameter set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PolicyRecord {
    /// The version this body is registered under (`policy-{hash12}` for
    /// derived bodies; operator-chosen names are accepted for the API
    /// source, validated path-safe).
    pub version: PolicyVersion,

    /// The parameter bundle.
    pub policy: ExecutorPolicy,

    /// Where the body came from.
    pub source: PolicySource,

    /// When the body was first registered (a converged re-registration
    /// keeps the original instant — the record is immutable).
    pub registered_at: DateTime<Utc>,
}

/// The synthetic record for the static floor. The floor is never
/// registered — it predates the registry and is always resolvable — so
/// reads and activations that name it synthesize this record on demand.
pub(crate) fn static_floor_record() -> PolicyRecord {
    PolicyRecord {
        version: PolicyVersion::default(),
        policy: ExecutorPolicy::static_v0(),
        source: PolicySource::Api,
        registered_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_else(Utc::now),
    }
}

/// One move of the active-version pointer: "from `activated_at` on, new
/// runs bind `version`." Append-only — the active version is the last
/// activation, and the full list is the registry's epoch history.
/// Activating the static floor is always legal (the floor needs no
/// registration); that is how a deployment reverts to pre-learning
/// behavior without a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PolicyActivation {
    /// The version that became active.
    pub version: PolicyVersion,

    /// When the move happened.
    pub activated_at: DateTime<Utc>,
}

/// The evidence that one checkpoint bound one policy version at admission.
/// Denormalized: the checkpoint header itself is the authoritative record
/// (every checkpoint carries its pinned version); the binding index exists
/// so epoch listing can answer "which runs bound this version" without a
/// checkpoint scan. A lost binding row is therefore recomputable, and its
/// write is best-effort — the header stamp is not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PolicyBinding {
    /// The checkpoint whose header was stamped.
    pub checkpoint_id: String,

    /// The (internal, tenant-scoped) thread the run belongs to.
    pub thread_id: String,

    /// The version bound at admission.
    pub version: PolicyVersion,

    /// When the binding happened.
    pub bound_at: DateTime<Utc>,
}

/// The result of a policy registration
/// ([`ServerStore::put_policy`](crate::server_store::ServerStore::put_policy)):
/// created, converged (same body re-registered — the idempotent create),
/// or conflicted (same version, different body — immutability refuses the
/// overwrite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyWrite {
    /// The version is new; the body is stored.
    Created,
    /// The version already named exactly this body; nothing changed.
    Converged,
    /// The version already names a different body (or the reserved floor
    /// version). Refused — immutability is what makes a version string a
    /// commitment.
    Conflict,
}

/// One epoch of the registry's history, derived for `GET /policy/epochs`:
/// a version's reign — from its activation to the next activation (or
/// `None`, still active) — plus the bindings recorded inside the window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PolicyEpoch {
    /// The version that served during the epoch.
    pub version: PolicyVersion,

    /// When the version became active.
    pub activated_at: DateTime<Utc>,

    /// When the next activation retired it (`None` while it serves).
    pub retired_at: Option<DateTime<Utc>>,

    /// The admission bindings recorded inside the window, oldest first.
    pub bindings: Vec<PolicyBinding>,
}

/// Fold the activation log and the binding index into the epoch listing.
/// `activations` must be ordered oldest first (the store contract). An
/// activation that renames the currently active version (a no-op move)
/// still opens a new epoch — the log is the history as it was written.
/// Bindings recorded before the first activation belong to the implicit
/// floor epoch, which is listed when such bindings exist; with no
/// activations at all the registry has never moved and the listing is
/// empty (every run binds the floor — there is nothing to list).
pub(crate) fn derive_epochs(
    activations: Vec<PolicyActivation>,
    mut bindings: Vec<PolicyBinding>,
) -> Vec<PolicyEpoch> {
    bindings.sort_by(|a, b| {
        a.bound_at
            .cmp(&b.bound_at)
            .then_with(|| a.checkpoint_id.cmp(&b.checkpoint_id))
    });
    let mut epochs = Vec::new();
    if let Some(first) = activations.first() {
        let floor_bindings: Vec<PolicyBinding> = bindings
            .iter()
            .filter(|binding| binding.bound_at < first.activated_at)
            .cloned()
            .collect();
        if !floor_bindings.is_empty() {
            epochs.push(PolicyEpoch {
                version: PolicyVersion::default(),
                activated_at: floor_bindings[0].bound_at,
                retired_at: Some(first.activated_at),
                bindings: floor_bindings,
            });
        }
    }
    for (index, activation) in activations.iter().enumerate() {
        let retired_at = activations.get(index + 1).map(|next| next.activated_at);
        let window: Vec<PolicyBinding> = bindings
            .iter()
            .filter(|binding| {
                binding.bound_at >= activation.activated_at
                    && retired_at.is_none_or(|retired| binding.bound_at < retired)
            })
            .cloned()
            .collect();
        epochs.push(PolicyEpoch {
            version: activation.version.clone(),
            activated_at: activation.activated_at,
            retired_at,
            bindings: window,
        });
    }
    epochs
}

/// `true` when `version` is a valid registry version string: path-safe
/// (it becomes a filename segment) and not the reserved floor name. The
/// derived shape (`policy-{hex12}`) satisfies this by construction; the
/// rule exists for API-chosen names.
pub(crate) fn validate_policy_version(version: &str) -> Result<(), String> {
    let valid = !version.is_empty()
        && version.len() <= 128
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid {
        return Err(format!(
            "invalid policy version `{version}` (allowed: [A-Za-z0-9._-], 1..=128 chars)"
        ));
    }
    if version == PolicyVersion::STATIC_V0 {
        return Err(format!(
            "policy version `{version}` is reserved: the static floor predates the registry \
             and is always resolvable — it is never registered"
        ));
    }
    Ok(())
}

/// Resolve the tenant's currently active policy as a full record: the
/// last activation's registered body, or the synthetic floor record when
/// the registry never moved (or moved back to the floor). `Err` when the
/// active version is not registered — an activations-only registry
/// corruption the serving path must refuse to guess around.
pub(crate) async fn active_policy_record(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
) -> StoreResult<PolicyRecord> {
    let activations = store.list_policy_activations(tenant).await?;
    let Some(active) = activations.last() else {
        return Ok(static_floor_record());
    };
    if active.version.as_str() == PolicyVersion::STATIC_V0 {
        return Ok(static_floor_record());
    }
    store
        .get_policy(tenant, active.version.as_str())
        .await?
        .ok_or_else(|| {
            format!(
                "active policy version `{}` is not registered — the registry is corrupt",
                active.version.as_str()
            )
        })
}

// --------------------------------------------------------------------- //
// The JSON-file layout
// --------------------------------------------------------------------- //

/// The registry directory under the store root (`{store_path}/policy`).
fn policy_dir(root: &Path) -> PathBuf {
    root.join("policy")
}

/// The policy-body directory (`{store_path}/policy/versions`).
pub(crate) fn policies_dir(root: &Path) -> PathBuf {
    policy_dir(root).join("versions")
}

/// The activation-log directory (`{store_path}/policy/activations`).
pub(crate) fn activations_dir(root: &Path) -> PathBuf {
    policy_dir(root).join("activations")
}

/// The binding-index directory (`{store_path}/policy/bindings`).
pub(crate) fn bindings_dir(root: &Path) -> PathBuf {
    policy_dir(root).join("bindings")
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records) — the `learn` loader's walk.
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

/// The path-derived scoped name of a record file under `dir`
/// (`{tenant}/{name}` for named tenants, the bare name for the default
/// tenant) — the path-keyed tenancy rule every layout here shares.
fn path_scoped_name(dir: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(dir)
        .ok()
        .map(|relative| relative.with_extension(""))
        .map(|relative| {
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
}

/// Persist one JSON record atomically (temp file + rename) under `dir`,
/// named by `scoped_name` — the durability discipline every file record
/// in the server shares.
async fn persist_record<T: Serialize>(
    dir: &Path,
    scoped_name: &str,
    record: &T,
    what: &str,
) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{scoped_name}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dir.join(format!("{scoped_name}.{what}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Persist one policy record, named by its tenant-scoped version
/// (`{tenant}/{version}.json` — versions are validated path-safe).
pub(crate) async fn persist_policy(
    root: &Path,
    scoped_version: &str,
    record: &PolicyRecord,
) -> io::Result<()> {
    persist_record(&policies_dir(root), scoped_version, record, "policy").await
}

/// Load all policy records under `policies_dir`, keyed by their
/// path-derived scoped version. Files that fail to parse are skipped with
/// a warning (the corrupt-tolerance rule every loader here shares).
pub(crate) fn load_policies(root: &Path) -> HashMap<String, PolicyRecord> {
    let dir = policies_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped = path_scoped_name(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<PolicyRecord>(&raw).ok());
        match (scoped, parsed) {
            (Some(name), Some(record)) => {
                out.insert(name, record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable policy file")
            }
        }
    }
    out
}

/// The activation file name: millisecond timestamp (zero-padded for
/// lexicographic order) plus the version, so a directory listing replays
/// the log in order and same-millisecond activations order by version.
pub(crate) fn activation_file_name(activation: &PolicyActivation) -> String {
    format!(
        "{:013}-{}",
        activation.activated_at.timestamp_millis(),
        activation.version.as_str()
    )
}

/// Append one activation to the tenant's log. Append-only by construction:
/// each activation is a new file, never an edit.
pub(crate) async fn append_activation(
    root: &Path,
    tenant: &str,
    activation: &PolicyActivation,
) -> io::Result<()> {
    let scoped = crate::auth::scope_id(tenant, &activation_file_name(activation));
    persist_record(&activations_dir(root), &scoped, activation, "activation").await
}

/// Load every tenant's activation log, keyed by the path-derived scoped
/// file name. Order is *not* derived from the key here — consumers sort
/// by `activated_at` (the file body), the same value the filename
/// approximates; a hand-edited filename cannot reorder history.
pub(crate) fn load_activations(root: &Path) -> HashMap<String, PolicyActivation> {
    let dir = activations_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped = path_scoped_name(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<PolicyActivation>(&raw).ok());
        match (scoped, parsed) {
            (Some(name), Some(activation)) => {
                out.insert(name, activation);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable activation file")
            }
        }
    }
    out
}

/// Record one admission binding, named by the tenant-scoped checkpoint id
/// (`{tenant}/{checkpoint_id}.json` — checkpoint ids are UUIDs, already
/// path-safe).
pub(crate) async fn persist_binding(
    root: &Path,
    tenant: &str,
    binding: &PolicyBinding,
) -> io::Result<()> {
    let scoped = crate::auth::scope_id(tenant, &binding.checkpoint_id);
    persist_record(&bindings_dir(root), &scoped, binding, "binding").await
}

/// Load all recorded bindings, keyed by their path-derived scoped
/// checkpoint id.
pub(crate) fn load_bindings(root: &Path) -> HashMap<String, PolicyBinding> {
    let dir = bindings_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let scoped = path_scoped_name(&dir, &path);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<PolicyBinding>(&raw).ok());
        match (scoped, parsed) {
            (Some(name), Some(binding)) => {
                out.insert(name, binding);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable binding file")
            }
        }
    }
    out
}

// --------------------------------------------------------------------- //
// The admission binding decorator
// --------------------------------------------------------------------- //

/// A [`Checkpointer`] decorator that binds the registry's active policy
/// version into checkpoint headers at admission.
///
/// Why this seam: the executor stamps every checkpoint header with its
/// `RunConfig`'s policy version (the static floor when unpinned), and the
/// server's run construction is outside this wave's file scope — but every
/// checkpoint of every run funnels through `Checkpointer::put`, so the
/// binding happens here, exactly once per run:
///
/// - **A checkpoint with a non-floor header passes through untouched.** The
///   version is pinned — by an explicit `RunConfig` pin, by the executor's
///   resume inheritance (a resumed run keeps the version its checkpoint
///   header pins), or by an earlier binding on this timeline.
/// - **A floor header on a thread with no history is admission.** The
///   decorator resolves the registry's active version for the thread's
///   tenant, stamps it (which may be the floor itself — "bound to the
///   floor" is evidence too), and records a [`PolicyBinding`].
/// - **A floor header on a thread with history inherits the timeline's
///   pin** — the in-flight rule: a mid-run promotion never changes
///   behavior under an execution already bound. The one exception: a
///   step-0 checkpoint over a thread whose history is strictly later
///   (every prior checkpoint has `step > 0`) is a *new* run on an old
///   thread, not a continuation — it binds at admission like a fresh
///   thread.
///
/// Honest edges, both stemming from the checkpoint carrying no run
/// identity: a step-0 floor-pinned checkpoint on a thread whose latest
/// checkpoint is also step 0 (a one-boundary history) is indistinguishable
/// from a resumed run's first write, so it inherits rather than re-binds;
/// and time-travel resume from a floor-pinned checkpoint on a
/// later-history thread binds the now-active version (a new run, by the
/// admission rule) rather than the historical floor. Both are documented
/// behavior, not accidents: when the evidence cannot separate "continue
/// the timeline" from "start a new one", the decorator continues.
pub(crate) struct PolicyBindingCheckpointer {
    inner: Arc<dyn Checkpointer>,
    store: Arc<dyn ServerStore>,
}

impl PolicyBindingCheckpointer {
    /// Wrap `inner`, resolving active versions and recording bindings
    /// through `store`.
    pub(crate) fn new(inner: Arc<dyn Checkpointer>, store: Arc<dyn ServerStore>) -> Self {
        Self { inner, store }
    }

    /// The registry's active version for `tenant`: the last activation, or
    /// the static floor when the registry never moved.
    async fn active_version(&self, tenant: &str) -> StoreResult<PolicyVersion> {
        let activations = self.store.list_policy_activations(tenant).await?;
        Ok(activations
            .last()
            .map(|activation| activation.version.clone())
            .unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl Checkpointer for PolicyBindingCheckpointer {
    async fn put(&self, mut checkpoint: Checkpoint) -> RuntimeResult<()> {
        if checkpoint.header.policy_version.as_str() == PolicyVersion::STATIC_V0 {
            let latest = self.inner.get_latest(&checkpoint.thread_id).await?;
            let admission = match &latest {
                None => true,
                // A step-0 checkpoint over a strictly later history is a new
                // run on an old thread; anything else continues the timeline.
                Some(prev) => checkpoint.step == 0 && prev.step > 0,
            };
            if admission {
                let tenant = crate::auth::tenant_of_internal(&checkpoint.thread_id);
                let version = self.active_version(tenant).await.map_err(|e| {
                    RustyError::Checkpoint(format!(
                        "resolve the active policy version for admission binding: {e}"
                    ))
                })?;
                checkpoint.header.policy_version = version.clone();
                // The binding index is denormalized evidence (the header is
                // authoritative), so its write is best-effort: a lost row is
                // recomputable from the checkpoint, a failed run is not.
                let binding = PolicyBinding {
                    checkpoint_id: checkpoint.id.clone(),
                    thread_id: checkpoint.thread_id.clone(),
                    version,
                    bound_at: Utc::now(),
                };
                if let Err(error) = self.store.put_policy_binding(tenant, &binding).await {
                    tracing::warn!(
                        checkpoint_id = %binding.checkpoint_id,
                        %error,
                        "policy binding evidence lost; the checkpoint header remains authoritative"
                    );
                }
            } else if let Some(prev) = latest {
                // Timeline continuity: inherit the pin the thread's history
                // carries (the in-flight rule).
                checkpoint.header.policy_version = prev.header.policy_version;
            }
        }
        self.inner.put(checkpoint).await
    }

    async fn get_latest(&self, thread_id: &str) -> RuntimeResult<Option<Checkpoint>> {
        self.inner.get_latest(thread_id).await
    }

    async fn list(&self, thread_id: &str) -> RuntimeResult<Vec<Checkpoint>> {
        self.inner.list(thread_id).await
    }

    async fn get_by_id(
        &self,
        thread_id: &str,
        checkpoint_id: &str,
    ) -> RuntimeResult<Option<Checkpoint>> {
        self.inner.get_by_id(thread_id, checkpoint_id).await
    }

    async fn fork_thread(
        &self,
        src_thread: &str,
        dst_thread: &str,
        at_checkpoint_id: Option<&str>,
    ) -> RuntimeResult<usize> {
        // Fork copies history byte-exactly, pins included — the default
        // implementation would route each copy through this decorator's
        // `put` and re-bind the fork's floor-pinned checkpoints, so the
        // fork forwards to the inner backend directly.
        self.inner
            .fork_thread(src_thread, dst_thread, at_checkpoint_id)
            .await
    }
}

impl std::fmt::Debug for PolicyBindingCheckpointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyBindingCheckpointer").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn activation(version: &str, millis: i64) -> PolicyActivation {
        PolicyActivation {
            version: PolicyVersion::new(version),
            activated_at: ts(millis),
        }
    }

    fn binding(checkpoint: &str, version: &str, millis: i64) -> PolicyBinding {
        PolicyBinding {
            checkpoint_id: checkpoint.into(),
            thread_id: "t-1".into(),
            version: PolicyVersion::new(version),
            bound_at: ts(millis),
        }
    }

    #[test]
    fn epochs_fold_activations_and_bindings_into_windows() {
        let activations = vec![activation("policy-a", 1_000), activation("policy-b", 2_000)];
        let bindings = vec![
            binding("c-pre", PolicyVersion::STATIC_V0, 500),
            binding("c-a1", "policy-a", 1_200),
            binding("c-a2", "policy-a", 1_800),
            binding("c-b1", "policy-b", 2_500),
        ];
        let epochs = derive_epochs(activations, bindings);
        assert_eq!(epochs.len(), 3);
        // The implicit floor epoch covers pre-activation bindings.
        assert_eq!(epochs[0].version.as_str(), PolicyVersion::STATIC_V0);
        assert_eq!(epochs[0].retired_at, Some(ts(1_000)));
        assert_eq!(epochs[0].bindings.len(), 1);
        // Each activation's window ends where the next begins.
        assert_eq!(epochs[1].version.as_str(), "policy-a");
        assert_eq!(epochs[1].activated_at, ts(1_000));
        assert_eq!(epochs[1].retired_at, Some(ts(2_000)));
        assert_eq!(epochs[1].bindings.len(), 2);
        assert_eq!(epochs[2].version.as_str(), "policy-b");
        assert_eq!(epochs[2].retired_at, None);
        assert_eq!(epochs[2].bindings.len(), 1);
    }

    #[test]
    fn epochs_are_empty_when_the_registry_never_moved() {
        assert!(derive_epochs(Vec::new(), Vec::new()).is_empty());
        // Bindings without activations list nothing: every run binds the
        // floor and there is no epoch history to report.
        assert!(derive_epochs(
            Vec::new(),
            vec![binding("c-1", PolicyVersion::STATIC_V0, 1)]
        )
        .is_empty());
    }

    #[test]
    fn version_validation_accepts_path_safe_names_and_reserves_the_floor() {
        assert!(validate_policy_version("policy-0123456789ab").is_ok());
        assert!(validate_policy_version("canary_retry.v2").is_ok());
        assert!(validate_policy_version("").is_err());
        assert!(validate_policy_version("a/b").is_err());
        assert!(validate_policy_version(PolicyVersion::STATIC_V0).is_err());
    }

    #[tokio::test]
    async fn layout_round_trips_all_three_entity_sets() {
        let root = std::env::temp_dir().join(format!("rusty-policy-test-{}", uuid::Uuid::new_v4()));
        let record = PolicyRecord {
            version: PolicyVersion::new("policy-a"),
            policy: ExecutorPolicy::static_v0(),
            source: PolicySource::Candidate {
                candidate_id: "cand-1".into(),
            },
            registered_at: ts(1_000),
        };
        persist_policy(&root, "policy-a", &record).await.unwrap();
        persist_policy(&root, "acme/policy-b", &record)
            .await
            .unwrap();
        let act = activation("policy-a", 1_000);
        append_activation(&root, "default", &act).await.unwrap();
        append_activation(&root, "acme", &act).await.unwrap();
        let bind = binding("c-1", "policy-a", 1_200);
        persist_binding(&root, "default", &bind).await.unwrap();
        persist_binding(&root, "acme", &bind).await.unwrap();
        std::fs::write(policies_dir(&root).join("broken.json"), b"{nope").unwrap();

        let policies = load_policies(&root);
        assert_eq!(policies.len(), 2, "corrupt files are skipped, not fatal");
        assert!(
            policies.contains_key("policy-a"),
            "default tenant: bare key"
        );
        assert!(policies.contains_key("acme/policy-b"));
        let activations = load_activations(&root);
        assert_eq!(activations.len(), 2);
        assert!(activations.contains_key(&activation_file_name(&act)));
        assert!(activations.contains_key(&format!("acme/{}", activation_file_name(&act))));
        let bindings = load_bindings(&root);
        assert_eq!(bindings.len(), 2);
        assert!(bindings.contains_key("c-1"));
        assert!(bindings.contains_key("acme/c-1"));
        let _ = std::fs::remove_dir_all(root);
    }
}
