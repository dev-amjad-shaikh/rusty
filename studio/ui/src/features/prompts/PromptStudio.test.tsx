import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useWorkStore } from "../../state/work";
import { PromptStudio } from "./PromptStudio";

const originalId = "a".repeat(64);
function response(value: unknown, status = 200) { return new Response(JSON.stringify(value), { status }); }
function renderPage() {
  const root = createRootRoute({ component: Outlet });
  const prompts = createRoute({ getParentRoute: () => root, path: "/agents/prompts", component: PromptStudio });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: () => <p>Agents</p> });
  const router = createRouter({ routeTree: root.addChildren([prompts, agents]), history: createMemoryHistory({ initialEntries: ["/agents/prompts"] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

beforeEach(() => {
  useWorkStore.setState({ assistant: null, objective: "", thread: null, receipt: { run_id: "run-1", thread_id: "thread-1", status: "running" }, cases: [], comparisons: [{ run: { run_id: "run-1", thread_id: "thread-1", graph: "research", attempt: 1, status: "success" }, evidence: { run_id: "run-1", events: [], complete: true }, agentName: "Analyst", objective: "Verify", capturedAt: "2026-08-11T00:00:00Z" }] });
});
afterEach(() => vi.unstubAllGlobals());

describe("prompt workshop", () => {
  it("loads immutable history, edits, and saves a run-attributed version", async () => {
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const url = new URL(input, "http://studio.local");
      const body = init?.body ? JSON.parse(String(init.body)) : null;
      if (url.pathname.replace(/^\/api/, "") === "/registry/artifacts" && init?.method !== "POST") return Promise.resolve(response({ artifacts: [{ surface: "prompt:system", family: "prompt", owner: { type: "human", human_id: "amjad" }, commits: [{ candidate_id: originalId, committed_at: "2026-08-11T00:00:00Z" }], created_at: "2026-08-11T00:00:00Z" }] }));
      if (url.pathname.replace(/^\/api/, "").endsWith("/commits") && init?.method !== "POST") return Promise.resolve(response({ surface: "prompt:system", family: "prompt", owner: { type: "human", human_id: "amjad" }, commits: [{ candidate_id: originalId, committed_at: "2026-08-11T00:00:00Z", author: { type: "human", human_id: "amjad" }, status: "promoted" }] }));
      if (url.pathname.replace(/^\/api/, "") === `/learn/candidates/${originalId}`) return Promise.resolve(response({ candidate: { candidate_id: originalId, content: { kind: "prompt", name: "system", prompt: "Be accurate." }, distilled_by: { type: "human", human_id: "amjad" }, created_at: "2026-08-11T00:00:00Z" }, status: "promoted" }));
      if (url.pathname.replace(/^\/api/, "") === "/learn/candidates" && init?.method === "POST") return Promise.resolve(response({ candidate_id: body.candidate.candidate_id, created: true, record: { candidate: body.candidate, status: "created" } }, 201));
      if (url.pathname.replace(/^\/api/, "").endsWith("/commits") && init?.method === "POST") return Promise.resolve(response({ surface: "prompt:system", committed: true, commit: { candidate_id: body.candidate_id, committed_at: "2026-08-11T00:00:01Z" }, commits: 2 }));
      throw new Error(`unexpected ${url} ${init?.method ?? "GET"}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPage();
    await waitFor(() => expect(screen.getByDisplayValue("Be accurate.")).toBeVisible());
    await userEvent.type(screen.getByLabelText("Author"), "amjad");
    await userEvent.selectOptions(screen.getByLabelText("Evidence run"), "run-1");
    await userEvent.clear(screen.getByLabelText("Instructions"));
    await userEvent.type(screen.getByLabelText("Instructions"), "Be accurate and concise.");
    await userEvent.click(screen.getByRole("button", { name: "Save version" }));
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("is in the prompt history"));
    const candidateRequest = fetchMock.mock.calls.find(([url, init]) => String(url).endsWith("/learn/candidates") && (init as RequestInit)?.method === "POST");
    expect(JSON.parse(String((candidateRequest?.[1] as RequestInit).body))).toMatchObject({ run_id: "run-1", candidate: { content: { prompt: "Be accurate and concise." }, distilled_by: { human_id: "amjad" } } });
  });
});
