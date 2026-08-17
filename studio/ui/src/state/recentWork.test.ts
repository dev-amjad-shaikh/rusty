import { beforeEach, describe, expect, it, vi } from "vitest";
import { readRecentWork, rememberRecentWork } from "./recentWork";

beforeEach(() => {
  sessionStorage.clear();
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-08-11T00:00:00Z"));
});

describe("recent Work identities", () => {
  it("remembers bounded run identities without storing evidence", () => {
    rememberRecentWork({ threadId: "thread-alpha", runId: "run-alpha" });
    rememberRecentWork({ threadId: "thread-beta", runId: "run-beta" });

    expect(readRecentWork().map((item) => item.runId)).toEqual(["run-beta", "run-alpha"]);
    const stored = sessionStorage.getItem("rusty-studio:recent-work:v2") ?? "";
    expect(stored).not.toContain("prompt");
  });

  it("keeps only twelve safe durable identities", () => {
    for (let index = 0; index < 14; index += 1) rememberRecentWork({ threadId: `thread-${index}`, runId: `run-${index}` });
    rememberRecentWork({ threadId: "thread-bad\t", runId: "run-bad" });
    const recent = readRecentWork();
    expect(recent).toHaveLength(12);
    expect(recent[0].runId).toBe("run-13");
    expect(recent.at(-1)?.runId).toBe("run-2");
    expect(recent.some((item) => item.runId === "run-bad")).toBe(false);
  });
});
