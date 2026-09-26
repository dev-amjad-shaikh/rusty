import { describe, expect, it } from "vitest";
import type { Run } from "../../engine/net/client";
import { liveRun, measure } from "./goal";

const run = (over: Partial<Run>): Run => ({ run_id: Math.random().toString(36).slice(2), thread_id: "t", graph: "g", status: "success", created_at: new Date().toISOString(), assistant_id: "desk", verification: { verdict: "verified" } as Run["verification"], ...over });

describe("the goal is measured on live runs", () => {
  it("leaves out evaluations and rehearsals in a world", () => {
    expect(liveRun(run({}))).toBe(true);
    expect(liveRun(run({ metadata: { channel: "evaluation" } }))).toBe(false);
    expect(liveRun(run({ worlds: [{ world_id: "w1", name: "facilities-twin" }] }))).toBe(false);
  });
  it("a suite that passes is not a goal met", () => {
    const runs = [
      run({ metadata: { channel: "evaluation" } }),
      run({ metadata: { channel: "evaluation" } }),
      run({ worlds: [{ world_id: "w1", name: "twin" }] }),
    ];
    const m = measure({ objective: "answer every report", metric: "Outcome verified", target: 70 }, runs, "desk");
    expect(m.sample).toBe(0);
    expect(m.current).toBeNull();
    const live = measure({ objective: "answer every report", metric: "Outcome verified", target: 70 }, [...runs, run({}), run({ verification: { verdict: "unverified" } as Run["verification"] })], "desk");
    expect(live.sample).toBe(2);
    expect(live.current).toBe(50);
  });
});
