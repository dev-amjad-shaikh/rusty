# Security Policy

## Supported versions

Rusty is pre-1.0. Only the latest minor release line receives security fixes; older minors are not patched. Crates and SDKs are versioned independently, so check the component you actually deploy:

| Component | Package | Supported line |
|---|---|---|
| `rusty-core/` | `rusty-agent-runtime` (crates.io) | latest `0.x` minor only |
| `rusty-server/` | `rusty-agent-server` | latest `0.x` minor only |
| `rusty-worker/` | `rusty-worker` | latest `0.x` minor only |
| `rusty-otel/` | `rusty-otel` | latest `0.x` minor only |
| `sdks/python/` | `rusty-agent-runtime` (PyPI) | latest `0.x` minor only |
| `sdks/typescript/` | `@rusty-runtime/client` (npm) | latest `0.x` minor only |

During the `0.x` series, breaking changes may land in any minor release; security fixes are applied on top of the current minor rather than backported.

## Reporting a vulnerability

Report vulnerabilities through **GitHub private vulnerability reporting** on this repository: open the *Security* tab and choose *Report a vulnerability*. This keeps the report private between you and the maintainers until a fix is published.

Do **not** open a public issue, discussion, or pull request for an unpatched vulnerability.

Please include:

- the component and version affected,
- a description of the impact and the attack scenario,
- reproduction steps or a proof of concept,
- any mitigations you are aware of.

## Response expectations

- **Acknowledgement:** within 72 hours of the report.
- **Initial assessment:** within 14 days — whether the report is accepted as a vulnerability, its severity, and the planned fix approach.
- **Fix and disclosure:** accepted vulnerabilities are fixed on the current minor line. Once a fixed release is available, the advisory is published through GitHub Security Advisories and credited to the reporter unless you ask otherwise.

If a report is declined, you will receive an explanation. This is a small volunteer-run project; the timelines above are targets, not guarantees.

## Scope notes

The following are known limitations, not vulnerabilities — reports about these will be closed as known issues:

- **Multi-tenant auth is implemented but not yet audited.** `rusty-agent-server` v0.4 shipped key-to-tenant mapping with per-tenant namespacing of threads, runs, assistants, crons, and KV (cross-tenant access answers 404). It has not received an independent security audit; treat multi-tenant deployments as hardening-in-progress.
- **Dev mode has no auth.** With no API key configured (the default), `rusty-agent-server` accepts all requests. This is intentional for local development and is not a vulnerability.
- **Permissive CORS by default.** `router()` layers `CorsLayer::permissive()` so browser clients work out of the box. Restrict origins in your own binary or at a reverse proxy for production.
- **Bind address.** The default bind address is `0.0.0.0:8080`, which exposes the server on all interfaces. For local work, bind to `127.0.0.1`; for anything beyond that, place the server behind a reverse proxy with TLS and configure an API key.
- **Do not run as root.** Run the server and workers as an unprivileged OS user. The runtime executes user-defined graphs, tools, and WASM nodes; OS-level isolation (least-privilege user, filesystem restrictions, network policy) is the deployer's responsibility.
- **Checkpoint and store contents are plaintext.** Threads, checkpoints, and KV entries are written to the store path (or Postgres) unencrypted. Protect the store with filesystem permissions or database access controls.

Vulnerabilities in third-party dependencies should be reported when the dependency is pinned or vendored here in a way that creates exposure; otherwise report them upstream.
