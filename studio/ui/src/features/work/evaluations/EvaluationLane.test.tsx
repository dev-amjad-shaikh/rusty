import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EvaluationCase } from "../../../state/work";
import { comparisonWindow, datasetIdentity, EvaluationLane, splitDataset } from "./EvaluationLane";

const cases: EvaluationCase[] = [{
  id: "local-1", caseId: "refund",
  runId: "run-source", threadId: "thread-source", agentName: "Support", agentId: "assistant-support",
  objective: "Answer the refund question", pointer: "/answer", expected: "30 days", createdAt: "2026-08-12T00:00:00Z",
}];

const report = (name: string, rate: number) => ({
  format_version: 1, name, dataset_name: "agent-regression", dataset_version: "2026-08-12",
  runs_per_case: 3, max_concurrency: 2,
  cases: [{ case_id: "refund", tags: ["studio"], pass_rate: rate, runs: Array.from({ length: 3 }, (_, repetition) => ({ repetition, status: { status: "done" }, passed: rate === 1, assertions: [], tool_calls: 0, latency_ms: name === "baseline" ? 10 : 12, cost_usd: .001, total_tokens: 10 })) }],
  summary: { cases: 1, runs: 3, runs_passed: rate === 1 ? 3 : 2, run_pass_rate: rate, case_pass_rate: rate, assertions: [], latency_ms: { min: 10, p50: 10, p95: name === "baseline" ? 10 : 12, max: 12, mean: 11 }, total_cost_usd: .003, total_tokens: 30 },
});

const candidateId = "b".repeat(64);
const complete = {
  experiment_id: "exp-fixed", dataset_name: "agent-regression", dataset_version: "2026-08-12", candidate_id: candidateId,
  config: { runs_per_case: 3, max_concurrency: 2, target_metric: "case_pass_rate", thresholds: { max_pass_rate_drop: .05, max_latency_p95_ratio: 1.25 } },
  status: { phase: "complete" }, created_at: "2026-08-12T00:00:01Z", updated_at: "2026-08-12T00:00:02Z",
  baseline_report: report("baseline", 1), candidate_report: report("candidate", .67),
  comparison: { baseline: "baseline", candidate: "candidate", thresholds: { max_pass_rate_drop: .05, max_latency_p95_ratio: 1.25 }, assertion_deltas: [], case_deltas: [{ case_id: "refund", baseline_pass_rate: 1, candidate_pass_rate: .67, change: "regressed" }], latency: { baseline_p50: 10, candidate_p50: 12, p50_ratio: 1.2, baseline_p95: 10, candidate_p95: 12, p95_ratio: 1.2 }, baseline_cost_usd: .003, candidate_cost_usd: .003, regressions: [{ regression: "case_pass_rate", case_id: "refund", baseline: 1, candidate: .67 }], regressed: true },
};

function renderLane() {
  const root = createRootRoute();
  const route = createRoute({ getParentRoute: () => root, path: "/", component: () => <EvaluationLane cases={cases} /> });
  const router = createRouter({ routeTree: root.addChildren([route]), history: createMemoryHistory({ initialEntries: ["/"] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

afterEach(() => vi.unstubAllGlobals());

describe("evaluation lane", () => {
  it("keeps dataset name and version identities injective when either contains @", () => {
    const left = datasetIdentity("support@v2", "candidate");
    const right = datasetIdentity("support", "v2@candidate");
    expect(left).not.toBe(right);
    expect(splitDataset(left)).toEqual(["support@v2", "candidate"]);
    expect(splitDataset(right)).toEqual(["support", "v2@candidate"]);
  });
  it("keeps large comparisons reviewable in bounded windows", () => {
    const items = Array.from({ length: 121 }, (_, index) => `case-${index}`);
    expect(comparisonWindow(items, 0)).toMatchObject({ items: items.slice(0, 50), start: 0, end: 50, pageCount: 3 });
    expect(comparisonWindow(items, 2)).toMatchObject({ items: items.slice(100), start: 100, end: 121, pageCount: 3 });
    expect(comparisonWindow(items, 99)).toMatchObject({ items: items.slice(100), start: 100, end: 121, pageCount: 3 });
  });

  it("publishes exact source provenance, renders paired outcomes, and saves only a reviewed gate", async () => {
    let published = false;
    let experiment: typeof complete | null = null;
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input, "http://studio.local").pathname.replace(/^\/api/, "");
      if (path === "/datasets" && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        expect(body.cases[0].source).toEqual({ run_id: "run-source", thread_id: "thread-source", agent_id: "assistant-support", captured_at: "2026-08-12T00:00:00Z" });
        published = true;
        return Promise.resolve(new Response(JSON.stringify({ name: "agent-regression", version: "2026-08-12", created: true, case_count: 1, digest: "a".repeat(64) }), { status: 201 }));
      }
      if (path === "/datasets") return Promise.resolve(new Response(JSON.stringify({ datasets: published ? [{ name: "agent-regression", version: "2026-08-12", created_at: "2026-08-12T00:00:00Z", case_count: 1, digest: "a".repeat(64) }] : [], truncated: false })));
      if (path.endsWith("/cases")) return Promise.resolve(new Response(JSON.stringify({ cases: [{ id: "refund", input: { objective: "Answer the refund question" }, expect: { state: [{ pointer: "/answer", expected: "30 days" }] }, tags: ["studio", "Support"], source: { run_id: "run-source", thread_id: "thread-source", agent_id: "assistant-support", captured_at: "2026-08-12T00:00:00Z" } }] })));
      if (path === "/experiments" && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        experiment = { ...complete, experiment_id: body.experiment_id, candidate_id: body.candidate_id, dataset_name: body.dataset_name, dataset_version: body.dataset_version, config: { runs_per_case: body.runs_per_case, max_concurrency: body.max_concurrency, target_metric: body.target_metric, thresholds: body.thresholds } };
        const { baseline_report: _baseline, candidate_report: _candidate, comparison: _comparison, ...summary } = experiment;
        return Promise.resolve(new Response(JSON.stringify({ ...summary, status: { phase: "queued" } }), { status: 201 }));
      }
      if (path === "/experiments") {
        const summary = experiment ? (({ baseline_report: _baseline, candidate_report: _candidate, comparison: _comparison, ...rest }) => rest)(experiment) : null;
        return Promise.resolve(new Response(JSON.stringify({ experiments: summary ? [summary] : [], truncated: false })));
      }
      if (path.startsWith("/experiments/")) return Promise.resolve(new Response(JSON.stringify(experiment)));
      if (path === "/learn/candidates") return Promise.resolve(new Response(JSON.stringify({ candidates: [{ candidate: { candidate_id: candidateId, content: { kind: "prompt", name: "support" }, created_at: "2026-08-12T00:00:00Z" }, status: "created" }] })));
      if (path === "/gates" && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        expect(body).toMatchObject({ experiment_id: experiment?.experiment_id, acknowledged: true, policy: { maximum_regressions: 0, forbid_removed_cases: true } });
        return Promise.resolve(new Response(JSON.stringify({ name: body.name, blocked_target: body.blocked_target, experiment_id: body.experiment_id, dataset_name: "agent-regression", dataset_version: "2026-08-12", policy: body.policy, decision: { format_version: 1, policy: body.name, candidate: "candidate", baseline: "baseline", outcome: "block", checks: [] }, created_at: "2026-08-12T00:00:03Z" }), { status: 201 }));
      }
      if (path === "/gates") return Promise.resolve(new Response(JSON.stringify({ gates: [], truncated: false })));
      throw new Error(`unexpected ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderLane();

    await userEvent.click(await screen.findByRole("button", { name: "Publish dataset" }));
    await waitFor(() => expect(screen.getByLabelText("Dataset")).toBeVisible());
    await userEvent.selectOptions(screen.getByLabelText("Candidate"), candidateId);
    await userEvent.click(screen.getByRole("button", { name: "Run experiment" }));
    expect(await screen.findByRole("heading", { name: "Paired outcomes" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Open source run" })).toHaveAttribute("href", "/work/thread-source/runs/run-source/trace");
    expect(screen.getByText("Regression", { selector: "b" })).toBeVisible();

    await userEvent.type(screen.getByLabelText("Gate name"), "production-quality");
    await userEvent.type(screen.getByLabelText("Release target"), "deployment:production");
    expect(screen.getByRole("button", { name: "Save release gate" })).toBeDisabled();
    await userEvent.click(screen.getByRole("checkbox", { name: /I reviewed this policy/ }));
    await userEvent.click(screen.getByRole("button", { name: "Save release gate" }));
    expect(await screen.findByText("Gate production-quality saved with a block decision.")).toBeVisible();
  });

  it("binds source-run links to the selected experiment instead of the authoring dataset", async () => {
    const exact = { ...complete, experiment_id: "exp-a", dataset_name: "dataset-a", dataset_version: "v1", baseline_report: { ...complete.baseline_report, dataset_name: "dataset-a", dataset_version: "v1" }, candidate_report: { ...complete.candidate_report, dataset_name: "dataset-a", dataset_version: "v1" } };
    const { baseline_report: _baseline, candidate_report: _candidate, comparison: _comparison, ...summary } = exact;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input, "http://studio.local").pathname.replace(/^\/api/, "");
      if (path === "/datasets") return Promise.resolve(new Response(JSON.stringify({ datasets: [
        { name: "dataset-b", version: "v1", created_at: "2026-08-12T00:00:00Z", case_count: 1, digest: "b".repeat(64) },
        { name: "dataset-a", version: "v1", created_at: "2026-08-12T00:00:00Z", case_count: 1, digest: "a".repeat(64) },
      ], truncated: false })));
      if (path === "/experiments") return Promise.resolve(new Response(JSON.stringify({ experiments: [summary], truncated: false })));
      if (path === "/experiments/exp-a") return Promise.resolve(new Response(JSON.stringify(exact)));
      if (path === "/datasets/dataset-a/versions/v1/cases") return Promise.resolve(new Response(JSON.stringify({ cases: [{ id: "refund", input: { objective: "A" }, source: { run_id: "run-a", thread_id: "thread-a", agent_id: "agent-a", captured_at: "2026-08-12T00:00:00Z" } }] })));
      if (path === "/learn/candidates") return Promise.resolve(new Response(JSON.stringify({ candidates: [] })));
      if (path === "/gates") return Promise.resolve(new Response(JSON.stringify({ gates: [], truncated: false })));
      throw new Error(`unexpected ${path}`);
    }));
    renderLane();

    expect(await screen.findByLabelText("Dataset")).toHaveDisplayValue("dataset-b@v1 · 1 cases");
    expect(await screen.findByRole("link", { name: "Open source run" })).toHaveAttribute("href", "/work/thread-a/runs/run-a/trace");
  });

  it("locks release acknowledgement when exact dataset sources are unavailable", async () => {
    const { baseline_report: _baseline, candidate_report: _candidate, comparison: _comparison, ...summary } = complete;
    const fetchMock = vi.fn().mockImplementation((input: string) => {
      const path = new URL(input, "http://studio.local").pathname.replace(/^\/api/, "");
      if (path === "/datasets") return Promise.resolve(new Response(JSON.stringify({ datasets: [{ name: "agent-regression", version: "2026-08-12", created_at: "2026-08-12T00:00:00Z", case_count: 1, digest: "a".repeat(64) }], truncated: false })));
      if (path === "/experiments") return Promise.resolve(new Response(JSON.stringify({ experiments: [summary], truncated: false })));
      if (path === "/experiments/exp-fixed") return Promise.resolve(new Response(JSON.stringify(complete)));
      if (path.endsWith("/cases")) return Promise.resolve(new Response(JSON.stringify({ error: "source evidence unavailable" }), { status: 503 }));
      if (path === "/learn/candidates") return Promise.resolve(new Response(JSON.stringify({ candidates: [] })));
      if (path === "/gates") return Promise.resolve(new Response(JSON.stringify({ gates: [], truncated: false })));
      throw new Error(`unexpected ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderLane();

    expect(await screen.findByText(/Release review stays locked/)).toBeVisible();
    expect(screen.getByRole("checkbox", { name: /I reviewed this policy/ })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Save release gate" })).toBeDisabled();
    const readsBeforeRetry = fetchMock.mock.calls.filter(([input]) => new URL(input, "http://studio.local").pathname.replace(/^\/api/, "").endsWith("/cases")).length;
    await userEvent.click(screen.getByRole("button", { name: "Retry source cases" }));
    await waitFor(() => expect(fetchMock.mock.calls.filter(([input]) => new URL(input, "http://studio.local").pathname.replace(/^\/api/, "").endsWith("/cases")).length).toBeGreaterThan(readsBeforeRetry));
  });
});
