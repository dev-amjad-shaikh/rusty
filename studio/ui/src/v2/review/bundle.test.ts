// The .rustyprint export bundle: spec-file layout per handoff 04 and the
// asserted no-secret-values scan.

import { describe, expect, it } from "vitest";
import { blankDraft } from "../draft/defaults";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import { buildExportBundle, scanForSecretValues } from "./bundle";

const catalogs: DraftCatalogs = {
  ...EMPTY_CATALOGS,
  connectors: [
    {
      id: "slack",
      name: "Slack",
      events: ["message"],
      tools: [
        { id: "slack.read", effect: "read" },
        { id: "slack.post_message", effect: "write" },
      ],
    },
  ],
};

function draft() {
  return {
    ...blankDraft("compose"),
    name: "Scout",
    description: "Finds leads",
    model: "anthropic:claude",
    autonomy: "supervised" as const,
    goal: "Find leads",
    measures: [{ name: "Leads", source: "outcome" as const, target: "≥ 10", window: "weekly", kind: "target" as const }],
    stable: "You are Scout.",
    context: "Acme CRM.",
    connectors: ["slack"],
    secrets: { slack: "rusty:secret:core:slack-bot" },
    wrapped: ["slack.post_message"],
    rules: [{ tool: "", rule: "Never double-email" }],
    triggers: [{ kind: "cron" as const, spec: "0 9 * * *", prompt: "Sweep" }],
    memory: [{ label: "accounts", description: "Known accounts", limit: 4000, scope: "agent" as const }],
    channelKind: "slack" as const,
    channelTarget: "#sales",
    cadence: "0 3 * * *",
    gateSuite: "scout-gate",
    reviewFork: true,
  };
}

describe("buildExportBundle", () => {
  it("produces the handoff 04 spec-file layout plus the assembled prompt", () => {
    const bundle = buildExportBundle(draft(), catalogs);
    expect(bundle.files.map((f) => f.path)).toEqual([
      "agent.md",
      "goal.md",
      "directive/stable.md",
      "directive/context.md",
      "rules.md",
      "triggers.md",
      "toolsets.md",
      "memory.md",
      "learning.md",
      "assembled-prompt.txt",
    ]);
  });

  it("writes agent.md frontmatter as the authoritative identity block", () => {
    const agent = buildExportBundle(draft(), catalogs).files[0].content;
    expect(agent).toContain("name: Scout");
    expect(agent).toContain("autonomy: supervised");
    expect(agent).toContain("connectors: [slack]");
    expect(agent).toContain("gate: scout-gate");
    expect(agent).toContain("# Scout");
  });

  it("marks approval-wrapped tools in toolsets.md", () => {
    const toolsets = buildExportBundle(draft(), catalogs).files.find((f) => f.path === "toolsets.md");
    expect(toolsets?.content).toContain("secret: rusty:secret:core:slack-bot");
    expect(toolsets?.content).toContain("- slack.post_message  # write  → approval_required(org_admins)");
  });

  it("passes the scan when secrets are SecretRef names", () => {
    expect(buildExportBundle(draft(), catalogs).scan.ok).toBe(true);
  });

  it("writes <unbound> for a connector without a secret ref and still passes", () => {
    const d = { ...draft(), secrets: {} };
    const bundle = buildExportBundle(d, catalogs);
    expect(bundle.files.find((f) => f.path === "toolsets.md")?.content).toContain("secret: <unbound>");
    expect(bundle.scan.ok).toBe(true);
  });
});

describe("scanForSecretValues", () => {
  it("fails the bundle when a secret line carries a raw value", () => {
    const d = { ...draft(), secrets: { slack: "xoxb-1234-raw-token" } };
    const bundle = buildExportBundle(d, catalogs);
    expect(bundle.scan.ok).toBe(false);
    expect(bundle.scan.findings[0]).toMatchObject({ path: "toolsets.md", line: 2 });
  });

  it("fails on embedded private key material anywhere in the bundle", () => {
    const d = { ...draft(), stable: "-----BEGIN PRIVATE KEY-----\nabc" };
    const bundle = buildExportBundle(d, catalogs);
    expect(bundle.scan.ok).toBe(false);
    expect(bundle.scan.findings.some((f) => f.path === "directive/stable.md")).toBe(true);
  });

  it("accepts ref-shaped names and the unbound marker", () => {
    const files = [{ path: "toolsets.md", content: "secret: rusty:secret:core:key\nsecret: <unbound>" }];
    expect(scanForSecretValues(files)).toEqual([]);
  });
});
