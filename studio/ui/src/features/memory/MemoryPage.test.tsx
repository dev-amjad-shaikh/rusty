import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { MemoryPage } from "./MemoryPage";

function setJson(label: RegExp, value: string) {
  fireEvent.change(screen.getByLabelText(label), { target: { value } });
}

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><MemoryPage /></QueryClientProvider>);
}

function json(value: unknown, status = 200) { return Promise.resolve(new Response(JSON.stringify(value), { status })); }

const idA = "a".repeat(64);
const idB = "b".repeat(64);
const idC = "c".repeat(64);

function memoryRecord(over: Record<string, unknown> = {}) {
  return {
    memory_id: idA,
    kind: "fact",
    scope: { scope: "user", id: "user-7" },
    provenance: { author: { type: "human", human_id: "amjad" }, written_at: "2026-08-09T06:00:00Z" },
    confidence: 1,
    validity: { valid_from: "2026-08-09T06:00:00Z" },
    created_at: "2026-08-09T06:00:00Z",
    content: { kind: "inline", value: { timezone: "Asia/Dubai" } },
    ...over,
  };
}

const older = memoryRecord({ memory_id: idA, key: "timezone", content: { kind: "inline", value: { timezone: "Asia/Dubai" } } });
const newer = memoryRecord({
  memory_id: idB,
  key: "timezone",
  supersedes: idA,
  created_at: "2026-08-10T06:00:00Z",
  provenance: { author: { type: "human", human_id: "amjad" }, evidence: { correction_id: "corr-1", event_ids: [], source_memory_ids: [] }, written_at: "2026-08-10T06:00:00Z" },
  content: { kind: "inline", value: { timezone: "Europe/Zurich" } },
});

beforeEach(() => {
  useConnectionStore.setState({ connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "fp" }, info: null, dialogOpen: false });
});
afterEach(() => vi.unstubAllGlobals());

function stubFetch(handler: (url: URL, init?: RequestInit) => Promise<Response> | Response | undefined) {
  const spy = vi.fn().mockImplementation((input: string | URL | Request, init?: RequestInit) => {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input : input.url);
    const result = handler(url, init);
    if (!result) return Promise.resolve(new Response("not stubbed", { status: 500 }));
    return Promise.resolve(result);
  });
  vi.stubGlobal("fetch", spy);
  return spy;
}

describe("Memory ledger", () => {
  it("requires a workspace before reading memory", () => {
    useConnectionStore.setState({ connection: null });
    renderPage();
    expect(screen.getByRole("heading", { name: "Open a workspace to inspect memory" })).toBeVisible();
    expect(screen.getAllByRole("button", { name: "Choose workspace" })).not.toHaveLength(0);
  });

  it("runs a scoped query and renders records with lifecycle badges", async () => {
    const fetchSpy = stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory/query") return json({ records: [newer, older] });
      if (url.pathname === `/memory/${idA}`) return json(older);
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Inspect timezone, record bbbb/ })).toBeVisible());
    const queryCall = fetchSpy.mock.calls.find(([input]) => new URL(String(input)).pathname === "/memory/query");
    expect(JSON.parse(String(queryCall![1]?.body))).toEqual({});
    expect(screen.getByText("Europe/Zurich", { exact: false })).toBeVisible();
    expect(screen.getAllByText("Superseded")).not.toHaveLength(0);
    expect(screen.getAllByText("Active")).not.toHaveLength(0);
  });

  it("opens the citation detail with the provenance spine and supersession chain", async () => {
    stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory/query") return json({ records: [newer] });
      if (url.pathname === `/memory/${idA}`) return json(older);
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    await userEvent.click(await screen.findByRole("button", { name: /Inspect timezone/ }));
    expect(await screen.findByRole("heading", { name: "timezone" })).toBeVisible();
    expect(screen.getByLabelText("Provenance spine")).toBeVisible();
    expect(screen.getAllByText("human:amjad", { exact: false })).not.toHaveLength(0);
    expect(screen.getByText(/correction corr-1/)).toBeVisible();
    expect(screen.getByText("Supersession chain")).toBeVisible();
    await waitFor(() => expect(screen.getByRole("button", { name: "aaaaaaaaaa…aaaaa" })).toBeVisible());
  });

  it("shows an honest empty state and a query error state", async () => {
    let fail = false;
    stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory/query") return fail ? json({ error: "store unavailable" }, 500) : json({ records: [] });
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    expect(await screen.findByRole("heading", { name: "No governed memory matched" })).toBeVisible();
    fail = true;
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent(/HTTP 500|unavailable/i));
  });

  it("filters loaded results client-side without re-querying", async () => {
    const fetchSpy = stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory/query") return json({ records: [newer, memoryRecord({ memory_id: idC, key: "language", content: { kind: "inline", value: { language: "German" } } })] });
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    await screen.findByRole("button", { name: /Inspect language/ });
    await userEvent.type(screen.getByLabelText("Filter these results"), "zurich");
    await waitFor(() => expect(screen.queryByRole("button", { name: /Inspect language/ })).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: /Inspect timezone/ })).toBeVisible();
    expect(fetchSpy.mock.calls.filter(([input]) => new URL(String(input)).pathname === "/memory/query")).toHaveLength(1);
  });
});

describe("Create memory", () => {
  it("validates client-side before any write reaches the server", async () => {
    const fetchSpy = stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "New memory" }));
    await userEvent.type(screen.getByLabelText("Scope identity"), "user-7");
    await userEvent.type(screen.getByLabelText("Your identity"), "amjad");
    setJson(/^Content/, "not json{");
    await userEvent.click(screen.getByRole("button", { name: "Write memory" }));
    expect(await screen.findByText(/must be valid JSON/)).toBeVisible();
    expect(fetchSpy.mock.calls.filter(([input]) => new URL(String(input)).pathname === "/memory")).toHaveLength(0);
  });

  it("writes a memory and shows the content-address receipt", async () => {
    const written = memoryRecord({ memory_id: idC, key: "timezone" });
    const fetchSpy = stubFetch((url, init) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory" && init?.method === "POST") return json({ memory_id: idC, created: true, record: written }, 201);
      if (url.pathname === `/memory/${idC}`) return json(written);
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "New memory" }));
    await userEvent.type(screen.getByLabelText("Scope identity"), "user-7");
    await userEvent.type(screen.getByLabelText(/^Lookup key/), "timezone");
    await userEvent.type(screen.getByLabelText("Your identity"), "amjad");
    setJson(/^Content/, '{"timezone":"Asia/Dubai"}');
    await userEvent.click(screen.getByRole("button", { name: "Write memory" }));
    expect(await screen.findByRole("heading", { name: "Memory written" })).toBeVisible();
    const writeCall = fetchSpy.mock.calls.find(([input]) => new URL(String(input)).pathname === "/memory");
    const body = JSON.parse(String(writeCall![1]?.body));
    expect(body).toMatchObject({ kind: "fact", scope: { scope: "user", id: "user-7" }, key: "timezone", author: { type: "human", human_id: "amjad" } });
    await userEvent.click(screen.getByRole("button", { name: "Inspect in the ledger" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "timezone" })).toBeVisible());
  });
});

describe("Corrections", () => {
  it("loads a target, submits a correction, and renders the old → new chain", async () => {
    const correctedRecord = memoryRecord({
      memory_id: idC,
      key: "timezone",
      supersedes: idB,
      candidacy: "pending",
      provenance: { author: { type: "human", human_id: "maya" }, evidence: { correction_id: "corr-test", event_ids: [], source_memory_ids: [] }, written_at: "2026-08-11T06:00:00Z" },
      created_at: "2026-08-11T06:00:00Z",
      content: { kind: "inline", value: { timezone: "Europe/Berlin" } },
    });
    const fetchSpy = stubFetch((url, init) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === `/memory/${idB}`) return json(newer);
      if (url.pathname === "/memory/corrections" && init?.method === "POST") {
        const payload = JSON.parse(String(init.body));
        return json({
          correction_id: payload.correction_id,
          attribution: `human:maya via correction:${payload.correction_id}`,
          candidate: true,
          memory_id: idC,
          created: true,
          record: {
            ...correctedRecord,
            provenance: { author: { type: "human", human_id: "maya" }, evidence: { correction_id: payload.correction_id, event_ids: [], source_memory_ids: [] }, written_at: "2026-08-11T06:00:00Z" },
          },
          superseded: idB,
          example_id: null,
        }, 201);
      }
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Corrections" }));
    await userEvent.type(screen.getByLabelText("Memory to correct"), idB);
    await userEvent.click(screen.getByRole("button", { name: "Load record" }));
    await waitFor(() => expect((screen.getByLabelText(/^Corrected content/) as HTMLTextAreaElement).value).toContain("Europe/Zurich"));
    expect(screen.getByRole("button", { name: "Submit correction" })).toBeDisabled();
    setJson(/^Corrected content/, '{"timezone":"Europe/Berlin"}');
    await userEvent.type(screen.getByLabelText("Your identity"), "maya");
    await userEvent.click(screen.getByRole("button", { name: "Submit correction" }));
    expect(await screen.findByRole("heading", { name: "Correction held as a candidate" })).toBeVisible();
    expect(screen.getByText("Old → new")).toBeVisible();
    const correctionCall = fetchSpy.mock.calls.find(([input]) => new URL(String(input)).pathname === "/memory/corrections");
    const body = JSON.parse(String(correctionCall![1]?.body));
    expect(body.target).toEqual({ type: "memory", memory_id: idB });
    expect(body.scope).toEqual({ scope: "user", id: "user-7" });
    expect(body.author).toBe("maya");
  });

  it("blocks a correction that does not change what the record asserts", async () => {
    const fetchSpy = stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === `/memory/${idB}`) return json(newer);
      return undefined;
    });
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Corrections" }));
    await userEvent.type(screen.getByLabelText("Memory to correct"), idB);
    await userEvent.click(screen.getByRole("button", { name: "Load record" }));
    await waitFor(() => expect((screen.getByLabelText(/^Corrected content/) as HTMLTextAreaElement).value).toContain("Europe/Zurich"));
    await userEvent.type(screen.getByLabelText("Your identity"), "maya");
    expect(screen.getByRole("button", { name: "Submit correction" })).toBeDisabled();
    expect(fetchSpy.mock.calls.filter(([input]) => new URL(String(input)).pathname === "/memory/corrections")).toHaveLength(0);
  });
});

describe("Conflict inbox", () => {
  it("lists conflicts with both peers and never picks a winner", async () => {
    const conflict = {
      scope: { scope: "user", id: "user-7" },
      key: "timezone",
      memory_ids: [idA, idB],
      overlap: { valid_from: "2026-08-09T06:00:00Z" },
    };
    stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [conflict] });
      if (url.pathname === "/memory/query") return json({ records: [older, newer] });
      return undefined;
    });
    renderPage();
    const tab = await screen.findByRole("button", { name: /Conflict inbox/ });
    await waitFor(() => expect(tab).toHaveTextContent("1"));
    await userEvent.click(tab);
    expect(await screen.findByRole("heading", { name: "1 conflict needs a human decision" })).toBeVisible();
    expect(screen.getByText(/never silently picks a winner/)).toBeVisible();
    expect(screen.getByRole("button", { name: `Inspect conflicting record ${"a".repeat(10)}…aaaaa` })).toBeVisible();
    expect(screen.getByRole("button", { name: `Inspect conflicting record ${"b".repeat(10)}…bbbbb` })).toBeVisible();
  });

  it("treats an unreachable conflict check as unknown, never as an all-clear", async () => {
    stubFetch((url) => {
      if (url.pathname === "/memory/conflicts") return json({ error: "offline" }, 503);
      return undefined;
    });
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: /Conflict inbox/ }));
    expect(await screen.findByRole("heading", { name: "Conflicts could not be checked" })).toBeVisible();
    expect(screen.getByText(/never presented as an all-clear/)).toBeVisible();
  });
});

describe("Forgetting", () => {
  function stubForgetFlow(receipt?: unknown) {
    return stubFetch((url, init) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory/query") return json({ records: [older] });
      if (url.pathname === "/memory/forget" && init?.method === "POST") {
        return json(receipt ?? { forgotten: [idA], invalidated: [], tombstone: { memory_id: idA, scope: { scope: "user", id: "user-7" }, reason: "retracted" } });
      }
      return undefined;
    });
  }

  async function openForgetPanel() {
    renderPage();
    await userEvent.click(screen.getByRole("button", { name: "Run query" }));
    await userEvent.click(await screen.findByRole("button", { name: /Inspect timezone/ }));
    await screen.findByRole("heading", { name: "Forget this memory" });
    await userEvent.click(screen.getByRole("button", { name: "I understand — continue" }));
  }

  it("keeps the irreversible action locked until the exact memory id is typed", async () => {
    const fetchSpy = stubForgetFlow();
    await openForgetPanel();
    const confirm = screen.getByLabelText(/Type the full memory id/);
    const button = screen.getByRole("button", { name: "Forget permanently" });
    expect(button).toBeDisabled();
    await userEvent.type(confirm, idA.slice(0, 63));
    expect(button).toBeDisabled();
    await userEvent.type(confirm, "f");
    expect(button).toBeDisabled();
    expect(fetchSpy.mock.calls.filter(([input]) => new URL(String(input)).pathname === "/memory/forget")).toHaveLength(0);
    await userEvent.clear(confirm);
    await userEvent.type(confirm, idA);
    expect(button).toBeEnabled();
    await userEvent.click(button);
    await waitFor(() => expect(fetchSpy.mock.calls.filter(([input]) => new URL(String(input)).pathname === "/memory/forget")).toHaveLength(1));
    const call = fetchSpy.mock.calls.find(([input]) => new URL(String(input)).pathname === "/memory/forget");
    expect(JSON.parse(String(call![1]?.body))).toEqual({ memory_id: idA, reason: "retracted" });
    expect(await screen.findByRole("heading", { name: /Forgotten — this cannot be undone/ })).toBeVisible();
  });

  it("surfaces a forget failure without pretending the record is gone", async () => {
    stubFetch((url, init) => {
      if (url.pathname === "/memory/conflicts") return json({ conflicts: [] });
      if (url.pathname === "/memory/query") return json({ records: [older] });
      if (url.pathname === "/memory/forget" && init?.method === "POST") return json({ error: `memory \`${idA}\` not found` }, 404);
      return undefined;
    });
    await openForgetPanel();
    await userEvent.type(screen.getByLabelText(/Type the full memory id/), idA);
    await userEvent.click(screen.getByRole("button", { name: "Forget permanently" }));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent(/not found/));
    expect(screen.queryByRole("heading", { name: /Forgotten/ })).not.toBeInTheDocument();
  });
});
