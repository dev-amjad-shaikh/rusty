import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  getKnowledgeSource,
  listKnowledgeSources,
  registerKnowledgeSource,
} from "../../lib/api/knowledge";
import { useConnectionStore } from "../../state/connection";
import { chunkRecord, emptyLibrary, fullSource, HASH_A, testConnection } from "./fixtures";
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

async function openRegisterForm() {
  renderPage();
  await screen.findByText("No sources yet");
  await userEvent.click(screen.getByRole("button", { name: "Register first source" }));
  return screen.findByRole("form", { name: "Register source" });
}

beforeEach(() => {
  useConnectionStore.setState({ connection: testConnection, info: null, workspaceStatus: "ready", dialogOpen: false });
  vi.mocked(listKnowledgeSources).mockReset().mockResolvedValue(emptyLibrary());
  vi.mocked(registerKnowledgeSource).mockReset();
  vi.mocked(getKnowledgeSource).mockReset();
});

describe("Register source", () => {
  it("registers a source, shows the receipt, and lands on the source detail", async () => {
    vi.mocked(registerKnowledgeSource).mockResolvedValue({
      source_id: "travel-policy",
      content_hash: HASH_A,
      version: 1,
      chunk_count: 3,
      created: true,
    });
    vi.mocked(getKnowledgeSource).mockResolvedValue({
      source: fullSource(),
      versions: 1,
      chunks: [chunkRecord()],
    });

    await openRegisterForm();
    await userEvent.type(screen.getByLabelText(/^Source id/), "travel-policy");
    await userEvent.click(screen.getByRole("button", { name: /markdown/ }));
    await userEvent.type(screen.getByLabelText(/^Title/), "Travel policy");
    await userEvent.clear(screen.getByLabelText(/^Author/));
    await userEvent.type(screen.getByLabelText(/^Author/), "human:maya");
    fireEvent.change(screen.getByLabelText(/^Confidence/), { target: { value: "0.9" } });
    await userEvent.type(screen.getByLabelText(/^Body/), "Hotels in Berlin are capped at 140 EUR per night.");
    await userEvent.click(screen.getByRole("button", { name: "Register source" }));

    expect(await screen.findByText("Source registered")).toBeVisible();
    expect(screen.getByText(HASH_A)).toBeVisible();
    expect(screen.getByText("3", { selector: "dd" })).toBeVisible();
    expect(registerKnowledgeSource).toHaveBeenCalledWith(testConnection, {
      source_id: "travel-policy",
      kind: "markdown",
      title: "Travel policy",
      author: "human:maya",
      body: "Hotels in Berlin are capped at 140 EUR per night.",
      confidence: 0.9,
      retention: { policy: "pinned" },
    });

    await userEvent.click(screen.getByRole("button", { name: "Open source" }));
    expect((await screen.findAllByText("Travel policy")).length).toBeGreaterThan(0);
    expect(screen.getByText("travel-policy#0")).toBeVisible();
  });

  it("reports an idempotent re-registration instead of claiming a write", async () => {
    vi.mocked(registerKnowledgeSource).mockResolvedValue({
      source_id: "travel-policy",
      content_hash: HASH_A,
      version: 1,
      chunk_count: 3,
      created: false,
    });
    await openRegisterForm();
    await userEvent.type(screen.getByLabelText(/^Source id/), "travel-policy");
    await userEvent.type(screen.getByLabelText(/^Title/), "Travel policy");
    await userEvent.type(screen.getByLabelText(/^Body/), "Same bytes as before.");
    await userEvent.click(screen.getByRole("button", { name: "Register source" }));
    expect(await screen.findByText("Already registered")).toBeVisible();
  });

  it("blocks submit until the id, title, author, and body are valid", async () => {
    await openRegisterForm();
    const submit = screen.getByRole("button", { name: "Register source" });
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/^Source id/), "not a valid id");
    await userEvent.type(screen.getByLabelText(/^Title/), "Travel policy");
    await userEvent.type(screen.getByLabelText(/^Body/), "Some body text.");
    expect(submit).toBeDisabled();
    await userEvent.clear(screen.getByLabelText(/^Source id/));
    await userEvent.type(screen.getByLabelText(/^Source id/), "travel-policy");
    expect(submit).toBeEnabled();
  });

  it("warns and blocks when the body passes the 1 MiB cap", async () => {
    await openRegisterForm();
    await userEvent.type(screen.getByLabelText(/^Source id/), "big-doc");
    await userEvent.type(screen.getByLabelText(/^Title/), "Big document");
    fireEvent.change(screen.getByLabelText(/^Body/), { target: { value: "x".repeat(1024 * 1024 + 1) } });
    expect(screen.getByText(/of 1.00 MiB/)).toHaveAttribute("data-over", "true");
    expect(screen.getByRole("button", { name: "Register source" })).toBeDisabled();
    expect(registerKnowledgeSource).not.toHaveBeenCalled();
  });

  it("surfaces the server rejection without losing the draft", async () => {
    vi.mocked(registerKnowledgeSource).mockRejectedValue(new Error("knowledge source `travel-policy` confidence must be in (0, 1]"));
    await openRegisterForm();
    await userEvent.type(screen.getByLabelText(/^Source id/), "travel-policy");
    await userEvent.type(screen.getByLabelText(/^Title/), "Travel policy");
    await userEvent.type(screen.getByLabelText(/^Body/), "Some body text.");
    await userEvent.click(screen.getByRole("button", { name: "Register source" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("confidence must be in (0, 1]");
    expect(screen.getByLabelText(/^Title/)).toHaveValue("Travel policy");
  });

  it("sends a TTL retention policy with the expiry instant", async () => {
    vi.mocked(registerKnowledgeSource).mockResolvedValue({
      source_id: "fx-rates",
      content_hash: HASH_A,
      version: 1,
      chunk_count: 1,
      created: true,
    });
    vi.mocked(getKnowledgeSource).mockResolvedValue({ source: fullSource(), versions: 1, chunks: [] });
    await openRegisterForm();
    await userEvent.type(screen.getByLabelText(/^Source id/), "fx-rates");
    await userEvent.type(screen.getByLabelText(/^Title/), "FX rates");
    await userEvent.type(screen.getByLabelText(/^Body/), "EURUSD 1.08");
    await userEvent.selectOptions(screen.getByLabelText("Retention"), "ttl");
    fireEvent.change(screen.getByLabelText(/^Expires at/), { target: { value: "2030-01-01T00:00" } });
    await userEvent.click(screen.getByRole("button", { name: "Register source" }));
    await screen.findByText("Source registered");
    expect(registerKnowledgeSource).toHaveBeenCalledWith(testConnection, expect.objectContaining({
      retention: { policy: "ttl", expires_at: new Date("2030-01-01T00:00").toISOString() },
    }));
  });
});
