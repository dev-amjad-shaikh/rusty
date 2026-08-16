# DeepSeek Harness → Rusty: concept review and adoption map

Date: 2026-08-16. Source: `github.com/deepseek-ai/deepseek-harness` (developer preview, v0.1.0-rc.6), its `docs/` subsystem references, and the Cordis preprint (*A Programming Paradigm for Spatiotemporal Composability*). Reviewed against rusty main at `8e3a55b`.

## What dsh is

An agent harness where **everything is a plugin**, composed by the Cordis kernel ("spatiotemporal composability": *temporal* = every registration is revertible via tracked disposers unwound LIFO on unload; *spatial* = plugins declare service dependencies and (de)activate reactively as providers appear and disappear). Frontends (CLI, Web, headless, ACP bridge, Python SDK) are swappable profile layers over one plugin tree; there is no privileged core.

Its strongest engineering ideas, in descending order of transfer value to rusty.

---

## Adoption map

### P0 — adopt now; small, high-fit, closes a real gap

**1. "Model-visible means logged" — the request-envelope invariant.**
dsh logs the full model request envelope (config + rendered system prompt + assembled tool schemas) as a session event, so every model request is a pure function of the log, and the runtime *asserts* this. Rusty's Flight Recorder journals tool calls and model exchanges, but the exact request envelope (which tool schemas were visible after allowlist filtering, what the system prompt contained) is reconstructable only indirectly. Adopting the invariant — journal the envelope per model call, and let replay *verify* envelope equality — closes the last hole in rusty's replay-fidelity story. This is the single most rusty-aligned idea in dsh: we already sell "evidence you can replay"; this makes the claim airtight.
*Rust shape:* a `request_envelope` journal event variant emitted by the ReAct/model node before dispatch; replay compares envelopes, not just outcomes. ~1 file in `rusty-core` + replay check + tests.

**2. Monotonic deny-only guards at tool dispatch.**
dsh runs policy *guards* after its allow/deny/ask waterfall that can return a denial but have no "allow" result — so listener ordering can never undo a denial. Rusty has admission (capability sets, per-run allowlists) and approval tokens, but a guard layer that is structurally incapable of widening access is a stronger composition rule for planes that want to *restrict* (tenant policy, run-scoped policy, capsule policy).
*Rust shape:* `trait DenyGuard { fn check(&self, tool: &str, args: &Value, effect: Effect) -> Option<Denial> }` evaluated in the executor's dispatch path after allowlist admission; denial is journaled. Composes with, never replaces, admission.

**3. Fail-closed approval vocabulary with durable asked/decided pairs.**
dsh's approval outcomes are a closed enum where only `allowed-once` grants (`rejected`/`cancelled`/`unavailable` all deny), and `approval/asked` + `approval/decided` event pairs are journaled *inside* the open turn so replay reconstructs the override. Rusty has approval tokens (composer publish, irreversible gates); adopting the closed vocabulary and the journaled pair normalizes every approval-gated surface (composer, run_cli irreversible, computer input) to one auditable shape.

### P1 — adopt next; medium effort, strategic

**4. A plugin kernel with RAII-revertible registrations ("fibers").**
dsh's temporal axis is the idea Rust expresses best: a registration that returns a guard, unwound in reverse order on unload, is just RAII. Rusty has no plugin system; today the composer publishes skills and tool *definitions*, but loading live capability bundles means ad-hoc registry mutation with no unwind. A `PluginKernel` giving each plugin a scope that collects `RegistrationGuard`s (dropped LIFO on unload) would make hot load/unload — including composer-published tools — a system invariant instead of author discipline.
*Rust shape:* `trait Plugin { fn apply(&self, ctx: &mut PluginContext) -> Result<()> }` where `PluginContext` hands out guards for every registry insertion; the fiber owns them; `Drop` unwinds. Idiomatic — ownership does what dsh needs a runtime for.

**5. Per-call sandbox seam with honest enforcement facts.**
dsh confines argv per invocation (Seatbelt/bwrap/ACL backends), reports enforcement as `full`/`partial`, and ships "denial dialects" per backend so consumers can distinguish "the sandbox denied it" from "the sandbox itself failed." Rusty's `run_cli` has an allowlist + jail but no OS-level confinement; adding a `SandboxBackend` seam (macOS `sandbox-exec` first, Linux bwrap later) with an enforcement report on every execution receipt would harden the exact capability the owner asked for and matches rusty's evidence-first posture — partial enforcement is reported, never papered over.

**6. Provider-neutral tool render intents for Studio.**
dsh computes a closed union of presentation intents (terminal/diff/search/read cards) from pure functions of tool args/results, so any frontend renders rich, replay-identical evidence cards. Studio currently renders journal evidence generically; a `render_intent` derivation (pure Rust fn → serde enum, consumed by Studio) would make Trace/Evaluate screens show per-tool evidence cards with zero per-tool UI code, and — because it derives from the journal — replay renders identically.

### P2 — keep on the radar; larger or situational

**7. Surface-op compaction over the immutable journal.** dsh rewrites the conversation *surface* (append/replace spans with source citations) while the log stays immutable — compaction that can never corrupt evidence. Adopt when context-window pressure becomes real for long rusty runs; rusty's channel/reducer model maps cleanly (surface = derived projection, journal untouched).

**8. Claude-Code/Codex-compatible hooks.** dsh implements the `hooks.json` wire protocol so existing user hooks run unmodified. Rusty's middleware SDK covers the capability; wire compatibility is an adoption play, not a technical one.

**9. Reactive coeffects (`inject`-style reactivation).** Powerful in a dynamic plugin world; premature for rusty until the plugin kernel (P1-4) exists and proves a need. Rust's `tokio::watch` makes it cheap when the time comes.

### Already in rusty — affirmed parity

Turn/step loop with durable inbox ≈ rusty's executor + pending runs; capability-seam triad ≈ rusty's `HttpApiTransport` / `BrowserDriver` / `CredentialBroker` / checkpoint-store seams; per-agent scoped tool sets ≈ capability sets + per-run allowlists; skills ≈ rusty's skill plane (compatible frontmatter shape); MCP bridging ≈ `mcp.rs` + `mcp_bridge.rs`; subagents ≈ remote nodes / A2A / teams; typed event log with fork/resume ≈ Flight Recorder + checkpoints. Where dsh has eval-dataset weakness, rusty's evaluations plane is ahead.

### Deliberately not adopted

- **Everything-is-a-plugin totality**: rusty's planes are statically composed Rust modules with typed contracts; dissolving them into runtime DI would trade compile-time honesty for dynamism rusty doesn't need. Adopt the *kernel* (4), not the *ideology*.
- **Agent-authored dynamic plugins (`cordis_*`)**: rusty's composer keeps generated artifacts as *data* (skills, manifests) behind approval — the same self-extension value without running model-authored code in the host process.
- **dsh's web-server posture** (loopback, no auth): rusty already has API keys/tenancy.

## Suggested sequencing

1. Request-envelope journaling + replay verification (P0-1)
2. Deny-only guard layer (P0-2) + closed approval vocabulary (P0-3)
3. Plugin kernel with RAII guards (P1-4), then wire composer publishing through it
4. Sandbox seam for `run_cli` (P1-5)
5. Render intents for Studio (P1-6)
