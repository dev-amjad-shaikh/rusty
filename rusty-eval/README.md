# rusty-eval

Agent TestOps for Rusty: versioned evaluation datasets, deterministic
assertions over recorded runs, experiment reports, and baseline-vs-candidate
comparison — built directly on the [`rusty-agent-runtime`](../rusty-core)
executor and Flight Recorder journal. No simulation harness, no live model
required.

## The pipeline

```
dataset (JSONL, versioned)
      │
      ▼
ExperimentRunner ── runs the agent N× per case through Executor::run,
      │            each run journaling into its own Flight Recorder journal
      ▼
RunEvidence ── ordered tool-call trajectory + final state + latency/cost,
      │        distilled from the journal
      ▼
Assertion::evaluate ── deterministic pass/fail with expected-vs-observed evidence
      │
      ▼
ExperimentReport ── per-case detail, pass rate per assertion, p50/p95 latency, cost
      │
      ▼
compare(baseline, candidate) ── per-assertion deltas, per-case regressions,
                                threshold-flagged release verdict
```

## Dataset format

JSONL: line 1 is the header, every following line is a case. Serialization
is canonical (field order fixed, map keys sorted), so datasets are diffable
in version control and `load → save` is byte-stable.

```jsonl
{"kind":"header","format_version":1,"name":"math-tools","version":"1.0.0"}
{"kind":"case","id":"add-two","input":{"messages":[{"role":"user","content":"2+3?"}]},"expect":{"tool_trajectory":[{"name":"calculator","args":{"/op":"add"}}],"state":[{"pointer":"/messages/3/content","expected":"the answer is 5"}]},"tags":["smoke"]}
```

Each case: `id`, `input` (merged into the run's initial state by default),
`expect` (trajectory as an ordered subsequence with optional JSON-pointer
argument matchers, state predicates, forbidden tools, cost/latency bounds),
and `tags`. Loading validates `format_version` and refuses unknown versions.

## Assertions

Deterministic checks only — no model in the loop:

| Assertion | Checks |
|---|---|
| `tool_call_order` | Expected calls appear as an ordered subsequence, argument matchers satisfied |
| `tool_call_count` | Exact call count for one tool |
| `state[...]` | Final-state value at a JSON pointer equals the expected value |
| `no_tool_call` | Blacklisted tools never called |
| `max_cost` / `max_latency` | Run totals within bounds |

Every verdict returns `{ assertion, passed, expected, observed, detail }` —
the report shows *why* a run failed, not just that it did.

## Judges

`JudgeModel` (one async `judge` method, mirroring the runtime's `ChatModel`)
is the evaluator seam. `RuleBasedJudge` scores the fraction of expectations
met without a live model. `ModelJudge` adapts any runtime `ChatModel` into a
structured LLM judge: the trusted rubric stays in the system instruction,
evidence travels as untrusted JSON, output is accepted only as one
schema-constrained tool call or strict JSON fallback, scores and rationale
sizes are bounded before parsing, response roles are verified, and pass/fail
is derived locally.

## Status

Foundation release (v0.1.0): library only, no CLI. Deliberately absent for
now: provider-specific judge clients (use the runtime's `ChatModel` adapters),
dataset/report storage backends beyond JSON files, parallel experiment
execution, and assertion kinds beyond the deterministic set above.
