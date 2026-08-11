import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { RunEvent } from "../../lib/contracts";
import { useConnectionStore } from "../../state/connection";
import { useWorkStore } from "../../state/work";
import { RunComparePage } from "./RunComparePage";

function event(run: string, thread: string, seq: number, kind: RunEvent["kind"], latency: string | null, tokens: string | null, status: RunEvent["status"] = "ok"): RunEvent {
  return { id: `${run}:${seq}`, run_id: run, thread_id: thread, node_id: "agent", seq: String(seq), kind, effect: "pure", input: null, output: null, latency_ms: latency, tokens: tokens ? { prompt_tokens: "0", completion_tokens: tokens, total_tokens: tokens } : null, cost_usd: null, status, parent: seq ? `${run}:${seq - 1}` : null, recorded_at: "2026-08-11T00:00:00Z", rawJson: "{}" };
}
function renderPage() {
  const root = createRootRoute({ component: Outlet });
  const compare = createRoute({ getParentRoute: () => root, path: "/work/compare", component: RunComparePage });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: () => <p>Work</p> });
  const trace = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId/trace", component: () => <p>Trace</p> });
  const router = createRouter({ routeTree: root.addChildren([compare, work, trace]), history: createMemoryHistory({ initialEntries: ["/work/compare"] }) });
  return render(<RouterProvider router={router} />);
}

beforeEach(() => {
  useConnectionStore.setState({ connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" }, info: null, dialogOpen: false });
  const connectionKey = "1|https://rusty.example|a";
  useWorkStore.setState({ comparisons: [
    { connectionKey, run: { run_id: "run-a", thread_id: "thread-a", graph: "research", attempt: 1, status: "success", output: {} }, evidence: { run_id: "run-a", complete: true, events: [event("run-a", "thread-a", 0, "node_input", null, null), event("run-a", "thread-a", 1, "model_call", "120", "25")] }, agentName: "Analyst", objective: "Baseline answer", capturedAt: "2026-08-11T00:00:00Z" },
    { connectionKey, run: { run_id: "run-b", thread_id: "thread-b", graph: "research", attempt: 1, status: "success", output: {} }, evidence: { run_id: "run-b", complete: true, events: [event("run-b", "thread-b", 0, "node_input", null, null), event("run-b", "thread-b", 1, "model_call", "90", "20"), event("run-b", "thread-b", 2, "tool_call", null, null)] }, agentName: "Analyst", objective: "Candidate answer", capturedAt: "2026-08-11T00:01:00Z" },
  ] });
});

describe("run comparison", () => {
  it("shows paired outcomes, coverage-aware metrics, aligned steps, and trace handoffs", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "See what changed between runs" })).toBeVisible();
    expect(screen.getByText("120 ms · 1/2")).toBeVisible();
    expect(screen.getByText("90 ms · 1/3")).toBeVisible();
    expect(screen.getByRole("table", { name: "Aligned run steps" })).toBeVisible();
    expect(screen.getByText(/agent · tool call/)).toBeVisible();
    expect(screen.getAllByRole("link", { name: "Open trace" })).toHaveLength(2);
  });
});
