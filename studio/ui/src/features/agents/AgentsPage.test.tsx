import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { useAgentMutationStore } from "../../state/agents";
import { AgentsPage } from "./AgentsPage";
import { editableAgent, modelRequirement, outputSchemaRequirement, toolContracts } from "./AgentIntentEditor";

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const root = createRootRoute({ component: Outlet });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: AgentsPage });
  const workspace = createRoute({ getParentRoute: () => root, path: "/agents/$assistantId", component: () => <h1>Agent workspace</h1> });
  const prompts = createRoute({ getParentRoute: () => root, path: "/agents/prompts", component: () => <p>Prompts</p> });
  const router = createRouter({ routeTree: root.addChildren([agents, workspace, prompts]), history: createMemoryHistory({ initialEntries: ["/agents"] }) });
  return { router, ...render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>) };
}

function json(value: unknown, status = 200) { return Promise.resolve(new Response(JSON.stringify(value), { status })); }

beforeEach(() => { useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", discoveryAttempt: 0, discoveryError: "", suggestedOrigin: "", dialogOpen: false }); useAgentMutationStore.setState({ uncertainByConnection: {} }); });
afterEach(() => vi.unstubAllGlobals());

describe("Agents", () => {
  it("opens with the Forge thesis and an accessible capability system", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "Build an agent that earns trust." })).toBeVisible();
    expect(screen.getByRole("img", { name: /Agent capability system/ })).toBeVisible();
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
    await waitFor(() => expect(screen.getByRole("heading", { name: "Design your first agent now" })).toBeVisible());
    await userEvent.click(screen.getByRole("button", { name: "Start a draft" }));
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

  it("clears a draft before showing it in a different workspace", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Start a draft" }));
    await userEvent.type(screen.getByLabelText("Name"), "Private analyst");
    await useConnectionStore.getState().connect("https://first.example", "first-key", { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] });
    await waitFor(() => expect(screen.getByLabelText("Name")).toHaveValue("Private analyst"));
    await useConnectionStore.getState().connect("https://second.example", "second-key", { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] });
    await waitFor(() => expect(screen.getByRole("heading", { name: "Create your first worker" })).toBeVisible());
    expect(screen.queryByDisplayValue("Private analyst")).not.toBeInTheDocument();
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
    await waitFor(() => expect(screen.getByRole("heading", { name: "Create your first worker" })).toBeVisible());
    await userEvent.click(screen.getByRole("button", { name: "Create agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Research analyst");
    await userEvent.selectOptions(screen.getByLabelText("Behavior"), "research");
    await userEvent.type(screen.getByLabelText("Responsibility"), "Investigate claims");
    await userEvent.click(screen.getByRole("tab", { name: /Model/ }));
    await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
    for (const capability of ["Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("tab", { name: new RegExp(capability) }));
    expect(screen.getByText(/Enforcement depends on the selected behavior and deployment policies/)).toBeVisible();
    fireEvent.submit(screen.getByRole("button", { name: "Create agent" }).closest("form")!);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Agent workspace" })).toBeVisible());
    const request = fetchMock.mock.calls.find((call) => call[1]?.method === "POST");
    const body = JSON.parse(String(request?.[1]?.body));
    expect(body).toMatchObject({ config: { studio_intent: { format: "rusty.agent-intent/v3" } } });
  });

  it("keeps an ambiguous create locked across a reconnect to the same tenant", async () => {
    const identity = { origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "same" };
    useConnectionStore.setState({ connection: { epoch: 1, ...identity }, info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] } });
    vi.stubGlobal("fetch", vi.fn().mockImplementation((_url: string, init?: RequestInit) => init?.method === "POST" ? Promise.reject(new Error("lost")) : json([])));
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Create agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Analyst");
    await userEvent.selectOptions(screen.getByLabelText("Behavior"), "research");
    await userEvent.type(screen.getByLabelText("Responsibility"), "Investigate claims");
    await userEvent.click(screen.getByRole("tab", { name: /Model/ }));
    await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
    for (const capability of ["Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("tab", { name: new RegExp(capability) }));
    await userEvent.click(screen.getByRole("button", { name: "Create agent" }));
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
    await userEvent.click(await screen.findByRole("button", { name: "Create agent" }));
    await userEvent.type(screen.getByLabelText("Name"), "Research analyst");

    void router.navigate({ to: "/agents/prompts" });
    expect(await screen.findByRole("dialog", { name: "Discard your changes?" })).toBeVisible();
    await waitFor(() => expect(screen.getByRole("button", { name: "Keep editing" })).toHaveFocus());
    await userEvent.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(router.state.location.pathname).toBe("/agents");
    expect(screen.getByLabelText("Name")).toHaveValue("Research analyst");

    const secondNavigation = router.navigate({ to: "/agents/prompts" });
    await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
    await secondNavigation;
    expect(await screen.findByText("Prompts")).toBeVisible();
  });

  it("keeps each capability tab associated with the live panel at every viewport", async () => {
    renderPage();
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [{ name: "research", channels: [] }] },
    });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json([])));
    await userEvent.click(await screen.findByRole("button", { name: "Create agent" }));
    const panel = screen.getByRole("tabpanel");
    expect(panel).toHaveAttribute("id", "agent-capability-panel");
    for (const tab of screen.getAllByRole("tab")) {
      expect(tab).toHaveAttribute("aria-controls", panel.id);
      expect(tab).not.toHaveAttribute("aria-orientation");
    }
  });
});
