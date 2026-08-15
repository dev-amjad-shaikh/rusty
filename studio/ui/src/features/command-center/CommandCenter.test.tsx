import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { durableConnectionScope, rememberRecentWork } from "../../state/recentWork";
import { useWorkStore } from "../../state/work";
import { CommandCenter } from "./CommandCenter";

const connection = { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "tenant" };

function renderCenter() {
  const root = createRootRoute();
  const command = createRoute({ getParentRoute: () => root, path: "/", component: CommandCenter });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: () => <p>Work</p> });
  const run = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId", component: () => <p>Run</p> });
  const trace = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId/trace", component: () => <p>Trace</p> });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: () => <p>Agents</p> });
  const newAgent = createRoute({ getParentRoute: () => root, path: "/agents/new", component: () => <p>New agent</p> });
  const operations = createRoute({ getParentRoute: () => root, path: "/operations", component: () => <p>Operations</p> });
  const router = createRouter({ routeTree: root.addChildren([command, work, run, trace, agents, newAgent, operations]), history: createMemoryHistory({ initialEntries: ["/"] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

function response(value: unknown) { return Promise.resolve(new Response(JSON.stringify(value), { status: 200 })); }
function run(runId: string, threadId: string, status: "pending" | "running" | "success" | "interrupted" | "error" | "cancelled") {
  return { run_id: runId, thread_id: threadId, graph: "react_agent", assistant_id: "agent-1", metadata: { studio: { objective: `${status} customer request` } }, attempt: 0, status, message: status === "interrupted" ? "Approval required before continuing." : undefined, error: status === "error" ? "Provider timed out after retry." : undefined };
}
const emptyJournal = { run_id: "artifact-journal", events: [], complete: false };

beforeEach(() => {
  sessionStorage.clear();
  useConnectionStore.setState({ connection, info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "react_agent", channels: [] }] }, workspaceStatus: "ready", dialogOpen: false });
  useWorkStore.getState().clear();
});
afterEach(() => { vi.unstubAllGlobals(); sessionStorage.clear(); });

describe("v4 Command Center", () => {
  it("groups exact recent runs and current exceptions without inventing a tenant-wide catalog", async () => {
    const user = userEvent.setup();
    const scope = durableConnectionScope(connection);
    for (const id of ["run-pending", "run-running", "run-success", "run-interrupted", "run-error", "run-cancelled"]) rememberRecentWork(scope, { threadId: `thread-${id}`, runId: id });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants") return response([{ assistant_id: "agent-1", name: "Research agent", graph: "react_agent", config: {}, metadata: {}, created_at: "2026-08-11T00:00:00Z", active_version_id: "version-1", version_count: 1 }]);
      if (url.pathname.startsWith("/runs/")) { const id = decodeURIComponent(url.pathname.slice(6)); const status = id.replace("run-", "") as "pending" | "running" | "success" | "interrupted" | "error" | "cancelled"; return response(run(id, `thread-${id}`, status)); }
      if (url.pathname === "/tasks" && url.search === "?status=dead") return response([{ task_id: "task-1", kind: "publish_report", pool: "default", status: "dead", last_error: "Provider rejected the write.", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" }]);
      if (url.pathname === "/tasks") return response([]);
      if (url.pathname === "/crons") return response([{ cron_id: "daily" }]);
      if (url.pathname === "/triggers") return response([{ trigger_id: "webhook", enabled: true }]);
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      throw new Error(`unexpected ${url}`);
    }));
    renderCenter();
    expect(await screen.findByRole("heading", { name: "Work board" })).toBeVisible();
    expect(screen.getByRole("heading", { level: 2, name: "Queued" })).toBeVisible();
    expect(await within(screen.getByRole("region", { name: "Queued" })).findByRole("link", { name: /pending customer request/ })).toBeVisible();
    expect(await within(screen.getByRole("region", { name: "Working" })).findByRole("link", { name: /running customer request/ })).toBeVisible();
    const needs = screen.getByRole("region", { name: "Needs you" });
    expect(await within(needs).findByRole("link", { name: /publish_report exhausted its retries/ })).toBeVisible();
    expect(await within(needs).findByRole("link", { name: /interrupted customer request/ })).toBeVisible();
    expect(within(needs).getByText("Approval required before continuing.")).toBeVisible();
    expect(await within(screen.getByRole("region", { name: "Stuck" })).findByRole("link", { name: /error customer request/ })).toBeVisible();
    expect(within(screen.getByRole("region", { name: "Stuck" })).getByText("Provider timed out after retry.")).toBeVisible();
    expect(await within(screen.getByRole("region", { name: "Done" })).findByRole("link", { name: /success customer request/ })).toBeVisible();
    expect(await within(screen.getByRole("region", { name: "Done" })).findByRole("link", { name: /cancelled customer request/ })).toBeVisible();
    expect(screen.getByText("Session runs and current operational exceptions.")).toBeVisible();
    expect(screen.getByRole("link", { name: /running customer request/ }).closest('[data-rusty-card="forged"]')).toHaveAttribute("data-tone", "working");
    await user.click(screen.getByRole("button", { name: "Active" }));
    expect(screen.getByRole("button", { name: "Active" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("link", { name: /running customer request/ })).toBeVisible();
    expect(screen.queryByRole("link", { name: /success customer request/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /publish_report exhausted its retries/ })).not.toBeInTheDocument();
    expect(screen.getByText("1 running · 2 need you · 1 stuck")).toBeVisible();
    expect(screen.getByText("Now: 3 blocked")).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Agent portfolio" })).not.toBeInTheDocument();
  });

  it("does not admit a crossed run identity and discloses the missing proof", async () => {
    rememberRecentWork(durableConnectionScope(connection), { threadId: "thread-a", runId: "run-a" });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants") return response([]);
      if (url.pathname === "/runs/run-a") return response(run("run-crossed", "thread-a", "success"));
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      return response([]);
    }));
    renderCenter();
    expect(await screen.findByText(/1 crossed run identity could not be verified/)).toBeVisible();
    expect(screen.getByText("Work status incomplete")).toBeVisible();
    expect(screen.queryByText(/Now:/)).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /success customer request/ })).not.toBeInTheDocument();
  });

  it("reports unavailable operational sources instead of an all-clear", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants") return response([]);
      if (url.pathname === "/tasks") return Promise.reject(new Error("offline"));
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      return response([]);
    }));
    renderCenter();
    expect(await screen.findByText(/task queue could not be verified/)).toBeVisible();
    expect(screen.getByText("0 running · attention status incomplete")).toBeVisible();
    expect(screen.queryByText(/all clear/i)).not.toBeInTheDocument();
  });

  it("keeps the Needs you lane bounded and hands off every additional exception", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants") return response([]);
      if (url.pathname === "/tasks" && url.search === "?status=dead") return response(Array.from({ length: 8 }, (_, index) => ({ task_id: `task-${index}`, kind: `job_${index}`, pool: "default", status: "dead", last_error: "Stopped.", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" })));
      if (url.pathname === "/tasks") return response([]);
      if (url.pathname === "/crons" || url.pathname === "/triggers") return response([]);
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      throw new Error(`unexpected ${url}`);
    }));
    renderCenter();
    const lane = await screen.findByRole("region", { name: "Needs you" });
    expect(await within(lane).findAllByRole("link", { name: /Review job_/ })).toHaveLength(6);
    expect(within(lane).getByRole("link", { name: "Review 2 more in Operations" })).toHaveAttribute("href", "/operations");
    expect(screen.getByText("0 running · 8 need you · 0 stuck")).toBeVisible();
  });

  it("offers useful local work before a workspace is connected", async () => {
    useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", dialogOpen: false });
    renderCenter();
    expect(await screen.findByRole("heading", { name: "Work board" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Start a local draft" })).toHaveAttribute("href", "/agents");
    expect(screen.getByRole("button", { name: "Choose workspace" })).toBeVisible();
  });

  it("turns a verified empty board into a next action instead of five captions", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants" || url.pathname === "/tasks" || url.pathname === "/crons" || url.pathname === "/triggers") return response([]);
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      throw new Error(`unexpected ${url}`);
    }));
    renderCenter();
    expect(await screen.findByRole("heading", { name: "Build your first agent" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Create first agent" })).toHaveAttribute("href", "/agents/new");
    expect(screen.queryByRole("heading", { name: "Queued" })).not.toBeInTheDocument();
    expect(screen.queryByText("No recent work is waiting.")).not.toBeInTheDocument();
  });

  it("hands a verified available agent from the empty board into its first task", async () => {
    const user = userEvent.setup();
    const agent = { assistant_id: "agent-1", name: "Research agent", graph: "react_agent", config: {}, metadata: {}, created_at: "2026-08-11T00:00:00Z", active_version_id: "version-1", version_count: 1 };
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants") return response([agent]);
      if (url.pathname === "/tasks" || url.pathname === "/crons" || url.pathname === "/triggers") return response([]);
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      throw new Error(`unexpected ${url}`);
    }));
    renderCenter();
    expect(await screen.findByRole("heading", { name: "Ready for the first task" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Start with Research agent" }));
    expect(await screen.findByText("Work")).toBeVisible();
    expect(useWorkStore.getState().assistant).toMatchObject({ assistant_id: "agent-1", active_version_id: "version-1" });
  });

  it("explains an empty filtered projection and returns to all work", async () => {
    const user = userEvent.setup();
    rememberRecentWork(durableConnectionScope(connection), { threadId: "thread-running", runId: "run-running" });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/assistants" || url.pathname === "/tasks" || url.pathname === "/crons" || url.pathname === "/triggers") return response([]);
      if (url.pathname === "/runs/run-running") return response(run("run-running", "thread-running", "running"));
      if (url.pathname === "/artifacts/journal") return response(emptyJournal);
      throw new Error(`unexpected ${url}`);
    }));
    renderCenter();
    await screen.findByRole("link", { name: /running customer request/ });
    await user.click(screen.getByRole("button", { name: "Needs attention" }));
    expect(screen.getByRole("heading", { name: "Nothing needs attention" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Show all work" }));
    expect(screen.getByRole("link", { name: /running customer request/ })).toBeVisible();
    await waitFor(() => expect(screen.getByRole("button", { name: "All work" })).toHaveFocus());
  });
});
