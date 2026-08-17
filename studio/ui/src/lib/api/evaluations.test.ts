import { afterEach, describe, expect, it, vi } from "vitest";
import { getExperiment } from "./evaluations";


afterEach(() => vi.unstubAllGlobals());

describe("evaluation API contracts", () => {
  it("preserves server-legal u64 report evidence beyond JavaScript's safe integer", async () => {
    const marker = "__U64__";
    const report = {
      format_version: 1, name: "candidate", dataset_name: "dataset", dataset_version: "v1",
      runs_per_case: 1, max_concurrency: 1,
      cases: [{ case_id: "case-1", pass_rate: 1, runs: [{ repetition: 0, status: { status: "done" }, passed: true, assertions: [], tool_calls: marker, latency_ms: marker, cost_usd: 0, total_tokens: marker }] }],
      summary: { cases: 1, runs: 1, runs_passed: 1, run_pass_rate: 1, case_pass_rate: 1, assertions: [], latency_ms: { min: marker, p50: marker, p95: marker, max: marker, mean: 1 }, total_cost_usd: 0, total_tokens: marker },
    };
    const record = { experiment_id: "exp-1", dataset_name: "dataset", dataset_version: "v1", candidate_id: "a".repeat(64), config: { runs_per_case: 1, max_concurrency: 1, target_metric: "case_pass_rate", thresholds: { max_pass_rate_drop: .05, max_latency_p95_ratio: 1.25 } }, status: { phase: "complete" }, created_at: "2026-08-12T00:00:00Z", updated_at: "2026-08-12T00:00:01Z", candidate_report: report };
    const body = JSON.stringify(record).replaceAll(`"${marker}"`, "18446744073709551615");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(body)));

    const parsed = await getExperiment("exp-1");
    expect(parsed.candidate_report?.summary.total_tokens).toBe("18446744073709551615");
    expect(parsed.candidate_report?.summary.latency_ms.p95).toBe("18446744073709551615");
    expect(parsed.candidate_report?.cases[0].runs[0].latency_ms).toBe("18446744073709551615");
  });
});
