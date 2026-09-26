import { describe, expect, it } from "vitest";
import { skillsForConnector, skillsForTools } from "./skillMatch";
import type { ServerSkill } from "../net/client";

const s = (name: string, tools?: string[]): ServerSkill => ({ name, description: name, revision: 1, content_hash: name, ...(tools ? { allowed_tools: tools } : {}) });
const library = [s("servicenow-find-the-table", ["servicenow.list-records"]), s("salesforce-soql-first", ["salesforce.soql-query"]), s("report-with-real-numbers"), s("never-file-twice", ["servicenow.list-records", "servicenow.create-incident"])];

describe("skill matching", () => {
  it("names the skills that follow a connector, by its tool prefix, in order", () => {
    expect(skillsForConnector(library, "servicenow").map((x) => x.name)).toEqual(["never-file-twice", "servicenow-find-the-table"]);
    expect(skillsForConnector(library, "slack")).toEqual([]);
  });
  it("names the skills an agent's tools make relevant; a tool-less skill follows nothing", () => {
    expect(skillsForTools(library, ["salesforce.soql-query", "echo"]).map((x) => x.name)).toEqual(["salesforce-soql-first"]);
    expect(skillsForTools(library, [])).toEqual([]);
  });
});
