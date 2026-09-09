// The drift gate for the AgentDraft generation pipeline: the committed
// TypeScript must equal a fresh render of the schema, so a schema edit
// without `npm run generate:draft` fails here.

import { describe, expect, it } from "vitest";
// @ts-expect-error -- plain .mjs generator, no declarations
import { renderDraftTypes } from "../../../scripts/agent-draft-schema.mjs";
import schema from "./agent-draft.schema.json";
import generated from "./agent-draft.gen.ts?raw";

describe("agent-draft schema generation", () => {
  it("the committed types match the schema exactly (no drift)", () => {
    expect(renderDraftTypes(schema)).toBe(generated);
  });

  it("generates the shared draft type and its item interfaces", () => {
    expect(generated).toContain("export interface AgentDraft {");
    expect(generated).toContain('source: "guided" | "compose" | "chat" | "import" | "improve" | "test";');
    expect(generated).toContain("export interface Measure {");
    expect(generated).toContain("export interface ToolRule {");
    expect(generated).toContain("export interface MemoryBlock {");
    expect(generated).toContain("export interface Trigger {");
    expect(generated).toContain("secrets: Record<string, string>;");
    expect(generated).toContain("base?: AgentDraft & { version: number };");
  });

  it("a schema change changes the render (the gate cannot go stale)", () => {
    const mutated = JSON.parse(JSON.stringify(schema));
    mutated.properties.nickname = { type: "string", "x-control": "text" };
    expect(renderDraftTypes(mutated)).not.toBe(generated);
    expect(renderDraftTypes(mutated)).toContain("nickname?: string;");
  });
});
