import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { OperationsPage } from "./OperationsPage";

function renderPage() {
  const root = createRootRoute({ component: Outlet });
  const operations = createRoute({ getParentRoute: () => root, path: "/operations", component: OperationsPage });
  const trace = createRoute({ getParentRoute: () => root, path: "/work/$threadId/runs/$runId/trace", component: () => <p>Trace destination</p> });
  const router = createRouter({ routeTree: root.addChildren([operations, trace]), history: createMemoryHistory({ initialEntries: ["/operations"] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

function response(value: unknown) { return new Response(JSON.stringify(value), { status: 200 }); }
const emptyArtifactJournal = { run_id: "artifact-journal", events: [], complete: false };

beforeEach(() => {
  useConnectionStore.setState({ connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" }, info: null, dialogOpen: false });
});
afterEach(() => vi.unstubAllGlobals());

describe("exception-led Operations", () => {
  it("opens actionable evidence and hands an owned run to Trace", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/tasks" && url.search === "?status=dead") return Promise.resolve(response([{
        task_id: "task-1", kind: "publish_report", pool: "default", status: "dead",
        last_error: "Provider rejected the write.", next_attempt_at: null,
        run_id: "run-1", thread_id: "thread-1", updated_at: "2026-08-11T00:00:00Z",
      }]));
      if (url.pathname === "/tasks") return Promise.resolve(response([]));
      if (url.pathname === "/crons") return Promise.resolve(response([{ cron_id: "daily" }]));
      if (url.pathname === "/triggers") return Promise.resolve(response([{ trigger_id: "hook", enabled: true }]));
      if (url.pathname === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      throw new Error(`unexpected ${url}`);
    }));
    renderPage();
    await waitFor(() => expect(screen.getByRole("heading", { name: "1 item" })).toBeVisible());
    expect(screen.getByText("Task failure queues and catalogs observed")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Review" }));
    expect(screen.getByRole("heading", { name: "publish_report exhausted its retries" })).toBeVisible();
    expect(screen.getAllByText("Provider rejected the write.")).toHaveLength(2);
    expect(screen.getByRole("link", { name: "Inspect contributing run" })).toHaveAttribute("href", "/work/thread-1/runs/run-1/trace");
    expect(screen.getByRole("link", { name: /Schedules/ })).toHaveAttribute("href", "/advanced/legacy?studio=schedules");
  });

  it("never presents missing evidence as healthy", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/tasks") return Promise.reject(new Error("offline"));
      if (url.pathname === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await waitFor(() => expect(screen.getByText("Not observed: task queue")).toBeVisible());
    expect(screen.getByText("No task failures need action")).toBeVisible();
    expect(screen.queryByText("Task failure queues and catalogs observed")).not.toBeInTheDocument();
  });

  it("closes selected evidence when connection ownership changes", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/tasks" && url.search === "?status=dead") return Promise.resolve(response([{ task_id: "task-a", kind: "publish", pool: "default", status: "dead", last_error: "tenant A", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" }]));
      if (url.pathname === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Review" }));
    expect(screen.getByRole("heading", { name: "publish exhausted its retries" })).toBeVisible();
    useConnectionStore.setState({ connection: { epoch: 2, origin: "https://rusty.example", apiKey: "key-b", tenantFingerprint: "b" } });
    await waitFor(() => expect(screen.queryByRole("heading", { name: "publish exhausted its retries" })).not.toBeInTheDocument());
  });

  it("removes selected evidence when a refresh proves the task recovered", async () => {
    let recovered = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input);
      if (url.pathname === "/tasks" && url.search === "?status=dead" && !recovered) return Promise.resolve(response([{ task_id: "task-a", kind: "publish", pool: "default", status: "dead", last_error: "tenant A", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" }]));
      if (url.pathname === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Review" }));
    expect(screen.getByRole("heading", { name: "publish exhausted its retries" })).toBeVisible();
    recovered = true;
    await userEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(screen.queryByRole("heading", { name: "publish exhausted its retries" })).not.toBeInTheDocument());
  });
});
