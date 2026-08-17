import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, RouterProvider, useBlocker } from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useRef } from "react";
import { useRuntimeStore } from "../state/runtime";
import type { ServerInfo } from "../lib/contracts";
import { UnsavedChangesDialog } from "../features/agents/UnsavedChangesDialog";
import { AppShell } from "./AppShell";
import { primaryDestinations } from "./navigation";

const localInfo: ServerInfo = { service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp", graphs: [] };

function runtimeReady() {
  useRuntimeStore.setState({ status: "ready", info: localInfo, error: "", attempt: 0 });
}

function runtimeUnavailable() {
  useRuntimeStore.setState({ status: "unavailable", info: null, error: "", attempt: 0 });
}

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
    expect(primaryDestinations.map((item) => item.label)).toEqual(["Command Center", "Agent Portfolio", "Agent Builder", "Prompt Library", "Skills & Tools", "Knowledge", "Connectors", "Run & Evaluate", "Memory", "Operations"]);
  });

  it("presents the local workspace without connection controls", async () => {
    runtimeReady();
    renderShell();
    const indicator = await screen.findByRole("status");
    expect(indicator).toHaveTextContent("Local workspace");
    expect(indicator).toHaveTextContent("Rusty 1");
    expect(screen.getByRole("link", { name: "Command Center — See work and exceptions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Agent Portfolio — Review active definitions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Agent Builder — Create a guided definition" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Prompt Library — Version and test prompts" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Run & Evaluate — Run, trace, and evaluate" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Operations — Review exceptions" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Skip to workspace" })).toHaveAttribute("href", "#studio-main");
    expect(screen.queryByRole("button", { name: /Switch workspace/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Choose a Rusty workspace/ })).not.toBeInTheDocument();
    expect(screen.queryByText("Disconnect")).not.toBeInTheDocument();
    expect(screen.queryByText("Connect")).not.toBeInTheDocument();
  });

  it("uses an explicit mobile navigation disclosure", async () => {
    runtimeUnavailable();
    renderShell();
    const menu = await screen.findByRole("button", { name: "Menu" });
    expect(menu).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(menu);
    expect(menu).toHaveAttribute("aria-expanded", "true");
    expect(menu).toHaveAttribute("aria-controls", "studio-navigation");
  });

  it("turns the v4 command surface into real workspace navigation", async () => {
    runtimeReady();
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
    runtimeUnavailable();
    renderShell();
    const opener = await screen.findByRole("button", { name: /Go to agents, work, prompts, or operations/ });
    opener.focus();
    await userEvent.click(opener);
    await userEvent.click(screen.getByRole("button", { name: "Close navigation" }));
    await waitFor(() => expect(opener).toHaveFocus());

    const menu = screen.getByRole("button", { name: "Menu" });
    menu.focus();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    const dialog = await screen.findByRole("dialog", { name: "Where do you want to go?" });
    dialog.dispatchEvent(new Event("cancel", { bubbles: true, cancelable: true }));
    await waitFor(() => expect(menu).toHaveFocus());
  });

  it("returns a blocked command handoff to the stable builder opener", async () => {
    runtimeReady();
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
    runtimeReady();
    renderShell();
    await userEvent.click(await screen.findByRole("link", { name: "Operations — Review exceptions" }));
    await waitFor(() => expect(screen.getByRole("main", { name: "Operations workspace" })).toHaveFocus());
  });

  it("keeps route content hidden until the local runtime answers", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => new Promise(() => {})));
    useRuntimeStore.setState({ status: "starting", info: null, error: "", attempt: 0 });
    renderShell();
    expect(await screen.findByRole("heading", { name: "Starting the local runtime…" })).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Work area" })).not.toBeInTheDocument();
  });

  it("offers a retry when the local runtime refuses Studio", async () => {
    useRuntimeStore.setState({ status: "unavailable", info: null, error: "This Rusty server needs an access key.", attempt: 0 });
    renderShell();
    expect(await screen.findByRole("alert")).toHaveTextContent("This Rusty server needs an access key.");
    expect(screen.getByRole("button", { name: "Retry" })).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Work area" })).not.toBeInTheDocument();
  });
});
