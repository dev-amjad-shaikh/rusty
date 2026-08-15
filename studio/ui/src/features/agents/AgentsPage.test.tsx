import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import axe from "axe-core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { useAgentMutationStore } from "../../state/agents";
import { useWorkStore } from "../../state/work";
import { AgentsPage } from "./AgentsPage";
import { AgentBuilderPage, clearAgentBuilderMemory } from "./AgentBuilderPage";
import { agentVersionFields, editableAgent, emptyAgentDraft, modelRequirement, outputSchemaRequirement, toolContracts } from "./AgentIntentEditor";

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const root = createRootRoute({ component: Outlet });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: AgentsPage });
  const builder = createRoute({ getParentRoute: () => root, path: "/agents/new", component: AgentBuilderPage });
  const workspace = createRoute({ getParentRoute: () => root, path: "/agents/$assistantId", component: () => <h1>Agent workspace</h1> });
  const prompts = createRoute({ getParentRoute: () => root, path: "/agents/prompts", component: () => <p>Prompts</p> });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: () => <h1>Run workspace</h1> });
  const home = createRoute({ getParentRoute: () => root, path: "/", component: () => <h1>Work board</h1> });
  const router = createRouter({ routeTree: root.addChildren([agents, builder, workspace, prompts, work, home]), history: createMemoryHistory({ initialEntries: ["/agents"] }) });
  return { router, ...render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>) };
}

function json(value: unknown, status = 200) { return Promise.resolve(new Response(JSON.stringify(value), { status })); }

async function completeBuilder() {
  await userEvent.type(screen.getByLabelText("Name"), "Research analyst");
  await userEvent.selectOptions(screen.getByLabelText("Behavior"), "research");
  await userEvent.type(screen.getByLabelText("Responsibility"), "Investigate claims");
  await userEvent.click(screen.getByRole("tab", { name: /Goals/ }));
  await userEvent.click(screen.getByRole("tab", { name: /Model/ }));
  await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
  for (const capability of ["Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("tab", { name: new RegExp(capability) }));
}

async function reviewAndCreate() {
  await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
  expect(await screen.findByRole("heading", { name: "Review version 1" })).toBeVisible();
  await userEvent.click(screen.getByRole("button", { name: "Create version 1" }));
}

beforeEach(() => { clearAgentBuilderMemory(); useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", discoveryAttempt: 0, discoveryError: "", suggestedOrigin: "", dialogOpen: false }); useAgentMutationStore.setState({ uncertainByConnection: {} }); useWorkStore.getState().clear(); });
afterEach(() => vi.unstubAllGlobals());

describe("Agents", () => {
  it("opens with the v4 portfolio and hands off to the dedicated builder", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "Agent portfolio" })).toBeVisible();
    expect(screen.queryByText("Table")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("link", { name: "New agent" }));
    expect(await screen.findByRole("heading", { name: "New agent" })).toBeVisible();
    expect(screen.getByRole("tablist", { name: "Agent capabilities" })).toBeVisible();
  });

  it("uses structured v4 controls for goals, memory, and output", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.click(screen.getByRole("tab", { name: /Goals/ }));
    await userEvent.click(screen.getByRole("button", { name: "Add Task success rate" }));
    const target = screen.getByRole("spinbutton", { name: "Task success rate target" });
    expect(target).toBeEnabled();
    await userEvent.clear(target);
    const clearedTarget = screen.getByRole("spinbutton", { name: "Task success rate target" });
    expect(clearedTarget).toBeEnabled();
    expect(screen.queryByText("Task success rate ≥  %")).not.toBeInTheDocument();
    await userEvent.type(clearedTarget, "95");
    await userEvent.click(screen.getByRole("tab", { name: /Knowledge/ }));
    await userEvent.click(screen.getByRole("button", { name: /Read memory/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /Run/ }));
    expect(screen.getByRole("checkbox", { name: /Run/ })).toBeChecked();
    await userEvent.click(screen.getByRole("tab", { name: /Output/ }));
    await userEvent.click(screen.getByRole("button", { name: /JSON object/ }));
    expect(screen.getByRole("button", { name: /JSON object/ })).toHaveClass(/selectedChoice/);
    expect(screen.getByRole("button", { name: /JSON object/ })).toHaveAttribute("aria-pressed", "true");
  });

  it("preserves prefix-colliding custom goals and canonicalizes memory scope order", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.click(screen.getByRole("tab", { name: /Goals/ }));
    const custom = screen.getByPlaceholderText("Describe another measurable outcome");
    await userEvent.type(custom, "Median latency should remain predictable{Enter}");
    await userEvent.click(screen.getByRole("button", { name: "Add Median latency" }));
    expect(screen.getByText("Median latency should remain predictable")).toBeVisible();
    expect(screen.getByRole("spinbutton", { name: "Median latency target" })).toBeEnabled();

    const base = { ...emptyAgentDraft(), name: "Analyst", responsibility: "Review evidence", graph: "research", model: "model-v1", memoryAccess: "read_only" as const };
    const first = agentVersionFields({ ...base, scopes: ["tenant", "run"] });
    const second = agentVersionFields({ ...base, scopes: ["run", "tenant"] });
    expect(first).toEqual(second);
    expect(first.config.studio_intent).toMatchObject({ memory: { scopes: ["run", "tenant"] } });
  });

  it("keeps the guided builder free of automated WCAG A and AA violations", async () => {
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    const results = await axe.run(document.body, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"] } });
    expect(results.violations.map((violation) => violation.id)).toEqual([]);
  });

  it("renders real agent evidence as the compact portfolio table", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([
      { assistant_id: "analyst", name: "Research analyst", graph: "research", config: {}, metadata: { description: "Verify claims and cite sources" }, created_at: "2026-08-11T00:00:00Z", active_version_id: "av-live", version_count: 3 },
      { assistant_id: "retired", name: "Legacy helper", graph: "missing_graph", config: {}, metadata: {}, created_at: "2026-08-10T00:00:00Z", active_version_id: "av-old", version_count: 1, archived_at: "2026-08-11T01:00:00Z" },
    ])));
    renderPage();

    const table = await screen.findByRole("table", { name: "Agent portfolio" });
    expect(table).toHaveTextContent("Research analyst");
    expect(table).toHaveTextContent("Verify claims and cite sources");
    expect(screen.getByRole("link", { name: "Open Research analyst" })).toHaveAttribute("href", "/agents/analyst");
    expect(screen.getByText("Active")).toBeVisible();
    expect(screen.getByText("Archived")).toBeVisible();
  });

  it("never presents a zero agent count before catalog evidence settles", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [] },
    });
    let settle!: (response: Response) => void;
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => new Promise<Response>((resolve) => { settle = resolve; })));
    renderPage();
    expect(await screen.findByText("Loading this workspace…")).toBeVisible();
    expect(screen.queryByText(/0 in this workspace/)).not.toBeInTheDocument();
    settle(new Response("not-json", { status: 500 }));
    expect(await screen.findByText("Agent count unavailable")).toBeVisible();
  });

  it("keeps non-round-trippable stored definitions view-only", () => {
    const intent = {
      format: "rusty.agent-intent/v3", model: "model-v1", tools: [],
      memory: { access: "none", scopes: [] }, approval: "runtime_policy",
      output: { mode: "text", schema: "" }, budget: {}, binding: {},
    };
    const definition = { name: "Analyst", graph: "research", config: { studio_intent: intent, recursion_limit: 12 }, metadata: { description: "Review evidence" } };
    expect(editableAgent(definition)).toBe(true);
    expect(editableAgent({ ...definition, config: { studio_intent: { ...intent, extension: true } } })).toBe(false);
    expect(editableAgent({ ...definition, config: { ...definition.config, recursion_limit: 1.5 } })).toBe(false);
    expect(editableAgent({ ...definition, metadata: { description: 42 } })).toBe(false);
  });
  it("keeps model credentials, hidden identities, and duplicate tools out of portable intent", () => {
    expect(() => modelRequirement("sk-abcdefghijklmnopqrstuvwxyz")).toThrow("secret token");
    expect(() => modelRequirement("https://user:secret@models.example/model")).toThrow("model identifier");
    expect(() => modelRequirement(" model-v1")).toThrow("surrounding spaces");
    expect(() => modelRequirement("model name")).toThrow("model identifier");
    expect(() => modelRequirement("SK-ABCDEFGHIJKLMNOPQRSTUVWXYZ")).toThrow("secret token");
    expect(() => modelRequirement("model\u202ehidden")).toThrow("hidden controls");
    expect(() => toolContracts("search | read_only\nsearch | pure")).toThrow("only once");
    expect(() => toolContracts(Array.from({ length: 17 }, (_, index) => `tool_${index} | pure`).join("\n"))).toThrow("no more than 16");
    expect(() => outputSchemaRequirement(" report.v1", "json_schema")).toThrow("surrounding spaces");
    expect(() => outputSchemaRequirement("https://schema.example/report", "json_schema")).toThrow("named schema identifier");
    expect(outputSchemaRequirement("report.v1", "json_schema")).toBe("report.v1");
    expect(modelRequirement("api-speech-preview")).toBe("api-speech-preview");
  });
  it("preserves an offline agent draft when its first workspace opens", async () => {
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Research analyst");
    await userEvent.type(screen.getByLabelText("Responsibility"), "Investigate claims");
    expect(screen.getByLabelText("Behavior")).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Choose workspace" }));
    expect(useConnectionStore.getState().dialogOpen).toBe(true);
    await useConnectionStore.getState().connect("https://rusty.example", "key", { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] });
    await waitFor(() => expect(screen.getByLabelText("Name")).toHaveValue("Research analyst"));
    expect(screen.getByLabelText("Responsibility")).toHaveValue("Investigate claims");
    expect(screen.getByLabelText("Behavior")).toBeEnabled();
  });

  it("keeps drafts private to their workspace and restores them when the user returns", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Private analyst");
    await useConnectionStore.getState().connect("https://first.example", "first-key", { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] });
    await waitFor(() => expect(screen.getByLabelText("Name")).toHaveValue("Private analyst"));
    await useConnectionStore.getState().connect("https://second.example", "second-key", { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] });
    await waitFor(() => expect(screen.getByRole("heading", { name: "New agent" })).toBeVisible());
    expect(screen.getByLabelText("Name")).toHaveValue("");
    await useConnectionStore.getState().connect("https://first.example", "first-key", { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] });
    await waitFor(() => expect(screen.getByLabelText("Name")).toHaveValue("Private analyst"));
  });

  it("creates an agent from the complete capability draft and admits the exact receipt", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    const fetchMock = vi.fn().mockImplementation((_url: string, init?: RequestInit) => {
      if (init?.method !== "POST") return json([]);
      const body = JSON.parse(String(init.body));
      return json({ ...body, created_at: "2026-08-11T00:00:00Z", active_version_id: "av-1", version_count: 1 }, 201);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Research analyst");
    await userEvent.selectOptions(screen.getByLabelText("Behavior"), "research");
    await userEvent.type(screen.getByLabelText("Responsibility"), "Investigate claims");
    await userEvent.click(screen.getByRole("tab", { name: /Model/ }));
    await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
    for (const capability of ["Goals", "Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("tab", { name: new RegExp(capability) }));
    expect(screen.getByText("Nothing runs until you create version 1.")).toBeVisible();
    expect(screen.getByText(/Requirements apply only where the selected behavior and deployment support them/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Review version 1" })).toHaveFocus());
    expect(screen.getAllByText("Investigate claims").length).toBeGreaterThan(0);
    expect(screen.getByText("No success criteria")).toBeVisible();
    expect(screen.getByText("No memory access")).toBeVisible();
    expect(screen.getByText("Deployment Policy · deployment step limit")).toBeVisible();
    expect(fetchMock.mock.calls.some((call) => call[1]?.method === "POST")).toBe(false);
    expect((await axe.run(document.body, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"] } })).violations.map((violation) => violation.id)).toEqual([]);
    fireEvent.submit(screen.getByRole("button", { name: "Create version 1" }).closest("form")!);
    expect(await screen.findByRole("heading", { name: "Research analyst is ready for its first task" })).toBeVisible();
    await waitFor(() => expect(screen.getByRole("heading", { name: "Research analyst is ready for its first task" })).toHaveFocus());
    expect((await axe.run(document.body, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"] } })).violations.map((violation) => violation.id)).toEqual([]);
    const request = fetchMock.mock.calls.find((call) => call[1]?.method === "POST");
    const body = JSON.parse(String(request?.[1]?.body));
    expect(body).toMatchObject({ config: { studio_intent: { format: "rusty.agent-intent/v3" } } });
    await userEvent.click(screen.getByRole("button", { name: "Start first task" }));
    expect(await screen.findByRole("heading", { name: "Run workspace" })).toBeVisible();
    expect(useWorkStore.getState().assistant?.assistant_id).toBe(body.assistant_id);
  });

  it("returns from final review to the owned draft with focus and values intact", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
    await userEvent.click(await screen.findByRole("button", { name: "Back to edit" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Review agent" })).toHaveFocus());
    await userEvent.click(screen.getByRole("tab", { name: /Purpose/ }));
    expect(screen.getByLabelText("Name")).toHaveValue("Research analyst");
    expect(screen.getByLabelText("Responsibility")).toHaveValue("Investigate claims");
  });

  it("closes a frozen review when another workspace takes ownership", async () => {
    const info: NonNullable<ReturnType<typeof useConnectionStore.getState>["info"]> = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] };
    useConnectionStore.setState({ connection: { epoch: 1, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
    expect(await screen.findByRole("heading", { name: "Review version 1" })).toBeVisible();
    useConnectionStore.setState({ connection: { epoch: 2, origin: "https://b.example", apiKey: "b", tenantFingerprint: "b" }, info });
    await waitFor(() => expect(screen.queryByRole("heading", { name: "Review version 1" })).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Review 7 more" })).toBeDisabled();
  });

  it("requires a fresh review after the same workspace reconnects", async () => {
    const info: NonNullable<ReturnType<typeof useConnectionStore.getState>["info"]> = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] };
    useConnectionStore.setState({ connection: { epoch: 1, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
    useConnectionStore.setState({ connection: { epoch: 2, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    expect(await screen.findByRole("alert")).toHaveTextContent("Review this agent again");
    expect(screen.queryByRole("heading", { name: "Review version 1" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Review agent" })).toBeEnabled();
  });

  it("keeps a definitive create failure visible in the workspace that owns it", async () => {
    const info: NonNullable<ReturnType<typeof useConnectionStore.getState>["info"]> = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] };
    useConnectionStore.setState({ connection: { epoch: 1, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    let finishPost!: (response: Response) => void;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => init?.method === "POST" ? new Promise<Response>((resolve) => { finishPost = resolve; }) : json([])));
    const { router } = renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await reviewAndCreate();
    useConnectionStore.setState({ connection: { epoch: 2, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    finishPost(new Response(JSON.stringify({ error: "Agent definition was rejected" }), { status: 400 }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/rejected|400/i);
    useConnectionStore.setState({ connection: { epoch: 3, origin: "https://b.example", apiKey: "b", tenantFingerprint: "b" }, info });
    await waitFor(() => expect(screen.getByRole("button", { name: "Review 7 more" })).toBeDisabled());
    useConnectionStore.setState({ connection: { epoch: 4, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    expect(await screen.findByRole("alert")).toHaveTextContent(/rejected|400/i);
    void router.navigate({ to: "/agents/prompts" });
    await userEvent.click(await screen.findByRole("button", { name: /Discard/ }));
    expect(await screen.findByText("Prompts")).toBeVisible();
    await router.navigate({ to: "/agents/new" });
    expect(await screen.findByRole("heading", { name: "New agent" })).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps an ambiguous create locked across a reconnect to the same tenant", async () => {
    const identity = { origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "same" };
    useConnectionStore.setState({ connection: { epoch: 1, ...identity }, info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] } });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => init?.method === "POST" ? Promise.reject(new Error("lost")) : json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Analyst");
    await userEvent.selectOptions(screen.getByLabelText("Behavior"), "research");
    await userEvent.type(screen.getByLabelText("Responsibility"), "Investigate claims");
    await userEvent.click(screen.getByRole("tab", { name: /Model/ }));
    await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
    for (const capability of ["Goals", "Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("tab", { name: new RegExp(capability) }));
    await reviewAndCreate();
    expect(await screen.findByRole("button", { name: "Create locked" })).toBeDisabled();
    useConnectionStore.setState({ connection: { epoch: 2, ...identity } });
    expect(await screen.findByRole("button", { name: "Create locked" })).toBeDisabled();
  });

  it("protects a new-agent draft when leaving the builder", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    const { router } = renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Research analyst");

    void router.navigate({ to: "/agents/prompts" });
    expect(await screen.findByRole("dialog", { name: "Discard your changes?" })).toBeVisible();
    await waitFor(() => expect(screen.getByRole("button", { name: "Keep editing" })).toHaveFocus());
    await userEvent.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(router.state.location.pathname).toBe("/agents/new");
    expect(screen.getByLabelText("Name")).toHaveValue("Research analyst");

    const secondNavigation = router.navigate({ to: "/agents/prompts" });
    await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
    await secondNavigation;
    expect(await screen.findByText("Prompts")).toBeVisible();
  });

  it("protects a draft parked in another workspace before leaving the builder", async () => {
    const info: NonNullable<ReturnType<typeof useConnectionStore.getState>["info"]> = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] };
    useConnectionStore.setState({ connection: { epoch: 1, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    const { router } = renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Workspace A draft");
    useConnectionStore.setState({ connection: { epoch: 2, origin: "https://b.example", apiKey: "b", tenantFingerprint: "b" }, info });
    await waitFor(() => expect(screen.getByLabelText("Name")).toHaveValue(""));

    void router.navigate({ to: "/agents/prompts" });
    expect(await screen.findByRole("dialog", { name: "Discard 1 workspace drafts?" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Discard all drafts" })).toBeVisible();
  });

  it("shows and focuses the capability that contains an invalid value", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await userEvent.click(screen.getByRole("tab", { name: /Tools/ }));
    await userEvent.click(screen.getByRole("button", { name: "Add tool" }));
    await userEvent.click(screen.getByRole("tab", { name: /Guardrails/ }));
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));

    expect(await screen.findByText("Use one `tool_name | effect` contract per line.")).toBeVisible();
    await waitFor(() => expect(screen.getByRole("tab", { name: /Tools/ })).toHaveFocus());
  });

  it("keeps invalid metric targets editable and routes their error to Goals", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await userEvent.click(screen.getByRole("tab", { name: /Goals/ }));
    await userEvent.click(screen.getByRole("button", { name: "Add Task success rate" }));
    const target = screen.getByRole("spinbutton", { name: "Task success rate target" });
    await userEvent.clear(target);
    await userEvent.type(screen.getByRole("spinbutton", { name: "Task success rate target" }), "101");
    await userEvent.click(screen.getByRole("tab", { name: /Guardrails/ }));
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
    expect(await screen.findByText(/Task success rate needs a valid numeric target from 0 to 100/)).toBeVisible();
    await waitFor(() => expect(screen.getByRole("tab", { name: /Goals/ })).toHaveFocus());
  });

  it("freezes the submitted definition and blocks departure until creation settles", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    let finish!: (response: Response) => void;
    let submitted: Record<string, unknown> = {};
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => {
      if (init?.method !== "POST") return json([]);
      submitted = JSON.parse(String(init.body));
      return new Promise<Response>((resolve) => { finish = resolve; });
    }));
    const { router } = renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await reviewAndCreate();
    expect(screen.getByRole("heading", { name: "Review version 1" })).toBeVisible();
    void router.navigate({ to: "/agents/prompts" });
    expect(await screen.findByRole("dialog", { name: "Rusty is still creating this agent" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Discard changes" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Stay here" }));
    finish(new Response(JSON.stringify({ ...submitted, created_at: "2026-08-11T00:00:00Z", active_version_id: "av-1", version_count: 1 }), { status: 201 }));
    expect(await screen.findByRole("heading", { name: "Research analyst is ready for its first task" })).toBeVisible();
  });

  it("keeps a late create failure inside the workspace that initiated it", async () => {
    const info: NonNullable<ReturnType<typeof useConnectionStore.getState>["info"]> = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] };
    useConnectionStore.setState({ connection: { epoch: 1, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    let rejectPost!: (reason: Error) => void;
    let postStarted = false;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => {
      if (init?.method === "POST") { postStarted = true; return new Promise<Response>((_resolve, reject) => { rejectPost = reject; }); }
      return json([]);
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await reviewAndCreate();
    expect(postStarted).toBe(true);
    useConnectionStore.setState({ connection: { epoch: 2, origin: "https://b.example", apiKey: "b", tenantFingerprint: "b" }, info });
    rejectPost(new TypeError("connection lost"));
    await waitFor(() => expect(Object.values(useAgentMutationStore.getState().uncertainByConnection).some(Boolean)).toBe(true));
    expect(screen.queryByText(/create result is uncertain/i)).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("button", { name: "Review 7 more" })).toBeDisabled());
  });

  it("records a late exact create in its originating workspace without restoring a committed draft", async () => {
    const info: NonNullable<ReturnType<typeof useConnectionStore.getState>["info"]> = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] };
    useConnectionStore.setState({ connection: { epoch: 1, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    let finishPost!: (response: Response) => void;
    let submitted: Record<string, unknown> = {};
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => {
      if (init?.method === "POST") {
        submitted = JSON.parse(String(init.body));
        return new Promise<Response>((resolve) => { finishPost = resolve; });
      }
      return json([]);
    }));
    renderPage();
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    await completeBuilder();
    await reviewAndCreate();
    await useConnectionStore.getState().connect("https://b.example", "b", info);
    finishPost(new Response(JSON.stringify({ ...submitted, created_at: "2026-08-11T00:00:00Z", active_version_id: "av-1", version_count: 1 }), { status: 201 }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Review 7 more" })).toBeDisabled());
    useConnectionStore.setState({ connection: { epoch: 3, origin: "https://a.example", apiKey: "a", tenantFingerprint: "a" }, info });
    expect(await screen.findByRole("heading", { name: "Research analyst is ready for its first task" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Review agent" }));
    expect(await screen.findByRole("heading", { name: "Agent workspace" })).toBeVisible();
  });

  it("keeps each capability tab associated with the live panel at every viewport", async () => {
    renderPage();
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    await userEvent.click(await screen.findByRole("link", { name: "New agent" }));
    const panel = screen.getByRole("tabpanel");
    expect(panel).toHaveAttribute("id", "agent-capability-panel");
    for (const tab of screen.getAllByRole("tab")) {
      expect(tab).toHaveAttribute("aria-controls", panel.id);
      expect(tab).not.toHaveAttribute("aria-orientation");
    }
  });
});
