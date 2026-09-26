#!/usr/bin/env node
// The UI tier contract, enforced. Four tiers, importable only downward:
//
//   tokens  <-  primitives  <-  views  <-  screens / app
//
// lib/, state/, and i18n/ are infrastructure: reachable from views and
// screens, never from primitives or tokens. On top of the import direction,
// every stylesheet outside tokens/ must style itself through tokens — no raw
// colors, no raw spacing/radius/type sizes, and no uppercase microcopy.
//
// Run with --allow-legacy while a port is in flight to skip the check that
// the retired directories (features/, components/, styles/) are gone.

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, dirname, resolve, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const SRC = resolve(dirname(fileURLToPath(import.meta.url)), "..", "src");
const allowLegacy = process.argv.includes("--allow-legacy");

const TIERS = { tokens: 0, primitives: 1, views: 2, screens: 3, app: 3 };
const INFRA = new Set(["lib", "state", "i18n"]);
const RETIRED = ["features", "components", "styles"];

const failures = [];

function fail(file, message) {
  failures.push(`${relative(SRC, file)}: ${message}`);
}

function walk(dir) {
  const files = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) files.push(...walk(path));
    else files.push(path);
  }
  return files;
}

function topDirOf(path) {
  const rel = relative(SRC, path);
  if (rel.startsWith("..")) return null;
  const [first] = rel.split(sep);
  return first.includes(".") ? "." : first;
}

// -- retired directories ----------------------------------------------------

if (!allowLegacy) {
  for (const dir of RETIRED) {
    if (existsSync(join(SRC, dir))) {
      failures.push(`src/${dir}/ still exists — the tier restructure retired it`);
    }
  }
}

// -- import direction -------------------------------------------------------

const importPattern = /(?:import|export)\s[^'"]*?from\s*['"]([^'"]+)['"]|import\s*['"]([^'"]+)['"]/g;

function checkImports(file) {
  const from = topDirOf(file);
  if (from === null || from === ".") return;
  const source = readFileSync(file, "utf8");
  for (const match of source.matchAll(importPattern)) {
    const spec = match[1] ?? match[2];
    if (!spec || !spec.startsWith(".")) continue;
    const target = topDirOf(resolve(dirname(file), spec));
    if (target === null || target === "." || target === from) continue;
    if (RETIRED.includes(target)) {
      if (!allowLegacy) fail(file, `imports retired src/${target}/ (${spec})`);
      continue;
    }
    if (from in TIERS && target in TIERS && TIERS[target] > TIERS[from]) {
      fail(file, `tier "${from}" imports upward from "${target}" (${spec})`);
    }
    if ((from === "tokens" || from === "primitives") && INFRA.has(target)) {
      fail(file, `tier "${from}" must stay free of src/${target}/ (${spec})`);
    }
    if (from === "tokens" && target !== "tokens") {
      fail(file, `tokens must not import from src/${target}/ (${spec})`);
    }
  }
}

// -- stylesheet discipline --------------------------------------------------

const COLOR_LITERAL = /#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?|hwb|oklch|oklab|lab|lch)\(/;
const SIZED_PROPS = /^\s*(?:font|font-size|border-radius|(?:padding|margin|gap|row-gap|column-gap)(?:-[a-z]+)*)\s*:/;

function checkStylesheet(file) {
  const from = topDirOf(file);
  if (from === "tokens") return;
  if (RETIRED.includes(from) && allowLegacy) return;
  const lines = readFileSync(file, "utf8").split("\n");
  lines.forEach((line, index) => {
    if (line.includes("tiers:allow")) return;
    const at = `line ${index + 1}`;
    if (COLOR_LITERAL.test(line)) {
      fail(file, `${at}: raw color literal — define it in src/tokens/tokens.css`);
    }
    if (/text-transform\s*:\s*uppercase/.test(line)) {
      fail(file, `${at}: uppercase microcopy is retired — write the label in sentence case`);
    }
    if (SIZED_PROPS.test(line)) {
      const value = line.slice(line.indexOf(":") + 1).replace(/var\([^)]*\)/g, "");
      if (/\d+(?:\.\d+)?px/.test(value)) {
        fail(file, `${at}: raw px in a spacing/type declaration — use a token`);
      }
    }
  });
}

function checkComponentColors(file) {
  const from = topDirOf(file);
  if (from === "tokens" || RETIRED.includes(from)) return;
  if (/\.test\.[jt]sx?$/.test(file) || file.includes(`${sep}test${sep}`)) return;
  const lines = readFileSync(file, "utf8").split("\n");
  lines.forEach((line, index) => {
    if (line.includes("tiers:allow")) return;
    if (/["'`]#[0-9a-fA-F]{3,8}\b/.test(line)) {
      fail(file, `line ${index + 1}: hex color in component code — colors live in src/tokens/tokens.css`);
    }
  });
}

for (const file of walk(SRC)) {
  if (/\.(ts|tsx)$/.test(file)) {
    checkImports(file);
    checkComponentColors(file);
  } else if (file.endsWith(".css")) {
    checkStylesheet(file);
  }
}

if (failures.length) {
  console.error(`check-tiers: ${failures.length} violation${failures.length === 1 ? "" : "s"}\n`);
  for (const failure of failures) console.error(`  ${failure}`);
  process.exit(1);
}
console.log("check-tiers: import direction and token discipline hold.");
