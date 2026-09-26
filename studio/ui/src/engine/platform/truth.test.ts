import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { useEngine } from "../state";

// The ledger of screens whose DATA still comes from src/fixtures. Every one of
// them is demo-fed: its rows exist in this browser, not on a server. Moving a
// screen onto the server means deleting its fixture imports AND taking it off
// this list — and the shell's Demo banner stays up until the list is empty.
// The list is checked both ways: a listed screen that no longer reads fixtures
// fails (update the ledger), and an unlisted screen that starts reading them
// fails (no screen quietly becomes demo-fed again). Type-only imports don't
// count — a type is a shape, not a record.
const DEMO_FED: string[] = [];

// vitest runs from the package root (studio/ui); jsdom's URL can't resolve
// import.meta.url, so the path is anchored on the working directory instead.
const screensDir = join(process.cwd(), "src", "knot");
// A value import (not `import type`) whose module lives under src/fixtures.
// Imports end at ";", so a multi-line specifier list stays inside one match.
const FIXTURE_VALUE_IMPORT = /^import\s+(?!type\s)[^;]*?from\s+"(?:\.\.\/)+fixtures\//m;

function screensReadingFixtures(): string[] {
  return readdirSync(screensDir, { recursive: true })
    .map(String)
    .filter((f) => f.endsWith(".tsx") && !f.endsWith(".test.tsx"))
    .filter((f) => FIXTURE_VALUE_IMPORT.test(readFileSync(join(screensDir, f), "utf8")))
    .map((f) => f.replace(/\.tsx$/, ""))
    .sort();
}

describe("truth ledger", () => {
  it("lists exactly the screens that still read fixture data", () => {
    const fed = screensReadingFixtures();
    const listed = [...DEMO_FED].sort();
    const stale = listed.filter((s) => !fed.includes(s));
    const unlisted = fed.filter((s) => !listed.includes(s));
    expect(stale, "screens listed as demo-fed that no longer read fixtures — remove them from DEMO_FED").toEqual([]);
    expect(unlisted, "screens that started reading fixture data — a screen must not become demo-fed unannounced").toEqual([]);
  });

});
