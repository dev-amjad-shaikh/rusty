// Assembled-prompt projection (handoff 04): tier contents and the byte count
// the Review card shows.

import { describe, expect, it } from "vitest";
import { blankDraft } from "../draft/defaults";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import { assemblePrompt } from "./prompt";

const NOW = "2026-09-09T10:00:00.000Z";

const catalogs: DraftCatalogs = {
  ...EMPTY_CATALOGS,
  skills: [{ id: "web-research", description: "Search and read the web" }],
};

function draft() {
  return {
    ...blankDraft("compose"),
    name: "Scout",
    goal: "Find leads",
    measures: [{ name: "Qualified leads", source: "outcome" as const, target: "≥ 10", window: "weekly", kind: "target" as const }],
    stable: "You are Scout.",
    context: "The workspace is Acme's CRM.",
    skills: ["web-research"],
    memory: [{ label: "accounts", description: "Known accounts", limit: 4000, scope: "agent" as const }],
    rules: [{ tool: "", rule: "Never email a prospect twice" }],
    channelKind: "slack" as const,
    channelTarget: "#sales",
  };
}

describe("assemblePrompt", () => {
  it("builds the stable tier: goal, measures, stable text, tool rules", () => {
    const prompt = assemblePrompt(draft(), catalogs, NOW);
    expect(prompt.stable).toContain("## Goal\nFind leads");
    expect(prompt.stable).toContain("You are measured on: Qualified leads ≥ 10 (weekly)");
    expect(prompt.stable).toContain("You are Scout.");
    expect(prompt.stable).toContain("## Tool rules\n- *: Never email a prospect twice");
  });

  it("builds the volatile tier: skills, memory, and the Now line", () => {
    const prompt = assemblePrompt(draft(), catalogs, NOW);
    expect(prompt.volatile).toContain("## Skills\n- web-research: Search and read the web");
    expect(prompt.volatile).toContain("## Memory\n- accounts (4000): Known accounts");
    expect(prompt.volatile).toContain(`## Now\n${NOW} · slack · #sales`);
  });

  it("passes the context tier through verbatim", () => {
    expect(assemblePrompt(draft(), catalogs, NOW).context).toBe("The workspace is Acme's CRM.");
  });

  it("counts UTF-8 bytes of the joined text", () => {
    const prompt = assemblePrompt(draft(), catalogs, NOW);
    expect(prompt.bytes).toBe(new TextEncoder().encode(prompt.text).byteLength);
    expect(prompt.text).toBe([prompt.stable, prompt.context, prompt.volatile].join("\n\n"));
  });

  it("joins multi-byte characters by byte, not by code unit", () => {
    const d = { ...blankDraft("compose"), goal: "Résumé — café", stable: "x" };
    const prompt = assemblePrompt(d, EMPTY_CATALOGS, NOW);
    expect(prompt.bytes).toBeGreaterThan(prompt.text.length);
  });

  it("shows an unbound channel in the Now line", () => {
    const d = { ...blankDraft("compose"), goal: "g", stable: "s" };
    expect(assemblePrompt(d, EMPTY_CATALOGS, NOW).volatile).toContain("unbound");
  });
});
