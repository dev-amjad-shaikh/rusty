# Versioning and Compatibility

How Rusty packages are versioned, which versions are current, and which
version numbers govern which compatibility boundary. For what each package
promises not to break, see [stability.md](stability.md).

## Policy

**Packages version independently.** There is no single "Rusty version".
Each crate bumps its own version in its own `Cargo.toml`; each SDK bumps
its own package manifest. The root [CHANGELOG.md](../CHANGELOG.md) groups
implemented work into named releases (R0.1 — Ignition through R0.10 —
Adaptation, with the v0.5 SDK/tenancy cycle in between) purely as a
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

**Server↔SDK compatibility is not yet versioned by a constant.** The
Python and TypeScript SDKs speak to `rusty-agent-server`'s HTTP/SSE API (an
Agent-Protocol subset; see `rusty-server/src/routes.rs`). That surface has
no numeric protocol version today — there is no `protocol_version` field
on HTTP requests and the `/info` endpoint does not report a server
version. Until one exists, the compatibility rule is:

- An SDK at version `0.x.y` is tested against the `rusty-agent-server` release
  from the same cycle (see the matrix below). That pairing is the
  supported configuration.
- Newer SDK minor versions against older servers (and vice versa) may
  work where the API overlap is additive, but are not validated. Rusty
  Studio carries client-side fallback notes for older servers where a
  feature needs a newer endpoint.

## Current versions

As of 2026-08-09 (v0.11.0 / R0.10 — Adaptation):

| Package | Registry | Source | Version |
|---|---|---|---|
| `rusty-agent-runtime` | crates.io | `rusty-core/` | 0.10.0 |
| `rusty-agent-server` | crates.io | `rusty-server/` | 0.10.0 |
| `rusty-worker` | crates.io | `rusty-worker/` | 0.3.4 |
| `rusty-otel` | crates.io | `rusty-otel/` | 0.1.5 |
| `rusty-eval` | crates.io | `rusty-eval/` | 0.1.3 |
| `@rusty-runtime/client` | npm | `sdks/typescript/` | 0.3.0 |
| `rusty-agent-runtime` (import: `rusty_client`) | PyPI | `sdks/python/` | 0.3.0 |

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
| `rusty-agent-server` | `@rusty-runtime/client`, `rusty_client` (PyPI) | same-cycle pairing (no constant yet) | SDK 0.3.x ↔ server 0.10.x is the tested pairing. Cross-cycle use is unvalidated. |
| `rusty-agent-runtime` | `rusty-agent-server`, `rusty-worker`, `rusty-otel` | crate versions | All three are path-dependents built in lockstep from this monorepo; a published crate pair must satisfy SemVer on the Rust API, which is unstable at 0.x (see [stability.md](stability.md)). |
| `rusty-agent-server` | Rusty Studio (`studio/`) | same-cycle pairing | Studio is a zero-build UI distributed in-repo; it calls the same-cycle server API and notes fallbacks for older servers in-line. |
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
