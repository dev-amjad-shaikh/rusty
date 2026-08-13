import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import axe from "axe-core";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Assistant } from "../../lib/contracts";
import { assistantVersionContentAddress } from "../../lib/api/assistants";
import { useConnectionStore } from "../../state/connection";
import { useWorkStore } from "../../state/work";
import { AgentWorkspace } from "./AgentWorkspace";

let v1 = "";
let v2 = "";
const createdAt = "2026-08-11T00:00:00Z";
const baseConfig = {
  studio_intent: {
    format: "rusty.agent-intent/v3",
    model: "model-v1",
    tools: [{ name: "search", effect: "read_only" }],
    memory: { access: "read_only", scopes: ["agent"] },
    approval: "external_effect",
    output: { mode: "text", schema: "" },
    budget: { max_tokens: "", max_cost_usd: "", max_latency_ms: "" },
    binding: { environment: "", surfaces: [] },
  },
  recursion_limit: 24,
};
const baseMetadata = { description: "Verify claims and cite evidence", audience: "Product team" };
const targetMetadata = { description: "Investigate claims and explain the evidence", audience: "Product team" };

beforeAll(async () => {
  v1 = await assistantVersionContentAddress({ parent_version_id: null, name: "Research analyst", graph: "research", config: baseConfig, metadata: baseMetadata });
  v2 = await assistantVersionContentAddress({ parent_version_id: v1, name: "Research analyst", graph: "research", config: baseConfig, metadata: targetMetadata });
});

function assistant(overrides: Partial<Assistant> = {}): Assistant {
  return {
    assistant_id: "analyst",
    name: "Research analyst",
    graph: "research",
    config: baseConfig,
    metadata: baseMetadata,
    created_at: createdAt,
    active_version_id: v1,
    version_count: 1,
    ...overrides,
  };
}

function exactVersion() {
  return {
    version_id: v2,
    parent_version_id: v1,
    name: "Research analyst",
    graph: "research",
    config: assistant().config,
    metadata: targetMetadata,
    created_at: "2026-08-11T01:00:00Z",
    active: false,
  };
}

function history(activeVersionId = v1, includeDraft = false, base = assistant()) {
  const target = exactVersion();
  const activeAgent = activeVersionId === v2
    ? { ...base, config: target.config, metadata: target.metadata, active_version_id: v2, version_count: 2 }
    : { ...base, version_count: includeDraft ? 2 : 1 };
  return {
    assistant_id: base.assistant_id,
    active_version_id: activeVersionId,
    assistant: activeAgent,
    versions: [
      ...(includeDraft ? [{ version_id: v2, parent_version_id: v1, graph: "research", created_at: "2026-08-11T01:00:00Z", active: activeVersionId === v2 }] : []),
      { version_id: base.active_version_id, graph: base.graph, created_at: createdAt, active: activeVersionId === base.active_version_id },
    ],
  };
}

function json(value: unknown, status = 200) {
  return Promise.resolve(new Response(JSON.stringify(value), { status }));
}

function renderWorkspace() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const root = createRootRoute({ component: Outlet });
  const library = createRoute({ getParentRoute: () => root, path: "/agents", component: () => <p>Agent library</p> });
  const workspace = createRoute({ getParentRoute: () => root, path: "/agents/$assistantId", component: AgentWorkspace });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: () => <p>Work surface</p> });
  const router = createRouter({ routeTree: root.addChildren([library, workspace, work]), history: createMemoryHistory({ initialEntries: ["/agents/analyst"] }) });
  return { router, ...render(<QueryClientProvider client={queryClient}><RouterProvider router={router} /></QueryClientProvider>) };
}

beforeEach(() => {
  useConnectionStore.setState({
    connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "tenant" },
    info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    dialogOpen: false,
  });
  useWorkStore.getState().clear();
});
afterEach(() => vi.unstubAllGlobals());

describe("Agent Workspace", () => {
  it("has no automated WCAG A/AA violations in the active definition", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json(history())));
    renderWorkspace();
    await screen.findByRole("heading", { name: "What this agent is set up to do" });
    const results = await axe.run(document.body, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"] } });
    expect(results.violations.map((violation) => violation.id)).toEqual([]);
  });

  it("shows one active definition and hands that exact agent to Work", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json(history())));
    const { router } = renderWorkspace();

    expect(await screen.findByRole("heading", { name: "Research analyst", level: 1 })).toBeVisible();
    expect(screen.getByRole("heading", { name: "What this agent is set up to do" })).toBeVisible();
    expect(screen.getByText("model-v1")).toBeVisible();
    expect(screen.queryByText(/R0\.|tenant queue|architecture/i)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Run active version" }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/work"));
    expect(useWorkStore.getState()).toMatchObject({ assistant: { assistant_id: "analyst", active_version_id: v1 }, objective: "", thread: null, receipt: null });
  });

  it("saves an immutable draft, keeps the active version, then requires review before activation", async () => {
    let hasDraft = false;
    let activeVersionId = v1;
    const fetchMock = vi.fn().mockImplementation((url: string, init?: RequestInit) => {
      const path = new URL(url).pathname;
      if (path.endsWith(`/versions/${v2}/activate`) && init?.method === "POST") {
        activeVersionId = v2;
        const target = exactVersion();
        return json({ assistant: assistant({ config: target.config, metadata: target.metadata, active_version_id: v2, version_count: 2 }), activated: true });
      }
      if (path.endsWith(`/versions/${v2}`)) return json({ assistant_id: "analyst", active_version_id: activeVersionId, version: { ...exactVersion(), active: activeVersionId === v2 } });
      if (path.endsWith("/versions") && init?.method === "POST") {
        hasDraft = true;
        const body = JSON.parse(String(init.body));
        return json({ assistant_id: "analyst", created: true, active_version_id: v1, version: { ...exactVersion(), name: body.name, graph: body.graph, config: body.config, metadata: body.metadata } }, 201);
      }
      if (path.endsWith("/versions")) return json(history(activeVersionId, hasDraft));
      throw new Error(`Unexpected request ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderWorkspace();

    await userEvent.click(await screen.findByRole("button", { name: "Create version" }));
    const responsibility = screen.getByLabelText("Responsibility");
    await userEvent.clear(responsibility);
    await userEvent.type(responsibility, "Investigate claims and explain the evidence");
    await userEvent.click(screen.getByRole("button", { name: "Save draft version" }));

    expect(await screen.findByText("Draft version saved. The active version is unchanged.")).toBeVisible();
    expect(await screen.findByRole("heading", { name: "Review before changing future runs" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Activate version" })).toBeDisabled();
    await userEvent.click(screen.getByLabelText("I reviewed every change. Future runs should use this version."));
    await userEvent.click(screen.getByRole("button", { name: "Activate version" }));

    expect(await screen.findByText("The reviewed version is now active.")).toBeVisible();
    expect(screen.getByText("Investigate claims and explain the evidence")).toBeVisible();
    const activationRequest = fetchMock.mock.calls.find(([url, init]) => String(url).endsWith(`/versions/${v2}/activate`) && init?.method === "POST");
    expect(JSON.parse(String(activationRequest?.[1]?.body))).toEqual({ expected_active_version_id: v1 });
  });

  it("confirms archive separately and preserves version history", async () => {
    let archived = false;
    const fetchMock = vi.fn().mockImplementation((url: string, init?: RequestInit) => {
      const path = new URL(url).pathname;
      if (path.endsWith("/archive") && init?.method === "POST") {
        archived = true;
        return json({ assistant: assistant({ archived_at: "2026-08-11T02:00:00Z" }), changed: true, lifecycle: "archived" });
      }
      if (path.endsWith("/versions")) return json({ ...history(), assistant: assistant(archived ? { archived_at: "2026-08-11T02:00:00Z" } : {}) });
      throw new Error(`Unexpected request ${path}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderWorkspace();

    await userEvent.click(await screen.findByRole("button", { name: "Archive" }));
    expect(screen.getByRole("heading", { name: "Archive this agent?" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Archive agent" }));
    expect(await screen.findByText("Agent archived.")).toBeVisible();
    expect(screen.getByRole("heading", { name: "1 immutable version" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Run active version" })).toBeDisabled();
  });

  it("protects an invalid edited draft before cancel, lifecycle, or navigation", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json(history())));
    const { router } = renderWorkspace();

    await userEvent.click(await screen.findByRole("button", { name: "Create version" }));
    await userEvent.clear(screen.getByLabelText("Responsibility"));
    expect(screen.getByRole("button", { name: "Save draft version" })).toBeDisabled();

    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await screen.findByRole("dialog", { name: "Discard your changes?" })).toBeVisible();
    await waitFor(() => expect(screen.getByRole("button", { name: "Keep editing" })).toHaveFocus());
    await userEvent.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(screen.getByLabelText("Responsibility")).toHaveValue("");

    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Create version" })).toHaveFocus());

    await userEvent.click(screen.getByRole("button", { name: "Create version" }));
    await userEvent.clear(screen.getByLabelText("Responsibility"));

    await userEvent.click(screen.getByRole("button", { name: "Archive" }));
    expect(await screen.findByRole("dialog", { name: "Discard your changes?" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Discard changes" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Archive this agent?" })).toHaveFocus());
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await userEvent.click(screen.getByRole("button", { name: "Create version" }));
    await userEvent.clear(screen.getByLabelText("Responsibility"));
    const navigation = router.navigate({ to: "/agents" });
    expect(await screen.findByRole("dialog", { name: "Discard your changes?" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Discard changes" }));
    await navigation;
    expect(await screen.findByText("Agent library")).toBeVisible();
  });

  it("preserves server chronology while labelling inactive history neutrally", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json(history(v1, true))));
    renderWorkspace();

    expect(await screen.findByText("Saved version")).toBeVisible();
    const historyItems = screen.getByRole("heading", { name: "2 immutable versions" }).closest("aside")!.querySelectorAll("li");
    expect(historyItems[0]).toHaveTextContent("Saved version");
    expect(historyItems[1]).toHaveTextContent("Active");
  });

  it("surfaces advanced stored-definition changes before activation acknowledgement", async () => {
    const advancedConfig = {
      ...baseConfig,
      studio_intent: { ...baseConfig.studio_intent, budget: { max_tokens: "500", max_cost_usd: "", max_latency_ms: "" }, binding: { environment: "canary", surfaces: ["prompt:system"] } },
      provider_extension: { region: "west" },
    };
    const advancedMetadata = { ...baseMetadata, release_channel: "canary" };
    const advancedVersionId = await assistantVersionContentAddress({ parent_version_id: v1, name: "Research analyst", graph: "research", config: advancedConfig, metadata: advancedMetadata });
    const advancedVersion = { version_id: advancedVersionId, parent_version_id: v1, name: "Research analyst", graph: "research", config: advancedConfig, metadata: advancedMetadata, created_at: "2026-08-11T01:00:00Z", active: false };
    vi.stubGlobal("fetch", vi.fn().mockImplementation((url: string) => new URL(url).pathname.endsWith(`/versions/${advancedVersionId}`)
      ? json({ assistant_id: "analyst", active_version_id: v1, version: advancedVersion })
      : json({ ...history(), assistant: assistant({ version_count: 2 }), versions: [{ version_id: advancedVersionId, parent_version_id: v1, graph: "research", created_at: advancedVersion.created_at, active: false }, ...history().versions] })));
    renderWorkspace();

    await userEvent.click(await screen.findByRole("button", { name: `Review version ${advancedVersionId.slice(0, 10)}…${advancedVersionId.slice(-6)}` }));
    const advanced = (await screen.findByRole("heading", { name: "Advanced settings" })).closest("article")!;
    expect(advanced).toHaveTextContent(/max_tokens.*500/);
    expect(advanced).toHaveTextContent(/release_channel.*canary/);
    expect(screen.queryByText("No visible capability changes")).not.toBeInTheDocument();
  });

  it("does not let a late version receipt cross into a new connection", async () => {
    let finishPost!: (response: Response) => void;
    const pendingPost = new Promise<Response>((resolve) => { finishPost = resolve; });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => init?.method === "POST" ? pendingPost : json(history())));
    renderWorkspace();

    await userEvent.click(await screen.findByRole("button", { name: "Create version" }));
    const responsibility = screen.getByLabelText("Responsibility");
    await userEvent.clear(responsibility);
    await userEvent.type(responsibility, "A connection-owned revision");
    await userEvent.click(screen.getByRole("button", { name: "Save draft version" }));
    expect(screen.getByRole("button", { name: "Saving version…" })).toBeDisabled();

    useConnectionStore.setState((state) => ({ connection: state.connection && { ...state.connection, epoch: 2, tenantFingerprint: "other-tenant" } }));
    await screen.findByRole("heading", { name: "What this agent is set up to do" });
    finishPost(new Response(JSON.stringify({ assistant_id: "analyst", created: true, active_version_id: v1, version: exactVersion() }), { status: 201 }));

    await waitFor(() => expect(screen.queryByText("Draft version saved. The active version is unchanged.")).not.toBeInTheDocument());
    expect(screen.getByText("Verify claims and cite evidence")).toBeVisible();
  });

  it("does not let a late version receipt take over another agent", async () => {
    let finishPost!: (response: Response) => void;
    const pendingPost = new Promise<Response>((resolve) => { finishPost = resolve; });
    const reviewerMetadata = { description: "Review policy exceptions", audience: "Operations" };
    const reviewerV1 = await assistantVersionContentAddress({ parent_version_id: null, name: "Policy reviewer", graph: "research", config: baseConfig, metadata: reviewerMetadata });
    const reviewer = assistant({ assistant_id: "reviewer", name: "Policy reviewer", metadata: reviewerMetadata, active_version_id: reviewerV1 });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((url: string, init?: RequestInit) => {
      if (init?.method === "POST") return pendingPost;
      return new URL(url).pathname.includes("/assistants/reviewer/") ? json(history(reviewerV1, false, reviewer)) : json(history());
    }));
    const { router } = renderWorkspace();

    await userEvent.click(await screen.findByRole("button", { name: "Create version" }));
    const responsibility = screen.getByLabelText("Responsibility");
    await userEvent.clear(responsibility);
    await userEvent.type(responsibility, "A route-owned revision");
    await userEvent.click(screen.getByRole("button", { name: "Save draft version" }));
    const navigation = router.navigate({ to: "/agents/$assistantId", params: { assistantId: "reviewer" } });
    await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
    await navigation;
    expect(await screen.findByRole("heading", { name: "Policy reviewer", level: 1 })).toBeVisible();

    finishPost(new Response(JSON.stringify({ assistant_id: "analyst", created: true, active_version_id: v1, version: exactVersion() }), { status: 201 }));
    await waitFor(() => expect(screen.queryByText("Draft version saved. The active version is unchanged.")).not.toBeInTheDocument());
    expect(screen.getByText("Review policy exceptions")).toBeVisible();
  });
});
