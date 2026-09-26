# Contributing to the Rusty platform

Thanks for helping build the Rusty platform. This repository is a monorepo of independently versioned crates plus client SDKs; each component has its own build and test loop.

## Components

| Component | What it is | Where to start |
|---|---|---|
| [`rusty-core/`](rusty-core/) | Rusty Core — the execution engine (crate `rusty-agent-runtime`) | [rusty-core/CONTRIBUTING.md](rusty-core/CONTRIBUTING.md): module map, ownership rules, PR checklist |
| [`rusty-server/`](rusty-server/) | Rusty Server — axum HTTP/SSE server crate and the `rusty` binary | [README](rusty-server/README.md); same PR rules as core |
| [`rusty-worker/`](rusty-worker/) | Rusty Worker — worker SDK for remote nodes | [README](rusty-worker/README.md); same PR rules as core |
| [`rusty-otel/`](rusty-otel/) | OpenTelemetry export | [README](rusty-otel/README.md); same PR rules as core |
| [`sdks/python/`](sdks/python/), [`sdks/typescript/`](sdks/typescript/) | Rusty SDK — zero-dependency client SDKs (`rusty-agent-runtime` on PyPI, `@rusty-runtime/client` on npm) | each SDK's README describes its e2e suite |
| [`studio/`](studio/) | Rusty Studio — zero-build debug UI | plain HTML/JS, no build step; see [docs/studio.md](docs/studio.md) |

## Checks

The four crates share a virtual workspace (root `Cargo.toml`), so builds resolve from the repository root; per-crate `--manifest-path` commands work as before. In the crate you touched, run the standard trio on stable Rust (MSRV 1.86):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The SDK e2e suites boot the real `server_demo` binary as a subprocess:

```bash
cargo build --manifest-path rusty-server/Cargo.toml --example server_demo
python3 -m unittest discover -s sdks/python/tests
node --test sdks/typescript/test/
```

## Ground rules

- **One concern per PR.** Changes to super-step, reducer, or interrupt/resume semantics need an issue and design discussion first; these are the project's core promises.
- **Tests ship with behavior changes.** Unit tests live in-module under `#[cfg(test)]`; server and SDK changes extend the integration/e2e suites.
- **Docs must match the real API.** Code samples in prose are reviewed against source; if you change a public signature, grep the READMEs and `docs/` for it.
- **Crates are versioned independently.** Bump the version in the crate you changed and record the change in the root [CHANGELOG.md](CHANGELOG.md). The platform-release numbers in the root README map to per-crate versions via [docs/roadmap.md](docs/roadmap.md).
- **`Cargo.lock` is not committed.** All four crates are published libraries, so the lockfile stays out of version control (it is git-ignored) and CI always builds against the latest semver-compatible dependencies. Pin exact versions in your own applications that depend on these crates, not here.

## Code of conduct

This project follows the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct). Be kind, be constructive, assume good faith.

## License

By contributing, you agree that your contributions are dual-licensed under MIT OR Apache-2.0, matching the project.
