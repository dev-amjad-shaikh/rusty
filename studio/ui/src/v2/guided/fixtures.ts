// Shared fixtures for the guided tests: one template with a mounted
// connector that carries both a read and an egress tool, plus the catalogs
// the form and validation resolve against.

import { blankDraft } from "../draft/defaults";
import type { DraftCatalogs } from "../draft/catalogs";
import type { Template } from "./templates";

export function fixtureTemplate(): Template {
  return {
    id: "tpl-support",
    name: "Support triage",
    version: 3,
    blurb: "Triage the inbound support queue.",
    document: {
      ...blankDraft("test"),
      description: "Triages the support inbox.",
      goal: "Zero unanswered tickets.",
      connectors: ["gmail"],
      skills: ["triage"],
    },
  };
}

export function fixtureCatalogs(): DraftCatalogs {
  return {
    models: ["gpt-5", "claude-opus"],
    connectors: [
      {
        id: "gmail",
        name: "Gmail",
        tools: [
          { id: "gmail.read", effect: "read" },
          { id: "gmail.send", effect: "egress" },
        ],
        events: ["message_received"],
      },
    ],
    skills: [{ id: "triage", description: "Triage inbound mail." }],
    evalSuites: [],
  };
}

export function freshStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (key: string) => map.get(String(key)) ?? null,
    key: (index: number) => [...map.keys()][index] ?? null,
    removeItem: (key: string) => void map.delete(String(key)),
    setItem: (key: string, value: string) => void map.set(String(key), String(value)),
  };
}
