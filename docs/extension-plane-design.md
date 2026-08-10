# Rusty Extension Plane design (R0.11)

The Extension Plane release closes the two P0 gaps the 2026-08-09 competitive review left
open: a **prompt/configuration registry** and a **credential/connection broker**. The
governing claim, stated precisely: **every configuration that can influence a run becomes a
versioned, content-addressed, promotable artifact — committed, diffed, owned, tagged to an
environment, promoted and rolled back by immutable version pointer, pinned into the run
manifest, and journaled at every transition — and no tool ever holds a raw credential:
credentials live in one broker, and tools receive short-lived, opaque capability handles,
non-serializable and scope-checked at use.** Both halves close their gap by composing
governance machinery that already exists rather than inventing new trust models: the
registry is the R0.8 candidate pipeline turned toward human-authored configuration; the
broker is the R0.9 deny-by-default capability model extended from capsule guests to every
tool the runtime drives.

The release has three parts, in dependency order: the **registry** (the larger surface),
the **broker** (the sharper trust change), and **middleware composition** (the shipped
interceptor SDK gains registry-aware ordering and governed configuration, so interception
policy versions like everything else). What R0.11 adds on top of the composed machinery is
named per component below — the registry surface, environment tags, diffs, webhooks, the
connection store, and the handle lifecycle — and the not-built list is equally explicit.

## Why this belongs in the runtime

Configuration managed at framework level — a prompt string literal in application code, a
`.env` file, a YAML edit and a redeploy — loses the same three things framework-level
memory lost before R0.8 and framework-level adaptation lost before R0.10. **Evidence** is
absent: "which prompt produced this action" is unanswerable, because the run manifest pins
a SHA-256 digest (`rusty-core/src/record.rs`, `RunManifest::pin_prompt`) and nothing in the
system can resolve that digest to a version, an author, or an approval. **Governance** is a
convention: a config edit ships because someone edited it — no candidate, no evaluation,
no rollback but another edit and another deploy. **Blast radius** is invisible: a changed
tool schema or model parameter reaches every run at once, attributed to a deploy, never to
a decision.

Credentials at framework level fail harder. A tool holding a raw API key has ambient
authority the runtime cannot see: the key is in process memory, in every crash dump, in
every accidental log line, scoped to nothing, revocable only by redeploying. The design
principles have said **capabilities over trust** since R0.5, and R0.9 made it structural
for capsule guests — secret grants hand the guest an opaque handle and the host resolves
handle → secret at the moment of use, so the bytes never enter guest linear memory
(`rusty-core/src/capsule.rs`, `docs/capsules-design.md`). Native tools deserve the same
discipline, and the review ranked it P0.

There is also a two-sided argument unique to this release. R0.7 shipped manifest *pinning*
without a registry: the receipt can name the hash of the prompt a run used, and no
component can say what that hash is a hash *of* — pinning without a registry is a receipt
that resolves nothing. R0.9 shipped secret *handles* without a broker: capsule grants name
"names in the server's secret store," and the server's only secret store today is the
receipt-key directory (`rusty-server/src/receipts.rs`) — handles without a broker are
tokens naming a store that does not exist. R0.11 builds the two resolvers the shipped
contracts already assume.

## The prompt/configuration registry

### Artifact families

The roadmap names prompts, policies, tool contracts, model settings, and memory
configurations; middleware composition joins them below. Each family maps onto the R0.8
candidate machinery or extends it additively:

| Registry family | Candidate kind | Run-manifest pin |
|---|---|---|
| Prompt | `CandidateKind::Prompt` — exists (R0.8) | `manifest.prompts[name]`, SHA-256 of the exact text |
| Executor policy | `CandidateKind::Policy` — exists (R0.8/R0.10) | `policy_version` in the checkpoint header |
| Memory set | `CandidateKind::MemorySet` — exists (R0.8) | scoped memory content, journaled reads |
| Tool permission | `CandidateKind::ToolPermission` — exists (R0.8) | grant direction; mechanics are R0.9 capsule manifests |
| Tool contract | **new kind** — the JSON schema a tool's pin digests | `manifest.tool_schemas[name]`, canonical-JSON digest |
| Model settings | **new kind** — model id plus parameters | `manifest.model` + `manifest.model_params` digest |
| Memory configuration | **new kind** — retrieval/assembly settings (`ContextBudget`, default filters, schema version) | `manifest.memory_schema` |
| Middleware composition | **new kind** — ordered layers plus per-layer configuration | **new additive manifest field** (see middleware below) |

The reused half is load-bearing. `CandidateContent::Prompt`'s text digest is exactly the
pin `RunManifest::pin_prompt` records — "the candidate and the run manifest speak one
content address" (`rusty-core/src/learn.rs`). The new kinds are the established
contract-evolution rule, not a redesign: additive variants on the closed
`CandidateKind`/`CandidateContent` enums, golden-pinned wire shapes, old records keep
deserializing — the same rule R0.10 applied when it weighed a `Speculation` family and
deferred it. One honest boundary on "memory configuration": R0.8's `MemorySet` carries
*records* at a scope; configuration means the retrieval and assembly settings that shape
what a run's memory reads return. Conflating the two would fake coverage, so the family is
a new kind and says so.

### What is reused, what is new

Reused unchanged — the point of the release:

- **The candidate contract** (`rusty-core/src/learn.rs`): immutable, content-addressed
  candidates; `derive_candidate_id` / `verify_address` (identity is integrity); the
  lifecycle status machine (`created → evaluated → promoted`, plus rollback);
  `EvidenceSpan` for authored provenance.
- **The promotion gate**: `PromotionEnvelope` / `EnvelopeRule` (`Auto` / `Approval` /
  `Canary`), `admit_promotion` as a pure function, refusal as a typed error, and the
  out-of-envelope `ApprovalToken` scoped to `promotion_effect_id` — derived over the
  candidate's content hash, so an approval for one version is non-transferable to another.
- **The version pointer**: `VersionPointer { surface, active, canary }` with byte-exact
  rollback — the restored version is the candidate that previously served, not a
  reconstruction — and canary binding by seeded draw (`canary_admits`), so a recorded run
  reproduces its assignment.
- **The journaled lifecycle**: `candidate_created` / `candidate_evaluated` /
  `candidate_promoted` / `candidate_rolled_back` with causal parentage, hard-fail
  journaling on this surface (nothing reaches the store the journal did not record first).
- **The storage discipline** (`rusty-server/src/learn.rs`): one JSON file per record under
  `{store_path}/learn/`, hash-named pointer files with key-carrying envelopes, atomic
  temp-write-then-rename, and the column-mapped Postgres tables with the status flip and
  pointer move in one transaction. Tenant isolation is the v0.5 `{tenant}/` id-prefixing
  unchanged — 404, never 403.
- **The evaluation seam**: `CandidateEvaluator` as a trait (the workspace's dependency
  direction forced it: `rusty-eval` links the runtime, never the reverse), with
  `EvaluationVerdict` / `EvaluationThresholds` mirroring `compare()`'s output.

Genuinely new — nothing below exists in the codebase today:

1. **The registry surface itself.** Named artifacts with an owner and a commit history.
   A commit is a candidate: content-addressed, immutable, authored (`ProvenanceAuthor` —
   the pipeline was built for distiller output; registry commits are operator-authored,
   and `human:{id}` attribution is the correction loop's discipline applied to
   configuration). The artifact record — name, family, owner, commit sequence — is the
   only new persisted entity; it is an index over candidates, never a fork of them.
   Ownership is review routing and attribution, not an ACL: tenant isolation remains API
   keys, and fine-grained RBAC stays post-R1.0.
2. **Environment tags on pointers.** The surface key carries the tag —
   `prompt:system@prod` versus `prompt:system@staging` — so the pointer store, hash-named
   files, transactional moves, and canary slots work unchanged. The tag set is
   deployment-declared (`dev` / `staging` / `prod` by convention, not by enum). What a tag
   is *not*: not a deployment, not an isolated store, not a trust boundary. R0.12 builds
   environments as a control plane; R0.11's tags let one deployment serve "the prod
   prompt" and "the staging prompt" from one registry with different envelope strictness
   per tag — dev commits may auto-promote, prod commits require approval.
3. **Diffs.** A derived view between two candidate ids of one artifact: a line diff for
   text families, a structural diff over the `canonicalize_value` form for JSON families
   (added/removed/changed leaves). Computed on read, never stored — the store holds
   immutable versions, and a stored diff would be a second, divergent account of the same
   change. The canonical form exists precisely so equal content hashes equal
   (`rusty-core/src/record.rs`); diffing over it inherits that determinism.
4. **Change webhooks.** Outbound notifications on registry lifecycle transitions, per
   artifact and per environment tag. Delivery is durable work: the transition and its
   notification intent commit together (the R0.6 outbox rule — a state transition and the
   evidence that caused it must not split-brain), then a leased, retried task delivers,
   classified under `ErrorClass`, dead-lettered with evidence on exhaustion. Payloads are
   HMAC-signed with a per-subscription secret — the inbound-trigger precedent
   (`rusty-server/src/triggers.rs` verifies per-trigger secrets) applied in the outbound
   direction. Deliveries journal; subscriptions are tenant-scoped configuration.
5. **Admission-time resolution.** The mechanism that binds registry versions into runs —
   designed under "pinning into the run" below.

### Pinning into the run

A run declares its target environment at submission (absent means the deployment's default
tag — declared configuration, never an invented per-run guess). At admission, each named
artifact the run uses resolves through the environment-tagged `VersionPointer` to a
candidate — the active version, or the canary when the seeded draw admits — and the
resolved content is what the manifest pins. Two mechanisms make the binding evidence:

1. **The manifest pin, unchanged.** `RunManifest::pin_prompt`, `pin_tool_schema`,
   `pin_model` record content digests exactly as they have since R0.7. For
   registry-resolved artifacts the digest *is* derivable from the candidate — the wire
   shape stays byte-frozen, and pre-R0.11 manifests keep deserializing. In-flight runs
   keep the versions their checkpoint headers pin; promotions bind new runs at admission —
   the conservatism every release since R0.7 has kept.
2. **A journaled resolution event.** One additive `RunEventKind` — the `CapsuleResolved`
   precedent (`rusty-core/src/capsule.rs`) applied to configuration — naming, per
   artifact: the artifact name, the environment tag, the candidate id, the pointer state
   (active or canary-admitted), and the digest pinned. The resolution is the digest ↔
   version join the manifest alone cannot express.

A deliberate deviation, stated plainly: the manifest pins digests, not candidate ids, and
this release does not add candidate ids to it — the R0.7 wire shape stays frozen, and the
join record is the journaled event instead. The one exception is middleware composition,
which has no digest slot at all and needs one additive optional field (below). The R0.9
receipt already signs the manifest digest read back from the run's last checkpoint header
(`docs/capsules-design.md`, wave 3), and the resolution event sits in the
signature-covered journal — so the audit walk is: signed receipt → manifest pin →
resolution event → candidate → commit author, evaluation, envelope version, approver.
"Which prompt produced this action" stays answerable from the signed receipt, which is the
roadmap's sentence made a path of ids.

## The credential/connection broker

### The connection model

One entity, the `Connection` — a named, tenant-scoped record: provider kind (closed enum,
additive-evolution: `oauth2_authorization_code`, `oauth2_client_credentials`, `api_key`,
`basic`); subject (the per-user binding — a user id within the tenant; absent for
service-level connections); the consent scope set (provider-semantics strings, recorded
when the human consents); token material (access token, refresh token where the provider
issues one, expiry); status (`active` / `needs_reauth` / `revoked`); and health
(`last_refresh_at`, the last failure classified under `ErrorClass`, consecutive failures).
The consent scope set is the ceiling: the OAuth consent happens with the provider, the
broker records what was granted, and everything downstream may only narrow — the
`CapsuleOverlay` stance (`intersect_grants` computes intersection; no code path computes
union) applied to credentials. A tool requesting beyond the consented set is denied at
handle issuance, not at the provider.

### Storage: encrypted at rest on both backends

Envelope encryption. Each connection's token material is encrypted under a per-connection
data key; data keys are wrapped by a deployment master key. The store — one file per
connection under `{store_path}/connections/` on the JSON backend, a column-mapped
`server_connections` table on Postgres — holds ciphertext and the wrapped data key only.
Both backends follow the established conventions: atomic temp-write-then-rename on files,
auto-migration under the transaction-scoped advisory lock on Postgres, tenant
id-prefixing, 404 never 403. The master key lives outside the store abstraction exactly as
receipt signing secrets do (`rusty-server/src/receipts.rs`: `{store_path}/keys/`, hex,
`0600` from the first byte, written once, journaled rotation), so the Postgres backend
cannot hold what a database must not leak.

A named deviation from the strongest reading of the R0.9 precedent: receipts drew the line
at "secrets never enter the store abstraction," and the broker stores *ciphertext* in the
abstraction, because connections are numerous, refreshed by background work, and queried
by id in ways a secrets directory serves badly. The precedent's principle survives
unchanged: a store leak must not leak credentials, and ciphertext without the host-local
master key is not a credential. Plaintext enters the store on neither backend, ever.

### Handles: issue, refresh, revoke, expire

A tool never sees a token. The handle lifecycle:

- **Issue.** A tool (or capsule, or MCP/A2A client call) that declares a credential need
  receives a `CredentialHandle` at admission or first use: an opaque random token bound to
  the connection id, a *narrowed* scope set, the tenant, the run, and a short TTL.
  Non-serializable in the R0.9 sense — redacted in `Debug`, no `Serialize` impl, carrying
  no bytes — the capsule secret handle generalized from guest linear memory to all tool
  code.
- **Resolve at use.** The call presents the handle; the broker checks live state
  (connection active, handle unexpired, scope covers the requested operation), refreshes
  the access token when it sits inside a declared expiry window, and injects the
  credential into the outbound request inside the host-side connector — "the bytes never
  enter guest linear memory" becomes "the bytes never enter tool code." Resolutions
  journal metadata — handle, connection, scopes checked — never bytes (the `CapsuleUse`
  precedent); denials journal the missing scope or absent grant (the `CapsuleDenied`
  precedent: attributable to a declaration, not a stack trace).
- **Refresh.** Automatic, in two places: at resolution when the token is inside the
  expiry window (jittered, one bounded attempt in the call's path), and a durable sweeper
  task for connections approaching expiry with no traffic — refresh is durable work
  (leased, retried under `ErrorClass`, journaled), because a credential the sweeper let
  lapse is a production incident with a quiet signature.
- **Revoke.** Immediate. The status flip and its journaled event commit together;
  outstanding handles fail at their next use. Scope widening is a new consent — a human
  act at the provider, recorded and journaled; there is no API path that widens a
  connection's consented set.
- **Expire.** Handles are short-lived and are never pinned anywhere. What the run's
  evidence pins — through the resolution event — is the *connection id and the consent
  scope set* the run resolved at admission. Credential rotation beneath a stable
  connection id changes nothing the run pinned, which is exactly how "rotate a credential
  without redeploying" composes with exact version pinning: the pin names the governed
  relationship, not the secret of the moment.

### Failure modes — everything fails closed

- **Revoked connection**: the next tool call is denied with a typed, journaled refusal
  naming the connection and the revoked grant. This is the release proof's third clause,
  and it is structural: resolution reads live connection state, so revocation takes effect
  at the next call, not the next deploy.
- **Expired refresh token / provider `invalid_grant`**: status flips to `needs_reauth`;
  calls fail closed with a typed re-auth signal — never silent retries with stale
  material, because a stale credential retried looks exactly like an attack retried.
- **Broker unreachable at resolution**: the call fails closed. A scope check that cannot
  be performed is a check that fails; there is no degraded mode that skips it.
- **Provider outage during a call**: ordinary failure — classified under `ErrorClass`,
  fed to the retry taxonomy and the R0.10 learned policies unchanged.

### The honest edge

The broker mediates Rusty-side tool calls — calls made through the runtime's
`ToolExecutor`, the capsule host's connectors, and the journaled MCP/A2A clients. A
credential obtained by code already inside a capsule through a different path — compiled
into its build, smuggled through a granted input — is outside the broker's reach.
Deny-by-default narrows that surface (no filesystem or network without grants, and a
`secret` grant is the only declared path to broker material), but the capsule model's own
boundary — the grant set is the whole reach — is the broker's boundary too. Stated here,
not smoothed over: the broker removes ambient credential authority from tools; it does not
make exfiltration by already-trusted code impossible, and no component in this release
pretends otherwise.

## Middleware composition

The shipped interceptor SDK (`rusty-core/src/middleware.rs`) composes layers into a
`MiddlewareChain` with onion semantics — before-hooks in registration order, after-hooks
in reverse, rejections terminal through the crate's error taxonomy. Ordering today is
registration order in builder code: interception policy is compiled in, and changing it
means a redeploy. R0.11 makes the composition itself a registry artifact — the
`middleware_composition` family: an ordered layer list plus per-layer configuration (a
`ToolCallBlocklist`'s policy is configuration; the layer's code is not). The chain's
semantics are untouched — governance wraps composition, not hook behavior.

At admission the run resolves the composition artifact for its environment tag, layers
instantiate in the declared order, and the composition's digest pins into the manifest.
That pin is the release's one additive manifest field — an optional `middleware` digest,
`skip_serializing_if`-absent when unset, per the sparse-wire rule every manifest field
follows — because no existing slot covers it, and the deviation is smaller than leaving
interception policy unpinned. A blocklist edit then moves like everything else: commit,
diff, evaluation where the envelope declares it, environment-tagged promotion, byte-exact
rollback. Ordering is registry-aware in both directions: the artifact declares the order,
and the resolved order is journaled evidence.

Two honest edges. Layers are code: the registry versions their composition and
configuration, not their behavior — a layer's logic change is a deploy, and the
checkpoint header's `graph_hash` story already covers code identity. And chains built
partly in code keep working: an absent composition artifact is today's behavior,
byte-stable, but its ordering is unpinned — "absent means unpinned, never a default," the
manifest's own rule, applied to middleware rather than around it.

## Governance wiring

- **Envelopes, per family per environment tag.** `PromotionEnvelope` grows additive
  fields for the new kinds — serde defaults keep R0.8-era envelopes deserializing, the
  established evolution rule. The `r08_default` stance extends: behavior-bearing kinds are
  `Approval` at prod; dev-tagged surfaces may `Auto` with a clean verdict; staging may
  `Canary` through the seeded-draw machinery that exists. `tool_permission` stays
  `Approval` at every tag — the R0.8 default already holds it there, and a widened grant
  is exactly the change a human must own.
- **Evaluation, honestly gated.** The `CandidateEvaluator` seam composes unchanged. For
  prompts and model settings the gate is the R0.8 composition — replay plus a `rusty-eval`
  experiment and a `compare()` verdict over a named dataset version. For tool contracts
  and middleware compositions, semantic evaluation is not always meaningful: a schema
  tightening or an ordering change is a contract judgment, not a metric, and the envelope
  may declare approval-only. A fake metric is worse than none, so the gate says which it
  is, per family, per tag.
- **Approvals compose the effect kernel, twice.** Out-of-envelope promotions require the
  scoped `ApprovalToken` — derived over the candidate's content hash, non-transferable,
  journaled on `CandidatePromoted`. On the broker side the approval primitive is consent:
  the human's OAuth grant is the approval, executed at the provider, recorded in the
  broker, journaled — and handle issuance can only narrow against it. No parallel approval
  mechanism on either side; both reuse what the kernel and the provider already have.
- **Receipts and journals close the loop.** Registry resolutions and transitions journal
  with the envelope version in force; the signed receipt covers the manifest digests and
  the journaled resolution chain, so the audit walk in "pinning into the run" is
  signature-covered end to end. Broker control-plane events — connection registered,
  consented, refreshed, revoked, handle issued or denied — journal into the deployment's
  evidence chain under the receipt-keys precedent: control-plane transitions get a
  journaled identity (`receipt-keys` was the first), so the broker's own history is
  auditable with the same machinery as a run's.
- **Tenant isolation is unchanged everywhere.** Artifacts, pointers, connections,
  subscriptions, and handles are tenant-namespaced; cross-tenant access answers 404.
  Nothing in this release creates a cross-tenant read path — a shared prompt catalog
  would be a tenant-isolation breach wearing a registry costume.

## What R0.11 deliberately does NOT build

- **Deployment environments.** Dev/staging/prod as isolated deployments with immutable
  revisions, canary deploys, and environment-scoped secrets is R0.12's control plane.
  R0.11's tags are labels on promotion targets — deliberately, so the registry lands
  without waiting for the control plane, and the control plane inherits governed artifacts
  instead of raw config.
- **A secrets manager or vault of record.** The broker brokers OAuth connections and
  per-user credentials for tool calls. It is not a general secret KV, not a vault
  replacement, and holds nothing it cannot re-derive from a provider or re-enroll. Static
  application secrets — database URLs, receipt signing keys — keep their current file
  discipline; they are not connections and gain nothing from brokerage.
- **SSO and enterprise identity.** Connection subjects are user ids within a tenant;
  OIDC/SAML, SCIM, and RBAC stay post-R1.0 per the roadmap's sequencing.
- **Prompt marketplaces or cross-tenant sharing.** Forbidden by tenant isolation today,
  and on the roadmap's de-prioritized list (a marketplace before signed capsules exist)
  regardless.
- **In-run configuration mutation.** Nothing changes a running run's bound versions.
  Promotions bind at admission; in-flight runs keep their pins. There is no hot-reload
  path, because a hot-reloaded prompt is precisely a silent behavioral rewrite — the thing
  the learning rule has forbidden since R0.8.
- **Semantic prompt search or optimization.** No vector retrieval here either; the R0.8
  de-prioritization stands, and structured registry queries (family, owner, tag, status)
  cover this release's use cases.

## Wave plan and release proof

**Wave 1 — registry contracts and store.** The additive `CandidateKind` /
`CandidateContent` variants (tool contract, model settings, memory configuration,
middleware composition) with golden files; the artifact/commit/owner surface;
environment-tagged surface keys; diff views; both store backends. Exit: two prompt
versions commit and diff; a pointer promotes per tag; all of it survives a restart on both
backends; goldens pin every new wire shape; R0.8 candidate and pointer records keep
deserializing.

> **Wave 1 status: implemented (2026-08-10).** The contracts landed as
> written (`CandidateKind` / `CandidateContent` gained `tool_contract`,
> `model_settings`, `memory_configuration`, `middleware_composition`;
> `ArtifactRecord` / `ArtifactCommit` / `RegistryDiff` in
> `rusty-core/src/registry.rs`, goldens in `rusty-core/tests/golden/`; the
> `/registry/artifacts` routes and the `server_registry_artifacts`
> migration in `rusty-server`), with four settled refinements: artifact
> names reject `/` and `@` (the tenant and tag separators), so memory
> scope ids containing `/` cannot get artifacts in v1; artifact commits
> journal nothing of their own (a commit *is* a candidate — the index
> rides the candidates' own lifecycle evidence); the new envelope fields
> default to `Approval` (the governance leaning); and a structural diff
> reports a one-sided subtree as a single added/removed leaf. Environment
> tags ride the surface key (`prompt:system@prod`) through the unchanged
> pointer machinery, per open question 1's leaning. The middleware
> manifest pin stays with wave 4.

**Wave 2 — admission resolution and pinning.** The journaled resolution event,
environment-tagged binding at admission, the receipt walk. Exit: a prompt version promotes
without redeploying and a new run resolves and pins it; a run started before the promotion
replays against its original pin and reports the exact candidate id it used; rollback
restores the prior version byte-exact; the walk receipt → manifest pin → resolution event
→ candidate → author is asserted as a test, not narrated.

**Wave 3 — the broker core.** Connection records, envelope-encrypted storage on both
backends, handle issue/resolve/revoke/expire, scope checks at use, `ToolExecutor` and
capsule-host connector integration, journaled uses and denials. Exit: a tool authenticates
a provider call through a handle without the bytes entering tool code; both backends hold
ciphertext only (a Postgres dump contains no plaintext credential); a revoked connection
fails closed at the next tool call with a typed, journaled denial.

**Wave 4 — OAuth lifecycle, health, and middleware composition.** Authorization-code and
client-credentials flows, automatic refresh (at-resolution plus the durable sweeper), the
connection health surface, middleware composition artifacts with the additive manifest
pin. Exit: an access token rotates beneath a stable connection id with no redeploy and no
change to anything the run pinned; a `needs_reauth` connection reports on the health
surface and fails closed; a middleware composition promotes to staging while prod's chain
serves unchanged, and both chains' digests are pinned in their runs' manifests.

**Release proof (the whole release).** The roadmap's sentence, automated as an integration
test in the release-proof family: **rotate a credential and promote a prompt version
without redeploying; a replayed run still pins and reports the exact versions it used; a
revoked connection fails closed at the next tool call.** Concretely: a scripted agent
calls a credential-requiring tool through a broker handle and runs to completion; the
credential rotates beneath the stable connection id and a new run succeeds without any
redeploy; a prompt version promotes through the envelope and a new run pins it while the
first run's replay serves its original candidate byte-identically and reports the
candidate id; then the connection is revoked and the very next tool call is refused —
typed, journaled, naming the revoked grant. The test walks the receipt to the prompt
version and asserts the credential bytes never appear in any journal, manifest, or store
row it can read.

## Open questions

Flagged before Wave 1 lands:

1. **Environment tag placement.** Tag inside the surface key (`prompt:system@prod`) versus
   a separate field on the pointer. Leaning: the surface key — the pointer store,
   hash-named files, transactional moves, and canary slots all work unchanged, and R0.12
   can promote environments to first-class records and re-key as a storage migration
   rather than a contract change.
2. **Evaluation requirements per family.** Which kinds may promote at prod on approval
   alone. Leaning: prompts and model settings require a clean `compare()` verdict over a
   named dataset version; tool contracts and middleware compositions may declare
   approval-only envelopes — a schema or ordering change is a contract judgment, and a
   fabricated metric is worse than an honest approval.
3. **Master-key management.** Where the deployment master key lives and how it rotates.
   Leaning: `{store_path}/keys/` under the receipt-secret discipline (`0600` from the
   first byte, outside the store abstraction, journaled rotation), with per-connection
   data keys re-wrapped lazily on rotation; KMS/HSM integration is the R1.0 security
   review's plug point, declared as a seam now so the broker's cryptography is
   key-source-agnostic from the start.
4. **Refresh timing.** Resolution-time refresh versus a background sweeper. Leaning:
   both — refresh at resolution inside a declared expiry window with jitter (one bounded
   attempt in the call's path), plus the durable sweeper for idle-but-expiring
   connections; a token must never refresh more than once per call, and the sweeper owns
   everything the call path shouldn't wait for.
5. **Handle TTL and revocation freshness.** How stale a resolution may be. Leaning:
   handles live for minutes and every resolution reads live connection state — the release
   proof's "fails closed at the next tool call" forbids caching revocation decisions, and
   one store read per resolution is cheap against a network call's latency. Handle
   validity itself (expiry, scope binding) is self-contained in the handle; only the
   connection liveness check hits the store.
6. **Diff representation for JSON families.** Text diffs are settled; structural diffs
   are not. Leaning: an added/removed/changed-leaves diff over the `canonicalize_value`
   form, computed on read — deterministic by construction, and honest that a reordering of
   canonical JSON is not a change.
7. **Unregistered middleware layers.** Whether chains may mix registry-composed and
   code-registered layers. Leaning: yes, with the code-registered suffix unpinned and the
   manifest saying so by absence — deployments that want full pinning compose entirely
   through the registry; forcing the migration in one release would break every existing
   chain for a property most runs never audit.
