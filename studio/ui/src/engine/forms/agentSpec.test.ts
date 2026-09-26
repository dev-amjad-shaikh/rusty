import { describe, expect, it } from "vitest";
import { composeCharter, findAgentBlock, formatAgentSpec, parseAgentSpec, specProblems, cadenceOf, cadenceWords, hasSchedule, budgetOf, budgetWords, factsNotCarried, emptySpec, tablesNotNamed, namesATable } from "./agentSpec";

const REPLY = `Here is what I propose.

\`\`\`agent
name: Pokedex
purpose: Answers questions about any Pokémon from PokeAPI.
role: You are the Pokédex. You look every Pokémon up before you speak.
status: proposed
instructions:
- Look the Pokémon up with pokeapi.get-pokemon before answering.
- When the name is unsure, browse with pokeapi.list-pokemon.
tools:
- pokeapi.get-pokemon — one Pokémon by name
- \`pokeapi.list-pokemon\`: browse names
skills:
- resolution-plan-format
constraints:
- Never guess; if the lookup fails, say so.
output: Short answers, the API's facts verbatim where they matter.
done_when:
- The answer quotes the API, or says the Pokémon does not exist.
needs:
- Nothing.
\`\`\`

WHAT I BUILT: nothing yet`;

describe("agentSpec", () => {
  it("finds the last agent block and parses scalars, lists and tool notes", () => {
    const block = findAgentBlock(REPLY);
    expect(block).not.toBeNull();
    const spec = parseAgentSpec(block!);
    expect(spec.name).toBe("Pokedex");
    expect(spec.status).toBe("proposed");
    expect(spec.instructions).toHaveLength(2);
    expect(spec.tools).toEqual([
      { name: "pokeapi.get-pokemon", when: "one Pokémon by name" },
      { name: "pokeapi.list-pokemon", when: "browse names" },
    ]);
    expect(spec.skills).toEqual(["resolution-plan-format"]);
    expect(spec.output).toBe("Short answers, the API's facts verbatim where they matter.");
    expect(spec.done_when[0]).toMatch(/quotes the API/);
    expect(spec.needs).toEqual([]); // "Nothing." under needs means none
    expect(findAgentBlock("no block here")).toBeNull();
  });

  it("composes the charter the server stores, in one shape", () => {
    const spec = parseAgentSpec(findAgentBlock(REPLY)!);
    const charter = composeCharter(spec);
    expect(charter.startsWith("You are the Pokédex.")).toBe(true);
    expect(charter).toContain("What you must do:\n- Look the Pokémon up");
    expect(charter).toContain("Tools and when to use them:\n- pokeapi.get-pokemon — one Pokémon by name");
    expect(charter).toContain("What you must never do:\n- Never guess");
    expect(charter).toContain("Done when:\n- The answer quotes the API");
  });

  it("names what stops creation, including tools the platform lacks", () => {
    const spec = parseAgentSpec(findAgentBlock(REPLY)!);
    expect(specProblems(spec, new Set(["pokeapi.get-pokemon"]))).toEqual(["Not on the platform: pokeapi.list-pokemon."]);
    expect(specProblems(parseAgentSpec("status: proposed"), null)).toEqual(["It needs a name.", "Say what it is or what it is for.", "It needs at least one instruction."]);
  });

  it("round-trips through its text form", () => {
    const spec = parseAgentSpec(findAgentBlock(REPLY)!);
    expect(parseAgentSpec(formatAgentSpec(spec))).toEqual(spec);
  });
});

describe("a schedule in the spec", () => {
  it("parses the cadence and the standing message", () => {
    const spec = parseAgentSpec("name: Night Watch\nschedule: every morning\nstanding_message: Summarise the incidents opened overnight.\ninstructions:\n- Read the incidents.");
    expect(spec.schedule).toBe("every morning");
    expect(spec.standing_message).toBe("Summarise the incidents opened overnight.");
    expect(hasSchedule(spec)).toBe(true);
    expect(cadenceOf(spec.schedule)).toEqual({ cron_expr: "0 9 * * *" });
  });
  it("reads cadences the way people say them", () => {
    expect(cadenceOf("every 30 minutes")).toEqual({ interval_secs: 1800 });
    expect(cadenceOf("every 2 hours")).toEqual({ interval_secs: 7200 });
    expect(cadenceOf("hourly")).toEqual({ interval_secs: 3600 });
    expect(cadenceOf("every day")).toEqual({ interval_secs: 86400 });
    expect(cadenceOf("0 9 * * 1-5")).toEqual({ cron_expr: "0 9 * * 1-5" });
    expect(cadenceOf("none")).toBeNull();
    expect(cadenceOf("whenever")).toBeNull();
    expect(cadenceWords({ interval_secs: 7200 })).toBe("every 2 hours");
    // A schedule reads the way people say it; an uncommon expression stays as written.
    expect(cadenceWords({ cron_expr: "0 9 * * *" })).toBe("every day at 09:00 (UTC)");
    expect(cadenceWords({ cron_expr: "30 7 * * 1-5" })).toBe("every weekday at 07:30 (UTC)");
    expect(cadenceWords({ cron_expr: "0 18 * * 5" })).toBe("every Friday at 18:00 (UTC)");
    expect(cadenceWords({ cron_expr: "*/15 * * * *" })).toBe("on the schedule */15 * * * * (UTC)");
  });
  it("names the problems of a schedule that cannot run", () => {
    const spec = { ...parseAgentSpec("name: X\nrole: You are X.\ninstructions:\n- Do it."), schedule: "sometimes", standing_message: "" };
    const problems = specProblems(spec, null);
    expect(problems.some((p) => p.includes("not a cadence"))).toBe(true);
    expect(problems.some((p) => p.includes("told what to do"))).toBe(true);
    expect(specProblems({ ...spec, schedule: "none" }, null)).toEqual([]);
  });
  it("round-trips through the block text", () => {
    const spec = { ...parseAgentSpec("name: X\nrole: You are X.\ninstructions:\n- Do it."), schedule: "every 3 minutes", standing_message: "Report." };
    const again = parseAgentSpec(formatAgentSpec(spec));
    expect(again.schedule).toBe("every 3 minutes");
    expect(again.standing_message).toBe("Report.");
  });
});

it("reads a list's 'none' as empty", () => {
  const spec = parseAgentSpec("name: X\nskills:\n- none\nneeds:\n- None.\ntools:\n- a.b — when");
  expect(spec.skills).toEqual([]);
  expect(spec.needs).toEqual([]);
  expect(spec.tools).toEqual([{ name: "a.b", when: "when" }]);
});

it("reads a skill line's name before its note", () => {
  const spec = parseAgentSpec("name: X\nskills:\n- cite-the-source - To cite the source of the fact.\n- `form-filling` — when a form is involved\n- plain-skill");
  expect(spec.skills).toEqual(["cite-the-source", "form-filling", "plain-skill"]);
});

describe("a budget in the spec", () => {
  it("reads tokens, dollars, or both, and none as no bound", () => {
    expect(budgetOf("20000 tokens")).toEqual({ max_tokens: 20000 });
    expect(budgetOf("20k tokens")).toEqual({ max_tokens: 20000 });
    expect(budgetOf("$0.05")).toEqual({ max_cost_usd: 0.05 });
    expect(budgetOf("20,000 tokens, $0.05")).toEqual({ max_tokens: 20000, max_cost_usd: 0.05 });
    expect(budgetOf("none")).toBeNull();
    expect(budgetOf("")).toBeNull();
    expect(budgetOf("a lot")).toBeNull();
    expect(budgetWords({ max_tokens: 20000, max_cost_usd: 0.05 })).toBe("20,000 tokens, $0.05");
    expect(budgetWords({ max_tokens: "100" })).toBe("100 tokens");
  });
  it("parses the block's budget line and formats it back", () => {
    const spec = parseAgentSpec("name: Thrifty\nbudget: 5000 tokens\ninstructions:\n- Answer.");
    expect(spec.budget).toBe("5000 tokens");
    expect(formatAgentSpec(spec)).toContain("budget: 5000 tokens");
  });
});

describe("tablesNotNamed", () => {
  const required = new Map([["servicenow.list-records", ["table"]], ["echo", []]]);
  it("names the table-taking tools a charter gives without naming a table", () => {
    const spec = { ...emptySpec(), tools: [{ name: "servicenow.list-records", when: "to retrieve data" }, { name: "echo", when: "" }], instructions: ["Use servicenow.list-records to retrieve search history and interaction records."] };
    expect(tablesNotNamed(spec, required)).toEqual([{ tool: "servicenow.list-records", param: "table" }]);
  });
  it("is quiet once a table is named anywhere in the charter", () => {
    const spec = { ...emptySpec(), tools: [{ name: "servicenow.list-records", when: "" }], instructions: ["Call servicenow.list-records on table ts_query with sysparm_query …"] };
    expect(tablesNotNamed(spec, required)).toEqual([]);
    expect(namesATable("the table data from the API")).toBe(false);
    expect(namesATable("read the table: interaction")).toBe(true);
  });
});

describe("factsNotCarried", () => {
  it("names the numbers, ids and names a proposal dropped, with their sentences", () => {
    const request = "An office weather desk. Our offices are Bangalore (latitude 12.97, longitude 77.59) and London (51.51, -0.13). It answers from open-meteo.get-v1forecast. It reads only.";
    const spec = { ...emptySpec(), role: "A weather desk for offices.", instructions: ["Use open-meteo.get-v1forecast with the office's coordinates", "Report only the numbers returned"] };
    const gap = factsNotCarried(request, spec);
    expect(gap.facts).toEqual(expect.arrayContaining(["12.97", "77.59", "51.51", "-0.13", "Bangalore", "London"]));
    expect(gap.facts).not.toContain("open-meteo.get-v1forecast");
    expect(gap.sentences).toEqual(["Our offices are Bangalore (latitude 12.97, longitude 77.59) and London (51.51, -0.13)."]);
  });
  it("does not take the word that opens a sentence for a name", () => {
    const spec = { ...emptySpec(), instructions: ["Use servicenow.list-records to retrieve incident data"] };
    expect(factsNotCarried("Dont you need search history tools, tool for incident, tools for other data sources in servicenow?", spec).facts).toEqual([]);
    expect(factsNotCarried("Offices are in London. Bangalore too.", spec).facts).toEqual(["London"]);
  });
  it("is quiet when the charter carries them", () => {
    const spec = { ...emptySpec(), instructions: ["Offices: Bangalore 12.97 77.59, London 51.51 -0.13"] };
    expect(factsNotCarried("Offices are Bangalore (12.97, 77.59) and London (51.51, -0.13).", spec).facts).toEqual([]);
  });
});
