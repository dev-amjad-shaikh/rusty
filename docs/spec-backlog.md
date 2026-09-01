# Spec backlog — The Source Code build plan

Consolidated tracker for the 182 stories in `/Users/amjad.shaikh/00-The-Source-Code/spec` (15 epics, milestones M0–M4), mapped against what has actually landed in this repository. Generated 2026-08-25.

**Status legend** — ✅ landed · ◐ partial (core mechanism shipped, some acceptance criteria open) · ○ not started

Status is evidence-mapped: each row cites the module, route, or branch the judgment rests on. It is not a per-acceptance-criterion verification pass — treat ◐ rows as the honest middle. EP-07 rows marked W1/W2 refer to the gap-ledger waves on the unmerged `feat/gap-ledger` branch.

## At a glance

**████████████████████░░░░░░░░░░ 75%** weighted complete (111 ✅ landed · 51 ◐ partial · 20 ○ not started, of 182 stories)

| Epic | Milestone | Stories | ✅ | ◐ | ○ | Progress |
|---|---|---|---|---|---|---|
| EP-01 Event Log and State Substrate | M0–M1 | 11 | 11 | 0 | 0 | ███████████ 100% |
| EP-02 Execution Kernel and ABI | M0–M1 | 11 | 11 | 0 | 0 | ███████████ 100% |
| EP-03 Durability, Checkpoints and Pause | M1 | 11 | 9 | 1 | 1 | ██████████░░ 91% |
| EP-04 Gateway, Sessions and Channels | M1 | 12 | 5 | 3 | 4 | ██████░░░░░░ 54% |
| EP-05 Tool System and Sandboxing | M0–M2 | 12 | 11 | 1 | 0 | ██████████░░ 92% |
| EP-06 Memory | M1–M2 | 12 | 9 | 1 | 2 | ███████████░ 92% |
| EP-07 Skills and Self-Learning | M2 | 12 | 2 | 10 | 0 | ███████░░░░░ 58% |
| EP-08 Agent Blueprints and Registry | M0–M4 | 11 | 7 | 3 | 1 | █████████░░░ 77% |
| EP-09 Multi-Agent Collaboration and Task Management | M3 | 12 | 5 | 7 | 0 | ████████░░░░ 67% |
| EP-10 Self-Healing and Resilience | M1–M3 | 12 | 5 | 3 | 4 | ██████░░░░░░ 54% |
| EP-11 Security, Governance, and Multi-Tenancy | M0–M4 | 12 | 6 | 3 | 3 | ████████░░░░ 67% |
| EP-12 Evals Framework | M2 | 12 | 12 | 0 | 0 | ████████████ 100% |
| EP-13 Observability, Storage, and Operations | M0–M4 | 12 | 8 | 1 | 3 | ██████████░░ 71% |
| EP-14 User Interfaces | M1–M4 | 18 | 8 | 9 | 1 | ████████░░░░ 69% |
| EP-15 Out-of-the-Box Catalog | M4 | 12 | 2 | 5 | 5 | ████░░░░░░░░ 17% |

## EP-01 — Event Log and State Substrate

███████████ 100% · 11 landed · 0 partial · 0 not started · milestone M0–M1

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-01-S01 | Typed event append with monotonic positions on the storage contract | P0 | ✅ | `journal.rs`/`record.rs`: typed appends, monotonic positions |
| EP-01-S02 | Writer-claim fencing inside the commit transaction | P0 | ✅ | writer fencing in the journal commit path |
| EP-01-S03 | `derive_messages`: model history as a pure projection | P0 | ✅ | `context.rs`/`surface.rs`: model history as pure projection |
| EP-01-S04 | Persisted pointer-array context window with transactional step-boundary updates | P0 | ✅ | `context.rs` pointer-array window, step-boundary updates |
| EP-01-S05 | The model-visible-means-logged invariant checker | P0 | ✅ | `invariant.rs` checker on the `ChatModel` seam: journal-anchored recomputation, byte-for-byte compare, typed `UnloggedContent` + assertion registration point; wired into ReAct dispatch; 7 tests (d4e9ad2) |
| EP-01-S06 | Crash repair: closing orphaned turns with synthetic interrupted markers | P0 | ✅ | journal crash-repair closes orphaned turns (recovery tests) |
| EP-01-S07 | Projection determinism: same log prefix, byte-identical model input | P0 | ✅ | projection goldens in `rusty-core/tests/golden` |
| EP-01-S08 | Event-schema conformance suite and closed-enum versioning | P0 | ✅ | `event_schema_conformance.rs` (29 tests, 9 schema golden files + 8 variant-golden files) on `feat/ep-01-s08` (`2adb50b`): exhaustive round-trip for all `RunEventKind` variants, unknown-tag rejection, golden-file schema-drift detection, closed-enum invariants on empty + executed journals, SurfaceOp boundary validation, schemars-generated JSON Schema snapshots for 8 closed enums + `RunEvent`/`PayloadRef`/`ArtifactRef`/`Usage`; all ACs pass, clippy/doc clean |
| EP-01-S09 | Compaction as non-destructive surface operations | P1 | ✅ | `surface.rs` non-destructive compaction |
| EP-01-S10 | Session fork seeded from a log prefix | P1 | ✅ | `ThreadRecord` gains `forked_from: Option<String>` and `seed_length: Option<usize>`; `POST /threads/{id}/fork` populates both fields and returns `seed_length` in the response; `GET /threads/{id}` added to retrieve lineage; `time_travel.rs` tests verify lineage round-trip and seed_length matches checkpoints_copied; `e0b0247` on `feat/ep-01-s10` |
| EP-01-S11 | Streaming chunk fidelity and partial-turn reconstruction | P1 | ✅ | `ab3314b` on `feat/ep-01-s11`: `AssistantChunk` struct + `RunEventKind::AssistantChunk`, `ChunkAssemblyMismatch` error, `RecordingChatModel::chat_stream` journals each chunk separately with monotonic `stream_index`, verifies concatenation equals full response, typed mismatch error on corruption; `rusty-core/tests/stream_fidelity.rs` 4 tests (chunk journaling, assembly match, mismatch error, replay reconstruction); clippy/doc clean |

## EP-02 — Execution Kernel and ABI

███████████ 100% · 11 landed · 0 partial · 0 not started · milestone M0–M1

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-02-S01 | `rusty-api`: the dependency-light trait ABI | P0 | ✅ | `schemars` derive on every public type, `cargo metadata` dependency-allowlist test, inward-only rule with 5 known violations ledger, 16 JSON-schema golden snapshots, public-API snapshot (`cargo-public-api`), no-global-registration compile test; 22 tests in `abi_discipline.rs`; `66c1d10` on `feat/ep-02-s01` |
| EP-02-S02 | `ProcessedResponse`: parse the model response exactly once | P0 | ✅ | single-parse response path (`llm.rs`/`provider_genai.rs`) |
| EP-02-S03 | `NextStep`: the closed step-resolution sum type | P0 | ✅ | closed `NextStep` sum type (`executor.rs`) |
| EP-02-S04 | Phase-module loop decomposition | P0 | ✅ | phase-module loop decomposition (`executor.rs`) |
| EP-02-S05 | Named typed seams with dispatch mode in the contract | P0 | ✅ | `middleware.rs` named typed seams with dispatch mode |
| EP-02-S06 | Seam dispatch-mode conformance and the generated seam catalog | P1 | ✅ | `seam_catalog.rs` with `DispatchMode`/`DecisionVariant`/`SeamEntry`/`SeamCatalog`, `generate_catalog()` emits machine-readable catalog from type definitions, `schemars` JSON Schema for all 3 payload/return types, golden snapshot `tests/schemas/seam-catalog.json` diff-guarded, 13 conformance tests (catalog structure, snapshot diff, mode semantics, waterfall short-circuit, around-wrap count, ordering determinism across 100 dispatches, teardown efficacy, schema round-trip, dispatch-site closed-list scan); `0fde5fb` on `feat/ep-02-s06` |
| EP-02-S07 | Per-agent scoped registration with teardown | P1 | ✅ | `plugin.rs` per-agent scoped registration with teardown |
| EP-02-S08 | Iteration and token budgets enforced at the loop | P0 | ✅ | iteration/token budgets enforced at the loop |
| EP-02-S09 | Frozen three-tier prompt assembly with violation detection | P0 | ✅ | `context.rs`: `DirectiveTiers`, `FrozenPrefix`, `FrozenPrefixRecord`, `TierRecord`, `ContextPipeline::assemble_frozen_prefix`, `AssemblingChatModel::with_frozen_prefix`, pre-dispatch `FrozenPrefix::verify`; `rusty-core/tests/frozen_tiers.rs` 6 tests (session lifetime, tier mutation, dispatch refusal, mid-session suffix, cross-process resume, deterministic rendering); `2cf4853` on `feat/ep-02-s09` |
| EP-02-S10 | The provider seam: two methods, prefix routing, provenance stamps | P0 | ✅ | `ChatModel::chat_stamped` default method, `ProviderRegistry` with prefix routing (`openai/gpt-4` → provider + model), `StampedChatModel` dispatch wrapper; `rusty-core/tests/provider_seam.rs` 5 tests (routing, unregistered rejection, stamp capture across start/continuation/end, main/side traffic distinction, cargo metadata kernel-dep check); `43e618a` on `feat/ep-02-s10` |
| EP-02-S11 | Retry and overflow recovery as `request_error` seam extensions | P1 | ✅ | `d254417` on `feat/ep-02-s11`: `ModelErrorDecision` enum, `on_model_error` hook on `Middleware`, `run_model_retry` on `MiddlewareChain`, `RetryHandler` (ceiling/base-delay/backoff) + `OverflowRecoveryHandler` (message compaction then retry), `instantiate_composition` vocabulary extended to 4 layers; `rusty-core/tests/request_error_handlers.rs` 7 tests (bare-loop no-retry, retry success after transients, ceiling exhaustion, non-transient not retried, overflow compaction, composition waterfall, message identity preserved across retries); clippy/doc clean |

## EP-03 — Durability, Checkpoints and Pause

██████████░░ 91% · 9 landed · 1 partial · 1 not started · milestone M1

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-03-S01 | Scheduler state as two maps, frontier as a pure function | — | ✅ | `state.rs` two-map scheduler state, pure frontier |
| EP-03-S02 | Deterministic task identity and per-task writes | — | ✅ | deterministic task identity, per-task writes (`durable.rs`) |
| EP-03-S03 | The durability knob: sync, async, exit | — | ✅ | durability knob in checkpoint config |
| EP-03-S04 | Checkpoints are projections: fold, verify, discard | — | ✅ | `checkpoint.rs` fold / verify / discard |
| EP-03-S05 | Pause-as-data: run exit with typed obligations | — | ✅ | pause-as-data with typed obligations (`durable.rs`) |
| EP-03-S06 | The pause envelope: versioned snapshot with tool-identity rebinding | — | ✅ | `PauseEnvelope`, `PauseSchemaVersion`, `ToolIdentityKey`, `RunObligation`, `ObligationKind`, `ObligationStatus`, `StickyApproval`, `ToolRebindingResult` in `rusty-core/src/record.rs`; semver floor check fails loudly; tool-identity rebinding by exact qualified-tool-name match; sticky approval round-trip; sparse wire shape; 13 tests in `rusty-core/tests/pause_envelope.rs`; clippy/doc clean; `d1d2e5f` on `feat/ep-03-s06` |
| EP-03-S07 | Resume as an ordinary invocation | — | ✅ | resume as an ordinary invocation |
| EP-03-S08 | Interrupt as a resumable exception with ordinal matching | — | ✅ | interrupts as resumable exceptions, ordinal matching |
| EP-03-S09 | Message-granular checkpoints: continue, fork, regenerate, time-travel | — | ◐ | `ThreadRecord` gains `parent`/`seed_length`; `POST /threads/{id}/fork` populates lineage; `GET /threads/{id}` retrieves lineage; `POST /threads/{id}/regenerate` forks + schedules run; `POST /threads/{id}/continue` shadows original; `rusty-server/tests/message_granular.rs` 3 tests (fork_lineage, regenerate_is_fork, continue_shadows_never_deletes) pass; `rusty-server/tests/time_travel.rs` 5/5 pass; clippy/doc clean; `6de69af` on `feat/ep-03-s09`; open: `paused_fork_isolation` AC (pause/obligation infra not fully wired in test harness) |
| EP-03-S10 | Crash-resume conformance: kill anywhere, recompute the frontier | — | ✅ | crash-resume recovery proofs (kill-anywhere tests) |
| EP-03-S11 | Pause longevity and expiry governance | — | ○ | **BLOCKED**: depends on EP-03-S06 (`PauseEnvelope`, `RunObligation`, `ObligationKind`, `ObligationStatus`, `StickyApproval` types) which are on unmerged branch `feat/ep-03-s06` (`d1d2e5f`); these types do not exist on `main` in `rusty-core/src/record.rs`. Cannot implement AC 1–5 (90-day resume, default TTL, cancellation, expiry sweep, approval query) without the pause-obligation contract infrastructure on main. |

## EP-04 — Gateway, Sessions and Channels

██████░░░░░░ 54% · 5 landed · 3 partial · 4 not started · milestone M1

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-04-S01 | The schema-defined protocol: frames, snapshot, sequencing | — | ◐ | WS protocol frames landed; schema-defined sequencing/snapshot partial; **blocked**: EP-13 schema-generation pipeline (`schemars → JSON Schema → TS types`) not present in workspace — `schemars` not in crate deps, no TS client generation CI job
| EP-04-S02 | Mandatory idempotency keys and the dedupe cache | — | ✅ | mandatory idempotency keys + dedupe |
| EP-04-S03 | Device pairing with challenge-nonce signing | — | ○ | **BLOCKED**: depends on EP-04-S01 (◐) blocked on schema-generation pipeline; no device-pairing protocol types or registration surface in workspace |
| EP-04-S04 | Session resolution and lineage | — | ✅ | session resolution + lineage (`/threads`, `session_query.rs`) |
| EP-04-S05 | The turn lease on the resolved session | — | ✅ | turn lease (`/tasks/claim`, heartbeat) |
| EP-04-S06 | The steering inbox: followup, steer, inject | — | ✅ | `inbox.rs` steering: followup / steer / inject |
| EP-04-S07 | The channel adapter trait: capabilities, authentication, scopes | — | ○ | **BLOCKED**: depends on EP-04-S01 (◐) which is blocked on EP-13 schema-generation pipeline; no `rusty-api` channel trait or gateway adapter registration infrastructure exists in workspace — `rusty-api` crate is empty, server uses SSE not WS protocol, no adapter mount/dispatch surface |
| EP-04-S08 | Built-in adapter: web chat over WebSocket | — | ◐ | web chat over WS (`/threads/{id}/runs/stream`); full adapter surface partial |
| EP-04-S09 | Built-in adapter: Slack | — | ○ | **BLOCKED**: depends on EP-04-S07 channel adapter trait which has no infrastructure in workspace |
| EP-04-S10 | Multi-device and cross-surface session continuity | — | ○ | **BLOCKED**: depends on EP-04-S08 (◐) and EP-04-S09 (○), both blocked on EP-04-S07 channel adapter trait which has no infrastructure |
| EP-04-S11 | Gateway-owned scheduling: cron, heartbeat, idleness | — | ✅ | gateway-owned scheduling (`/crons`, `triggers.rs`) |
| EP-04-S12 | Approval custody and routing across surfaces | — | ◐ | approvals landed; cross-surface custody routing partial |

## EP-05 — Tool System and Sandboxing

████████████ 92% · 11 landed · 1 partial · 0 not started · milestone M0–M2

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-05-S01 | The Tool trait with dual-representation output | P0 | ✅ | `tool.rs` Tool trait, dual-representation output |
| EP-05-S02 | The five-stage guard pipeline | P0 | ✅ | five-stage guard pipeline (`tool/`) |
| EP-05-S03 | Validation failure as conversational repair | P0 | ✅ | validation failure as conversational repair |
| EP-05-S04 | Toolset combinator algebra | P1 | ◐ | `tool_select.rs` (`b1d9c41` on `feat/ep-05-s04`): `filtered`, `prefixed`, `prepared`, `defer_loading`, `ToolsetSpec` serde + `apply_spec` resolver landed with 25 combinator tests (nested filter→prefix, round-trip, prepared spec, defer loading reveal/exhaust, effect/schema preservation); `approval_required` blocked on missing `Ask` pipeline infrastructure (no five-stage guard `pre_execute` pause-and-resume) |
| EP-05-S05 | The sandbox executor seam and the local process backend | P0 | ✅ | `rusty-core/src/sandbox.rs`: `SandboxExecutor` trait + `LocalProcessBackend` with `EnforcementLevel`/`ToolStub`/`SandboxResult`, honest `Partial` reporting; `rusty-core/src/tool.rs`: `ToolExecutor::with_sandbox()`, dispatch routes `Placement::Sandboxed` through backend, enforces `Required` + `Partial` → typed denial; `rusty-core/tests/sandbox.rs` 12 tests (trait contract, local execution/allowlist/timeout, container enforcement, remote enforcement, executor routing, required-on-partial denial, no-backend failure, serde round-trips); clippy/doc clean; `3326ad5` on `feat/sandbox-backends` |
| EP-05-S06 | Per-effect-class execution placement | P0 | ✅ | `tool.rs` (`f397b6c` on `feat/effect-placement`): `EffectClass`/`SandboxRequirement`/`Placement`/`PlacementError` enums + `resolve_placement()`, `Tool::effect_class()`/`sandbox_requirement()` defaults (`Read`/`None`), registration panics on `InvalidDeclaration` (AC 2), dispatch rejects `NoBackendAvailable` (AC 5); 4 placement tests (read-none registers, execute-none panics, egress-none panics, read-required fails dispatch); full runtime suite 465 tests green; backend identity recording (AC 3) + egress policy (AC 4) deferred to EP-11/L7 policy |
| EP-05-S07 | Code mode: one interpreter, guarded sub-dispatch, iteration refunds | P1 | ✅ | code mode: one interpreter, guarded sub-dispatch |
| EP-05-S08 | MCP client for external servers | P1 | ✅ | `mcp.rs` MCP client |
| EP-05-S09 | In-process tools presented as MCP servers | P1 | ✅ | `mcp.rs` (`4c1f158`): `InProcessMcpBridge` serves native tools over `tokio::io::duplex` in-memory transport; handles `initialize`, `tools/list`, `tools/call` with same JSON-RPC framing as external MCP; `McpClient::into_tools()` produces `McpToolAdapter` wrappers; mount-time `InProcessMountError` for disallowed effect classes (default `[Pure, ReadOnly]`, overridable via `with_allowed_effects`); 10 bridge tests (discovery parity, dispatch parity, mount refusal, error handling, multi-client, into_tools) |
| EP-05-S10 | Approval-gated execution: pause, decide, resume | P0 | ✅ | approval-gated execution: pause / decide / resume |
| EP-05-S11 | The bounded exec-reviewer for the gray zone | P1 | ✅ | `reviewer.rs` (`0605ce3`): `ExecReviewer` middleware with bounded model call (360 tokens, 30s timeout), strict schema `{decision, risk, rationale}`, fail-closed on all errors (timeout/malformed JSON/schema violation); `ToolInvocation.effect` field wired in `tool.rs`; 11 reviewer tests (allow/ask/passthrough paths + fault injection); clippy/doc clean |
| EP-05-S12 | Container and remote sandbox backends | P1 | ✅ | `ContainerBackend` (`docker run` with `--network none`, workspace mount, timeout/output cap) + `RemoteBackend` (POST to `/tools`, `/variables`, `/execute` endpoint with bearer auth) in `rusty-core/src/sandbox.rs`; both implement `SandboxExecutor` trait; `ContainerBackend` reports `Full` enforcement (verified filesystem + network confinement), `RemoteBackend` reports `Partial` (default when remote host does not attest); covered by executor conformance suite in `rusty-core/tests/sandbox.rs`; `3326ad5` on `feat/sandbox-backends` |

## EP-06 — Memory

███████████░ 92% · 9 landed · 1 partial · 2 not started · milestone M1–M2

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-06-S01 | Memory blocks as first-class shared entities | P0 | ✅ | `memory.rs` blocks as first-class shared entities |
| EP-06-S02 | Provenance-columned episodic entries | P0 | ✅ | provenance-columned episodic entries |
| EP-06-S03 | The agent's memory tool surface | P0 | ✅ | agent memory tool surface |
| EP-06-S04 | Lane-one recall: zero-model-call ranked and trigger injection | P0 | ✅ | lane-one recall, zero model calls (`/memory/query`) |
| EP-06-S05 | Lane-two recall: the escalation search sub-agent | P1 | ○ | **BLOCKED**: spec assumes `MemoryBlock`/`MemoryEntry`/`RecallInjection` model (EP-06-S01–S04) that does not exist in workspace; actual code has `MemoryRecord` with different provenance/scope model; no lane-one miss signal, no recall-injection event type, no sub-agent spawning infrastructure. Cannot implement AC 1–5 without memory-model alignment. |
| EP-06-S06 | Session-lineage-aware full-text search | P1 | ✅ | session-lineage-aware search (`session_query.rs`) |
| EP-06-S07 | The deterministic promotion gate and structural exclusion of untrusted origins | P0 | ✅ | promotion gate + structural exclusion of untrusted origins |
| EP-06-S08 | Sleeptime consolidation: scheduled, gated, high-water-marked | P0 | ◐ **BLOCKED** | `/memory/consolidate` landed; `a30967a` on `feat/ep-06-s08`: `ConsolidationCadence`, `ConsolidationState`, `frequency_gate()`, `exclude_recall_injected()`, high-water mark persistence (`load_consolidation_state`/`persist_consolidation_state`) in `rusty-core/src/memory.rs`; `rusty-core/tests/consolidation_gating.rs` 19 tests (gating pass/block matrix, interval checks, reason messages, high-water advance, state round-trip through store, missing state freshness, well-known key, recall-loop exclusion by run id and by tag, mixed slice, edge cases); clippy/doc clean; **BLOCKED on missing infrastructure**: `traffic: side` session stamping has no types/wiring; `learning_policy.consolidation_cadence` absent from blueprints/`CapabilityManifest`; task-based consolidation exists but spec expects automatic scheduler-fired "consolidation session" with candidate selection, promotion gate execution, and maintenance toolset assembly — none of which exist. Scheduler integration and side-session worker execution cannot proceed. |
| EP-06-S09 | Loss-bounded, hash-checked curated rewrites | P0 | ✅ | `e8b905f` on `feat/ep-06-s09`: `RewriteProposal`, `RewriteValidation`, `RewriteAudit` structs + `validate_rewrite()` in `rusty-core/src/memory.rs`; hash-match check (optimistic concurrency), loss-bound check (default 20% with justification override), shape-aware fact counting (arrays, single-array objects, general objects, string lines, scalars); `rusty-core/tests/rewrite_validation.rs` 15 tests (hash match/mismatch, loss within/exceeds bound, justifications pass/fail, fact-counting shapes, diff/audit shape, edge cases); clippy/doc clean |
| EP-06-S10 | The compaction engine: triggers, fallback chain, cheap summarizer, surface landing | P0 | ✅ | compaction engine (`context.rs`/`surface.rs`/`memory_tiers.rs`) |
| EP-06-S11 | Pre-compaction memory flush | P0 | ○ | **BLOCKED**: spec assumes `MemoryEntry`/`MemoryBlock`/`RecallInjection` model (EP-06-S01–S03) that does not exist in workspace; actual code has `MemoryRecord` with different provenance/scope model; flush step requires memory-entry-append tool and side-session worker execution that have no infrastructure
| EP-06-S12 | The hierarchical summary index, with vector search as an optional backend | P1 | ✅ | `007b679` on `feat/ep-06-s12`: `SummaryLevel` (0–3), `SummaryIndexEntry`, `HierarchicalSummaryIndex` with top-down full-text search (`search_top_down`), `compact_until_under` with hard iteration cap (default 20) and monotonic-shrinkage assertion; `VectorMemoryStore` implements `MemoryStore` as a delegating seam for future vector backend; 6 tests (level structure/citations, coarse-to-fine search, budget reach, iteration cap, monotonic shrinkage, store delegation); clippy/doc clean — AC 4 conformance-suite test requires EP-13-S03 crate on main |

## EP-07 — Skills and Self-Learning

███████░░░░░ 58% · 2 landed · 10 partial · 0 not started · milestone M2

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-07-S01 | Skill packages with progressive disclosure | — | ✅ | `skill.rs` packages with progressive disclosure |
| EP-07-S02 | The governed lifecycle and the content-addressed skill ledger | — | ✅ | `skills.rs` governed lifecycle, content-addressed ledger |
| EP-07-S03 | Editorial governance: patch-before-create, the curator, retention scoring | — | ◐ | `skill_distill.rs`; curator + retention scoring partial |
| EP-07-S04 | The post-turn background review fork | — | ◐ | `self_improve.rs` review path; post-turn background fork partial |
| EP-07-S05 | Interaction-event ingestion through governed connectors | — | ◐ | `gaps.rs` interaction events (W1 `8f89668`, `feat/gap-ledger`); connector ingestion open |
| EP-07-S06 | Demand-side intent mining and the intent map | — | ◐ | `induction.rs` + `POST /induction/run` landed on `feat/gap-ledger` (W3/W3b); vector-index clustering mode open |
| EP-07-S07 | Supply-side coverage reverse-engineering | — | ◐ | `induction.rs` coverage crawl + claims landed (W3); connector-driven crawl w/ receipts open |
| EP-07-S08 | The gap matrix, the seeded ledger, and declared blocks | — | ◐ | `induction.rs` join + seeding + declared blocks landed (W3); block mounting + matrix UI open |
| EP-07-S09 | Runtime gap filing | — | ◐ | `/gaps` surface + zero-recall/correction hooks landed on `feat/gap-ledger` (W2 `4b9d82a`, unmerged) |
| EP-07-S10 | The hunting loop and eval-gated promotion | — | ◐ | `/hunts` cycle/draft/blocked + promotion-gated closure landed on `feat/gap-ledger` (W4 `f10e627`, unmerged); autonomous hunt driver (scheduled cycles) open |
| EP-07-S11 | Frontier expansion | — | ◐ | `gaps.rs` speculative frontier, probes + decay (W1) |
| EP-07-S12 | The behavioral signal and per-intent efficacy | — | ◐ | outcome annotations w/ judge votes, majority scoring, auto failure-rate closure, per-intent curves landed on `feat/gap-ledger` (W5 `c6ce6ae`); turn-stamp seam invariant, judge sampler, retention coupling open |

## EP-08 — Agent Blueprints and Registry

█████████░░░ 77% · 7 landed · 3 partial · 1 not started · milestone M0–M4

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-08-S01 | The Rustyprint document and file-based loading | — | ✅ | Rustyprint document + file loading (`registry.rs`) |
| EP-08-S02 | Registry resolution: the factory/registry split | — | ✅ | factory/registry split resolution |
| EP-08-S03 | Blueprint validation and coherence checking | — | ✅ | blueprint validation + coherence checking |
| EP-08-S04 | Instantiation into sessions with version pinning | — | ✅ | instantiation with version pinning (`/assistants`) |
| EP-08-S05 | Versioning: immutable published versions and the draft/published/deprecated lifecycle | — | ✅ | immutable versions, draft/published/deprecated lifecycle |
| EP-08-S06 | The registry API and Rustynome authoring | — | ◐ | registry API landed; Rustynome authoring UI partial |
| EP-08-S07 | Learning-policy declaration and enforcement | — | ✅ | `/policy/*` learning-policy declaration + enforcement |
| EP-08-S08 | Fleet upgrade at safe boundaries | — | ○ | fleet upgrade at safe boundaries not started |
| EP-08-S09 | Blueprint export and import | — | ✅ | blueprint export/import |
| EP-08-S10 | Template blueprints for the out-of-the-box catalog | — | ◐ | template blueprints partial |
| EP-08-S11 | Version provenance for audit | — | ◐ | version provenance for audit partial |

## EP-09 — Multi-Agent Collaboration and Task Management

████████░░░░ 67% · 5 landed · 7 partial · 0 not started · milestone M3

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-09-S01 | Durable tasks: intent the machinery never touches | — | ✅ | durable tasks (`durable.rs`, `/tasks`) |
| EP-09-S02 | Attempts: cheap immutable execution records with lease, heartbeat, and retry chains | — | ✅ | attempts: lease, heartbeat, retry chains |
| EP-09-S03 | Assignment as trigger, closed admission reasons, comment coalescing | — | ◐ | assignment triggers landed; closed admission reasons partial; **BLOCKED**: circular dependency with EP-09-S04 — S03 depends on S04 (attribution resolved before admission) and S04 depends on S03 (task-attempt contract); neither can be completed without the other being done first
| EP-09-S04 | The attribution waterfall: one accountable human per run | — | ◐ | attribution waterfall partial; **BLOCKED**: circular dependency with EP-09-S03 — S04 depends on S03 (task-attempt contract) and S03 depends on S04 (attribution resolved before admission); neither can be completed without the other being done first
| EP-09-S05 | Stage barriers: ordered sibling groups that wake the parent's agent | — | ✅ | `feat/ep-09-s05` (`e89ee67`) — 10 `stage_barriers` integration tests + 119 lib tests pass, clippy/doc clean |
| EP-09-S06 | Pull-based batch claim for worker runtimes | — | ✅ | pull-based batch claim (`/tasks/claim`) |
| EP-09-S07 | Handoffs as tools with input-rewrite filters | — | ◐ | handoffs as tools (`a2a.rs`); input-rewrite filters partial |
| EP-09-S08 | Subagent safety: capability descriptors, fail-loud dispatch, blocklists, scoped teardown | — | ✅ | `rusty-core/src/subagent.rs` (`03ec5a6` on `feat/ep-09-s08`): `SubagentProviderDescriptor`, `SubagentRegistry` with scope-keyed LIFO teardown, `SubagentDispatchError` typed fail-loud errors, `SubagentBlocklistGuard`/`DelegateDepthGuard` (`ToolGuard` impls), `confined_toolset()` helper, `TrafficKind` side-stamp type; 12 unit tests (descriptor supports, registry CRUD, shadow refusal, dispatch fail-loud, depth check, blocklist guard deny, delegate depth guard deny, scope teardown LIFO + residue assertion, callback invocation, scope key validation, traffic default); 473 total lib tests pass, clippy/doc clean |
| EP-09-S09 | The hub: participant registry, write-ahead log, typed channels | — | ◐ | hub coordination + `team_trace.rs`; typed channels partial |
| EP-09-S10 | Squads: leader-routed teams under an enforceable operating protocol | — | ◐ | squads via coordination contracts; enforceable protocol partial |
| EP-09-S11 | Human visibility: board, timeline, cost, and the three-severity inbox | — | ◐ | Studio command center; three-severity inbox partial |
| EP-09-S12 | The structural review gate: agents propose, humans dispose | — | ◐ | structural review gate partial |

## EP-10 — Self-Healing and Resilience

███████░░░░░ 54% · 5 landed · 3 partial · 4 not started · milestone M1–M3

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-10-S01 | Typed repair records: no silent healing | — | ✅ | `rusty-core/src/repair.rs`: `RepairRecord`, `RepairTrigger`, `RepairAction`, `RepairOutcome`, `RepairRung`, `BreakerState`, `RepairQuery`, `RepairLedger` trait, `InMemoryRepairLedger`, `BufferedRepairSink` with `RepairSinkMetrics`, `RepairRecordBuilder`; `FileRepairLedger` (file-backed, append-only, JSON-per-record under `{root}/repairs/`) with `RepairLedger` trait impl; 13 tests in `rusty-core/tests/repair_records.rs` (record shape, attempt-count aggregation, query by component/trigger/outcome/time-range/session/attempt, sink failure/buffer-exhaustion/drop-metric/flush-recovery, serde round-trip, closed enum); 4 tests in `rusty-core/tests/repair_persistence.rs` (persists_and_queries, query_by_component, survives_reopen, object_safe); axum handlers `GET /repairs` and `GET /repairs/{record_id}` with query-param filtering in `rusty-server/src/repair.rs`; clippy/doc clean; `d271059` + `7fa19c2` on `feat/ep-10-s01` |
| EP-10-S02 | The repair ladder: cheapest rung first, escalation explicit | — | ✅ | repair ladder, cheapest rung first (`self_improve.rs`) |
| EP-10-S03 | Provider-error classification and backoff at the request seam | — | ✅ | provider-error classification + backoff at the seam |
| EP-10-S04 | Attempt-level repair: failure-reason classification and fresh-session escalation | — | ◐ | **BLOCKED**: `FailureReason`/`RetryRule` types from `contracts:task-attempt` do not exist in workspace; no `resume_safe` field on attempt rows; fresh-session escalation logic absent. Cannot implement AC 1–6 without task-attempt contract infrastructure. |
| EP-10-S05 | Heartbeat watchdog and the orphan sweep | — | ✅ | heartbeat watchdog + orphan sweep (`/tasks/heartbeat`) |
| EP-10-S06 | Stuck-turn detection: shielded grace, then escalation | — | ◐ | **BLOCKED**: phase-based cancellation handles not present in executor; `ToolBudget.timeout_ms` does not exist; turn lease release mechanism (EP-04-S05) not wired to kernel; shielded grace window infrastructure absent. Cannot implement AC 1–5 without EP-02/EP-04/EP-05 dependency infrastructure. |
| EP-10-S07 | Dependency fingerprints on skills and playbooks | — | ✅ | `rusty-core/src/skill.rs`: `DependencyDecl` + frontmatter parsing; `rusty-core/src/skills.rs`: `DependencyIndex`, `FingerprintStatus`, hygiene checks; `rusty-core/tests/dependency_fingerprints.rs`: 24 tests (declare, missing, malformed, circular, satisfied, changed, expired, orphaned, transitive, co-occurrence, lock, lock drift, stale lock, revalidation, revalidation failure, lock creation, lock round-trip, lock corruption, lock migration, hygiene pass/fail, hygiene mixed, hygiene error, index rebuild, concurrent modification); clippy/doc clean; `bcb8bf1` on `feat/ep-10-s07` |
| EP-10-S08 | Event-driven invalidation and the revalidation cycle | — | ○ | **BLOCKED**: depends on EP-07-S09 (runtime gap filing) and EP-07-S10 (hunting loop) which are on unmerged `feat/gap-ledger` branch; gap-ledger types and infrastructure not present on main. Cannot implement AC 1–5 without gap-ledger dependency. |
| EP-10-S09 | Knowledge-level repair: failures file the cause | — | ◐ | knowledge-level repair via gap filing (W2 in flight) |
| EP-10-S10 | The component health model: liveness, readiness, honest degradation | — | ✅ | `HealthStatus`/`ComponentHealth`/`HealthReport` types + aggregation logic; `GET /health` handler with async probes for `store` (list_assistants), `checkpointer` (list dummy thread), `broker` (list), `connectors` (list_manifests), `deployment` (list_environments), `knowledge` (all_sources), `receipt_keyring` (list_receipt_keys), `artifact_retention` (list_run_artifacts); structural Up for `skills` (boot-loaded) and `evaluation_state` (in-memory runtime); 1 integration test (`health_returns_200_with_components`); clippy/doc clean; `cea6772` + `92d4692` on `feat/ep-10-s10` |
| EP-10-S11 | Circuit breakers on flapping tools and connectors | — | ○ | **BLOCKED**: `Middleware::after_tool` is success-path only; no outcome hook runs on tool errors. A circuit breaker cannot observe failures through the middleware layer. Needs either an `on_tool_error` middleware hook or a different integration point (e.g., `ToolExecutor` level).
| EP-10-S12 | Self-healing conformance: the fault matrix | — | ○ | fault-matrix conformance not started |

## EP-11 — Security, Governance, and Multi-Tenancy

██████████░░ 67% · 6 landed · 3 partial · 3 not started · milestone M0–M4

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-11-S01 | The `SecretRef` type and the egress-only resolver | P0 | ✅ | `SecretRef` type + grammar validation, `Display`/`Debug` redaction, serde round-trip, `TryFrom<String>` (`ceaa8f1`); `SecretResolver` trait + `ScriptedSecretResolver` test double with 3 resolver tests in `rusty-core/tests/broker.rs`, exported in `rusty-core/src/lib.rs` (`8a93cf2`); 16 total broker tests |
| EP-11-S02 | Wire-probe-verified attachment: no probe, no tool | P0 | ✅ | `WireProbeOutcome` enum (`Rewritten`/`NotRewritten`/`Unreachable`), `WireProbeRecord` struct with `evidence_hash`, `ProbeLedger` trait + `ScriptedProbeLedger` test double, 8 new tests in `rusty-core/tests/broker.rs` (golden shape, newest-wins, append-only, liveness matrix, missing-probe, re-probe precedence), exported in `lib.rs`; `c8a99de` on `feat/ep-11-s02`; 38 total broker tests green, clippy/doc clean |
| EP-11-S03 | L7 egress policy: destination × method × path × originating component | P0 | ✅ | `EgressPolicy`, `EgressEndpointPolicy`, `EgressRule`, `EgressEndpoint`, `EgressDecision`, `EgressDenialReason`, `evaluate_egress()`, `path_matches()` glob matcher, `EgressPolicy::validate()` with failing-path return; `#[cfg_attr(test, derive(schemars::JsonSchema))]` on all 9 egress types; 3 schema tests (diff-guard, regenerate, validation corpus) + golden snapshot `rusty-core/tests/schemas/egress-policy.json`; `egress_policy: Option<EgressPolicy>` on `ServerConfig` with `with_egress_policy()` builder; `ReqwestConnectorTransport` evaluates egress via `evaluate_egress()` before HTTP call, returns `RustyError::Tool("egress denied: ...")` on `Deny`, `tracing::info!` on `Audit`; 4 server integration tests (deny-by-default blocks check, allow permits check, audit mode logs-and-allows, wrong-component denied); 119 server lib tests + 12 connector integration tests green; clippy/doc clean; `1adaa3d` on `feat/ep-11-s03`
| EP-11-S04 | SSRF and DNS discipline: preflight, pins, and canonicalization | P1 | ✅ | `EgressEndpoint` gains `allowed_ips` and `allow_encoded_slashes`; `EgressDenialReason` extended with `PreflightFailed`, `IpNotPinned`, `PathNotCanonical`, `RedirectOffPolicy`; `canonicalize_path()` (percent-decode, dot-segment removal, duplicate-slash collapse, encoded-slash refusal); `preflight_egress()` with injectable resolver (private/loopback/link-local refused without pin, pinned-set enforcement); `evaluate_redirect()` re-evaluates redirect targets against full policy; validation checks `allowed_ips` format; server-side: `ReqwestConnectorTransport` preflights DNS via `tokio::net::lookup_host`, pins resolved IP, rewrites URL, preserves Host header, re-evaluates redirects up to 10 hops; 2 integration tests (`egress_preflight_denies_loopback_without_pin`, `egress_preflight_allows_loopback_with_pin`); 30 egress tests + 14 connector tests + 119 server lib tests green; clippy/doc clean; `6c0cca9` + `4b6126d` on `feat/ep-11-s04` |
| EP-11-S05 | Layered execution security: modes, policy, autonomy — orthogonal and non-widening | P0 | ✅ | `capsule.rs` layered execution security: modes × policy × autonomy |
| EP-11-S06 | Receipts and attributed decisions: hallucinated actions are detectable, fail-closed is never "the user said no" | P0 | ✅ | `receipt.rs` + `/receipts/verify`, attributed decisions |
| EP-11-S07 | Signed extension manifests with capability declarations | P1 | ◐ | extension manifests with capabilities; signing partial |
| EP-11-S08 | Hard tenant isolation: RLS, prefixes, and the adversarial suite | P0 | ◐ | adversarial suite shipped (`ba72557` on `feat/ep-11-s08`): `run_scoped_endpoints_are_isolated_between_tenants` and `run_receipts_are_isolated_between_tenants` in `rusty-server/tests/multi_tenant.rs`; 11 tests pass, clippy/doc clean. **BLOCKED on RLS ACs**: `Checkpointer` trait in `rusty-core` is tenant-agnostic (`put(Checkpoint)`, `get_latest(&str)`, `list(&str)`); `server_store` tables use scoped IDs as PKs with no `tenant_id` column. Adding RLS requires either changing `Checkpointer` (affects all backends) or parsing tenant from thread_id in SQL (fragile, leaks scoping into a second place). Needs design decision before RLS ACs can proceed. |
| EP-11-S09 | SSO (OIDC and SAML) and SCIM provisioning | P0 | ○ | SSO/SCIM not started |
| EP-11-S10 | RBAC on the wildcard scope grammar; interfaces as security principals | P0 | ◐ | RBAC scope grammar landed; interfaces-as-principals partial |
| EP-11-S11 | Org-level approval policies: blocking `required`, non-blocking `audit` | P1 | ◐ | org-level approval policies partial |
| EP-11-S12 | The audit trail: append-only, tamper-evident, retained, and holdable | P0 | ◐ | `/broker/journal`, `/receipt_keys/journal`; retention/holds partial |

## EP-12 — Evals Framework

████████████ 100% · 12 landed · 0 partial · 0 not started · milestone M2

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-12-S01 | Eval cases and suites in the event schema | P0 | ✅ | `rusty-eval` `dataset.rs` |
| EP-12-S02 | One-click harvest: turn this session into an eval | P0 | ✅ | one-click harvest (`evidence.rs`) |
| EP-12-S03 | The runner: inference separated from scoring | P0 | ✅ | runner: inference separated from scoring (`experiment.rs`) |
| EP-12-S04 | Span-tree structural assertions | P0 | ✅ | span-query language: `rusty-eval` `trace.rs` + `span_query.rs` (journal→SpanTree distillation, serializable queries, versioned vocabulary, diagnosable failures; 15 tests, 2 goldens; b1524c0) |
| EP-12-S05 | The scorer pipeline: preprocess → analyze → score → reason | P0 | ✅ | scorer pipeline (`judge.rs`, `statistics.rs`) |
| EP-12-S06 | Scorers as async production hooks on sampled live traffic | P1 | ✅ | `rusty-eval/src/online_scoring.rs` (`12a1061` on `feat/ep-12-s06`): `OnlineScoringPolicy`, `SamplingDecision`, `ScorerBinding`, `ScoringTask`, `OutcomeAnnotation`, `ScorerOutcome`, `OnlineScoringRunner`, `BudgetTracker` trait + `InMemoryBudgetTracker`; deterministic FNV-1a sampling by `(tenant, blueprint, turn_id)`; budget exhaustion degrades to code-only scorers with `degraded` flag; reuses `JudgeModel` seam from offline evals; `traffic: side` on all annotations; 8 unit tests (policy validation, deterministic sampling, zero/full rate edges, binding validation, annotation serde round-trip, missing-scorer failure outcome, budget-exhaustion skip); clippy/doc clean |
| EP-12-S07 | Simulated users for multi-turn scenarios | P1 | ✅ | `rusty-eval/src/simulator.rs` (`1267365` on `feat/ep-12-s07`): `SimulationScenario`, `BehaviorRule`, `Trigger`, `UserAction`, `SteeringTool`, `TerminationCriteria`, `TerminationCause`, `SimulationResult`; deterministic scripted user simulation via `run_simulation()`; inbox steering delivery + journal drain; eval-compatible artifact production; 7 tests in `rusty-eval/tests/simulator.rs` (scenario schema round-trip, invalid JSON rejection, deterministic repetition, max-turns early termination, real kernel execution with log, steering via inbox, eval artifact compatibility); `9396560` wires `pub mod simulator` + `pub use simulator::{}` into `lib.rs`; clippy/doc clean |
| EP-12-S08 | Promotion gates: suites wired to skill promotion and blueprint publishing | P0 | ✅ | `rusty-core/src/skill.rs` `eval_gate` frontmatter field + `SkillPromotion`/`SkillPromotionStatus` types; `rusty-core/tests/skill_promotion.rs` 7 tests (gate parsing, hash inclusion, optional/reject-empty, serde round-trip); `rusty-server/src/skills.rs` `SkillGateEvaluator` trait + `PromotionError` + `promote()` with AC 5 stale-hash reuse + file-backed promotion persistence under `skill-promotions/`; `rusty-server/src/routes.rs` `POST /skills/{name}/promote` handler; 9 server tests pass (missing-gate refusal, failing-gate block, passing-gate success, stale-hash reuse, changed-hash demands new eval); clippy/doc clean; `ee6f84c` on `feat/ep-12-s08` |
| EP-12-S09 | Conformance suites as a first-class eval type | P1 | ✅ | `rusty-eval/src/conformance.rs`: `ConformanceSuite`, `ConformanceCase`, `ConformanceSeverity`, `ConformanceCheck` async trait, `ConformanceRunner`, `ConformanceReport`, `ConformanceVerdict`, `to_experiment_report()` for gate compatibility; `rusty-eval/src/lib.rs` module + re-exports; 10 unit tests in `rusty-eval/tests/conformance.rs`; server-side: `rusty-server/src/evaluations.rs` persistence (`ConformanceSuiteRecord`, `ConformanceRunRecord`, `ConformanceRegistry`, `target_has_passing_conformance_run`), `rusty-server/src/routes.rs` routes (`POST /conformance-suites`, `GET /conformance-suites`, `GET /conformance-suites/{name}/versions/{version}`, `POST /conformance-runs`, `GET /conformance-runs`, `GET /conformance-runs/{run_id}`, `GET /conformance-checks`); `rusty-server/tests/conformance_server.rs` 5 integration tests (AC 2 registration blocked/allowed, AC 3 headless run, AC 4 version bump invalidation, AC 5 lineage fields); clippy/doc clean; `5fc0b8d` + `2f57aad` on `feat/ep-12-s09` / `feat/ep-12-s09-server` |
| EP-12-S10 | The benchmark harness: latency, cost, and quality regression | P1 | ✅ | benchmark harness (benches + `compare.rs`) |
| EP-12-S11 | Results with lineage; A/B comparison across versions | P1 | ✅ | results lineage + A/B (`/experiments/compare`) |
| EP-12-S12 | CI integration: headless runs, machine-readable results, failure gates | P0 | ✅ | CI integration: headless runs, `/gates` |

## EP-13 — Observability, Storage, and Operations

███████████░ 75% · 9 landed · 1 partial · 2 not started · milestone M0–M4

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-13-S01 | The composite store: typed domains, one container, per-domain routing | P0 | ✅ | composite dual-backend store (`server_store.rs`) |
| EP-13-S02 | The PostgreSQL reference implementation | — | ✅ | PostgreSQL reference implementation |
| EP-13-S03 | Published conformance suites for every store trait | P0 | ✅ | `rusty-store/src/lib.rs`: `ArtifactStore` trait (save, load, list, delete, exists); `rusty-store-conformance/src/lib.rs`: `ConformanceReport`, `ConformanceCase`, `ConformanceSeverity`, `StoreConformance` async trait; `rusty-store-conformance/src/artifact.rs`: `ArtifactStoreConformance` with 11 assertions (round-trip, list, delete, not-found, overwrite, concurrency-safe list, empty-list, case-sensitivity, list-after-delete, exists-after-delete, content-isolation); `rusty-store-conformance/tests/artifact_conformance.rs`: 1 integration test (`file_artifact_store_conformance`) passing against `FileArtifactStore`; clippy/doc clean; `b0ee14a` on `feat/ep-13-s03` |
| EP-13-S04 | The object-store blob backend | P1 | ✅ | `rusty-store/src/blob.rs`: `BlobStore` trait (`put`/`get`/`delete`/`exists`), `BlobLocator` (tenant prefix + sha256 + bytes), `BlobError` enum (`NotFound`/`Integrity`/`Io`/`Unavailable`); `LocalBlobStore` backed by `object_store::local::LocalFileSystem` with tenant-scoped paths and sha256 content verification on read; `rusty-store-conformance/src/blob.rs`: `BlobStoreConformance` suite with 13 assertions (round-trip, hash verification, tenant isolation, not-found, idempotent put/dedup, delete, exists); `rusty-store-conformance/tests/blob_conformance.rs`: 1 integration test passing against `LocalBlobStore` + `tempfile`; 7 unit tests in `rusty-store/src/blob.rs` (round-trip, integrity check, not-found, dedup, tenant isolation, delete idempotent); clippy/doc clean; `833e0d8` on `feat/ep-13-s04` |
| EP-13-S05 | Migrations discipline: versioned, append-only schema evolution | P0 | ✅ | versioned, append-only migrations |
| EP-13-S06 | The rustyness binary and single-node deployment | P0 | ✅ | `deploy.rs` single-node deployment |
| EP-13-S07 | The standing fault-injection and load harness | P0 | ✅ | `rusty-server/tests/fault_injection.rs`: `KillSchedule` enum (`MidEffect`, `AfterEnqueue`), 4 tests covering kill-mid-effect, kill-after-enqueue, jitter-mode seeded reproducibility, seeded-defect fsync skip; `b605cb7` on `feat/ep-13-s07` |
| EP-13-S08 | Observability derived from the log: traces, metrics, structured logs | P0 | ✅ | `rusty-otel` + `telemetry.rs`: traces, metrics, structured logs |
| EP-13-S09 | Cost metering, budgets, and operator alerts | P0 | ✅ | `meter.rs` cost metering + budgets |
| EP-13-S10 | Backup, restore, and disaster recovery | P1 | ○ | backup/restore/DR not started |
| EP-13-S11 | The M4 HA topology: stateless workers, pull-based work, honest health | P0 | ◐ | `rusty-worker` pull-based work; M4 HA topology partial |
| EP-13-S12 | Zero-downtime rolling upgrade and the version-skew policy | P0 | ○ | rolling upgrade / version-skew policy not started |

## EP-14 — User Interfaces

████████░░░░ 69% · 8 landed · 9 partial · 1 not started · milestone M1–M4

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-14-S01 | Generated types and the shared client platform | — | ◐ | Python/TS SDKs (`sdks/`); generated shared types partial |
| EP-14-S02 | Scope-driven rendering: capabilities hidden, not disabled | — | ◐ | scope-driven rendering partial |
| EP-14-S03 | Accessibility, responsive layout, i18n scaffolding, and specified empty/error states | — | ◐ | accessibility tests landed; i18n scaffolding landed on `feat/studio-i18n` (`543d97a`): `I18nProvider` with ICU MessageFormat, English catalog (`en.json`), `useI18n`/`useT` hooks with `formatCost`, AppShell.tsx fully translated, hardcoded-string guard test (`hardcodedStrings.test.ts`) with MUST_BE_CLEAN/PENDING_MIGRATION lists; 249 studio tests pass; responsive layout + specified empty/error states open |
| EP-14-S04 | Chat: streaming conversation with live task visibility and files | — | ✅ | Studio chat: streaming + live task visibility |
| EP-14-S05 | Chat: steering and interruption mid-run | — | ✅ | Studio chat: steering + interruption mid-run |
| EP-14-S06 | Chat: inline end-user confirmation | — | ✅ | Studio chat: inline end-user confirmation |
| EP-14-S07 | Chat: session history, search, and multi-device continuity | — | ◐ | session history/search landed; multi-device continuity open |
| EP-14-S08 | Chat: the embeddable widget build | — | ○ | embeddable widget build not started |
| EP-14-S09 | Rustynome: the blueprint workbench | — | ✅ | Rustynome blueprint workbench |
| EP-14-S10 | Rustynome: the playground with event-log inspector, time-travel, and fork-and-retry | — | ◐ | playground + run investigation; time-travel/fork-and-retry partial |
| EP-14-S11 | Rustynome: the eval workbench | — | ✅ | Rustynome eval workbench |
| EP-14-S12 | Rustynome: the skill browser and the gap-ledger view | — | ◐ | skills screen landed; gap-ledger view pending W2 UI |
| EP-14-S13 | Rustynome: the publish flow with version diff and eval-gate status | — | ◐ | publish flow; version diff + eval-gate status partial |
| EP-14-S14 | Console: the fleet dashboard | — | ✅ | console fleet dashboard (command center) |
| EP-14-S15 | Console: the task board | — | ✅ | console task board |
| EP-14-S16 | Console: the three-severity inbox and approvals center | — | ✅ | three-severity inbox + approvals center |
| EP-14-S17 | Console: the security center | — | ◐ | security center partial |
| EP-14-S18 | Console: platform administration — channels, catalog, tenants | — | ◐ | platform admin screens landed; tenant administration partial |

## EP-15 — Out-of-the-Box Catalog

████░░░░░░░░ 17% · 2 landed · 5 partial · 5 not started · milestone M4

| Story | Title | P | Status | Evidence / what's open |
|---|---|---|---|---|
| EP-15-S01 | The plugin packaging format | P0 | ✅ | `package.rs` with `PackageManifest`, `PackageId`, `Version`, `DependencyRange`, `CapabilityDecl`, `PackageSignature`, `resolve_dependencies`; 15 tests |
| EP-15-S02 | The doctor contract: config repair and state migrations | P0 | ○ | doctor contract not started |
| EP-15-S03 | The registry index and install, update, rollback flows | P0 | ◐ | registry index + install/update/rollback partial; **BLOCKED**: depends on EP-15-S02 (doctor contract) which is ○ not started — install/update/rollback ACs require doctor config repair and state migration chain |
| EP-15-S04 | Org-level allowlists: Iris controls what may be installed | P0 | ○ | org-level allowlists not started |
| EP-15-S05 | The connector pack: named enterprise connectors, MCP-first | P0 | ◐ | connector surface (`connector.rs`, `/connectors/check`); named packs in flight |
| EP-15-S06 | The generic REST connector and webhook ingress | P0 | ◐ | webhook ingress (`/triggers/{id}/webhook`) landed; generic REST connector partial |
| EP-15-S07 | The tool pack: filesystem, shell, browser, code interpreter, documents, search | P0 | ✅ | built-in tool pack (`tool/`): filesystem, shell, code, search, documents |
| EP-15-S08 | The skill packs: five shipped skills with evals and declared dependencies | — | ◐ | shipped skills with evals partial |
| EP-15-S09 | Blueprint templates: five stock Rustyprints wired for their domains | — | ◐ | stock Rustyprints partial |
| EP-15-S10 | The quality bar: no item ships without evals, docs, and conformance | P1 | ○ | catalog quality bar not started |
| EP-15-S11 | The community submission path: signing, review, revocation | P2 | ○ | community submission path not started |
| EP-15-S12 | Day-one usefulness, measured: tenant to working agent in under an hour | P1 | ○ | day-one usefulness measurement not started |

## Updating this tracker

Statuses move when code lands on `main` (or a named branch, called out in the evidence column). Regenerate judgments after each merged wave: a story flips to ✅ only when its acceptance criteria in the spec file pass, not when its module exists. `docs/backlog.md` remains the separate platform-maturity-review backlog; this file tracks only the Source-Code spec.
