import axe from "axe-core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AgentsPage } from "../features/agents/AgentsPage";
import { OperationsPage } from "../features/operations/OperationsPage";
import { WorkPage } from "../features/work/WorkPage";
import { useConnectionStore } from "../state/connection";
import { AppShell } from "./AppShell";

async function scan(path: "/agents" | "/work" | "/operations") {
  useConnectionStore.setState({ connection: null, info: null, dialogOpen: false });
  const root = createRootRoute({ component: AppShell });
  const routes = [
    createRoute({ getParentRoute: () => root, path: "/agents", component: AgentsPage }),
    createRoute({ getParentRoute: () => root, path: "/work", component: WorkPage }),
    createRoute({ getParentRoute: () => root, path: "/operations", component: OperationsPage }),
  ];
  const router = createRouter({ routeTree: root.addChildren(routes), history: createMemoryHistory({ initialEntries: [path] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  const results = await axe.run(document.body, { runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"] } });
  return results.violations.map((violation) => ({ id: violation.id, impact: violation.impact, nodes: violation.nodes.map((node) => node.target) }));
}

describe("typed Studio accessibility", () => {
  it.each(["/agents", "/work", "/operations"] as const)("has no automated WCAG A/AA violations on %s", async (path) => {
    await expect(scan(path)).resolves.toEqual([]);
  });
});
