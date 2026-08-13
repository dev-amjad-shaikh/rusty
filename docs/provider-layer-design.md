# Provider layer design

Status: design accepted 2026-08-12 · R1.0 gate (see the gates table in
[stability.md](stability.md)) · supersedes the "bring your own provider"
note in the rusty-core README.

How Rusty gets provider breadth — Anthropic, Gemini, Ollama, and twenty
more — without hand-built adapters, and without letting an external
crate anywhere near the evidence formats.

## The decision

Three facts frame everything below.

**The `ChatModel` trait is a durability seam, not just an API.** Exact
replay matches a recorded model call by the sha256 of its canonical
request JSON — `{"messages", "tools"}` in *our* serde form
(`rusty-core/src/replay.rs`). `ChatMessage`, `ToolCall`, and `Usage` are
journaled verbatim, stored in state channels, and pinned by golden
files. Whoever owns those types owns our persistence format. That
ownership is not for sale, so the trait and its vocabulary stay in
rusty-core no matter which provider crate we adopt.

**Hand-building one adapter per provider does not scale.** The existing
`OpenAiCompatibleClient` speaks the OpenAI wire format only. Native
Anthropic and Gemini differ in roles (Anthropic hoists `system` out of
the message list), tool-call shapes (`tool_use` blocks, `functionCall`
parts), usage field names (`input_tokens`/`output_tokens`, cache and
reasoning tokens), and streaming chunk grammar. Each native protocol is
a real adapter with real maintenance, and the set of providers worth
supporting grows faster than this project can write them.

**The Rust ecosystem now has two credible abstraction crates.** The
2026-08 survey (sources: lib.rs / crates.io download and dependent-crate
counts, repository release histories) considered Rig (`rig-core`),
genai, async-openai, graniet's `llm`, llm-connector, anyllm, and
mistral.rs. The finding: async-openai is OpenAI-only; llm-chain is
dead; mistral.rs is an inference engine, not an abstraction layer;
graniet/llm, llm-connector, and anyllm are too thinly adopted to bet a
1.0 on. That leaves Rig and genai.

We adopt **genai, behind an optional feature, wrapped by a thin shim**.
Rig is the ecosystem leader and trait-first, but it costs 3 MB and
~2 breaking releases a month, tracks the newest Rust aggressively, and
carries an unresolved "maybe GPL-3.0" flag on lib.rs — the wrong risk
profile for a stability release. genai (`jeremychone/rust-genai`, MIT
OR Apache-2.0, ~38k downloads/month, 26+ native-protocol adapters) has
the leanest dependency profile of the credible options, normalized
streaming and tool calling across providers, and a deliberately
provider-neutral message model that maps onto ours with one conversion
layer. Its risks — a solo maintainer and a long beta train — are
confined to an optional feature we can replace with `rig-core` later
without touching the core crate. The hand-rolled
`OpenAiCompatibleClient` stays as the zero-extra-dependency default.

This is the hybrid the roadmap asks for: an *integrated* provider
layer, not hand-built adapters, and not an outsourced vocabulary.

## What changes in the core crate (additive)

Three pressure points surfaced by the seam analysis get fixed now,
while the change is cheap, because every provider integration needs
them. All are additive at the serde level; existing goldens stand.

**1. Usage grows optional detail; cost becomes computable.**
`Usage` gains `cached_tokens` and `reasoning_tokens`
(`Option<u64>`, `serde(default, skip_serializing_if)` — absent on the
wire when unset, so the pinned shape is unchanged). Providers that
report cache or reasoning tokens (Anthropic, OpenAI reasoning models,
Gemini) no longer lose them at the boundary.

Cost: `ChatModel` gains a defaulted `pricing()` method returning
`Option<ModelPricing>` (per-million-token input/output rates, with an
optional cached-input rate). The journaling path — the same place that
already stamps `tokens` on a `ModelCall` event — computes
`cost_usd = f(usage, pricing)` when both are present and records it on
the event. Today *nothing* produces `cost_usd`: the evidence layer
aggregates it, `rusty-eval`'s `MaxCost` gates read it, and every run
silently reports zero. After this change the field has a real producer
and cost gates become meaningful. A model that cannot price itself
returns `None` and behaves exactly as today.

**2. LLM errors carry a classification.** `RustyError::Llm(String)`
erases the difference between a rate limit and a fatal auth failure;
the retry classifier inside `OpenAiCompatibleClient` already knows the
difference and throws it away at the trait boundary. We add a
classified variant — `LlmFailure { class: LlmErrorClass, message }`
with `LlmErrorClass ∈ { RateLimited, Timeout, Server, Auth,
InvalidRequest, Decode, Unknown }` — produced by the built-in clients.
`Llm(String)` stays for user implementations; classifying helpers treat
it as `Unknown`. The durable-work `ErrorClass` taxonomy gains a mapping
from `LlmErrorClass`, so LLM failures inside tasks retry with the right
policy instead of the stringly-typed default.

**3. Middleware intercepts streaming too.** `MiddlewareChatModel` today
overrides `chat` only; a wrapped model's `chat_stream` falls through to
the default single-chunk fallback, silently killing token streaming for
any middleware-wrapped model. It will forward `chat_stream`: request
hooks run on the way in, the token callback passes through, response
hooks run on the accumulated final `ChatResponse`, and a rejection
short-circuits before the provider is ever called.

## The genai adapter (feature `genai`)

One new module, `rusty-core/src/provider_genai.rs`, compiled only with
`--features genai` (genai pinned to its stable `0.6` line as an
optional dependency; default features off where avoidable — in
particular the `aws-lc-sys` TLS backend, in favor of ring, to keep C
builds out of the default path). It exports `GenaiChatModel`, a
`ChatModel` over `genai::Client`.

The adapter's contract:

- **Translate at the boundary, journal in our vocabulary.** Messages
  convert from `ChatMessage` to genai's `ChatMessage` (system messages
  hoisted per provider rules, `role: tool` results mapped to tool-result
  blocks); tool schemas pass through **in the order given** — the ReAct
  node sorts schemas by name to keep the replay request hash
  process-stable, and the adapter must not reorder, rename, or
  re-serialize them. Responses convert back: text into
  `ChatMessage.content`, provider tool calls into our `ToolCall` with
  `id` ↔ `tool_call_id` pairing preserved (replay depends on it), and
  usage mapped into `Usage` including the new detail fields.
- **Streaming is driven, not returned.** genai exposes a stream; our
  trait wants `chat_stream(&mut dyn FnMut(TokenChunk) + Send) ->
  Result<ChatResponse>`. The adapter drives the stream internally,
  fires the callback on text deltas only, accumulates tool-call deltas
  silently (matching the OpenAI client's behavior), captures terminal
  usage, and emits exactly one `finish: true` chunk.
- **Errors are classified, not stringified.** Provider errors map onto
  `LlmErrorClass` (429 → `RateLimited`, connect/timeout → `Timeout`,
  5xx → `Server`, 401/403 → `Auth`, 4xx → `InvalidRequest`, decode
  failures → `Decode`), so retry policy survives the boundary.
- **`effect()` stays `NonIdempotent`.** The safe default; nothing about
  a provider call is safely retryable at the trait level.
- **Configuration follows the existing pattern**: explicit
  `GenaiChatModel::new(...)` plus `from_env` mirroring genai's
  env-key conventions, with the model string selecting the provider
  through genai's prefix routing.

Validation: unit tests with recorded genai response shapes (no network
in CI), a scripted round-trip proving replay-hash stability across the
translation, and a live example gated behind env keys, exercised
manually against at least OpenAI and one non-OpenAI provider (Anthropic
or Gemini) before the gate is called closed.

## What stays out

- **Multimodal messages.** `ChatMessage.content` is text. Content
  blocks, images, and audio are a `ChatMessage` change with serde
  consequences for replay and checkpoints — a post-1.0 design of its
  own, not a rider on this one.
- **Embeddings and vector stores.** genai defers these by design; Rig
  has them. If Rusty grows retrieval primitives, that decision revisits
  rig-core — which is why the adapter is a feature, not a marriage.
- **A built-in price list.** `pricing()` is supplied by whoever
  constructs the model. Shipping a vendor price table in the crate
  would be stale the week it ships; docs will show how to set rates and
  leave the numbers to the operator.
- **WASM-compatible provider calls.** genai's tokio/reqwest stack
  targets native; capsules that need models declare the capability and
  the host mediates, as the capsule contract already specifies.

## Open questions

1. genai's 0.7 line is in weekly beta with a 1.0 plausible before our
   R1.0 ships. If 0.7 stabilizes first, adopt it instead of 0.6 —
   decide at implementation time against its changelog, not before.
2. Whether `MiddlewareChatModel`'s streaming interception should run
   response hooks per-token (it will not — per-response only; per-token
   interception is a transform API nobody has asked for).
3. Whether `rusty-eval`'s judge benefits from `LlmErrorClass` for
   judge-failure taxonomy — likely, but eval-side changes wait for a
   concrete need.

> **Wave 1 status: implemented.** The three additive core changes landed
> as written: `Usage` carries `cached_tokens` / `reasoning_tokens`
> (serde-skipped when unset — every golden, including
> `tests/golden/run_event.json`, is byte-unchanged), `ModelPricing` plus
> the defaulted `ChatModel::pricing()` and
> `OpenAiCompatibleClient::with_pricing` make cost computable, and
> `RecordingChatModel` is the `cost_usd` producer — priced models journal
> cost, unpriced models journal exactly what they journaled before (the
> executor and server never stamp `tokens` on model-call events outside
> the recording wrapper, so no second site needed the change).
> `RustyError::LlmFailure { class, message }` with `LlmErrorClass` now
> crosses the provider boundary from `OpenAiCompatibleClient`'s retry
> classifier (retry behavior unchanged; `Llm(String)` stays and
> classifies as `Unknown` via `RustyError::llm_class`), and
> `From<LlmErrorClass> for durable::ErrorClass` maps the classes onto the
> retry taxonomy — `Auth` / `InvalidRequest` / `Decode` land on
> `InvalidInput`, the taxonomy's one never-retried failure class.
> `MiddlewareChatModel` overrides `chat_stream`: hooks run around the
> stream, token deltas forward live, and a rejection prevents the inner
> call, proven by scripted-stream tests in
> `rusty-core/src/middleware.rs`. One follow-up noted for the adapter
> wave: the OpenAI client's wire decode does not yet hoist
> `prompt_tokens_details.cached_tokens` /
> `completion_tokens_details.reasoning_tokens` into the new `Usage`
> fields — the type has the home, the mapping lands with the genai
> adapter's usage translation.

> **Wave 2 status: implemented.** The genai adapter landed as designed,
> with one judgment call on the TLS backend. genai is pinned to the
> stable `0.6` line (resolved 0.6.5) — the 0.7 train was still
> beta-only at implementation time, answering open question 1 against
> the changelog as instructed. The TLS choice needed a detour the
> design did not foresee: genai 0.6's `rustls-tls` feature selects
> reqwest 0.13's `rustls`, which in that line means the aws-lc-rs
> backend — the C/assembly build the design says to keep out — and
> genai offers no ring-selecting feature. The adapter therefore takes
> genai with `default-features = false` (genai documents the
> no-TLS-feature configuration as its supported bring-your-own-TLS
> path), turns on reqwest 0.13's `rustls-no-provider` through a
> renamed edge of our own, and installs rustls's ring provider as the
> process default before any genai client is built: pure-Rust TLS,
> the same provider the default reqwest 0.12 path already rides, no
> `aws-lc-sys` in the tree (verified via `cargo tree`). The boundary
> behaves as specified: system messages hoist into genai's
> request-level `system` field, `role: tool` messages map to
> tool-result content with `id` ↔ `call_id` pairing preserved, tool
> schemas pass through in order with the `parameters` value unmodified
> (determinism proven by a serialize-twice test — the replay request
> hash is computed upstream in our serde form and cannot be disturbed
> here), usage maps the detail fields including `cached_tokens` /
> `reasoning_tokens`, and errors classify onto `LlmErrorClass` from
> both of genai's HTTP error surfaces. Streaming drives genai's event
> stream internally: text deltas fire the callback, genai's
> pre-assembled tool-call events accumulate silently, terminal usage
> comes from the `End` event (requested via `capture_usage`), and
> exactly one `finish: true` chunk closes the stream. Two fidelity
> limits are documented in the module docs rather than papered over:
> the OpenAI participant `name` and genai's reasoning/thought-signature
> parts have no home in our `ChatMessage` vocabulary and are dropped at
> the boundary. All translation is pure functions, tested without
> network in `rusty-core/tests/provider_genai.rs` (19 tests);
> `rusty-core/examples/genai_live.rs` (gated by `required-features`)
> is the manual live check — run it against OpenAI and one non-OpenAI
> provider before the gate is called closed.
>
> **MSRV blocker found at verification time, unresolved.** The whole
> genai 0.6 line (0.6.0 through 0.6.5, checked per-release) uses let
> chains, stabilized in rustc 1.88: the feature build fails on the
> workspace MSRV toolchain (1.86) with 59 `E0658` errors inside genai
> itself. genai declares no `rust-version`, so this only surfaces at
> compile time. The default (feature-off) build remains clean on 1.86,
> and a committed `serde_with >=3, <3.18` edge caps the one transitive
> dependency whose own declared MSRV (1.88) exceeded ours. Per the
> implementation contract the MSRV was NOT bumped. The decision —
> raise the workspace MSRV to 1.88, wait for genai 0.7 to stabilize
> and reassess, or target genai 0.5 — is open and blocks the R1.0
> gate for this wave; everything short of that gate is done and
> green on the current stable toolchain.
>
> **Resolution (2026-08-12): the feature-raised floor, per the capsules
> precedent.** The workspace MSRV stays at 1.86; the `genai` feature
> raises the effective floor to rustc 1.88 for feature-enabled builds
> only — the same posture `docs/capsules-design.md` took when
> cedar-policy required 1.89 for `capsules`. Default builds are
> untouched, and the CI MSRV job checks the default feature set, so
> enforcement is unchanged. Bumping the whole workspace to satisfy one
> optional dependency would punish every default-build user for a
> feature they never turned on; waiting for genai 0.7 would stall the
> R1.0 gate on someone else's release train.
>
> **Live validation (2026-08-12): Fireworks passed.** The gated example
> ran a real ReAct agent over `GenaiChatModel` against Fireworks
> (`fireworks::accounts/fireworks/models/gpt-oss-20b`): live token
> deltas streamed through the event tap, the accumulated final response
> assembled correctly, and state merged through the normal graph
> machinery — no errors. genai drives Fireworks over its
> OpenAI-compatible protocol, so this proves the adapter end-to-end
> against a live provider but not the native-protocol translations
> (Anthropic's top-level `system`, Gemini's `functionCall` parts); one
> run against a native-protocol provider remains the last check before
> the gate is called fully closed.
