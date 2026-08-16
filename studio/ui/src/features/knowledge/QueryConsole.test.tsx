import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { listKnowledgeSources, queryKnowledge } from "../../lib/api/knowledge";
import { useConnectionStore } from "../../state/connection";
import { CHUNK_ADDR_1, CHUNK_ADDR_2, citedChunk, emptyLibrary, testConnection } from "./fixtures";
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

async function openConsole() {
  renderPage();
  await screen.findByText("No sources yet");
  await userEvent.click(screen.getByRole("button", { name: "Test retrieval" }));
  return screen.findByRole("heading", { name: "Query console" });
}

beforeEach(() => {
  useConnectionStore.setState({ connection: testConnection, info: null, workspaceStatus: "ready", dialogOpen: false });
  vi.mocked(queryKnowledge).mockReset();
  vi.mocked(listKnowledgeSources).mockReset().mockResolvedValue(emptyLibrary());
});

describe("Query console", () => {
  it("runs a query with limits and renders cited chunks in the returned order", async () => {
    vi.mocked(queryKnowledge).mockResolvedValue({
      query: "hotel cap",
      results: [
        citedChunk({ text: "Hotels in Berlin are capped at 140 EUR per night.", score: 0.8123 }),
        citedChunk({
          citation: {
            source_id: "per-diem",
            source_hash: "f".repeat(64),
            title: "Per diem rules",
            chunk_id: "per-diem#2",
            chunk_index: 2,
            content_address: CHUNK_ADDR_2,
            byte_start: 640,
            byte_end: 1024,
          },
          text: "Per diem covers meals only.",
          score: 0.4417,
          word_count: 21,
        }),
      ],
    });
    await openConsole();
    await userEvent.type(screen.getByLabelText("Query"), "hotel cap");
    await userEvent.clear(screen.getByLabelText("Max results"));
    await userEvent.type(screen.getByLabelText("Max results"), "6");
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));

    expect(await screen.findByText("2 cited chunks")).toBeVisible();
    const cards = screen.getAllByRole("listitem");
    expect(cards[0]).toHaveTextContent("Hotels in Berlin are capped at 140 EUR per night.");
    expect(cards[0]).toHaveTextContent("score 0.812");
    expect(cards[0]).toHaveTextContent(CHUNK_ADDR_1);
    expect(cards[0]).toHaveTextContent("0–512");
    expect(cards[1]).toHaveTextContent("Per diem covers meals only.");
    expect(cards[1]).toHaveTextContent("per-diem#2");
    expect(queryKnowledge).toHaveBeenCalledWith(testConnection, "hotel cap", { max_results: 6, max_bytes: 65536 });
  });

  it("says so when no live source matches", async () => {
    vi.mocked(queryKnowledge).mockResolvedValue({ query: "nothing", results: [] });
    await openConsole();
    await userEvent.type(screen.getByLabelText("Query"), "nothing");
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    expect(await screen.findByText("No results")).toBeVisible();
    expect(screen.getByText(/No live source matched/)).toBeVisible();
  });

  it("surfaces a rejected query", async () => {
    vi.mocked(queryKnowledge).mockRejectedValue(new Error("query max_results 0 is outside 1..=100"));
    await openConsole();
    await userEvent.type(screen.getByLabelText("Query"), "hotel cap");
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("query max_results 0 is outside 1..=100");
  });

  it("blocks the run button while limits are outside their ceilings", async () => {
    await openConsole();
    await userEvent.type(screen.getByLabelText("Query"), "hotel cap");
    const runButton = screen.getByRole("button", { name: "Run query" });
    expect(runButton).toBeEnabled();
    await userEvent.clear(screen.getByLabelText("Max results"));
    await userEvent.type(screen.getByLabelText("Max results"), "0");
    await waitFor(() => expect(runButton).toBeDisabled());
    expect(queryKnowledge).not.toHaveBeenCalled();
  });
});
