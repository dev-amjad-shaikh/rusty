import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { listKnowledgeSources } from "../../lib/api/knowledge";
import { emptyLibrary, HASH_B, listedSource } from "./fixtures";
import { KnowledgePage } from "./KnowledgePage";

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

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><KnowledgePage /></QueryClientProvider>);
}

beforeEach(() => {
  vi.mocked(listKnowledgeSources).mockReset().mockResolvedValue(emptyLibrary());
});

describe("Knowledge library", () => {
  it("shows an empty state with a register CTA when the library is empty", async () => {
    renderPage();
    expect(await screen.findByText("No sources yet")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Register first source" }));
    expect(await screen.findByRole("form", { name: "Register source" })).toBeInTheDocument();
  });

  it("renders sources in deterministic order with provenance and retention state", async () => {
    vi.mocked(listKnowledgeSources).mockResolvedValue({
      sources: [
        listedSource({ source_id: "vendor-directory", title: "Vendor directory", kind: "csv", author: "agent:ingest-2", version: 4, supersedes: HASH_B }),
        listedSource({ source_id: "fx-rates", title: "FX rates", kind: "json", retention: { policy: "ttl", expires_at: "2020-01-01T00:00:00Z" }, chunk_count: 12 }),
      ],
      tombstones: [{
        source_id: "old-handbook",
        scope: { scope: "tenant", id: "acme" },
        title: "Old handbook",
        purged_hashes: [HASH_B],
        reason: "expired",
        purged_at: "2026-07-01T00:00:00Z",
      }],
    });
    renderPage();
    const rows = await screen.findAllByRole("row");
    const sourceRows = rows.filter((row) => /fx-rates|vendor-directory/.test(row.textContent ?? ""));
    expect(sourceRows).toHaveLength(2);
    expect(sourceRows[0]).toHaveTextContent("fx-rates");
    expect(sourceRows[1]).toHaveTextContent("vendor-directory");
    expect(sourceRows[0]).toHaveTextContent(/Expired/);
    expect(sourceRows[1]).toHaveTextContent("Pinned");
    expect(sourceRows[1]).toHaveTextContent("agent:ingest-2");
    expect(sourceRows[1]).toHaveTextContent("v4");
    expect(screen.getByRole("heading", { name: "Purged sources" })).toBeVisible();
    expect(screen.getByText("Old handbook")).toBeVisible();
  });

  it("filters by title, id, author, and kind", async () => {
    vi.mocked(listKnowledgeSources).mockResolvedValue({
      sources: [
        listedSource({ source_id: "travel-policy", title: "Travel policy" }),
        listedSource({ source_id: "vendor-directory", title: "Vendor directory", kind: "csv" }),
      ],
      tombstones: [],
    });
    renderPage();
    await screen.findByText("Travel policy");
    await userEvent.type(screen.getByLabelText("Filter"), "vendor");
    expect(screen.queryByText("Travel policy")).not.toBeInTheDocument();
    expect(screen.getByText("Vendor directory")).toBeVisible();
    await userEvent.clear(screen.getByLabelText("Filter"));
    await userEvent.selectOptions(screen.getByLabelText("Kind"), "csv");
    expect(screen.queryByText("Travel policy")).not.toBeInTheDocument();
    await userEvent.selectOptions(screen.getByLabelText("Kind"), "text");
    expect(screen.getByText("Nothing matches")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Clear filter" }));
    expect(await screen.findByText("Travel policy")).toBeVisible();
  });

  it("names the failure and offers retry when the library cannot be loaded", async () => {
    vi.mocked(listKnowledgeSources).mockRejectedValue(new Error("Rusty could not be reached."));
    renderPage();
    expect(await screen.findByText("Sources could not be loaded")).toBeVisible();
    expect(screen.getByRole("alert")).toHaveTextContent("Rusty could not be reached.");
    vi.mocked(listKnowledgeSources).mockResolvedValue(emptyLibrary());
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText("No sources yet")).toBeVisible();
  });

  it("switches between sources, the query console, and retention", async () => {
    renderPage();
    await screen.findByText("No sources yet");
    await userEvent.click(screen.getByRole("button", { name: "Test retrieval" }));
    expect(await screen.findByRole("heading", { name: "Query console" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Retention" }));
    expect(await screen.findByRole("heading", { name: "Retention sweep" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Sources" }));
    expect(await screen.findByText("No sources yet")).toBeVisible();
  });
});
