import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { useWorkStore } from "../../state/work";
import { durableConnectionScope, rememberRecentWork } from "../../state/recentWork";
import { mutationScope } from "../../lib/api/client";
import type { RunEvent } from "../../lib/contracts";
import { evaluationDatasetJsonl, traceGraphLayout, traceWindow, WorkPage } from "./WorkPage";

function testRouter(initialEntry = "/work") {
  const root = createRootRoute({ component: Outlet });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: WorkPage });
  const run = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId", component: WorkPage });
  const trace = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId/trace", component: WorkPage });
  const evaluate = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId/evaluate", component: WorkPage });
  return createRouter({ routeTree: root.addChildren([work, run, trace, evaluate]), history: createMemoryHistory({ initialEntries: [initialEntry] }) });
}

function renderPage(initialEntry = "/work") {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const router = testRouter(initialEntry);
  return { ...render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>), router, client };
}

const assistant = { assistant_id: "agent-1", name: "Research analyst", graph: "research", config: {}, metadata: {}, created_at: "2026-08-11T00:00:00Z", active_version_id: "av-1", version_count: 1 };
const events = [
  { id: "run-1:0", run_id: "run-1", thread_id: "thread-1", node_id: "research", seq: 0, kind: "node_input", effect: "pure", input: { inline: { objective: "Verify" } }, output: null, latency_ms: null, tokens: null, cost_usd: null, status: "ok", parent: null, recorded_at: "2026-08-11T00:00:00Z" },
  { id: "run-1:1", run_id: "run-1", thread_id: "thread-1", node_id: "research", seq: 1, kind: "model_call", effect: "non_idempotent", input: null, output: { inline: { text: "done" } }, latency_ms: 12, tokens: { prompt_tokens: 8, completion_tokens: 4, total_tokens: 12 }, cost_usd: 0.001, status: "ok", parent: "run-1:0", recorded_at: "2026-08-11T00:00:01Z" },
  { id: "run-1:2", run_id: "run-1", thread_id: "thread-1", node_id: "research", seq: 2, kind: "node_output", effect: "pure", input: null, output: { inline: { answer: "done" } }, latency_ms: 3, tokens: null, cost_usd: null, status: "ok", parent: "run-1:1", recorded_at: "2026-08-11T00:00:02Z" },
];

beforeEach(() => {
  sessionStorage.clear();
  vi.mocked(HTMLElement.prototype.scrollTo).mockClear();
  useConnectionStore.setState({ connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" }, info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] }, dialogOpen: false });
  useWorkStore.setState({ connectionKey: null, assistant: null, objective: "", thread: null, receipt: null, cases: [], comparisons: [], uncertainByConnection: {} });
});
afterEach(() => vi.unstubAllGlobals());

describe("continuous Work journey", () => {
  it("keeps create, run, visual trace, and evaluation in one owned workspace", async () => {
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/threads" && init?.method === "POST") return Promise.resolve(new Response(JSON.stringify({ thread_id: "thread-1", tenant: "default", graph: "research", metadata: { assistant_id: "agent-1" }, created_at: "2026-08-11T00:00:00Z" }), { status: 201 }));
      if (path === "/threads/thread-1/runs" && init?.method === "POST") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", thread_id: "thread-1", status: "running" }), { status: 202 }));
      if (path === "/runs/run-1") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", thread_id: "thread-1", graph: "research", attempt: 1, status: "success", output: { answer: "done" } })));
      if (path === "/runs/run-1/events") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", events, complete: true })));
      throw new Error(`unexpected ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    const { router, container } = renderPage();
    await waitFor(() => expect(screen.getByRole("option", { name: "Research analyst" })).toBeVisible());
    await userEvent.selectOptions(screen.getByLabelText("Agent"), "agent-1");
    await userEvent.type(screen.getByLabelText("Goal"), "Verify the release claim");
    await userEvent.click(screen.getByRole("button", { name: "Start run" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/work/thread-1/runs/run-1"));
    await waitFor(() => expect(screen.getByRole("button", { name: /Inspect trace|Follow trace/ })).toBeVisible());
    await userEvent.click(screen.getByRole("button", { name: /Inspect trace|Follow trace/ }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Work completed" })).toBeVisible());
    expect(screen.getByRole("list", { name: "Causal execution graph" })).toBeVisible();
    expect(container.querySelectorAll("svg path")).toHaveLength(2);
    expect(HTMLElement.prototype.scrollTo).toHaveBeenCalledWith(expect.objectContaining({ left: expect.any(Number), top: expect.any(Number) }));
    await userEvent.click(screen.getByRole("button", { name: "Evaluate this run" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Turn this run into a reusable test" })).toBeVisible());
    expect(screen.getByLabelText("Frozen input")).toHaveValue("Verify the release claim");
    await userEvent.type(screen.getByLabelText("Expected answer"), "The release claim is verified.");
    await userEvent.click(screen.getByRole("checkbox"));
    await userEvent.click(screen.getByRole("button", { name: "Add evaluation case" }));
    expect(screen.getByRole("heading", { name: "1 saved case" })).toBeVisible();
  });

  it("exports the page-memory dataset in Rust evaluation JSONL shape", () => {
    const text = evaluationDatasetJsonl([{ connectionKey: "1|https://rusty.example|a", id: "local-1", caseId: "release", runId: "run-1", threadId: "thread-1", agentName: "Analyst", objective: "Verify release", pointer: "/answer", expected: "verified", createdAt: "2026-08-11T00:00:00Z" }]);
    const [header, item] = text.trim().split("\n").map((line) => JSON.parse(line));
    expect(header).toEqual({ kind: "header", format_version: 1, name: "rusty-studio-evaluations", version: "v1" });
    expect(item).toMatchObject({ kind: "case", id: "release", input: { objective: "Verify release" }, expect: { state: [{ pointer: "/answer", expected: "verified" }] } });
  });

  it("offers bounded recent work from the exact current connection", async () => {
    const connection = useConnectionStore.getState().connection!;
    rememberRecentWork(durableConnectionScope(connection), { threadId: "thread-old", runId: "run-old" });
    rememberRecentWork(durableConnectionScope({ ...connection, tenantFingerprint: "other" }), { threadId: "thread-other", runId: "run-other" });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/runs/run-old") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-old", thread_id: "thread-old", graph: "research", attempt: 1, status: "success" })));
      throw new Error(`unexpected ${path}`);
    }));
    renderPage();
    expect(await screen.findByRole("heading", { name: "Continue where you left off" })).toBeVisible();
    const recent = await screen.findByRole("link", { name: /research.*Work completed/i });
    expect(recent).toHaveAttribute("href", "/work/thread-old/runs/run-old");
    expect(screen.queryByText("run-other")).not.toBeInTheDocument();
  });

  it("keeps every large-trace step reachable while bounding each DOM window", () => {
    const large = Array.from({ length: 301 }, (_, index) => ({ ...events[0], id: `run-1:${index}`, seq: String(index) })) as unknown as RunEvent[];
    const first = traceWindow(large, 0);
    const last = traceWindow(large, 99);
    expect(first.items).toHaveLength(61);
    expect(first.items[0]).toMatchObject({ seq: "0" });
    expect(last).toMatchObject({ page: 2, pages: 3, start: 181, end: 301 });
    expect(last.items.at(-1)).toMatchObject({ seq: "300" });
    expect([first, traceWindow(large, 1), last].flatMap((window) => window.items).map((item) => item.seq)).toEqual(large.map((item) => item.seq));
    const boundary = Array.from({ length: 121 }, (_, index) => ({ ...events[0], id: `run-1:${index}`, parent: index ? `run-1:${index - 1}` : null, seq: String(index) })) as unknown as RunEvent[];
    expect(traceWindow(boundary, 0).items).toHaveLength(1);
    expect(traceWindow(boundary, 1).items).toHaveLength(120);
    expect(traceGraphLayout([boundary[120]], boundary)[0].x).toBeGreaterThan(26);
  });

  it("never attributes an older routed run to the newer session", async () => {
    useWorkStore.setState({ connectionKey: "1|https://rusty.example|a", assistant: { ...assistant, assistant_id: "agent-b", name: "Agent B" }, objective: "Objective B", thread: { thread_id: "thread-b", tenant: "default", graph: "research", metadata: { assistant_id: "agent-b" }, created_at: "2026-08-11T00:00:00Z" }, receipt: { run_id: "run-b", thread_id: "thread-b", status: "running" } });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/runs/run-a") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-a", thread_id: "thread-a", graph: "research", attempt: 1, status: "success" })));
      if (path === "/runs/run-a/events") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-a", events: [], complete: true })));
      throw new Error(`unexpected ${path}`);
    }));
    renderPage("/work/thread-a/runs/run-a/evaluate");
    expect(await screen.findByRole("heading", { name: "Turn this run into a reusable test" })).toBeVisible();
    expect(screen.getByLabelText("Frozen input")).toHaveValue("Input is available in the run evidence.");
    expect(screen.getByText("Run identity only")).toBeVisible();
    expect(screen.queryByText("Objective B")).not.toBeInTheDocument();
  });

  it("locks retry when launch acceptance is uncertain", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/threads" && init?.method === "POST") return Promise.reject(new Error("connection lost"));
      throw new Error(`unexpected ${path}`);
    }));
    renderPage();
    await userEvent.selectOptions(await screen.findByLabelText("Agent"), "agent-1");
    await userEvent.type(screen.getByLabelText("Goal"), "Verify");
    await userEvent.click(screen.getByRole("button", { name: "Start run" }));
    expect(await screen.findByRole("heading", { name: "Check Rusty before starting again" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Start run" })).not.toBeInTheDocument();
  });

  it("rejects a crossed run snapshot instead of rendering its journal", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/runs/run-1") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", thread_id: "thread-other", graph: "research", attempt: 1, status: "success" })));
      if (path === "/runs/run-1/events") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", events, complete: true })));
      throw new Error(`unexpected ${path}`);
    }));
    renderPage("/work/thread-1/runs/run-1");
    expect(await screen.findByRole("heading", { name: "This workspace could not prove the requested run" })).toBeVisible();
    expect(screen.queryByRole("list", { name: "Causal execution graph" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Start run" })).not.toBeInTheDocument();
    expect(screen.queryByText("Loading exact run evidence…")).not.toBeInTheDocument();
  });

  it("never exposes the fresh composer while a routed run is still loading", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path.startsWith("/runs/run-1")) return new Promise(() => {});
      throw new Error(`unexpected ${path}`);
    }));
    renderPage("/work/thread-1/runs/run-1");
    expect(await screen.findByText("Loading exact run evidence…")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Start run" })).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Goal")).not.toBeInTheDocument();
  });

  it("keeps an unrelated launch lock out of an exact existing run", async () => {
    const connection = useConnectionStore.getState().connection!;
    useWorkStore.setState({ uncertainByConnection: { [mutationScope(connection)]: "Unresolved newer launch" } });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/runs/run-1") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", thread_id: "thread-1", graph: "research", attempt: 1, status: "success" })));
      if (path === "/runs/run-1/events") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", events, complete: true })));
      throw new Error(`unexpected ${path}`);
    }));
    renderPage("/work/thread-1/runs/run-1");
    expect(await screen.findByRole("heading", { name: "Work is underway" })).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Check Rusty before starting again" })).not.toBeInTheDocument();
  });

  it("hides retained trace data when a refresh can no longer prove it", async () => {
    let failRun = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      if (path === "/runs/run-1") return failRun ? Promise.reject(new Error("offline")) : Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", thread_id: "thread-1", graph: "research", attempt: 1, status: "success" })));
      if (path === "/runs/run-1/events") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", events, complete: true })));
      throw new Error(`unexpected ${path}`);
    }));
    const { client } = renderPage("/work/thread-1/runs/run-1/trace");
    expect(await screen.findByRole("list", { name: "Causal execution graph" })).toBeVisible();
    failRun = true;
    await client.refetchQueries({ queryKey: [1, "https://rusty.example", "a", "run", "run-1"], exact: true });
    expect(await screen.findByRole("heading", { name: "This workspace could not prove the requested run" })).toBeVisible();
    expect(screen.queryByRole("list", { name: "Causal execution graph" })).not.toBeInTheDocument();
  });
});
