import axe from "axe-core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AgentsPage } from "../features/agents/AgentsPage";
import { OperationsPage } from "../features/operations/OperationsPage";
import { ReleasesPage } from "../features/operations/releases/ReleasesPage";
import { WorkPage } from "../features/work/WorkPage";
import { CommandCenter } from "../features/command-center/CommandCenter";
import { useRuntimeStore } from "../state/runtime";
import { AppShell } from "./AppShell";

async function scan(path: "/" | "/agents" | "/work" | "/operations" | "/operations/releases") {
  // Pages render once the local runtime has answered; failing requests leave
  // each page in its loading or error state, which is what we scan here.
  vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("offline")));
  useRuntimeStore.setState({
    status: "ready",
    info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [] },
    error: "",
    attempt: 0,
  });
  const root = createRootRoute({ component: AppShell });
  const routes = [
    createRoute({ getParentRoute: () => root, path: "/", component: CommandCenter }),
    createRoute({ getParentRoute: () => root, path: "/agents", component: AgentsPage }),
    createRoute({ getParentRoute: () => root, path: "/work", component: WorkPage }),
    createRoute({ getParentRoute: () => root, path: "/operations", component: OperationsPage }),
    createRoute({ getParentRoute: () => root, path: "/operations/releases", component: ReleasesPage }),
  ];
  const router = createRouter({ routeTree: root.addChildren(routes), history: createMemoryHistory({ initialEntries: [path] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  await new Promise((resolve) => setTimeout(resolve, 50));
  const results = await axe.run(document.body, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"] } });
  return results.violations.map((violation) => ({ id: violation.id, impact: violation.impact, nodes: violation.nodes.map((node) => node.target) }));
}

describe("typed Studio accessibility", () => {
  it.each(["/", "/agents", "/work", "/operations", "/operations/releases"] as const)("has no automated WCAG A/AA violations on %s", async (path) => {
    await expect(scan(path)).resolves.toEqual([]);
  });
});
