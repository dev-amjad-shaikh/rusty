# Backup configuration

This document describes the backup topology for Rusty deployments.

## What is backed up

| Tier | Mechanism | Destination |
|---|---|---|
| PostgreSQL data | periodic base backup + continuous WAL archiving | object store (S3-compatible) |
| Blob store | bucket versioning enabled natively | object store (S3-compatible) |
| Journal snapshots | embedded artifact map or `FileArtifactStore` | colocated with PostgreSQL or spilled to object store |

## Postgres base backups

A base backup is a consistent snapshot of the PostgreSQL data directory.  The
reference deployment runs `pg_basebackup` on a schedule (default: daily at
02:00 local time) and uploads the resulting tarball to the object store under
`backups/base/{timestamp}/`.

## WAL archiving

WAL segments are archived continuously via `archive_command` to the object
store under `backups/wal/{timeline}/`.  Archiving is synchronous with respect
to the Postgres transaction commit when `synchronous_commit = remote_apply` is
configured, giving an RPO of zero for committed events.

## Archive lag metric and alert

The `rustyness` exporter exposes:

```
rusty_backup_archive_lag_seconds{tenant="..."}
```

This is the wall-clock time between the last WAL segment successfully archived
and the current Postgres `pg_current_wal_lsn()`.  An alert fires when the lag
exceeds the configured bound (default: 60 seconds).

## Object-store bucket versioning

The blob store bucket has versioning enabled so that an overwritten or
deleted object can be recovered.  Versioning is a bucket policy, not a Rusty
application setting; it must be confirmed at deployment time.

## Verification

After every base backup, the restore-rehearsal CI job (see `docs/recovery.md`)
restores the backup to a fresh environment and runs `rustyness verify-log` to
confirm that journal snapshots are intact and that every artifact reference
resolves.
