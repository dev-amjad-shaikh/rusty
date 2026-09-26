import { describe, expect, it } from "vitest";
import { composeSkillBody, composeSkillMd, emptySkillDraft, parseSkillBody, shapeTools, skillNameProblem } from "./skillDraft";

describe("skillDraft", () => {
  it("names follow the server's kebab-case rule", () => {
    expect(skillNameProblem("triage-and-route")).toBeNull();
    expect(skillNameProblem("")).toMatch(/needs a name/);
    expect(skillNameProblem("Bad-Name")).toMatch(/Lowercase/);
    expect(skillNameProblem("a--b")).toMatch(/Lowercase/);
    expect(skillNameProblem("x".repeat(65))).toMatch(/64/);
  });

  it("composes the front matter the server parses, omitting what is unset", () => {
    const md = composeSkillMd({ ...emptySkillDraft(), name: "ack-first", description: "Acknowledge  before\nanything else.", body: "# Ack\n\n1. Reply." });
    expect(md).toBe("---\nname: ack-first\ndescription: Acknowledge before anything else.\n---\n\n# Ack\n\n1. Reply.\n");
  });

  it("lists tools comma-separated and carries licence and gate", () => {
    const md = composeSkillMd({ name: "route", description: "Routes.", license: "MIT", evalGate: "route-gate", tools: ["servicenow.list-records", "route_item"], body: "Do it." });
    expect(md).toContain("license: MIT\nallowed-tools: servicenow.list-records, route_item\neval-gate: route-gate\n---");
  });
});

describe("a procedure with tool bindings", () => {
  const body = `## When to use

When a person reports a facilities problem.

## Tools you use

- \`facilities-desk.list-tickets\` — to see what is open
- \`memory.recall\` — what was filed before

## Workflow

1. Call \`facilities-desk.list-tickets\` first and look for an open ticket about the same room.
2. If one exists, answer with its number.
3. Otherwise call \`facilities-desk.create-ticket\` exactly once.

## Done looks like

- Every problem is matched or filed once.

## Notes

Anything else the author wrote stays.
`;

  it("parses the sections, binds each step to the tool it names, and keeps the rest verbatim", () => {
    const shape = parseSkillBody(body);
    expect(shape.when).toBe("When a person reports a facilities problem.");
    expect(shape.tools).toEqual([{ name: "facilities-desk.list-tickets", why: "to see what is open" }, { name: "memory.recall", why: "what was filed before" }]);
    expect(shape.steps.map((s) => s.tool)).toEqual(["facilities-desk.list-tickets", "", "facilities-desk.create-ticket"]);
    // A bound step's words are the author's; the "Call `tool`" the composer adds is not.
    expect(parseSkillBody("## Workflow\n\n1. Call `desk.file` — with the room\n2. Read `references/x.md` and `field=value`").steps).toEqual([{ tool: "desk.file", text: "with the room" }, { tool: "", text: "Read `references/x.md` and `field=value`" }]);
    expect(shape.done).toEqual(["Every problem is matched or filed once."]);
    expect(shape.rest).toBe("## Notes\n\nAnything else the author wrote stays.");
    // The tools it uses: what the steps bind first, then what was added.
    expect(shapeTools(shape).map((t) => t.name)).toEqual(["facilities-desk.list-tickets", "facilities-desk.create-ticket", "memory.recall"]);
  });

  it("composes a body the parser reads back the same, and names a bound tool in the step", () => {
    const shape = parseSkillBody(body);
    const again = parseSkillBody(composeSkillBody(shape));
    expect(again.steps).toEqual(shape.steps);
    expect(again.done).toEqual(shape.done);
    expect(again.rest).toBe(shape.rest);
    const composed = composeSkillBody({ when: "x", tools: [], steps: [{ tool: "desk.file", text: "with the room" }, { tool: "", text: "answer" }], done: ["filed once"], rest: "" });
    expect(composed).toContain("1. Call `desk.file` — with the room");
    expect(composed).toContain("2. answer");
    expect(composed).toContain("- `desk.file`");
  });
});
