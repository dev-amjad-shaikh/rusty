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

## Addendum: the two pillars in depth

### Traceability — the honest scorecard

Rusty's journal is closer to dsh's session log than the first pass suggested: `RunEventKind::ModelCall` already records the request (messages + tool schemas), so the "model-visible means logged" gap is not the log's *existence* but its *completeness proof*. Corrected comparison:

**Where rusty is already stronger**
- A *closed* event vocabulary matched exhaustively — dsh's merge-extensible event map needs `ignorable` markers and refusal-to-reconstruct rules to stay safe; rusty's compiler enforces totality.
- Effects are classified and journaled per call (read-only/idempotent/compensatable/irreversible), with deterministic idempotency keys and effect receipts — dsh has no effect taxonomy; its approvals sit beside the log, not inside a taxonomy.
- Replay is *verified* (server-side exact replay answers `verified: true` or names the divergence), plus run diff, fork, and time-travel — dsh's story is reconstructability, not verification.
- Evals are first-class (datasets sourced from run evidence); dsh has none.

**Where dsh is genuinely stronger**
- **Streaming fidelity**: raw `assistant/chunk` events give token-level replay; rusty journals request/response, not the stream between.
- **Derived surface**: `surfaceOp` replace-spans with `sourceEventSeqs` citations let compaction rewrite the conversation surface while the log stays immutable; rusty has no compaction projection yet.
- **Operational traceability**: telemetry as a mirrored ledger with a redaction *waterfall*, and a token meter that replays the log into request-pressure and per-surface pricing; rusty has journal evidence but no metering/analytics derived from it.
- **Cross-session introspection as agent tools** (`session_search`, `session_trace`) — the agent can query its own history's evidence; rusty's journals are operator-visible, not agent-visible.
- **Format-versioned headers** with explicit upgrade refusal — rusty's store evolution is quieter than that.

Net: rusty's evidence is stronger for *verification*, dsh's for *operations*. The P0 item shrinks accordingly: journal the run-config half of the envelope (model parameters, resolved capability set) alongside the already-journaled messages+schemas, add optional chunk capture, and make replay *assert* envelope equality.

### Flexibility — what "everything is a plugin" actually buys, and rusty's idiomatic answer

dsh's total-plugin model buys five concrete things: any row of the composition tree is replaceable by config (no privileged core); hot unload/reload with guaranteed unwind; deployments as data (profile/bundle/patch trees, so per-tenant assemblies need no code branch); per-agent scoped contributions in one process; and a third-party distribution channel.

The costs are real too: dependency injection at runtime gives up compile-time contract checking precisely where a harness most needs it (tool admission, secrets, effects), and their own README warns of compatibility-breaking churn — the flexibility tax is paid in auditability.

Rusty's answer should not imitate the ideology; it should deliver the same five wins through three mechanisms it already half-has:

1. **Trait seams for provider swaps** (already done — `ChatModel`, `HttpApiTransport`, `BrowserDriver`, `CredentialBroker`, checkpoint stores). This covers "replace any provider," at compile time, with exhaustiveness checking dsh cannot have.
2. **Data-plane extension** (already done — connector manifests, SKILL.md packages, composed tool recipes: content-addressed, validated, scanned). This covers "deployments as data" and "third-party distribution" without running third-party *code*.
3. **The missing piece — runtime code plugins**: a `PluginKernel` whose registrations return RAII guards (dropped LIFO on unload — ownership giving us for free what Cordis needs a runtime for), with **WASM capsules as the guest vehicle** (`capsule_host` + journaled `WasmCall` already exist). Sandboxed, capability-granted, journaled guest modules are the Rust-native "everything can be a plugin": hot-loadable, memory-safe, effect-admitted, evidence-recorded — without dissolving the typed planes into runtime DI.

That combination delivers dsh's flexibility profile where it matters (per-tenant assemblies, hot reload, third-party extension, composer-published capabilities) while keeping every extension point auditable at the boundary where it enters the system.
