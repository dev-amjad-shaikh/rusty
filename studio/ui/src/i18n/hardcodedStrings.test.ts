import { describe, it, expect } from "vitest";
import { readFileSync } from "fs";
import { resolve } from "path";

const SRC_DIR = resolve(__dirname, "..");

// Files that must have zero hardcoded user-facing strings.
const MUST_BE_CLEAN = ["app/AppShell.tsx"];

// Files still pending migration — logged but not enforced.
const PENDING_MIGRATION = [
  "app/navigation.ts",
  "app/RuntimeBootstrap.tsx",
];

// Load the English message catalog — any value that appears verbatim in a
// MUST_BE_CLEAN file is a violation (it should be referenced via t()).
const enMessages = JSON.parse(
  readFileSync(resolve(SRC_DIR, "i18n/messages/en.json"), "utf-8"),
) as Record<string, string>;

const MESSAGE_VALUES = Object.values(enMessages);

// Patterns that are never user-facing, even if they happen to match a message.
const SAFE_PATTERNS = [
  /^\s*$/, // whitespace only
  /^[a-z][a-z0-9-]*$/, // kebab-case CSS classes / IDs
  /^[a-z]+(\/[a-z]+)*$/, // module paths
  /^\d+(\.\d+)?$/, // numbers
  /^(true|false|null|undefined)$/, // JS literals
  /^#[a-fA-F0-9]{3,8}$/, // hex colors
  /^https?:\/\/.+/, // URLs
  /^M[0-9.,\sLCSQTAZz-]+$/, // SVG path data
  /^[A-Z][a-z]+[A-Z][a-zA-Z]+$/, // PascalCase identifiers
  /^[a-z]+(_[a-z]+)*$/, // snake_case keys
  /^[a-z]+\.[a-z-]+$/, // dotted keys like "workspace.local"
];

function isSafe(text: string): boolean {
  const trimmed = text.trim();
  if (trimmed.length === 0) return true;
  return SAFE_PATTERNS.some((re) => re.test(trimmed));
}

function findUntranslatedStrings(filePath: string): string[] {
  const content = readFileSync(resolve(SRC_DIR, filePath), "utf-8");

  // Strip imports and comments so they do not produce false positives.
  const code = content
    .replace(/^\s*import\s+.*?;?\s*$/gm, "")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/.*/g, "");

  const findings = new Set<string>();

  // Quoted string literals.
  const quotedRe = /"([^"]*)"|'([^']*)'/g;
  let m: RegExpExecArray | null;
  while ((m = quotedRe.exec(code)) !== null) {
    const text = m[1] ?? m[2];
    if (MESSAGE_VALUES.includes(text) && !isSafe(text)) {
      findings.add(text);
    }
  }

  // JSX text nodes (pure text, no expressions).
  const textNodeRe = />([^<{][^<]*)</g;
  while ((m = textNodeRe.exec(code)) !== null) {
    const text = m[1].trim();
    if (MESSAGE_VALUES.includes(text) && !isSafe(text)) {
      findings.add(text);
    }
  }

  return [...findings];
}

describe("hardcoded-string guard", () => {
  it("MUST_BE_CLEAN files have no hardcoded user-facing strings", () => {
    for (const file of MUST_BE_CLEAN) {
      const findings = findUntranslatedStrings(file);
      expect(
        findings,
        `${file} contains untranslated strings: ${findings.join(", ")}`,
      ).toEqual([]);
    }
  });

  it("PENDING_MIGRATION files are tracked (non-blocking)", () => {
    for (const file of PENDING_MIGRATION) {
      const findings = findUntranslatedStrings(file);
      if (findings.length > 0) {
        // eslint-disable-next-line no-console
        console.log(
          `[PENDING] ${file}: ${findings.length} hardcoded strings`,
        );
      }
    }
    // This test always passes — it is for visibility only.
    expect(true).toBe(true);
  });
});
