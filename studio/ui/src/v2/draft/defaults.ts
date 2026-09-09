// Blank-draft factory: the skeleton every entry path starts from (R-A4's
// "Start blank" renders the full form against this). Field defaults come from
// the schema shape — scalar fields start empty so their violations surface
// immediately as unfilled slots.

import type { AgentDraft } from "./agent-draft.gen";

export function blankDraft(source: AgentDraft["source"]): AgentDraft {
  return {
    source,
    name: "",
    description: "",
    model: "",
    autonomy: "read_only",
    goal: "",
    measures: [],
    stable: "",
    context: "",
    connectors: [],
    wrapped: [],
    rules: [],
    secrets: {},
    skills: [],
    memory: [],
    channelKind: "",
    channelTarget: "",
    triggers: [],
    reviewFork: false,
    cadence: "",
    gateSuite: "",
  };
}
