import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import type { ConnectionState } from "../api/socket";
import { StudioShell } from "./AppShell";
import type { Role } from "./scopes";

const LIVE: ConnectionState = { kind: "live", seq: 41 };

function renderShell(options: {
  role?: Role;
  connection?: ConnectionState;
  activeRoute?: string;
  onNavigate?: (route: string) => void;
  onRetry?: () => void;
  onRoleChange?: (role: Role) => void;
} = {}) {
  return render(
    <StudioShell
      role={options.role ?? "builder"}
      connection={options.connection ?? LIVE}
      activeRoute={options.activeRoute ?? "/agents"}
      onNavigate={options.onNavigate}
      onRetry={options.onRetry}
      onRoleChange={options.onRoleChange}
    />,
  );
}

function sidebar() {
  return within(screen.getByRole("navigation", { name: "Studio navigation" }));
}

describe("v2 app shell", () => {
  it("renders the frame with all three nav groups for a builder fixture", () => {
    renderShell({ role: "builder" });
    expect(screen.getByText("Rustynome")).toBeInTheDocument();
    for (const group of ["Build", "Test", "Operate"]) {
      expect(sidebar().getByRole("heading", { name: group })).toBeInTheDocument();
    }
    for (const item of ["Agents", "Drafts", "Catalog", "Playground", "Evals", "Observe", "Improve"]) {
      expect(sidebar().getByRole("link", { name: item })).toBeInTheDocument();
    }
  });

  it("gates nav by fixture scopes — capabilities are absent, not disabled", () => {
    renderShell({ role: "auditor" });
    expect(sidebar().queryByRole("heading", { name: "Build" })).toBeNull();
    expect(sidebar().queryByRole("heading", { name: "Test" })).toBeNull();
    expect(sidebar().queryByRole("link", { name: "Agents" })).toBeNull();
    expect(sidebar().queryByRole("link", { name: "Work" })).toBeNull();
    expect(sidebar().getByRole("link", { name: "Security" })).toBeInTheDocument();
    expect(sidebar().getByRole("link", { name: "Inbox" })).toBeInTheDocument();
    // Absent means absent from the DOM — nothing rendered with aria-disabled.
    expect(sidebar().queryByRole("link", { name: "Playground" })).toBeNull();
  });

  it("marks the active route and navigates through onNavigate", async () => {
    const user = userEvent.setup();
    const navigated: string[] = [];
    renderShell({ role: "admin", activeRoute: "/agents", onNavigate: (route) => navigated.push(route) });
    expect(sidebar().getByRole("link", { name: "Agents" })).toHaveAttribute("aria-current", "page");
    await user.click(sidebar().getByRole("link", { name: "Security" }));
    expect(navigated).toEqual(["/security"]);
  });

  it("shows no banner while live", () => {
    renderShell({ connection: { kind: "live", seq: 9 } });
    expect(screen.queryByRole("status", { name: "Connection status" })).toBeNull();
  });

  it("shows the reconnecting banner with the last snapshot seq on an injected gap", () => {
    renderShell({ connection: { kind: "reconnecting", lastSeq: 17 } });
    const banner = screen.getByRole("status", { name: "Connection status" });
    expect(banner).toHaveTextContent("Showing last snapshot · seq 17 · live updates paused");
  });

  it("shows the offline banner and its Retry button calls onRetry", async () => {
    const user = userEvent.setup();
    let retries = 0;
    renderShell({ connection: { kind: "offline", lastSeq: 4 }, onRetry: () => { retries += 1; } });
    const banner = screen.getByRole("status", { name: "Connection status" });
    expect(banner).toHaveTextContent("Offline · counts and lists may be stale · actions are queued, not applied");
    await user.click(within(banner).getByRole("button", { name: "Retry" }));
    expect(retries).toBe(1);
  });

  it("re-routes to the nearest permitted screen when the role drops the current one", async () => {
    const user = userEvent.setup();
    const navigated: string[] = [];
    function Harness() {
      const [role, setRole] = useState<Role>("admin");
      return (
        <StudioShell
          role={role}
          onRoleChange={setRole}
          connection={LIVE}
          activeRoute="/security"
          onNavigate={(route) => navigated.push(route)}
        />
      );
    }
    render(<Harness />);
    await user.selectOptions(screen.getByRole("combobox", { name: "Role" }), "builder");
    expect(navigated).toEqual(["/agents"]);
  });

  it("renders the placeholder for a permitted route without screen content", () => {
    renderShell({ role: "builder", activeRoute: "/drafts" });
    expect(screen.getByRole("heading", { name: "Drafts", level: 1 })).toBeInTheDocument();
    expect(screen.getByText("This screen is not built yet — it lands with its own story.")).toBeInTheDocument();
  });

  it("toggles between the two themes on the shell root", async () => {
    const user = userEvent.setup();
    const { container } = renderShell();
    const root = container.firstElementChild!;
    expect(root).toHaveAttribute("data-theme", "light");
    await user.click(screen.getByRole("button", { name: "Toggle theme" }));
    expect(root).toHaveAttribute("data-theme", "dark");
    await user.click(screen.getByRole("button", { name: "Toggle theme" }));
    expect(root).toHaveAttribute("data-theme", "light");
  });
});
