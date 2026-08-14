import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { evaluationDatasetJsonl, useWorkStore } from "../../state/work";
import { durableConnectionScope, readRecentWork, rememberRecentWork } from "../../state/recentWork";
import { mutationScope } from "../../lib/api/client";
import type { RunEvent } from "../../lib/contracts";
import { traceGraphLayout, traceWindow, WorkPage } from "./WorkPage";

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
  return { ...render(<StrictMode><QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider></StrictMode>), router, client };
}

const assistant = { assistant_id: "agent-1", name: "Research analyst", graph: "research", config: {}, metadata: {}, created_at: "2026-08-11T00:00:00Z", active_version_id: "av-1", version_count: 1 };
const events = [
  { id: "run-1:0", run_id: "run-1", thread_id: "thread-1", node_id: "research", seq: 0, kind: "node_input", effect: "pure", input: { inline: { objective: "Verify" } }, output: null, latency_ms: null, tokens: null, cost_usd: null, status: "ok", parent: null, recorded_at: "2026-08-11T00:00:00Z" },
  { id: "run-1:1", run_id: "run-1", thread_id: "thread-1", node_id: "research", seq: 1, kind: "model_call", effect: "non_idempotent", input: null, output: { inline: { text: "done" } }, latency_ms: 12, tokens: { prompt_tokens: 8, completion_tokens: 4, total_tokens: 12 }, cost_usd: 0.001, status: "ok", parent: "run-1:0", recorded_at: "2026-08-11T00:00:01Z" },
  { id: "run-1:2", run_id: "run-1", thread_id: "thread-1", node_id: null, seq: 2, kind: "node_output", effect: "pure", input: null, output: { inline: { answer: "done" } }, latency_ms: 3, tokens: null, cost_usd: null, status: "ok", parent: "run-1:1", recorded_at: "2026-08-11T00:00:02Z" },
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
      if (path === "/runs/run-1") return Promise.resolve(new Response(JSON.stringify({ run_id: "run-1", thread_id: "thread-1", graph: "research", assistant_id: "agent-1", metadata: { studio: { objective: "Verify the release claim" } }, attempt: 1, status: "success", output: { answer: "done" } })));
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
    const launchRequest = fetchMock.mock.calls.find(([input, init]) => new URL(String(input)).pathname === "/threads/thread-1/runs" && init?.method === "POST");
    expect(JSON.parse(String(launchRequest?.[1]?.body))).toMatchObject({
      assistant_id: "agent-1",
      expected_active_version_id: "av-1",
      input: { objective: "Verify the release claim" },
    });
    await waitFor(() => expect(screen.getByRole("button", { name: /Inspect trace|Follow trace/ })).toBeVisible());
    expect(screen.getByText("Sequence 2")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: /Inspect trace|Follow trace/ }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Work completed" })).toBeVisible());
    expect(screen.getByRole("button", { name: "research · model call, sequence 1, status ok" })).toBeVisible();
    expect(container.querySelectorAll("svg path")).toHaveLength(2);
    expect(HTMLElement.prototype.scrollTo).toHaveBeenCalledWith(expect.objectContaining({ left: expect.any(Number), top: expect.any(Number) }));
    await userEvent.click(screen.getByRole("button", { name: "Evaluate this run" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Turn this run into a reusable test" })).toBeVisible());
    expect(screen.getByLabelText("Frozen input")).toHaveValue("Verify the release claim");
    expect(screen.getByLabelText("Final-state path")).toHaveValue("/answer");
    expect(screen.getByLabelText(/Expected value/)).toHaveValue('"done"');
    await userEvent.clear(screen.getByLabelText(/Expected value/));
    await userEvent.type(screen.getByLabelText(/Expected value/), '"The release claim is verified."');
    await userEvent.click(screen.getByRole("checkbox"));
    await userEvent.click(screen.getByRole("button", { name: "Add evaluation case" }));
    expect(screen.getByRole("heading", { name: "1 reviewed case" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "Prove the next version is better" })).toBeVisible();
    expect(useWorkStore.getState().cases[0]).toMatchObject({ pointer: "/answer", expected: "The release claim is verified." });
  });

  it("exports the page-memory dataset in Rust evaluation JSONL shape", () => {
    const text = evaluationDatasetJsonl([{ connectionKey: "1|https://rusty.example|a", id: "local-1", caseId: "release", runId: "run-1", threadId: "thread-1", agentName: "Analyst", agentId: "assistant-analyst", objective: "Verify release", pointer: "/answer", expected: "verified", createdAt: "2026-08-11T00:00:00Z" }]);
    const [header, item] = text.trim().split("\n").map((line: string) => JSON.parse(line));
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

  it("resets every evaluation draft and acknowledgement when the exact run changes", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant])));
      const match = path.match(/^\/runs\/(run-[ab])(?:\/events)?$/);
      if (!match) throw new Error(`unexpected ${path}`);
      const runId = match[1], threadId = runId === "run-a" ? "thread-a" : "thread-b";
      if (path.endsWith("/events")) {
        const exactEvents = events.map((event, index) => ({ ...event, id: `${runId}:${index}`, run_id: runId, thread_id: threadId, parent: index ? `${runId}:${index - 1}` : null }));
        return Promise.resolve(new Response(JSON.stringify({ run_id: runId, events: exactEvents, complete: true })));
      }
      return Promise.resolve(new Response(JSON.stringify({ run_id: runId, thread_id: threadId, graph: "research", assistant_id: "agent-1", metadata: { studio: { objective: `Objective ${runId.at(-1)?.toUpperCase()}` } }, attempt: 1, status: "success", output: { answer: runId === "run-a" ? "A" : "B" } })));
    }));
    const { router } = renderPage("/work/thread-a/runs/run-a/evaluate");
    expect(await screen.findByLabelText("Case name")).toHaveValue("run-run-a");
    await userEvent.clear(screen.getByLabelText("Case name"));
    await userEvent.type(screen.getByLabelText("Case name"), "edited-a");
    await userEvent.click(screen.getByRole("checkbox"));
    expect(screen.getByRole("checkbox")).toBeChecked();

    await router.navigate({ to: "/work/$threadId/runs/$runId/evaluate", params: { threadId: "thread-b", runId: "run-b" } });
    await waitFor(() => expect(screen.getByLabelText("Case name")).toHaveValue("run-run-b"));
    expect(screen.getByLabelText("Frozen input")).toHaveValue("Objective B");
    expect(screen.getByLabelText(/Expected value/)).toHaveValue('"B"');
    expect(screen.getByRole("checkbox")).not.toBeChecked();
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

  it("expires a stale prepared version after a definite server conflict", async () => {
    const current = { ...assistant, active_version_id: "av-2", version_count: 2 };
    useWorkStore.getState().prepare("1|https://rusty.example|a", assistant);
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([current])));
      if (path === "/threads" && init?.method === "POST") return Promise.resolve(new Response(JSON.stringify({ thread_id: "thread-stale", tenant: "default", graph: "research", metadata: { assistant_id: "agent-1" }, created_at: "2026-08-11T00:00:00Z" }), { status: 201 }));
      if (path === "/threads/thread-stale/runs" && init?.method === "POST") return Promise.resolve(new Response(JSON.stringify({ error: "assistant version changed" }), { status: 409 }));
      throw new Error(`unexpected ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPage();
    await userEvent.type(await screen.findByLabelText("Goal"), "Verify the current release");
    await userEvent.click(screen.getByRole("button", { name: "Start run" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("Review the current active version");
    expect(screen.queryByRole("heading", { name: "Check Rusty before starting again" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("Agent")).toHaveValue("");
    expect(useWorkStore.getState().assistant).toBeNull();
    expect(useWorkStore.getState().uncertainByConnection).toEqual({});
    const launchBody = JSON.parse(String(fetchMock.mock.calls.find(([input, init]) => new URL(String(input)).pathname.endsWith("/runs") && init?.method === "POST")?.[1]?.body));
    expect(launchBody.expected_active_version_id).toBe("av-1");
  });

  it("does not let an unmounted launch success replace a newer same-connection handoff", async () => {
    let finishRun!: (response: Response) => void;
    const pendingRun = new Promise<Response>((resolve) => { finishRun = resolve; });
    const agentB = { ...assistant, assistant_id: "agent-2", name: "Policy reviewer", active_version_id: "av-b" };
    useWorkStore.getState().prepare("1|https://rusty.example|a", assistant);
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant, agentB])));
      if (path === "/threads" && init?.method === "POST") return Promise.resolve(new Response(JSON.stringify({ thread_id: "thread-a", tenant: "default", graph: "research", metadata: { assistant_id: "agent-1" }, created_at: "2026-08-11T00:00:00Z" }), { status: 201 }));
      if (path === "/threads/thread-a/runs" && init?.method === "POST") return pendingRun;
      throw new Error(`unexpected ${path}`);
    }));
    const view = renderPage();
    await userEvent.type(await screen.findByLabelText("Goal"), "Verify A");
    await userEvent.click(screen.getByRole("button", { name: "Start run" }));
    view.unmount();
    useWorkStore.getState().prepare("1|https://rusty.example|a", agentB);

    finishRun(new Response(JSON.stringify({ run_id: "run-a", thread_id: "thread-a", status: "running" }), { status: 202 }));
    await waitFor(() => expect(useWorkStore.getState().assistant?.assistant_id).toBe("agent-2"));
    expect(useWorkStore.getState().receipt).toBeNull();
    expect(readRecentWork('["https://rusty.example","a"]')).toEqual([expect.objectContaining({ runId: "run-a", threadId: "thread-a" })]);
  });

  it("does not let an unmounted stale-version rejection expire a newer handoff", async () => {
    let finishRun!: (response: Response) => void;
    const pendingRun = new Promise<Response>((resolve) => { finishRun = resolve; });
    const agentB = { ...assistant, assistant_id: "agent-2", name: "Policy reviewer", active_version_id: "av-b" };
    useWorkStore.getState().prepare("1|https://rusty.example|a", assistant);
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (path === "/assistants") return Promise.resolve(new Response(JSON.stringify([assistant, agentB])));
      if (path === "/threads" && init?.method === "POST") return Promise.resolve(new Response(JSON.stringify({ thread_id: "thread-a", tenant: "default", graph: "research", metadata: { assistant_id: "agent-1" }, created_at: "2026-08-11T00:00:00Z" }), { status: 201 }));
      if (path === "/threads/thread-a/runs" && init?.method === "POST") return pendingRun;
      throw new Error(`unexpected ${path}`);
    }));
    const view = renderPage();
    await userEvent.type(await screen.findByLabelText("Goal"), "Verify A");
    await userEvent.click(screen.getByRole("button", { name: "Start run" }));
    view.unmount();
    useWorkStore.getState().prepare("1|https://rusty.example|a", agentB);

    finishRun(new Response(JSON.stringify({ error: "assistant version changed" }), { status: 409 }));
    await waitFor(() => expect(useWorkStore.getState().assistant?.assistant_id).toBe("agent-2"));
    expect(useWorkStore.getState().uncertainByConnection).toEqual({});
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
    expect(screen.queryByRole("button", { name: /research · model call/i })).not.toBeInTheDocument();
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
    expect(await screen.findAllByRole("heading", { name: "Work completed" })).toHaveLength(2);
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
    expect(await screen.findByRole("button", { name: /research · model call/i })).toBeVisible();
    failRun = true;
    await client.refetchQueries({ queryKey: [1, "https://rusty.example", "a", "run", "run-1"], exact: true });
    expect(await screen.findByRole("heading", { name: "This workspace could not prove the requested run" })).toBeVisible();
    expect(screen.queryByRole("button", { name: /research · model call/i })).not.toBeInTheDocument();
  });
});
