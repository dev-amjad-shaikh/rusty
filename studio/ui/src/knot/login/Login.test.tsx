import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { Login } from "./Login";

const world: { oidc: { configured: false } | { configured: true; name: string; issuer: string; start: string } } = { oidc: { configured: false } };
const serverState = vi.hoisted(() => ({ refused: false, problem: null as string | null, refresh: async () => {} }));
vi.mock("../../engine/net/server", () => ({ useServer: Object.assign((sel: (s: typeof serverState) => unknown) => sel(serverState), { getState: () => serverState }) }));
vi.mock("../../engine/net/client", () => ({
  apiBase: () => "http://127.0.0.1:8100",
  login: async () => ({}),
  oidcPublic: async () => world.oidc,
  oidcStartUrl: (returnTo?: string) => `http://127.0.0.1:8100/auth/oidc/start${returnTo ? `?return_to=${encodeURIComponent(returnTo)}` : ""}`,
}));

function renderLogin() {
  const rootRoute = createRootRoute({ component: () => <Outlet /> });
  const route = createRoute({ getParentRoute: () => rootRoute, path: "/login", component: Login });
  const router = createRouter({ routeTree: rootRoute.addChildren([route]), history: createMemoryHistory({ initialEntries: ["/login"] }) });
  return render(<RouterProvider router={router} />);
}

describe("Login", () => {
  it("says so when a session ran out, so the drop to the form does not read as a fault", async () => {
    serverState.refused = true; serverState.problem = "your session ended — sign in again";
    renderLogin();
    expect(await screen.findByText(/Your session ended — a session ends after 12 hours without you/)).toBeInTheDocument();
    serverState.refused = false; serverState.problem = null;
  });

  it("offers names and passwords only until a provider is set", async () => {
    world.oidc = { configured: false };
    renderLogin();
    await screen.findByLabelText("Sign-in name");
    await new Promise((r) => setTimeout(r, 0));
    expect(document.querySelector('[data-action="sign-in-oidc"]')).toBeNull();
  });

  it("offers Sign in with the provider, sent through the server's start route and back to the studio", async () => {
    world.oidc = { configured: true, name: "Example Identity", issuer: "http://127.0.0.1:8300", start: "/auth/oidc/start" };
    renderLogin();
    const link = await screen.findByText("Sign in with Example Identity");
    expect(link.getAttribute("href")).toBe("http://127.0.0.1:8100/auth/oidc/start?return_to=%2Fagents");
  });
});
