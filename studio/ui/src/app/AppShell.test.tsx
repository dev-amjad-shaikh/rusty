import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../state/connection";
import { AppShell } from "./AppShell";
import { primaryDestinations } from "./navigation";

function renderShell() {
  const root = createRootRoute({ component: AppShell });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: () => <h1>Work area</h1> });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: () => <h1>Agents area</h1> });
  const operations = createRoute({ getParentRoute: () => root, path: "/operations", component: () => <h1>Operations area</h1> });
  const router = createRouter({ routeTree: root.addChildren([work, agents, operations]), history: createMemoryHistory({ initialEntries: ["/work"] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

afterEach(() => vi.unstubAllGlobals());

describe("Studio product architecture", () => {
  it("has exactly three primary destinations", () => {
    expect(primaryDestinations.map((item) => item.label)).toEqual(["Forge", "Work", "Operations"]);
  });

  it("presents the current workspace without connection controls", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "tenant" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [] },
      workspaceStatus: "ready", dialogOpen: false,
    });
    renderShell();
    const switcher = await screen.findByRole("button", { name: "Switch workspace, currently rusty.example" });
    expect(screen.getByRole("link", { name: "Forge — Design and activate agents" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Work — Run, trace, and evaluate" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Operations — Review exceptions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Skip to workspace" })).toHaveAttribute("href", "#studio-main");
    expect(switcher).toHaveTextContent("rusty.example");
    expect(screen.queryByText("Disconnect")).not.toBeInTheDocument();
    expect(screen.queryByText("Connect")).not.toBeInTheDocument();
    await userEvent.click(switcher);
    expect(screen.getByRole("dialog", { name: "Switch workspace" })).toBeVisible();
  });

  it("moves focus to the new workspace heading after primary navigation", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "tenant" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [] },
      workspaceStatus: "ready", dialogOpen: false,
    });
    renderShell();
    await userEvent.click(await screen.findByRole("link", { name: "Operations — Review exceptions" }));
    await waitFor(() => expect(screen.getByRole("main", { name: "Operations workspace" })).toHaveFocus());
  });

  it("keeps route content hidden until automatic discovery settles", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => new Promise(() => {})));
    useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "discovering", discoveryAttempt: 21, discoveryError: "", suggestedOrigin: "", dialogOpen: false });
    renderShell();
    expect(await screen.findByRole("heading", { name: "Opening your workspace" })).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Work area" })).not.toBeInTheDocument();
  });
});
