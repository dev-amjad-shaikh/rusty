import { afterEach, describe, expect, it, vi } from "vitest";
import { health, login, onSignedOut } from "./client";

const answer = (status: number, body: unknown) =>
  vi.fn(async () => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } }));

describe("the client on a 401", () => {
  afterEach(() => { vi.unstubAllGlobals(); onSignedOut(() => {}); });

  it("says the session ended, in words, and tells the shell", async () => {
    vi.stubGlobal("fetch", answer(401, { error: "Unauthorized" }));
    const told = vi.fn();
    onSignedOut(told);
    await expect(health()).rejects.toThrow(/your session ended — sign in again/);
    expect(told).toHaveBeenCalledTimes(1);
  });

  it("leaves a sign-in refusal in the server's own words", async () => {
    vi.stubGlobal("fetch", answer(401, { error: "wrong password" }));
    const told = vi.fn();
    onSignedOut(told);
    await expect(login("bob", "nope")).rejects.toThrow(/wrong password/);
    expect(told).not.toHaveBeenCalled();
  });
});
