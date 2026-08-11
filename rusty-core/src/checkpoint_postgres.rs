//! Postgres-backed checkpointer (crate feature `postgres`).
//!
//! Stores checkpoints in a single table, `rusty_checkpoints`:
//!
//! | column         | type          | notes                              |
//! |----------------|---------------|------------------------------------|
//! | `thread_id`    | `text`        | part of the primary key            |
//! | `checkpoint_id`| `text`        | part of the primary key            |
//! | `step`         | `bigint`      | super-step index                   |
//! | `state`        | `jsonb`       | full channel state                 |
//! | `next_nodes`   | `jsonb`       | JSON array of node names           |
//! | `created_at`   | `timestamptz` | wall-clock creation time (UTC)     |
//! | `header`       | `jsonb`       | R0.5 provenance header; nullable   |
//! | `journal_ref`  | `jsonb`       | R0.5 journal-head ref; nullable    |
//! | `base`         | `text`        | W4 delta-chain link; nullable      |
//!
//! The primary key `(thread_id, checkpoint_id)` enforces the no-overwrite
//! contract of the [`Checkpointer`] trait at the database level: a duplicate
//! `put` fails with a unique-violation error mapped to
//! [`RustyError::Checkpoint`].
//!
//! [`PostgresCheckpointer::connect`] runs an idempotent auto-migration
//! (`CREATE TABLE IF NOT EXISTS`, then additive
//! `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` for the R0.5 columns) before
//! returning, so a fresh database is ready to use immediately.
//!
//! # R0.5 schema evolution
//!
//! `header` and `journal_ref` are **nullable** `jsonb` columns carrying the
//! checkpoint's [`crate::record::CheckpointHeader`] provenance and its
//! [`crate::record::JournalRef`] evidence link. The migration is forward-
//! and rollback-safe:
//!
//! - **Older code against the migrated table** — every statement names its
//!   columns explicitly (no `SELECT *`), so the added columns are invisible
//!   to pre-R0.5 readers and writers; their inserts leave both columns
//!   `NULL`.
//! - **This code against pre-R0.5 rows** — `NULL` decodes to the serde
//!   defaults ([`CheckpointHeader::default`], `None`), matching what the
//!   in-memory and JSON-file backends reconstruct for old checkpoints.
//!
//! All `sqlx` errors are mapped to [`RustyError::Checkpoint`]; all
//! statements use bound parameters (`$1`, `$2`, ...), never string
//! interpolation, so thread ids and checkpoint payloads cannot inject SQL.
//!
//! # R0.7 wave 4 schema evolution: delta checkpoints
//!
//! `base` is a **nullable** `text` column carrying the delta-chain link
//! ([`Checkpoint::base`]): `NULL` is a full snapshot — every pre-W4 row —
//! and a set `base` names the checkpoint this row's `state` diffs against.
//! `state` then holds only the channels that changed (channel-granularity,
//! matching the CoW layer). Materialization is checkpointer-internal, same
//! as the file backend: read methods fold chains ([`fold_chain`],
//! `materialize_all`) and never return a delta. Writes are diffs against
//! the thread's materialized head ([`PostgresCheckpointer::delta_head`]),
//! bounded by the shared [`DeltaPolicy`]. The head is cached per process
//! (thread writes are single-writer by contract — the same activation-lease
//! discipline the server enforces); a cold process rebuilds the cache entry
//! by walking the chain once.
//!
//! This module also hosts the Postgres half of the W4 artifact store
//! ([`PostgresArtifactStore`]): the content-addressed blob table journal
//! snapshots spill oversized payloads to (see `journal::ArtifactStore`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::checkpoint::{
    encode_delta, fold_chain, materialize_all, Checkpoint, Checkpointer, DeltaEncoding, DeltaHead,
    DeltaPolicy,
};
use crate::error::{Result, RustyError};
use crate::journal::ArtifactStore;
use crate::record::{sha256_hex, ArtifactRef, CheckpointHeader, JournalRef};
use crate::state::State;

/// The table checkpoints are stored in.
pub const TABLE_NAME: &str = "rusty_checkpoints";

/// Idempotent schema migration run by [`PostgresCheckpointer::connect`].
const CREATE_TABLE_SQL: &str = "\
CREATE TABLE IF NOT EXISTS rusty_checkpoints (
    thread_id     text        NOT NULL,
    checkpoint_id text        NOT NULL,
    step          bigint      NOT NULL,
    state         jsonb       NOT NULL,
    next_nodes    jsonb       NOT NULL,
    created_at    timestamptz NOT NULL,
    PRIMARY KEY (thread_id, checkpoint_id)
)";

/// R0.5 additive migration: the provenance header column. Nullable so
/// pre-R0.5 rows (and rows written by older code against a migrated table)
/// are valid; `NULL` decodes to [`CheckpointHeader::default`].
const ADD_HEADER_SQL: &str = "\
ALTER TABLE rusty_checkpoints ADD COLUMN IF NOT EXISTS header jsonb";

/// R0.5 additive migration: the journal-head reference column. Nullable;
/// `NULL` decodes to `None` (no journal attached, or a pre-R0.5 row).
const ADD_JOURNAL_REF_SQL: &str = "\
ALTER TABLE rusty_checkpoints ADD COLUMN IF NOT EXISTS journal_ref jsonb";

/// W4 additive migration: the delta-chain link column. Nullable; `NULL`
/// decodes to `None` (a full snapshot — every pre-W4 row).
const ADD_BASE_SQL: &str = "\
ALTER TABLE rusty_checkpoints ADD COLUMN IF NOT EXISTS base text";

/// The full migration, in execution order: the base table first, then
/// additive column migrations oldest-to-newest. Every statement is
/// idempotent (`IF NOT EXISTS`), so the sequence is safe to re-run and safe
/// to run against a partially migrated database. New schema changes append
/// to this list — never reorder or edit a landed entry.
const MIGRATION_SQL: &[&str] = &[
    CREATE_TABLE_SQL,
    ADD_HEADER_SQL,
    ADD_JOURNAL_REF_SQL,
    ADD_BASE_SQL,
];

/// Insert one checkpoint. No `ON CONFLICT` clause: a duplicate
/// `(thread_id, checkpoint_id)` must surface as an error (SQLSTATE 23505),
/// preserving the trait's no-overwrite contract.
const INSERT_SQL: &str = "\
INSERT INTO rusty_checkpoints
    (thread_id, checkpoint_id, step, state, next_nodes, created_at, header, journal_ref, base)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

/// The most recent checkpoint for a thread. Recency is insertion order (the
/// trait contract), with `created_at` as the insertion proxy — checkpoints
/// are minted fresh when stored — and `checkpoint_id` as a deterministic
/// tie-break. `step` deliberately plays no role: replay on the same thread
/// appends checkpoints whose step is at or below the head, and resume must
/// continue that newest timeline.
const GET_LATEST_SQL: &str = "\
SELECT thread_id, checkpoint_id, step, state, next_nodes, created_at, header, journal_ref, base
FROM rusty_checkpoints
WHERE thread_id = $1
ORDER BY created_at DESC, checkpoint_id DESC
LIMIT 1";

/// All checkpoints for a thread, oldest first (time-travel listing). The
/// `(created_at, checkpoint_id)` tie-break makes the order total: replay on
/// the same thread legitimately appends rows sharing a `step`, and
/// `fork_thread` truncates by list position, so same-step rows must not
/// order nondeterministically.
const LIST_SQL: &str = "\
SELECT thread_id, checkpoint_id, step, state, next_nodes, created_at, header, journal_ref, base
FROM rusty_checkpoints
WHERE thread_id = $1
ORDER BY step ASC, created_at ASC, checkpoint_id ASC";

/// One checkpoint by primary key. Overrides the trait default, which would
/// fetch and decode the thread's entire history and then linear-search;
/// time-travel replay hits this on every run. Also the chain-walk step for
/// delta materialization.
const GET_BY_ID_SQL: &str = "\
SELECT thread_id, checkpoint_id, step, state, next_nodes, created_at, header, journal_ref, base
FROM rusty_checkpoints
WHERE thread_id = $1 AND checkpoint_id = $2";

/// Transaction-scoped advisory lock key used by
/// [`PostgresCheckpointer::migrate`] to serialize concurrent migrations.
const MIGRATION_LOCK_KEY: i64 = 0x6167_7067_5f6d_6967; // "agpg_mig"

/// Map a `sqlx` error to [`RustyError::Checkpoint`], giving duplicate
/// checkpoint ids (SQLSTATE 23505, unique_violation) a clearer message.
fn map_sqlx_error(op: &str, err: sqlx::Error) -> RustyError {
    if let sqlx::Error::Database(db_err) = &err {
        if db_err.code().as_deref() == Some("23505") {
            return RustyError::Checkpoint(format!(
                "{op}: duplicate checkpoint id (unique violation): {db_err}"
            ));
        }
    }
    RustyError::Checkpoint(format!("{op}: {err}"))
}

/// One row of `rusty_checkpoints`, decoupled from the live database so
/// the serde mapping is unit-testable without a Postgres server.
#[derive(Debug, Clone, PartialEq)]
struct CheckpointRow {
    thread_id: String,
    checkpoint_id: String,
    step: i64,
    state: Value,
    next_nodes: Value,
    created_at: DateTime<Utc>,
    /// Serialized [`CheckpointHeader`]; `None` for pre-R0.5 rows (`NULL`).
    header: Option<Value>,
    /// Serialized [`JournalRef`]; `None` when the checkpoint carries no
    /// journal reference (pre-R0.5 rows, or runs without a journal).
    journal_ref: Option<Value>,
    /// The delta-chain link ([`Checkpoint::base`]); `None` for full
    /// snapshots — every pre-W4 row (`NULL`).
    base: Option<String>,
}

impl CheckpointRow {
    /// Checkpoint -> row: `usize` step to `bigint`, state to `jsonb` object,
    /// `next_nodes` to a `jsonb` array, header/journal-ref to `jsonb` (or
    /// `NULL` when absent).
    fn from_checkpoint(checkpoint: &Checkpoint) -> Result<Self> {
        let step = i64::try_from(checkpoint.step).map_err(|_| {
            RustyError::Checkpoint(format!(
                "step {} does not fit into a Postgres bigint",
                checkpoint.step
            ))
        })?;
        Ok(Self {
            thread_id: checkpoint.thread_id.clone(),
            checkpoint_id: checkpoint.id.clone(),
            step,
            state: checkpoint.state.to_value(),
            next_nodes: serde_json::to_value(&checkpoint.next_nodes)?,
            created_at: checkpoint.created_at,
            header: Some(serde_json::to_value(&checkpoint.header)?),
            journal_ref: checkpoint
                .journal_ref
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?,
            base: checkpoint.base.clone(),
        })
    }

    /// Decode a query result row by column name, mapping decode failures to
    /// [`RustyError::Checkpoint`].
    fn from_pg_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        let get = |name: &str| format!("failed to decode column `{name}`");
        Ok(Self {
            thread_id: row
                .try_get("thread_id")
                .map_err(|e| map_sqlx_error(&get("thread_id"), e))?,
            checkpoint_id: row
                .try_get("checkpoint_id")
                .map_err(|e| map_sqlx_error(&get("checkpoint_id"), e))?,
            step: row
                .try_get("step")
                .map_err(|e| map_sqlx_error(&get("step"), e))?,
            state: row
                .try_get("state")
                .map_err(|e| map_sqlx_error(&get("state"), e))?,
            next_nodes: row
                .try_get("next_nodes")
                .map_err(|e| map_sqlx_error(&get("next_nodes"), e))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| map_sqlx_error(&get("created_at"), e))?,
            header: row
                .try_get("header")
                .map_err(|e| map_sqlx_error(&get("header"), e))?,
            journal_ref: row
                .try_get("journal_ref")
                .map_err(|e| map_sqlx_error(&get("journal_ref"), e))?,
            base: row
                .try_get("base")
                .map_err(|e| map_sqlx_error(&get("base"), e))?,
        })
    }

    /// Row -> checkpoint: `bigint` step back to `usize` (negative values are
    /// rejected), `jsonb` state back to [`State`] (must be a JSON object),
    /// `jsonb` array back to `Vec<String>`. `NULL` header/journal-ref
    /// columns (pre-R0.5 rows) decode to the serde defaults — the same
    /// reconstruction the other backends apply to old checkpoints.
    fn into_checkpoint(self) -> Result<Checkpoint> {
        let step = usize::try_from(self.step).map_err(|_| {
            RustyError::Checkpoint(format!(
                "stored step {} for checkpoint `{}` is negative; cannot map to usize",
                self.step, self.checkpoint_id
            ))
        })?;
        let state = State::from_value(self.state)?;
        let next_nodes: Vec<String> = serde_json::from_value(self.next_nodes)?;
        let header = self
            .header
            .map(serde_json::from_value::<CheckpointHeader>)
            .transpose()?
            .unwrap_or_default();
        let journal_ref = self
            .journal_ref
            .map(serde_json::from_value::<JournalRef>)
            .transpose()?;
        Ok(Checkpoint {
            id: self.checkpoint_id,
            thread_id: self.thread_id,
            step,
            state,
            next_nodes,
            created_at: self.created_at,
            header,
            journal_ref,
            base: self.base,
        })
    }
}

/// Postgres-backed [`Checkpointer`]: durable, multi-process safe, and suitable
/// for production deployments where several executor instances share one
/// database. Clone it freely — clones share the same connection pool (and the
/// same delta-head cache, so the write-side diff stays warm across clones).
///
/// This backend opts into W4 delta checkpoints ([`DeltaPolicy`]): `put`
/// diffs against the thread's materialized head and stores changed channels
/// only, bounded by the chain policy; reads fold chains transparently.
#[derive(Debug, Clone)]
pub struct PostgresCheckpointer {
    pool: PgPool,
    // The materialized chain head per thread, for the write-side delta
    // diff. Thread writes are single-writer by contract (the server's
    // activation lease; the same discipline the file backend documents), so
    // a per-process cache is coherent for its own writes; readers never
    // consult it, and a cold process rebuilds an entry by walking the
    // chain once. A foreign writer would only soften the chain bound, never
    // corrupt: a delta always names a real ancestor as its base.
    heads: Arc<Mutex<HashMap<String, DeltaHead>>>,
    policy: DeltaPolicy,
}

impl PostgresCheckpointer {
    /// Connect to Postgres at `url` (e.g.
    /// `postgres://user:pass@localhost/dbname`) and run the idempotent
    /// auto-migration (`CREATE TABLE IF NOT EXISTS rusty_checkpoints`)
    /// before returning. Connection and migration failures surface as
    /// [`RustyError::Checkpoint`].
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(|e| map_sqlx_error("connect", e))?;
        let checkpointer = Self::from_pool(pool);
        checkpointer.migrate().await?;
        Ok(checkpointer)
    }

    /// Wrap an existing pool (no migration is run — call
    /// [`PostgresCheckpointer::migrate`] explicitly if needed).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            heads: Arc::new(Mutex::new(HashMap::new())),
            policy: DeltaPolicy::default(),
        }
    }

    /// Builder-style: override the delta-chain policy
    /// ([`DeltaPolicy::full_only`] restores the pre-W4 full-snapshot write
    /// behavior). Clones made afterwards share the new policy.
    pub fn with_delta_policy(self, policy: DeltaPolicy) -> Self {
        Self { policy, ..self }
    }

    /// The delta-chain policy this checkpointer writes under.
    pub fn delta_policy(&self) -> DeltaPolicy {
        self.policy
    }

    /// The thread's materialized delta-chain head: what the next `put`
    /// diffs against, and what the server store's transactional checkpoint
    /// path (`checkpoint_and_enqueue`) uses to make the *same* encoding
    /// decision core makes — that path must write through its own
    /// transaction, so it cannot call [`Checkpointer::put`].
    ///
    /// Cached per process; a cold cache (or a thread written by another
    /// process) is rebuilt by walking the on-disk chain once.
    pub async fn delta_head(&self, thread_id: &str) -> Result<Option<DeltaHead>> {
        if let Some(head) = self.lock_heads()?.get(thread_id) {
            return Ok(Some(head.clone()));
        }
        let Some(raw) = self.fetch_latest(thread_id).await? else {
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

    /// The latest raw row (unfolded — possibly a delta), or `None`.
    async fn fetch_latest(&self, thread_id: &str) -> Result<Option<Checkpoint>> {
        let row = sqlx::query(GET_LATEST_SQL)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("get latest checkpoint", e))?;
        row.map(|r| CheckpointRow::from_pg_row(&r).and_then(CheckpointRow::into_checkpoint))
            .transpose()
    }

    /// One raw row by primary key (unfolded).
    async fn fetch_raw(&self, thread_id: &str, checkpoint_id: &str) -> Result<Option<Checkpoint>> {
        let row = sqlx::query(GET_BY_ID_SQL)
            .bind(thread_id)
            .bind(checkpoint_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("get checkpoint by id", e))?;
        row.map(|r| CheckpointRow::from_pg_row(&r).and_then(CheckpointRow::into_checkpoint))
            .transpose()
    }

    /// Walk `base` links from `tip` back to the chain root, root-first.
    /// Defensively bounded like the file backend's: writes cap chains at
    /// the policy bound, so a far longer or cyclic chain is corruption or
    /// a foreign writer — error, don't fold forever.
    async fn load_chain(&self, thread_id: &str, tip: Checkpoint) -> Result<Vec<Checkpoint>> {
        let fold_bound = self.policy.max_chain_len.saturating_mul(4).max(128);
        let mut chain = vec![tip];
        let mut visited = std::collections::HashSet::new();
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
            let base = self.fetch_raw(thread_id, &base_id).await?.ok_or_else(|| {
                RustyError::Checkpoint(format!(
                    "checkpoint delta chain in thread `{thread_id}` is broken: \
                     base `{base_id}` does not exist"
                ))
            })?;
            chain.push(base);
        }
        chain.reverse();
        Ok(chain)
    }

    /// Materialize a raw checkpoint (fold its chain when it is a delta).
    async fn materialize(&self, thread_id: &str, raw: Checkpoint) -> Result<Checkpoint> {
        if raw.base.is_none() {
            return Ok(raw);
        }
        let chain = self.load_chain(thread_id, raw).await?;
        fold_chain(&chain)
    }

    /// The shared write path. `force_full` stores a full snapshot
    /// regardless of policy — the fork compaction semantics
    /// ([`Checkpointer::fork_thread`]).
    async fn put_internal(&self, checkpoint: Checkpoint, force_full: bool) -> Result<()> {
        let encoding = if force_full {
            DeltaEncoding::full(&checkpoint)
        } else {
            let head = self.delta_head(&checkpoint.thread_id).await?;
            encode_delta(&checkpoint, head.as_ref(), &self.policy)
        };
        let row = CheckpointRow::from_checkpoint(&encoding.checkpoint)?;
        sqlx::query(INSERT_SQL)
            .bind(&row.thread_id)
            .bind(&row.checkpoint_id)
            .bind(row.step)
            .bind(&row.state)
            .bind(&row.next_nodes)
            .bind(row.created_at)
            .bind(&row.header)
            .bind(&row.journal_ref)
            .bind(&row.base)
            .execute(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("put checkpoint", e))?;
        // The next put diffs against THIS put's full state, held shared —
        // unchanged channels stay pointer-identical, which is what makes
        // the diff cheap.
        self.lock_heads()?.insert(
            checkpoint.thread_id.clone(),
            DeltaHead {
                checkpoint,
                chain_len: encoding.chain_len,
            },
        );
        Ok(())
    }

    /// Run the idempotent schema migration (base table, then the additive
    /// R0.5 column migrations, in order). Safe to call repeatedly, and safe under concurrency: a
    /// transaction-scoped advisory lock serializes concurrent migrators,
    /// avoiding the `CREATE TABLE IF NOT EXISTS` check-then-create race
    /// (duplicate key on `pg_type_typname_nsp_index`).
    pub async fn migrate(&self) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_sqlx_error("migrate", e))?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_sqlx_error("migrate", e))?;
        for statement in MIGRATION_SQL {
            sqlx::query(statement)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_sqlx_error("migrate", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| map_sqlx_error("migrate", e))?;
        Ok(())
    }

    /// The underlying connection pool, exposed so callers can run their own
    /// maintenance statements (retention sweeps, test cleanup) over the same
    /// pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// The table content-addressed artifacts are stored in (R0.7 wave 4).
pub const ARTIFACTS_TABLE_NAME: &str = "rusty_artifacts";

/// Idempotent artifact-table migration, run by
/// [`PostgresArtifactStore::connect`] / [`PostgresArtifactStore::migrate`].
/// Standalone (not part of [`MIGRATION_SQL`]): the artifact store is a
/// separate subsystem with its own lifecycle — a database can carry
/// checkpoints without artifacts.
const CREATE_ARTIFACTS_TABLE_SQL: &str = "\
CREATE TABLE IF NOT EXISTS rusty_artifacts (
    sha256     text        NOT NULL PRIMARY KEY,
    payload    bytea       NOT NULL,
    created_at timestamptz NOT NULL
)";

/// Artifact insert: `ON CONFLICT DO NOTHING` — dedupe by construction. The
/// hash is the identity, so a re-put of the same content is a no-op (the
/// exact opposite of the checkpoint no-overwrite contract, and deliberately
/// so: artifact-addressed retry traffic must be cheap).
const INSERT_ARTIFACT_SQL: &str = "\
INSERT INTO rusty_artifacts (sha256, payload, created_at)
VALUES ($1, $2, $3)
ON CONFLICT (sha256) DO NOTHING";

/// Point lookup by content address.
const GET_ARTIFACT_SQL: &str = "\
SELECT payload FROM rusty_artifacts WHERE sha256 = $1";

/// Existence check by content address.
const HAS_ARTIFACT_SQL: &str = "\
SELECT EXISTS (SELECT 1 FROM rusty_artifacts WHERE sha256 = $1)";

/// Byte deletion by content address (R0.12 wave 2): the retention
/// sweeper's prune. `rows_affected` is the converge signal — a retried
/// prune of an already-deleted address affects zero rows.
const DELETE_ARTIFACT_SQL: &str = "\
DELETE FROM rusty_artifacts WHERE sha256 = $1";

/// Postgres-backed [`ArtifactStore`] (R0.7 wave 4): content-addressed blobs
/// in `rusty_artifacts`, suitable for multi-process deployments where the
/// file backend's one-writer precondition does not hold. Clone freely —
/// clones share the connection pool.
///
/// Garbage collection was deliberately absent in R0.7: reachability from
/// live threads, tasks, and journals is an operator decision (a reaper
/// deleting an artifact a crashed run still needs is a durability bug).
/// R0.12 wave 2 adds [`ArtifactStore::delete`] as the *mechanism*; the
/// decision stays with the retention sweeper's pinning rule, which prunes
/// an address only when no unexpired record and no verified signed
/// receipt still names it.
#[derive(Debug, Clone)]
pub struct PostgresArtifactStore {
    pool: PgPool,
}

impl PostgresArtifactStore {
    /// Connect to Postgres at `url` and run the idempotent auto-migration
    /// (`CREATE TABLE IF NOT EXISTS rusty_artifacts`) before returning.
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(|e| map_sqlx_error("connect", e))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Wrap an existing pool (no migration is run — call
    /// [`PostgresArtifactStore::migrate`] explicitly if needed).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run the idempotent artifact-table migration. Safe to call
    /// repeatedly: a single `CREATE TABLE IF NOT EXISTS`.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(CREATE_ARTIFACTS_TABLE_SQL)
            .execute(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("migrate artifacts table", e))?;
        Ok(())
    }

    /// The underlying connection pool, for caller-run maintenance
    /// (retention sweeps, test cleanup).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl ArtifactStore for PostgresArtifactStore {
    async fn put(&self, bytes: &[u8]) -> Result<ArtifactRef> {
        let reference = ArtifactRef {
            sha256: sha256_hex(bytes),
            bytes: bytes.len() as u64,
        };
        sqlx::query(INSERT_ARTIFACT_SQL)
            .bind(&reference.sha256)
            .bind(bytes)
            .bind(Utc::now())
            .execute(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("put artifact", e))?;
        Ok(reference)
    }

    async fn get(&self, sha256: &str) -> Result<Vec<u8>> {
        let row = sqlx::query(GET_ARTIFACT_SQL)
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("get artifact", e))?;
        let bytes: Vec<u8> = row
            .map(|r| r.try_get("payload"))
            .transpose()
            .map_err(|e| map_sqlx_error("decode artifact payload", e))?
            .ok_or_else(|| RustyError::Checkpoint(format!("artifact `{sha256}` does not exist")))?;
        // Integrity is verified on every read: the returned bytes re-hash
        // to the requested address, or this is corruption, not data.
        let actual = sha256_hex(&bytes);
        if actual != sha256 {
            return Err(RustyError::Serialization(serde_json::Error::io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "artifact integrity failure: `{sha256}` re-hashes to `{actual}`; \
                         the stored bytes are corrupt"
                    ),
                ),
            )));
        }
        Ok(bytes)
    }

    async fn contains(&self, sha256: &str) -> Result<bool> {
        sqlx::query_scalar(HAS_ARTIFACT_SQL)
            .bind(sha256)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("check artifact", e))
    }

    async fn delete(&self, sha256: &str) -> Result<bool> {
        let result = sqlx::query(DELETE_ARTIFACT_SQL)
            .bind(sha256)
            .execute(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("delete artifact", e))?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait]
impl Checkpointer for PostgresCheckpointer {
    async fn put(&self, checkpoint: Checkpoint) -> Result<()> {
        self.put_internal(checkpoint, false).await
    }

    async fn get_latest(&self, thread_id: &str) -> Result<Option<Checkpoint>> {
        let Some(raw) = self.fetch_latest(thread_id).await? else {
            return Ok(None);
        };
        self.materialize(thread_id, raw).await.map(Some)
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<Checkpoint>> {
        let rows = sqlx::query(LIST_SQL)
            .bind(thread_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_sqlx_error("list checkpoints", e))?;
        let raw: Vec<Checkpoint> = rows
            .iter()
            .map(|r| CheckpointRow::from_pg_row(r).and_then(CheckpointRow::into_checkpoint))
            .collect::<Result<_>>()?;
        // Fold every delta chain, then restore the statement's total order
        // (`materialize_all` resolves chains out of order).
        let mut all = materialize_all(raw);
        all.sort_by(|a, b| {
            a.step
                .cmp(&b.step)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.id.cmp(&b.id))
        });
        Ok(all)
    }

    async fn get_by_id(&self, thread_id: &str, checkpoint_id: &str) -> Result<Option<Checkpoint>> {
        let Some(raw) = self.fetch_raw(thread_id, checkpoint_id).await? else {
            return Ok(None);
        };
        self.materialize(thread_id, raw).await.map(Some)
    }

    /// Forked histories are written as **full snapshots** (eager
    /// compaction): a fork is a new timeline and the natural compaction
    /// point, so time-travel reads on the fork never fold a chain — and
    /// never another timeline's.
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

    fn sample_checkpoint() -> Checkpoint {
        let mut state = State::new();
        state.insert("answer", serde_json::json!(42));
        state.insert(
            "messages",
            serde_json::json!([{"role": "user", "content": "hi"}]),
        );
        Checkpoint::new("thread-1", 3, state, vec!["node_b".into(), "node_c".into()])
    }

    // ---- SQL generation (no database required) ----

    #[test]
    fn create_table_sql_matches_schema_contract() {
        assert!(CREATE_TABLE_SQL.starts_with("CREATE TABLE IF NOT EXISTS rusty_checkpoints"));
        for column in [
            "thread_id     text",
            "checkpoint_id text",
            "step          bigint",
            "state         jsonb",
            "next_nodes    jsonb",
            "created_at    timestamptz",
            "PRIMARY KEY (thread_id, checkpoint_id)",
        ] {
            assert!(
                CREATE_TABLE_SQL.contains(column),
                "migration SQL is missing `{column}`:\n{CREATE_TABLE_SQL}"
            );
        }
    }

    #[test]
    fn insert_sql_binds_all_columns_without_upsert() {
        assert!(INSERT_SQL.starts_with("INSERT INTO rusty_checkpoints"));
        assert!(INSERT_SQL.contains(
            "(thread_id, checkpoint_id, step, state, next_nodes, created_at, header, journal_ref, base)"
        ));
        assert!(INSERT_SQL.contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        // The no-overwrite contract relies on the PK violation erroring out.
        assert!(!INSERT_SQL.to_uppercase().contains("ON CONFLICT"));
    }

    #[test]
    fn migration_is_create_then_additive_idempotent_alters() {
        // The base table is created first; additive column migrations are
        // appended in order (R0.5 header/journal columns, W4 base column).
        assert_eq!(MIGRATION_SQL.len(), 4);
        assert!(MIGRATION_SQL[0].starts_with("CREATE TABLE IF NOT EXISTS rusty_checkpoints"));
        assert!(MIGRATION_SQL[1].contains("ADD COLUMN IF NOT EXISTS header jsonb"));
        assert!(MIGRATION_SQL[2].contains("ADD COLUMN IF NOT EXISTS journal_ref jsonb"));
        assert!(MIGRATION_SQL[3].contains("ADD COLUMN IF NOT EXISTS base text"));
        // Every statement is idempotent, so re-running the migration (or
        // running it against a partially migrated database) is safe.
        for statement in MIGRATION_SQL {
            assert!(
                statement.to_uppercase().contains("IF NOT EXISTS"),
                "migration statement must be idempotent: {statement}"
            );
            assert!(
                statement.contains("rusty_checkpoints"),
                "migration statement must target the checkpoints table: {statement}"
            );
        }
        // The ALTERs are additive only: no DROP, no retype, no NOT NULL —
        // older code reading or writing the table keeps working (rollback-
        // safe), and pre-R0.5 rows stay valid (forward-safe).
        for statement in &MIGRATION_SQL[1..] {
            let upper = statement.to_uppercase();
            assert!(upper.starts_with("ALTER TABLE"));
            assert!(!upper.contains("DROP"), "got: {statement}");
            assert!(!upper.contains("NOT NULL"), "got: {statement}");
        }
    }

    #[test]
    fn get_latest_sql_uses_insertion_order_recency() {
        assert!(GET_LATEST_SQL.contains("WHERE thread_id = $1"));
        // Recency = insertion order (the trait contract): `created_at` is the
        // insertion proxy; `step` must NOT appear in the ordering, so replay
        // appending lower-step checkpoints still resumes the newest timeline.
        assert!(GET_LATEST_SQL.contains("ORDER BY created_at DESC, checkpoint_id DESC"));
        assert!(!GET_LATEST_SQL.contains("step DESC"));
        assert!(GET_LATEST_SQL.contains("LIMIT 1"));
    }

    #[test]
    fn list_sql_orders_by_step_with_total_tie_break() {
        assert!(LIST_SQL.contains("WHERE thread_id = $1"));
        // Same-step duplicates (same-thread replay) must order
        // deterministically: fork_thread truncates by list position.
        assert!(LIST_SQL.contains("ORDER BY step ASC, created_at ASC, checkpoint_id ASC"));
        assert!(!LIST_SQL.contains("LIMIT"));
    }

    #[test]
    fn get_by_id_sql_is_a_primary_key_point_lookup() {
        assert!(GET_BY_ID_SQL.contains("WHERE thread_id = $1 AND checkpoint_id = $2"));
        assert!(GET_BY_ID_SQL.starts_with("SELECT"));
    }

    #[test]
    fn read_statements_select_the_r05_columns() {
        for sql in [GET_LATEST_SQL, LIST_SQL, GET_BY_ID_SQL] {
            assert!(
                sql.contains("header"),
                "statement must read `header`: {sql}"
            );
            assert!(
                sql.contains("journal_ref"),
                "statement must read `journal_ref`: {sql}"
            );
            // W4: reads must also fetch the delta-chain link so
            // materialization can fold.
            assert!(sql.contains("base"), "statement must read `base`: {sql}");
            // Explicit column lists, never `SELECT *`: added columns must not
            // change what older statements return (and vice versa).
            assert!(!sql.contains("SELECT *"), "got: {sql}");
        }
    }

    #[test]
    fn statements_use_bound_parameters_only() {
        for sql in [INSERT_SQL, GET_LATEST_SQL, LIST_SQL, GET_BY_ID_SQL] {
            assert!(sql.contains("$1"), "statement must bind parameters: {sql}");
        }
    }

    // ---- Serde / row mapping (no database required) ----

    #[test]
    fn checkpoint_row_roundtrip_preserves_all_fields() {
        let checkpoint = sample_checkpoint();
        let row = CheckpointRow::from_checkpoint(&checkpoint).unwrap();

        assert_eq!(row.thread_id, checkpoint.thread_id);
        assert_eq!(row.checkpoint_id, checkpoint.id);
        assert_eq!(row.step, checkpoint.step as i64);
        assert!(row.state.is_object());
        assert!(row.next_nodes.is_array());

        let back = row.into_checkpoint().unwrap();
        assert_eq!(back.id, checkpoint.id);
        assert_eq!(back.thread_id, checkpoint.thread_id);
        assert_eq!(back.step, checkpoint.step);
        assert_eq!(back.state, checkpoint.state);
        assert_eq!(back.next_nodes, checkpoint.next_nodes);
        assert_eq!(back.created_at, checkpoint.created_at);
        assert_eq!(back.header, checkpoint.header);
        assert_eq!(back.journal_ref, checkpoint.journal_ref);
    }

    /// An R0.5 checkpoint with a stamped provenance header and a journal
    /// reference round-trips through the row mapping unchanged.
    #[test]
    fn checkpoint_row_roundtrip_preserves_header_and_journal_ref() {
        let mut checkpoint = sample_checkpoint();
        checkpoint.header = CheckpointHeader {
            format_version: crate::record::CURRENT_FORMAT_VERSION,
            graph_version: "react-v3".into(),
            graph_hash: "5d41402abc4b2a76b9719d911017c592".into(),
            policy_version: Default::default(),
            logical_clock: 1_700_000_000_042,
            manifest: None,
        };
        checkpoint.journal_ref = Some(JournalRef {
            events: 17,
            sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
        });

        let row = CheckpointRow::from_checkpoint(&checkpoint).unwrap();
        assert_eq!(
            row.header.as_ref().and_then(|h| h.get("graph_version")),
            Some(&serde_json::json!("react-v3"))
        );
        assert_eq!(
            row.journal_ref.as_ref().and_then(|r| r.get("events")),
            Some(&serde_json::json!(17))
        );

        let back = row.into_checkpoint().unwrap();
        assert_eq!(back.header, checkpoint.header);
        assert_eq!(back.journal_ref, checkpoint.journal_ref);
    }

    /// Rows written before the R0.5 migration (or by older code against a
    /// migrated table) carry `NULL` header/journal_ref; decoding them must
    /// reproduce the serde defaults — the same reconstruction the other
    /// backends apply to pre-R0.5 checkpoints.
    #[test]
    fn null_r05_columns_decode_to_serde_defaults() {
        let mut row = CheckpointRow::from_checkpoint(&sample_checkpoint()).unwrap();
        row.header = None;
        row.journal_ref = None;

        let back = row.into_checkpoint().unwrap();
        assert_eq!(back.header, CheckpointHeader::default());
        assert_eq!(back.journal_ref, None);
    }

    /// The W4 delta-chain link round-trips through the row mapping; `NULL`
    /// (pre-W4 rows) decodes to `None` — a full snapshot.
    #[test]
    fn base_column_roundtrips_and_null_decodes_to_full_snapshot() {
        let mut checkpoint = sample_checkpoint();
        assert_eq!(checkpoint.base, None);
        let row = CheckpointRow::from_checkpoint(&checkpoint).unwrap();
        assert_eq!(row.base, None);
        assert_eq!(row.into_checkpoint().unwrap().base, None);

        checkpoint.base = Some("parent-checkpoint-id".into());
        let row = CheckpointRow::from_checkpoint(&checkpoint).unwrap();
        assert_eq!(row.base.as_deref(), Some("parent-checkpoint-id"));
        assert_eq!(
            row.into_checkpoint().unwrap().base.as_deref(),
            Some("parent-checkpoint-id")
        );
    }

    // ---- Artifact store SQL shape (no database required) ----

    #[test]
    fn artifact_statements_are_idempotent_and_bound() {
        assert!(
            CREATE_ARTIFACTS_TABLE_SQL.starts_with("CREATE TABLE IF NOT EXISTS rusty_artifacts")
        );
        for column in ["sha256", "payload", "created_at"] {
            assert!(
                CREATE_ARTIFACTS_TABLE_SQL.contains(column),
                "artifact table is missing `{column}`"
            );
        }
        // Dedupe by construction: the hash is the primary key and re-puts
        // are no-ops — the opposite of the checkpoint no-overwrite contract.
        assert!(INSERT_ARTIFACT_SQL.contains("ON CONFLICT (sha256) DO NOTHING"));
        for sql in [INSERT_ARTIFACT_SQL, GET_ARTIFACT_SQL, HAS_ARTIFACT_SQL] {
            assert!(sql.contains("$1"), "statement must bind parameters: {sql}");
            assert!(!sql.contains("SELECT *"), "got: {sql}");
        }
    }

    /// A stored header that fails to deserialize is a hard error, not a
    /// silent default: provenance must never be dropped quietly.
    #[test]
    fn malformed_header_json_is_rejected() {
        let mut row = CheckpointRow::from_checkpoint(&sample_checkpoint()).unwrap();
        row.header = Some(serde_json::json!({"format_version": "not-a-number"}));
        assert!(row.into_checkpoint().is_err());
    }

    #[test]
    fn row_with_negative_step_is_rejected() {
        let mut row = CheckpointRow::from_checkpoint(&sample_checkpoint()).unwrap();
        row.step = -1;
        let err = row.into_checkpoint().unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));
    }

    #[test]
    fn row_with_non_object_state_is_rejected() {
        let mut row = CheckpointRow::from_checkpoint(&sample_checkpoint()).unwrap();
        row.state = serde_json::json!([1, 2, 3]);
        assert!(row.into_checkpoint().is_err());
    }

    #[test]
    fn row_with_non_array_next_nodes_is_rejected() {
        let mut row = CheckpointRow::from_checkpoint(&sample_checkpoint()).unwrap();
        row.next_nodes = serde_json::json!("not-an-array");
        assert!(row.into_checkpoint().is_err());
    }

    // ---- Live integration test (opt-in; requires a real Postgres) ----

    /// Full roundtrip against a live database. Not run by default:
    /// `cargo test --features postgres -- --ignored` with `DATABASE_URL` set.
    #[tokio::test]
    #[ignore = "requires a live Postgres; set DATABASE_URL to run"]
    async fn postgres_live_roundtrip() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set; skipping live Postgres test");
            return;
        };
        let store = PostgresCheckpointer::connect(&url).await.unwrap();

        // The auto-migration added the R0.5 columns (and is re-runnable:
        // `connect` migrates every time, including against a database that
        // was already migrated).
        for column in ["header", "journal_ref"] {
            let present: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM information_schema.columns
                     WHERE table_name = 'rusty_checkpoints' AND column_name = $1
                 )",
            )
            .bind(column)
            .fetch_one(store.pool())
            .await
            .unwrap();
            assert!(present, "migration must add column `{column}`");
        }

        // Isolate the test in its own thread id.
        let thread = format!("test-{}", uuid::Uuid::new_v4());
        assert!(store.get_latest(&thread).await.unwrap().is_none());
        assert!(store.list(&thread).await.unwrap().is_empty());

        let cp0 = sample_checkpoint_with(&thread, 0);
        let cp1 = sample_checkpoint_with(&thread, 1);
        store.put(cp0.clone()).await.unwrap();
        store.put(cp1.clone()).await.unwrap();

        // Latest = most recent put (insertion-order recency).
        let latest = store.get_latest(&thread).await.unwrap().unwrap();
        assert_eq!(latest.id, cp1.id);

        // List = ascending steps.
        let all = store.list(&thread).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, cp0.id);
        assert_eq!(all[1].id, cp1.id);
        assert_eq!(all[0].state, cp0.state);
        assert_eq!(all[1].next_nodes, cp1.next_nodes);

        // Duplicate (thread_id, checkpoint_id) is rejected.
        let err = store.put(cp1).await.unwrap_err();
        assert!(matches!(err, RustyError::Checkpoint(_)));

        // R0.5 provenance: a checkpoint with a stamped header and journal
        // reference round-trips byte-for-byte through jsonb.
        let mut cp2 = sample_checkpoint_with(&thread, 2);
        cp2.header.graph_version = "live-test-v1".into();
        cp2.header.graph_hash = "5d41402abc4b2a76b9719d911017c592".into();
        cp2.header.logical_clock = 42;
        cp2.journal_ref = Some(JournalRef {
            events: 9,
            sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
        });
        store.put(cp2.clone()).await.unwrap();
        let back = store
            .get_by_id(&thread, &cp2.id)
            .await
            .unwrap()
            .expect("checkpoint stored above");
        assert_eq!(back.header, cp2.header);
        assert_eq!(back.journal_ref, cp2.journal_ref);

        // A row written by *older* code (no header/journal_ref columns in
        // its INSERT — simulated with the pre-R0.5 statement shape) reads
        // back with serde defaults, not an error.
        let cp3 = sample_checkpoint_with(&thread, 3);
        sqlx::query(
            "INSERT INTO rusty_checkpoints
                (thread_id, checkpoint_id, step, state, next_nodes, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&cp3.thread_id)
        .bind(&cp3.id)
        .bind(cp3.step as i64)
        .bind(cp3.state.to_value())
        .bind(serde_json::to_value(&cp3.next_nodes).unwrap())
        .bind(cp3.created_at)
        .execute(store.pool())
        .await
        .unwrap();
        let back = store
            .get_by_id(&thread, &cp3.id)
            .await
            .unwrap()
            .expect("legacy row inserted above");
        assert_eq!(back.header, CheckpointHeader::default());
        assert_eq!(back.journal_ref, None);

        // ---- R0.7 wave 4: delta checkpoints against the live database ----
        //
        // A second thread whose checkpoints share a large unchanged channel:
        // the second put must persist a *delta* row (base set, only the
        // changed channel in `state`), while reads fold the chain back to a
        // full snapshot.
        let delta_thread = format!("test-delta-{}", uuid::Uuid::new_v4());
        let blob = "x".repeat(4096);
        let mut state_a = State::new();
        state_a.insert("blob", serde_json::json!(blob));
        state_a.insert("answer", serde_json::json!(0));
        let cp_a = Checkpoint::new(&delta_thread, 0, state_a, vec!["next".into()]);
        let mut state_b = State::new();
        state_b.insert("blob", serde_json::json!(blob));
        state_b.insert("answer", serde_json::json!(1));
        let cp_b = Checkpoint::new(&delta_thread, 1, state_b, vec!["next".into()]);
        store.put(cp_a.clone()).await.unwrap();
        store.put(cp_b.clone()).await.unwrap();

        // The raw row for cp_b is a delta: base names cp_a and the stored
        // state carries only the changed channel.
        let (raw_base, raw_state): (Option<String>, Value) = sqlx::query_as(
            "SELECT base, state FROM rusty_checkpoints
             WHERE thread_id = $1 AND checkpoint_id = $2",
        )
        .bind(&delta_thread)
        .bind(&cp_b.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(raw_base.as_deref(), Some(cp_a.id.as_str()));
        assert!(raw_state.get("answer").is_some());
        assert!(
            raw_state.get("blob").is_none(),
            "delta row must not re-store the unchanged channel: {raw_state}"
        );

        // Reads fold the chain: get_latest/get_by_id return full snapshots
        // with `base` cleared.
        let latest = store.get_latest(&delta_thread).await.unwrap().unwrap();
        assert_eq!(latest.id, cp_b.id);
        assert_eq!(latest.state, cp_b.state);
        assert_eq!(latest.base, None);
        let folded = store
            .get_by_id(&delta_thread, &cp_b.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(folded.state, cp_b.state);

        // Forks compact eagerly: every copied row is a full snapshot.
        let fork_thread = format!("test-fork-{}", uuid::Uuid::new_v4());
        let copied = store
            .fork_thread(&delta_thread, &fork_thread, None)
            .await
            .unwrap();
        assert_eq!(copied, 2);
        let dangling: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM rusty_checkpoints
             WHERE thread_id = $1 AND base IS NOT NULL",
        )
        .bind(&fork_thread)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(dangling, 0, "fork must write full snapshots only");
        let forked_latest = store.get_latest(&fork_thread).await.unwrap().unwrap();
        assert_eq!(forked_latest.state, cp_b.state);

        // ---- R0.7 wave 4: content-addressed artifact store ----
        let artifacts = PostgresArtifactStore::from_pool(store.pool().clone());
        artifacts.migrate().await.unwrap();
        let payload = b"live-test artifact payload".to_vec();
        let reference = artifacts.put(&payload).await.unwrap();
        assert!(artifacts.contains(&reference.sha256).await.unwrap());
        assert_eq!(artifacts.get(&reference.sha256).await.unwrap(), payload);
        // Re-put dedupes: same content, same reference, still one row.
        let again = artifacts.put(&payload).await.unwrap();
        assert_eq!(again, reference);
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rusty_artifacts WHERE sha256 = $1")
                .bind(&reference.sha256)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(rows, 1);
        // Integrity on read: a tampered payload is rejected, not returned.
        sqlx::query("UPDATE rusty_artifacts SET payload = $2 WHERE sha256 = $1")
            .bind(&reference.sha256)
            .bind(b"tampered".as_slice())
            .execute(store.pool())
            .await
            .unwrap();
        assert!(artifacts.get(&reference.sha256).await.is_err());

        // Cleanup.
        sqlx::query("DELETE FROM rusty_checkpoints WHERE thread_id = ANY($1)")
            .bind(vec![thread, delta_thread, fork_thread])
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM rusty_artifacts WHERE sha256 = $1")
            .bind(&reference.sha256)
            .execute(store.pool())
            .await
            .unwrap();
    }

    fn sample_checkpoint_with(thread: &str, step: usize) -> Checkpoint {
        let mut state = State::new();
        state.insert("answer", serde_json::json!(step));
        Checkpoint::new(thread, step, state, vec!["next".into()])
    }
}
