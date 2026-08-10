//! # rusty-agent-server
//!
//! The network face of [`rusty_agent_runtime`]: an axum-based HTTP + SSE server
//! implementing a pragmatic Agent-Protocol subset (see
//! `docs/rusty-server-design.md`). The server ships as a **library** —
//! users build their graphs, register them in a [`GraphRegistry`], and call
//! [`serve`] (or [`router`] to embed the routes in a larger application):
//!
//! ```no_run
//! use rusty_agent_runtime::prelude::*;
//! use rusty_agent_server::{serve, GraphRegistry, ServerConfig};
//!
//! # async fn demo(graph: Graph, spec: StateSpec) -> std::io::Result<()> {
//! let mut registry = GraphRegistry::new();
//! registry.register("my_agent", graph, spec);
//!
//! let config = ServerConfig::new(
//!     "127.0.0.1:8080".parse().unwrap(),
//!     "./data/checkpoints",
//! );
//! serve(registry, config).await
//! # }
//! ```
//!
//! ## Endpoint inventory (v0.5)
//!
//! | Endpoint | Purpose |
//! |---|---|
//! | `GET /ok` | liveness |
//! | `GET /info` | service version + registered graphs and their channels |
//! | `POST /threads` | create a thread bound to a registered graph |
//! | `POST /threads/{id}/fork` | time travel: copy the thread's checkpoint history (full or up to `checkpoint_id`) into a new thread |
//! | `GET /threads/{id}/state` | latest checkpoint as `{values, next, checkpoint}` (the checkpoint ref carries its pinned `policy_version`, R0.8 wave 4) |
//! | `POST /threads/{id}/state` | write a new checkpoint (`update_state` analog; `as_node` is accepted for LangGraph compatibility but not recorded; optional `enqueue: [...]` submits tasks atomically with the checkpoint through the R0.6 wave-2b transactional outbox — one transaction on Postgres, outbox-first best-effort ordering on the JSON-file backend) |
//! | `POST /threads/{id}/history` | checkpoint list, newest first, `limit`/`before` |
//! | `POST /threads/{id}/runs` | background run: `202 + run_id` |
//! | `POST /threads/{id}/runs/wait` | blocking run: terminal result as JSON |
//! | `POST /threads/{id}/runs/stream` | run with SSE streaming (`updates`/`values`/`messages`/`metadata`/`error`/`end`); a fresh run starts a new frame sequence, so `Last-Event-ID` is ignored here |
//! | `GET /runs/{id}/stream` | attach to an existing run's SSE stream: replay honoring `Last-Event-ID`, then live frames |
//! | `GET /runs/{id}/events` | Flight Recorder: the run's journaled `RunEvent`s as `{run_id, events, complete}` (snapshot flushed per checkpoint boundary and at run completion; persisted under `{store_path}/journals/` or the `server_journals` table; fetchable by run id even after the live run record is evicted or the process restarts) |
//! | `GET /runs/{id}/fixture` | Flight Recorder: download the run as a portable `ReplayFixture` bundle (journal + graph topology hash + final checkpoint) for CI replay |
//! | `POST /runs/replay` | Flight Recorder: re-drive a journaled run against its registered graph and verify the replayed evidence → `{run_id, verified, expected_events, actual_events, first_divergence}` (`422` when the graph is not registered in this process or the journal carries recorded effect calls) |
//! | `GET /runs/diff?base=&branch=` | Flight Recorder: structural diff of two runs' journals (core's `BranchDiff` shape: `first_divergent_seq`, `added`/`removed` events, per-step channel diffs, token/cost totals) |
//! | `DELETE /threads/{id}/runs/{run_id}` | rollback: delete a finished run's checkpoints (JSON-file checkpointer only; `409` on Postgres) |
//! | `GET /runs/{run_id}` | run status polling (plus `output`/`error`/`interrupt` once terminal) |
//! | `POST /assistants` | create a named graph alias with config metadata |
//! | `GET /assistants` / `GET /assistants/{id}` | list / fetch assistants |
//! | `POST /assistants/{id}/archive` / `POST /assistants/{id}/restore` | reversibly retire or return an assistant using an expected-active-version guard; archived assistants retain their immutable lineage and history but reject new runs |
//! | `GET /assistants/{id}/versions` | list the bounded immutable configuration lineage and active serving pointer |
//! | `POST /assistants/{id}/versions` | stage an immutable child version against the exact active base (never activates it) |
//! | `GET /assistants/{id}/versions/{version_id}` | fetch one exact immutable configuration version for review |
//! | `POST /assistants/{id}/versions/{version_id}/activate` | atomically activate or roll back to a reviewed version using an expected-active guard |
//! | `POST /crons` | schedule recurring runs (interval secs or 5-field cron expr) |
//! | `GET /crons` / `DELETE /crons/{id}` | list / delete crons |
//! | `POST /triggers` | create an event-driven trigger: name, target (assistant or thread), action (`start_run` / `resume_thread` / `send_message`), `{{event.*}}` input template, per-trigger webhook secret, optional debounce window |
//! | `GET /triggers` / `GET /triggers/{id}` / `PATCH /triggers/{id}` / `DELETE /triggers/{id}` | trigger registry (tenant-scoped) |
//! | `POST /triggers/{id}/webhook` | signed event ingress: HMAC-SHA256 over the raw body, `X-Rusty-Signature: sha256=<hex>`, constant-time compare; `401` unsigned/invalid. Not behind the API-key layer — the signature is the credential and resolves the owning tenant |
//! | `GET /triggers/{id}/events` | the trigger's event log (payload hash, action, run id, status), newest first |
//! | `GET /triggers/{id}/dead-letter` | failed events, inspectable and replayable |
//! | `POST /triggers/{id}/events/{event_id}/replay` | re-execute a logged event immediately (the replay is itself logged) |
//! | `PUT /store/{ns}/{key}` | upsert a JSON value in a namespace (`201` create, `200` replace) |
//! | `GET /store/{ns}/{key}` / `DELETE /store/{ns}/{key}` | fetch / delete one item |
//! | `GET /store/{ns}` | list a namespace's items |
//! | `POST /tasks` | R0.6 durable task queue: enqueue a task `{kind, payload, pool?, max_attempts?, idempotency_key?, worker_version?}` → `201 {task_id, deduplicated}` (`200` + `deduplicated: true` when the idempotency key already names a live task in this tenant; `429 quota_exceeded` when the tenant is over its wave-3 task quota — see `ServerConfig::with_task_quota`) |
//! | `POST /tasks/outbox` | R0.6 wave 2b: enqueue through the transactional outbox (same payload as `POST /tasks`, same quota gate) → `202 {task_id, deduplicated}`; the relay publishes the task into the queue within one poll interval, at-least-once, deduped on the idempotency key |
//! | `POST /tasks/claim` | R0.6: claim the oldest claimable task `{worker_id, pools?, worker_version?, lease_ms}` → `200 {task}` with a fresh lease, `204` when nothing is claimable. Wave 3: pools at their configured concurrency limit (`ServerConfig::with_pool_limit`) hand out nothing, and a task pinned with `worker_version` is leased only to a worker advertising that exact version |
//! | `GET /tasks/metrics` | R0.6 wave 3: the autoscaling signals, tenant-scoped — per-pool queue depth, live leases, lease saturation against the configured limit, and oldest-visible-task age. Metrics, not a mechanism: the autoscaler is the operator's |
//! | `POST /tasks/{id}/heartbeat` | R0.6: extend the held lease `{worker_id, lease_ms}` → `{lease_expires_at}`; `409` when the lease is lost |
//! | `POST /tasks/{id}/complete` | R0.6: settle the held lease `{worker_id, result, receipt?}` → updated task record; `409` when the lease is lost. The optional `receipt` (wave 2b: `{provider, provider_id, idempotency_key, task_id?}`) is the worker's report of an idempotent effect's provider confirmation; the server journals it into the task's run as an `effect_receipt` event |
//! | `POST /tasks/{id}/fail` | R0.6: record a failed attempt `{worker_id, error_class, message, retryable}` → `{requeued, next_attempt_at, dead, escalation}` (backoff + jitter requeue, or dead-letter); `409` when the lease is lost. R0.7 wave 2: a failed agent turn (non-`cancelled`) also feeds the agent's supervision policy — `escalation` reports where the escalation notice landed when the restart intensity was exceeded |
//! | `GET /tasks/{id}` | R0.6: the task record (404 unknown/cross-tenant) |
//! | `GET /tasks?status=…` | R0.6: the tenant's tasks, oldest first; `status=dead` is the DLQ listing |
//! | `POST /agents` | R0.7 Agent Fabric (wave 1): register an agent `{agent_id, manifest, metadata?}` — the manifest is a core `CapabilityManifest` (`agent_kind`, `manifest_version`, `accepts`, …) → `201`; `409` when the id is taken |
//! | `GET /agents` / `GET /agents/{id}` | R0.7: list the tenant's agents / fetch one registration (404 unknown/cross-tenant) |
//! | `POST /agents/{id}/mailbox` | R0.7: send a message into the agent's mailbox `{kind, payload, idempotency_key?, …}` → `201 {task_id, deduplicated}` (`200` when the key already names a live message); `400` when the manifest's `accepts` does not declare the kind; same quota gate as `POST /tasks` |
//! | `GET /agents/{id}/status` | R0.7: the agent's activation lease plus mailbox gauges (`queued` / `in_flight` / `dead` message counts) |
//! | `POST /agents/{id}/activate` | R0.7: claim the agent's single activation lease `{worker_id, lease_ms}` → `200 {owner, fencing, lease_expires_at}`; `409` + the current lease when another host holds it live (expired leases are stolen, fencing bumped) |
//! | `POST /agents/{id}/activate/heartbeat` | R0.7: renew the held activation `{worker_id, fencing, lease_ms}`; `409` on fencing loss |
//! | `POST /agents/{id}/activate/release` | R0.7: drop the held activation `{worker_id, fencing}` so a draining host's replacement activates promptly; `409` on fencing loss |
//! | `POST /agents/{id}/mailbox/next` | R0.7: claim the oldest queued mailbox message as one turn `{worker_id, fencing, lease_ms}` → `200 {task}` leased to the worker, `204` when empty or a turn is already in flight (one message at a time per agent is server-enforced), `409` when the activation lease is not held. The turn settles through the ordinary `/tasks/{id}/heartbeat|complete|fail` protocol |
//! | `GET /agents/{id}/supervision` | R0.7 wave 2: the agent's supervision evidence — declared policy, latches, full attempt history, and the journaled `SupervisionEvent` / `AgentExit` events of its supervision journal (integrity re-verified on read) |
//! | `POST /agents/{id}/cancel` | R0.7 wave 2: cancel the agent's outstanding mailbox traffic — queued/retry-scheduled messages go terminal-`cancelled`, the leased turn keeps its lease with `cancel_requested` set — and journal an `AgentExit`; idempotent (`200` with empty lists when nothing is outstanding) |
//! | `POST /agents/{id}/restart` | R0.7 wave 2: the operator's manual restart — records a `manual_restart` supervision attempt, clears the escalation/deadline latches, and journals the event so the agent's next turn runs fresh |
//! | `POST /teams/{team_id}/cancel` | R0.7 wave 2: cancel every agent registered with the `team_id` label, member by member with the agent-cancel semantics; `404` when no tenant agent declares the team |
//! | `POST /memory` | R0.8 Rusty Learn (wave 1): write a governed memory record `{kind, scope, content, author, key?, tags?, priority?, confidence?, written_at?, valid_from?, valid_until?, expires_at?, supersedes?, evidence?, run_id?, parent?}` → `201 {memory_id, created, record}` (`200` + `created: false` when the content address is already stored — writes are idempotent by construction). Gates: `run` scope is runtime-only (`400`); `agent` scope requires the agent registered with `StateScope::Private` declared (`404` / `403`); `tenant` scope id must be the caller's tenant (`403`); confidence defaults to `1.0` for human authors and is required otherwise (`400`). With `run_id`, the write is journaled into that run as a `memory_write` event (best-effort) |
//! | `GET /memory/{memory_id}` | R0.8: fetch one record by content address (`404` unknown/cross-tenant); artifact-spilled bodies are re-inlined, so the served record is self-contained |
//! | `POST /memory/query` | R0.8: structured retrieval — the `MemoryQuery` filters plus optional `budget`, `run_id`, `parent`. `as_of` resolves at read time. With `budget`, answers the deterministic token-bounded `MemoryAssembly` (`422` when a hard budget overflows); without, the rank-ordered records. With `run_id` (budget required), the read is journaled into that run as a `memory_read` event (best-effort) |
//! | `POST /memory/corrections` | R0.8 (wave 2): submit a human correction `{correction_id, author, target, corrected, scope, rationale?, run_id?, parent?}` → `201 {correction_id, attribution, candidate, memory_id, created, record, superseded, example_id}`. Author attribution is mandatory (validated at deserialization). Run scope adopts directly; agent scope or wider yields an attributed candidate (`candidacy: pending`, queryable via `candidates_only`). A `run_event` target additionally yields an `example`-kind record — the journaled input plus the corrected behavior — journaled into the corrected run through the memory-write seam; a `memory` target inherits the target's key, and same-key correction writes auto-supersede the prior record |
//! | `POST /memory/consolidate` | R0.8 (wave 2): enqueue a consolidation as a durable `memory_consolidation` task `{scope, memory_ids, distiller, key?, tags?, priority?, pool?, run_id?, parent?}` → `201 {task_id, deduplicated, kind}` (deduped on scope + sorted source set). The claiming worker owns the distillation semantics and writes the `summary` record through the governed write path with the sources in `evidence.source_memory_ids` (which supersedes them) and the task payload's `written_at`; the task settles through the unchanged lease protocol |
//! | `POST /memory/conflicts` | R0.8 (wave 2): the conflict review listing `{scope?}` → `{conflicts: [{scope, key, memory_ids, overlap}]}` — live same-key records with overlapping validity and contradictory content. Flags only; nothing is resolved |
//! | `POST /memory/forget` | R0.8 (wave 2): erase one record `{memory_id, reason, run_id?, parent?}` → `200 {forgotten, invalidated, tombstone}`. Real deletion; dependent summaries are invalidated (deleted) transitively; a metadata-only `memory_forget` tombstone is journaled into the named run (best-effort) |
//! | `POST /memory/forget_scope` | R0.8 (wave 2): erase every record at a scope address `{scope, reason, run_id?, parent?}` → `200 {forgotten, invalidated, tombstones}` — one tombstone per forgotten record; idempotent (empty scope → `200` with empty lists); tenant scope is self-only (`403`) |
//! | `POST /learn/candidates` | R0.8 (wave 3): register a distilled candidate `{candidate, run_id, parent?}` → `201 {candidate_id, created, record}` (`200` + `created: false` when the content address is already stored — creation converges). The address is verified (`422`); the `candidate_created` event is journaled into `run_id`'s journal before the store write — hard-fail: an unresolvable run stops the transition (`404`) |
//! | `GET /learn/candidates` / `GET /learn/candidates/{id}` | R0.8 (wave 3): the tenant's candidates (sorted by id) / fetch one record (`404` unknown/cross-tenant) |
//! | `POST /learn/candidates/{id}/evaluate` | R0.8 (wave 3): drive the configured `CandidateEvaluator` `{request, run_id, parent?}` → `200 {candidate_id, status, evaluation}`; `409` when no evaluator is configured or the lifecycle forbids re-evaluation; `422` when the evaluation fails or violates the seam contract (it must name this candidate and the request's dataset version) |
//! | `POST /learn/candidates/{id}/promote` | R0.8 (wave 3): run the promotion gate `{run_id, approval?, tag?, parent?}` → `200 {candidate_id, status, receipt, pointer}`. `403` on approval failures (out-of-envelope promotion needs an `ApprovalToken` scoped to the candidate's promotion effect id — non-transferable), `422` on evidence failures, `409` when the candidate is not `evaluated`. The status flip and the version-pointer move are one store transition. R0.11 (wave 1): the optional `tag` moves the pointer on the environment-tagged surface (`prompt:system@prod`) through the unchanged machinery; absent is the untagged, pre-R0.11 behavior |
//! | `POST /learn/candidates/{id}/rollback` | R0.8 (wave 3): re-point the surface to the displaced version `{run_id, cause, tag?, parent?}` → `200 {candidate_id, status, receipt, pointer}`; `409` when the candidate is not `promoted` or the pointer no longer serves it. Byte-exact: the pointer's `to` is the promotion's recorded `previous`, and candidates are content-addressed — the restored version is the version that served. R0.11 (wave 1): `tag` rolls back the tagged surface's pointer |
//! | `GET /learn/versions` | R0.8 (wave 3): the tenant's version pointers, sorted by surface |
//! | `POST /registry/artifacts` | R0.11 (wave 1): declare a registry artifact `{family, name, owner}` → `201 {surface, created, artifact}` (`200` converged on an identical re-declaration; `409` when the surface is taken under a different family or owner; `422` on a name outside the naming rules). The artifact is an index over the candidate pipeline — a commit is a candidate, never a fork |
//! | `GET /registry/artifacts?family=` / `GET /registry/artifacts/{family}/{name}` | R0.11 (wave 1): the tenant's artifacts (optionally one family's, sorted by surface) / fetch one (`404` unknown/cross-tenant) |
//! | `POST /registry/artifacts/{family}/{name}/commits` | R0.11 (wave 1): commit a candidate `{candidate_id, committed_at?}` → `200 {surface, committed, commit, commits}` (`200` + `committed: false` when already committed; `404` unknown artifact/candidate; `422` family or surface mismatch; `409` concurrent commit) |
//! | `GET /registry/artifacts/{family}/{name}/commits` | R0.11 (wave 1): the history walk — commits oldest first, joined with each candidate's status and author |
//! | `GET /registry/artifacts/{family}/{name}/diff?from=&to=` | R0.11 (wave 1): the diff view between two committed versions, computed on read (never stored) — line diff for prompts, structural canonical-JSON diff for JSON families; `422` when either candidate is not committed to this artifact |
//! | `POST /policy/versions` | R0.8 (wave 4): register an immutable executor policy body `{version?, policy}` → `201 {version, record}` (`200` converged when the version already names exactly this body; `409` when it names a different one — registry immutability; `400` invalid version). Without `version`, the content-derived `policy-{hash12}` name is minted. The reserved `static-v0` floor is never registerable |
//! | `GET /policy/versions` / `GET /policy/versions/{version}` | R0.8 (wave 4): the tenant's registered policy bodies (sorted by version) / fetch one (`404` unknown/cross-tenant; the floor resolves as a synthetic record) |
//! | `POST /policy/activations` | R0.8 (wave 4): move the active-version pointer `{version}` → `200 {version, active}`; `422` when the version is not registered (the floor is always activatable — reverting to pre-learning behavior needs no candidate) |
//! | `GET /policy/active` | R0.8 (wave 4): the tenant's active policy — the last activation's body, or the floor when the registry never moved |
//! | `GET /policy/epochs` | R0.8 (wave 4): the epoch history — each activation's reign window plus the admission bindings recorded inside it (and the implicit floor epoch covering pre-activation bindings) |
//! | `GET /policy/drift` | R0.10 (wave 4): the drift check — the named (default active) candidate-derived version's journaled production decisions measured against the twin baseline it was promoted on; `422` for the floor and API-registered bodies (no promotion baseline) |
//! | `POST /capsules` | R0.9 Rusty Capsules (wave 1): register an immutable capsule manifest `{manifest}` → `201 {capsule_id, record}` (`200` converged when the content address already names exactly this manifest; `409` when the `(name, version)` pin is claimed by a different address — registry immutability; `422` when the manifest fails validation). The manifest's content address is derived; callers never mint ids |
//! | `GET /capsules` / `GET /capsules/{id}` | R0.9 (wave 1): the tenant's registered capsule manifests (sorted by content address) / fetch one (`404` unknown/cross-tenant) |
//! | `POST /capsules/resolve` | R0.9 (wave 1): resolve a run's capsule pins `{pins: {name: version}, run_id, parent?}` → `200 {resolutions}` — each pin's stored manifest re-derives its content address before answering (a tampered record fails closed, `422`; an unknown pin is `404`), and one `capsule_resolved` event per pin is journaled into `run_id`'s journal (hard-fail: an unresolvable run is `404`). R0.9 wave 2 adds the optional `budget` field and, with the `capsules` feature, the admission composition: Cedar decides the admission and each declared grant (`403` on refusal, one journaled `capsule_denied` per forbidden grant), overlays intersect the effective grants, and budgets compose (`422` on a token/cost overspend) |
//! | `POST /capsule_policies/versions` / `GET /capsule_policies/versions` / `GET /capsule_policies/versions/{version}` | R0.9 (wave 2, feature `capsules`): register an immutable Cedar policy body `{policy_text, version?}` → `201 {version, record}` (`200` converged; `409` when the version names a different body; `422` unparseable) / list the tenant's bodies (sorted by version) / fetch one (`404` unknown/cross-tenant). Without the feature: `503 capsule_policy_unavailable` |
//! | `GET /capsule_policies/active` / `POST /capsule_policies/active` | R0.9 (wave 2, feature `capsules`): the tenant's active policy body (`404` in the unconfigured posture) / move the active-version pointer `{version}` → `200 {active}` (`422` unregistered version); the move eagerly refreshes the revocation cache. Without the feature: `503 capsule_policy_unavailable` |
//! | `POST /capsules/overlays` / `GET /capsules/overlays` / `GET /capsules/overlays/{name}` | R0.9 (wave 2, feature `capsules`): attach (or replace) a tenant overlay `{overlay}` → `201` new / `200` replaced (`403` when the active policy refuses a widening attach; `422` invalid) / list the tenant's overlays (sorted by name) / fetch one (`404` unknown/cross-tenant). Without the feature: `503 capsule_policy_unavailable` |
//! | `GET /runs/{id}/receipt` | R0.9 (wave 3): the run's signed `RunReceipt` — minted on first request over the run's reverified journal (manifest and executor policy read back from its last checkpoint header), then stored and served while the journal's head stands; a run whose journal advanced gets a fresh receipt. `409` when nothing is persisted yet (queued or pre-checkpoint); `404` unknown/cross-tenant |
//! | `POST /receipts/verify` | R0.9 (wave 3): verify caller-supplied evidence `{snapshot, receipt, key_id?}` → `200` with the typed `VerifiedRun` summary, or `422 receipt_verification_failed` naming the mismatched component (`journal_head` / `manifest_digest` / `effect_ledger` / `denials_ledger` / `capsule_resolutions` / `capsule_policies` / `signer_key_id` / `signature`). The public key resolves from the deployment's key history by `key_id` (default: the receipt's `signer`); an unknown key id is `404` |
//! | `GET /receipt_keys` | R0.9 (wave 3): the deployment's signing-key history (public keys, registration and retirement instants) plus the active key id — what an auditor needs to verify receipts offline |
//! | `POST /receipt_keys/rotate` | R0.9 (wave 3): rotate the signing key → `201 {previous_key_id, key_id, public_key, event_id}`; the new key id is journaled as a `signing_key_rotated` event in the deployment's receipts journal, and receipts signed by the retired key keep verifying against the history |
//! | `GET /receipt_keys/journal` | R0.9 (wave 3): the deployment's key-lineage journal — the chained `signing_key_rotated` events, integrity re-verified on read |
//! | `POST /mcp` | R0.9 (wave 4): the MCP bridge (pinned revision `2025-03-26`) — every registered graph as one tool. `initialize` / `ping` / `notifications/*` / `tools/list` (tool schemas derived from each graph's state spec: append channels are arrays, deep-merge channels objects) / `tools/call` (runs the graph on a fresh thread; plain-JSON answer, or SSE `notifications/progress` + the final response when the request accepts `text/event-stream`; a mid-stream disconnect cancels the run). JSON-RPC errors in the envelope with HTTP 200; notifications answer `202` |
//! | `GET /.well-known/agent-card.json` | R0.9 (wave 4): the A2A agent card (pinned spec `0.3.0`), derived from the registry on every read — one skill per registered graph; deterministic, no timestamps |
//! | `POST /a2a` | R0.9 (wave 4): the A2A JSON-RPC task surface — `message/send` (enqueue a durable task, `kind = "a2a"`, idempotent on `messageId`; a capsule data part `{"capsule": {name, version}, "input"}` routes to the in-process executor over the `a2a-capsule` pool, plain messages queue on `a2a` for external workers) / `message/stream` (the same plus SSE status events) / `tasks/get` / `tasks/cancel`. The context id maps to one Flight Recorder journal (`a2a-{tenant}-{contextId}`), so capsule executions leave their evidence on the native `/runs/{id}/events` and `/fixture` endpoints |
//! | `PUT /capsules/{id}/blob` | R0.9 (wave 4): upload the component bytes a registered manifest's `build_digest` commits to (raw body) → `201 {capsule_id, sha256, bytes}`; `404` unknown capsule, `422` digest mismatch, `409` different bytes under a taken address (registry immutability over bytes) |
//! | `POST /connections` | R0.11 (wave 3): register a connection `{provider, subject?, scopes, token}` → `201 {connection}`; the token material is envelope-encrypted before it touches the store and the registration journaled — the bytes never travel beyond the broker |
//! | `GET /connections` | R0.11 (wave 3): the tenant's connection records (metadata only — sealed material never leaves the broker), sorted by id |
//! | `GET /connections/{id}` | R0.11 (wave 3): one connection record → `200 {connection}`; `404` unknown/cross-tenant |
//! | `POST /connections/{id}/consent` | R0.11 (wave 3): record a consent act `{scopes?, token?}` (one required, `422` otherwise) → `200 {connection, journaled}`; a scope-set change journals `connection_consented`, material-only `connection_refreshed`; re-recording the same fact converges without a second event. Re-activates a `needs_reauth` connection — this is the re-auth path |
//! | `POST /connections/{id}/revoke` | R0.11 (wave 3): revoke `{reason?}` → `200 {connection, event_id?}` (re-revocation converges with no `event_id`); outstanding handles fail at their next use with a typed, journaled `connection_revoked` denial |
//! | `DELETE /connections/{id}` | R0.11 (wave 3): revoke-then-erase → `200 {deleted: true}`; the sealed material is really deleted, and resolution fails closed `unknown_connection` thereafter |
//! | `GET /connections/{id}/health` | R0.11 (wave 3): the connection's status and health counters (last failure class, consecutive failures, last refresh) |
//! | `GET /broker/journal` | R0.11 (wave 3): the deployment's broker evidence chain — registrations, consents, refreshes, revocations, issuances, uses, and denials, integrity re-verified on read (the `receipt_keys/journal` precedent for a second control plane) |
//!
//! Runs support `command.resume` (HITL), `config.recursion_limit`, the
//! `reject` / `enqueue` multitask strategies (one active run per thread),
//! `assistant_id` (resolved to its bound graph, with the assistant's
//! `config.recursion_limit` as a default), and `checkpoint.checkpoint_id`
//! (time-travel replay from that checkpoint instead of the latest).

mod a2a;
mod agents;
mod assistants;
mod auth;
mod broker;
pub mod capsule_policy;
mod capsules;
mod coordination;
mod crons;
mod error;
mod journals;
mod learn;
mod mcp_bridge;
mod memory;
mod outbox;
mod policy;
mod receipts;
mod registry;
mod replay;
mod routes;
mod runs;
mod server_store;
mod sse;
mod store;
mod supervision;
mod tasks;
mod threads;
mod triggers;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use rusty_agent_runtime::capsule::ResourceBudget;
use rusty_agent_runtime::graph::Graph;
use rusty_agent_runtime::learn::{CandidateEvaluator, EnvironmentTag, PromotionEnvelope};
use rusty_agent_runtime::state::StateSpec;
use tokio_util::sync::CancellationToken;

pub use error::ApiError;
pub use learn::{DatasetSource, DirectoryDatasetSource, EvalCandidateEvaluator, EvaluationAgent};
pub use runs::RunStatus;

/// Default bound on graceful shutdown (25 s): how long
/// [`serve_with_shutdown`] lets in-flight requests, runs, and the outbox
/// relay finish after the shutdown signal before the server stops anyway.
///
/// Chosen to fit under two outer backstops: Kubernetes' default 30 s
/// pod-termination grace (so the drain finishes before SIGKILL would
/// arrive) and the task queue's default 30 s lease (so a drained server has
/// released its work well before lease expiry would reassign it). The
/// durable machinery — checkpoint resume, lease expiry — remains the
/// correctness net; the grace bound only decides how *fast* the common
/// case is. Override with [`ServerConfig::with_shutdown_grace`].
pub const DEFAULT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(25);

/// Names the JSON-file layout already owns at the store root
/// (`agent_leases/`, `agents/`, `assistants/`, `capsules/`,
/// `capsule_policies/`, `coordinations/`, `crons/`, `journals/`, `learn/`,
/// `memory/`, `memory_artifacts/`, `outbox/`, `policy/`, `registry/`,
/// `store/`,
/// `tasks/`, `threads/`, `trigger_events/`, `triggers/`, plus the
/// `latest` pointer file inside each thread's checkpoint dir).
/// Client-chosen ids and tenant ids claiming one of these would write
/// checkpoints into platform directories (or platform records into
/// checkpoint dirs), so both `validate_client_id` and
/// [`ServerConfig::with_tenant_key`] reject them.
pub(crate) const RESERVED_NAMES: &[&str] = &[
    "agent_leases",
    "agents",
    "assistants",
    "capsules",
    "capsule_policies",
    "connections",
    "coordinations",
    "crons",
    "journals",
    "keys",
    "learn",
    "memory",
    "memory_artifacts",
    "outbox",
    "policy",
    "receipts",
    "registry",
    "store",
    "tasks",
    "threads",
    "trigger_events",
    "triggers",
    "latest",
];

/// One registered graph: the compiled topology plus the state schema the
/// executor needs to drive it.
#[derive(Debug, Clone)]
struct GraphEntry {
    graph: Graph,
    spec: StateSpec,
}

/// The set of graphs this server hosts — the Rust analog of the `graphs` map
/// in LangGraph's `langgraph.json`. Registration is compile-checked in user
/// code; a `GraphRegistry` is heterogeneous (each entry carries its own
/// [`StateSpec`]), which is safe because `State` is a JSON map at runtime.
#[derive(Debug, Default, Clone)]
pub struct GraphRegistry {
    entries: HashMap<String, GraphEntry>,
}

impl GraphRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a compiled graph under `name`, together with the state spec
    /// the executor should merge its node updates through. Re-registering a
    /// name replaces the previous entry.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        graph: Graph,
        spec: StateSpec,
    ) -> &mut Self {
        self.entries.insert(name.into(), GraphEntry { graph, spec });
        self
    }

    /// `true` if a graph is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// All registered graph names, sorted for stable output.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    /// The declared channel names of a registered graph's spec, sorted.
    pub fn channel_names(&self, name: &str) -> Vec<String> {
        let mut channels: Vec<String> = self
            .entries
            .get(name)
            .map(|entry| entry.spec.channel_names().map(str::to_owned).collect())
            .unwrap_or_default();
        channels.sort_unstable();
        channels
    }

    pub(crate) fn get(&self, name: &str) -> Option<(Graph, StateSpec)> {
        self.entries
            .get(name)
            .map(|entry| (entry.graph.clone(), entry.spec.clone()))
    }
}

/// Tenant quota for the durable task queue (R0.6 wave 3): three gauges,
/// each capped independently — tasks queued, tasks in flight, dead-letter
/// depth — enforced at **submission** (`POST /tasks`, `POST /tasks/outbox`,
/// and `update_state`'s atomic `enqueue` list). Over quota answers `429
/// quota_exceeded`, the honest shape for "retry this submission later".
///
/// The gauges are the store's `TaskUsage` definitions: the backlog
/// counts scheduled retries *and* outbox rows pending publication (a flood
/// through the outbox must not bypass the quota), in flight counts every
/// `leased` record, and the DLQ counts because an unbounded dead-letter
/// queue is a quiet disk-full outage. `None` (the default for every field)
/// means unlimited, preserving pre-wave-3 behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskQuota {
    /// Cap on the tenant's backlog (queued + retry-scheduled + pending
    /// outbox rows). A submission that would push the backlog past the cap
    /// is rejected.
    pub max_queued: Option<usize>,

    /// Cap on tasks in flight (status `leased`). Pure backpressure: a
    /// tenant already at the cap has its new submissions rejected until
    /// workers settle.
    pub max_in_flight: Option<usize>,

    /// Cap on dead-letter depth. A tenant at the cap must inspect and
    /// re-drive its DLQ before more work is accepted.
    pub max_dlq: Option<usize>,
}

impl TaskQuota {
    /// Every gauge capped: `TaskQuota { max_queued: Some(q),
    /// max_in_flight: Some(f), max_dlq: Some(d) }`. Field-by-field
    /// construction works too — the struct's fields are public and each
    /// `None` means unlimited.
    pub fn capped(max_queued: usize, max_in_flight: usize, max_dlq: usize) -> Self {
        Self {
            max_queued: Some(max_queued),
            max_in_flight: Some(max_in_flight),
            max_dlq: Some(max_dlq),
        }
    }

    /// `true` when no gauge is capped — the quota gate short-circuits
    /// without a store read (the default configuration's fast path).
    pub(crate) fn is_unlimited(&self) -> bool {
        self.max_queued.is_none() && self.max_in_flight.is_none() && self.max_dlq.is_none()
    }
}

/// Server configuration.
///
/// Checkpointing is rooted at `store_path` via
/// [`rusty_agent_runtime::checkpoint::JsonFileCheckpointer`]. Auth maps static API
/// keys (checked against the `X-Api-Key` header) to tenants: the legacy
/// single [`ServerConfig::with_api_key`] maps its key to the `default`
/// tenant, while [`ServerConfig::with_tenant_key`] adds `(tenant, key)`
/// pairs for multi-tenant deployments. With no keys configured the server
/// runs in open (dev) mode — no header required, everything lives in the
/// `default` tenant. Every tenant-scoped resource (threads + checkpoints,
/// assistants, crons, KV namespaces) is isolated per tenant; cross-tenant
/// access answers `404`. With the `postgres` feature,
/// `ServerConfig::with_postgres` switches **both** the run checkpointer
/// (core's `PostgresCheckpointer`) and the server store (assistants /
/// crons / threads / KV) to Postgres.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind when using [`serve`] (default `0.0.0.0:8080`).
    pub bind_addr: SocketAddr,

    /// Root directory for checkpoint files
    /// (`{store_path}/{thread_id}/{checkpoint_id}.json`). Also roots the
    /// JSON-file assistants/crons/threads/KV persistence. Unused for
    /// checkpointing when `database_url` is set (still used as the
    /// `store_path` reported by `GET /info`).
    pub store_path: PathBuf,

    /// Postgres connection URL. When set (requires the `postgres` feature —
    /// see `ServerConfig::with_postgres`), checkpoints live in core's
    /// `rusty_checkpoints` table and the platform surface in the
    /// `server_assistants` / `server_crons` / `server_threads` /
    /// `server_kv` / `server_journals` tables, all auto-migrated on
    /// connect. Connections are established lazily on first use.
    pub database_url: Option<String>,

    /// Per-thread in-flight run cap used as the **enqueue queue depth**
    /// (default 1). There is always at most one *active* run per thread.
    pub max_concurrent_runs_per_thread: usize,

    /// Static API key required via the `X-Api-Key` header, mapped to the
    /// `default` tenant (legacy single-key mode). `None` (the default) with
    /// an empty [`ServerConfig::api_keys`] is dev mode: no authentication.
    pub api_key: Option<String>,

    /// Additional `(api_key, tenant)` pairs for multi-tenant deployments
    /// (see [`ServerConfig::with_tenant_key`]). Each key maps to exactly one
    /// tenant; every tenant's threads, assistants, crons, and KV namespaces
    /// are isolated from all others (cross-tenant access answers `404`).
    pub api_keys: Vec<(String, String)>,

    /// Per-run SSE event-log capacity (frames retained for replay, default
    /// 1000).
    pub event_log_capacity: usize,

    /// How often the transactional-outbox relay publishes pending rows into
    /// the task queue (R0.6 wave 2b, default 250 ms). The relay is
    /// crash-safe at any interval — pending rows survive restarts and
    /// publishing dedupes on the task's idempotency key — so this only
    /// tunes outbox-to-queue latency, not correctness.
    pub outbox_relay_interval: std::time::Duration,

    /// Bound on graceful shutdown (R0.6 wave 2c, default
    /// [`DEFAULT_SHUTDOWN_GRACE`]): after the shutdown signal, in-flight
    /// requests and runs get this long to finish before the server stops
    /// regardless. Runs stopped by the drain are resumable from their last
    /// checkpoint, so a short grace costs a resume, never work.
    pub shutdown_grace: std::time::Duration,

    /// Per-pool in-flight caps for the durable task queue (R0.6 wave 3):
    /// pool name → maximum live leases. The claim path counts a pool's
    /// unexpired leases and stops handing out new ones at the cap, so
    /// pools coexist without starving each other (a saturated GPU pool
    /// never blocks IO-pool claims). Pools without an entry are
    /// uncapped — the pre-wave-3 behavior. See
    /// [`ServerConfig::with_pool_limit`].
    pub task_pool_limits: HashMap<String, usize>,

    /// The default tenant quota (R0.6 wave 3), applied to every tenant
    /// without an entry in [`ServerConfig::tenant_task_quotas`]. All
    /// gauges uncapped by default. See [`TaskQuota`].
    pub task_quota: TaskQuota,

    /// Per-tenant quota overrides (R0.6 wave 3), replacing
    /// [`ServerConfig::task_quota`] wholesale for the named tenant (an
    /// override *is* the tenant's quota, not a patch on the default).
    /// See [`ServerConfig::with_tenant_quota`].
    pub tenant_task_quotas: HashMap<String, TaskQuota>,

    /// The promotion envelope (R0.8 Rusty Learn, wave 3): the declared,
    /// per-deployment standing approval the promotion gate evaluates
    /// every promotion against. Defaults to
    /// [`PromotionEnvelope::r08_default`] — memory-set candidates at
    /// run/agent scope auto-promote on cleared evidence; everything else
    /// requires an approval token. See
    /// [`ServerConfig::with_promotion_envelope`].
    pub promotion_envelope: PromotionEnvelope,

    /// The candidate evaluator (R0.8 wave 3): the evaluation
    /// composition `POST /learn/candidates/{id}/evaluate` drives. `None`
    /// (the default) answers `409` — a deployment without an evaluator
    /// can hold and inspect candidates, but promotion is gated on
    /// evidence, and evidence requires an evaluator. See
    /// [`ServerConfig::with_candidate_evaluator`].
    pub candidate_evaluator: Option<Arc<dyn CandidateEvaluator>>,

    /// The deployment's default environment tag (R0.11 Extension Plane,
    /// wave 2): the promotion target a run resolves against when its
    /// registry binding declares no environment of its own. `None` (the
    /// default) resolves the untagged surface — the pre-R0.11 behavior.
    /// This is *declared configuration*: a run's environment is its
    /// binding's tag or this default, never an invented per-run guess.
    /// See [`ServerConfig::with_default_environment_tag`].
    pub default_environment_tag: Option<EnvironmentTag>,

    /// Operator-authored Cedar policy files (R0.9 Rusty Capsules, wave
    /// 2), loaded for the `default` tenant at startup — the deployment's
    /// standing authorization, the way static API keys are its standing
    /// authentication. Requires the `capsules` feature; each file is
    /// registered under its content-derived version
    /// (`cedar-{sha256[..12]}`), never activated — activation is an
    /// explicit operator move through `POST /capsule_policies/active`. See
    /// [`ServerConfig::with_capsule_policy_file`].
    pub capsule_policy_files: Vec<PathBuf>,

    /// The tenant-wide budget ceiling (R0.9 wave 2): the outermost bound
    /// every capsule budget composes under. Fuel, memory, wall time, and
    /// output bytes clamp down to it; a run declaring more `max_tokens`
    /// or `max_cost_usd` than the ceiling (or than the run's own budget)
    /// is refused `422` — token and cost bounds cannot be retrofitted
    /// mid-run the way fuel can, so admission refuses the overspend. See
    /// [`ServerConfig::with_capsule_budget_ceiling`].
    pub capsule_budget_ceiling: Option<ResourceBudget>,

    /// The deployment's network-egress seam for capsules the A2A bridge
    /// executes in-process (R0.9 wave 4): the [`NetworkConnector`] every
    /// bridge-built capsule host is constructed with. Without one, A2A
    /// tasks carrying a capsule payload fail closed with
    /// `capsule_execution_unavailable` — the bridge never executes guest
    /// code against an egress path the operator did not explicitly wire.
    /// Requires the `capsules` feature. See
    /// [`ServerConfig::with_capsule_connector`].
    #[cfg(feature = "capsules")]
    pub capsule_connector: Option<Arc<dyn rusty_agent_runtime::capsule_host::NetworkConnector>>,

    /// The credential handle TTL (R0.11 Extension Plane, wave 3): how
    /// long one issued handle stays valid, default 300 seconds — "handles
    /// live for minutes": short enough that expiry is routine, long
    /// enough that a run's burst of provider calls reuses one issuance.
    /// Revocation never waits on this — resolution reads the connection's
    /// live state, so a revoked connection fails at the next call
    /// regardless. See [`ServerConfig::with_broker_handle_ttl`].
    pub broker_handle_ttl: std::time::Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], 8080)),
            store_path: PathBuf::from("./data/checkpoints"),
            database_url: None,
            max_concurrent_runs_per_thread: 1,
            api_key: None,
            api_keys: Vec::new(),
            event_log_capacity: 1000,
            outbox_relay_interval: crate::outbox::DEFAULT_RELAY_INTERVAL,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            task_pool_limits: HashMap::new(),
            task_quota: TaskQuota::default(),
            tenant_task_quotas: HashMap::new(),
            promotion_envelope: PromotionEnvelope::r08_default(),
            candidate_evaluator: None,
            default_environment_tag: None,
            capsule_policy_files: Vec::new(),
            capsule_budget_ceiling: None,
            #[cfg(feature = "capsules")]
            capsule_connector: None,
            broker_handle_ttl: broker::DEFAULT_HANDLE_TTL,
        }
    }
}

impl ServerConfig {
    /// A config with the given bind address and checkpoint store root;
    /// everything else at its default.
    pub fn new(bind_addr: SocketAddr, store_path: impl Into<PathBuf>) -> Self {
        Self {
            bind_addr,
            store_path: store_path.into(),
            ..Self::default()
        }
    }

    /// Builder-style: require an API key on every request. The key maps to
    /// the `default` tenant (legacy single-tenant mode).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Builder-style: map an API key to a tenant (multi-tenant mode). Every
    /// request presenting `key` via `X-Api-Key` runs as `tenant`, fully
    /// isolated from all other tenants. Tenant ids must match
    /// `[A-Za-z0-9._-]` (1–64 chars) and must not be a reserved layout name
    /// (`assistants`, `crons`, `store`, `threads`, `latest`) — they become a
    /// path segment in the JSON-file layout and a `{tenant}/` id prefix
    /// everywhere else.
    ///
    /// # Panics
    ///
    /// Panics on an empty key or an invalid tenant id (configuration is a
    /// programmer error, caught at startup).
    pub fn with_tenant_key(mut self, tenant: impl Into<String>, key: impl Into<String>) -> Self {
        let tenant = tenant.into();
        let key = key.into();
        let valid = !tenant.is_empty()
            && tenant.len() <= 64
            && tenant
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !RESERVED_NAMES.contains(&tenant.as_str());
        assert!(
            valid,
            "invalid tenant id `{tenant}` (allowed: [A-Za-z0-9._-], 1..=64 chars, not a reserved name)"
        );
        assert!(
            !key.is_empty(),
            "API key for tenant `{tenant}` must not be empty"
        );
        self.api_keys.push((key, tenant));
        self
    }

    /// `true` when at least one API key is configured (legacy `api_key` or
    /// any tenant key), i.e. requests must authenticate.
    pub fn auth_enabled(&self) -> bool {
        self.api_key.is_some() || !self.api_keys.is_empty()
    }

    /// The tenant a presented API key maps to, or `None` for unknown keys.
    /// Tenant keys are checked first (last registration wins on duplicate
    /// keys); the legacy `api_key` maps to the `default` tenant.
    pub fn tenant_for_key(&self, key: &str) -> Option<&str> {
        if let Some((_, tenant)) = self.api_keys.iter().rev().find(|(k, _)| k == key) {
            return Some(tenant.as_str());
        }
        if self.api_key.as_deref() == Some(key) {
            return Some("default");
        }
        None
    }

    /// Builder-style: persist everything in Postgres at `url` (e.g.
    /// `postgres://user:pass@localhost/rusty`). Switches the run
    /// checkpointer to [`rusty_agent_runtime::checkpoint_postgres::PostgresCheckpointer`]
    /// **and** the assistants/crons/threads/KV/journals server store to the
    /// `server_*` tables. Schemas auto-migrate on (lazy) connect.
    #[cfg(feature = "postgres")]
    pub fn with_postgres(mut self, url: impl Into<String>) -> Self {
        self.database_url = Some(url.into());
        self
    }

    /// Builder-style: set the per-thread enqueue queue depth cap. Values
    /// below 1 are clamped to 1 (a zero-deep queue would reject every
    /// `enqueue` run).
    pub fn with_max_concurrent_runs_per_thread(mut self, cap: usize) -> Self {
        self.max_concurrent_runs_per_thread = cap;
        self
    }

    /// Builder-style: set the per-run SSE event-log capacity. Values below
    /// 16 are clamped to 16 (replay needs room for at least the
    /// metadata/updates/end frames of a minimal run).
    pub fn with_event_log_capacity(mut self, capacity: usize) -> Self {
        self.event_log_capacity = capacity;
        self
    }

    /// Builder-style: set the transactional-outbox relay's poll interval
    /// (R0.6 wave 2b). Tests can set a long interval to drive publishing
    /// deterministically; correctness never depends on the interval.
    pub fn with_outbox_relay_interval(mut self, interval: std::time::Duration) -> Self {
        self.outbox_relay_interval = interval;
        self
    }

    /// Builder-style: set the graceful-shutdown bound (R0.6 wave 2c; see
    /// [`DEFAULT_SHUTDOWN_GRACE`] for the default's rationale). Past this
    /// window the server stops even if requests or runs are still in flight
    /// — durability (checkpoint resume, lease expiry) is the backstop.
    pub fn with_shutdown_grace(mut self, grace: std::time::Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// Builder-style: cap the named pool's live leases (R0.6 wave 3). The
    /// claim path counts a pool's unexpired leases and stops handing out
    /// new ones at `max_in_flight`, so a saturated pool (GPU-bound work)
    /// never starves the others (IO-bound work). A cap of `0` pauses the
    /// pool: nothing is leased from it until the cap is raised. Pools
    /// without an entry stay uncapped.
    ///
    /// # Panics
    ///
    /// Panics on an invalid pool name (configuration is a programmer error,
    /// caught at startup); names must match `[A-Za-z0-9._-]` (1–128 chars),
    /// the same rule the enqueue and claim paths validate.
    pub fn with_pool_limit(mut self, pool: impl Into<String>, max_in_flight: usize) -> Self {
        let pool = pool.into();
        assert!(
            crate::tasks::validate_pool(&pool).is_ok(),
            "invalid pool name `{pool}` (allowed: [A-Za-z0-9._-], 1..=128 chars)"
        );
        self.task_pool_limits.insert(pool, max_in_flight);
        self
    }

    /// Builder-style: set the default tenant quota for the durable task
    /// queue (R0.6 wave 3), applied to every tenant without a
    /// [`ServerConfig::with_tenant_quota`] override. Over quota, task
    /// submissions answer `429 quota_exceeded`. The default is uncapped on
    /// every gauge. See [`TaskQuota`] for the gauge definitions.
    pub fn with_task_quota(mut self, quota: TaskQuota) -> Self {
        self.task_quota = quota;
        self
    }

    /// Builder-style: override the task quota for one tenant (R0.6 wave 3).
    /// The override replaces [`ServerConfig::task_quota`] wholesale for
    /// that tenant — an uncapped gauge in the override is unlimited even if
    /// the default caps it.
    ///
    /// # Panics
    ///
    /// Panics on an invalid tenant id (same rule as
    /// [`ServerConfig::with_tenant_key`]).
    pub fn with_tenant_quota(mut self, tenant: impl Into<String>, quota: TaskQuota) -> Self {
        let tenant = tenant.into();
        let valid = !tenant.is_empty()
            && tenant.len() <= 64
            && tenant
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !RESERVED_NAMES.contains(&tenant.as_str());
        assert!(
            valid,
            "invalid tenant id `{tenant}` (allowed: [A-Za-z0-9._-], 1..=64 chars, not a reserved name)"
        );
        self.tenant_task_quotas.insert(tenant, quota);
        self
    }

    /// The quota governing `tenant`: its override when one is configured,
    /// else the server-wide default ([`ServerConfig::task_quota`]).
    pub(crate) fn quota_for(&self, tenant: &str) -> &TaskQuota {
        self.tenant_task_quotas
            .get(tenant)
            .unwrap_or(&self.task_quota)
    }

    /// Builder-style: set the promotion envelope (R0.8 Rusty Learn, wave
    /// 3) — the declared, per-deployment standing approval the promotion
    /// gate evaluates every promotion against. Replaces
    /// [`PromotionEnvelope::r08_default`].
    ///
    /// # Panics
    ///
    /// Panics when the envelope fails [`PromotionEnvelope::validate`]
    /// (a canary fraction outside `(0, 1]`, a negative improvement bar —
    /// configuration is a programmer error, caught at startup).
    pub fn with_promotion_envelope(mut self, envelope: PromotionEnvelope) -> Self {
        if let Err(error) = envelope.validate() {
            panic!("invalid promotion envelope: {error}");
        }
        self.promotion_envelope = envelope;
        self
    }

    /// Builder-style: register the candidate evaluator (R0.8 wave 3) —
    /// the evaluation composition `POST /learn/candidates/{id}/evaluate`
    /// drives. Without one the route answers `409`: promotion is gated
    /// on evidence, and evidence requires an evaluator.
    pub fn with_candidate_evaluator(mut self, evaluator: Arc<dyn CandidateEvaluator>) -> Self {
        self.candidate_evaluator = Some(evaluator);
        self
    }

    /// Builder-style: declare the deployment's default environment tag
    /// (R0.11 wave 2) — the promotion target a registry-bound run
    /// resolves against when its binding names no environment. `None`
    /// (the default) resolves the untagged surface. The tag is validated
    /// at construction (the same rules the wire applies), so no
    /// deployment can declare a default its own pointers could never
    /// carry.
    pub fn with_default_environment_tag(mut self, tag: impl Into<String>) -> Self {
        self.default_environment_tag = Some(
            EnvironmentTag::new(tag.into())
                .unwrap_or_else(|e| panic!("invalid default environment tag: {e}")),
        );
        self
    }

    /// Builder-style: add one operator-authored Cedar policy file (R0.9
    /// Rusty Capsules, wave 2), loaded for the `default` tenant at
    /// startup. Files are *registered*, never activated — moving a
    /// tenant's active pointer is an explicit operator move through
    /// `POST /capsule_policies/active`, so a restart can never silently
    /// change what serves. Without the `capsules` feature the files are
    /// ignored (the plane's routes answer `503` either way).
    pub fn with_capsule_policy_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.capsule_policy_files.push(path.into());
        self
    }

    /// Builder-style: set the tenant-wide capsule budget ceiling (R0.9
    /// wave 2) — the outermost bound every capsule budget composes
    /// under. `None` (the default) leaves budgets bounded only by the
    /// run's own declaration and each capsule's manifest.
    pub fn with_capsule_budget_ceiling(mut self, ceiling: ResourceBudget) -> Self {
        self.capsule_budget_ceiling = Some(ceiling);
        self
    }

    /// Builder-style: set the network connector for capsules the A2A
    /// bridge executes in-process (R0.9 wave 4) — the deployment's
    /// explicit egress seam. `None` (the default) refuses capsule
    /// execution over A2A with `capsule_execution_unavailable`: guest
    /// code never runs against an egress path the operator did not wire.
    #[cfg(feature = "capsules")]
    pub fn with_capsule_connector(
        mut self,
        connector: Arc<dyn rusty_agent_runtime::capsule_host::NetworkConnector>,
    ) -> Self {
        self.capsule_connector = Some(connector);
        self
    }

    /// Builder-style: set the credential handle TTL (R0.11 wave 3). The
    /// TTL bounds how long one issuance is reused, not how long a
    /// revocation takes to bite — resolution reads live state, so a
    /// revoked connection fails at the very next call.
    pub fn with_broker_handle_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.broker_handle_ttl = ttl;
        self
    }
}

/// Build the axum [`Router`] for a registry and config. Use this to embed the
/// rusty-agent-server routes into a larger application, or to drive the API in tests
/// via `tower::ServiceExt::oneshot`.
///
/// The router's drain control is internal and never fires (embedders who
/// want cooperative drain should use [`router_with_shutdown`]).
pub fn router(registry: GraphRegistry, config: ServerConfig) -> Router {
    router_with_shutdown(registry, config, CancellationToken::new())
}

/// Build the axum [`Router`] with an explicit drain control (R0.6 wave 2c).
///
/// Cancelling `shutdown` starts the cooperative drain **inside** the
/// application — in-flight runs stop at their next checkpoint boundary, the
/// outbox relay finishes its current pass and stops, the cron scheduler
/// stops firing, and new run submissions are rejected with `503` — while
/// the HTTP surface keeps serving so in-flight requests can complete.
/// Stopping HTTP itself is the embedder's job (axum's
/// `with_graceful_shutdown`); [`serve_with_shutdown`] wires both halves
/// together.
pub fn router_with_shutdown(
    registry: GraphRegistry,
    config: ServerConfig,
    shutdown: CancellationToken,
) -> Router {
    routes::router_with_shutdown(registry, config, shutdown)
}

/// Build the router and bind it to `config.bind_addr`. Blocks until the
/// server shuts down: SIGINT or SIGTERM starts the graceful drain described
/// on [`serve_with_shutdown`].
pub async fn serve(registry: GraphRegistry, config: ServerConfig) -> std::io::Result<()> {
    serve_with_shutdown(registry, config, shutdown_signal()).await
}

/// Build the router, bind it to `config.bind_addr`, and serve until
/// `shutdown` resolves, then drain gracefully (R0.6 wave 2c). The drain is
/// ordered so the common case of a rolling deploy strands nothing:
///
/// 1. **Stop taking new work.** axum stops accepting connections; the
///    server's shared drain token fires, so new run submissions answer
///    `503` and the cron scheduler stops firing.
/// 2. **Let in-flight work land.** In-flight requests complete (axum waits
///    for them); in-flight runs are cooperatively cancelled at their next
///    super-step boundary — where a checkpoint was just persisted — and
///    end terminal-`cancelled`, resumable by re-running the thread; the
///    outbox relay finishes its current publish pass and stops (pending
///    rows are durable and publish on the next process's first pass).
/// 3. **Stop, bounded.** The whole drain is capped at
///    [`ServerConfig::shutdown_grace`]. Past it the server returns anyway:
///    anything still in flight is abandoned mid-step, which is exactly the
///    crash case the durable machinery already covers — runs resume from
///    their last checkpoint and leased tasks return to visibility within
///    one lease period. Grace makes the common case fast; it is never the
///    correctness mechanism.
pub async fn serve_with_shutdown(
    registry: GraphRegistry,
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let addr = config.bind_addr;
    let grace = config.shutdown_grace;
    // Open (dev) mode on a non-loopback address exposes the full API — run
    // creation, KV writes, checkpoint deletion — to the network. That's a
    // legitimate dev choice, but it must never be a quiet one.
    if !config.auth_enabled() && !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "serving WITHOUT authentication on a non-loopback address; \
             configure `with_api_key`/`with_tenant_key` or bind 127.0.0.1"
        );
    }
    let draining = CancellationToken::new();
    let app = router_with_shutdown(registry, config, draining.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "rusty-server listening");
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let draining = draining.clone();
        async move {
            shutdown.await;
            tracing::info!(
                grace_ms = grace.as_millis() as u64,
                "shutdown requested; draining"
            );
            // One token drives the whole cooperative drain: runs stop at
            // their next checkpoint boundary, the relay finishes its pass,
            // the cron scheduler stops, new runs are rejected.
            draining.cancel();
        }
    });
    tokio::select! {
        // `WithGracefulShutdown` is `IntoFuture`, not `Future`, so it needs
        // this async wrapper to sit in a `select!` arm.
        result = async { server.await } => result,
        () = async {
            // The grace clock starts when the drain does. axum's graceful
            // shutdown waits for in-flight connections without a bound of
            // its own, so this arm is the bound.
            draining.cancelled().await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!(
                grace_ms = grace.as_millis() as u64,
                "drain grace expired; forcing shutdown — in-flight runs resume \
                 from their last checkpoint and leased tasks return at lease expiry"
            );
            Ok(())
        }
    }
}

/// The default shutdown signal for [`serve`]: resolves on SIGINT or SIGTERM
/// (the two signals orchestrators and terminals actually send). Exposed so
/// binaries that compose their own shutdown logic can reuse it.
pub async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        // Signal-handler installation only fails when the fd limit is
        // exhausted; a server that cannot hear SIGTERM must not start
        // pretending it drains gracefully.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing the SIGTERM handler must succeed");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
