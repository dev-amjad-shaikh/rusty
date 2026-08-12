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
