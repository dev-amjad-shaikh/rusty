import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const calls = { health: vi.fn(), listAssistants: vi.fn(), listRuns: vi.fn(), listSkills: vi.fn(), me: vi.fn() };
vi.mock("./client", async () => {
  const real = await vi.importActual<typeof import("./client")>("./client");
  return {
    ...real,
    health: () => calls.health(),
    listAssistants: () => calls.listAssistants(),
    listRuns: () => calls.listRuns(),
    listSkills: () => calls.listSkills(),
    me: () => calls.me(),
  };
});

import { ServerError } from "./client";
import { useServer } from "./server";
import { useEngine } from "../state";

const person = { principal: { id: "bob", name: "Bob", kind: "user", roles: ["builder"] }, tenant: "default", roles: ["builder"] };

describe("what the studio says about the server", () => {
  beforeEach(() => {
    calls.health.mockResolvedValue({ status: "ok", components: [] });
    calls.listAssistants.mockResolvedValue([{ assistant_id: "a", name: "Agent A" }]);
    calls.listRuns.mockResolvedValue([]);
    calls.listSkills.mockResolvedValue([]);
    calls.me.mockResolvedValue(person);
  });
  afterEach(() => { useEngine.getState().setTruth("server"); useEngine.getState().setConn({ state: "live", message: "" }); });

  it("is down only when the wire fails, and up again on the next answer", async () => {
    calls.health.mockRejectedValueOnce(new ServerError(0, "/health", "Failed to fetch"));
    await useServer.getState().refresh();
    expect(useServer.getState().reach).toBe("down");
    expect(useServer.getState().problem).toBe("Failed to fetch");
    expect(useEngine.getState().truth).toBe("demo");

    await useServer.getState().refresh();
    expect(useServer.getState().reach).toBe("up");
    expect(useServer.getState().problem).toBeNull();
    expect(useEngine.getState().truth).toBe("server");
    expect(useEngine.getState().conn.state).toBe("live");
    expect(useServer.getState().assistants.map((a) => a.name)).toEqual(["Agent A"]);
  });

  it("a server that stops answering is said to be away, and what was on screen stays as the last snapshot", async () => {
    await useServer.getState().refresh();
    expect(useServer.getState().reach).toBe("up");
    calls.health.mockRejectedValueOnce(new ServerError(0, "/health", "the server did not answer"));
    await useServer.getState().probe();
    const s = useServer.getState();
    expect(s.reach).toBe("down");
    expect(s.problem).toBe("the server did not answer");
    expect(s.me?.principal.name).toBe("Bob");
    expect(s.assistants.map((a) => a.name)).toEqual(["Agent A"]);
    expect(useEngine.getState().truth).toBe("demo");
    // Up again on the next answer; a probe while down is not a second outage.
    await useServer.getState().probe();
    expect(useServer.getState().reach).toBe("down");
    await useServer.getState().refresh();
    expect(useServer.getState().reach).toBe("up");
    expect(useEngine.getState().truth).toBe("server");
  });

  it("a call the server refused is not an outage: the server is up, the call is named, and the rest is shown", async () => {
    calls.listRuns.mockRejectedValueOnce(new ServerError(403, "/runs", "runs:read is not in this key's scopes"));
    await useServer.getState().refresh();
    const s = useServer.getState();
    expect(s.reach).toBe("up");
    expect(s.problem).toBe("/runs → runs:read is not in this key's scopes");
    expect(s.assistants.map((a) => a.name)).toEqual(["Agent A"]);
    expect(s.me?.principal.id).toBe("bob");
    expect(useEngine.getState().truth).toBe("server");
    expect(useEngine.getState().conn.state).toBe("partial");
    expect(useEngine.getState().conn.message).toContain("/runs failed: runs:read is not in this key's scopes");
  });

  it("nobody signed in is neither", async () => {
    calls.me.mockRejectedValueOnce(new ServerError(401, "/me", "your session ended — sign in again"));
    await useServer.getState().refresh();
    expect(useServer.getState().reach).toBe("up");
    expect(useServer.getState().refused).toBe(true);
    expect(useEngine.getState().truth).toBe("server");
  });
});
