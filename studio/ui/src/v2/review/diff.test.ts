// Field-level diff vs published head (R-A8): section grouping, signs, and
// GOVERNANCE flags on autonomy / approval wrappers / triggers.

import { describe, expect, it } from "vitest";
import { blankDraft } from "../draft/defaults";
import type { AgentDraft } from "../draft/agent-draft.gen";
import { diffDraft, nextVersion, versionLabel } from "./diff";

function basedDraft(): AgentDraft {
  const base = { ...blankDraft("compose"), name: "Scout", model: "anthropic:claude", version: 3 };
  return { ...blankDraft("compose"), name: "Scout", model: "anthropic:claude", base };
}

describe("diffDraft", () => {
  it("treats a draft without a base as new: filled fields are + rows", () => {
    const draft = { ...blankDraft("guided"), name: "Scout", goal: "Find leads", autonomy: "supervised" as const };
    const sections = diffDraft(draft);
    const identity = sections.find((s) => s.id === "identity");
    expect(identity?.rows).toContainEqual({ sign: "+", field: "name", value: "Scout" });
    // empty fields do not produce rows on a new draft
    expect(identity?.rows.some((r) => r.field === "description")).toBe(false);
    // autonomy is governance-significant even on first publish
    expect(sections.find((s) => s.id === "autonomy")?.governance).toBe(true);
  });

  it("omits unchanged fields and sections entirely", () => {
    const draft = basedDraft();
    expect(diffDraft(draft)).toEqual([]);
  });

  it("marks changed scalars with ~ and the struck-through old value", () => {
    const draft = { ...basedDraft(), model: "openai:gpt" };
    const identity = diffDraft(draft).find((s) => s.id === "identity");
    expect(identity?.rows).toEqual([
      { sign: "~", field: "model", value: "openai:gpt", oldValue: "anthropic:claude" },
    ]);
  });

  it("marks cleared scalars as removed", () => {
    const draft = basedDraft();
    draft.base!.goal = "Find leads";
    const goal = diffDraft(draft).find((s) => s.id === "goal");
    expect(goal?.rows).toContainEqual({ sign: "-", field: "goal", value: "Find leads" });
  });

  it("flags GOVERNANCE when approval wrappers change (Toolsets)", () => {
    const draft = { ...basedDraft(), wrapped: ["slack.post_message"] };
    const toolsets = diffDraft(draft).find((s) => s.id === "toolsets");
    expect(toolsets?.governance).toBe(true);
    expect(toolsets?.rows).toContainEqual({
      sign: "+",
      field: "wrapped",
      value: "slack.post_message → approval_required(org_admins)",
    });
  });

  it("flags GOVERNANCE on trigger changes and autonomy changes", () => {
    const draft = {
      ...basedDraft(),
      autonomy: "full" as const,
      triggers: [{ kind: "cron" as const, spec: "0 9 * * *", prompt: "Sweep" }],
    };
    const sections = diffDraft(draft);
    expect(sections.find((s) => s.id === "triggers")?.governance).toBe(true);
    expect(sections.find((s) => s.id === "autonomy")?.governance).toBe(true);
  });

  it("does not flag governance on non-governance sections", () => {
    const draft = { ...basedDraft(), skills: ["web-research"] };
    const skills = diffDraft(draft).find((s) => s.id === "skills");
    expect(skills?.governance).toBe(false);
  });

  it("diffs secret refs by name only", () => {
    const draft = { ...basedDraft(), connectors: ["slack"], secrets: { slack: "rusty:secret:core:slack-bot" } };
    const toolsets = diffDraft(draft).find((s) => s.id === "toolsets");
    expect(toolsets?.rows).toContainEqual({
      sign: "+",
      field: "secrets.slack",
      value: "rusty:secret:core:slack-bot",
    });
  });

  it("keeps sections in handoff order", () => {
    const draft = {
      ...basedDraft(),
      name: "Renamed",
      autonomy: "full" as const,
      triggers: [{ kind: "cron" as const, spec: "0 9 * * *", prompt: "Sweep" }],
    };
    const ids = diffDraft(draft).map((s) => s.id);
    expect(ids).toEqual(["identity", "triggers", "autonomy"]);
  });
});

describe("version labels", () => {
  it("labels a first publish as new · v1", () => {
    expect(versionLabel(blankDraft("compose"))).toBe("new · v1");
    expect(nextVersion(blankDraft("compose"))).toBe(1);
  });

  it("labels a re-publish as v3 → v4", () => {
    const draft = basedDraft();
    expect(versionLabel(draft)).toBe("v3 → v4");
    expect(nextVersion(draft)).toBe(4);
  });
});
