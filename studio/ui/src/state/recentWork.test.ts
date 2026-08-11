import { beforeEach, describe, expect, it, vi } from "vitest";
import { durableConnectionScope, readRecentWork, rememberRecentWork } from "./recentWork";

const alpha = { epoch: 1, origin: "https://rusty.example/", apiKey: "alpha-secret", tenantFingerprint: "alpha" };
const beta = { epoch: 2, origin: "https://rusty.example", apiKey: "beta-secret", tenantFingerprint: "beta" };

beforeEach(() => {
    sessionStorage.clear();
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-08-11T00:00:00Z"));
});

describe("recent Work identities", () => {
  it("separates tenants without storing access keys or evidence", () => {
    const alphaScope = durableConnectionScope(alpha);
    const betaScope = durableConnectionScope(beta);
    rememberRecentWork(alphaScope, { threadId: "thread-alpha", runId: "run-alpha" });
    rememberRecentWork(betaScope, { threadId: "thread-beta", runId: "run-beta" });

    expect(readRecentWork(alphaScope).map((item) => item.runId)).toEqual(["run-alpha"]);
    expect(readRecentWork(betaScope).map((item) => item.runId)).toEqual(["run-beta"]);
    const stored = sessionStorage.getItem("rusty-studio:recent-work:v1") ?? "";
    expect(stored).not.toContain("alpha-secret");
    expect(stored).not.toContain("beta-secret");
    expect(stored).not.toContain("prompt");
  });

  it("keeps only twelve safe durable identities per connection", () => {
    const scope = durableConnectionScope(alpha);
    for (let index = 0; index < 14; index += 1) rememberRecentWork(scope, { threadId: `thread-${index}`, runId: `run-${index}` });
    rememberRecentWork(scope, { threadId: "thread-bad\t", runId: "run-bad" });
    const recent = readRecentWork(scope);
    expect(recent).toHaveLength(12);
    expect(recent[0].runId).toBe("run-13");
    expect(recent.at(-1)?.runId).toBe("run-2");
    expect(recent.some((item) => item.runId === "run-bad")).toBe(false);
  });
});
