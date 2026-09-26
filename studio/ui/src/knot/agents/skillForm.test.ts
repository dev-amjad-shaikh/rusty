import { describe, expect, it } from "vitest";
import { writeSkillBody as skillBody, skillFormOf, type SkillForm } from "./skillForm";

const form: SkillForm = {
  purpose: "Posts one short notice to the Echo Board for whoever asked.",
  tools: [{ name: "echo-board.post-notice", when: "posts the notice" }, { name: "agents.list", when: "" }],
  steps: ["Take the words the person wants posted.", "Post them once with the tool."],
  never: ["Never post something the person did not ask for."],
  done: "Say what was posted.",
};

describe("skill form", () => {
  it("writes the body the platform links tools in", () => {
    const body = skillBody(form);
    expect(body).toContain("- `echo-board.post-notice` — posts the notice");
    expect(body).toContain("- `agents.list`\n");
    expect(body).toContain("1. Take the words");
    expect(body).toContain("## When done\nSay what was posted.");
  });

  it("reads back what it wrote", () => {
    expect(skillFormOf(skillBody(form))).toEqual(form);
  });

  it("drops blank steps and rules instead of numbering them", () => {
    const body = skillBody({ ...form, steps: ["", "Only step.", "  "], never: [""] });
    expect(body).toContain("## Steps\n1. Only step.");
    expect(body).not.toContain("## Never");
  });

  it("leaves a free document to the document view", () => {
    expect(skillFormOf("# Account briefing\n\nYou are a briefer.\n\n## Workflow\n\n### Step 1\nRead.")).toBeNull();
    expect(skillFormOf("Intro.\n\n## Steps\n1. One.\nA loose paragraph.")).toBeNull();
    expect(skillFormOf("Intro.\n\n## Tools you use\n- a tool without backticks")).toBeNull();
  });
});

describe("a new skill", () => {
  it("picks the agent's only tool, and none from many", async () => {
    const { emptySkillForm } = await import("./skillForm");
    expect(emptySkillForm(["echo-board.post-notice"]).tools).toEqual([{ name: "echo-board.post-notice", when: "" }]);
    expect(emptySkillForm(["a.one", "b.two", "c.three"]).tools).toEqual([]);
  });
});
