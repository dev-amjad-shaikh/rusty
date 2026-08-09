# Stability Contract (v0.x)

What Rusty guarantees today, what it does not, and how that changes at
R1.0. This document is a contract, not an aspiration: if something is not
listed under "stable", assume it can change in the next minor release.

Version numbers and the compatibility matrix live in
[versioning.md](versioning.md); release history lives in
[../CHANGELOG.md](../CHANGELOG.md).

## Stable today

Three surfaces carry a stability guarantee at v0.x. Breaking any one of
them is treated as a protocol-level event, not a routine release note.

**1. The remote-execution wire protocol (v1).**
The `NodeTask` / `TaskResult` exchange between a `RemoteNode` and a
`rusty-worker` over `POST /execute` is governed by `PROTOCOL_VERSION`
(currently `1`, defined in `rusty-core/src/remote.rs`). Within protocol
v1, changes are additive-only:

- Workers must reject tasks whose `protocol_version` they do not support
  — never silently misinterpret one.
- Responses are accepted regardless of their version field, so a newer
  worker can serve an older caller.
- A field removal, rename, or meaning change requires bumping
  `PROTOCOL_VERSION` to 2 and shipping worker support before callers.

**2. The checkpoint format, within a minor version line.**
Checkpoints written by `JsonFileCheckpointer` or `PostgresCheckpointer`
are serde-JSON serializations of the `Checkpoint` struct
(`rusty-core/src/checkpoint.rs`). The guarantee is narrow and precise:

- A checkpoint written by any `rusty-agent-runtime` `0.x.*` release is
  readable by every other `0.x.*` release in that same minor line —
  including restore, `get_by_id` replay, and `fork_thread` / time-travel
  forks.
- Across a minor bump (`0.x → 0.x+1`) the struct may change. When it
  does, the CHANGELOG entry for that release says so and ships a
  migration path where one exists. There is no guarantee that a newer
  runtime reads older checkpoints, and no guarantee in the other
  direction either.

**3. The Flight Recorder evidence formats (format_version 1).**
The wire shapes defined in `rusty-core/src/record.rs` — `RunEvent` (with
`RunEventKind`, `EventStatus`, `PayloadRef`, `ArtifactRef`), the
`Effect` taxonomy, `DecisionEvent`, and the `CheckpointHeader` stamped
into every checkpoint — together with the `JournalSnapshot` export form
(`rusty-core/src/journal.rs`) and the `ReplayFixture` envelope
(`rusty-core/src/replay.rs`, `FIXTURE_FORMAT_VERSION` = 1) are pinned by
golden-file tests (`rusty-core/tests/golden/`); accidental drift fails
CI. The guarantee mirrors the checkpoint guarantee, within a minor
version line:

- Within a `rusty-agent-runtime` `0.x.*` minor line, these shapes evolve
  additively via serde defaults: journals, fixtures, and checkpoint
  headers written by one release in the line deserialize under every
  other release in the line. Pre-R0.5 checkpoints (no header)
  deserialize into `CheckpointHeader::default()` — that fallback is part
  of the contract.
- `ReplayFixture::import` rejects an unsupported `format_version` at the
  boundary rather than misreading it; the same boundary-rejection rule
  applies to checkpoint headers if `CURRENT_FORMAT_VERSION` ever leaves
  1.
- A non-additive change to any of these shapes (a field removal,
  rename, or meaning change) is a minor-release event recorded in the
  CHANGELOG with a migration path where one exists.

## Not stable — may change in any 0.x minor release

Everything below can break between minor versions. Every such change is
recorded in the CHANGELOG under the release that makes it, but no
deprecation window is implied unless the entry says one.

- **The Rust API surface of all four crates** — public types, traits,
  function signatures, feature flags, and module layout of
  `rusty-agent-runtime`, `rusty-agent-server`, `rusty-worker`, and
  `rusty-otel`. Pin an exact version (`=0.x.y`) if rebuilds must not
  break.
- **HTTP request/response JSON fields** on the `rusty-agent-server` API. Route
  paths have been additive historically, but field-level additions,
  renames, and removals may occur at a minor bump.
- **SSE event families and their payload fields.** The stream currently
  emits `metadata`, `values`, `updates`, `error`, and `end` frames
  (default `stream_mode` is `["values", "updates"]`; `metadata`, `error`,
  and `end` are always emitted — see `rusty-server/src/runs.rs`). New
  families or fields may appear, and payload shapes may change, at any
  minor bump. Clients must ignore unknown events and unknown fields.
- **SDK class and function shapes** — `RustyClient` / `RustyError` /
  `SSEEvent` in the Python SDK and the exported surface of
  `@rusty-runtime/client`. Method names, signatures, and error-mapping
  behavior may change at a minor bump.
- **Studio** (`studio/`) — it is a debug UI, not an API. Its behavior
  and internals change freely.
- **Tenant-isolation internals** — the `{tenant}/` id-prefixing scheme
  is an implementation detail. Cross-tenant semantics (404, never 403)
  are intended behavior, but the prefix layout is not a public format.

## Deprecation policy

At 0.x, deprecation is a CHANGELOG commitment, not a code-level
mechanism:

- A feature the maintainers intend to remove is marked **deprecated** in
  the CHANGELOG entry of the release that deprecates it.
- Where feasible, removal lands no sooner than the following minor
  release. "Where feasible" excludes security fixes and correctness
  bugs, which may change behavior immediately.
- The wire protocol is exempt from this discretion: its rules (above)
  always apply.

There is no `#[deprecated]` lint guarantee across the Rust API at 0.x —
do not rely on compiler warnings as the deprecation channel. The
CHANGELOG is the channel.

## What changes at R1.0

R1.0 — Unleashed (see [roadmap.md](roadmap.md)) flips the default from
"may break" to "must not break" for the public surface:

- **Full SemVer.** Breaking changes require a major bump. This applies
  to the Rust API of all four crates, the HTTP/SSE API, and both SDK
  surfaces.
- **The HTTP/SSE API becomes a versioned, stable surface.** The
  server↔SDK pairing rule from [versioning.md](versioning.md) is
  replaced by an explicit compatibility declaration; the current
  same-cycle pairing requirement goes away.
- **Checkpoint migrations are guaranteed.** A 1.x runtime reads
  checkpoints written by any earlier 1.x release; a migration path is
  provided across the 0.x → 1.0 boundary for the documented
  checkpointers.
- **MSRV bumps become minor-release events.** The MSRV (currently 1.86,
  see [versioning.md](versioning.md#msrv)) may only rise in a minor
  release, never in a patch.
- **Deprecation gains teeth.** Public API removals are preceded by at
  least one minor release of `#[deprecated]` warnings (Rust) or
  documented deprecation notices (SDKs, HTTP fields).

Until R1.0 ships, the v0.x rules above are the whole contract.
