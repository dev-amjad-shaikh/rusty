# Recovery objectives

This document states the recovery-point objective (RPO) and recovery-time
objective (RTO) for Rusty deployments, and how they are measured rather than
asserted.

## RPO — Recovery Point Objective

| Scenario | RPO | Basis |
|---|---|---|
| Committed events | **Zero** | `synchronous_commit = remote_apply` guarantees that an acknowledged commit is on durable WAL on the standby before the client receives success. |
| Archive lag | **≤ 60 s** | The `rusty_backup_archive_lag_seconds` metric is alerted at this bound; actual lag is published beside the objective. |
| Object-store blob | **Zero** | Bucket versioning retains every version; a deleted blob is recoverable. |

## RTO — Recovery Time Objective

| Topology | Target RTO | Measurement |
|---|---|---|
| Single-node (development / small production) | **≤ 30 minutes** | Measured by the scheduled restore-rehearsal CI job: backup → destroy → restore → `verify-log` → replay seeded sessions. The time is published as a CI artifact. |
| HA topology (M4) | **≤ 5 minutes** | Measured by the kill-a-node drill in `rusty-server/tests/fault_injection.rs`; worker failover plus lease re-acquisition is timed and bounded. |

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

RPO and RTO are not marketing numbers.  The rehearsal job publishes its timing
on every run; if the measured RTO exceeds the target, the job fails and the
deployment team is paged.
