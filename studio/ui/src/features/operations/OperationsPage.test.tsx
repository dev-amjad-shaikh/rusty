import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
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
const unavailableArtifactId = "a".repeat(64);
const unavailableArtifactJournal = { run_id: "artifact-journal", complete: true, events: [{ id: "artifact-journal:0", run_id: "artifact-journal", thread_id: "thread-artifact", node_id: null, seq: "0", kind: "artifact_unavailable", effect: "pure", input: null, output: { artifact_id: unavailableArtifactId, surface: "run-output" }, latency_ms: null, tokens: null, cost_usd: null, status: "error", parent: null, recorded_at: "2026-08-11T00:00:00Z", rawJson: "{}" }] };
const unavailableArtifact = { artifact_id: unavailableArtifactId, media_kind: "file", media_type: "text/plain", lineage: { run_id: "run-artifact", effect_id: { kind: "tool_call" }, event_id: "run-artifact:2" }, versions: [{ sha256: unavailableArtifactId, bytes: 42, committed_at: "2026-08-11T00:00:00Z" }], retention: { policy: "receipt_bound" }, created_at: "2026-08-11T00:00:00Z" };

afterEach(() => vi.unstubAllGlobals());

describe("exception-led Operations", () => {
  it("opens actionable evidence and hands an owned run to Trace", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=dead") return Promise.resolve(response([{
        task_id: "task-1", kind: "publish_report", pool: "default", status: "dead",
        last_error: "Provider rejected the write.", next_attempt_at: null,
        run_id: "run-1", thread_id: "thread-1", updated_at: "2026-08-11T00:00:00Z",
      }]));
      if (url.pathname.replace(/^\/api/, "") === "/tasks") return Promise.resolve(response([]));
      if (url.pathname.replace(/^\/api/, "") === "/crons") return Promise.resolve(response([{ cron_id: "daily" }]));
      if (url.pathname.replace(/^\/api/, "") === "/triggers") return Promise.resolve(response([{ trigger_id: "hook", enabled: true }]));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      throw new Error(`unexpected ${url}`);
    }));
    renderPage();
    await waitFor(() => expect(screen.getByRole("heading", { name: "1 item" })).toBeVisible());
    expect(screen.getByText("All sources checked")).toBeVisible();
    const review = screen.getByRole("button", { name: "Review" });
    await userEvent.click(review);
    const evidenceHeading = screen.getByRole("heading", { name: "publish_report exhausted its retries" });
    expect(evidenceHeading).toBeVisible();
    expect(evidenceHeading).toHaveFocus();
    expect(screen.getAllByText("Provider rejected the write.")).toHaveLength(2);
    expect(screen.getByRole("link", { name: "Inspect contributing run" })).toHaveAttribute("href", "/work/thread-1/runs/run-1/trace");
    expect(screen.getByText("Schedules").closest("article")).toBeVisible();
    expect(screen.queryByRole("link", { name: /Schedules/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Close" }));
    await waitFor(() => expect(review).toHaveFocus());
  });

  it("never presents missing evidence as healthy", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks") return Promise.reject(new Error("offline"));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await waitFor(() => expect(screen.getByText("Unavailable: task queue")).toBeVisible());
    expect(screen.getByText("Task status could not be verified")).toBeVisible();
    expect(screen.getByText("Refresh to check for work that may need attention.")).toBeVisible();
    expect(screen.queryByText("No task failures need action")).not.toBeInTheDocument();
  });

  it("closes selected evidence when a refresh replaces it", async () => {
    let replaced = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=dead") return Promise.resolve(response(replaced
        ? [{ task_id: "task-b", kind: "publish", pool: "default", status: "dead", last_error: "new failure", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:01:00Z" }]
        : [{ task_id: "task-a", kind: "publish", pool: "default", status: "dead", last_error: "tenant A", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" }]));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Review" }));
    expect(screen.getByRole("heading", { name: "publish exhausted its retries" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "publish exhausted its retries" })).toHaveFocus();
    replaced = true;
    await userEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(screen.queryByRole("heading", { name: "publish exhausted its retries" })).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Refresh" })).toHaveFocus();
  });

  it("does not steal focus when refreshed evidence updates the selected item", async () => {
    let refreshed = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=dead") return Promise.resolve(response([{ task_id: "task-a", kind: "publish", pool: "default", status: "dead", last_error: refreshed ? "new evidence" : "old evidence", next_attempt_at: null, run_id: null, thread_id: null, updated_at: refreshed ? "2026-08-11T00:01:00Z" : "2026-08-11T00:00:00Z" }]));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Review" }));
    expect(screen.getByRole("heading", { name: "publish exhausted its retries" })).toHaveFocus();
    refreshed = true;
    const refresh = screen.getByRole("button", { name: "Refresh" });
    await userEvent.click(refresh);
    await waitFor(() => expect(screen.getAllByText("new evidence")).toHaveLength(2));
    expect(refresh).toHaveFocus();
  });

  it("moves focus to a stable heading when an owned artifact inspector is dismissed", async () => {
    let recovered = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response(recovered ? emptyArtifactJournal : unavailableArtifactJournal));
      if (url.pathname.replace(/^\/api/, "") === `/artifacts/${unavailableArtifactId}`) return Promise.resolve(response(unavailableArtifact));
      if (url.pathname.replace(/^\/api/, "") === `/artifacts/${unavailableArtifactId}/preview`) return Promise.resolve(response({ artifact_id: unavailableArtifactId, preview: { kind: "empty", reason: "Stored bytes unavailable" } }));
      return Promise.resolve(response([]));
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Review" }));
    const inspector = await screen.findByRole("dialog");
    await waitFor(() => expect(within(inspector).getByRole("button", { name: "Close" })).toHaveFocus());
    recovered = true;
    await userEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Refresh" })).toHaveFocus();
  });

  it("removes selected evidence when a refresh proves the task recovered", async () => {
    let recovered = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=dead" && !recovered) return Promise.resolve(response([{ task_id: "task-a", kind: "publish", pool: "default", status: "dead", last_error: "tenant A", next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" }]));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response(emptyArtifactJournal));
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
