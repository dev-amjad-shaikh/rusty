//! Fleet upgrades at safe boundaries (EP-07 of blueprints — EP-08-S08):
//! rolling a published assistant version across the sessions pinned to
//! earlier versions, with adoption exclusively at turn boundaries.
//!
//! The version pin is the session's anchor: the first assistant-bound run
//! on a thread records which immutable version it admitted under, and
//! every later run on that thread governs by the pin — a global
//! `/versions/{id}/activate` moves *new* sessions and unpinned legacy
//! threads, never a pinned one. An upgrade operation is the governed
//! motion for pinned sessions: it names the target version, lists every
//! session pinned to an ancestor of the target, and each listed session
//! adopts when its next turn admits — the turn lease guarantees no turn
//! is in flight at that moment, an in-flight turn completes under its
//! pinned version, and no code path re-pins mid-turn.
//!
//! - **Initiation** (`POST /assistants/{id}/upgrades`) is idempotent on
//!   its key: the operation id is content-derived from
//!   `(tenant, assistant, key)`, so a re-issue returns the same operation
//!   and converges the remaining sessions without re-adoption events. One
//!   open operation per assistant — a second initiation while sessions
//!   still wait is a `409`, because which operation a boundary adopts
//!   into must never be ambiguous.
//! - **Adoption** (the run-admission seam) re-resolves the target at the
//!   boundary. A target that no longer resolves fails that session with
//!   the typed error on the record — the session stays fully functional
//!   on its prior version and the fleet does not stall.
//! - **Reporting** (`GET …/upgrades/{op_id}`) answers per-session status:
//!   `awaiting_turn`, `adopted` (with the instant), `failed` (with the
//!   typed error). The `paused` status the spec names arrives with its
//!   producer when the pause store lands on `main` (EP-03-S11).
//!
//! Persistence follows the `threads.rs` pattern: one JSON file per pin
//! and per operation under the store root, reloaded when the router is
//! built, so a restart never loses an in-flight upgrade.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::record::sha256_hex;
use serde::{Deserialize, Serialize};

use crate::assistants::AssistantRecord;

/// The version one session's runs govern by. Recorded at the thread's
/// first assistant-bound run; advanced only by adoption at a turn
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AssistantPin {
    /// The assistant (external id) the session is bound to.
    pub assistant_id: String,
    /// The immutable version the session's runs govern by.
    pub version_id: String,
}

/// One session's state inside an upgrade operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum UpgradeStatus {
    /// Pinned to an earlier version; adopts at its next turn boundary.
    AwaitingTurn,
    /// Adopted the target at the recorded instant.
    Adopted {
        /// When the boundary adopted.
        at: DateTime<Utc>,
    },
    /// Adoption failed at the boundary; the session stays on its prior
    /// version, the typed error named.
    Failed {
        /// The resolution error, verbatim.
        error: String,
        /// When the failure committed.
        at: DateTime<Utc>,
    },
}

impl UpgradeStatus {
    /// `true` while the session still owes a boundary adoption.
    fn open(&self) -> bool {
        matches!(self, UpgradeStatus::AwaitingTurn)
    }
}

/// One session's row in an operation: its pin at initiation and where it
/// stands now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionUpgrade {
    /// The thread (external id) this row tracks.
    pub thread_id: String,
    /// The version the session was pinned to when the operation opened.
    pub from_version_id: String,
    /// Where the session stands.
    pub status: UpgradeStatus,
}

/// One fleet-upgrade operation: the target, the operator, and every
/// session it covers, keyed by internal (`{tenant}/`-prefixed) thread id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UpgradeOperation {
    /// `up-{hash}` over `(tenant, assistant, idempotency key)` — a replay
    /// of the same request finds the same operation.
    pub operation_id: String,
    /// The owning tenant.
    pub tenant: String,
    /// The assistant (external id) being rolled forward.
    pub assistant_id: String,
    /// The version sessions adopt into.
    pub target_version_id: String,
    /// Who initiated (the operator's tenant identity on this surface).
    pub initiated_by: String,
    /// The caller's idempotency key.
    pub idempotency_key: String,
    /// When the operation opened.
    pub created_at: DateTime<Utc>,
    /// Every covered session, keyed by internal thread id.
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionUpgrade>,
}

impl UpgradeOperation {
    /// `true` while any session still owes a boundary — the operation
    /// stays open until every session adopts or fails.
    pub(crate) fn is_open(&self) -> bool {
        self.sessions.values().any(|session| session.status.open())
    }
}

/// What initiating an upgrade came to.
#[derive(Debug)]
pub(crate) enum InitiateOutcome {
    /// A new operation opened.
    Created(UpgradeOperation),
    /// The idempotency key replayed: the existing operation, converged.
    Existing(UpgradeOperation),
    /// The key was already spent on a different target version.
    KeyConflict {
        /// The operation the key belongs to.
        existing: String,
    },
    /// The target is not a version of this assistant.
    TargetUnknown,
    /// Another operation for this assistant still has sessions awaiting
    /// adoption — which operation a boundary would adopt into must never
    /// be ambiguous.
    InProgress {
        /// The open operation's id.
        operation_id: String,
    },
}

/// The upgrade plane: pins and operations, in-memory over one JSON file
/// per record, reloaded at router build.
pub(crate) struct UpgradePlane {
    store_root: PathBuf,
    inner: Mutex<UpgradeState>,
}

#[derive(Default)]
struct UpgradeState {
    /// Internal thread id → the session's pin.
    pins: HashMap<String, AssistantPin>,
    /// Operation id → the operation.
    operations: HashMap<String, UpgradeOperation>,
}

/// The operation id for one `(tenant, assistant, key)` triple —
/// deterministic, so idempotent replays converge by identity.
fn operation_id(tenant: &str, assistant_id: &str, key: &str) -> String {
    format!(
        "up-{}",
        sha256_hex(format!("{tenant}\0{assistant_id}\0{key}").as_bytes())
    )
}

/// `true` when `ancestor` sits on `version`'s parent chain — the target's
/// lineage answer to "is this session pinned to an earlier version".
fn is_ancestor(assistant: &AssistantRecord, ancestor: &str, version: &str) -> bool {
    let mut cursor = assistant.version(version).and_then(|v| v.parent_version_id);
    while let Some(id) = cursor {
        if id == ancestor {
            return true;
        }
        cursor = assistant.version(&id).and_then(|v| v.parent_version_id);
    }
    false
}

impl UpgradePlane {
    /// Load pins and operations from the store root; unreadable files are
    /// skipped with a warning, the `threads.rs` convention.
    pub(crate) fn load(store_root: &Path) -> Self {
        let mut state = UpgradeState::default();
        for path in collect_json(&store_root.join("assistant_pins")) {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<AssistantPin>(&raw).ok())
            {
                Some(pin) => {
                    let id = path
                        .strip_prefix(store_root.join("assistant_pins"))
                        .ok()
                        .map(|rel| rel.with_extension("").to_string_lossy().replace('\\', "/"));
                    if let Some(id) = id {
                        state.pins.insert(id, pin);
                    }
                }
                None => tracing::warn!(path = %path.display(), "skipping unreadable pin file"),
            }
        }
        for path in collect_json(&store_root.join("upgrades")) {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<UpgradeOperation>(&raw).ok())
            {
                Some(operation) => {
                    state
                        .operations
                        .insert(operation.operation_id.clone(), operation);
                }
                None => tracing::warn!(path = %path.display(), "skipping unreadable upgrade file"),
            }
        }
        Self {
            store_root: store_root.to_path_buf(),
            inner: Mutex::new(state),
        }
    }

    /// The session's pin, when the thread has bound an assistant.
    pub(crate) fn pin_for(&self, internal_thread_id: &str) -> Option<AssistantPin> {
        self.inner
            .lock()
            .unwrap()
            .pins
            .get(internal_thread_id)
            .cloned()
    }

    /// Record or advance a pin, persisted.
    pub(crate) fn set_pin(&self, internal_thread_id: &str, pin: AssistantPin) -> io::Result<()> {
        persist_json(
            &self.store_root.join("assistant_pins"),
            internal_thread_id,
            &pin,
        )?;
        self.inner
            .lock()
            .unwrap()
            .pins
            .insert(internal_thread_id.to_string(), pin);
        Ok(())
    }

    /// The open operation covering this session, when one lists it as
    /// still awaiting its boundary.
    pub(crate) fn open_operation_for(
        &self,
        internal_assistant_id: &str,
        internal_thread_id: &str,
    ) -> Option<UpgradeOperation> {
        self.inner
            .lock()
            .unwrap()
            .operations
            .values()
            .find(|operation| {
                crate::auth::scope_id(&operation.tenant, &operation.assistant_id)
                    == internal_assistant_id
                    && operation
                        .sessions
                        .get(internal_thread_id)
                        .is_some_and(|session| session.status.open())
            })
            .cloned()
    }

    /// Open a fleet upgrade: every session pinned to a strict ancestor of
    /// the target joins as `awaiting_turn`. Idempotent on the key; one
    /// open operation per assistant.
    pub(crate) fn initiate(
        &self,
        tenant: &crate::auth::TenantContext,
        assistant: &AssistantRecord,
        target_version_id: &str,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> InitiateOutcome {
        let id = operation_id(tenant.tenant(), &assistant.assistant_id, idempotency_key);
        let mut state = self.inner.lock().unwrap();
        if let Some(existing) = state.operations.get(&id) {
            if existing.target_version_id == target_version_id {
                return InitiateOutcome::Existing(existing.clone());
            }
            return InitiateOutcome::KeyConflict {
                existing: existing.operation_id.clone(),
            };
        }
        if assistant.version(target_version_id).is_none() {
            return InitiateOutcome::TargetUnknown;
        }
        // The record keys by the internal (tenant-scoped) id already —
        // scoping it again would double the prefix and never match a pin.
        let internal_assistant = &assistant.assistant_id;
        if let Some(open) = state.operations.values().find(|operation| {
            crate::auth::scope_id(&operation.tenant, &operation.assistant_id) == *internal_assistant
                && operation.is_open()
        }) {
            return InitiateOutcome::InProgress {
                operation_id: open.operation_id.clone(),
            };
        }
        let sessions = state
            .pins
            .iter()
            .filter(|(internal_thread, pin)| {
                // The pin's assistant id is external; ownership comes from
                // the thread's internal id — a same-named assistant under
                // another tenant never matches.
                crate::auth::strip_owned(tenant.tenant(), internal_thread).is_some()
                    && crate::auth::scope_id(tenant.tenant(), &pin.assistant_id)
                        == *internal_assistant
                    && pin.version_id != target_version_id
                    && is_ancestor(assistant, &pin.version_id, target_version_id)
            })
            .map(|(internal_thread, pin)| {
                let external = crate::auth::strip_owned(tenant.tenant(), internal_thread)
                    .unwrap_or(internal_thread)
                    .to_string();
                (
                    internal_thread.clone(),
                    SessionUpgrade {
                        thread_id: external,
                        from_version_id: pin.version_id.clone(),
                        status: UpgradeStatus::AwaitingTurn,
                    },
                )
            })
            .collect();
        let operation = UpgradeOperation {
            operation_id: id.clone(),
            tenant: tenant.tenant().to_string(),
            // The wire and the scope filters use the external id; the
            // `tenant` field carries the namespace.
            assistant_id: tenant
                .unscope(&assistant.assistant_id)
                .unwrap_or(&assistant.assistant_id)
                .to_string(),
            target_version_id: target_version_id.to_string(),
            initiated_by: tenant.tenant().to_string(),
            idempotency_key: idempotency_key.to_string(),
            created_at: now,
            sessions,
        };
        if persist_json(&self.store_root.join("upgrades"), &id, &operation).is_err() {
            tracing::warn!(operation = %id, "upgrade operation persistence failed; serving from memory");
        }
        state.operations.insert(id, operation.clone());
        InitiateOutcome::Created(operation)
    }

    /// Record a session's adoption of the operation's target at a turn
    /// boundary. Returns the version the session now governs by.
    pub(crate) fn record_adoption(
        &self,
        operation_id: &str,
        internal_thread_id: &str,
        at: DateTime<Utc>,
    ) {
        let mut state = self.inner.lock().unwrap();
        if let Some(operation) = state.operations.get_mut(operation_id) {
            if let Some(session) = operation.sessions.get_mut(internal_thread_id) {
                session.status = UpgradeStatus::Adopted { at };
                let operation = operation.clone();
                if persist_json(&self.store_root.join("upgrades"), operation_id, &operation)
                    .is_err()
                {
                    tracing::warn!(operation = %operation_id, "adoption persistence failed");
                }
            }
        }
    }

    /// Record a session's failed adoption: the typed error on the record,
    /// the session still on its prior version.
    pub(crate) fn record_failure(
        &self,
        operation_id: &str,
        internal_thread_id: &str,
        error: String,
        at: DateTime<Utc>,
    ) {
        let mut state = self.inner.lock().unwrap();
        if let Some(operation) = state.operations.get_mut(operation_id) {
            if let Some(session) = operation.sessions.get_mut(internal_thread_id) {
                session.status = UpgradeStatus::Failed { error, at };
                let operation = operation.clone();
                if persist_json(&self.store_root.join("upgrades"), operation_id, &operation)
                    .is_err()
                {
                    tracing::warn!(operation = %operation_id, "failure persistence failed");
                }
            }
        }
    }

    /// One operation by id.
    pub(crate) fn get(&self, operation_id: &str) -> Option<UpgradeOperation> {
        self.inner
            .lock()
            .unwrap()
            .operations
            .get(operation_id)
            .cloned()
    }

    /// Every operation for one assistant (by internal id), unordered.
    pub(crate) fn list_for(&self, internal_assistant_id: &str) -> Vec<UpgradeOperation> {
        self.inner
            .lock()
            .unwrap()
            .operations
            .values()
            .filter(|operation| {
                crate::auth::scope_id(&operation.tenant, &operation.assistant_id)
                    == internal_assistant_id
            })
            .cloned()
            .collect()
    }
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's pins).
fn collect_json(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    while let Some(dir) = frontier.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                frontier.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                out.push(path);
            }
        }
    }
    out
}

/// Persist one record as `{dir}/{id}.json`, creating tenant
/// subdirectories as needed (the id may carry a `{tenant}/` prefix).
fn persist_json(dir: &Path, id: &str, value: &impl Serialize) -> io::Result<()> {
    let path = dir.join(format!("{id}.json"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_vec_pretty(value).expect("upgrade records serialize infallibly");
    std::fs::write(path, raw)
}
