# Versioning and Compatibility

How Rusty packages are versioned, which versions are current, and which
version numbers govern which compatibility boundary. For what each package
promises not to break, see [stability.md](stability.md).

## Policy

**Packages version independently.** There is no single "Rusty version".
Each crate bumps its own version in its own `Cargo.toml`; each SDK bumps
its own package manifest. The root [CHANGELOG.md](../CHANGELOG.md) groups
implemented work into named releases (R0.1 — Ignition through R0.12 —
Operations Plane, with the v0.5 SDK/tenancy cycle in between) purely as a
branding and history layer — a named release does not imply a shared
version number.

**Pre-1.0 SemVer.** All packages are `0.x`. Within `0.x`:

- A **minor** bump (`0.x.0 → 0.x+1.0`) may contain breaking changes. Every
  breaking change is recorded in the root CHANGELOG under that release.
- A **patch** bump (`0.x.y → 0.x.y+1`) is fixes only — no API or
  wire-format changes.

**The wire protocol versions separately from every package.**
`PROTOCOL_VERSION` (`rusty-core/src/remote.rs`) is a single `u32`
constant, currently **`1`**. It governs the remote-execution protocol
between a `RemoteNode` (embedded in any process using
`rusty-agent-runtime`, including `rusty-agent-server`) and a `rusty-worker`
(`POST /execute`, `NodeTask` / `TaskResult`). Evolution within protocol v1
is additive-only: workers must reject tasks whose `protocol_version` they
do not support, and responses are accepted regardless of their version
field so newer workers can serve older clients. A non-additive change
requires bumping `PROTOCOL_VERSION` to 2.

**Server↔SDK compatibility is versioned by `API_PROTOCOL_VERSION`.**
`rusty-agent-server` carries `pub const API_PROTOCOL_VERSION: u32 = 1`
(`rusty-server/src/lib.rs`) and reports it from `GET /info` as
`api_protocol_version`, alongside the crate `version`. The Python and
TypeScript SDKs speak to that HTTP/SSE API (an Agent-Protocol subset; see
`rusty-server/src/routes.rs`). Within API v1, evolution is additive-only:
new routes, new optional request fields, new response fields, and new SSE
event families may appear in minor releases; a removal, rename, or meaning
change requires bumping to 2 — a new major line of the server. Clients
must ignore unknown fields and unknown SSE events (the standing contract
in [stability.md](stability.md)). The compatibility rule is:

- An SDK is compatible with any server whose `/info` reports an
  `api_protocol_version` it supports (currently 1); the SDK refuses or
  warns on anything else.
- SDK-side enforcement lands when the SDKs adopt the handshake — neither
  SDK gates on `api_protocol_version` yet. Until they do, the same-cycle
  pairing (SDK 0.5.x ↔ server 0.12.x) remains the tested configuration,
  and cross-cycle use remains unvalidated rather than refused. Rusty
  Studio carries client-side fallback notes for older servers where a
  feature needs a newer endpoint.

## Current versions

As of 2026-08-11 (v0.13.0 / R0.12 — Operations Plane):

| Package | Registry | Source | Version |
|---|---|---|---|
| `rusty-agent-runtime` | crates.io | `rusty-core/` | 0.12.0 |
| `rusty-agent-server` | crates.io | `rusty-server/` | 0.12.0 |
| `rusty-worker` | crates.io | `rusty-worker/` | 0.3.6 |
| `rusty-otel` | crates.io | `rusty-otel/` | 0.1.7 |
| `rusty-eval` | crates.io | `rusty-eval/` | 0.1.5 |
| `@rusty-runtime/client` | npm | `sdks/typescript/` | 0.5.0 |
| `rusty-agent-runtime` (import: `rusty_client`) | PyPI | `sdks/python/` | 0.5.0 |

Note the name collision by design: the Rust core crate and the Python SDK
are both published as `rusty-agent-runtime` (crates.io and PyPI
respectively). They are different packages with independent version
numbers; the Python SDK is imported as `rusty_client`.

All seven packages publish through the Release workflow — see
[releasing.md](releasing.md) for the registry mechanics.

## Compatibility matrix

Which package pairs must agree, and on what:

| Producer | Consumer | Governing version | Rule |
|---|---|---|---|
| `rusty-agent-runtime` (RemoteNode) | `rusty-worker` | `PROTOCOL_VERSION` = 1 | Worker rejects tasks with an unsupported `protocol_version`. Both sides currently speak v1. Keep worker and runtime on compatible protocol majors. |
| `rusty-agent-server` | `@rusty-runtime/client`, `rusty_client` (PyPI) | `API_PROTOCOL_VERSION` = 1 | Additive-only within API v1: an SDK is compatible with any server whose `/info` reports an `api_protocol_version` it supports, and refuses or warns on anything else. The SDKs do not enforce the handshake yet — SDK 0.5.x ↔ server 0.12.x remains the tested pairing. |
| `rusty-agent-runtime` | `rusty-agent-server`, `rusty-worker`, `rusty-otel` | crate versions | All three are path-dependents built in lockstep from this monorepo; a published crate pair must satisfy SemVer on the Rust API, which is unstable at 0.x (see [stability.md](stability.md)). |
| `rusty-agent-server` | Rusty Studio (`studio/ui`) | same-cycle pairing | Studio's typed wire schemas and committed production bundle are distributed in-repo; CI validates them against the same-cycle server contract. The legacy console remains a temporary advanced compatibility surface. |
| checkpoint writers (`JsonFileCheckpointer`, `PostgresCheckpointer`) | checkpoint readers | `rusty-agent-runtime` minor version | Checkpoints written by one `0.x.*` line are guaranteed readable by that same minor line only. See [stability.md](stability.md). |

## MSRV

The minimum supported Rust version for all four crates is **1.86**,
declared once in `[workspace.package]` (`rust-version = "1.86"`) and
inherited by every crate via `rust-version.workspace = true`.

MSRV is enforced in CI: the `msrv / ${{ matrix.crate }}` job in
`.github/workflows/ci.yml` reads the `rust-version` field from each
crate's manifest at runtime and checks that crate with exactly that
toolchain. A manifest missing the field fails the job.

Pre-1.0, an MSRV bump may land in any minor release and is recorded in
the CHANGELOG. At R1.0 the policy tightens — see
[stability.md](stability.md#what-changes-at-r10).

## Bumping a version

1. Edit the version in the owning manifest only (per-crate `Cargo.toml`,
   `sdks/python/pyproject.toml`, `sdks/typescript/package.json`).
2. Add a CHANGELOG section for the release, listing every package whose
   version moved and every breaking change per package.
3. If the change touches `NodeTask` / `TaskResult` on the
   remote-execution path, decide explicitly whether it is additive
   (protocol stays 1) or not (protocol bumps to 2, workers must be
   updated first).
