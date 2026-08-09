#!/usr/bin/env node

import { readdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const studioDir = path.dirname(fileURLToPath(import.meta.url));
const runnerName = path.basename(fileURLToPath(import.meta.url));
const suites = readdirSync(studioDir, { withFileTypes: true })
  .filter((entry) => entry.isFile())
  .map((entry) => entry.name)
  .filter((name) => /^test-.+\.mjs$/.test(name) && name !== runnerName)
  .sort();

if (suites.length === 0) {
  console.error("FAIL: no Studio test suites were discovered");
  process.exit(1);
}

const failures = [];

for (const suite of suites) {
  console.log(`\n=== ${suite} ===`);
  const result = spawnSync(process.execPath, [path.join(studioDir, suite)], {
    cwd: studioDir,
    stdio: "inherit",
  });

  if (result.error) {
    console.error(`FAIL: ${suite} could not start: ${result.error.message}`);
    failures.push(suite);
  } else if (result.status !== 0) {
    const reason = result.signal ? `signal ${result.signal}` : `exit ${result.status}`;
    console.error(`FAIL: ${suite} (${reason})`);
    failures.push(suite);
  }
}

if (failures.length > 0) {
  console.error(`\n${failures.length} of ${suites.length} Studio test suites failed: ${failures.join(", ")}`);
  process.exit(1);
}

console.log(`\nPASS: ${suites.length} Studio test suites`);
