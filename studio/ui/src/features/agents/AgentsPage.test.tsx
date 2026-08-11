import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { useAgentMutationStore } from "../../state/agents";
import { AgentsPage, modelRequirement, outputSchemaRequirement, toolContracts } from "./AgentsPage";

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const root = createRootRoute({ component: Outlet });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: AgentsPage });
  const prompts = createRoute({ getParentRoute: () => root, path: "/agents/prompts", component: () => <p>Prompts</p> });
  const router = createRouter({ routeTree: root.addChildren([agents, prompts]), history: createMemoryHistory({ initialEntries: ["/agents"] }) });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

function json(value: unknown, status = 200) { return Promise.resolve(new Response(JSON.stringify(value), { status })); }

beforeEach(() => { useConnectionStore.setState({ connection: null, info: null, dialogOpen: false }); useAgentMutationStore.setState({ uncertainByConnection: {} }); });
afterEach(() => vi.unstubAllGlobals());

describe("Agents", () => {
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
  it("offers one clear connection action when disconnected", async () => {
    renderPage();
    await waitFor(() => expect(screen.getByRole("heading", { name: "Connect Rusty to load agents" })).toBeVisible());
    await userEvent.click(screen.getByRole("button", { name: "Connect Rusty" }));
    expect(useConnectionStore.getState().dialogOpen).toBe(true);
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
    await userEvent.click(screen.getByRole("button", { name: /Model/ }));
    await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
    for (const capability of ["Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("button", { name: new RegExp(capability) }));
    expect(screen.getByText(/Enforcement depends on the selected behavior and deployment policies/)).toBeVisible();
    fireEvent.submit(screen.getByRole("button", { name: "Create agent" }).closest("form")!);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Research analyst" })).toBeVisible());
    const request = fetchMock.mock.calls.find((call) => call[1]?.method === "POST");
    const body = JSON.parse(String(request?.[1]?.body));
    expect(screen.getByRole("link", { name: "Manage" })).toHaveAttribute("href", `/advanced/legacy?studio=agents&agent=${body.assistant_id}`);
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
    await userEvent.click(screen.getByRole("button", { name: /Model/ }));
    await userEvent.type(screen.getByLabelText("Model requirement"), "model-v1");
    for (const capability of ["Knowledge", "Tools", "Output", "Guardrails"]) await userEvent.click(screen.getByRole("button", { name: new RegExp(capability) }));
    await userEvent.click(screen.getByRole("button", { name: "Create agent" }));
    expect(await screen.findByRole("button", { name: "Create locked" })).toBeDisabled();
    useConnectionStore.setState({ connection: { epoch: 2, ...identity } });
    expect(await screen.findByRole("button", { name: "Create locked" })).toBeDisabled();
  });
});
