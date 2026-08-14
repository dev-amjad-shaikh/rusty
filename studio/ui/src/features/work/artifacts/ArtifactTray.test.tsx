import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../../state/connection";
import { ArtifactTray } from "./ArtifactTray";
import { releaseOutcomeCopy } from "./ArtifactInspector";

function renderTray(runId = "run-1") {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><ArtifactTray runId={runId} /></QueryClientProvider>);
}

const artifact = {
  artifact_id: "a".repeat(64),
  name: "weekly-report",
  media_kind: "file" as const,
  media_type: "text/plain",
  lineage: { run_id: "run-1", effect_id: { kind: "tool_call", node_id: "writer" }, event_id: "run-1:3" },
  versions: [{ sha256: "a".repeat(64), bytes: 42, committed_at: "2026-08-11T00:00:00Z" }],
  retention: { policy: "receipt_bound" as const },
  created_at: "2026-08-11T00:00:00Z",
};

beforeEach(() => {
  useConnectionStore.setState({ connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" } });
});
afterEach(() => vi.unstubAllGlobals());

describe("ArtifactTray", () => {
  it("describes every release convergence and pruning outcome exactly", () => {
    expect(releaseOutcomeCopy({ converged: false, pruned: false })).toBe("Release recorded. This call did not prove that stored bytes were removed; retention cleanup will retry if needed.");
    expect(releaseOutcomeCopy({ converged: false, pruned: true })).toBe("Release recorded and stored bytes were removed.");
    expect(releaseOutcomeCopy({ converged: true, pruned: false })).toBe("Release was already recorded. This retry did not remove additional stored bytes.");
    expect(releaseOutcomeCopy({ converged: true, pruned: true })).toBe("Release was already recorded. This retry removed the stored bytes.");
  });

  it("lists run artifacts and opens the inspector", async () => {
    let releaseCalls = 0;
    let previewCalls = 0;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/artifacts") return Promise.resolve(new Response(JSON.stringify({ artifacts: [artifact] })));
      if (path === `/artifacts/${"a".repeat(64)}/preview`) { previewCalls += 1; return Promise.resolve(new Response(JSON.stringify({ artifact_id: "a".repeat(64), preview: { kind: "text", text: "hello world", truncated: false, source_bytes: 42 } }))); }
      if (path === `/artifacts/${"a".repeat(64)}/release`) return Promise.resolve(new Response(JSON.stringify({ artifact_id: "a".repeat(64), released: true, converged: releaseCalls > 0, pruned: releaseCalls++ === 0, journal_event_id: "run-1:4" })));
      if (path === "/artifacts/names/weekly-report") return new Promise(() => {});
      throw new Error(`unexpected ${path}`);
    }));
    renderTray();
    await waitFor(() => expect(screen.getByRole("button", { name: /weekly-report/ })).toBeVisible());
    expect(screen.getByText("42 B · text/plain")).toBeVisible();
    const opener = screen.getByRole("button", { name: /weekly-report/ });
    await userEvent.click(opener);
    await waitFor(() => expect(screen.getByRole("dialog")).toBeVisible());
    expect(screen.getByRole("heading", { name: "weekly-report" })).toBeVisible();
    expect(screen.getByText("hello world")).toBeVisible();
    await waitFor(() => expect(screen.getByRole("button", { name: "Close" })).toHaveFocus());
    await userEvent.tab({ shift: true });
    expect(screen.getByRole("button", { name: "Download exact bytes" })).toHaveFocus();
    await userEvent.tab();
    expect(screen.getByRole("button", { name: "Close" })).toHaveFocus();
    await userEvent.click(screen.getByRole("button", { name: "Release" }));
    await userEvent.type(screen.getByLabelText("Author"), "reviewer");
    await userEvent.click(screen.getByRole("button", { name: "Release stored bytes" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Release recorded and stored bytes were removed");
    expect(screen.queryByText("hello world")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stored bytes removed" })).toBeDisabled();
    await waitFor(() => expect(screen.getByRole("button", { name: "Preview" })).toHaveFocus());
    await userEvent.click(screen.getByRole("button", { name: "Release" }));
    expect(screen.getAllByText("Release recorded and stored bytes were removed.")).toHaveLength(2);
    await userEvent.type(screen.getByLabelText("Author"), "reviewer");
    await userEvent.click(screen.getByRole("button", { name: "Release stored bytes" }));
    await waitFor(() => expect(screen.getAllByText("Release was already recorded. This retry did not remove additional stored bytes.")).toHaveLength(2));
    expect(screen.getByRole("button", { name: "Stored bytes removed" })).toBeDisabled();
    expect(screen.queryByText("hello world")).not.toBeInTheDocument();
    expect(previewCalls).toBe(1);
    await waitFor(() => expect(screen.getByRole("button", { name: "Preview" })).toHaveFocus());
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(opener).toHaveFocus());
  });

  it("shows an empty state when the run produced no artifacts", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/artifacts") return Promise.resolve(new Response(JSON.stringify({ artifacts: [] })));
      throw new Error(`unexpected ${path}`);
    }));
    renderTray();
    await waitFor(() => expect(screen.getByText("No artifacts were committed for this run.")).toBeVisible());
  });

  it("shows an unavailable state when artifact loading fails", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/artifacts") return Promise.resolve(new Response(JSON.stringify({ error: "offline" }), { status: 503 }));
      throw new Error(`unexpected ${path}`);
    }));
    renderTray();
    await waitFor(() => expect(screen.getByText("Could not load outputs for this run.")).toBeVisible());
  });
});
