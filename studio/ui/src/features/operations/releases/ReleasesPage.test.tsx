import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ReleasesPage } from "./ReleasesPage";

function renderPage(initialEntry = "/operations/releases") {
  const root = createRootRoute({ component: Outlet });
  const releases = createRoute({ getParentRoute: () => root, path: "/operations/releases", component: ReleasesPage });
  const releasesEnv = createRoute({ getParentRoute: () => root, path: "/operations/releases/$environment", component: ReleasesPage });
  const router = createRouter({
    routeTree: root.addChildren([releases, releasesEnv]),
    history: createMemoryHistory({ initialEntries: [initialEntry] }),
  });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

function response(value: unknown, status = 200) { return new Response(JSON.stringify(value), { status }); }

afterEach(() => vi.unstubAllGlobals());

const sampleRevision = {
  revision_id: "a1".repeat(32),
  content: {
    graph: "pipeline",
    graph_hash: "b2".repeat(32),
    source_environment: "staging",
    pins: [{ surface: "prompt:system", candidate_id: "c3".repeat(32) }],
  },
  author: { type: "human" as const, human_id: "ops" },
  created_at: "2026-08-11T00:00:00Z",
};

const sampleEnvironment = {
  name: "staging",
  gate: { policy: "r0.12-default", dataset_version: "support-v3" },
  approval_required: false,
  created_by: { type: "human" as const, human_id: "ops" },
  created_at: "2026-08-11T00:00:00Z",
};

const sampleBoard = {
  environment: "staging",
  gate: { policy: "r0.12-default", dataset_version: "support-v3" },
  approval_required: false,
  active_revision: null,
  canary: null,
  last_gate_decision: null,
  recent_runs: { active: { runs: 0, errored: 0, interrupted: 0 }, canary: { runs: 0, errored: 0, interrupted: 0 } },
};

function mockDeploymentApis() {
  vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
    const url = new URL(input, "http://studio.local");
    if (url.pathname.replace(/^\/api/, "") === "/deployments/health") return Promise.resolve(response({ environments: [], deployment_chain_head: null }));
    if (url.pathname.replace(/^\/api/, "") === "/deployments/environments") return Promise.resolve(response({ environments: [sampleEnvironment] }));
    if (url.pathname.replace(/^\/api/, "") === "/deployments/revisions") return Promise.resolve(response({ revisions: [sampleRevision] }));
    if (url.pathname.replace(/^\/api/, "") === "/deployments/journal") return Promise.resolve(response({ run_id: "deployments", events: [], complete: false }));
    throw new Error(`unexpected ${url}`);
  }));
}

describe("Releases workspace", () => {
  it("lists environments and revisions after loading", async () => {
    mockDeploymentApis();
    renderPage();
    const envList = await screen.findByRole("region", { name: "Declared targets" });
    await waitFor(() => expect(within(envList).getByRole("button", { name: /staging/ })).toBeVisible());
    expect(screen.getByText("Immutable pin sets")).toBeVisible();
  });

  it("shows the current decision when an environment is selected", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/deployments/health") return Promise.resolve(response({ environments: [sampleBoard], deployment_chain_head: null }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/environments") return Promise.resolve(response({ environments: [sampleEnvironment] }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/revisions") return Promise.resolve(response({ revisions: [sampleRevision] }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/journal") return Promise.resolve(response({ run_id: "deployments", events: [], complete: false }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/environments/staging/pointer") return Promise.resolve(response({ pointer: { surface: "deployment:staging" } }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/secrets") return Promise.resolve(response({ secrets: [] }));
      throw new Error(`unexpected ${url}`);
    }));
    renderPage("/operations/releases/staging");
    expect(await screen.findByText(/Nothing serves staging/)).toBeVisible();
    expect(screen.getByText(/Current decision/)).toBeVisible();
    expect(screen.getByText(/Gate policy/)).toBeVisible();
  });

  it("declares a new environment and refreshes the list", async () => {
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/deployments/environments" && init?.method === "POST") {
        return Promise.resolve(response({ created: true, environment: { name: "prod", approval_required: false, created_by: { type: "human", human_id: "ops" }, created_at: "2026-08-11T00:00:00Z" } }, 201));
      }
      if (url.pathname.replace(/^\/api/, "") === "/deployments/health") return Promise.resolve(response({ environments: [], deployment_chain_head: null }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/environments") return Promise.resolve(response({ environments: [sampleEnvironment, { name: "prod", approval_required: false, created_by: { type: "human", human_id: "ops" }, created_at: "2026-08-11T00:00:00Z" }] }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/revisions") return Promise.resolve(response({ revisions: [] }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/journal") return Promise.resolve(response({ run_id: "deployments", events: [], complete: false }));
      throw new Error(`unexpected ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPage();
    const envList = await screen.findByRole("region", { name: "Declared targets" });
    await waitFor(() => expect(within(envList).getByRole("button", { name: /staging/ })).toBeVisible());
    await userEvent.click(screen.getByRole("button", { name: "Create environment" }));
    const dialog = screen.getByRole("dialog");
    await userEvent.type(within(dialog).getByPlaceholderText("staging"), "prod");
    await userEvent.click(within(dialog).getByRole("button", { name: "Create environment" }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledWith(expect.stringContaining("/deployments/environments"), expect.objectContaining({ method: "POST" })));
    await waitFor(() => expect(within(envList).getByRole("button", { name: /prod/ })).toBeVisible());
  });

  it("renders deployment timeline events in plain language", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/deployments/health") return Promise.resolve(response({ environments: [], deployment_chain_head: "head" }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/environments") return Promise.resolve(response({ environments: [] }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/revisions") return Promise.resolve(response({ revisions: [] }));
      if (url.pathname.replace(/^\/api/, "") === "/deployments/journal") return Promise.resolve(response({ run_id: "deployments", events: [{ id: "deployments:0", run_id: "deployments", thread_id: "deployments", node_id: null, seq: 0, kind: "environment_declared", effect: "pure", input: null, output: { kind: "inline", value: { declaration: { environment: "staging" } } }, latency_ms: null, tokens: null, cost_usd: null, status: "ok", parent: null, recorded_at: "2026-08-11T00:00:00Z" }], complete: false }));
      throw new Error(`unexpected ${url}`);
    }));
    renderPage();
    await waitFor(() => expect(screen.getByText("Environment declared")).toBeVisible());
  });
});
