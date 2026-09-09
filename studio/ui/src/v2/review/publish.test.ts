// Publish preconditions (handoff 04) and scope gating: 0 violations ∧ gate
// Passing ∧ publish scope — anything less keeps the button disabled with the
// hint, and without the scope Publish is absent entirely (R-A2, R-X2).

import { describe, expect, it } from "vitest";
import { canPublish, publishReadiness } from "./publish";
import type { Violation } from "../draft/validate";

const violation: Violation = {
  path: "name",
  kind: "schema",
  rule: "identity.name.required",
  message: "Name is required.",
  specFile: "agent.md",
};

describe("canPublish", () => {
  it("honors the wildcard and the generic blueprint publish scope", () => {
    expect(canPublish(["*"], "agent-1")).toBe(true);
    expect(canPublish(["blueprints:*"], "agent-1")).toBe(true);
    expect(canPublish(["blueprints:publish"], "agent-1")).toBe(true);
  });

  it("honors the per-blueprint scope from the handoff", () => {
    expect(canPublish(["blueprints:agent-1:publish"], "agent-1")).toBe(true);
    expect(canPublish(["blueprints:agent-2:publish"], "agent-1")).toBe(false);
  });

  it("refuses read-only roles", () => {
    expect(canPublish(["blueprints:read"], "agent-1")).toBe(false);
    expect(canPublish([], undefined)).toBe(false);
  });
});

describe("publishReadiness", () => {
  it("is ready only with 0 violations, a passing gate, and the scope", () => {
    const readiness = publishReadiness({
      violations: [],
      gate: "passing",
      scopes: ["blueprints:publish"],
    });
    expect(readiness).toEqual({ ready: true, reasons: [] });
  });

  it("lists every unmet precondition as a hint", () => {
    const readiness = publishReadiness({
      violations: [violation, violation],
      gate: "not_run",
      scopes: [],
      agentId: "agent-1",
    });
    expect(readiness.ready).toBe(false);
    expect(readiness.reasons).toEqual([
      "2 open violations",
      "Eval gate is not Passing",
      "You do not hold the publish scope for this blueprint",
    ]);
  });

  it("asks for a re-run when the gate went stale", () => {
    const readiness = publishReadiness({
      violations: [],
      gate: "stale",
      scopes: ["blueprints:publish"],
    });
    expect(readiness.reasons).toEqual(["Eval gate is stale — re-run it"]);
  });
});
