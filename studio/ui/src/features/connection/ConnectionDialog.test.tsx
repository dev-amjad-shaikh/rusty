import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { ConnectionDialog } from "./ConnectionDialog";
import { localWorkspaceOrigin } from "./WorkspaceBootstrap";

function renderDialog() {
  const client = new QueryClient();
  return render(<QueryClientProvider client={client}><button type="button" onClick={useConnectionStore.getState().openDialog}>Open connection</button><ConnectionDialog /></QueryClientProvider>);
}

const info = { service: "rusty-server" as const, version: "1", checkpointer: "json_file" as const, server_store: "json_file" as const, store_path: "/tmp", graphs: [] };

beforeEach(() => useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", discoveryAttempt: 0, discoveryError: "", suggestedOrigin: "", dialogOpen: false }));
afterEach(() => vi.unstubAllGlobals());

describe("connection boundary", () => {
  it("rejects credentialed or path-bearing server addresses", async () => {
    const fetchMock = vi.fn(); vi.stubGlobal("fetch", fetchMock);
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    await userEvent.click(screen.getByText("Use another server or access key"));
    const input = screen.getByLabelText("Server address");
    await userEvent.clear(input); await userEvent.type(input, "https://user:secret@rusty.example/api?key=x");
    await userEvent.click(screen.getByRole("button", { name: "Open workspace" }));
    expect(screen.getByRole("alert")).toHaveTextContent("http or https address");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("opens the local workspace without asking for infrastructure details", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify(info), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    await userEvent.click(screen.getByRole("button", { name: "Use local workspace" }));
    await waitFor(() => expect(useConnectionStore.getState().connection?.origin).toBe(localWorkspaceOrigin()));
    expect(fetchMock).toHaveBeenCalledWith(`${localWorkspaceOrigin()}/info`, expect.any(Object));
    expect(useConnectionStore.getState().dialogOpen).toBe(false);
  });

  it("reveals access-key recovery when the local workspace requires authorization", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 401 })));
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    await userEvent.click(screen.getByRole("button", { name: "Use local workspace" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("needs an access key");
    expect(screen.getByText("Use another server or access key").parentElement).toHaveAttribute("open");
    expect(screen.getByLabelText(/Access key/)).toBeVisible();
  });

  it("accepts the product gateway path but rejects arbitrary server paths", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify(info), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    await userEvent.click(screen.getByText("Use another server or access key"));
    const input = screen.getByLabelText("Server address");
    await userEvent.clear(input); await userEvent.type(input, "https://rusty.example/api/");
    await userEvent.click(screen.getByRole("button", { name: "Open workspace" }));
    await waitFor(() => expect(useConnectionStore.getState().connection?.origin).toBe("https://rusty.example/api"));
  });

  it("uses an access key without leaving it in the closed interface", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify(info), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    await userEvent.click(screen.getByText("Use another server or access key"));
    await userEvent.clear(screen.getByLabelText("Server address"));
    await userEvent.type(screen.getByLabelText("Server address"), "https://rusty.example");
    await userEvent.type(screen.getByLabelText(/Access key/), "private-key");
    await userEvent.click(screen.getByRole("button", { name: "Open workspace" }));
    await waitFor(() => expect(useConnectionStore.getState().dialogOpen).toBe(false));
    expect(fetchMock).toHaveBeenCalledWith("https://rusty.example/info", expect.objectContaining({ headers: expect.objectContaining({ "X-Api-Key": "private-key" }) }));
    expect(screen.queryByDisplayValue("private-key")).not.toBeInTheDocument();
  });

  it("closes with Escape and restores the opener focus", async () => {
    renderDialog();
    const opener = screen.getByRole("button", { name: "Open connection" }); opener.focus();
    await userEvent.click(opener);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Open a workspace" })).toHaveFocus());
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(opener).toHaveFocus());
  });

  it("keeps advanced workspace recovery in the keyboard loop while collapsed", async () => {
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Open a workspace" })).toHaveFocus());
    await userEvent.keyboard("{Shift>}{Tab}{/Shift}");
    expect(screen.getByText("Use another server or access key")).toHaveFocus();
  });

  it("does not offer to reopen the local workspace when it is already active", async () => {
    useConnectionStore.setState({
      connection: { epoch: 1, origin: localWorkspaceOrigin(), apiKey: "", tenantFingerprint: "local" },
      info,
      workspaceStatus: "ready",
    });
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    expect(screen.getByText("Current workspace")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Use local workspace" })).not.toBeInTheDocument();
  });
});
