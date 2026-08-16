//! Checkpointing: thread-scoped, versioned state snapshots.
//!
//! Checkpoints happen **at super-step boundaries, never mid-node** — so on
//! resume the affected node re-runs from its start (node logic must be
//! idempotent). One primitive, four use cases: durable execution,
//! human-in-the-loop (interrupt → serialize → approve → resume), time travel
//! (load any historical checkpoint, fork alternate paths), and
//! partial-failure recovery.
//!
//! - [`InMemoryCheckpointer`] — RAM-only, lost on restart (dev/test).
//! - [`JsonFileCheckpointer`] — one JSON file per checkpoint under a
//!   directory, pure `serde_json` + `tokio::fs` (durable across restarts).
//!
//! # Delta checkpoints (R0.7 wave 4)
//!
//! Durable backends may store a checkpoint as a **channel-level delta**
//! against the checkpoint before it ([`Checkpoint::base`]) instead of a full
//! snapshot: only the channels whose values changed are written, which is
//! what stops a 1000-step run from writing 1000 full copies of a largely
//! unchanged state. The encoding is **checkpointer-internal**:
//!
//! - `put` always receives a full snapshot; the backend decides delta vs
//!   full ([`encode_delta`]), bounded by a [`DeltaPolicy`] (chain length +
//!   byte ratio) so resume folds stay O(1)-bounded by construction.
//! - Every read method returns a fully materialized checkpoint: chains are
//!   folded onto their base inside the backend ([`fold_delta`]). Callers
//!   never observe `base`.
//! - Checkpoints written before W4 have no `base` field and deserialize as
//!   full snapshots — exactly what they always were.
//!
//! [`JsonFileCheckpointer`] and the Postgres backend opt in;
//! [`InMemoryCheckpointer`] does not — its `put` is a sub-2 µs move, so a
//! delta would buy nothing.
//!
//! # Format-versioned headers
//!
//! Every persisted checkpoint carries its [`CheckpointHeader`] with the
//! stamped `format_version`. Load paths enforce it
//! ([`ensure_supported_format`]): a checkpoint written by a newer format
//! version is refused with a message naming the found version, the
//! supported version, and the upgrade direction — never silently
//! reinterpreted. Older versions load under the additive-evolution
//! contract, so the refusal is one-directional by design.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::record::{CheckpointHeader, JournalRef, CURRENT_FORMAT_VERSION};
use crate::state::State;

/// A versioned snapshot of one thread's state at a super-step boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique checkpoint id (UUID v4; also serves as a fork handle for
    /// time travel).
    pub id: String,

    /// The thread (session) this checkpoint belongs to. Threads cannot see
    /// each other's state.
    pub thread_id: String,

    /// Zero-based super-step index at whose boundary this snapshot was taken.
    pub step: usize,

    /// The full channel state at the boundary.
    ///
    /// In a **delta checkpoint** ([`Checkpoint::base`] set — a storage
    /// detail only opting-in backends produce and no read method ever
    /// returns) this holds just the channels whose values changed since the
    /// base, as full per-channel values. Channels are never deleted from a
    /// state, so overlaying the delta onto the materialized base restores
    /// the full state ([`fold_delta`]).
    pub state: State,

    /// The node set scheduled to run in the *next* super-step. Restored on
    /// resume so execution continues exactly where it suspended.
    pub next_nodes: Vec<String>,

    /// Wall-clock creation time (UTC).
    pub created_at: DateTime<Utc>,

    /// The checkpoint this one diffs against (R0.7 wave 4 delta
    /// checkpoints). `None` is a full snapshot — every checkpoint written
    /// before W4, and every checkpoint a read method returns: folding is
    /// checkpointer-internal, so `base` is only ever observed inside a
    /// backend's storage (checkpoint files, database rows). Additive:
    /// absent from the wire when unset, so pre-W4 checkpoints deserialize
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,

    /// Flight Recorder provenance (R0.5): checkpoint format version, graph
    /// version/content hash, active policy version, and the run's logical
    /// clock value at creation. See [`CheckpointHeader`] for the semantics.
    ///
    /// `#[serde(default)]` keeps checkpoints written before R0.5 (which have
    /// no header field) deserializable: they load with
    /// [`CheckpointHeader::default`] — current format version, unversioned
    /// graph, static policy.
    #[serde(default)]
    pub header: CheckpointHeader,

    /// The journal head at this boundary (`None` pre-R0.5), binding this
    /// state snapshot to the run evidence that produced it.
    #[serde(default)]
    pub journal_ref: Option<JournalRef>,
}

impl Checkpoint {
    /// A new checkpoint with a fresh UUID v4 id and current timestamp.
    ///
    /// Convenience constructor used by tests and pre-R0.5 call paths: the
    /// header falls back to [`CheckpointHeader::default`] and no journal
    /// reference is stamped. The executor mints checkpoints field-by-field
    /// instead, sourcing id/timestamp from the run's determinism seams and
    /// stamping the real header and journal head.
    pub fn new(
        thread_id: impl Into<String>,
        step: usize,
        state: State,
        next_nodes: Vec<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            thread_id: thread_id.into(),
            step,
            state,
            next_nodes,
            created_at: Utc::now(),
            header: CheckpointHeader::default(),
            journal_ref: None,
            base: None,
        }
    }
}

/// Delta-checkpoint chain policy (R0.7 wave 4): when an opting-in backend
/// rewrites a boundary as a full snapshot instead of another chain link.
///
/// Both bounds exist to keep resume reads bounded. An unbounded delta chain
/// makes every resume an O(chain) fold — the event-sourcing failure mode
/// checkpoints exist to avoid (checkpoints, not journals, are the resume
/// authority). The bound is enforced at write time and is a *policy*, never
/// a correctness condition: readers fold whatever chain they find.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeltaPolicy {
    /// Maximum number of consecutive delta links above a full snapshot. A
    /// put that would exceed it writes a full snapshot instead, so resume
    /// folds at most `max_chain_len` channel-sets. Default: 32.
    pub max_chain_len: usize,

    /// Full-snapshot threshold as a fraction of the full state's serialized
    /// size: when a delta's serialized size reaches this share of the state
    /// it would replace, the delta has stopped paying for itself — it costs
    /// nearly a full write *and* extends the chain — so a full snapshot is
    /// written instead. Default: 0.8.
    pub max_byte_ratio: f64,
}

impl Default for DeltaPolicy {
    fn default() -> Self {
        Self {
            max_chain_len: 32,
            max_byte_ratio: 0.8,
        }
    }
}

impl DeltaPolicy {
    /// Full snapshots at every boundary: the pre-W4 write behavior. Used by
    /// backends that do not opt in, and as the measurement baseline for the
    /// wave-4 benchmarks.
    pub fn full_only() -> Self {
        Self {
            max_chain_len: 0,
            max_byte_ratio: 0.0,
        }
    }

    /// Whether delta encoding is on at all (`full_only` disables it).
    fn deltas_enabled(&self) -> bool {
        self.max_chain_len > 0
    }
}

/// The materialized head of a thread's checkpoint chain: what an opting-in
/// backend diffs the next `put` against.
#[derive(Debug, Clone)]
pub struct DeltaHead {
    /// The head checkpoint, fully materialized (never itself a delta).
    pub checkpoint: Checkpoint,

    /// Consecutive delta links below the head (0 = the head is itself a
    /// full snapshot). Backends track this at write time to enforce
    /// [`DeltaPolicy::max_chain_len`] without re-walking the chain per put.
    pub chain_len: usize,
}

/// What [`encode_delta`] decided to persist for one `put`, plus the chain
/// length the stored checkpoint leaves behind.
#[derive(Debug, Clone)]
pub struct DeltaEncoding {
    /// The checkpoint to store: either the put's checkpoint unchanged
    /// (`base == None`, a full snapshot), or a channel-level delta whose
    /// `base` names the chain head's id and whose `state` carries only the
    /// channels that changed since the head.
    pub checkpoint: Checkpoint,

    /// Consecutive delta links below this checkpoint once stored (0 for a
    /// full snapshot).
    pub chain_len: usize,
}

impl DeltaEncoding {
    /// Store the checkpoint as a full snapshot (chain resets).
    pub(crate) fn full(checkpoint: &Checkpoint) -> Self {
        Self {
            checkpoint: Checkpoint {
                base: None,
                ..checkpoint.clone()
            },
            chain_len: 0,
        }
    }
}

/// The serialized size of a state — the byte measure
/// [`DeltaPolicy::max_byte_ratio`] speaks in. `serde_json::to_vec` of a
/// `Value` tree is infallible in practice (the same argument
/// `record::canonical_json_digest` makes); panicking here would require a
/// state that cannot serialize, which no checkpoint path can carry anyway.
fn state_bytes(state: &State) -> usize {
    serde_json::to_vec(state)
        .expect("a State always serializes")
        .len()
}

/// Decide what an opting-in backend persists for one `put`: a full
/// snapshot, or a channel-level delta against the thread's head.
///
/// This is the write half of the W4 delta-checkpoint contract, shared by
/// every opting-in backend (the JSON-file checkpointer, the Postgres
/// checkpointer, and the server store's checkpoint path) so they all make
/// the *same* decision for the same `(checkpoint, head, policy)` triple.
///
/// Rules, in order:
///
/// 1. Deltas disabled, no head, or the chain is already at
///    [`DeltaPolicy::max_chain_len`] → full snapshot (the chain-length
///    bound: resume folds stay bounded by construction).
/// 2. The changed-channel delta's serialized size reaches
///    [`DeltaPolicy::max_byte_ratio`] of the full state's → full snapshot
///    (the byte-ratio bound: a delta that costs ~full buys nothing).
/// 3. Otherwise a delta: `base` names the head's id, `state` carries only
///    channels whose values changed. Unchanged channels are detected by
///    pointer first (the CoW layer shares them) and by value second, so
///    equal content is never rewritten even across a deserialize.
///
/// `put` always receives full snapshots; a caller-set `base` is never
/// honored — encoding is checkpointer-internal.
pub fn encode_delta(
    checkpoint: &Checkpoint,
    head: Option<&DeltaHead>,
    policy: &DeltaPolicy,
) -> DeltaEncoding {
    let Some(head) = head.filter(|_| policy.deltas_enabled()) else {
        return DeltaEncoding::full(checkpoint);
    };
    if head.chain_len >= policy.max_chain_len {
        return DeltaEncoding::full(checkpoint);
    }
    let delta = State::from_shared_channels(
        checkpoint
            .state
            .channels_changed_since(&head.checkpoint.state),
    );
    let delta_bytes = state_bytes(&delta);
    let full_bytes = state_bytes(&checkpoint.state);
    if (delta_bytes as f64) >= policy.max_byte_ratio * (full_bytes as f64) {
        return DeltaEncoding::full(checkpoint);
    }
    DeltaEncoding {
        checkpoint: Checkpoint {
            base: Some(head.checkpoint.id.clone()),
            state: delta,
            ..checkpoint.clone()
        },
        chain_len: head.chain_len + 1,
    }
}

/// Fold one delta link onto its materialized base: the base's channels
/// overlaid with the delta's changed channels. Sharing is preserved — the
/// materialized state shares unchanged channel values with the base rather
/// than copying them (the same CoW observation the diff ran on).
///
/// The result's `base` is cleared: materialization is checkpointer-internal,
/// so what a read method returns is always a full, self-contained
/// checkpoint.
pub fn fold_delta(base: &Checkpoint, delta: &Checkpoint) -> Checkpoint {
    let mut state = base.state.clone();
    for (channel, value) in delta.state.shared_channels() {
        state.insert_shared(channel.to_owned(), value.clone());
    }
    Checkpoint {
        id: delta.id.clone(),
        thread_id: delta.thread_id.clone(),
        step: delta.step,
        state,
        next_nodes: delta.next_nodes.clone(),
        created_at: delta.created_at,
        header: delta.header.clone(),
        journal_ref: delta.journal_ref.clone(),
        base: None,
    }
}

/// Materialize a raw stored chain — root-first, each link's `base` naming
/// the previous link's id — into the full checkpoint at its tip. Errors on
/// a broken chain (a link whose base is not the previous link, or a root
/// that is itself a delta): that is corruption, not data.
///
/// Public so every opting-in storage path folds chains identically (the
/// server store's checkpoint path included).
pub fn fold_chain(chain: &[Checkpoint]) -> Result<Checkpoint> {
    let Some((root, links)) = chain.split_first() else {
        return Err(RustyError::Checkpoint(
            "cannot fold an empty checkpoint chain".into(),
        ));
    };
    if let Some(base) = &root.base {
        return Err(RustyError::Checkpoint(format!(
            "checkpoint chain root `{}` is itself a delta against `{base}`; \
             the chain is broken",
            root.id
        )));
    }
    let mut tip = root.clone();
    for link in links {
        if link.base.as_deref() != Some(tip.id.as_str()) {
            return Err(RustyError::Checkpoint(format!(
                "checkpoint `{}` claims base `{:?}` but follows `{}`; the chain is broken",
                link.id, link.base, tip.id
            )));
        }
        tip = fold_delta(&tip, link);
    }
    Ok(tip)
}

/// Materialize a whole thread's raw checkpoints, in any order: fold each
/// delta chain onto its base, memoizing materialized links so overlapping
/// chains fold once. Deltas whose base is absent or cyclic (a corrupt or
/// partial store) are skipped with a warning, matching the forgiving read
/// paths the file backend already practices — one corrupt file must not
/// poison a thread's whole history.
pub(crate) fn materialize_all(raw: Vec<Checkpoint>) -> Vec<Checkpoint> {
    let by_id: HashMap<String, Checkpoint> = raw.into_iter().map(|c| (c.id.clone(), c)).collect();
    let mut memo: HashMap<String, Checkpoint> = HashMap::new();
    let mut out = Vec::with_capacity(by_id.len());

    'starts: for start_id in by_id.keys() {
        if let Some(materialized) = memo.get(start_id) {
            out.push(materialized.clone());
            continue;
        }
        // Walk from this checkpoint up its base chain, collecting links
        // until a full root or an already-materialized checkpoint.
        // Iterative, not recursive: a corrupt store could chain deeper than
        // any stack.
        let mut links: Vec<&Checkpoint> = Vec::new();
        let mut visiting: HashSet<&str> = HashSet::new();
        let mut cursor = by_id.get(start_id);
        let mut folded: Option<Checkpoint> = None;
        while let Some(current) = cursor {
            if !visiting.insert(current.id.as_str()) {
                tracing::warn!(
                    checkpoint_id = %start_id,
                    "checkpoint delta chain has a cycle; skipping"
                );
                continue 'starts;
            }
            if let Some(materialized) = memo.get(&current.id) {
                folded = Some(materialized.clone());
                break;
            }
            match &current.base {
                None => {
                    folded = Some(current.clone());
                    break;
                }
                Some(base_id) => {
                    links.push(current);
                    match by_id.get(base_id) {
                        Some(base) => cursor = Some(base),
                        None => {
                            tracing::warn!(
                                checkpoint_id = %current.id,
                                base = %base_id,
                                "checkpoint delta base is missing; skipping chain"
                            );
                            continue 'starts;
                        }
                    }
                }
            }
        }
        // `folded` is always set when the loop exits normally (every path
        // that cannot resolve has already continued `starts`).
        let Some(mut tip) = folded else {
            continue;
        };
        // Fold the collected links root-ward → tip: the last link pushed is
        // the closest to the resolved base.
        for link in links.iter().rev() {
            tip = fold_delta(&tip, link);
            memo.insert(link.id.clone(), tip.clone());
        }
        out.push(tip);
    }
    out
}

/// Refuse a checkpoint whose stamped format version this build cannot
/// interpret.
///
/// The envelope evolves additively (serde defaults; see [`CheckpointHeader`]),
/// so an *older* version is never a mismatch — it deserializes under the
/// documented compatibility contract. The refusal case is a checkpoint
/// written by a *newer* format: loading it would reinterpret bytes this
/// build does not understand, so the load fails closed, naming the found
/// version, the supported version, and the upgrade direction. `what`
/// identifies the artifact in the message (a checkpoint id, a file, a row).
pub fn ensure_supported_format(header: &CheckpointHeader, what: &str) -> Result<()> {
    if header.format_version > CURRENT_FORMAT_VERSION {
        return Err(RustyError::Checkpoint(format!(
            "{what} was written in checkpoint format version {}, but this build supports \
             version {CURRENT_FORMAT_VERSION} — upgrade the runtime to read it; newer \
             checkpoint bytes are never silently reinterpreted",
            header.format_version
        )));
    }
    Ok(())
}

/// How a direct checkpoint-file read failed. Internal to the file backend:
/// scans treat corruption forgivingly but propagate a format refusal —
/// serving a thread's history while silently skipping a checkpoint this
/// build cannot interpret would truncate evidence at a format boundary,
/// which is exactly the failure the stamped header exists to prevent.
enum ReadFailure {
    /// IO or JSON corruption: forgiving scans skip the file with a warning.
    Corrupt(RustyError),
    /// A stamped format version newer than this build supports.
    Unsupported(RustyError),
}

impl ReadFailure {
    fn into_error(self) -> RustyError {
        match self {
            ReadFailure::Corrupt(e) | ReadFailure::Unsupported(e) => e,
        }
    }
}

/// The checkpointer interface (the LangGraph `BaseCheckpointSaver` analog).
///
/// Implementations must be safe to share across tasks (`Send + Sync`) and
/// are typically held as `Arc<dyn Checkpointer>` by the executor.
#[async_trait]
pub trait Checkpointer: Send + Sync {
    /// Persist a checkpoint. Implementations must not overwrite an existing
    /// checkpoint with the same id (ids are unique by construction).
    ///
    /// The checkpoint handed to `put` is always a **full snapshot**;
    /// backends that opt into delta checkpoints (R0.7 wave 4) decide the
    /// encoding internally ([`encode_delta`]) — a caller-set
    /// [`Checkpoint::base`] is never honored.
    async fn put(&self, checkpoint: Checkpoint) -> Result<()>;

    /// The most recent checkpoint for a thread, or `None` if the thread has
    /// never been checkpointed. Recency is defined by **insertion (put)
    /// order — the last successfully stored checkpoint wins** — not by
    /// super-step number: replay on the same thread legitimately appends
    /// checkpoints whose `step` is at or below the existing head, and a
    /// later resume must continue that newest timeline.
    ///
    /// Backends without an explicit insertion sequence use `created_at` as
    /// the insertion proxy. That is exact as long as checkpoints are minted
    /// fresh ([`Checkpoint::new`]) when stored and forked histories are
    /// copied oldest-first, which [`Checkpointer::fork_thread`] does.
    async fn get_latest(&self, thread_id: &str) -> Result<Option<Checkpoint>>;

    /// All checkpoints for a thread, oldest first (time-travel listing).
    ///
    /// The order is total and identical across backends — ascending
    /// `(step, created_at, id)` — so that [`Checkpointer::fork_thread`]'s
    /// truncation-by-position is deterministic even when replay has appended
    /// several checkpoints sharing the same `step`.
    async fn list(&self, thread_id: &str) -> Result<Vec<Checkpoint>>;

    /// Fetch one specific checkpoint of a thread by id (time-travel handle).
    ///
    /// The default implementation lists the thread and finds the id, which is
    /// correct (if not maximally efficient) for every reasonable backend.
    /// Returns `None` when the thread has no checkpoint with that id.
    async fn get_by_id(&self, thread_id: &str, checkpoint_id: &str) -> Result<Option<Checkpoint>> {
        let all = self.list(thread_id).await?;
        Ok(all.into_iter().find(|c| c.id == checkpoint_id))
    }

    /// Fork a thread's history into a new thread (time travel).
    ///
    /// Copies the source thread's checkpoints — oldest first — into
    /// `dst_thread`, preserving each checkpoint's `id`, `step`, `state`,
    /// `next_nodes`, and `created_at`; only the `thread_id` changes. When
    /// `at_checkpoint_id` is given, only checkpoints up to and including that
    /// checkpoint are copied (fork from a mid-history point); when `None`,
    /// the full history is copied. Returns the number of checkpoints copied.
    ///
    /// The default implementation re-`put`s each selected checkpoint with the
    /// destination thread id. This is correct for every implementation whose
    /// `put` uniqueness scope is per-thread — including both built-in impls
    /// ([`InMemoryCheckpointer`] keys its map by thread, and
    /// [`JsonFileCheckpointer`] stores under `{dir}/{thread_id}/`), so reused
    /// ids cannot collide across threads. An implementation whose `put`
    /// enforces globally unique ids, or whose storage path ignores
    /// `checkpoint.thread_id`, **must override this method** (e.g. a SQL
    /// backend with a global primary key would mint fresh ids or insert with
    /// an explicit thread column).
    ///
    /// Delta-checkpoint backends (R0.7 wave 4) additionally override this to
    /// write the forked checkpoints as **full snapshots**: a fork is a new
    /// timeline and the natural compaction point, so time-travel reads on
    /// the fork never fold a chain — and never another timeline's.
    ///
    /// Errors when the source thread has no checkpoints, when
    /// `at_checkpoint_id` does not exist on the source thread, or when
    /// `src_thread == dst_thread` (ids would collide within one thread).
    async fn fork_thread(
        &self,
        src_thread: &str,
        dst_thread: &str,
        at_checkpoint_id: Option<&str>,
    ) -> Result<usize> {
        if src_thread == dst_thread {
            return Err(RustyError::Checkpoint(format!(
                "cannot fork thread `{src_thread}` onto itself: checkpoint ids would collide"
            )));
        }
        let all = self.list(src_thread).await?;
        if all.is_empty() {
            return Err(RustyError::Checkpoint(format!(
                "cannot fork thread `{src_thread}`: no checkpoints found"
            )));
        }
        let selected: Vec<Checkpoint> = match at_checkpoint_id {
            None => all,
            Some(id) => {
                let pos = all.iter().position(|c| c.id == id).ok_or_else(|| {
                    RustyError::Checkpoint(format!(
                        "cannot fork thread `{src_thread}`: unknown checkpoint id `{id}`"
                    ))
                })?;
                all[..=pos].to_vec()
            }
        };
        let copied = selected.len();
        for mut checkpoint in selected {
            checkpoint.thread_id = dst_thread.to_string();
            self.put(checkpoint).await?;
        }
        tracing::info!(
            src_thread = %src_thread,
            dst_thread = %dst_thread,
            copied = copied,
            "thread history forked"
        );
        Ok(copied)
    }
}

/// In-memory checkpointer: thread-safe (all operations take a single mutex
/// over the store), lost on restart. Suitable for development, tests, and
/// ephemeral runs.
#[derive(Debug, Default, Clone)]
pub struct InMemoryCheckpointer {
    // thread_id -> checkpoints in insertion (super-step) order.
    inner: Arc<Mutex<HashMap<String, Vec<Checkpoint>>>>,
}

impl InMemoryCheckpointer {
    /// An empty store. Clones of the returned checkpointer share the same
    /// underlying map.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Vec<Checkpoint>>>> {
        self.inner
            .lock()
            .map_err(|_| RustyError::Checkpoint("in-memory checkpointer lock poisoned".into()))
    }
}

#[async_trait]
impl Checkpointer for InMemoryCheckpointer {
    async fn put(&self, checkpoint: Checkpoint) -> Result<()> {
        let mut guard = self.lock()?;
        let entry = guard.entry(checkpoint.thread_id.clone()).or_default();
        if entry.iter().any(|c| c.id == checkpoint.id) {
            return Err(RustyError::Checkpoint(format!(
                "checkpoint id `{}` already exists for thread `{}`",
                checkpoint.id, checkpoint.thread_id
            )));
        }
        tracing::debug!(
            thread_id = %checkpoint.thread_id,
            checkpoint_id = %checkpoint.id,
            step = checkpoint.step,
            "checkpoint stored (in-memory)"
        );
        entry.push(checkpoint);
        Ok(())
    }

    async fn get_latest(&self, thread_id: &str) -> Result<Option<Checkpoint>> {
        let guard = self.lock()?;
        Ok(guard.get(thread_id).and_then(|v| v.last()).cloned())
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<Checkpoint>> {
        let guard = self.lock()?;
        let mut all = guard.get(thread_id).cloned().unwrap_or_default();
        // Same total order as every other backend (ascending
        // `(step, created_at, id)`), not raw insertion order: replay can
        // append out-of-step-order checkpoints, and fork truncation must be
        // deterministic across backends.
        all.sort_by(|a, b| {
            a.step
                .cmp(&b.step)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.id.cmp(&b.id))
        });
        Ok(all)
    }
}

/// File-backed checkpointer: one pretty-printed JSON file per checkpoint
/// (`{dir}/{thread_id}/{checkpoint_id}.json`) plus a `latest` pointer file
/// (`{dir}/{thread_id}/latest` holding the most recent checkpoint id), using
/// only `serde_json` and `tokio::fs` — no database dependencies.
///
/// Writes are atomic: payload is written to a uniquely named temp file in the
/// same directory and then renamed over the target path, so a crash mid-write
/// can never leave a truncated checkpoint file behind. Puts are serialized
/// per thread (an in-process lock per `thread_id`), so concurrent same-thread
/// puts cannot interleave the checkpoint file and pointer writes and leave
/// `latest` pointing at the older checkpoint. The lock is per-process:
/// multiple writer PROCESSES over the same directory are not serialized —
/// treat one writer process per thread directory as a precondition.
///
/// Read paths are forgiving: a missing thread directory yields `None` / an
/// empty list, a missing or corrupt `latest` pointer falls back to scanning
/// the checkpoint files, and individual corrupt checkpoint files are skipped
/// during scans. Genuine IO failures surface as
/// [`RustyError::Checkpoint`]. One deliberate exception: a checkpoint
/// stamped with a newer format version than this build supports is
/// **refused** ([`ensure_supported_format`]), never skipped — skipping it
/// would serve an older checkpoint as "latest" while evidence this build
/// cannot interpret sits in the same directory.
///
/// # Delta checkpoints (R0.7 wave 4)
///
/// This backend opts into delta encoding: `put` diffs the incoming full
/// snapshot against the thread's materialized head and stores only the
/// channels that changed, bounded by a [`DeltaPolicy`]
/// ([`JsonFileCheckpointer::with_delta_policy`] reconfigures or disables
/// it). The head is cached in-process (`heads`), so the hot path diffs
/// against the previous put's shared channel values — pointer-equal for
/// unchanged channels, no re-read — and a cold process rebuilds the cache
/// entry by reading the current chain once. Reads fold chains transparently
/// ([`fold_chain`], `materialize_all`); checkpoint files with no `base`
/// field (everything written before W4) load exactly as before.
#[derive(Debug, Clone)]
pub struct JsonFileCheckpointer {
    dir: PathBuf,
    // Per-thread put locks behind a map mutex: the map is locked only long
    // enough to clone the per-thread `Arc`, never across the put itself.
    put_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    // The materialized chain head per thread, for the write-side delta
    // diff. Coherent in-process because puts are serialized per thread (the
    // same one-writer-process precondition the whole backend documents);
    // readers never consult it.
    heads: Arc<Mutex<HashMap<String, DeltaHead>>>,
    policy: DeltaPolicy,
}

impl JsonFileCheckpointer {
    /// A checkpointer rooted at `dir` (created lazily on first `put`), with
    /// the default [`DeltaPolicy`] (delta encoding on, chain bounded at 32).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_delta_policy(dir, DeltaPolicy::default())
    }

    /// A checkpointer rooted at `dir` with an explicit delta policy —
    /// [`DeltaPolicy::full_only`] restores the pre-W4 full-snapshot write
    /// behavior (the benchmark baseline).
    pub fn with_delta_policy(dir: impl Into<PathBuf>, policy: DeltaPolicy) -> Self {
        Self {
            dir: dir.into(),
            put_locks: Arc::new(Mutex::new(HashMap::new())),
            heads: Arc::new(Mutex::new(HashMap::new())),
            policy,
        }
    }

    /// The delta-chain policy this checkpointer writes under.
    pub fn delta_policy(&self) -> DeltaPolicy {
        self.policy
    }

    /// The root directory checkpoints are stored under.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The lock serializing `put` for one thread. `Clone`s of this
    /// checkpointer share the same lock map, so clones still serialize
    /// against each other.
    fn put_lock(&self, thread_id: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
        let mut map = self
            .put_locks
            .lock()
            .map_err(|_| RustyError::Checkpoint("put-lock map poisoned".into()))?;
        Ok(map
            .entry(thread_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone())
    }

    /// `{dir}/{thread_id}/` — per-thread subdirectory.
    fn thread_dir(&self, thread_id: &str) -> PathBuf {
        self.dir.join(thread_id)
    }

    /// `{dir}/{thread_id}/{checkpoint_id}.json`.
    fn checkpoint_path(&self, thread_id: &str, checkpoint_id: &str) -> PathBuf {
        self.thread_dir(thread_id)
            .join(format!("{checkpoint_id}.json"))
    }

    /// `{dir}/{thread_id}/latest` — pointer file holding the most recent
    /// checkpoint id (plain text).
    fn latest_path(&self, thread_id: &str) -> PathBuf {
        self.thread_dir(thread_id).join("latest")
    }

    /// Atomically write `bytes` to `path` via a unique temp file + rename.
    /// The temp file lives in the same directory so the rename stays on one
    /// filesystem. Best-effort temp cleanup on failure.
    ///
    /// `pub(crate)`: the W4 artifact store (`journal::FileArtifactStore`)
    /// writes blobs under the same discipline.
    pub(crate) async fn atomic_write(path: &PathBuf, bytes: &[u8]) -> Result<()> {
        let tmp = path.with_file_name(format!(
            ".{}.tmp-{}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "checkpoint".into()),
            uuid::Uuid::new_v4()
        ));
        if let Err(e) = tokio::fs::write(&tmp, bytes).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(RustyError::Checkpoint(format!(
                "failed to write temp file `{}`: {e}",
                tmp.display()
            )));
        }
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(RustyError::Checkpoint(format!(
                "failed to rename `{}` -> `{}`: {e}",
                tmp.display(),
                path.display()
            )));
        }
        Ok(())
    }

    /// Read and deserialize one checkpoint file, enforcing the stamped
    /// format version ([`ensure_supported_format`]): corruption maps to
    /// [`ReadFailure::Corrupt`], a newer format to [`ReadFailure::Unsupported`].
    async fn read_checkpoint(path: &PathBuf) -> std::result::Result<Checkpoint, ReadFailure> {
        let bytes = tokio::fs::read(path).await.map_err(|e| {
            ReadFailure::Corrupt(RustyError::Checkpoint(format!(
                "failed to read checkpoint file `{}`: {e}",
                path.display()
            )))
        })?;
        let checkpoint: Checkpoint = serde_json::from_slice(&bytes).map_err(|e| {
            ReadFailure::Corrupt(RustyError::Checkpoint(format!(
                "corrupt checkpoint file `{}`: {e}",
                path.display()
            )))
        })?;
        ensure_supported_format(
            &checkpoint.header,
            &format!("checkpoint `{}` (file `{}`)", checkpoint.id, path.display()),
        )
        .map_err(ReadFailure::Unsupported)?;
        Ok(checkpoint)
    }

    /// Load every parseable `*.json` checkpoint in a thread directory,
    /// **unfolded** (a delta checkpoint stays a delta here — folding happens
    /// in the read methods above this layer). A missing directory yields an
    /// empty vec; corrupt files are skipped.
    async fn scan_thread_raw(&self, thread_id: &str) -> Result<Vec<Checkpoint>> {
        let dir = self.thread_dir(thread_id);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(RustyError::Checkpoint(format!(
                    "failed to read thread directory `{}`: {e}",
                    dir.display()
                )))
            }
        };

        let mut checkpoints = Vec::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    return Err(RustyError::Checkpoint(format!(
                        "failed to iterate thread directory `{}`: {e}",
                        dir.display()
                    )))
                }
            };
            let path = entry.path();
            let is_json = path.extension().is_some_and(|ext| ext == "json");
            if !is_json {
                continue;
            }
            match Self::read_checkpoint(&path).await {
                Ok(cp) => checkpoints.push(cp),
                // A format refusal is never skipped: listing the thread
                // without the unreadable checkpoint would silently truncate
                // its history at the format boundary.
                Err(ReadFailure::Unsupported(e)) => return Err(e),
                // Graceful degradation: one corrupt file must not poison the
                // whole thread's history.
                Err(ReadFailure::Corrupt(e)) => {
                    tracing::warn!(
                        thread_id = %thread_id,
                        path = %path.display(),
                        error = %e,
                        "skipping corrupt checkpoint file during scan"
                    );
                    continue;
                }
            }
        }
        Ok(checkpoints)
    }

    /// The most recently stored raw checkpoint (unfolded — possibly a
    /// delta): the `latest` pointer fast path, falling back to a directory
    /// scan when the pointer is missing, corrupt, or dangling.
    async fn read_latest_raw(&self, thread_id: &str) -> Result<Option<Checkpoint>> {
        let latest_path = self.latest_path(thread_id);
        if let Ok(id_bytes) = tokio::fs::read(&latest_path).await {
            if let Ok(id) = std::str::from_utf8(&id_bytes) {
                let id = id.trim();
                if !id.is_empty() {
                    let path = self.checkpoint_path(thread_id, id);
                    match Self::read_checkpoint(&path).await {
                        Ok(cp) => return Ok(Some(cp)),
                        // A format refusal is terminal, not a fallback
                        // trigger: scanning would either hit the same refusal
                        // or, worse, settle on an older checkpoint as
                        // "latest" while a newer-format one exists.
                        Err(ReadFailure::Unsupported(e)) => return Err(e),
                        Err(ReadFailure::Corrupt(e)) => tracing::warn!(
                            thread_id = %thread_id,
                            path = %path.display(),
                            error = %e,
                            "latest pointer target unreadable; falling back to directory scan"
                        ),
                    }
                }
            }
        }
        // Fallback: the checkpoint with the greatest `created_at` — the
        // insertion-order proxy shared with the other backends (see the
        // trait's `get_latest` contract), not the highest step, so a replay
        // that appended lower-step checkpoints still resumes the newest
        // timeline.
        Ok(self
            .scan_thread_raw(thread_id)
            .await?
            .into_iter()
            .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id))))
    }

    /// Load the raw delta chain ending at `tip`, root-first. Defensively
    /// bounded: writes cap chains at the policy bound, so a chain far past
    /// it means corruption or a foreign writer — and a cycle is always
    /// corruption. Both error rather than fold forever.
    async fn load_chain(&self, thread_id: &str, tip: Checkpoint) -> Result<Vec<Checkpoint>> {
        // The fold bound: generous headroom over the write bound, so any
        // chain this backend could have written folds, and only corrupt or
        // foreign chains hit the limit.
        let fold_bound = self.policy.max_chain_len.saturating_mul(4).max(128);
        let mut chain = vec![tip];
        let mut visited = HashSet::new();
        while let Some(base_id) = chain.last().expect("chain starts non-empty").base.clone() {
            if !visited.insert(base_id.clone()) {
                return Err(RustyError::Checkpoint(format!(
                    "checkpoint delta chain in thread `{thread_id}` has a cycle at `{base_id}`"
                )));
            }
            if chain.len() > fold_bound {
                return Err(RustyError::Checkpoint(format!(
                    "checkpoint delta chain in thread `{thread_id}` exceeds {fold_bound} links; \
                     refusing to fold (write bound is {})",
                    self.policy.max_chain_len
                )));
            }
            let path = self.checkpoint_path(thread_id, &base_id);
            chain.push(
                Self::read_checkpoint(&path)
                    .await
                    .map_err(ReadFailure::into_error)?,
            );
        }
        chain.reverse();
        Ok(chain)
    }

    /// The thread's materialized chain head for the write-side delta diff:
    /// cached, or rebuilt by reading and folding the current chain (a cold
    /// process pays this once per thread). `None` when the thread has no
    /// checkpoints.
    async fn head_of(&self, thread_id: &str) -> Result<Option<DeltaHead>> {
        if let Some(head) = self.lock_heads()?.get(thread_id) {
            return Ok(Some(head.clone()));
        }
        let Some(raw) = self.read_latest_raw(thread_id).await? else {
            return Ok(None);
        };
        let chain = self.load_chain(thread_id, raw).await?;
        let head = DeltaHead {
            chain_len: chain.len() - 1,
            checkpoint: fold_chain(&chain)?,
        };
        self.lock_heads()?
            .insert(thread_id.to_owned(), head.clone());
        Ok(Some(head))
    }

    fn lock_heads(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, DeltaHead>>> {
        self.heads
            .lock()
            .map_err(|_| RustyError::Checkpoint("delta-head cache poisoned".into()))
    }

    /// The shared write path. `force_full` stores the checkpoint as a full
    /// snapshot regardless of policy — the fork compaction semantics
    /// ([`Checkpointer::fork_thread`]).
    async fn put_internal(&self, checkpoint: Checkpoint, force_full: bool) -> Result<()> {
        // Serialize the whole put (checkpoint file THEN pointer file) per
        // thread: without this, two concurrent same-thread puts can
        // interleave (file A, file B, pointer B, pointer A) and leave
        // `latest` pointing at the older checkpoint. Held across `.await`s,
        // hence a tokio mutex — a std guard would make the future `!Send`.
        let lock = self.put_lock(&checkpoint.thread_id)?;
        let _put_guard = lock.lock().await;

        let thread_dir = self.thread_dir(&checkpoint.thread_id);
        tokio::fs::create_dir_all(&thread_dir).await.map_err(|e| {
            RustyError::Checkpoint(format!(
                "failed to create thread directory `{}`: {e}",
                thread_dir.display()
            ))
        })?;

        let path = self.checkpoint_path(&checkpoint.thread_id, &checkpoint.id);
        // Preserve the no-overwrite contract: checkpoint ids are unique by
        // construction, so an existing file means a duplicate `put`.
        if tokio::fs::try_exists(&path).await.map_err(|e| {
            RustyError::Checkpoint(format!(
                "failed to stat checkpoint file `{}`: {e}",
                path.display()
            ))
        })? {
            return Err(RustyError::Checkpoint(format!(
                "checkpoint id `{}` already exists for thread `{}`",
                checkpoint.id, checkpoint.thread_id
            )));
        }

        // Delta encoding (W4): diff the incoming full snapshot against the
        // thread's materialized head and store only the channels that
        // changed, bounded by the chain policy. The encoded file is what
        // later reads fold back.
        let encoding = if force_full {
            DeltaEncoding::full(&checkpoint)
        } else {
            let head = self.head_of(&checkpoint.thread_id).await?;
            encode_delta(&checkpoint, head.as_ref(), &self.policy)
        };

        let bytes = serde_json::to_vec_pretty(&encoding.checkpoint)?;
        Self::atomic_write(&path, &bytes).await?;

        // Update the latest pointer (also atomically). Written after the
        // checkpoint file itself so a crash never leaves a dangling pointer.
        Self::atomic_write(
            &self.latest_path(&checkpoint.thread_id),
            checkpoint.id.as_bytes(),
        )
        .await?;

        tracing::debug!(
            thread_id = %checkpoint.thread_id,
            checkpoint_id = %checkpoint.id,
            step = checkpoint.step,
            delta = encoding.checkpoint.base.is_some(),
            path = %path.display(),
            "checkpoint persisted (json file)"
        );

        // The next put diffs against THIS put's full state, held shared:
        // channels the next step does not write stay pointer-identical,
        // which is what makes the diff cheap.
        self.lock_heads()?.insert(
            checkpoint.thread_id.clone(),
            DeltaHead {
                checkpoint,
                chain_len: encoding.chain_len,
            },
        );
        Ok(())
    }
}

#[async_trait]
impl Checkpointer for JsonFileCheckpointer {
    async fn put(&self, checkpoint: Checkpoint) -> Result<()> {
        self.put_internal(checkpoint, false).await
    }

    async fn get_latest(&self, thread_id: &str) -> Result<Option<Checkpoint>> {
        let Some(raw) = self.read_latest_raw(thread_id).await? else {
            return Ok(None);
        };
        if raw.base.is_none() {
            // A full snapshot — everything written before W4, chain roots,
            // forked checkpoints — needs no folding.
            return Ok(Some(raw));
        }
        let chain = self.load_chain(thread_id, raw).await?;
        fold_chain(&chain).map(Some)
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<Checkpoint>> {
        // Fold every delta chain, then impose the same total order as every
        // other backend (ascending `(step, created_at, id)`), not raw file
        // order: replay can append out-of-step-order checkpoints, and fork
        // truncation must be deterministic across backends.
        let mut all = materialize_all(self.scan_thread_raw(thread_id).await?);
        all.sort_by(|a, b| {
            a.step
                .cmp(&b.step)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.id.cmp(&b.id))
        });
        Ok(all)
    }

    /// Forked histories are written as **full snapshots** (eager
    /// compaction): a fork is a new timeline and the natural compaction
    /// point, so time-travel reads on the fork never fold a chain — and
    /// never another timeline's. The source thread is untouched.
    async fn fork_thread(
        &self,
        src_thread: &str,
        dst_thread: &str,
        at_checkpoint_id: Option<&str>,
    ) -> Result<usize> {
        if src_thread == dst_thread {
            return Err(RustyError::Checkpoint(format!(
                "cannot fork thread `{src_thread}` onto itself: checkpoint ids would collide"
            )));
        }
        let all = self.list(src_thread).await?;
        if all.is_empty() {
            return Err(RustyError::Checkpoint(format!(
                "cannot fork thread `{src_thread}`: no checkpoints found"
            )));
        }
        let selected: Vec<Checkpoint> = match at_checkpoint_id {
            None => all,
            Some(id) => {
                let pos = all.iter().position(|c| c.id == id).ok_or_else(|| {
                    RustyError::Checkpoint(format!(
                        "cannot fork thread `{src_thread}`: unknown checkpoint id `{id}`"
                    ))
                })?;
                all[..=pos].to_vec()
            }
        };
        let copied = selected.len();
        for mut checkpoint in selected {
            checkpoint.thread_id = dst_thread.to_string();
            checkpoint.base = None;
            self.put_internal(checkpoint, true).await?;
        }
        tracing::info!(
            src_thread = %src_thread,
            dst_thread = %dst_thread,
            copied = copied,
            "thread history forked"
        );
        Ok(copied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(thread: &str, step: usize) -> Checkpoint {
        Checkpoint::new(thread, step, State::new(), vec!["next".into()])
    }

    #[tokio::test]
    async fn in_memory_roundtrip() {
        let store = InMemoryCheckpointer::new();
        assert!(store.get_latest("t1").await.unwrap().is_none());
        assert!(store.list("t1").await.unwrap().is_empty());

        store.put(cp("t1", 0)).await.unwrap();
        store.put(cp("t1", 1)).await.unwrap();
        store.put(cp("t2", 0)).await.unwrap();

        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.step, 1);
        assert_eq!(latest.next_nodes, vec!["next".to_string()]);

        let all = store.list("t1").await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].step, 0);
        assert_eq!(all[1].step, 1);

        // Threads are isolated.
        assert_eq!(store.list("t2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn in_memory_rejects_duplicate_id() {
        let store = InMemoryCheckpointer::new();
        let checkpoint = cp("t1", 0);
        store.put(checkpoint.clone()).await.unwrap();
        let err = store.put(checkpoint).await.unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));
    }

    #[tokio::test]
    async fn checkpoint_serializes() {
        let checkpoint = cp("t1", 2);
        let json = serde_json::to_string(&checkpoint).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, checkpoint.id);
        assert_eq!(back.step, 2);
    }

    /// Unique temp root under the OS temp dir, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("rusty-checkpoint-test-{}", uuid::Uuid::new_v4())),
            )
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn json_file_roundtrip() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        assert!(store.get_latest("t1").await.unwrap().is_none());
        assert!(store.list("t1").await.unwrap().is_empty());

        let mut state = State::new();
        state.insert("answer", serde_json::json!(42));
        let cp0 = Checkpoint::new("t1", 0, state.clone(), vec!["node_b".into()]);
        let id0 = cp0.id.clone();
        store.put(cp0).await.unwrap();

        // File layout: <root>/<thread_id>/<checkpoint_id>.json
        assert!(tmp.0.join("t1").join(format!("{id0}.json")).exists());

        let back = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(back.id, id0);
        assert_eq!(back.thread_id, "t1");
        assert_eq!(back.step, 0);
        assert_eq!(back.state, state);
        assert_eq!(back.next_nodes, vec!["node_b".to_string()]);

        // Threads are isolated.
        assert!(store.get_latest("t2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn json_file_latest_pointer_tracks_most_recent_put() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        store.put(cp("t1", 0)).await.unwrap();
        let cp1 = cp("t1", 1);
        let id1 = cp1.id.clone();
        store.put(cp1).await.unwrap();

        // The pointer file holds the most recent checkpoint id as text.
        let pointer = std::fs::read_to_string(tmp.0.join("t1").join("latest")).unwrap();
        assert_eq!(pointer, id1);

        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.id, id1);
        assert_eq!(latest.step, 1);
    }

    #[tokio::test]
    async fn json_file_list_sorted_by_step_regardless_of_put_order() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        store.put(cp("t1", 2)).await.unwrap();
        store.put(cp("t1", 0)).await.unwrap();
        store.put(cp("t1", 1)).await.unwrap();

        let all = store.list("t1").await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].step, 0);
        assert_eq!(all[1].step, 1);
        assert_eq!(all[2].step, 2);

        // Recency is insertion order, not highest step: the pointer tracks
        // the most recent put (step 1), and the scan fallback agrees because
        // the freshest `created_at` is also the last put's.
        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.step, 1);
    }

    /// The `get_latest`/`list` contract is backend-independent: recency =
    /// insertion order, listing = ascending `(step, created_at, id)`. Every
    /// `Checkpointer` impl must agree with this test.
    #[tokio::test]
    async fn recency_and_list_order_agree_across_backends() {
        let tmp = TestDir::new();
        let memory = InMemoryCheckpointer::new();
        let json_file = JsonFileCheckpointer::new(tmp.0.clone());

        // Out-of-step-order puts (as replay-on-same-thread produces): each
        // checkpoint is minted fresh, so `created_at` increases per put.
        let steps = [2usize, 0, 1];
        for step in steps {
            memory.put(cp("t1", step)).await.unwrap();
            json_file.put(cp("t1", step)).await.unwrap();
        }

        let stores: [&dyn Checkpointer; 2] = [&memory, &json_file];
        for store in stores {
            // Latest = last put (step 1), not highest step (step 2).
            let latest = store.get_latest("t1").await.unwrap().unwrap();
            assert_eq!(latest.step, 1, "backend disagrees on recency");
            // List = ascending step order regardless of put order.
            let listed: Vec<usize> = store
                .list("t1")
                .await
                .unwrap()
                .iter()
                .map(|c| c.step)
                .collect();
            assert_eq!(listed, [0, 1, 2], "backend disagrees on list order");
        }
    }

    #[tokio::test]
    async fn json_file_missing_thread_returns_none_and_empty() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        assert!(store.get_latest("never-seen").await.unwrap().is_none());
        assert!(store.list("never-seen").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn json_file_rejects_duplicate_id() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        let checkpoint = cp("t1", 0);
        store.put(checkpoint.clone()).await.unwrap();
        let err = store.put(checkpoint).await.unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));
    }

    #[tokio::test]
    async fn json_file_durable_across_instances() {
        let tmp = TestDir::new();
        let cp0 = cp("t1", 0);
        let id0 = cp0.id.clone();

        JsonFileCheckpointer::new(tmp.0.clone())
            .put(cp0)
            .await
            .unwrap();

        // A fresh instance over the same root sees the checkpoint
        // (simulates process restart).
        let reopened = JsonFileCheckpointer::new(tmp.0.clone());
        let latest = reopened.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.id, id0);
        assert_eq!(reopened.list("t1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn json_file_corrupt_files_are_handled_gracefully() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        let good = cp("t1", 0);
        let good_id = good.id.clone();
        store.put(good).await.unwrap();

        // A corrupt checkpoint file next to a valid one must not break
        // list/get_latest.
        std::fs::write(tmp.0.join("t1").join("garbage.json"), b"{not json!!").unwrap();
        let all = store.list("t1").await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, good_id);

        // A corrupt latest pointer falls back to scanning.
        std::fs::write(tmp.0.join("t1").join("latest"), b"no-such-checkpoint-id").unwrap();
        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.id, good_id);

        // Garbage bytes in the pointer also fall back to scanning.
        std::fs::write(tmp.0.join("t1").join("latest"), [0xff, 0xfe, 0x00]).unwrap();
        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.id, good_id);
    }

    #[tokio::test]
    async fn get_by_id_hit_and_miss() {
        let store = InMemoryCheckpointer::new();
        let cp0 = cp("t1", 0);
        let id0 = cp0.id.clone();
        store.put(cp0).await.unwrap();
        store.put(cp("t1", 1)).await.unwrap();
        store.put(cp("t2", 0)).await.unwrap();

        let hit = store.get_by_id("t1", &id0).await.unwrap().unwrap();
        assert_eq!(hit.id, id0);
        assert_eq!(hit.step, 0);
        assert_eq!(hit.thread_id, "t1");

        // Unknown id on an existing thread, and any id on an unknown thread.
        assert!(store.get_by_id("t1", "no-such-id").await.unwrap().is_none());
        assert!(store.get_by_id("t2", &id0).await.unwrap().is_none());
        assert!(store.get_by_id("never-seen", &id0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fork_full_history_copies_all_checkpoints() {
        let store = InMemoryCheckpointer::new();
        let mut src = Vec::new();
        for step in 0..3 {
            let mut state = State::new();
            state.insert("n", serde_json::json!(step));
            let checkpoint = Checkpoint::new("src", step, state, vec!["next".into()]);
            src.push(checkpoint.clone());
            store.put(checkpoint).await.unwrap();
        }

        let copied = store.fork_thread("src", "dst", None).await.unwrap();
        assert_eq!(copied, 3);

        let dst = store.list("dst").await.unwrap();
        assert_eq!(dst.len(), 3);
        for (forked, original) in dst.iter().zip(src.iter()) {
            // Everything is preserved except the thread id (ids may be reused
            // across threads; uniqueness is per-thread).
            assert_eq!(forked.id, original.id);
            assert_eq!(forked.step, original.step);
            assert_eq!(forked.state, original.state);
            assert_eq!(forked.next_nodes, original.next_nodes);
            assert_eq!(forked.created_at, original.created_at);
            assert_eq!(forked.thread_id, "dst");
        }
        assert_eq!(store.get_latest("dst").await.unwrap().unwrap().step, 2);

        // The source thread is untouched.
        assert_eq!(store.list("src").await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn fork_at_mid_checkpoint_truncates_history() {
        let store = InMemoryCheckpointer::new();
        let mut ids = Vec::new();
        for step in 0..4 {
            let checkpoint = cp("src", step);
            ids.push(checkpoint.id.clone());
            store.put(checkpoint).await.unwrap();
        }

        // Fork at the step-1 checkpoint: only steps 0 and 1 are copied.
        let copied = store
            .fork_thread("src", "dst", Some(&ids[1]))
            .await
            .unwrap();
        assert_eq!(copied, 2);

        let dst = store.list("dst").await.unwrap();
        assert_eq!(dst.len(), 2);
        assert_eq!(dst[0].id, ids[0]);
        assert_eq!(dst[1].id, ids[1]);
        assert_eq!(dst[0].step, 0);
        assert_eq!(dst[1].step, 1);
        // Latest of the fork is the cut point, not the source's head.
        assert_eq!(store.get_latest("dst").await.unwrap().unwrap().id, ids[1]);
    }

    #[tokio::test]
    async fn fork_errors_on_empty_src_unknown_id_and_self_fork() {
        let store = InMemoryCheckpointer::new();
        let checkpoint = cp("src", 0);
        let id0 = checkpoint.id.clone();
        store.put(checkpoint).await.unwrap();

        let err = store.fork_thread("empty", "dst", None).await.unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));

        let err = store
            .fork_thread("src", "dst", Some("no-such-id"))
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));

        let err = store
            .fork_thread("src", "src", Some(&id0))
            .await
            .unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));

        // Failed forks leave no partial state behind on the destination.
        assert!(store.list("dst").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn json_file_fork_across_threads_persists_correctly() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        let mut ids = Vec::new();
        for step in 0..3 {
            let mut state = State::new();
            state.insert("n", serde_json::json!(step));
            let checkpoint = Checkpoint::new("src", step, state, vec!["next".into()]);
            ids.push(checkpoint.id.clone());
            store.put(checkpoint).await.unwrap();
        }

        let copied = store
            .fork_thread("src", "dst", Some(&ids[1]))
            .await
            .unwrap();
        assert_eq!(copied, 2);

        // Files land under the destination thread's own directory (reused
        // ids live in a different path, so no collision).
        assert!(tmp.0.join("dst").join(format!("{}.json", ids[0])).exists());
        assert!(tmp.0.join("dst").join(format!("{}.json", ids[1])).exists());
        assert!(!tmp.0.join("dst").join(format!("{}.json", ids[2])).exists());

        // The forked files carry the destination thread id in their payload.
        let latest = store.get_latest("dst").await.unwrap().unwrap();
        assert_eq!(latest.id, ids[1]);
        assert_eq!(latest.thread_id, "dst");
        assert_eq!(latest.step, 1);

        // Durable across instances (process restart): the fork survives.
        let reopened = JsonFileCheckpointer::new(tmp.0.clone());
        let dst = reopened.list("dst").await.unwrap();
        assert_eq!(dst.len(), 2);
        assert_eq!(dst[0].id, ids[0]);
        assert_eq!(dst[1].id, ids[1]);
        assert!(dst.iter().all(|c| c.thread_id == "dst"));

        // The source thread is untouched.
        assert_eq!(reopened.list("src").await.unwrap().len(), 3);
        assert_eq!(
            reopened.get_latest("src").await.unwrap().unwrap().id,
            ids[2]
        );
    }

    // ---- Delta checkpoints (R0.7 wave 4) ----

    /// A state whose `blob` channel stays constant while `step` turns over —
    /// the shape delta checkpoints exist for.
    fn evolving_state(step: usize, blob: &str) -> State {
        let mut state = State::new();
        state.insert("blob", serde_json::json!(blob));
        state.insert("step", serde_json::json!(step));
        state
    }

    /// Read one checkpoint file straight off disk as raw JSON.
    fn file_json(tmp: &TestDir, thread: &str, id: &str) -> serde_json::Value {
        let text = std::fs::read_to_string(tmp.0.join(thread).join(format!("{id}.json"))).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn delta_chain_roundtrips_through_read_paths() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());
        let blob = "x".repeat(1024);

        let mut ids = Vec::new();
        for step in 0..5 {
            let cp = Checkpoint::new("t1", step, evolving_state(step, &blob), vec!["next".into()]);
            ids.push(cp.id.clone());
            store.put(cp).await.unwrap();
        }

        // Puts after the first are deltas: `base` names the previous
        // checkpoint and `state` carries only the changed `step` channel.
        let on_disk = file_json(&tmp, "t1", &ids[2]);
        assert_eq!(on_disk["base"], serde_json::json!(ids[1]));
        assert_eq!(
            on_disk["state"],
            serde_json::json!({"step": 2}),
            "delta file must carry only changed channels"
        );

        // Reads materialize: get_latest folds the chain back to full state.
        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.base, None, "read methods never return a delta");
        assert_eq!(latest.state, evolving_state(4, &blob));

        // list materializes every link.
        let all = store.list("t1").await.unwrap();
        assert_eq!(all.len(), 5);
        for (step, cp) in all.iter().enumerate() {
            assert_eq!(cp.state, evolving_state(step, &blob));
            assert_eq!(cp.base, None);
        }

        // get_by_id (trait default, over list) materializes mid-chain links.
        let mid = store.get_by_id("t1", &ids[1]).await.unwrap().unwrap();
        assert_eq!(mid.state, evolving_state(1, &blob));
    }

    #[tokio::test]
    async fn delta_chain_is_bounded_by_policy() {
        let tmp = TestDir::new();
        let policy = DeltaPolicy {
            max_chain_len: 3,
            ..DeltaPolicy::default()
        };
        let store = JsonFileCheckpointer::with_delta_policy(tmp.0.clone(), policy);

        let mut ids = Vec::new();
        for step in 0..7 {
            let cp = Checkpoint::new("t1", step, evolving_state(step, "b"), vec![]);
            ids.push(cp.id.clone());
            store.put(cp).await.unwrap();
        }

        // Chain: full, delta, delta, delta, FULL (bound hit), delta, delta.
        let expect_full = [true, false, false, false, true, false, false];
        for (i, id) in ids.iter().enumerate() {
            let on_disk = file_json(&tmp, "t1", id);
            assert_eq!(
                on_disk.get("base").is_some(),
                !expect_full[i],
                "checkpoint {i} full/delta mismatch: {on_disk}"
            );
        }
        // Reads are unaffected by where the chain compacted.
        let all = store.list("t1").await.unwrap();
        for (step, cp) in all.iter().enumerate() {
            assert_eq!(cp.state, evolving_state(step, "b"));
        }
    }

    #[tokio::test]
    async fn delta_byte_ratio_forces_full_when_delta_stops_paying() {
        let tmp = TestDir::new();
        let policy = DeltaPolicy {
            max_chain_len: 32,
            max_byte_ratio: 0.5,
        };
        let store = JsonFileCheckpointer::with_delta_policy(tmp.0.clone(), policy);

        let base_state = State::from_value(serde_json::json!({"a": "AAAA", "b": "BBBB"})).unwrap();
        let cp0 = Checkpoint::new("t1", 0, base_state, vec![]);
        let id0 = cp0.id.clone();
        store.put(cp0).await.unwrap();

        // Every channel changed: the delta is the whole state — past the
        // 0.5 ratio, so a full snapshot is written instead of a chain link.
        let changed = State::from_value(serde_json::json!({"a": "CCCC", "b": "DDDD"})).unwrap();
        let cp1 = Checkpoint::new("t1", 1, changed, vec![]);
        let id1 = cp1.id.clone();
        store.put(cp1).await.unwrap();

        assert!(file_json(&tmp, "t1", &id0).get("base").is_none());
        assert!(
            file_json(&tmp, "t1", &id1).get("base").is_none(),
            "a delta worth ~full must reset the chain"
        );
        assert_eq!(
            store.get_latest("t1").await.unwrap().unwrap().state,
            State::from_value(serde_json::json!({"a": "CCCC", "b": "DDDD"})).unwrap()
        );
    }

    #[tokio::test]
    async fn delta_dedupes_equal_content_without_pointer_sharing() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        store
            .put(Checkpoint::new(
                "t1",
                0,
                State::from_value(serde_json::json!({"k": "v"})).unwrap(),
                vec![],
            ))
            .await
            .unwrap();
        // A fresh, independently built state with identical content: no
        // pointer sharing, so only value equality can dedupe it.
        let cp1 = Checkpoint::new(
            "t1",
            1,
            State::from_value(serde_json::json!({"k": "v"})).unwrap(),
            vec![],
        );
        let id1 = cp1.id.clone();
        store.put(cp1).await.unwrap();

        let on_disk = file_json(&tmp, "t1", &id1);
        assert!(on_disk.get("base").is_some());
        assert_eq!(
            on_disk["state"],
            serde_json::json!({}),
            "identical content must produce an empty delta"
        );
        assert_eq!(
            store.get_latest("t1").await.unwrap().unwrap().state,
            State::from_value(serde_json::json!({"k": "v"})).unwrap()
        );
    }

    #[tokio::test]
    async fn reopened_store_rebuilds_the_head_and_keeps_differencing() {
        let tmp = TestDir::new();
        let blob = "y".repeat(512);

        let id0;
        {
            let store = JsonFileCheckpointer::new(tmp.0.clone());
            let cp = Checkpoint::new("t1", 0, evolving_state(0, &blob), vec![]);
            id0 = cp.id.clone();
            store.put(cp).await.unwrap();
        }

        // A fresh instance over the same root (process restart): the head
        // cache is cold and must be rebuilt from the on-disk chain.
        let reopened = JsonFileCheckpointer::new(tmp.0.clone());
        let cp1 = Checkpoint::new("t1", 1, evolving_state(1, &blob), vec![]);
        let id1 = cp1.id.clone();
        reopened.put(cp1).await.unwrap();

        let on_disk = file_json(&tmp, "t1", &id1);
        assert_eq!(on_disk["base"], serde_json::json!(id0));
        assert_eq!(on_disk["state"], serde_json::json!({"step": 1}));
        assert_eq!(
            reopened.get_latest("t1").await.unwrap().unwrap().state,
            evolving_state(1, &blob)
        );
    }

    #[tokio::test]
    async fn pre_delta_checkpoints_load_unchanged() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        // Hand-write a checkpoint file in the pre-W4 shape (no `base`
        // field), as an older binary would have produced it.
        let cp = Checkpoint::new("t1", 0, evolving_state(3, "legacy"), vec!["next".into()]);
        let id = cp.id.clone();
        let dir = tmp.0.join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&cp).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("base");
        let text = serde_json::to_string_pretty(&json).unwrap();
        assert!(!text.contains("\"base\""));
        std::fs::write(dir.join(format!("{id}.json")), &text).unwrap();
        std::fs::write(dir.join("latest"), &id).unwrap();

        let latest = store.get_latest("t1").await.unwrap().unwrap();
        assert_eq!(latest.id, id);
        assert_eq!(latest.state, evolving_state(3, "legacy"));
        assert_eq!(latest.next_nodes, vec!["next".to_string()]);
        assert_eq!(latest.base, None);

        // A delta put on top of a legacy checkpoint diffs against it fine.
        let cp1 = Checkpoint::new("t1", 1, evolving_state(4, "legacy"), vec![]);
        let id1 = cp1.id.clone();
        store.put(cp1).await.unwrap();
        assert_eq!(file_json(&tmp, "t1", &id1)["base"], serde_json::json!(id));
    }

    #[tokio::test]
    async fn fork_compacts_to_full_snapshots() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        let mut ids = Vec::new();
        for step in 0..4 {
            let cp = Checkpoint::new("src", step, evolving_state(step, "f"), vec![]);
            ids.push(cp.id.clone());
            store.put(cp).await.unwrap();
        }

        let copied = store
            .fork_thread("src", "dst", Some(&ids[2]))
            .await
            .unwrap();
        assert_eq!(copied, 3);

        // Every forked checkpoint is a full snapshot on disk — the fork is
        // the compaction point, so its timeline never folds a chain.
        for id in &ids[..3] {
            let on_disk = file_json(&tmp, "dst", id);
            assert!(
                on_disk.get("base").is_none(),
                "forked checkpoint must be a full snapshot: {on_disk}"
            );
        }
        // And reads agree, on both timelines.
        assert_eq!(
            store.get_latest("dst").await.unwrap().unwrap().state,
            evolving_state(2, "f")
        );
        assert_eq!(
            store.get_latest("src").await.unwrap().unwrap().state,
            evolving_state(3, "f")
        );
    }

    #[tokio::test]
    async fn full_only_policy_restores_pre_w4_writes() {
        let tmp = TestDir::new();
        let store =
            JsonFileCheckpointer::with_delta_policy(tmp.0.clone(), DeltaPolicy::full_only());

        let mut ids = Vec::new();
        for step in 0..3 {
            let cp = Checkpoint::new("t1", step, evolving_state(step, "z"), vec![]);
            ids.push(cp.id.clone());
            store.put(cp).await.unwrap();
        }
        for id in &ids {
            assert!(file_json(&tmp, "t1", id).get("base").is_none());
        }
        assert_eq!(store.list("t1").await.unwrap().len(), 3);
    }

    #[test]
    fn encode_delta_decides_full_delta_and_bounds() {
        let head_state = evolving_state(0, "blob");
        let head_cp = Checkpoint::new("t", 0, head_state.clone(), vec![]);
        let head = DeltaHead {
            checkpoint: head_cp.clone(),
            chain_len: 0,
        };
        let policy = DeltaPolicy::default();

        // Delta: one small channel changed.
        let next = Checkpoint::new("t", 1, evolving_state(1, "blob"), vec![]);
        let encoding = encode_delta(&next, Some(&head), &policy);
        assert_eq!(encoding.checkpoint.base, Some(head_cp.id.clone()));
        assert_eq!(
            encoding.checkpoint.state,
            State::from_value(serde_json::json!({"step": 1})).unwrap()
        );
        assert_eq!(encoding.chain_len, 1);

        // Chain bound: a head already at the limit forces a full snapshot.
        let at_bound = DeltaHead {
            chain_len: policy.max_chain_len,
            ..head.clone()
        };
        let encoding = encode_delta(&next, Some(&at_bound), &policy);
        assert_eq!(encoding.checkpoint.base, None);
        assert_eq!(encoding.chain_len, 0);

        // No head or disabled policy: full snapshot.
        assert!(encode_delta(&next, None, &policy).checkpoint.base.is_none());
        assert!(encode_delta(&next, Some(&head), &DeltaPolicy::full_only())
            .checkpoint
            .base
            .is_none());

        // A caller-set base is never honored: put contract is full snapshots.
        let mut with_base = next.clone();
        with_base.base = Some("caller-lie".into());
        let encoding = encode_delta(&with_base, Some(&at_bound), &policy);
        assert_eq!(encoding.checkpoint.base, None);
    }

    #[test]
    fn fold_chain_materializes_and_rejects_broken_chains() {
        let blob = "q".repeat(64);
        let mut chain: Vec<Checkpoint> = Vec::new();
        // The materialized tip the backend would diff the next put against.
        let mut head: Option<DeltaHead> = None;
        for step in 0..4 {
            let full = Checkpoint::new("t", step, evolving_state(step, &blob), vec![]);
            let encoding = encode_delta(&full, head.as_ref(), &DeltaPolicy::default());
            chain.push(encoding.checkpoint);
            head = Some(DeltaHead {
                checkpoint: full,
                chain_len: encoding.chain_len,
            });
        }

        let tip = fold_chain(&chain).unwrap();
        assert_eq!(tip.state, evolving_state(3, &blob));
        assert_eq!(tip.base, None);

        // A root that is itself a delta is a broken chain.
        let err = fold_chain(&chain[1..]).unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));

        // A link pointing at the wrong base is a broken chain.
        let mut broken = chain.clone();
        broken[2].base = Some("not-the-parent".into());
        assert!(fold_chain(&broken).is_err());
    }

    #[test]
    fn materialize_all_folds_memoized_and_skips_corruption() {
        let full = Checkpoint::new("t", 0, evolving_state(0, "m"), vec![]);
        let mut link = Checkpoint::new("t", 1, evolving_state(1, "m"), vec![]);
        link.base = Some(full.id.clone());
        link.state = State::from_value(serde_json::json!({"step": 1})).unwrap();
        let mut dangling = Checkpoint::new("t", 2, State::new(), vec![]);
        dangling.base = Some("missing-base".into());
        let mut cyclic = Checkpoint::new("t", 3, State::new(), vec![]);
        cyclic.base = Some(cyclic.id.clone());

        let materialized = materialize_all(vec![link.clone(), dangling, cyclic, full.clone()]);
        // The dangling and cyclic links are skipped (corruption must not
        // poison the thread); the full root and its link fold.
        assert_eq!(materialized.len(), 2);
        let by_step: HashMap<usize, &Checkpoint> =
            materialized.iter().map(|c| (c.step, c)).collect();
        assert_eq!(by_step[&0].state, evolving_state(0, "m"));
        assert_eq!(by_step[&1].state, evolving_state(1, "m"));
        assert!(materialized.iter().all(|c| c.base.is_none()));
    }

    #[tokio::test]
    async fn json_file_refuses_a_newer_format_version() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        let good = cp("t1", 0);
        store.put(good).await.unwrap();

        // A checkpoint file stamped by a newer format, written by hand next
        // to the good one (and pointed at by `latest`, as a newer binary's
        // put would leave it).
        let mut newer = cp("t1", 1);
        newer.header.format_version = CURRENT_FORMAT_VERSION + 1;
        let bytes = serde_json::to_vec_pretty(&newer).unwrap();
        std::fs::write(tmp.0.join("t1").join(format!("{}.json", newer.id)), &bytes).unwrap();
        std::fs::write(tmp.0.join("t1").join("latest"), &newer.id).unwrap();

        for outcome in [
            store.get_latest("t1").await.unwrap_err().to_string(),
            store.list("t1").await.unwrap_err().to_string(),
            store
                .get_by_id("t1", &newer.id)
                .await
                .unwrap_err()
                .to_string(),
        ] {
            assert!(
                outcome.contains(&(CURRENT_FORMAT_VERSION + 1).to_string()),
                "names the found version: {outcome}"
            );
            assert!(
                outcome.contains(&format!("version {CURRENT_FORMAT_VERSION}")),
                "names the supported version: {outcome}"
            );
            assert!(
                outcome.contains("upgrade the runtime"),
                "names the upgrade direction: {outcome}"
            );
        }
        // The good checkpoint is not deleted or shadowed — the thread is
        // simply unreadable until the runtime is upgraded.
    }

    #[tokio::test]
    async fn json_file_loads_an_older_format_version_under_the_additive_contract() {
        let tmp = TestDir::new();
        let store = JsonFileCheckpointer::new(tmp.0.clone());

        // A header stamped by an older writer: older versions deserialize
        // under the additive-evolution contract, so the load succeeds.
        let mut older = cp("t1", 0);
        older.header.format_version = CURRENT_FORMAT_VERSION - 1;
        let id = older.id.clone();
        std::fs::create_dir_all(tmp.0.join("t1")).unwrap();
        std::fs::write(
            tmp.0.join("t1").join(format!("{id}.json")),
            serde_json::to_vec_pretty(&older).unwrap(),
        )
        .unwrap();

        let loaded = store.get_by_id("t1", &id).await.unwrap().unwrap();
        assert_eq!(loaded.header.format_version, CURRENT_FORMAT_VERSION - 1);
    }

    #[test]
    fn ensure_supported_format_refuses_only_newer_versions() {
        let mut header = CheckpointHeader::default();
        assert!(ensure_supported_format(&header, "checkpoint `x`").is_ok());
        header.format_version = 0;
        assert!(ensure_supported_format(&header, "checkpoint `x`").is_ok());
        header.format_version = CURRENT_FORMAT_VERSION + 1;
        let err = ensure_supported_format(&header, "checkpoint `x`").unwrap_err();
        assert!(err.to_string().contains("checkpoint `x`"));
    }
}
