# Rusty Operations Plane design (R0.12)

The Operations Plane release closes the two gaps the roadmap leaves after the Extension
Plane: a first-class **artifact plane** for the binary outputs runs produce, and a
**deployment control plane** for shipping graph changes through dev/staging/prod without
leaving the single binary. The governing claim, stated precisely: **every artifact a run
produces becomes a content-addressed, lineage-carrying, permission-checked, retainable
object whose bytes live in the R0.7 blob store and whose existence is journaled back to
the effect that produced it — and every deployment change moves as an immutable revision
through declared environments, gated by `rusty-eval` release gates, canaried and shadowed
by seeded draw at admission, rolled back by revision pointer, with each transition
journaled into the deployment's evidence chain so the whole path lands under the signed
receipt.** Both halves close their gap by composing machinery that already exists: the
artifact plane is the R0.7 `ArtifactStore` and `ArtifactRef` contracts turned toward
operators instead of replay; the control plane is the R0.8 version-pointer pipeline
applied to deployment revisions, with R0.11's environment tags promoted to first-class
records — exactly the landing R0.11's own design reserved for this release.

The release has two planes, in the roadmap's order: the **artifact plane** (the smaller,
self-contained surface) and the **deployment control plane** (the larger surface, and the
release proof's subject). They share one discipline — the deployment's journaled evidence
chain — and one constraint, restated from the roadmap because everything below bends to
it: **single-binary self-hosting remains the default; the control plane manages it, never
replaces it.**

## Why this belongs in the runtime

Artifacts managed at framework level — a generated file on a local path, a blob URL in a
tool result, a base64 blob in a message — lose the same three things framework-level
memory lost before R0.8 and framework-level configuration lost before R0.11. **Evidence**
is absent: the journal already records the producing effect with its class, latency,
cost, and causal parentage, but nothing connects that effect to the file that outlived
the run, so "which run made this image, and was it the canary's" is unanswerable a day
later. **Governance** is a convention: an output persists until someone deletes a
directory — no retention policy, no permission check at read, no version history but
whatever the filesystem kept. **Blast radius** is invisible: an artifact read by a later
run or an external system carries no record of who may read it or when it stops existing.

Deployment managed at framework level fails the same way, one level up. A graph change
ships because someone rebuilt and restarted the binary: no immutable revision to name, no
environment to stage through, no gate but CI's, no rollback but the previous binary. The
evidence for this gap is internal and already written down: R0.11's design lists
deployment environments in its not-built list with the sentence "R0.12 builds
environments as a control plane," and R0.11's open question 1 leans on this release to
promote environments to first-class records. The tags shipped in R0.11 name promotion
targets that do not exist yet — `prompt:system@prod` resolves against a
deployment-declared label, and nothing in the system can say what prod *is*, what
revision it runs, or what it ran yesterday. Tags without environments are labels pointing
at a control plane that does not exist; this release builds it.

There is also a two-sided argument unique to the artifact plane. R0.7 shipped
content-addressed artifact *references* without an artifact *surface*: `ArtifactRef`
(`rusty-core/src/record.rs`) gives oversized payloads a SHA-256 identity, and the
`ArtifactStore` trait (`rusty-core/src/journal.rs`, with `FileArtifactStore` and the
Postgres `rusty_artifacts` table) gives the bytes a home — but both exist to make journal
snapshots replayable, not to make outputs operable. Nothing answers "list this tenant's
outputs," "show a preview," or "delete everything older than thirty days except what a
receipt still pins." References without a surface are addresses that resolve nothing
beyond replay. R0.9 then shipped signed receipts committing to journal heads that cover
artifact *hashes*, and nothing can yet say whether the bytes behind a hash still exist.
R0.12 builds the surface the shipped contracts already assume, and states precisely how
it must not break them.

## The artifact plane

### Two artifact concepts, one word

The word "artifact" now names two things in this codebase, and conflating them would fake
coverage the way R0.11 refused to conflate memory records with memory configuration.
**Registry artifacts** (R0.11, `rusty-core/src/registry.rs`, `ArtifactRecord`) are
human-authored, JSON-shaped *configuration*: prompts, tool contracts, model settings,
middleware compositions. A registry commit *is* a candidate; governance is the learn
pipeline; the bytes are small, canonical, and diffed. **Run artifacts** (this release)
are run-produced, arbitrary-byte *outputs*: generated files, images, audio, exported
datasets. A run artifact is not a candidate, has no promotion lifecycle, and is never
diffed; its governance is lineage, permissions, and retention, not envelopes and
evaluation.

The type system keeps them apart. This release's core type is `RunArtifact`, and the
server surface is `/artifacts` — deliberately distinct from `/registry/artifacts`, so the
route grammar itself says which concept a caller addresses. Where the two meet, they meet
by reference, never union: a run artifact may *carry* a rendered prompt or an exported
dataset, but it carries them as bytes with a media kind, and no code path coerces one
concept into the other. The naming debt is acknowledged: R0.7's `ArtifactContract`
(`rusty-core/src/durable.rs` — kind, size bound, optional schema) already describes the
*shape* of task and mailbox payloads; `RunArtifact` records are where those payloads'
bytes become operable once they outlive the queue.

### The data model

One persisted entity, the `RunArtifact` — the plane's only new record, deliberately thin
because the bytes and the evidence already have homes:

- `artifact_id` — the content address: lowercase hex SHA-256 of the bytes, exactly the
  `ArtifactRef.sha256` rule. Identity is integrity; two runs producing byte-identical
  outputs share one object, and a read re-hashes before it serves.
- `name` — an optional logical name (`weekly-report`) under which versions accumulate.
  Unnamed artifacts are address-only: produced, referenced, retained, expired. The naming
  rules are the registry's (`MAX_ARTIFACT_NAME_LEN`, no `@`, no `/`, no control
  characters) for the same reason: names ride in keys that tenant prefixes and
  environment tags already punctuate.
- `media_kind` — a closed, additively-evolved enum (`file` / `image` / `audio` / `data`),
  plus the declared media type string the producing node asserted. The enum drives
  preview eligibility; the string is preserved verbatim because the runtime cannot
  certify a producer's claim, only record it.
- `lineage` — the join that makes the plane evidence: the producing run id, the
  deterministic `EffectId` of the producing effect (`rusty-core/src/effects.rs`,
  `derive_effect_id`), and the journal event id whose output carried the reference.
  Recorded at commit, never edited.
- `versions` — for named artifacts, an ordered, append-only sequence of
  `ArtifactVersion { sha256, bytes, committed_at }` — the `ArtifactCommit` discipline
  (append-only, content-addressed) applied to bytes. The current version is the last;
  older versions are addresses, retained under the same policy.
- `retention` — a declared `RetentionPolicy`: a closed enum of `pinned` (retain until
  explicitly released), `days(n)`, and `receipt_bound` (the default — retain at least as
  long as any signed receipt whose journal references the address). Retention is declared
  at commit and journaled with it; a change that would shorten live retention is a
  governance act, not housekeeping, and follows the approval discipline below.
- `permissions` — tenant scope, and nothing finer. Cross-tenant access answers 404, never
  403, per the v0.5 rule. The plane adds one check of its own: an artifact whose
  producing run's evidence is gone is read-refused with a typed miss rather than served
  from a store whose context no longer exists.

The journaled half is one additive `RunEventKind`, `ArtifactCommitted` — the
`CapsuleResolved` / `ConfigResolved` precedent applied to outputs. Its payload,
`ArtifactCommitment`, names the artifact id, the name and version index when named, the
media kind, the byte count, the producing `EffectId`, and the declared retention. The
event sits in the run's own journal, so the signed receipt's head covers it transitively:
the walk is signed receipt → journal head → `ArtifactCommitted` → `EffectId` → the
effect's journaled record → the bytes behind the address. The bytes never enter the
journal; the journal carries the reference and the commitment, and the plane carries the
rest.

### What is reused, what is new

Reused unchanged — the point of the plane:

- **The addressing contract.** `ArtifactRef`, `PayloadRef::Artifact`, and the canonical
  hashing rule (`content_hash` agrees across inline and referenced representations) are
  untouched. A run artifact's address is the same digest the journal already stamps.
- **The byte stores.** `ArtifactStore`'s `put` / `get` / `contains` with integrity
  verification on every read: `FileArtifactStore`'s one-file-per-address,
  dedupe-by-construction writes, and `PostgresArtifactStore`'s `rusty_artifacts` table.
  The plane stores bytes through the trait and nothing else, so both backends come along
  and a store that cannot prove its bytes stays corruption, not data.
- **The effect kernel.** `derive_effect_id` gives lineage its deterministic anchor: the
  producing effect's id is re-derivable at audit from the journaled scope, kind, input
  hash, and key — no new identity minting.
- **The storage discipline** (`rusty-server/src/server_store.rs`): one JSON file per
  record on the file backend, atomic temp-write-then-rename, column-mapped Postgres
  tables auto-migrated under the transaction-scoped advisory lock, tenant id-prefixing.
- **The sweeper precedent** (the broker's refresh sweeper): retention enforcement is
  durable work — leased, retried under `ErrorClass`, journaled — because an artifact the
  sweeper failed to expire is a quiet policy violation.

Genuinely new — nothing below exists today:

1. **The `RunArtifact` record and version sequence**, above: the metadata the blob store
   deliberately does not carry.
2. **The journaled `ArtifactCommitted` event** and its payload, golden-pinned like every
   wire shape since R0.5.
3. **The `/artifacts` server surface**: commit, read, list by tenant with filters (name,
   media kind, producing run), version history, preview, retention administration.
   Commits arrive from two paths — a node declaring an output through the SDK object, and
   the server persisting a journaled `PayloadRef::Artifact` whose producing node opted
   in — and both write the same record shape and journal the same event.
4. **Previews**, derived on read: bounded text/JSON rendering for `file` and `data`, a
   downscaled thumbnail for `image`, waveform metadata for `audio`. The `RegistryDiff`
   precedent governs exactly: computed on read, never stored — a stored preview is a
   second, divergent account of the same bytes, and a preview that cannot be derived is
   an honest empty answer, not a placeholder.
5. **The retention sweeper and its pinning rule**, covered under the honest edges because
   it is where this plane can hurt itself.

### Storage on both backends

Bytes go through `ArtifactStore` unchanged: files under the artifact directory on the
file backend, `rusty_artifacts` on Postgres. Metadata follows the established convention.
The file backend keeps one JSON file per `RunArtifact` under `{store_path}/artifacts/`
(`artifacts` joins the reserved layout names), named by content address, with named-
artifact version indexes as hash-named pointer files carrying key-bearing envelopes — the
`learn/versions` discipline, because artifact names carry the same punctuation surface
keys do. Postgres gains one additive migration, `server_run_artifacts` — content-
addressed primary key, tenant column, column-mapped fields, full record as `payload`,
and a listing index on `(tenant, name)` mirroring `server_registry_artifacts`'s
`(tenant, family, name)` — appended to `MIGRATION_SQL` under the advisory-locked,
`CREATE TABLE IF NOT EXISTS` convention every R0.11 table followed.

One deliberate asymmetry, stated plainly: metadata is tenant-scoped, bytes are not
namespaced at all. Content addressing already makes byte storage global — two tenants
producing identical bytes share one object — and that is correct for integrity, provided
the metadata layer is the only path that lists or resolves. It is: nothing serves bytes
without resolving a `RunArtifact` record first, and records are tenant-scoped, so a
shared address grants no cross-tenant read path.

### Failure modes — everything fails closed

- **Integrity failure on read**: the store's existing refusal — bytes that do not
  re-hash to their address are corruption, a typed error, never a served object.
- **Missing bytes for a live record**: a typed, journaled miss (`artifact_unavailable`),
  distinct from 404 — the record exists, the bytes do not, and the difference is exactly
  what a retention audit needs. Replay against a pruned artifact fails the same way: the
  R0.5 exact-replay rule (every effect served from the journal) fails closed with the
  miss named, never a silent live re-execution.
- **Sweeper failure**: the scan-and-prune step journals what it intends before it
  deletes; a crash mid-sweep leaves intentions auditable and bytes recoverable. A sweeper
  that cannot verify a receipt's coverage of an address does not prune it — coverage it
  cannot check is coverage it must assume.
- **Journal failure at commit**: hard-fail journaling governs this surface as it governs
  the learn plane — nothing reaches the store the journal did not record first. A commit
  that cannot journal its event does not persist the record; the bytes may sit orphaned
  in the blob store (content-addressed, unlisted, eventually swept), which is a storage
  cost, never an evidence lie.

### The honest edges

**Large binaries against a JSON journal.** The journal's artifact map holds JSON `Value`s
keyed by hash, bounded inline by `INLINE_PAYLOAD_MAX_BYTES` (4 KB). Run artifacts are
arbitrary bytes, potentially hundreds of megabytes, and they do not belong in snapshots:
a snapshot is a replay fixture, and embedding a generated video in one makes every export
a copy of every output. The rule is that the journal carries references and commitments,
never run-artifact bytes — the `snapshot_externalized` discipline (spill payloads to
`artifact_refs` so a fixture names bytes instead of embedding them) made the default
rather than the option. The cost is real and stated: an exact replay that must *compare*
a regenerated binary against the recorded one needs the bytes, so fixtures that exercise
artifact comparison externalize them alongside the snapshot, content-addressed, verified
on load. Replay of control flow never needs the bytes — the reference is the evidence —
and that is the common case the rule optimizes for.

**Retention against the immutable receipt chain.** A signed receipt commits to a journal
head; the journal covers `ArtifactCommitted` events naming hashes; the bytes behind a
hash are not in the chain. Deleting bytes therefore cannot falsify a receipt — the chain
verifies over events, and the events survive — but it can falsify the receipt's
*usefulness*: a receipt proving a run produced `sha256:abc…` is cold comfort when `abc…`
was pruned. The leaning is the `receipt_bound` default above: an address referenced by
any non-expired signed receipt is unprunable under time-based policies, the sweeper
checks coverage before every prune, and an operator who wants bytes gone sooner releases
the pin explicitly — a journaled act on the deployment chain, because shortening evidence
retention is a governance decision with a name on it, never a sweeper optimization.
Receipt retention itself stays operator policy (R0.9 deferred fleet key management; R0.12
defers fleet evidence lifecycle on the same argument), declared as a seam.

**Permissions stop at tenancy.** The plane's permission model is the deployment's: API
keys, tenant isolation, 404 never 403. Per-user or per-run grants inside a tenant are
post-R1.0 RBAC work, and the record's `permissions` field is deliberately one tenant
scope so a later layer narrows additively rather than migrating. Stated here so no one
reads "permissions" in the roadmap bullet as more than it is.

## The deployment control plane

### Revisions and environments

Two new persisted entities, both small because the heavy machinery is reused.

The **`DeploymentRevision`** — an immutable, content-addressed declaration of what may
serve. It carries: `revision_id` (content address over the canonical form, the
`derive_candidate_id` discipline); the graph/assistant identity and the `graph_hash` the
checkpoint header already computes (code identity is the R0.7/R0.11 story, unchanged); a
**pin set** — the registry surfaces the revision binds, resolved to candidate ids at
revision *creation* from a declared source environment; the author (`human:{id}`, the
registry commit discipline); and `created_at`. The pin-set freeze is a designed choice: a
revision evaluated against a recorded dataset must be the same thing the gate evaluated
when it canaries, and a pin set resolved at admission would make the gate's evidence a
moving target. Freezing makes revisions heavier — a registry prompt promotion does not
flow into an existing revision — and that weight is the price of evaluable deployments,
paid deliberately and argued out in the honest edges.

The **`Environment`** — R0.11's tags promoted to first-class records, per the leaning
R0.11's open question 1 recorded for this release. It carries: its name (the R0.11 tag
set — `dev` / `staging` / `prod` by deployment-declared convention, not by enum); the
deployment pointer (below); the gate and approval declarations that govern promotions
into it; and creation metadata. What an environment is *not* — the R0.11 tag discipline
applied one level up: not a separate process, not an isolated store, not a trust
boundary. Environments are logical surfaces over one deployment's stores and one binary's
admission path; the control plane manages which revision each binds at admission.

Environments and revisions meet in the **`DeploymentPointer`** — the R0.8
`VersionPointer { surface, active, canary }` shape applied to revisions, one pointer per
environment, surface key `deployment:{env}`. `active` is the full-traffic revision;
`canary` is a `CanaryDeployment { revision_id, fraction }` binding one revision to a
declared fraction of new runs while `active` serves the rest. Promotion moves `active`
and clears any canary — a full promotion supersedes the experiment it graduated from, the
exact `VersionPointer::promoted` semantics. Rollback re-points `active` to the previously
serving revision: byte-exact, because the restored revision is the immutable record that
served before, not a reconstruction. Canary admission is the seeded draw:
`canary_admits` over the pointer surface, so a canary at staging and a canary at prod are
independent draws over the same run id, and a recorded run re-derives its assignment from
the journaled resolution alone. The draw machinery is reused verbatim; only the surface
it draws over is new.

### What is reused, what is new

Reused unchanged:

- **The pointer and draw machinery** (`rusty-core/src/learn.rs`): the two-slot pointer
  shape, the promotion/rollback moves, `canary_admits`'s documented seed derivation, and
  the admission conservatism — new runs bind at admission, in-flight runs keep the
  revision their checkpoint header pins. No hot reload, for the reason R0.11 gave: a
  hot-reloaded deployment is a silent behavioral rewrite, forbidden since R0.8.
- **The registry as the configuration substrate**: revisions pin registry candidate ids;
  they do not re-version configuration. The registry answers "which prompt"; the revision
  answers "which frozen set of prompts, tools, and models, over which code."
- **The journaled resolution precedent** (`ConfigResolution`, R0.11 wave 2): one additive
  `RunEventKind::DeploymentResolved`, journaled at admission, naming the environment, the
  bound `revision_id`, the pointer slot (`active` or `canary`), and the pin-set digest.
  It sits inside the journal the run's signed receipt covers — the audit walk is signed
  receipt → manifest digest → `DeploymentResolved` → revision → pin set → candidates →
  authors and approvals, every hop signature-covered.
- **The eval gate as a pure function** (`rusty-eval/src/gate.rs`): `GatePolicy`,
  `evaluate_gate`, `GateDecision` with typed checks. The workspace's dependency direction
  is respected exactly as the R0.8 `CandidateEvaluator` seam respected it — `rusty-eval`
  links the runtime, never the reverse — so the control plane consumes the gate through a
  declared core seam, not a crate dependency.
- **The deployment-chain precedent** (`receipt-keys`, `credential-broker`): control-plane
  transitions journal into one deployment evidence chain, `deployment-control`, with the
  author in each payload — a deployment transition is not any run's event, and the chain
  is the lineage evidence the audit reads.
- **The custody precedent** (`rusty-server/src/broker.rs`): envelope-encrypted secret
  storage, keys under `{store_path}/keys/` in the receipt-secret discipline.

Genuinely new:

1. **`DeploymentRevision` and `Environment` records**, above.
2. **The `DeploymentPointer` store surface** — one pointer per environment, CAS moves
   with the journaled transition and the pointer move in one transaction (the learn
   store's rule: a crash cannot leave a promoted revision whose pointer never moved).
3. **`RunEventKind::DeploymentResolved`** and its payload, golden-pinned.
4. **Environment-scoped secrets**, below.
5. **The release-gate wiring and shadow admission context**, under canary and shadow.
6. **The health and log surfaces**, derived views, below.

### Environment-scoped secrets

R0.11 drew a careful line: the broker brokers OAuth connections and per-user credentials;
static application secrets kept their file discipline because they "are not connections
and gain nothing from brokerage." R0.12 changes one term of that argument: deployments
now have environments, and a secret's *scope* is part of its identity — the staging
database URL and the prod database URL are two secrets that must never be
interchangeable. Environment-scoped secrets are therefore custody, not brokerage: named,
tenant-scoped, environment-tagged values, envelope-encrypted at rest under the deployment
master key (per-secret data keys wrapped by the master key, XChaCha20-Poly1305 with the
secret id as associated data — the broker's construction verbatim), stored as ciphertext
on both backends, resolved at use inside host-side code, journaled as metadata (never
bytes) on the deployment chain. There is no refresh, no consent, no provider — a static
secret is set, read at admission or connector setup, rotated by replacement under the
stable scoped name, and revoked by deletion; each act journals. Rotation beneath a stable
scoped name is the broker's "rotate a credential without redeploying" argument applied to
static material: what a run's evidence pins is the scoped name, not the value of the
moment.

Two broker boundaries carry over unchanged. Plaintext enters the store on neither
backend, ever — a Postgres dump contains no plaintext secret, and the exit criteria
assert it by reading raw rows. And the master key lives outside the store abstraction
under `{store_path}/keys/`, `0600` from the first byte, so the Postgres backend cannot
hold what a database must not leak. Scoping is enforcement, not convention: a run
admitted to staging resolves `name@staging` and nothing else; a request outside the run's
environment is denied at resolution with a typed, journaled refusal — the `CapsuleDenied`
discipline (attributable to a declaration, not a stack trace) applied to environment
scope.

### Release gates, canary, and shadow

**The gate.** A promotion into an environment with a declared gate requires a
`GateDecision` whose `allowed()` is true, over a named dataset version, computed through
the core seam. The gate's inputs are the revision's own evaluation: the frozen pin set
replayed against the recorded dataset (the R0.8 composition — replay plus a `rusty-eval`
experiment and a `compare()` verdict — lifted from candidates to revisions), with
`compare()` against the currently serving revision as baseline.
`detect_pass_rate_regression`'s fail-closed stance is inherited, not re-argued:
underpowered comparisons are `insufficient_evidence`, never silently safe, and an
unavailable gate is a refused promotion. The decision journals onto the deployment chain
— policy name, dataset version, outcome, failed checks — because a gate with no record is
a gate that can be retroactively widened. Strictness per environment is deployment-
declared: dev may promote on a clean verdict alone; prod additionally requires a human
approval token scoped to the revision's promotion effect id (derived over the revision's
content address — the `promotion_effect_id` discipline, non-transferable).

**The canary.** A revision canaries by binding the pointer's canary slot to a declared
fraction; admission draws per run, seeded, reproducible. Graduation is a promotion act:
the operator (or a declared auto-rule at dev) promotes the canaried revision after its
evidence clears — the same gate seam, optionally fed by canary-vs-active comparison over
the window's journaled runs. There is no automatic traffic controller in this release:
fraction changes are declared, journaled acts, because an invisible controller moving
production traffic is precisely the unowned decision every governance surface since R0.8
exists to refuse.

**The shadow.** A shadow deployment runs the candidate revision against copied or
recorded traffic *without the candidate's effects reaching the world*. The effect kernel
makes this enforceable rather than advisory: the shadow executes under an
`EffectAdmissionContext` that admits `Pure` and `ReadOnly` effects and refuses everything
above — `Idempotent` included, because "idempotent" means safe to retry under one key,
not safe to execute twice from two revisions, and a shadowed charge is a charge.
Refusals are typed violations surfaced as shadow-run evidence, the `MissingApproval`
discipline: the shadow's report shows which effects it would have attempted, classified,
never executed. Where the candidate's behavior depends on a refused effect's result, the
shadow serves the recorded outcome from the source run's journal — the hybrid-replay
rule, pin selected effects and re-run others — so the shadow evaluates *decisions*
against the recorded world rather than executing a parallel one. Shadow runs journal into
their own journals, marked by role (R0.10's twin shadow-pair discipline), and never sign
receipts as production evidence; their verdicts feed the gate, and the gate's journal
entry is the durable record. The honest limit is in the edges: a shadow proves nothing
about an irreversible effect's real outcome, only that the candidate would have attempted
it.

### Health and log surfaces

Derived views, never new stores. `GET /deployments/health` is a tenant-wide board — the
`/connections/health` precedent — reporting per environment: the active revision, the
canary binding and fraction, the last gate decision, recent run outcomes by pointer slot,
and the deployment chain's head. Per-revision health reads the journaled runs that bound
it through `DeploymentResolved`: counts, failure classes, latency and cost summaries,
computed on read from journals the server already persists. Logs are the journals — run
journals, the deployment chain, the receipts journal — exposed as filtered read views;
there is no second log pipeline to drift from the evidence. The failure posture is the
broker's: health that cannot be checked is health that must be assumed absent.

### Storage on both backends

The file backend keeps `{store_path}/deployments/` (`deployments` joins the reserved
layout names): one JSON file per revision named by content address, one per environment,
hash-named pointer files with key-carrying envelopes (the `learn/versions` discipline),
and `{store_path}/env-secrets/` holding ciphertext envelopes per scoped name. Atomic
temp-write-then-rename throughout. Postgres gains two additive migrations appended to
`MIGRATION_SQL` under the advisory-locked convention: `server_deployments` (revision and
environment records, column-mapped, tenant column, listing index on
`(tenant, environment)`) and `server_env_secrets` (ciphertext and wrapped data keys only,
keyed by tenant-scoped `name@environment`, mirroring `server_connections`'s custody
shape). Pointer moves execute as one transaction each, per the R0.6 outbox discipline: a
state transition and the evidence that caused it must not split-brain.

### Failure modes — everything fails closed

- **Unresolvable revision or environment at admission**: the run never starts — the
  R0.11 admission rule (404 unpromoted, 422 malformed) applied to deployments. A pointer
  serving nothing binds nothing; there is no implicit "latest," because latest is a guess
  with a deploy's blast radius.
- **Gate unavailable or underpowered**: promotion refused. `insufficient_evidence` fails
  closed, and a gate the control plane cannot reach is a gate that did not pass.
- **Shadow effect refusal**: typed violations, journaled as shadow evidence, never
  executed — the shadow context holds no approval tokens to consume and no retry path
  around the refusal.
- **Environment-secret scope miss**: denied at resolution, typed and journaled, naming
  the scope requested and the scope the run holds. A scope check that cannot be performed
  is a check that fails; there is no degraded mode that skips it.
- **Concurrent pointer moves**: serialized by the store's CAS and the transaction
  discipline; the loser retries against the moved pointer or fails with a typed conflict —
  never a lost move.

### The honest edges

**Canary traffic splitting inside one binary.** A real canary in the platform sense
splits traffic at a load balancer across two builds. R0.12's canary splits *run
admission* inside one binary by seeded draw: both revisions' graphs are compiled into the
serving process (the assistant registry already holds many graphs; a revision selects
among them plus a frozen pin set), and the draw decides which revision a new run binds.
What this gives up, stated plainly: it cannot isolate a crash or a memory leak to one
revision — process-level faults are shared, so a candidate that panics takes prod traffic
down with it, and the control plane says so rather than implying otherwise. What it
keeps: the whole governance story — immutable revisions, gated promotion, reproducible
assignment, byte-exact rollback — without leaving the single-binary default the roadmap
forbids replacing. Process isolation is the multi-host topology question, listed in the
not-built section with the seam named (remote workers already execute graphs out of
process; a fleet-aware canary is a placement policy over that seam, post-R1.0).

**Shadows and irreversibility.** The shadow admission context refuses `Idempotent`,
`Compensatable`, and `NonIdempotent` effects; it does not simulate them. A candidate
whose value lives in its irreversible effects — a new charge flow, a new email path —
gets shadow evidence only up to the refusal boundary, and its gate evidence must come
from the recorded dataset replay instead. This is the taxonomy doing the work it was
built for, and also its limit honestly reported: the runtime can guarantee a shadow does
not double-apply *declared* effects; a node performing an undeclared side effect outside
the typed path is outside the guarantee, the same cooperative-boundary limit R0.7
documented for calling `Tool::call` directly.

**The pin-set freeze, priced.** Frozen pin sets make revisions evaluable and make them
stale: every registry promotion that should reach prod requires a new revision. The
design accepts the staleness because the alternative — pins resolved at admission — makes
the gate's evidence expire silently: a revision canaried on Monday against pin set P
serves P′ by Wednesday with no deployment-layer record of the change. A revision-refresh
path (re-resolve the source environment, mint a new revision addressing the delta) is
cheap precisely because revisions are content-addressed declarations, and the refresh is
a journaled act like any other. Heavy and honest beats light and unaccountable — the same
trade the run manifest made in R0.7.

## Governance wiring

- **Envelopes, per environment.** Promotion rules are deployment-declared per
  environment: dev may auto-promote on a clean gate verdict; staging may require the gate
  plus canary residency; prod requires the gate plus a human approval token scoped to the
  revision's promotion effect id — the `r08_default` stance extended one level up, the
  revision standing in for the candidate. The declaration is versioned and journaled with
  each transition, so an audit reads the rule in force, not a later edit.
- **Approvals compose the effect kernel, unchanged.** Out-of-envelope promotions mint an
  `ApprovalToken` scoped to an effect id derived over the revision's content address; an
  approval for one revision admits no other. No parallel approval mechanism — the third
  release in a row to reuse the kernel's boundary rather than invent one.
- **Receipts close the loop twice.** Every control-plane transition — revision
  registered, promoted, rolled back, canary declared or cleared, gate decision, secret
  set or rotated — journals onto the `deployment-control` chain with the author attached.
  Every run admitted under a revision journals `DeploymentResolved` in its own journal,
  under the signed receipt's head. The release proof's "every step on the receipt chain"
  is therefore two claims, both testable: the deployment's history is a hash-chained
  journal an auditor can walk, and each affected run's signed receipt reaches its bound
  revision through the journaled resolution.
- **Tenant isolation is unchanged everywhere.** Revisions, environments, pointers,
  secrets, and artifacts are tenant-namespaced; cross-tenant access answers 404. A shared
  revision catalog would be a tenant-isolation breach wearing a control-plane costume —
  the R0.11 sentence applied one release later.
- **The single binary is the trust boundary, and says so.** The control plane's authority
  ends at the process it manages: it cannot attest anything about the machine, the build,
  or the operator's other infrastructure, and its health surface reports
  runtime-observable facts only. Remote attestation and fleet identity are the R1.0
  security review's scope, deferred here exactly as receipt attestation was in R0.9.

## What R0.12 deliberately does NOT build

- **Multi-host or multi-process deployment orchestration.** Fleet rollouts,
  process-isolated canaries, autoscaling, and placement across machines are post-R1.0
  topology work. The seam exists (remote workers execute graphs out of process today) and
  is named rather than built toward blind. Single-binary self-hosting remains the
  default; the control plane manages it, never replaces it.
- **A container, VM, or package manager.** A revision pins graph identity and
  configuration, not machine images. Building and distributing binaries stays the
  operator's toolchain; the control plane starts where the binary boots.
- **Hot reload of running runs.** Promotions bind at admission; in-flight runs keep the
  revision their checkpoint header pins. There is no path that changes what a started run
  is executing, because that path is a silent behavioral rewrite — forbidden since R0.8.
- **Automatic traffic controllers.** Canary fractions change by declared, journaled acts.
  No controller observes metrics and moves traffic on its own; auto-graduation rules are
  deployment-declared configuration at dev only, never an invented default.
- **A general secrets vault.** Environment-scoped secrets cover static deployment
  configuration with an environment dimension. They hold nothing dynamic (the broker owns
  OAuth material) and gain no import/export, sharing, or leasing features this release.
- **Object-storage backends.** S3-class artifact storage is a declared seam behind the
  `ArtifactStore` trait, not an implementation: put/get/contains is exactly what an
  object store satisfies, and this release ships the two backends the codebase already
  has.
- **Artifact RBAC beyond tenancy, and cross-tenant sharing.** Both are post-R1.0, for the
  reason R0.11 refused a prompt marketplace: isolation first, sharing never before the
  security review.
- **Log aggregation or metrics pipelines.** The health and log surfaces are derived read
  views over journals the server already keeps; the runtime does not grow a second
  observability stack beside the evidence it signs.

## Wave plan and release proof

**Wave 1 — run artifact contracts and store.** The `RunArtifact` / `ArtifactVersion` /
`RetentionPolicy` records, the `ArtifactCommitted` event and its payload with golden
files, the commit path from both sources (SDK-declared outputs and journaled spill),
lineage recording, and the `/artifacts` routes on both backends with the
`server_run_artifacts` migration. Exit: an effect commits a named artifact; lineage
resolves run → effect → bytes on both backends; a restart preserves records and versions;
goldens pin every new wire shape; a read that fails integrity is refused as corruption;
R0.7 journal artifacts and snapshots keep deserializing unchanged.

> **Wave 1 status: implemented (2026-08-10).** The contracts landed in
> `rusty-core/src/artifact.rs` (`RunArtifact` / `ArtifactVersion` /
> `ArtifactLineage` / `RetentionPolicy` / `MediaKind`, the `commit_artifact`
> constructor, goldens in `rusty-core/tests/golden/` with the golden asserts
> in the module's unit tests), the additive `RunEventKind::ArtifactCommitted`
> variant, and the `/artifacts` routes with the `server_run_artifacts`
> migration in `rusty-server` — with eight settled refinements: the event
> variant lives in `record.rs` where `RunEventKind` is defined, not
> `journal.rs` as the plan's phrasing suggested; commit bytes ride the
> payloads hex-encoded (the repo's dependency-free `broker` codec — no base64
> dependency); the effect id is caller-declared and checked for the
> digest-derived shape at commit, with the full verification at audit; a
> second name for the same bytes, a taken name with different bytes, or
> different bytes on the same lineage answer 409 — version accumulation is
> wave 2; name uniqueness on Postgres is advisory in this wave (no unique
> constraint, a documented edge the file store enforces by layout); the typed
> 410 miss for gone bytes is not journaled here — the journaled miss is wave
> 2's exit criterion; spill commits inherit the post-hoc journal-append
> caveat for live runs (the executor seam is follow-up work); and identical
> re-commits converge to 200 without a second journal event.

**Wave 2 — permissions, previews, versions, retention.** Named-artifact version
sequences, previews derived on read per media kind, the retention sweeper as durable work
with receipt-coverage pinning, and the journaled retention-release act. Exit: a named
artifact accumulates three versions and serves each by address; a preview derives for an
image and answers empty-honestly for an underivable kind; the sweeper prunes expired
bytes and a subsequent exact replay against them fails closed with a typed, journaled
miss; an address pinned by a live signed receipt survives the sweep, and the operator's
journaled release is the only path that prunes it; a Postgres dump asserts metadata holds
no byte payload.

> **Wave 2 status: implemented (2026-08-10).** Version accumulation
> landed as `append_artifact_version` plus a store-level
> compare-and-swap (`put_run_artifact_version`), previews as
> `derive_preview` behind `GET /artifacts/{id}/preview`, and retention
> as the `ArtifactRetention` plane behind `POST /artifacts/{id}/release`,
> `POST /artifacts/sweep`, and `GET /artifacts/journal`, with the three
> retention payloads (`ArtifactPrune` / `ArtifactRelease` /
> `ArtifactUnavailability`) and the preview wire shapes golden-pinned in
> `rusty-core` — with ten settled refinements: the three event variants
> live in `record.rs` appended after `artifact_committed` (the wave-1
> precedent), not a new module; previews are dependency-free (BMP/PNM
> thumbnails, WAV PCM metadata, the 4 KB text/JSON window), so
> compressed formats answer the honest `empty` per open question 7 — a
> codec dependency stays the measured-need seam; every retention act
> (releases, prune intentions, typed misses) journals onto the
> deployment's `run-artifacts` evidence chain, never the producing
> run's journal, so a retention act cannot rewrite receipt-covered
> evidence; receipt coverage verifies with the new
> `verify_receipt_prefix` — a receipt pins the addresses its covered
> events name however much the journal grew since the mint (whole-
> journal verification would pin-all forever after any post-mint
> commit); coverage the sweeper cannot verify (a missing journal, an
> unknown signer key, a failed verification, an unparseable covered
> commitment) pins *everything* that pass — fail closed, never prune
> what a receipt may cover; `POST /artifacts/sweep` was added as the
> operator-triggered pass (deterministic for a given store state, so
> tests and audits reproduce it), with the interval sweeper off by
> default behind `artifact_sweep_interval`; miss journaling converges on
> the first observation per (tenant, address) and is best-effort — the
> typed 410 is the contract and stands either way, the broker's
> `journal_denial` precedent; the release journals hard-fail but its
> prune tail is best-effort (the sweeper converges a failed delete,
> and the journaled intention is never re-journaled); the Postgres
> version CAS rides a per-name advisory lock and needs no new migration
> — versions accumulate inside the record's JSONB payload, so
> `MIGRATION_SQL` stays at 51 statements; and the "exact replay fails
> closed" exit is realized as the artifact byte read, which *is* the
> replay's byte source — server-side `/runs/replay` replays control flow
> from the journal and never touches blob bytes, the split this design
> states. The 8-bit PCM waveform magnitude fix (8-bit samples are
> unsigned-offset) is an internal correction, not a deviation.

**Wave 3 — revisions, environments, and environment secrets.** The `DeploymentRevision`
and `Environment` records with the frozen pin set, the per-environment `DeploymentPointer`
with byte-exact rollback and one-transaction moves, `DeploymentResolved` journaled at
admission with goldens, environment-scoped secret custody on both backends, and the
`/deployments` routes with `server_deployments` and `server_env_secrets` migrations.
Exit: a revision registers and promotes dev → staging → prod by pointer move; a rollback
restores the prior revision byte-exact; a run admitted to staging journals its resolution
and its signed receipt walks to the bound revision; an environment secret resolves at use
with ciphertext only in both stores (the Postgres assertion reads the raw row); a
cross-environment secret request fails closed, typed and journaled.

> **Wave 3 status: implemented (2026-08-11).** The contracts landed in
> `rusty-core/src/deploy.rs` (`DeploymentRevision` / `RevisionContent` /
> `RegistryPin`-based pin sets with `pin_set_digest`, `Environment` with
> its declaration and gate records, `DeploymentPointer` with the canary
> slot, the promotion / rollback / registration / declaration / secret
> act payloads, `DeploymentResolved`, `deployment_admission`, and the
> env-secret custody functions — twelve goldens in
> `rusty-core/tests/golden/`), the additive `RunEventKind` variants, and
> the `/deployments` routes with the `server_deployments` and
> `server_env_secrets` migrations in `rusty-server`, exercised end to end
> by `rusty-server/tests/deployments.rs` (promote/rollback byte-exactness,
> admission and the receipt walk, secret custody and scope, restart, and
> the Postgres raw-row assertion) — with twelve settled refinements: the
> seven event variants live in `record.rs` where `RunEventKind` is
> defined (the wave-1/wave-2 precedent), not a new module; the revision's
> `graph_hash` is server-computed at registration from the registered
> graph's current topology hash and re-checked at admission (name and
> hash drift both refuse 422), so a build the revision no longer
> describes is never run; the pin set freezes the source environment's
> ACTIVE pointers only — a canary binding never freezes into a revision;
> the rollback target re-derives from the chain's transition replay with
> an installed-stack snapshot, so a crash in the journal-written /
> pointer-unmoved window re-derives `to` and the rebuilt act dedupes into
> the orphaned entry; the file backend moves journal-then-pointer under
> the chain lock with act dedupe (timestamps excluded) for crash-retry
> convergence — a re-issued or converged move answers `200 {applied:
> false}` without journaling — while the Postgres backend moves in one
> exact transaction (`SELECT … FOR UPDATE` on the pointer row); the
> env-secret master keys are a new `esk-` family minted beside the
> broker's `bmk-` keys under the store root (the broker's custody is
> untouched), and the XChaCha20-Poly1305 envelope construction is
> duplicated from the broker's private functions — one construction, two
> key families; the resolve route carries an explicit `holder` field —
> the tenant is the HTTP trust boundary while the authoritative
> run-binding seam (the run's journaled admission environment answering
> for the holder) is in-process — and a cross-scope request fails closed
> `403 environment_scope_denied`, typed AND journaled (best-effort, the
> broker's denial discipline); the pointer shape ships its canary slot
> and admission's seeded draw reuses `canary_admits`, but the canary-bind
> routes land in wave 4; the environment's gate and approval declarations
> are recorded, not enforced — enforcement wires in wave 4; a revision's
> optional assistant binding is recorded at registration (the assistant
> must exist), not cross-checked against the run's assistant at
> admission; and `MIGRATION_SQL` grows 51 → 55 statements (the two tables
> plus their listing indexes). The `deployment-control` chain the release
> proof names is the wave's evidence chain, one journal per deployment,
> the broker's `broker-control` precedent.

**Wave 4 — release gates, canary, shadow, and health.** The gate seam over
`evaluate_gate` with journaled decisions, canary binding by seeded draw at admission, the
shadow admission context refusing effects above `ReadOnly` with recorded-outcome serving,
and the `/deployments/health` board. Exit: a failing gate refuses a prod promotion with
the decision journaled; a 10% canary binds a reproducible seeded subset of new runs and a
recorded run re-derives its assignment; a shadow run completes with its irreversible
effects refused — typed, classified, journaled as shadow evidence — and its decisions
compared against the recorded world; the health board reports both environments'
pointers, canary state, and last gate decision from journaled data alone.

> **Wave 4 status: implemented (2026-08-11).** The shadow kernel landed in
> `rusty-core/src/effects.rs` (`EffectAdmissionContext::shadow` /
> `serve_shadow`, `EffectViolation::ShadowRefused`, `ShadowRefusal`,
> `ShadowOutcomeSource` — a refusal typed, classified, and served
> from the recorded world when the source journal holds the outcome),
> with `JournalShadowSource` in `replay.rs` and the executor's
> `RunConfig.effect_admission` seam; the wave-4 contracts landed in
> `rusty-core/src/deploy.rs` (the `RevisionGateEvaluator` seam with
> `GateDeclaration` / `GateEvaluation` / `GateVerdict` /
> `GateCheckRecord` / `GateDecisionRecord`, `CanaryDeployment` /
> `CanaryDeclaration` / `CanaryClearance`, the `ShadowRunStarted` /
> `ShadowVerdict` payloads — seven goldens in
> `rusty-core/tests/golden/`), with six additive `RunEventKind`
> variants in `record.rs` (the wave-3 precedent); the server grew
> `list_journals` on both backends, the `with_revision_gate_evaluator`
> config seam, the `evaluate_gate`-backed `EvalRevisionGateEvaluator`
> (`rusty-server/src/gate.rs`), the gate and approval checks wired
> into promote, the canary declare/clear routes,
> `POST /deployments/shadows`, and `GET /deployments/health` —
> exercised end to end by `rusty-server/tests/release_gates.rs` (five
> exit-clause tests) and the release proof in
> `rusty-server/tests/operations_release.rs` — with these settled
> refinements: the gate runs ahead of the approval check on EVERY
> promote attempt and journals each decision, so a refused token
> leaves its allowed gate decision on the chain (attribution of the
> attempt, not just the outcome); a canary into a gated environment
> runs the gate — the gate protects the environment, not the pointer
> slot; the `statistical_power` gate check passes only on
> `StatisticalDecision::NoRegression` — an observed regression blocks
> even when the coarse policy thresholds miss it, and insufficient
> evidence fails closed (the sketch's `!= InsufficientEvidence` read
> would have let a regression through); `GateCheckRecord.metric`
> carries the eval metric's serde JSON form as a string, so the record
> names exactly what the evaluator measured; the approval token scopes
> to the revision's own promotion effect id
> (`revision_promotion_effect_id`) — a token minted for one revision
> admits no other; canary declare and clear converge the promotion way
> (`200 {applied: false}`, nothing journaled — an identical binding
> and an empty slot are states, not errors); a full promotion
> supersedes the canary it graduates from — the slot clears with the
> pointer move, no separate clearance act; the shadow's journal is its
> whole evidence and holds no thread record, so the receipts route
> refuses a shadow run id by construction — shadows never sign
> production receipts; the health board derives pointers, canary
> tallies, and last gate decisions from the journaled chains alone —
> no new store; and the release proof's "manifest digest" hop reads as
> the receipt-signed journal head hash equaling the exported
> snapshot's recomputed `head_hash`.

**Release proof (the whole release).** The roadmap's sentence, automated as an
integration test in the release-proof family (`rusty-server/tests/operations_release.rs`):
**ship a graph change to staging, evaluate it against a recorded dataset, canary it at
10%, and roll back by revision pointer — with every step on the receipt chain.**
Concretely: a revision pinning the changed graph registers and promotes to staging; the
gate seam replays it against a named recorded dataset and journals an allowing
`GateDecision`; the revision binds staging's canary slot at fraction 0.1 and a scripted
burst of new runs splits by the seeded draw — the split reproducible from the journaled
resolutions alone; a run admitted to the canary produces a named artifact whose lineage
walks signed receipt → journal head → `ArtifactCommitted` → `EffectId` → bytes; then the
pointer rolls back byte-exact to the previously serving revision, and the rollback, the
canary, the gate decision, the promotion, and the registration all appear in order on the
`deployment-control` chain, each carrying its author, while the canary run's own signed
receipt still verifies against its bound revision. The test asserts the chain's order and
the walk's hops as facts, not narrative.

## Open questions

Flagged before Wave 1 lands:

1. **Environment re-keying.** Whether promoting tags to first-class records should re-key
   the R0.11 tagged surfaces (`prompt:system@prod`) into environment-scoped foreign keys.
   Leaning: no — environment records are an index and a declaration surface over the
   existing tagged pointers; the tag stays the pointer key, the environment record names
   the same string, no storage migration touches R0.11 data, and the two planes can never
   disagree about which string is an environment.
2. **Run-artifact bytes in replay fixtures.** Externalize by default, or embed small
   ones. Leaning: externalize everything, embed nothing — one rule, verified on load, and
   the 4 KB inline boundary stays a journal-internal concern rather than leaking into
   fixture policy. Replayed control flow never reads the bytes; comparisons opt into
   loading them.
3. **Retention versus receipt coverage.** How long receipt-pinned bytes live when
   receipts themselves have no enforced lifecycle. Leaning: the `receipt_bound` default
   makes bytes outlive their pinning receipts; receipt retention is operator policy
   declared at deployment, not enforced by the sweeper this release; and the journaled
   release act is the only shortening path — a seam declared so an R1.0
   evidence-lifecycle policy lands additively rather than as a migration of deletion
   semantics.
4. **The pin-set freeze.** Frozen candidate ids at revision creation versus per-
   environment resolution at admission. Leaning: frozen, per the evaluability argument in
   the honest edges, with a journaled revision-refresh path so the staleness is an
   explicit act rather than silent drift. The rejected alternative is recorded here so a
   future pain point re-opens it deliberately, not by rediscovery.
5. **Shadow coverage for `Idempotent` effects.** Whether keyed idempotent effects may
   execute in shadows under a shadow-namespaced key. Leaning: not this release — a
   shadow-namespaced key is still a second execution against a provider, and "safe to
   retry under one key" is not "safe to run twice"; recorded-outcome serving covers the
   evaluation need, and the refusal boundary stays simple enough to audit.
6. **Canary blast radius inside one process.** Whether process-level isolation for
   canaried revisions belongs in R0.12 via remote-worker placement. Leaning: deferred —
   the shared-process limit is stated in the honest edges, the seam (worker placement
   policy) is post-R1.0 topology work, and a half-isolation that looks like fault
   containment would be worse than the honest statement.
7. **Preview computation cost.** Synchronous derivation on read versus durable work with
   a cached result. Leaning: synchronous with byte bounds — previews are operator-scale
   reads, a cached preview is a stored derived view (the thing the diff precedent
   refuses), and a media kind that outgrows synchronous derivation joins the not-built
   list until the need is measured rather than assumed.
