# Recovery objectives

This document states the recovery-point objective (RPO) and recovery-time
objective (RTO) for Rusty deployments, and how they are measured rather than
asserted.

## RPO — Recovery Point Objective

| Scenario | RPO | Basis |
|---|---|---|
| Committed events | **Zero** | `synchronous_commit = remote_apply` guarantees that an acknowledged commit is on durable WAL on the standby before the client receives success. |
| Archive lag | **≤ 60 s** | Deployment-side WAL-archive monitoring is alerted at this bound (see `docs/backup.md`); Rusty does not export this metric itself. |
| Object-store blob | **Zero** | Bucket versioning retains every version; a deleted blob is recoverable. |

## RTO — Recovery Time Objective

These are **targets**, not measurements. A scheduled restore-rehearsal job
that would publish actual timings does not exist yet; until it does, treat the
numbers below as the bar a rehearsal must clear, and run the restore procedure
by hand on a schedule.

| Topology | Target RTO | Measurement |
|---|---|---|
| Single-node (development / small production) | **≤ 30 minutes** | Rehearse by hand: backup → destroy → restore → `rustyness verify-log` → replay seeded sessions, and time it. |
| HA topology (M4) | **≤ 5 minutes** | The kill-a-node drill in `rusty-server/tests/fault_injection.rs` exercises worker failover and lease re-acquisition. |

## Restore procedure

1. **Restore base** — download the latest base backup from the object store.
2. **Replay WAL** — replay archived WAL segments to the desired point in time.
3. **Verify log** — run `rustyness verify-log` against every journal snapshot:
   * gap-free positions,
   * paired turn events,
   * artifact locator resolution (`--artifacts` flag).
4. **Replay sessions** — replay three seeded sessions and compare derived
   transcripts to pre-destruction captures.
5. **Resume paused runs** — confirm that a run paused across the destruction
   resumes correctly (EP-03-S07).

A restore is valid only when the log verifies.  Checkpoints, traces, and
windows are recomputable projections; the log is the sole source of truth.

## Honesty

RPO and RTO are targets, not published measurements. The rehearsal job that
would publish timings — and fail when a measured RTO misses its target — is
not wired yet; the restore procedure above is the rehearsal, run it on a
schedule and record what you measure.
