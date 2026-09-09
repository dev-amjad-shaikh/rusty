// Eval gate state machine: run/re-run lifecycle and staleness when the draft
// content moves under a completed run.

import { describe, expect, it } from "vitest";
import { blankDraft } from "../draft/defaults";
import {
  INITIAL_GATE,
  contentHash,
  gateCaseResult,
  gateCompleted,
  gateStarted,
  gateStatus,
} from "./gate";

describe("gate state", () => {
  it("starts as not_run", () => {
    expect(gateStatus(INITIAL_GATE, contentHash(blankDraft("compose")))).toBe("not_run");
  });

  it("is running while a run id is in flight", () => {
    const state = gateStarted(INITIAL_GATE, "run-1");
    expect(gateStatus(state, contentHash(blankDraft("compose")))).toBe("running");
  });

  it("completes to passing or failing", () => {
    const draft = blankDraft("compose");
    const hash = contentHash(draft);
    const running = gateStarted(INITIAL_GATE, "run-1");
    expect(gateStatus(gateCompleted(running, hash, true, []), hash)).toBe("passing");
    expect(gateStatus(gateCompleted(running, hash, false, []), hash)).toBe("failing");
  });

  it("goes stale when the draft content changes after a completed run", () => {
    const draft = blankDraft("compose");
    const passed = gateCompleted(gateStarted(INITIAL_GATE, "run-1"), contentHash(draft), true, []);
    const moved = { ...draft, goal: "A different goal" };
    expect(gateStatus(passed, contentHash(moved))).toBe("stale");
  });

  it("streams case results without disturbing the run in flight", () => {
    let state = gateStarted(INITIAL_GATE, "run-1");
    state = gateCaseResult(state, { name: "case-a", pass: true, score: 0.9 });
    state = gateCaseResult(state, { name: "case-b", pass: false, score: 0.1 });
    expect(state.cases.map((c) => c.name)).toEqual(["case-a", "case-b"]);
    expect(gateStatus(state, "anything")).toBe("running");
  });

  it("replaces a re-reported case in place", () => {
    let state = gateStarted(INITIAL_GATE, "run-1");
    state = gateCaseResult(state, { name: "case-a", pass: false, score: 0.2 });
    state = gateCaseResult(state, { name: "case-a", pass: true, score: 0.8 });
    expect(state.cases).toEqual([{ name: "case-a", pass: true, score: 0.8 }]);
  });
});

describe("contentHash", () => {
  it("is stable for identical content and key order", () => {
    const a = { ...blankDraft("compose"), name: "Scout", goal: "g" };
    const b = { ...blankDraft("compose"), goal: "g", name: "Scout" };
    expect(contentHash(a)).toBe(contentHash(b));
  });

  it("changes when nested content changes", () => {
    const a = { ...blankDraft("compose"), measures: [{ name: "m", source: "eval" as const, target: "≥ 1", window: "w", kind: "gate" as const }] };
    const b = { ...blankDraft("compose"), measures: [{ name: "m", source: "eval" as const, target: "≥ 2", window: "w", kind: "gate" as const }] };
    expect(contentHash(a)).not.toBe(contentHash(b));
  });
});
