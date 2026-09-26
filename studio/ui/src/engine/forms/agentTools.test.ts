// The tool picker reads the server's catalog and nothing else. These tests
// hold the two facts a builder relies on: what is shown is grouped by where
// it comes from, and what is picked is exactly what the server will run.

import { describe, expect, it } from "vitest";
import { connectorLabel, groupTools, toolSelection } from "./agentTools";

const CATALOG = [
  { name: "servicenow.list-records", description: "List records", effect: "read_only" },
  { name: "calculator", description: "Arithmetic", effect: "pure" },
  { name: "servicenow.check-connection", description: "Verify", effect: "read_only" },
  { name: "salesforce.soql-query", description: "SOQL", effect: "read_only" },
  { name: "servicenow.get-record", description: "One record", effect: "read_only" },
  { name: "servicenow@1a1b32af.list-records", description: "List records", effect: "read_only" },
  { name: "read_document", description: "Read a file", effect: "read_only" },
];

describe("the tools an agent may call", () => {
  it("are grouped by where they come from — built-ins first, then one group per connection", () => {
    const groups = groupTools(CATALOG);
    expect(groups.map((g) => g.label)).toEqual(["Built in", "Salesforce", "Servicenow", "Servicenow · 1a1b32af"]);
    expect(groups[0].tools.map((t) => t.name)).toEqual(["calculator", "read_document"]);
    expect(groups[2].tools.map((t) => t.name)).toEqual(["servicenow.get-record", "servicenow.list-records"]);
  });

  it("keeps the check operation out of the picker — it is a gate, not an action", () => {
    const names = groupTools(CATALOG).flatMap((g) => g.tools.map((t) => t.name));
    expect(names).not.toContain("servicenow.check-connection");
  });

  it("declares exactly the names the server offers, once each, in a stable order", () => {
    expect(toolSelection(["servicenow.list-records", "calculator", "servicenow.list-records"])).toEqual([
      { name: "calculator" },
      { name: "servicenow.list-records" },
    ]);
  });

  it("names a second connection to the same connector by its instance", () => {
    expect(connectorLabel("servicenow")).toBe("Servicenow");
    expect(connectorLabel("servicenow@1a1b32af")).toBe("Servicenow · 1a1b32af");
  });
});
