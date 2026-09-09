// Slot cards (R-A4): derived from the template document; unfilled slots
// surface as slot-kind violations merged into the live report without
// duplicating paths the shared rule set already flags.

import { describe, expect, it } from "vitest";
import { blankDraft } from "../draft/defaults";
import { validateDraft } from "../draft/validate";
import type { AgentDraft } from "../draft/agent-draft.gen";
import { deriveSlots, slotFilled, slotRequired, slotViolations, withSlotViolations } from "./slots";
import { fixtureTemplate } from "./fixtures";

function document(): AgentDraft {
  return fixtureTemplate().document;
}

describe("deriveSlots", () => {
  it("derives the handoff slot order, one credential per mounted connector", () => {
    const doc = {
      ...document(),
      connectors: ["gmail", "slack"],
    };
    expect(deriveSlots(doc).map((slot) => slot.path)).toEqual([
      "name",
      "goal",
      "channelKind",
      "channelTarget",
      "secrets.gmail",
      "secrets.slack",
      "model",
    ]);
  });

  it("marks credential slots as probed and mono", () => {
    const credential = deriveSlots(document()).find((slot) => slot.path === "secrets.gmail");
    expect(credential?.probe).toBe(true);
    expect(credential?.mono).toBe(true);
  });
});

describe("slotFilled / slotRequired", () => {
  it("reads scalar and credential values from the draft", () => {
    const doc = document();
    const slots = deriveSlots(doc);
    const byPath = new Map(slots.map((slot) => [slot.path, slot]));
    const draft = { ...blankDraft("guided"), name: "Bot", secrets: { gmail: "rusty:secret:kv:gmail" } };
    expect(slotFilled(draft, byPath.get("name")!)).toBe(true);
    expect(slotFilled(draft, byPath.get("goal")!)).toBe(false);
    expect(slotFilled(draft, byPath.get("secrets.gmail")!)).toBe(true);
    expect(slotFilled(draft, byPath.get("model")!)).toBe(false);
  });

  it("channel target is not applicable until a kind is chosen, then required", () => {
    const slot = deriveSlots(document()).find((s) => s.path === "channelTarget")!;
    const unbound = blankDraft("guided");
    expect(slotFilled(unbound, slot)).toBe(true);
    expect(slotRequired(unbound, slot)).toBe(false);
    const bound = { ...unbound, channelKind: "slack" as const };
    expect(slotFilled(bound, slot)).toBe(false);
    expect(slotRequired(bound, slot)).toBe(true);
  });

  it("channel kind is owed only when connectors are mounted without a trigger", () => {
    const slot = deriveSlots(document()).find((s) => s.path === "channelKind")!;
    expect(slotRequired(blankDraft("guided"), slot)).toBe(false);
    const mounted = { ...blankDraft("guided"), connectors: ["gmail"] };
    expect(slotRequired(mounted, slot)).toBe(true);
    const triggered = { ...mounted, triggers: [{ kind: "cron" as const, spec: "0 9 * * *", prompt: "go" }] };
    expect(slotRequired(triggered, slot)).toBe(false);
  });
});

describe("slotViolations", () => {
  it("flags every required-but-unfilled slot as a slot-kind violation", () => {
    const doc = document();
    const slots = deriveSlots(doc);
    // Template goal arrives filled; name, channel, credential, and model do not.
    const draft = { ...blankDraft("guided"), goal: doc.goal, connectors: doc.connectors };
    const violations = slotViolations(draft, slots);
    expect(violations.map((v) => v.path)).toEqual([
      "name",
      "channelKind",
      "secrets.gmail",
      "model",
    ]);
    expect(violations.every((v) => v.kind === "slot")).toBe(true);
    expect(violations.every((v) => v.rule === "guided.slot.unfilled")).toBe(true);
    expect(violations.find((v) => v.path === "name")?.specFile).toBe("agent.md");
  });
});

describe("withSlotViolations", () => {
  it("merges into the live report without duplicating covered paths", () => {
    const doc = document();
    const slots = deriveSlots(doc);
    const draft = { ...blankDraft("guided"), connectors: doc.connectors };
    const base = validateDraft(draft);
    const merged = withSlotViolations(base, draft, slots);
    // name, secrets.gmail, and channelKind already carry violations from the
    // shared rule set; only model is added by the slot layer.
    const paths = merged.map((v) => v.path);
    expect(paths.filter((p) => p === "name")).toHaveLength(1);
    expect(paths.filter((p) => p === "secrets.gmail")).toHaveLength(1);
    expect(paths.filter((p) => p === "model")).toHaveLength(1);
    expect(merged.find((v) => v.path === "model")?.rule).toBe("guided.slot.unfilled");
  });
});
