import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  correctKnowledgeSource,
  getKnowledgeChunk,
  getKnowledgeSource,
} from "../../lib/api/knowledge";
import { CHUNK_ADDR_1, CHUNK_ADDR_2, chunkRecord, fullSource, HASH_A, HASH_B, HASH_C } from "./fixtures";
import { SourceDetail } from "./SourceDetail";

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

function renderDetail() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <SourceDetail sourceId="travel-policy" onBack={() => {}} />
    </QueryClientProvider>,
  );
}

const twoChunks = [
  chunkRecord(),
  chunkRecord({ chunk_id: "travel-policy#1", chunk_index: 1, byte_start: 512, byte_end: 1024, content_address: CHUNK_ADDR_2, bytes: 512, word_count: 77 }),
];

beforeEach(() => {
  vi.mocked(getKnowledgeSource).mockReset().mockResolvedValue({
    source: fullSource({ version: 2, supersedes: HASH_B }),
    versions: 2,
    chunks: twoChunks,
  });
  vi.mocked(getKnowledgeChunk).mockReset().mockResolvedValue({
    citation: {
      source_id: "travel-policy",
      source_hash: HASH_A,
      title: "Travel policy",
      chunk_id: "travel-policy#0",
      chunk_index: 0,
      content_address: CHUNK_ADDR_1,
      byte_start: 0,
      byte_end: 512,
    },
    text: "Hotels in Berlin are capped at 140 EUR per night.",
    word_count: 84,
  });
  vi.mocked(correctKnowledgeSource).mockReset();
});

describe("Source detail", () => {
  it("renders metadata, the chunk inventory, and the supersession chain", async () => {
    renderDetail();
    expect(await screen.findByRole("heading", { name: "Travel policy" })).toBeVisible();
    expect(screen.getByText("human:maya")).toBeVisible();
    expect(screen.getAllByText(HASH_A).length).toBeGreaterThan(0);
    expect(screen.getByText("current")).toBeVisible();
    expect(screen.getByText("superseded")).toBeVisible();
    expect(screen.getByText(HASH_B)).toBeVisible();
    expect(screen.getByText("travel-policy#0")).toBeVisible();
    expect(screen.getByText("travel-policy#1")).toBeVisible();
  });

  it("fetches a chunk on demand with its citation", async () => {
    renderDetail();
    const viewButtons = await screen.findAllByRole("button", { name: "View" });
    await userEvent.click(viewButtons[0]);
    expect(await screen.findByText("Hotels in Berlin are capped at 140 EUR per night.")).toBeVisible();
    expect(screen.getByText(CHUNK_ADDR_1)).toBeVisible();
    expect(screen.getAllByText("0–512").length).toBeGreaterThan(1);
    expect(getKnowledgeChunk).toHaveBeenCalledWith("travel-policy", 0, undefined);
  });

  it("pins a superseded version when fetching evidence", async () => {
    renderDetail();
    const viewButtons = await screen.findAllByRole("button", { name: "View" });
    await userEvent.click(viewButtons[0]);
    await screen.findByText("Hotels in Berlin are capped at 140 EUR per night.");
    await userEvent.selectOptions(screen.getByLabelText("Chunk version"), HASH_B);
    await waitFor(() => expect(getKnowledgeChunk).toHaveBeenCalledWith("travel-policy", 0, HASH_B));
  });

  it("names the failure when a chunk cannot be fetched", async () => {
    vi.mocked(getKnowledgeChunk).mockRejectedValue(new Error("knowledge source `travel-policy` has no chunk `9`"));
    renderDetail();
    const viewButtons = await screen.findAllByRole("button", { name: "View" });
    await userEvent.click(viewButtons[0]);
    expect(await screen.findByRole("alert")).toHaveTextContent("has no chunk");
  });

  it("runs a correction and refreshes the chain", async () => {
    vi.mocked(correctKnowledgeSource).mockResolvedValue({
      source_id: "travel-policy",
      content_hash: HASH_C,
      version: 3,
      supersedes: HASH_A,
      chunk_count: 2,
    });
    renderDetail();
    await userEvent.click(await screen.findByRole("button", { name: "Correct this source" }));
    await userEvent.type(screen.getByLabelText(/^Corrected body/), "Hotels in Berlin are capped at 160 EUR per night.");
    await userEvent.click(screen.getByRole("button", { name: "Register correction" }));

    expect(await screen.findByText("Correction registered")).toBeVisible();
    expect(screen.getByText(HASH_C)).toBeVisible();
    expect(screen.getByText("v3")).toBeVisible();
    expect(correctKnowledgeSource).toHaveBeenCalledWith(
      "travel-policy",
      "human:maya",
      "Hotels in Berlin are capped at 160 EUR per night.",
    );

    vi.mocked(getKnowledgeSource).mockResolvedValue({
      source: fullSource({ version: 3, content_hash: HASH_C, supersedes: HASH_A }),
      versions: 3,
      chunks: twoChunks,
    });
    await userEvent.click(screen.getByRole("button", { name: "View updated source" }));
    await waitFor(() => expect(screen.getAllByText(HASH_C).length).toBeGreaterThan(1));
    expect(screen.getByText(/version 3 of 3/)).toBeVisible();
  });

  it("surfaces a rejected byte-identical correction", async () => {
    vi.mocked(correctKnowledgeSource).mockRejectedValue(new Error("a correction must change the body"));
    renderDetail();
    await userEvent.click(await screen.findByRole("button", { name: "Correct this source" }));
    await userEvent.type(screen.getByLabelText(/^Corrected body/), "Identical bytes.");
    await userEvent.click(screen.getByRole("button", { name: "Register correction" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("a correction must change the body");
  });

  it("renders a purged source as metadata-only tombstone", async () => {
    vi.mocked(getKnowledgeSource).mockResolvedValue({
      tombstone: {
        source_id: "travel-policy",
        scope: { scope: "tenant", id: "acme" },
        title: "Travel policy",
        purged_hashes: [HASH_A, HASH_B],
        reason: "expired",
        purged_at: "2026-09-01T00:00:00Z",
      },
    });
    renderDetail();
    expect(await screen.findByText(/This source was purged/)).toBeVisible();
    expect(screen.getByText("TTL expired")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Correct this source" })).not.toBeInTheDocument();
  });

  it("names the failure when the source cannot be loaded", async () => {
    vi.mocked(getKnowledgeSource).mockRejectedValue(new Error("knowledge source `travel-policy` not found"));
    renderDetail();
    expect(await screen.findByText("Source could not be loaded")).toBeVisible();
    expect(screen.getByRole("alert")).toHaveTextContent("not found");
  });
});
