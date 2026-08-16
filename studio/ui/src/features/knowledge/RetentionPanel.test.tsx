import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  applyKnowledgeRetention,
  listKnowledgeSources,
  planKnowledgeRetention,
} from "../../lib/api/knowledge";
import { emptyLibrary, HASH_A, HASH_B, testConnection } from "./fixtures";
import { RetentionPanel } from "./RetentionPanel";

vi.mock("../../lib/api/knowledge", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api/knowledge")>();
  return {
    ...actual,
    listKnowledgeSources: vi.fn(),
    getKnowledgeSource: vi.fn(),
    getKnowledgeChunk: vi.fn(),
    registerKnowledgeSource: vi.fn(),
    correctKnowledgeSource: vi.fn(),
    queryKnowledge: vi.fn(),
    planKnowledgeRetention: vi.fn(),
    applyKnowledgeRetention: vi.fn(),
  };
});

function renderPanel() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><RetentionPanel connection={testConnection} /></QueryClientProvider>);
}

const plan = {
  entries: [{
    source_id: "fx-rates",
    source_hash: HASH_A,
    body_hash: HASH_B,
    scope: { scope: "tenant" as const, id: "acme" },
    title: "FX rates",
    version: 2,
    expires_at: "2026-06-01T00:00:00Z",
    chunk_count: 4,
    chunk_bytes: 2048,
  }],
  total_chunk_bytes: 2048,
};

const receipt = {
  plan,
  tombstones: [{
    source_id: "fx-rates",
    scope: { scope: "tenant" as const, id: "acme" },
    title: "FX rates",
    purged_hashes: [HASH_A],
    reason: "expired" as const,
    purged_at: "2026-09-01T00:00:00Z",
  }],
};

beforeEach(() => {
  vi.mocked(listKnowledgeSources).mockReset().mockResolvedValue(emptyLibrary());
  vi.mocked(planKnowledgeRetention).mockReset();
  vi.mocked(applyKnowledgeRetention).mockReset();
});

describe("Retention", () => {
  it("gates the sweep behind a computed plan and an explicit confirm", async () => {
    vi.mocked(planKnowledgeRetention).mockResolvedValue(plan);
    vi.mocked(applyKnowledgeRetention).mockResolvedValue(receipt);
    renderPanel();

    expect(await screen.findByText("Nothing has been purged.")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Apply sweep" })).not.toBeInTheDocument();
    expect(applyKnowledgeRetention).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("button", { name: "Plan sweep" }));
    expect(await screen.findByRole("table", { name: "Retention plan" })).toBeVisible();
    expect(screen.getByText("fx-rates")).toBeVisible();
    expect(screen.getByText("TTL expired")).toBeVisible();
    expect(screen.getByText(/1 version would purge · 2 KiB of chunk bytes/)).toBeVisible();
    expect(applyKnowledgeRetention).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("button", { name: "Apply sweep" }));
    expect(screen.getByRole("alertdialog", { name: "Confirm retention sweep" })).toBeVisible();
    expect(applyKnowledgeRetention).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("button", { name: "Keep everything" }));
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    expect(applyKnowledgeRetention).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("button", { name: "Apply sweep" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm purge of 1 version" }));
    expect(await screen.findByText("Sweep applied")).toBeVisible();
    expect(screen.getByText(/1 tombstone written/)).toBeVisible();
    expect(applyKnowledgeRetention).toHaveBeenCalledWith(testConnection, undefined);
  });

  it("offers nothing to apply when the plan is empty", async () => {
    vi.mocked(planKnowledgeRetention).mockResolvedValue({ entries: [], total_chunk_bytes: 0 });
    renderPanel();
    await userEvent.click(screen.getByRole("button", { name: "Plan sweep" }));
    expect(await screen.findByText(/Nothing would purge/)).toBeVisible();
    expect(screen.queryByRole("button", { name: "Apply sweep" })).not.toBeInTheDocument();
  });

  it("invalidates the plan when the evaluation instant changes", async () => {
    vi.mocked(planKnowledgeRetention).mockResolvedValue(plan);
    renderPanel();
    await userEvent.click(screen.getByRole("button", { name: "Plan sweep" }));
    await screen.findByRole("table", { name: "Retention plan" });
    fireEvent.change(screen.getByLabelText("Evaluate as of"), { target: { value: "2030-01-01T00:00" } });
    expect(screen.queryByRole("table", { name: "Retention plan" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply sweep" })).not.toBeInTheDocument();
  });

  it("names the failure when the plan cannot be computed", async () => {
    vi.mocked(planKnowledgeRetention).mockRejectedValue(new Error("Rusty could not be reached."));
    renderPanel();
    await userEvent.click(screen.getByRole("button", { name: "Plan sweep" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Rusty could not be reached.");
  });

  it("lists tombstones as metadata-only purge receipts", async () => {
    vi.mocked(listKnowledgeSources).mockResolvedValue({
      sources: [],
      tombstones: receipt.tombstones,
    });
    renderPanel();
    expect(await screen.findByText("FX rates")).toBeVisible();
    expect(screen.getByText(/the record is what a citation in an old journal resolves to/i)).toBeVisible();
  });
});
