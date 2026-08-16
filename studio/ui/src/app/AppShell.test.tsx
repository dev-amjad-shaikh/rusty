import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider, useBlocker } from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useRef } from "react";
import { useConnectionStore } from "../state/connection";
import { UnsavedChangesDialog } from "../features/agents/UnsavedChangesDialog";
import { AppShell } from "./AppShell";
import { primaryDestinations } from "./navigation";

function BlockingBuilder() {
  const heading = useRef<HTMLHeadingElement>(null);
  const blocker = useBlocker({ shouldBlockFn: () => true, withResolver: true });
  return <><h1 ref={heading} tabIndex={-1}>Builder area</h1>{blocker.status === "blocked" && <UnsavedChangesDialog returnFocusRef={heading} onKeep={blocker.reset} onDiscard={blocker.proceed} />}</>;
}

function renderShell(initialEntry = "/work", blockBuilder = false) {
  const root = createRootRoute({ component: AppShell });
  const command = createRoute({ getParentRoute: () => root, path: "/", component: () => <h1>Command area</h1> });
  const work = createRoute({ getParentRoute: () => root, path: "/work", component: () => <h1>Work area</h1> });
  const agents = createRoute({ getParentRoute: () => root, path: "/agents", component: () => <h1>Agents area</h1> });
  const builder = createRoute({ getParentRoute: () => root, path: "/agents/new", component: blockBuilder ? BlockingBuilder : () => <h1>Builder area</h1> });
  const operations = createRoute({ getParentRoute: () => root, path: "/operations", component: () => <h1>Operations area</h1> });
  const prompts = createRoute({ getParentRoute: () => root, path: "/agents/prompts", component: () => <h1>Prompts area</h1> });
  const router = createRouter({ routeTree: root.addChildren([command, work, agents, builder, prompts, operations]), history: createMemoryHistory({ initialEntries: [initialEntry] }) });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return { router, ...render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>) };
}

afterEach(() => vi.unstubAllGlobals());

describe("Studio product architecture", () => {
  it("exposes only implemented lifecycle destinations", () => {
    expect(primaryDestinations.map((item) => item.label)).toEqual(["Command Center", "Agent Portfolio", "Agent Builder", "Prompt Library", "Skills & Tools", "Knowledge", "Run & Evaluate", "Memory", "Connectors", "Operations"]);
  });

  it("presents the current workspace without connection controls", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "tenant" },
      info: { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [] },
      workspaceStatus: "ready", dialogOpen: false,
    });
    renderShell();
    const switcher = await screen.findByRole("button", { name: "Switch workspace, currently rusty.example" });
    expect(screen.getByRole("link", { name: "Command Center — See work and exceptions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Agent Portfolio — Review active definitions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Agent Builder — Create a guided definition" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Prompt Library — Version and test prompts" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Run & Evaluate — Run, trace, and evaluate" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Operations — Review exceptions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Skip to workspace" })).toHaveAttribute("href", "#studio-main");
    expect(switcher).toHaveTextContent("rusty.example");
    expect(screen.queryByText("Disconnect")).not.toBeInTheDocument();
    expect(screen.queryByText("Connect")).not.toBeInTheDocument();
    await userEvent.click(switcher);
    expect(screen.getByRole("dialog", { name: "Switch workspace" })).toBeVisible();
  });

  it("uses an explicit mobile navigation disclosure", async () => {
    useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", dialogOpen: false });
    renderShell();
    const menu = await screen.findByRole("button", { name: "Menu" });
    expect(menu).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(menu);
    expect(menu).toHaveAttribute("aria-expanded", "true");
    expect(menu).toHaveAttribute("aria-controls", "studio-navigation");
  });

  it("turns the v4 command surface into real workspace navigation", async () => {
    useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", dialogOpen: false });
    renderShell();
    await userEvent.click(await screen.findByRole("button", { name: /Go to agents, work, prompts, or operations/ }));
    const dialog = screen.getByRole("dialog", { name: "Where do you want to go?" });
    expect(dialog).toBeVisible();
    await userEvent.type(screen.getByPlaceholderText("Agents, work, prompts, operations…"), "builder");
    expect(within(dialog).getByRole("link", { name: /Agent Builder/ })).toBeVisible();
    expect(within(dialog).queryByRole("link", { name: /Operations/ })).not.toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("link", { name: /Agent Builder/ }));
    expect(await screen.findByRole("heading", { name: "Builder area" })).toBeVisible();
  });

  it("returns command navigation focus to the exact opener", async () => {
    useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", dialogOpen: false });
    renderShell();
    const opener = await screen.findByRole("button", { name: /Go to agents, work, prompts, or operations/ });
    opener.focus();
    await userEvent.click(opener);
    await userEvent.click(screen.getByRole("button", { name: "Close navigation" }));
    await waitFor(() => expect(opener).toHaveFocus());

    const workspace = screen.getByRole("button", { name: "Choose a Rusty workspace" });
    workspace.focus();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    const dialog = await screen.findByRole("dialog", { name: "Where do you want to go?" });
    dialog.dispatchEvent(new Event("cancel", { bubbles: true, cancelable: true }));
    await waitFor(() => expect(workspace).toHaveFocus());
  });

  it("returns a blocked command handoff to the stable builder opener", async () => {
    useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", dialogOpen: false });
    const { router } = renderShell("/agents/new", true);
    const opener = await screen.findByRole("button", { name: /Go to agents, work, prompts, or operations/ });
    await userEvent.click(opener);
    const dialog = screen.getByRole("dialog", { name: "Where do you want to go?" });
    await userEvent.click(within(dialog).getByRole("link", { name: /Operations/ }));
    expect(await screen.findByRole("dialog", { name: "Discard your changes?" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(router.state.location.pathname).toBe("/agents/new");
    await waitFor(() => expect(screen.getByRole("heading", { name: "Builder area" })).toHaveFocus());
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
