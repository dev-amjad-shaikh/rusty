import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../../state/connection";
import { ArtifactTray } from "./ArtifactTray";

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
  it("lists run artifacts and opens the inspector", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/artifacts") return Promise.resolve(new Response(JSON.stringify({ artifacts: [artifact] })));
      if (path === `/artifacts/${"a".repeat(64)}/preview`) return Promise.resolve(new Response(JSON.stringify({ artifact_id: "a".repeat(64), preview: { kind: "text", text: "hello world", truncated: false, source_bytes: 42 } })));
      throw new Error(`unexpected ${path}`);
    }));
    renderTray();
    await waitFor(() => expect(screen.getByRole("button", { name: /weekly-report/ })).toBeVisible());
    expect(screen.getByText("42 B · text/plain")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: /weekly-report/ }));
    await waitFor(() => expect(screen.getByRole("dialog")).toBeVisible());
    expect(screen.getByRole("heading", { name: "weekly-report" })).toBeVisible();
    expect(screen.getByText("hello world")).toBeVisible();
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
