# Releasing Rusty

How the seven packages reach their registries, what the Release workflow guarantees, and the operational details that only bite once. For version numbering and the compatibility matrix see [versioning.md](versioning.md); for what each release contains see [roadmap.md](roadmap.md) and [../CHANGELOG.md](../CHANGELOG.md).

## What publishes where

| Package | Registry | Manifest |
|---|---|---|
| `rusty-agent-runtime` | crates.io | `rusty-core/Cargo.toml` |
| `rusty-eval` | crates.io | `rusty-eval/Cargo.toml` |
| `rusty-agent-server` | crates.io | `rusty-server/Cargo.toml` (directory keeps the historical `rusty-server/` name; the lib is `rusty_agent_server`; the wire identity `"service": "rusty-server"` is deliberately unchanged) |
| `rusty-worker` | crates.io | `rusty-worker/Cargo.toml` |
| `rusty-otel` | crates.io | `rusty-otel/Cargo.toml` |
| `rusty-agent-runtime` | PyPI | `sdks/python/pyproject.toml` (hatchling) |
| `@rusty-runtime/client` | npm | `sdks/typescript/package.json` |

First publish completed 2026-08-10 (core/server 0.9.0, worker 0.3.3, otel 0.1.4, eval 0.1.2, Python SDK 0.2.0, TS SDK 0.2.0).

## The Release workflow

`.github/workflows/release.yml` has two modes:

- **Tag push `v*`** — a real release. Every stage preflights, then publishes.
- **`workflow_dispatch` with `dry_run=true`** (the default) — preflight only: `cargo publish --dry-run`, `python -m build`, `npm pack`. Nothing is uploaded. A real release can also be dispatched manually with `dry_run=false`:

```
gh workflow run Release --repo dev-amjad-shaikh/rusty -f dry_run=false
```

Guarantees built into the workflow:

- **Fails closed on missing secrets.** The `config` job probes which registry tokens exist and aborts a real release if any is absent; the `release-summary` job treats a skipped stage as a failure, so a release can never report green with stages silently skipped (the v0.7.0 false-green that motivated this design).
- **Idempotent.** Each publish step checks the registry for the exact version first and skips if present. Re-running a half-completed release is safe and converges.
- **Dependency order with retries.** crates.io publishes `rusty-agent-runtime` first, then `rusty-eval`, then `rusty-agent-server`, `rusty-worker`, `rusty-otel`, retrying up to 5 times with 30 s backoff to absorb index propagation lag while dependents resolve the freshly published versions.

## Secrets

Set once in the GitHub repo (**Settings → Secrets and variables → Actions**):

| Secret | Where it comes from |
|---|---|
| `CARGO_REGISTRY_TOKEN` | crates.io → Account Settings → API Tokens (scope: publish for the five crates) |
| `PYPI_API_TOKEN` | PyPI → Account Settings → API tokens |
| `NPM_TOKEN` | npm → Access Tokens → **granular** token with publish on `@rusty-runtime/client` and "bypass 2FA for automation" enabled (a classic automation token also works) |

Two first-publish quirks, both resolved, worth remembering:

- **crates.io requires a verified account email** before the first `cargo publish` succeeds — the API error does not say this clearly.
- **npm rejected the first granular token**; the working configuration is a granular token scoped to the package with 2FA bypass for automation.

## Cutting a release

1. Bump versions in lockstep per [versioning.md](versioning.md): the five crate manifests plus internal `path`/`version` dependency pins, `sdks/python/pyproject.toml`, `sdks/typescript/package.json`.
2. Add the CHANGELOG entry and update [roadmap.md](roadmap.md) status.
3. Commit (`Release vX.Y.Z — …`), `git tag -a vX.Y.Z`, push main and tags.
4. The tag push runs the Release workflow in real mode — registry publication is part of the release, not a separate step.
5. Create the GitHub release with the CHANGELOG section as notes:

```
gh release create vX.Y.Z --repo dev-amjad-shaikh/rusty --title "vX.Y.Z — …" --notes-file notes.md
```

6. Verify: `gh run list --repo dev-amjad-shaikh/rusty --limit 3` for the workflow, then spot-check the registry pages or APIs.

## Hardening backlog

- **Narrow the PyPI token to project scope** — the first token was account-wide because the project did not exist yet; it does now.
- **Migrate to OIDC Trusted Publishing** — crates.io, PyPI, and npm all support binding the GitHub Actions workflow to the package so no long-lived tokens are needed. Trusted publishing can only be configured once the package exists, which is why the first release used classic tokens. The workflow can be converted registry-by-registry without changing the release process above.
