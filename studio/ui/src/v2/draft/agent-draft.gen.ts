/**
 * The AgentDraft document (handoff 04), generated from
 * `agent-draft.schema.json` by `npm run generate:draft` — do not edit by
 * hand. One type shared by Guided, Compose, Chat, Import, and Improve (R-A1).
 */

export interface Measure {
  name: string;
  source: "outcome" | "connector" | "eval";
  target: string;
  window: string;
  kind: "target" | "gate" | "guardrail";
}

export interface MemoryBlock {
  label: string;
  description: string;
  limit: number;
  scope: "agent" | "user";
}

export interface ToolRule {
  tool: string;
  rule: string;
}

export interface Trigger {
  kind: "cron" | "event";
  spec: string;
  prompt: string;
}

/** The studio-side draft document (handoff 04). One type shared by Guided, Compose, Chat, Import, and Improve (R-A1); the AgentDraftForm walks this schema, so a field added here appears in every entry path without per-path code. Regenerate the TypeScript with `npm run generate:draft`. */
export interface AgentDraft {
  /** Which entry path created or last touched the draft. */
  source: "guided" | "compose" | "chat" | "import" | "improve" | "test";
  /** Set when editing a published blueprint. */
  agentId?: string;
  /** Snapshot of the published head this draft diffs against. */
  base?: AgentDraft & { version: number };
  /** Template id the draft started from, if any. */
  template?: string | null;
  /** Agent name. */
  name: string;
  /** Model ref, prefix-routed. */
  model: string;
  /** What the agent is for, in one paragraph. */
  description: string;
  /** How much the agent may do without asking. */
  autonomy: "read_only" | "supervised" | "full";
  /** The outcome the agent is judged on. */
  goal: string;
  /** Measures the agent is judged on; they enter the stable tier and drive Metrics (R-A10). */
  measures: Measure[];
  /** Stable tier — identity and guidance. */
  stable: string;
  /** Context tier — workspace facts. */
  context: string;
  /** RegistryRef ids of mounted connectors; tools derive from connectors. */
  connectors: string[];
  /** One SecretRef name per mounted connector — names only, never values. */
  secrets: Record<string, string>;
  /** Mounted tool ids wrapped in approval_required(org_admins). */
  wrapped: string[];
  /** Structured tool rules (R-A11): a tool or all, plus the rule text. */
  rules: ToolRule[];
  /** Skill ids; the index shows Promoted only at runtime. */
  skills: string[];
  /** Memory blocks mounted on the agent. */
  memory: MemoryBlock[];
  /** Primary channel kind; empty means unbound. */
  channelKind: "" | "slack" | "teams" | "email" | "web";
  /** Channel target — a channel id, address, or route. */
  channelTarget: string;
  /** Schedules and connector events that run the agent without a message (R-A12). */
  triggers: Trigger[];
  /** Run a post-turn review fork. */
  reviewFork: boolean;
  /** Consolidation cadence (cron). */
  cadence: string;
  /** Eval suite that gates promotion. */
  gateSuite: string;
}
