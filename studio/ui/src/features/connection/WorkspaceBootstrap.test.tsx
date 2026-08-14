import { StrictMode } from "react";
import { render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { localWorkspaceOrigin, resetWorkspaceDiscoveryForTests, WORKSPACE_DISCOVERY_TIMEOUT_MS, WorkspaceBootstrap } from "./WorkspaceBootstrap";

const info = { service: "rusty-server" as const, version: "1", checkpointer: "json_file" as const, server_store: "json_file" as const, store_path: "/tmp", graphs: [] };

function response(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
}

beforeEach(() => {
  resetWorkspaceDiscoveryForTests();
  useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "discovering", discoveryAttempt: 0, discoveryError: "", suggestedOrigin: "", dialogOpen: false });
});
afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); });

describe("automatic workspace discovery", () => {
  it("opens the local Studio gateway once under React StrictMode", async () => {
    const fetchMock = vi.fn().mockResolvedValue(response(info));
    vi.stubGlobal("fetch", fetchMock);
    render(<StrictMode><WorkspaceBootstrap /></StrictMode>);
    await waitFor(() => expect(useConnectionStore.getState().workspaceStatus).toBe("ready"));
    expect(useConnectionStore.getState().connection).toMatchObject({ origin: localWorkspaceOrigin(), apiKey: "" });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledWith(`${localWorkspaceOrigin()}/info`, expect.any(Object));
  });

  it("falls back to APIs served directly from the Studio origin", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ message: "missing" }, 404))
      .mockResolvedValueOnce(response(info));
    vi.stubGlobal("fetch", fetchMock);
    render(<WorkspaceBootstrap />);
    await waitFor(() => expect(useConnectionStore.getState().workspaceStatus).toBe("ready"));
    expect(useConnectionStore.getState().connection?.origin).toBe(window.location.origin);
    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([`${localWorkspaceOrigin()}/info`, `${window.location.origin}/info`]);
  });

  it("shows recovery after local discovery fails and can retry safely", async () => {
    const fetchMock = vi.fn().mockRejectedValue(new Error("offline"));
    vi.stubGlobal("fetch", fetchMock);
    render(<WorkspaceBootstrap />);
    await waitFor(() => expect(useConnectionStore.getState().workspaceStatus).toBe("unavailable"));
    expect(useConnectionStore.getState().discoveryError).toContain("not available");
    fetchMock.mockReset();
    fetchMock.mockResolvedValue(response(info));
    useConnectionStore.getState().retryDiscovery();
    await waitFor(() => expect(useConnectionStore.getState().workspaceStatus).toBe("ready"));
  });

  it("leaves the opening screen when a local server never answers", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => new Promise(() => {})));
    render(<WorkspaceBootstrap />);
    await vi.advanceTimersByTimeAsync(WORKSPACE_DISCOVERY_TIMEOUT_MS);
    expect(useConnectionStore.getState().workspaceStatus).toBe("unavailable");
    expect(useConnectionStore.getState().discoveryError).toContain("not available");
  });

  it("does not let late discovery replace a manual workspace choice", async () => {
    let resolve!: (value: Response) => void;
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => new Promise<Response>((done) => { resolve = done; })));
    render(<StrictMode><WorkspaceBootstrap /></StrictMode>);
    await waitFor(() => expect(resolve).toBeTypeOf("function"));
    useConnectionStore.getState().openDialog();
    resolve(response(info));
    await new Promise((done) => setTimeout(done, 0));
    expect(useConnectionStore.getState().connection).toBeNull();
    expect(useConnectionStore.getState().workspaceStatus).toBe("unavailable");
  });
});
