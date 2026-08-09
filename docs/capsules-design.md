# Rusty Capsules design (R0.9)

Rusty's Capsules release gives the runtime governed execution of untrusted
code and a federation surface to the agent protocols. The capsule rule,
stated precisely: **no code the runtime does not trust may reach the
filesystem, the network, a secret, the clock, a model, or another tool
unless its manifest declared that reach, a policy permitted it, and the
grant can be shown afterward. Every capability use — and every denied
attempt — is journaled with causal parentage, bounded by a declared
resource budget, and attributable to the exact manifest grant that allowed
or refused it.** What a framework does with a subprocess and a prayer,
Rusty does as a declared, enforced, journaled execution unit whose entire
reach is reconstructable from evidence.

The release has five parts, each composable on its own: the **capsule
manifest** (a content-addressed, golden-pinned declaration of identity,
interface, capabilities, and budgets), the **capability host** (a WASM
Component Model host where imports exist only when granted — deny by
default, structurally), **Cedar policies** (authorization at capsule
admission, grant checks, and tenant overlays that can only narrow),
**signed run receipts** (a cryptographic statement over the hash-chained
journal naming the code, capsules, policies, and permissions that produced
a run's actions), and the **protocol bridges** (MCP server and client, A2A
server and client, with streaming and cancellation preserved in all four
directions). Contracts land first, in `rusty-core/src/capsule.rs` and
`rusty-core/src/receipt.rs` — as with `memory.rs` and `learn.rs` before
them, the core crate, the server, and the SDKs must agree on the shapes
byte-for-byte, and golden-file tests pin them.

## Why this belongs in the runtime

Untrusted code enters an agent system through exactly the seams a framework
cannot see: a tool call, a remote node, an MCP server, a fetched artifact.
At framework level, isolation loses the same three things framework-level
memory lost before R0.8. **Authority is ambient**: a LangGraph tool or a
CrewAI agent executes in the host process with the host's filesystem,
network, and environment — the permission model is the deployment's, and it
is invisible to the code that matters. **Denial is silent**: when a
sandbox does refuse an action, the refusal is a log line if it is anything;
nothing durable records which grant was absent, so an operator cannot
answer "what did this agent try to reach?" after the fact. **Budgets are
advisory**: wall-time limits and memory caps enforced outside the execution
record cannot be replayed, so a run that burned 40 seconds inside a 30
second budget leaves evidence of the 40, not of the bound that was
supposed to stop it. The runtime already holds every piece needed to do
better — a hash-chained journal that records every effect
(`rusty-core/src/journal.rs`), a typed effect kernel with deterministic
effect ids and approval boundaries (`rusty-core/src/effects.rs`), a
content-addressed manifest that already reserves a slot for capsule version
pins (`RunManifest::capsules`, `rusty-core/src/record.rs`), durable tasks
with leases, quotas, and budgets (`rusty-server/src/tasks.rs`,
`rusty-core/src/durable.rs`), and a WASM sandbox that already meters fuel
and caps memory (`rusty-core/src/wasm_node.rs`) — so R0.9 builds isolation
and federation where those primitives live, the same argument that put
memory and learning in the runtime rather than in application code.

## Lineage, named

Rusty Capsules stands on established work, and says so:

- **Object capabilities** (Miller, "Robust Composition: Towards a Unified
  Approach to Access Control and Concurrency Control," 2000; Mark Miller's
  earlier E work) — authority as unforgeable references rather than
  identity-checked permissions: holding the capability *is* the permission,
  and a component that was never handed the secret-store capability cannot
  reach secrets no matter what it asks for, because there is no ambient
  name to ask for. The capability host's import-as-grant model is this
  idea: an import that does not exist in the component's world is not a
  permission that was checked and refused — it is a door that was never
  built. Deny by default stops being a policy stance and becomes a
  structural fact.
- **WASI and the WASM Component Model** (the Bytecode Alliance;
  component-model.bytecodealliance.org) — capability-based system access
  for sandboxed modules. WASI's design moved POSIX-style ambient authority
  into explicitly passed handles; the Component Model's WIT **worlds**
  make the whole interface a declaration: a world names exactly the
  imports a component receives and the exports it provides, and the host
  chooses which world to instantiate against. Our manifest's declared
  interface *is* a WIT world reference, and capability grants map onto
  which of the world's imports the host links. Where we are uncertain
  about toolchain specifics, this document states design intent, not API
  fact — the Component Model and `wit-bindgen` are still moving, and open
  question 1 owns the version-cadence decision.
- **Wasmtime's resource governance** (docs.wasmtime.dev) — fuel metering
  (deterministic instruction accounting; `wasm_node.rs` already consumes
  it), epoch interruption (a host-deadline mechanism that preempts a guest
  that stops yielding, which `wasm_node.rs` does not yet use), and
  `ResourceLimiter` (memory and table growth caps, already implemented as
  `StoreLimits`). R0.9 composes all three rather than inventing a
  scheduler: fuel is the CPU budget, epochs are the wall-time budget's
  enforcement arm, the limiter is the memory budget.
- **Deno's permission model** — per-invocation capability flags
  (`--allow-net=api.example.com`, `--allow-read=/data`) demonstrating that
  host/protocol/method-scoped grants are the right granularity for
  developer-facing isolation, and that the grant must be declared by the
  party that runs the code, not the code itself. Our network grant shape —
  hosts, protocols, methods — follows Deno's scoping directly.
- **E2B / Firecracker microVMs** — isolation by machine boundary. We
  adopt the threat model (untrusted tenant code must not share a kernel
  with the control plane) and reject the cost model for the common case:
  microVM cold starts are measured in hundreds of milliseconds and the
  boundary crosses an entire machine — kernel, init, network stack — when
  the untrusted unit is usually a single node invocation. MicroVMs remain
  the right answer for hostile multi-tenant *processes*; capsules are the
  answer for untrusted *invocations*, at per-call cost. The two compose:
  nothing here stops a deployment from running the whole server inside a
  microVM.
- **MCP's ambient-authority posture** (the Model Context Protocol
  ecosystem as shipped) — MCP servers today run as local processes with
  the user's full authority, or as remote endpoints trusted on the strength
  of a URL. Rusty's MCP bridges do not fix that ecosystem fact; they make
  Rusty's own exposure governed (a graph exposed as an MCP tool runs under
  its declared manifest and budget) and Rusty's consumption durable (an
  MCP call is a journaled, idempotency-keyed effect). The bridge section
  is honest about which half of the problem is ours.
- **Cedar** (AWS's open-source authorization language; the `cedar-policy`
  crate) — policy-as-data authorization with a formal semantics, used here
  for capsule admission, capability-grant checks, and tenant-overlay
  legality. Cedar's entity hierarchy maps naturally onto tenant →
  capsule → capability, and its analysis tooling (symmetry checking,
  policy comparison) is the credible path to answering "can this overlay
  ever widen?" as a verification question rather than a code review
  question. The honest edge — Cedar evaluates static policies and cannot
  un-happen a revoked grant — is drawn in the Cedar section.
- **Sigstore and transparency logs** (sigstore.dev; the Certificate
  Transparency lineage before it) — signing as witnessed, append-only
  public evidence. R0.9 adopts the *signing* half locally (a run receipt
  over the journal head) and defers the *witnessing* half (transparency
  log, KMS, remote attestation) to R1.0+, per the roadmap's own
  "signing/attestation follows the MVP" ordering. The receipt format is
  designed so a transparency log can witness it later without a shape
  change.

## What Rusty does differently

Two things, both consequences of what already shipped.

1. **Capsules are not plugins.** A plugin system extends the host's
   capability; a capsule system *bounds* the guest's. The manifest
   declares capabilities, budgets, and effect classes before any code
   runs; the host enforces the declaration structurally (unlinked imports,
   not runtime checks alone); the journal records every use and every
   denial; and the R0.7 `RunManifest` capsule pins resolve to
   content-addressed manifest digests, so a checkpoint can answer "which
   exact build of which capsule, under which grants, produced this
   state?" A plugin registry cannot answer that question because it never
   asked it.
2. **Deny by default is auditable, not aspirational.** Every runtime
   claims deny-by-default; the claim is usually a README. In Rusty the
   claim is checkable because the evidence plane predates the isolation
   plane: a denied capability attempt is a journaled `RunEvent` naming
   the capsule, the requested capability, and the manifest grant that was
   absent — replayable, hash-chained, and covered by the run's signed
   receipt. The release proof is a denial you can *show*, not a denial
   you can *assert*. That is the sequencing rule of the roadmap applied
   once more: replay and evidence landed in R0.5–R0.7 precisely so that
   R0.9's security claims could be tested rather than stated.

## The capsule manifest (`rusty-core/src/capsule.rs`)

One serde-versioned struct, `CapsuleManifest`, additive-evolution only,
golden pinned under `rusty-core/tests/golden/` — the same discipline as
`MemoryRecord` and `Candidate`. The manifest is **content-addressed**:
`CapsuleId` is `sha256_hex` over the canonical serialization of the
manifest's content (the one hashing primitive shared with artifact
references, journal heads, and candidate ids), so identity is integrity —
two builds of the same declaration converge on one id, and a tampered
manifest fails its own address. Every field exists because a downstream
enforcement point needs it:

- `identity` — capsule name plus human-facing metadata. Not the address;
  the address is derived, never minted.
- `version` — the exact version string, the value the R0.7
  `CapsuleVersion` placeholder already pins in `RunManifest::capsules`.
  R0.9 makes the placeholder real without breaking the wire: the
  checkpoint header keeps pinning the version *string* (additive, no
  migration), and the server's capsule registry resolves
  `(identity, version)` → `CapsuleId` at admission, journaling the
  resolution (the manifest digest) into the run so the full chain —
  header pin → journaled resolution → receipt — reaches the content
  address. Typing the placeholder in R0.7 localizes this evolution to the
  registry instead of every manifest consumer, exactly as its docs
  promised.
- `build_digest` — SHA-256 of the guest artifact (the `.wasm` component
  bytes). Admission recomputes it; a manifest naming bytes it was not
  built from does not load. The digest, not the version string, is what
  the host caches compiled modules under.
- `interface` — the declared graph/node interface as a **WIT world
  reference** (`rusty:capsule/world@x.y.z`): the world version the
  component was built against, plus the typed inputs/outputs the world
  exports. World versions are additive — a new world adds imports or
  tightens types; old worlds keep instantiating — the same evolution rule
  the serde contracts follow. R0.9 ships exactly one world version (open
  question 6).
- `effects` — the closed `Effect` classes (`rusty-core/src/record.rs`)
  the capsule may produce. The taxonomy's own docs have reserved this
  consumer since R0.5 ("Capsules (R0.9): which effects a sandboxed
  capsule may perform at all under its capability grants"). A capsule
  whose declared classes top out at `ReadOnly` is refused at admission if
  its requested capability grants imply writes — the declaration and the
  grants must agree, and the host enforces the stricter of the two.
- `capabilities` — a `BTreeSet<CapabilityGrant>`, closed enum
  `CapabilityGrant`:

  | Grant variant | Carries | Host meaning |
  |---|---|---|
  | `filesystem` | path prefixes, mode (`read` / `read_write`) | WASI-style preopened directories, scoped to the prefixes; nothing else on the filesystem exists for the guest |
  | `network` | hostnames, protocols, HTTP methods | outbound calls through a host-side connector that matches host + protocol + method before any socket opens |
  | `secret` | secret *handles* (names in the server's secret store) | the guest receives opaque, non-serializable handle tokens; the host resolves them at use and the bytes never enter guest linear memory |
  | `tool` | tool names in the run's `ToolRegistry` | the guest's `tool-call` import is linked, dispatching through the host's tool executor — which means the effect kernel's admission path applies to capsule tool calls unchanged |
  | `model` | model names the deployment serves | the guest's `model-call` import is linked; usage accrues to the capsule's budget |

  The set is the whole reach. A manifest with an empty `capabilities`
  set — the default — describes a pure-compute guest, which is precisely
  what `wasm_node.rs`'s ABI v0 already executes; nothing regresses.
- `budget` — a `ResourceBudget`: `fuel` (CPU, Wasmtime fuel units),
  `max_memory_bytes` (linear-memory cap), `wall_time_ms` (enforced by
  epoch interruption), `max_tokens` and `max_cost_usd` (model usage,
  evidence-grade `f64` matching `AgentBudget`), `max_output_bytes` (the
  guest's result payload, checked before the host accepts it). Every
  field optional on the wire; `None` means the run's own budget bounds
  apply — never an invented default, per the `AgentBudget` convention.

**Signing follows the MVP, per the roadmap.** The R0.9 manifest is
content-addressed but not yet signed: the digest proves integrity against
the registry, not provenance against an author. Manifest signing and
attestation are deliberately deferred (see the not-built list); the run
receipt (below) is where R0.9's signing budget is spent, because the
receipt covers the manifest digests transitively through the run manifest.

## The capability host (`rusty-core/src/capsule_host.rs`, feature `wasm`)

The honest starting point: **wasmtime is already here.** `rusty-core`
carries `wasmtime = "47"` behind the optional `wasm` feature, and
`wasm_node.rs` runs core-WASM guests with fuel metering, a
`ResourceLimiter` memory cap, and an empty `Linker` — no WASI, no host
functions, no ambient authority. R0.9 does not introduce a WASM
dependency; it upgrades the sandbox from "no imports at all" to "imports
that exist only when granted," which is a strictly harder problem and the
one the Component Model exists for. `wasm_node.rs` and its ABI v0 stay
untouched — pure-compute guests with no imports remain the fastest, most
portable path for trusted-but-isolated compute — and capsules are a new
host beside it. There is no forced migration.

**Deny by default, structurally.** A capsule instantiates against its
manifest's WIT world, and the host links exactly the imports the granted
capabilities name. A component built without the `secret-store` import
cannot reach secrets even in-process: there is no symbol to call, no
handle to forge, no ambient authority to discover — the object-capability
property, enforced by the linker rather than by a check that could be
skipped. Grants narrower than the world (a `network` grant naming one
hostname) are enforced inside the host's import implementation: the
import exists, but the host-side connector matches host, protocol, and
method before opening anything, and a mismatch is a denial.

**Denials are evidence.** A denied capability attempt — an unlinked
import the guest probes for, a granted import used outside its scope — is
journaled as a new additive `RunEventKind::CapsuleDenied` (the same
evolution rule `MemoryRead` through `CandidateRolledBack` followed; old
journals keep deserializing). The payload names the capsule id, the
requested capability, and **the manifest grant that was absent** — the
denial is attributable to a declaration, not to a stack trace. This is
the release proof's "visibly denied" made structural.

**Guest outputs are validated before host actions.** The guest's result
is deserialized and checked against the declared output types (the WIT
export signature, surfaced as JSON Schema at the Rust boundary — the same
draft-2020-12 dialect `ArtifactContract::schema` pinned in R0.7) before
any host action it requests is performed, and `max_output_bytes` is
enforced at the same gate. An untrusted component's output is input to
the host; the runtime treats it exactly as it treats a worker's
`NodeTaskResponse` — a claim to validate, never a structure to trust.

**Resource governance composes three wasmtime mechanisms and two shipped
budget systems.** Fuel is the CPU budget (deterministic, replay-stable);
epoch interruption is the wall-time budget's enforcement arm (the host
bumps the epoch on deadline; a guest that stops yielding is preempted —
`wasm_node.rs`'s fuel-only model cannot express this, which is why
capsules need the new host); `StoreLimits`' memory cap carries over
unchanged. Upward, the capsule's `ResourceBudget` is clamped at admission
to the *minimum* of what the manifest declares and what the enclosing
scope permits — the run's `TaskBudget`/`AgentBudget`
(`rusty-core/src/durable.rs`, `rusty-core/src/agents.rs`) and the R0.6
tenant quotas (`rusty-server/src/tasks.rs`). One honest note: the
roadmap's R0.7 "RunBudgets with inheritance" shipped as `TaskBudget`
(per-task attempts/timeouts) and `AgentBudget` (tokens/cost/deadline
across turns) rather than as a single named `RunBudget` type; capsules
inherit through those two objects plus pool/tenant quotas, and this
document names the composition rather than pretending the roadmap's type
exists. Budget breaches terminate the capsule invocation and journal the
breach (the budget that bit, the consumption at breach) — a budget that
cannot be shown in evidence is advisory, per the opening argument.

**Secret handles.** A `secret` grant gives the guest an opaque handle —
a token meaningful only to the host's import implementation, carrying no
bytes, redacted in `Debug`, never serialized into guest linear memory.
The host resolves handle → secret at the moment of use (inside the
host-side network connector, so a granted HTTP call can be authenticated
without the guest ever holding the credential), and the resolution is
journaled as metadata — handle name, never value. A crashed or hostile
guest's dumped memory contains tokens that outlive the invocation by
nothing.

## Cedar policies (`rusty-server/src/capsule_policy.rs`, feature `capsules`)

Three decisions need authorization, all at the server:

1. **Capsule admission** — may this tenant load this capsule (identity,
   build digest) at all?
2. **Grant checks** — does policy permit *these* capability grants for
   this tenant? A manifest is the guest's declaration; Cedar is the
   deployment's verdict on the declaration. A capsule may declare fewer
   grants than policy permits (narrowing is always safe); it may not run
   with grants policy forbids, no matter what its manifest says.
3. **Tenant overlays** — an operator attaches an overlay to a tenant that
   *further* restricts what capsules in that tenant may do. The
   roadmap's rule — overlays may only narrow — is enforced **twice, and
   the structural half comes first**: the effective capability set is
   computed as manifest grants ∩ overlay, a set intersection that
   mechanically cannot add a grant; Cedar then decides whether the
   overlay itself may be applied (who may author it, against which
   capsules). Policy decides legality; arithmetic decides narrowing. No
   code path computes union.

Cedar is the engine because the questions are static and relational —
tenant → capsule → capability is an entity hierarchy, and "can overlay B
ever widen manifest A?" is a policy-analysis question Cedar's tooling was
built to answer. Policies are operator-authored `.cedar` files loaded
through server config, **versioned on both store backends** per the
established convention (`{store}/capsule_policies/` on the JSON backend,
a column-mapped `server_capsule_policies` table on Postgres), and the
active policy version is pinned into every capsule admission event and
every run receipt — the policy plane's epoch-binding discipline applied
to authorization instead of executor decisions.

**The honest edge, stated plainly.** Cedar evaluates static policies. It
cannot make a revoked grant retroactively un-happen: a capsule admitted
an hour ago under a policy version that has since changed is still
running under the grants it was admitted with. R0.9's answer is
composition, not pretense — revocation takes effect at the *next
capability use*: host-side import implementations re-check the live
policy (or a policy version bounded by a short documented cache epoch) on
each granted call, the same way the effect kernel's admission checks run
at execution time rather than at token-mint time. In-flight invocations
keep their wall-time and fuel budgets regardless — a revocation that
cannot interrupt a running capsule still bounds how long it can run.
Cross-process and cross-restart attestation of *why* a capsule was
admitted is the receipt's job; making the receipt says "policy version P
permitted this" is what closes the loop for audit.

**Dependency call, made and justified.** `cedar-policy` joins
`rusty-agent-server` (not core): authorization is a server concern — the core
crate's contracts stay engine-free, the way `CandidateEvaluator` is a
trait core owns and the server implements. It lands behind a
`rusty-agent-server` cargo feature `capsules` (which also enables
`rusty-agent-runtime/wasm`), and a server built without the feature
**refuses capsule workloads at admission** — fail closed, not silent
skip. The build-cost honesty: wasmtime with cranelift is the heavy
dependency (it dominates clean-build time; it is why the feature gate
predates this design), and `cedar-policy` is pure Rust with no native
deps — noticeable but not structural. The alternative considered — a new
`rusty-capsules` crate owning the host — is rejected: the host needs the
journal, the effect kernel, and the node/executor seam (all core), and
splitting the manifest contract (`capsule.rs`, core, golden-pinned) from
its enforcement across a crate boundary buys a marginally faster default
build at the price of the contract/enforcement split R0.8's
`CandidateEvaluator` seam exists specifically to avoid. Core owns
contracts and the host; server owns authorization and admission; the
feature gates protect build times. That is the call.

## Signed run receipts (`rusty-core/src/receipt.rs`)

The R0.7 effect kernel left one seam explicitly open: `ApprovalToken`'s
docs name cross-process attestation "R0.9's signed-receipt work." This is
that work. A `RunReceipt` (serde-versioned, golden pinned) is a signed
statement over evidence that already exists:

- `run_id` and the **journal head hash** — the SHA-256 chain head
  (`JournalRef`) every checkpoint already stamps. Signing the head signs
  every event in the chain transitively; that is what hash chains are
  for.
- The **run manifest digests** — prompt hashes, tool schema hashes,
  model + parameters, memory schema, and now the resolved capsule
  `CapsuleId`s (content addresses, not version strings).
- The **effect receipts ledger** — the digests of the run's
  `EffectReceipt`s (provider, provider id, idempotency key, effect id),
  so the receipt covers what the run *did to the world*, not just what it
  computed.
- The **policy versions** — the executor `PolicyVersion` pinned in every
  checkpoint header, plus the Cedar policy versions under which capsules
  were admitted.
- The **denials ledger** — the `CapsuleDenied` event ids. A receipt over
  a run that attempted forbidden access says so; the visibility of the
  denial survives into the signed statement.
- `signer` (a key id) and `signature` over the canonical serialization
  of all of the above.

**Key management for v1, honestly scoped.** Ed25519 (`ed25519-dalek`, a
new core dependency — small, pure-Rust, no native code), one keypair per
server deployment, generated on first boot under `{store}/keys/` with
filesystem permissions documented; rotation is a documented operation
that journals the new key id. This is *local signing with local keys* —
it proves integrity and origin against a key the operator holds, and no
more. KMS integration, remote attestation, and transparency-log
witnessing (the Sigstore half) are R1.0+; the receipt's canonical form is
the exact byte string a transparency log would witness, so that
integration lands additively. **Verification API**:
`verify_receipt(&JournalSnapshot, &RunReceipt, &PublicKey) -> Result<VerifiedRun>`
in core (recompute the head over the snapshot's event chain, recompute
the manifest and ledger digests, check the signature), plus
`GET /runs/{id}/receipt` and `POST /receipts/verify` on the server. The
verification walks the same digests the journal already computes; it adds
a signature check, not a new evidence pipeline.

**What the receipt proves, and what it does not.** It proves which code
(graph version and hash), which capsule builds, which memory schema,
which policies, and which permissions produced this run's actions — what
Rusty received, authorized, and executed, with the denials attached. It
does **not** prove that an external LLM's answer was truthful, that a
tool's provider behaved honestly, or that a remote A2A agent did what it
claimed; those are claims about systems whose journals Rusty does not
hold, and a signature over Rusty's evidence cannot witness them. The
receipt is a statement about *this runtime's* conduct, and the document
says that plainly wherever receipts are described.

## Protocol bridges

The roadmap's protocol-native principle: interoperate across languages
and vendors through MCP and A2A; Rusty is the durable runtime underneath.
Four directions, each preserving the two properties the runtime owns —
durability (journaled, replayable) and lifecycle control (streaming,
cancellation) — and each honest about which trust problem it does *not*
solve. Bridges multiply the untrusted surface, which is why they land
after the host: an MCP server exposed by a runtime that cannot isolate
what it runs is a liability with a schema.

### MCP server bridge (`rusty-server/src/mcp_bridge.rs`)

Expose any registered assistant/graph as an MCP tool. The tool's JSON
schema is **generated from the graph's typed IO** (the state channels and
their schemas, the same source the server API documents), not
hand-written; a graph whose IO changes changes its exposed schema, which
is drift the manifest/receipt plane can see. A `tools/call` submits a
background run; progress streams back as MCP progress notifications over
the transport's stream, sourced from the server's existing SSE event
stream; MCP cancellation maps onto the server's run cancel (the
`CancellationToken` thread that R0.6 wave 2c established), so a client
that hangs up actually stops the work — leased tasks see the
`cancel_requested` heartbeat hint, in-flight guests hit their epoch.
Transport: Streamable HTTP on the axum server (open question 4).

### MCP client — the existing module, made durable (`rusty-core/src/mcp.rs`)

`mcp.rs` already ships: JSON-RPC framing, stdio transport
(`McpStdioClient::spawn`), `McpToolAdapter` adapting MCP tools into
Rusty's `Tool` trait, frame caps for hostile peers. R0.9 changes its
*evidence posture*, not its protocol surface: a journaled wrapper
(`JournaledMcpTool`, additive) records each call as an effect with a
derived idempotency key (`derive_effect_id` over run scope, tool name,
canonical arguments), so exact replay serves the journaled response
instead of re-calling the server — and a replayed run never respawns a
side-effecting stdio server. The honest edge: stdio child processes are
replay-hostile by nature, and the answer is the same one the Flight
Recorder already gives for model calls — the journal is the replay
source; the transport is live-only.

### A2A server (`rusty-server/src/a2a.rs`)

Expose Rusty agents as A2A agents. The **Agent Card is generated** from
the assistant registry plus the capability manifest — name, skills (the
graph's declared IO), endpoint — never hand-maintained, because a Card
that drifts from the runtime it describes is a protocol-level lie.
Inbound A2A tasks map onto R0.6 durable tasks (leased, retried under the
`ErrorClass` taxonomy, quota-counted); A2A artifacts map onto the
content-addressed artifact store; streaming maps onto SSE; cancellation
maps onto task cancel. An A2A peer gets Rusty's durability without
knowing Rusty exists — the protocol-native stance, server side.

### A2A client (`rusty-core/src/a2a.rs`)

Consume remote A2A agents as durable nodes, shaped after `remote.rs`:
one `Node` trait, the remote agent behind it. An `A2aNode` submits a
task to the remote agent and journals the call as an effect (task id as
the idempotency handle); the remote task's terminal state and artifacts
land in the journal as the node's output, so replay serves the recorded
outcome. Cancellation propagates: the run's `CancellationToken` cancels
the remote A2A task. The trust posture is stated, not solved: the remote
agent's own conduct is *its* receipt's problem — Rusty journals what it
sent and what came back, and the signed receipt names the endpoint.

## Composition with the shipped systems

Six systems, one system seen from six sides:

- **Flight Recorder.** Capability uses and denials are journaled
  `RunEvent`s with causal parentage (additive `CapsuleDenied`; capsule
  resolution journaled at admission). The receipt signs the journal head
  — evidence first, signature second. Exact replay serves journaled
  capsule outputs and journaled MCP/A2A calls, so a sandboxed run is as
  replayable as a native one — the property that makes "the untrusted
  agent did X" a replayable claim rather than a forensic reconstruction.
- **Effect kernel.** Capsule tool calls dispatch through the host's tool
  executor, so effect admission (`derive_effect_id`, `ApprovalToken`,
  the receipt ledger) applies to guest-initiated effects unchanged —
  the guest cannot reach around the kernel because the kernel is on the
  host side of the import. The manifest's declared effect classes bound
  what the capsule may even request; the kernel governs what executes.
- **Durable Work.** Capsule invocations of consequence run as durable
  tasks — leased, retried, dead-lettered with evidence, quota-counted
  per tenant; A2A inbound tasks *are* R0.6 tasks. Budget composition:
  capsule `ResourceBudget` ≤ task/agent budgets ≤ tenant quotas, clamped
  at admission, never widened in flight.
- **Agent Fabric.** `CapabilityManifest` (what an agent may do: scopes,
  budgets, message kinds) and `CapsuleManifest` (what untrusted code may
  reach: capabilities, budgets, effects) are the same idea at two trust
  levels — declaration before execution, checked at every access. Tenant
  overlays narrow both. Supervision applies: a capsule that traps past
  its restart budget escalates through the same mailbox path as any
  failing agent turn.
- **Rusty Learn.** `learn.rs`'s docs already name this release: the
  `tool_permission` candidate kind's *grant mechanics* — which
  capabilities a narrowed grant drops, how a widened one is bounded —
  are R0.9's capsule-manifest work. The mechanics land as the
  `CapabilityGrant` set operations (narrow = subset, journaled through
  the candidate pipeline; widen = policy-checked at admission, and an
  overlay can still only narrow). A promoted `policy` candidate's
  executor-policy parameters may tighten default capsule budgets (fuel,
  wall time) for new admissions — overlays interact with the policy
  plane as *budget defaults*, never as capability grants.
- **wasm_node (the shipped sandbox).** ABI v0 pure-compute guests keep
  working unchanged; capsules subsume the untrusted-with-capabilities
  case. One codebase, two trust postures, no migration.

## What R0.9 deliberately does NOT build

- **No capsule marketplace or registry service.** The roadmap de-prioritizes
  an agent marketplace until signed capsules exist; R0.9 builds the signed
  *evidence* (receipts over manifest digests), not the distribution
  surface. Distribution is content-addressed blobs through the existing
  artifact store plus a server-side capsule registry mapping
  `(identity, version) → CapsuleId`.
- **No manifest signing or attestation.** The roadmap says it:
  "signing/attestation follows the MVP." The manifest is content-addressed
  (integrity), not signed (provenance); run receipts are R0.9's signing
  surface, and manifest provenance joins it in R1.0+.
- **No transparency log, KMS, or remote attestation / TEE.** Local
  Ed25519 keys, documented. The receipt format is built so a Sigstore-style
  witness lands additively; hardware attestation is a different threat
  model and is not promised.
- **No guest authoring toolchain.** Guests are opaque components built by
  whatever the ecosystem provides (`wit-bindgen` today); Rusty ships the
  manifest, the WIT world, and one in-tree reference guest (Rust) for
  tests. Teaching the world to compile to components is not the runtime's
  job.
- **No graph-topology capsules.** Topology stays code, pinned by
  `graph_hash` — a capsule is a *node-level* execution unit. A capsule
  that rewires the graph is the self-modifying-graph pattern R0.8 already
  refused, wearing a sandbox costume.
- **No capability-widening overlays.** Structurally impossible (effective
  grants = manifest ∩ overlay), and also not offered as an API shape.
- **No capsule path for first-party Rust nodes.** Trusted code keeps
  running natively at native speed; capsules are for untrusted,
  third-party, or untrusted-tenant code. The honest reason: isolation
  costs fuel accounting, serialization across the boundary, and a world
  definition per interface — paying that for code you already trust buys
  nothing, and pretending every node should be a capsule would tax the
  common case to uniformize the rare one. Isolation begins where trust
  ends, not before.
- **No sandboxing of MCP servers Rusty spawns.** The stdio MCP client
  runs the server as a child process with ambient authority — the
  ecosystem's posture, honestly named. Sandboxed MCP consumption (run the
  server *as a capsule*) is a credible R1.0 composition and is not
  claimed here.

## Wave plan and release proof

Ordering rule: **isolation before bridges, evidence before signatures.**
Bridges multiply the untrusted surface, so the capability host must land
first; receipts sign evidence, so the journaled denials must exist before
the signature has something to say. Four waves.

**Wave 1 — manifest contract and capability host MVP.**
`rusty-core/src/capsule.rs` (`CapsuleManifest`, `CapsuleId`,
`CapabilityGrant`, `ResourceBudget`, the WIT world reference) with golden
files; `capsule_host.rs` behind `wasm` — component instantiation against
the declared world, import linking from grants, fuel + memory + epoch
governance, output validation, journaled denials
(`RunEventKind::CapsuleDenied`); the registry resolving `CapsuleVersion`
pins to `CapsuleId`s and journaling the resolution. Exit: a guest whose
manifest grants no capabilities cannot perform I/O of any kind (the
import does not exist); fuel, memory, and wall-time limits each abort a
planted misbehaving guest; a scoped-grant violation journals a
`CapsuleDenied` naming the absent grant.

> **Wave 1 status: implemented.** The manifest contract landed as
> written (`CapsuleManifest` / `CapsuleId` / `CapsuleIdentity` /
> `CapsuleInterface` / `CapabilityGrant` / `ResourceBudget` /
> `derive_capsule_id`, with goldens in `rusty-core/tests/golden/`), the
> capability host landed behind `wasm` (structural import gating, grant-
> scoped linking, fuel + memory + epoch governance, the output gate,
> journaled uses and denials), and the server registry resolves
> `(name, version)` pins to content addresses over both store backends
> (`POST /capsules`, `GET /capsules[{/id}]`, `POST /capsules/resolve`),
> journaling one resolution per pin. Five additive refinements worth
> naming. **Three event kinds joined `RunEventKind`, not one**:
> `capsule_resolved` / `capsule_call` / `capsule_denied` — the design
> named the denial and "journaled the resolution" without naming their
> kinds, and a granted use needs its own evidence (the memory plane's
> read/write precedent). **A `Clock` grant variant exists**: the opening
> rule names the clock as governed I/O, but the grant table predates it —
> `clock.now-millis` is granted and journaled like any other capability.
> **The v1 world is narrower than the table above**: only
> `rusty:capsule/net@0.1.0` (`fetch`) and `rusty:capsule/clock@0.1.0`
> (`now-millis`) are importable this wave; filesystem, secret, tool, and
> model grants are contract-only (valid in manifests, nothing links them
> yet) — narrowing the v1 surface beat fighting wasmtime for imports no
> wave-1 exit criterion exercises. **Output-schema validation is
> declared-but-pinned, not enforced** (the `ArtifactContract` precedent):
> the output gate enforces `max_output_bytes` and well-formed JSON; the
> optional `output_schema` travels in the manifest for a later wave's
> validator. **No `Cargo.toml` change was needed**: wasmtime 47's default
> features already include `component-model` and `wat`, so the reference
> guests are hand-written component WAT compiled by wasmtime itself — no
> guest toolchain (`wit-bindgen`, `cargo-component`) builds or tests this
> wave, exactly the design's stance. Two implementation notes: wall-time
> enforcement is a per-host ticker thread bumping the engine epoch every
> 5 ms with the deadline expressed in ticks (started on first use,
> stopped on drop), and the network connector is a deployment seam
> (`NetworkConnector` trait) with no default implementation — a host
> with a granted network capability but no configured connector fails
> closed at invocation.

**Wave 2 — Cedar, tenant overlays, budget composition.** `cedar-policy`
behind the server's `capsules` feature; admission and grant checks;
overlay authoring with structural narrowing; policy versioning on both
store backends; budget clamping against task/agent budgets and tenant
quotas; revocation-at-next-use re-checks. Exit: an overlay that attempts
to widen a manifest's grants is refused (Cedar) and, when hand-crafted
past policy, still cannot widen (intersection); a revoked grant fails at
the capsule's next capability use with the denial journaled against the
new policy version; a capsule whose declared budget exceeds the run's
budget is clamped or refused at admission and the clamp is journaled.

> **Wave 2 status: implemented.** The plane landed as written:
> `cedar-policy` behind the server's `capsules` feature (which also
> enables core's `wasm` host), the three decisions evaluated as typed
> Cedar requests (`AdmitCapsule`, `UseCapability` — one per declared
> grant, `AttachOverlay` — with the computed `widens` signal in
> context), policy versioning on both store backends (`POST
> /capsule_policies/versions[{/version}]`, `GET/POST
> /capsule_policies/active`), tenant overlays (`POST/GET
> /capsules/overlays[{/name}]`) narrowing every resolution's effective
> grants by structural intersection whether or not Cedar spoke, budget
> composition against the run's budget and the tenant ceiling
> (`ServerConfig::with_capsule_budget_ceiling`), and revocation at the
> next use through core's `GrantRecheck` seam served by the server's
> `CapsulePolicyPlane` (public — the server has no invocation route, so
> embedders building `CapsuleHost`s plug `plane.rechecker(tenant)` into
> them). Refinements worth naming. **`cedar-policy` is pinned to v4**
> (resolved 4.12.0): it requires rustc ≥ 1.89 while the workspace
> declares 1.86, so the `capsules` feature raises the effective floor
> for feature-enabled builds only — default builds are untouched. **No
> Cedar schema**: every request is built by typed constructors from
> per-request JSON entities, so schema checking has no untrusted input
> to bite on; the operator's policy text is the only free-form input,
> parse-checked at registration. **The unconfigured posture is
> deliberate**: a tenant with no active policy admits capsules the
> wave-1 way (upgraded registries must not brick); enforcement begins
> per tenant at the first activation. A build without the feature is the
> opposite posture — every wave-2 route refuses with the typed `503
> capsule_policy_unavailable`. **The budget split is clamp-vs-refuse**:
> fuel, memory, wall time, and output bytes clamp (enforcement-local
> resources the host bounds regardless) and the clamp is journaled on
> the resolution; declared `max_tokens` / `max_cost_usd` exceeding the
> tightest enclosing bound refuse `422` — accounting axes cannot be
> retrofitted mid-run. **The active pointer keeps no history** (unlike
> the executor plane's append-only activation log): one pointer file per
> tenant on the JSON backend, a two-statement transaction flipping an
> `active` column on Postgres — which version decided each admission is
> pinned on the admission events themselves. **The revocation cache
> epoch is honest**: in-process, a revocation is effective as soon as
> the activating request completes (mutations refresh eagerly, admission
> installs what it decided under); across processes sharing one store it
> lands at the next restart, with a best-effort startup preload. Two
> contract notes: `CapsuleResolution` gained four additive optional
> fields (`policy_version`, `overlays`, `effective_grants`,
> `clamped_budget` — serde-skipped when absent, so wave-1 goldens and
> journals are unchanged), and `CapsuleDenial` gained an optional
> `policy_version`, present on authorization refusals and absent on
> wave-1 scope denials.

**Wave 3 — signed receipts.** `rusty-core/src/receipt.rs`
(`RunReceipt`, `verify_receipt`) with goldens; Ed25519 key lifecycle
(generate, rotate, journal); server endpoints; receipt coverage of
journal head, manifest digests including resolved capsule ids, effect
ledger, policy versions, denials. Exit: a receipt verifies against the
run's exported `JournalSnapshot`; flipping one byte in any journaled
event fails verification; rotating the signing key is journaled and old
receipts still verify against the key history.

> **Wave 3 status: implemented.** The plane landed as written:
> `RunReceipt` (serde-versioned, golden-pinned with its canonical form
> and key id in `rusty-core/tests/golden/`) signs the journal head, the
> manifest digests, the resolved capsule content addresses, the effect
> and denials ledgers, and the policy versions; `verify_receipt`
> recomputes the head with the journal's own chain step and answers a
> typed `VerifiedRun` or a `ReceiptRejection` naming the mismatched
> component; the server mints (`GET /runs/{id}/receipt`), verifies
> (`POST /receipts/verify`), and runs the key lifecycle (`GET
> /receipt_keys`, `POST /receipt_keys/rotate`, `GET
> /receipt_keys/journal`). Refinements worth naming. **Mint semantics are
> mint-on-first-read, then stored-and-served**: the receipt is minted
> over the run's reverified persisted journal on first request and
> replaced when the journal's head advances — minting at completion
> would put signing in the runner's hot path for runs nobody audits, and
> the head's event count already says exactly which journal state the
> signature covers. **The manifest and executor policy are read back
> from the run's last checkpoint header** (the journal does not hold
> them): they are carried, signature-covered evidence, and the receipt
> carries the manifest in full plus its `manifest_digest` commitment, so
> tampering with either half fails verification naming
> `manifest_digest`. **One event kind joined `RunEventKind`**:
> `signing_key_rotated` — the design named the rotation journaling
> without naming the kind, and the deployment's receipts journal (run id
> `receipt-keys`, the supervision-journal precedent applied to the
> control plane) records genesis too (`previous_key_id` absent), so the
> lineage is complete, not just the rotations. **Key ids are full
> content addresses** — sha256 of the public key bytes, the capsule-id
> convention — so "which key signed what" never depends on a registry's
> say-so. **Secrets never enter the store abstraction**: on both
> backends the secret lives at `{store_path}/keys/{key_id}.secret`
> (hex, `0600` from the first byte, written once per key id); the store
> holds only public history (`{store_path}/keys/{key_id}.json` files,
> the `server_receipt_keys` table), so the Postgres backend cannot hold
> what a database must not leak. **Generation draws OS entropy through
> `uuid`** (two v4 draws of `getrandom`), so `ed25519-dalek` is the
> wave's only `Cargo.toml` change — exactly the design's dependency
> call. Two honest edges: verification uses `verify_strict`
> (canonical-S, one valid encoding per signature), and externalized
> journal snapshots are refused at mint and verify in v1 with a typed
> error — the ledger digests must cover payloads the verifier can
> resolve. The multi-host posture is stated, not solved: a host booting
> against a shared store without the local secret becomes its own
> signer (a new journaled key id; old receipts keep verifying), and
> fleet-scale key management remains the R1.0 KMS work of open question
> 3.

**Wave 4 — bridges and the release proof.** The four bridge directions
with streaming and cancellation preserved; generated MCP schemas and
generated Agent Cards; journaled MCP/A2A client calls with derived
idempotency keys. Exit: the release proof below.

> **Wave 4 status: implemented.** The bridges landed as written. Server
> side: `POST /mcp` exposes every registered graph as one MCP tool
> (`tools/list` schemas derived from each graph's state spec — append
> channels are arrays, deep-merge channels objects; `tools/call` runs the
> graph on a fresh thread, answering plain JSON or SSE
> `notifications/progress` plus the terminal result, with
> disconnect-cancels-the-run and `notifications/cancelled` mapped to the
> run-level cancel); `GET /.well-known/agent-card.json` serves the
> derived, deterministic Agent Card; `POST /a2a` maps A2A tasks onto the
> durable task queue (`message/send`, `message/stream`, `tasks/get`,
> `tasks/cancel`); `PUT /capsules/{id}/blob` stores the component bytes
> the manifest's `build_digest` commits to, digest-checked at the route.
> Client side: `JournaledMcpTool` journals every live MCP call (effect id
> derived from the request hash) and replays from a `ReplaySource`
> without a client by construction; `A2aNode` delegates over JSON-RPC
> with the derived `a2a-{thread}-{step}-{node}` message id as its
> idempotency handle, journals the terminal task as one `RemoteCall`,
> and maps remote cancellation to `tasks/cancel`. Refinements worth
> naming. **The protocol revisions are pinned, not negotiated** — MCP
> `2025-03-26` (the Streamable HTTP revision) and A2A `0.3.0` (which
> renamed the well-known card path) — the runtime's own pin posture:
> the pin is what the conformance evidence records. **An A2A context is
> one Flight Recorder journal** (`a2a-{tenant}-{contextId}` — the tenant
> is embedded because journal keys are bare run ids and context ids are
> client-chosen), bound to a synthetic thread record whose graph is the
> registered name `a2a`, so the native `/events`, `/fixture`, and
> `/receipt` endpoints resolve context evidence exactly as for a graph
> run; the release proof registers that trivial graph. **Capsule
> payloads execute in-process; plain messages queue for external
> workers** — the pool is the addressing (`a2a-capsule` vs `a2a`), the
> bridge's drainer claims only capsule work, and the connector is the
> deployment's explicit egress seam (`ServerConfig::with_capsule_connector`;
> absent, capsule tasks fail closed with `capsule_execution_unavailable`).
> **The filesystem refusal is journaled at admission**: the v1 world has
> no filesystem import for a guest to probe, so the bridge journals the
> unscoped (empty-scope) structural denial the host would have recorded —
> the caller's `requires: ["filesystem"]` declaration is the refusal
> trigger, and the evidence shape is identical to a host-raised denial.
> **Journal writes are serialized per context** (`journal_locks`): each
> execution is a load → append → persist cycle over a whole-journal
> snapshot, so concurrent capsule tasks of one context cannot clobber
> each other's freshly journaled events. **Artifacts are content-addressed
> by construction** — the artifact id derives from the canonical output
> bytes, the body lives on the durable task record, and the digest is
> journaled; no second artifact store, and both store backends stay
> equal. Two honest edges: the MCP bridge answers JSON-RPC errors in the
> envelope with HTTP 200 (clients dispatch on `error.code`, and a non-200
> would read as transport failure), and batch requests are refused — one
> envelope per POST keeps the SSE answer shape unambiguous.

**Release proof (the whole release).** The roadmap's sentence, automated
as an integration test in the crash-recovery family
(`rusty-server/tests/capsules_release.rs`): *run an untrusted remote
agent that attempts forbidden network and filesystem access and visibly
deny it.* Concretely: a third-party-shaped capsule (the in-tree
reference guest, built to probe) arrives through the A2A bridge as a
durable node, with a manifest granting exactly one network host and no
filesystem. The test drives the capsule to (a) call its granted host —
succeeds, journaled as an effect; (b) call a second, ungranted host —
denied at the host connector, `CapsuleDenied` journaled naming the
absent `network` grant; (c) attempt filesystem access — the import does
not exist in its world; the denial is structural and journaled. Then the
assertions that make "visibly" a test rather than a word: the journal
contains both denials with causal parentage into the invoking run; each
denial names the exact manifest grant that was absent; and the run's
signed receipt verifies — and covers the denials ledger, so the signed
statement itself records that forbidden access was attempted and refused.
Finally, tampering with any journaled denial event fails receipt
verification. Deny by default, attributable, and provable — as a test.

## Open questions

Flagged for the owner before wave 1 lands:

1. **Wasmtime version cadence and build cost.** Wasmtime 47 is already an
   optional dependency; the Component Model toolchain (WIT, `wit-bindgen`,
   `wasmtime`'s component support) is still moving, and cranelift dominates
   clean-build time. Leaning: pin wasmtime per release with a documented
   upgrade cadence (one minor per release cycle, security fixes exempt),
   keep the host behind `wasm` so default builds never pay for it, and
   treat CI caching as the mitigation rather than freezing on an old
   major. No fork, under any circumstance.
2. **How guests are built and distributed in v1.** Rusty does not ship a
   toolchain, so the honest question is what a guest author *does*.
   Leaning: any language targeting the Component Model; distribution as
   content-addressed component blobs through the R0.7 artifact store,
   registered by `(identity, version)` against a `build_digest` the
   server recomputes; exactly one in-tree reference guest (Rust,
   `wit-bindgen`) that the release proof and goldens exercise. A
   publishing workflow (OCI registries are the obvious candidate) is
   R1.0+.
3. **Receipt key management.** Local Ed25519 keys are honest but not
   operable at fleet scale (rotation across N servers, key compromise
   response). Leaning: v1 as designed — per-deployment keys, journaled
   rotation, documented compromise runbook (the key id on every receipt
   is what makes "receipts signed after the compromise date are suspect"
   answerable); KMS and transparency-log witnessing in R1.0 against the
   unchanged canonical receipt bytes.
4. **MCP transport scope.** The client ships stdio; the server bridge
   needs a transport that fits axum. Leaning: server bridge on Streamable
   HTTP (SSE for progress, per the spec's direction); client keeps stdio
   and gains Streamable HTTP in wave 4; the protocol revision is pinned
   and *recorded, not validated*, exactly as `MCP_PROTOCOL_VERSION`
   already does — negotiation theater is worse than an honest pin.
5. **A2A spec version pin.** A2A is younger and moving faster than MCP;
   its task/artifact model maps cleanly onto R0.6's, but the wire shapes
   are not frozen. Leaning: pin a dated spec revision in the module docs
   (the same discipline as the MCP pin), implement the generated-Card
   plus tasks/artifacts/streaming subset only, and mark the pin as the
   additive-evolution boundary — a second revision lands as additional
   endpoints, never a rewrite of pinned shapes.
6. **WIT world versioning.** One world (`rusty:capsule/world@0.1.0`) in
   wave 1 is minimal; the question is the evolution rule when a later
   release needs new imports. Leaning: worlds evolve additively like
   every other contract here — new imports arrive in a new world version,
   old world versions keep instantiating indefinitely (the host links
   what each world declares), and the manifest's world reference is how a
   capsule says which interface era it belongs to. Retiring a world
   version is a deprecation-policy question for `stability.md`, deferred
   until there is a second world.
