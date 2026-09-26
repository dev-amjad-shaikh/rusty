import { describe, expect, it } from "vitest";
import { assembledPrompt, emptyDraft, validateDraft, type AgentDraft } from "./draft";

function validDraft(): AgentDraft {
  return {
    ...emptyDraft("guided"),
    name: "Deal Desk", description: "Answers pricing questions.", model: "kimi",
    autonomy: "read_only", goal: "Answer pricing questions accurately.",
    measures: [{ name: "Answer accuracy", source: "eval", target: ">= 95%", window: "7d", kind: "gate" }],
    stable: "You are the deal desk assistant.",
  };
}

describe("validateDraft — the one validation set", () => {
  it("flags an empty draft's required fields, anchored to path + kind", () => {
    const v = validateDraft(emptyDraft("guided"));
    const paths = v.map((x) => x.path);
    expect(paths).toContain("identity.name");
    expect(paths).toContain("directive.stable");
    expect(paths).toContain("goal.statement");
    expect(paths).toContain("goal.measures");
    expect(v.every((x) => ["schema", "coherence", "slot"].includes(x.kind))).toBe(true);
  });

  it("passes a complete read-only draft with no side-effecting tools", () => {
    expect(validateDraft(validDraft())).toEqual([]);
  });

  it("reports read_only conflicting with a mounted write tool, on autonomy and the tool", () => {
    const d = { ...validDraft(), connectors: ["stripe"], secrets: { stripe: "rusty:secret:vault:stripe" } };
    const v = validateDraft(d, { mountedTools: [{ id: "stripe.refunds.create", effect: "write" }] });
    expect(v.some((x) => x.path === "identity.autonomy")).toBe(true);
    expect(v.some((x) => x.path === "toolsets.stripe.refunds.create")).toBe(true);
  });

  it("requires a SecretRef for each mounted connector and a channel or trigger", () => {
    const d = { ...validDraft(), connectors: ["servicenow"], secrets: {} };
    const v = validateDraft(d);
    expect(v.some((x) => x.path === "connectors.servicenow.secret" && x.kind === "slot")).toBe(true);
    expect(v.some((x) => x.path === "channels[0]" && x.kind === "slot")).toBe(true);
  });

  it("assembles a read-only prompt projection with a byte count", () => {
    const p = assembledPrompt(validDraft(), [{ id: "cite", description: "cite sources" }]);
    expect(p.stable).toContain("## Goal");
    expect(p.stable).toContain("measured on");
    expect(p.volatile).toContain("## Skills");
    expect(p.bytes).toBeGreaterThan(0);
  });
});
