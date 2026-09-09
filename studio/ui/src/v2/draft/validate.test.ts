// The validation rule table from handoff 04, rule by rule. Every violation
// must anchor to a control path and to a spec file.

import { describe, expect, it } from "vitest";
import type { AgentDraft } from "./agent-draft.gen";
import { specFileFor, type DraftCatalogs } from "./catalogs";
import { blankDraft } from "./defaults";
import { isCronSpec, validateDraft, type Violation } from "./validate";

const CATALOGS: DraftCatalogs = {
  models: ["gpt-5", "claude-opus"],
  connectors: [
    {
      id: "slack",
      name: "Slack",
      tools: [
        { id: "slack.read", effect: "read" },
        { id: "slack.post", effect: "write" },
      ],
      events: ["message"],
    },
    {
      id: "crm",
      name: "CRM",
      tools: [{ id: "crm.lookup", effect: "read" }],
      events: ["deal_closed"],
    },
  ],
  skills: [{ id: "triage", description: "Triage inbound tickets" }],
  evalSuites: ["service-desk-regression"],
};

function validDraft(): AgentDraft {
  return {
    ...blankDraft("guided"),
    name: "Support",
    description: "Front-line support agent",
    model: "gpt-5",
    autonomy: "supervised",
    goal: "Resolve tickets end to end",
    measures: [{ name: "CSAT", source: "outcome", target: ">= 4.5", window: "7d", kind: "target" }],
    stable: "You are the front-line support agent.",
    connectors: ["slack"],
    secrets: { slack: "rusty:secret:vault:slack" },
    channelKind: "slack",
    channelTarget: "#support",
    cadence: "0 3 * * *",
    gateSuite: "service-desk-regression",
  };
}

function byPath(violations: Violation[], path: string): Violation[] {
  return violations.filter((v) => v.path === path);
}

describe("validateDraft — the 04 rule table", () => {
  it("a complete draft validates clean", () => {
    expect(validateDraft(validDraft(), CATALOGS)).toEqual([]);
  });

  it("a blank draft surfaces schema rules anchored to controls and spec files", () => {
    const violations = validateDraft(blankDraft("guided"), CATALOGS);
    const name = byPath(violations, "name")[0];
    expect(name).toMatchObject({ kind: "schema", rule: "identity.name.required", specFile: "agent.md" });
    expect(byPath(violations, "stable")[0]).toMatchObject({ specFile: "directive/stable.md" });
    expect(byPath(violations, "goal")[0]).toMatchObject({ specFile: "goal.md" });
    expect(byPath(violations, "measures")[0]).toMatchObject({
      kind: "coherence",
      rule: "goal.measures.min",
      specFile: "goal.md",
    });
    expect(byPath(violations, "gateSuite")[0]).toMatchObject({
      kind: "coherence",
      rule: "learning.gate.required",
      specFile: "learning.md",
    });
  });

  it("measures need a name and a target each", () => {
    const draft = validDraft();
    draft.measures = [{ name: "", source: "outcome", target: "", window: "7d", kind: "target" }];
    const violations = validateDraft(draft, CATALOGS);
    expect(byPath(violations, "measures[0].name")[0]?.rule).toBe("goal.measures.name");
    expect(byPath(violations, "measures[0].target")[0]?.rule).toBe("goal.measures.target");
  });

  it("read_only autonomy conflicts land on autonomy and on each offending tool", () => {
    const draft = { ...validDraft(), autonomy: "read_only" as const };
    const violations = validateDraft(draft, CATALOGS);
    expect(byPath(violations, "autonomy")[0]).toMatchObject({
      kind: "coherence",
      rule: "autonomy.read_only.conflict",
      specFile: "agent.md",
    });
    expect(byPath(violations, "toolset.slack.post")[0]).toMatchObject({
      kind: "coherence",
      specFile: "toolsets.md",
    });
    // The read-effect tool is not flagged.
    expect(byPath(violations, "toolset.slack.read")).toEqual([]);
  });

  it("every mounted connector needs a SecretRef name", () => {
    const draft = { ...validDraft(), secrets: {} };
    const violations = validateDraft(draft, CATALOGS);
    expect(byPath(violations, "secrets.slack")[0]).toMatchObject({
      kind: "slot",
      rule: "connectors.secret.required",
      specFile: "toolsets.md",
    });
  });

  it("mounted connectors require a channel or a trigger; a trigger satisfies it", () => {
    const draft = { ...validDraft(), channelKind: "" as const, channelTarget: "" };
    expect(byPath(validateDraft(draft, CATALOGS), "channelKind")[0]?.rule).toBe("channel.required");

    const withTrigger: AgentDraft = {
      ...draft,
      triggers: [{ kind: "cron", spec: "*/15 * * * *", prompt: "Sweep the queue" }],
    };
    expect(byPath(validateDraft(withTrigger, CATALOGS), "channelKind")).toEqual([]);
  });

  it("a channel kind without a target is a slot violation", () => {
    const draft = { ...validDraft(), channelTarget: "" };
    expect(byPath(validateDraft(draft, CATALOGS), "channelTarget")[0]?.rule).toBe(
      "channel.target.required",
    );
  });

  it("memory blocks need descriptions and unique labels", () => {
    const draft = validDraft();
    draft.memory = [
      { label: "prefs", description: "", limit: 400, scope: "user" },
      { label: "prefs", description: "User preferences", limit: 400, scope: "user" },
    ];
    const violations = validateDraft(draft, CATALOGS);
    expect(byPath(violations, "memory[0].description")[0]).toMatchObject({
      kind: "coherence",
      specFile: "memory.md",
    });
    expect(byPath(violations, "memory[1].label")[0]?.rule).toBe("memory.label.unique");
    expect(byPath(violations, "memory[0].label")).toEqual([]);
  });

  it("tool rules flag dangling tools and empty text", () => {
    const draft = validDraft();
    draft.rules = [
      { tool: "ghost.tool", rule: "Never run this" },
      { tool: "slack.post", rule: "  " },
      { tool: "", rule: "Applies to all tools" },
    ];
    const violations = validateDraft(draft, CATALOGS);
    expect(byPath(violations, "rules[0].tool")[0]).toMatchObject({
      kind: "coherence",
      rule: "tool_rules.dangling",
      specFile: "rules.md",
    });
    expect(byPath(violations, "rules[1].rule")[0]?.rule).toBe("tool_rules.text.required");
    expect(violations.filter((v) => v.path.startsWith("rules[2]"))).toEqual([]);
  });

  it("trigger specs: cron takes 5 fields; events must come from mounted connectors", () => {
    const draft = validDraft();
    draft.triggers = [
      { kind: "cron", spec: "* * * *", prompt: "Too few fields" },
      { kind: "event", spec: "", prompt: "No event chosen" },
      { kind: "event", spec: "crm.deal_closed", prompt: "CRM is not mounted" },
      { kind: "event", spec: "slack.message", prompt: "" },
      { kind: "event", spec: "slack.message", prompt: "Triage it" },
    ];
    const violations = validateDraft(draft, CATALOGS);
    expect(byPath(violations, "triggers[0].spec")[0]?.rule).toBe("triggers.cron.fields");
    expect(byPath(violations, "triggers[1].spec")[0]?.rule).toBe("triggers.event.required");
    expect(byPath(violations, "triggers[2].spec")[0]).toMatchObject({
      kind: "coherence",
      rule: "triggers.event.mounted",
    });
    expect(byPath(violations, "triggers[3].prompt")[0]?.rule).toBe("triggers.prompt.required");
    expect(violations.filter((v) => v.path.startsWith("triggers[4]"))).toEqual([]);
  });

  it("the promotion gate must name a known eval suite when a catalog is present", () => {
    const draft = { ...validDraft(), gateSuite: "unknown-suite" };
    expect(byPath(validateDraft(draft, CATALOGS), "gateSuite")[0]?.rule).toBe("learning.gate.known");
  });
});

describe("isCronSpec", () => {
  it("accepts 5-field cron and rejects the rest", () => {
    expect(isCronSpec("*/15 * * * *")).toBe(true);
    expect(isCronSpec("0 3 1 * 1-5")).toBe(true);
    expect(isCronSpec("* * * *")).toBe(false);
    expect(isCronSpec("every hour")).toBe(false);
    expect(isCronSpec("")).toBe(false);
  });
});

describe("specFileFor", () => {
  it("anchors paths to the 04 spec-file layout via the schema", () => {
    expect(specFileFor("name")).toBe("agent.md");
    expect(specFileFor("measures[2].target")).toBe("goal.md");
    expect(specFileFor("stable")).toBe("directive/stable.md");
    expect(specFileFor("secrets.slack")).toBe("toolsets.md");
    expect(specFileFor("toolset.slack.post")).toBe("toolsets.md");
    expect(specFileFor("triggers[0].spec")).toBe("triggers.md");
    expect(specFileFor("gateSuite")).toBe("learning.md");
  });
});
