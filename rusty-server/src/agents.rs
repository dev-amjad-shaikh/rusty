//! Agent registry and activation leases (R0.7 Agent Fabric, wave 1):
//! durable agent identity and the single-activation mechanism behind
//! turn-serialized mailbox draining.
//!
//! The **agent registry** records each agent's pinned
//! [`CapabilityManifest`] under its tenant-scoped id — the server-side half
//! of the identity triple (registry record, checkpoint thread, mailbox).
//! Records follow the assistants/crons conventions exactly: one JSON file
//! per record under `{store_path}/agents/` on the default backend
//! (tenant-scoped ids live one directory deeper), a JSONB-payload
//! `server_agents` table on Postgres.
//!
//! The **activation lease** is the one genuinely new mechanism of the
//! mailbox design ("Typed durable mailboxes on the R0.6 queue",
//! `docs/agent-fabric-design.md`): sequential turn processing requires at
//! most one activation of an agent at a time, across all agent-host
//! workers. A per-agent record — `server_agent_leases` on Postgres, one
//! JSON file per agent under `{store_path}/agent_leases/` on the file
//! backend — carries owner, expiry, and a **fencing ordinal** that
//! increments on every successful claim. A host claims the lease, then
//! drains the mailbox one message at a time; a host that dies stops
//! heartbeating, the lease expires, and another host re-activates the
//! agent. "No two concurrent turns" rests on this lease the same way "no
//! two owners of a task" rests on `FOR UPDATE SKIP LOCKED`; the fencing
//! ordinal is what lets a *stale* holder be told apart from the current
//! one after a steal — its claims and renewals name the ordinal they hold
//! and are rejected once it has moved.
//!
//! On the JSON-file backend, exactness rests on the documented
//! one-writer-process precondition (the `JsonFileCheckpointer` rule,
//! adopted verbatim by design open question 1): the in-process index lock
//! serializes claims within the process, and single-process deployments are
//! what make that sufficient. A multi-process file-store deployment would
//! need an in-process registry serializing activation claims — documented,
//! deliberately not built.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusty_agent_runtime::agents::{CapabilityManifest, SupervisionAttempt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tasks::TaskRecord;

/// One registered agent: its pinned manifest plus free-form metadata,
/// stored under the tenant-scoped agent id (like assistants, tenancy rides
/// the id prefix; the wire shows the external id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentRecord {
    /// Tenant-scoped agent id (`{tenant}/{id}` for named tenants).
    pub agent_id: String,
    /// The pinned capability manifest (core's wire contract, stored
    /// verbatim).
    pub manifest: CapabilityManifest,
    /// The team this agent belongs to (R0.7 wave 2): a declared label, not
    /// a registry — `POST /teams/{team_id}/cancel` addresses every member
    /// by it. Teams are a coordination grouping (wave 3 grows the typed
    /// patterns over them); the wave-2 cancellation tree needs only the
    /// membership fact, so the label rides the registration additively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default)]
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
    /// Server-side supervision state (R0.7 wave 2): the attempt history,
    /// the escalation latch, and the deadline-breach latch. Persisted with
    /// the record (additive — records written before wave 2 deserialize
    /// empty) but never served on the registry endpoints; the deliberate
    /// read surface is `GET /agents/{id}/supervision` — see
    /// [`AgentRecord::wire`].
    #[serde(default, skip_serializing_if = "AgentSupervision::is_empty")]
    pub supervision: AgentSupervision,
}

impl AgentRecord {
    /// The registry-endpoint representation: the full record minus the
    /// supervision state, which has its own dedicated endpoint. Bulk reads
    /// (`GET /agents`) must not drag growing evidence arrays through a
    /// listing shape clients poll.
    pub(crate) fn wire(&self) -> Value {
        let mut value =
            serde_json::to_value(self).expect("AgentRecord serialization is infallible");
        if let Value::Object(ref mut map) = value {
            map.remove("supervision");
        }
        value
    }
}

/// Server-side supervision state of one agent (R0.7 wave 2): the durable
/// half of "no restart happens without a journaled decision". The attempt
/// history is the evidence an escalation carries; the latches make
/// escalation and deadline-breach handling exactly-once per supervision
/// episode (an operator's manual restart is the reset, OTP's "I've fixed
/// the child" path).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentSupervision {
    /// The full attempt history (core's [`SupervisionAttempt`] wire
    /// shape), oldest first — failures, deadline breaches, and manual
    /// restarts in one append-only evidence log.
    #[serde(default)]
    pub attempts: Vec<SupervisionAttempt>,
    /// Latched when the restart budget exhausted and the escalation went
    /// out: further failures are counted (`suppressed_failures`) but
    /// produce no new restarts and no escalation flood. Cleared by
    /// `POST /agents/{id}/restart`.
    #[serde(default)]
    pub escalated: bool,
    /// Latched when the agent-level deadline breach was first handled
    /// (outstanding mailbox traffic cancelled, the decision journaled), so
    /// every later mailbox claim does not re-run the breach path. Cleared
    /// by the manual restart, like `escalated`.
    #[serde(default)]
    pub deadline_breached: bool,
    /// Failures observed while `escalated` is latched — counted, not
    /// appended, so a permanently broken agent cannot grow `attempts`
    /// without bound after the evidence that matters (the escalation
    /// history) is already preserved.
    #[serde(default)]
    pub suppressed_failures: u64,
}

impl AgentSupervision {
    /// `true` for the pristine state — drives the sparse-storage
    /// `skip_serializing_if` on [`AgentRecord::supervision`], so
    /// unsupervised agents' records stay byte-identical to wave 1.
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The next attempt ordinal (1-based position in the history).
    pub(crate) fn next_ordinal(&self) -> u32 {
        self.attempts.len() as u32 + 1
    }
}

/// One agent's activation lease: the host currently allowed to run the
/// agent's turns, the visibility expiry, and the fencing ordinal.
///
/// `TaskLease`'s shape (owner + expiry, renewed by heartbeat while held)
/// plus `fencing`: a monotonically increasing ordinal incremented by every
/// successful claim, so a host holding a stale ordinal — crashed and
/// restarted, or partitioned past its expiry — is rejected by the
/// lease-guarded operations even when its owner string happens to match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ActivationLease {
    /// Tenant-scoped agent id this lease serializes turns for.
    pub agent_id: String,
    /// The claiming host's identity.
    pub owner: String,
    /// 1-based claim ordinal; every successful claim (fresh or steal after
    /// expiry) increments it. Lease-guarded operations must name the
    /// ordinal they hold.
    pub fencing: u64,
    /// Past this instant the lease is stealable by another host.
    pub expires_at: DateTime<Utc>,
    /// When the current holder claimed the lease.
    pub acquired_at: DateTime<Utc>,
}

impl ActivationLease {
    /// `true` when `owner` + `fencing` currently hold this lease at `now`:
    /// identity, ordinal, and liveness all checked — a stale ordinal or an
    /// expired lease is not a holder, whatever the owner string says.
    pub(crate) fn held_by(&self, owner: &str, fencing: u64, now: DateTime<Utc>) -> bool {
        self.owner == owner && self.fencing == fencing && self.expires_at > now
    }
}

/// What an activation claim (`POST /agents/{id}/activate`) resolved to.
#[derive(Debug, Clone)]
pub(crate) enum ActivationOutcome {
    /// The caller now holds the lease (fresh claim, or a steal of an
    /// expired one — the fencing ordinal has moved).
    Claimed(Box<ActivationLease>),
    /// A live lease is held by another owner; carries the current record
    /// so the route's 409 can name the holder and expiry.
    Held(Box<ActivationLease>),
}

/// What a lease-guarded activation mutation (heartbeat / release) resolved
/// to. Mirrors the task queue's `MutationOutcome` discipline: the owner +
/// fencing check is atomic with the mutation, so a stale holder can never
/// resurrect its activation.
#[derive(Debug, Clone)]
pub(crate) enum ActivationMutation {
    /// The mutation landed; carries the updated record.
    Applied(Box<ActivationLease>),
    /// The lease exists but the caller does not hold it (wrong owner,
    /// stale fencing ordinal, or expired). Routes answer 409.
    FencingLost,
    /// No lease exists for this agent (never claimed, or released). Routes
    /// answer 404.
    Unknown,
}

/// What a turn-serialized mailbox claim (`POST /agents/{id}/mailbox/next`)
/// resolved to.
#[derive(Debug, Clone)]
pub(crate) enum MailboxClaim {
    /// The oldest claimable mailbox message, leased to the caller as one
    /// turn of work.
    Claimed(Box<TaskRecord>),
    /// Nothing to hand out: the mailbox is empty (or backing off), **or a
    /// turn is already in flight** — a live-leased message for this
    /// recipient makes the whole mailbox unclaimable until it settles or
    /// its task lease expires. That gate is the server-enforced half of
    /// "one message at a time per agent": turn serialization does not rely
    /// on host discipline alone.
    Empty,
    /// The caller does not hold a live activation lease for this agent
    /// (never claimed, expired, or stolen — owner + fencing did not match).
    /// Routes answer 409; the host must (re-)activate first.
    ActivationLost,
}

/// The scope of one mailbox claim, bundled for the same reason
/// [`crate::tasks::ClaimScope`] bundles the pool claim's inputs: the
/// activation check (agent id, owner, fencing) and the mailbox address
/// (recipient) are applied together before any candidate is chosen, and
/// passing them separately invited half-applied checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MailboxClaimScope<'a> {
    /// Tenant-scoped agent id whose activation lease must be held.
    pub agent_id: &'a str,
    /// The mailbox address being drained: `agent:{agent_id}` (external id —
    /// the tenant column on the task record carries the isolation).
    pub recipient: &'a str,
    /// The claiming host's identity (becomes the task lease owner, so the
    /// ordinary `/tasks/{id}/heartbeat|complete|fail` settlement protocol
    /// settles the turn unchanged).
    pub owner: &'a str,
    /// The fencing ordinal the host holds, checked against the lease.
    pub fencing: u64,
}

// --------------------------------------------------------------------- //
// Validation (routes map `Err` to 400)
// --------------------------------------------------------------------- //

/// A queue recipient (R0.7): today the only addressing discipline is the
/// agent mailbox, `agent:{agent_id}` — so a recipient must carry the agent
/// prefix and a well-formed id behind it. Strict now, loosened later:
/// validation only ever relaxes, so a recipient accepted by this wave can
/// never become invalid (the additive-evolution rule applied to
/// validation).
pub(crate) fn validate_recipient(recipient: &str) -> Result<(), String> {
    let Some(agent_id) = rusty_agent_runtime::agents::agent_id_from_recipient(recipient) else {
        return Err(
            "`recipient` must be a mailbox address of the form `agent:{agent_id}`".to_string(),
        );
    };
    let ok = !agent_id.is_empty()
        && agent_id.len() <= 256
        && agent_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err("`recipient` agent id must match [A-Za-z0-9._-] and be 1..=256 chars".to_string())
    }
}

// --------------------------------------------------------------------- //
// JSON-file persistence
//   agents:        {store_path}/agents/{agent_id}.json
//   agent leases:  {store_path}/agent_leases/{agent_id}.json
// --------------------------------------------------------------------- //

/// The agents directory under the store root. `agents` is a reserved
/// layout name (see [`crate::RESERVED_NAMES`]): client-chosen thread ids
/// may not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("agents")
}

/// The activation-lease directory under the store root (`agent_leases` is
/// likewise reserved).
pub(crate) fn lease_dir(root: &Path) -> PathBuf {
    root.join("agent_leases")
}

/// Persist one record atomically (temp file + rename) under `dir`, named
/// by `id` — the durability discipline every file record in the server
/// shares: a crash mid-write must never leave a truncated record behind.
/// The id may carry a `{tenant}/` prefix, so the parent directory is
/// created, not just the flat dir.
async fn persist_record<T: Serialize>(dir: &Path, id: &str, record: &T) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{id}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = dir.join(format!("{id}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records), mirroring the assistants loader.
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

/// Load all records under `dir`, skipping (with a warning) any file that
/// fails to parse — one corrupt record must not take the registry (or the
/// lease set) down at boot. The `id_of` projection keys the index.
fn load_records<T: serde::de::DeserializeOwned>(
    dir: &Path,
    what: &str,
    id_of: impl Fn(&T) -> String,
) -> HashMap<String, T> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(dir, &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<T>(&raw).ok());
        match parsed {
            Some(record) => {
                out.insert(id_of(&record), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable {what} file")
            }
        }
    }
    out
}

/// Persist one agent record (create or overwrite), atomically.
pub(crate) async fn persist(root: &Path, record: &AgentRecord) -> io::Result<()> {
    persist_record(&dir(root), &record.agent_id, record).await
}

/// Load all persisted agent records.
pub(crate) fn load(root: &Path) -> HashMap<String, AgentRecord> {
    load_records(&dir(root), "agent", |record: &AgentRecord| {
        record.agent_id.clone()
    })
}

/// Persist one activation lease (claim / heartbeat), atomically.
pub(crate) async fn persist_lease(root: &Path, lease: &ActivationLease) -> io::Result<()> {
    persist_record(&lease_dir(root), &lease.agent_id, lease).await
}

/// Remove an agent's activation lease file (release). A missing file is
/// not an error: the release is recorded by the file being gone.
pub(crate) async fn remove_lease(root: &Path, agent_id: &str) -> io::Result<()> {
    let path = lease_dir(root).join(format!("{agent_id}.json"));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Load all persisted activation leases.
pub(crate) fn load_leases(root: &Path) -> HashMap<String, ActivationLease> {
    load_records(
        &lease_dir(root),
        "agent lease",
        |lease: &ActivationLease| lease.agent_id.clone(),
    )
}

/// `lease_ms` as a chrono duration, clamped to what `i64` milliseconds can
/// hold (the task queue applies the same clamp).
pub(crate) fn lease_duration(lease_ms: u64) -> Duration {
    Duration::milliseconds(lease_ms.min(i64::MAX as u64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> AgentRecord {
        let manifest = CapabilityManifest::new("researcher", "researcher/1.4.0");
        AgentRecord {
            agent_id: "acme/researcher-7".to_string(),
            manifest,
            team_id: None,
            metadata: json!({"team": "qa"}),
            created_at: Utc::now(),
            supervision: AgentSupervision::default(),
        }
    }

    fn lease() -> ActivationLease {
        ActivationLease {
            agent_id: "acme/researcher-7".to_string(),
            owner: "host-1".to_string(),
            fencing: 3,
            expires_at: Utc::now() + Duration::seconds(30),
            acquired_at: Utc::now(),
        }
    }

    #[test]
    fn held_by_checks_owner_fencing_and_liveness() {
        let lease = lease();
        let now = Utc::now();
        assert!(lease.held_by("host-1", 3, now));
        assert!(!lease.held_by("host-2", 3, now), "wrong owner");
        assert!(!lease.held_by("host-1", 2, now), "stale fencing ordinal");
        assert!(
            !lease.held_by("host-1", 3, now + Duration::seconds(31)),
            "expired lease"
        );
    }

    #[test]
    fn recipient_validation_accepts_only_mailbox_addresses() {
        assert!(validate_recipient("agent:researcher-7").is_ok());
        assert!(validate_recipient("agent:team.eu-west_2").is_ok());
        // Pools and free-form recipients are not mailbox addresses.
        assert!(validate_recipient("pool-default").is_err());
        assert!(validate_recipient("agent:").is_err());
        assert!(validate_recipient("agent:bad/id").is_err());
        assert!(validate_recipient("agent:bad id").is_err());
        assert!(validate_recipient(&format!("agent:{}", "x".repeat(257))).is_err());
    }

    #[tokio::test]
    async fn registry_and_lease_files_round_trip_with_corrupt_tolerance() {
        let root = std::env::temp_dir().join(format!("rusty-agents-test-{}", uuid::Uuid::new_v4()));
        persist(&root, &record()).await.unwrap();
        persist_lease(&root, &lease()).await.unwrap();
        // Corrupt files are skipped with a warning, never fatal at boot.
        std::fs::create_dir_all(dir(&root)).unwrap();
        std::fs::write(dir(&root).join("broken.json"), b"{nope").unwrap();

        let agents = load(&root);
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents["acme/researcher-7"].manifest.agent_kind,
            "researcher"
        );
        let leases = load_leases(&root);
        assert_eq!(leases.len(), 1);
        assert_eq!(leases["acme/researcher-7"].fencing, 3);

        remove_lease(&root, "acme/researcher-7").await.unwrap();
        assert!(load_leases(&root).is_empty());
        // Releasing twice is not an error.
        remove_lease(&root, "acme/researcher-7").await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_record_wave2_fields_are_additive_and_wire_strips_supervision() {
        // A wave-1 record — no `team_id`, no `supervision` keys — loads
        // with honest defaults, and a pristine record stays minimal.
        let wave1_shape = json!({
            "agent_id": "acme/researcher-7",
            "manifest": {"agent_kind": "researcher", "manifest_version": "researcher/1.4.0"},
            "metadata": null,
            "created_at": Utc::now(),
        });
        let pristine: AgentRecord = serde_json::from_value(wave1_shape).unwrap();
        assert_eq!(pristine.team_id, None);
        assert!(pristine.supervision.is_empty());
        let stored = serde_json::to_value(&pristine).unwrap();
        assert!(stored.get("team_id").is_none());
        assert!(stored.get("supervision").is_none());

        // Once supervision state exists it persists with the record, but
        // the registry wire projection strips it — the dedicated
        // supervision endpoint is the evidence read surface.
        let mut supervised = record();
        supervised.team_id = Some("squad-1".into());
        supervised.supervision.escalated = true;
        supervised.supervision.suppressed_failures = 2;
        let stored = serde_json::to_value(&supervised).unwrap();
        assert_eq!(stored["team_id"], json!("squad-1"));
        assert_eq!(stored["supervision"]["escalated"], json!(true));
        let wire = supervised.wire();
        assert!(wire.get("supervision").is_none());
        assert_eq!(wire["team_id"], json!("squad-1"));
        assert_eq!(wire["manifest"]["agent_kind"], json!("researcher"));

        // Round-trip preserves the supervision evidence.
        let back: AgentRecord = serde_json::from_value(stored).unwrap();
        assert!(back.supervision.escalated);
        assert_eq!(back.supervision.suppressed_failures, 2);
        assert_eq!(back.supervision.next_ordinal(), 1);
    }
}
