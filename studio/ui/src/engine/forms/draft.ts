// The AgentDraft — one client draft type ≡ the server Blueprint document — and
// the single validation set applied identically to save, publish, import, and
// load (04-data-model.md). Every violation is anchored to a control path and a
// kind, and returned together. Screens never re-implement these rules.

export type Autonomy = "read_only" | "supervised" | "full";
export type MeasureKind = "target" | "gate" | "guardrail";
export type MeasureSource = "outcome" | "connector" | "eval";
export type ChannelKind = "" | "slack" | "teams" | "email" | "web";
export type ToolEffect = "read" | "write" | "execute" | "egress";

export interface Measure { name: string; source: MeasureSource; target: string; window: string; kind: MeasureKind }
export interface ToolRule { tool: string; rule: string }           // tool "" = all tools
export interface MemoryBlock { label: string; description: string; limit: number; scope: "agent" | "user" }
export interface Trigger { kind: "cron" | "event"; spec: string; prompt: string }
export type DraftSource = "guided" | "compose" | "chat" | "import" | "improve" | "test";

export interface AgentDraft {
  source: DraftSource;
  agentId?: string;
  base?: AgentDraft & { version: number };
  template?: string | null;
  name: string;
  description: string;
  model: string;
  autonomy: Autonomy;
  goal: string;
  measures: Measure[];
  stable: string;
  context: string;
  connectors: string[];
  wrapped: string[];
  rules: ToolRule[];
  secrets: Record<string, string>;        // connectorId → SecretRef name
  skills: string[];
  memory: MemoryBlock[];
  channelKind: ChannelKind;
  channelTarget: string;
  triggers: Trigger[];
  reviewFork: boolean;
  cadence: string;
  gateSuite: string;
}

export function emptyDraft(source: DraftSource): AgentDraft {
  return {
    source, template: null, name: "", description: "", model: "", autonomy: "supervised",
    goal: "", measures: [], stable: "", context: "", connectors: [], wrapped: [], rules: [],
    secrets: {}, skills: [], memory: [], channelKind: "", channelTarget: "", triggers: [],
    reviewFork: false, cadence: "", gateSuite: "",
  };
}

export type ViolationKind = "schema" | "coherence" | "slot";
export interface Violation { path: string; kind: ViolationKind; message: string; file?: string }

/** Which mounted tools carry a side-effecting boundary — supplied by the catalog
 * (tools derive from connectors). Absent ⇒ the autonomy/effect coherence rule
 * and the rule-references-a-mounted-tool check are skipped. */
export interface DraftCatalog {
  mountedTools: { id: string; effect: ToolEffect }[];
  /** The channels the world has, with their intake state — absent ⇒ the
   * channel-kind check is skipped. */
  channels?: { kind: string; status: "connected" | "paused" | "available" }[];
  /** The models the world can run on, and what each is known to do — absent ⇒
   * the model-capability check is skipped. */
  models?: { id: string; label: string; tools: boolean }[];
  /** Which connector each known SecretRef belongs to — absent ⇒ the
   * credential-ownership check is skipped. */
  secretOwners?: Record<string, string>;
  /** The tools each skill's procedure names — absent ⇒ the curated-subset
   * check is skipped. */
  skillTools?: Record<string, string[]>;
}

const CRON_FIELDS = 5;
const SIDE_EFFECTS: ToolEffect[] = ["write", "execute", "egress"];

/** The shapes a credential takes when it is pasted into text: the issuers'
 * prefixes, and a long unbroken blob carrying both letters and digits — the
 * shape of a key, not of a sentence. Hyphens are excluded from the blob so
 * ordinary hyphenated prose never matches. */
const CREDENTIAL_SHAPES = [
  /\bsk-[A-Za-z0-9-]{10,}/,
  /xox[baprs]-[A-Za-z0-9-]{8,}/,
  /gh[pousr]_[A-Za-z0-9]{16,}/,
  /github_pat_[A-Za-z0-9_]{20,}/,
  /AKIA[0-9A-Z]{12,}/,
  /Bearer\s+[A-Za-z0-9._-]{16,}/,
  /-----BEGIN [A-Z ]*PRIVATE KEY-----/,
  /\b(?=[A-Za-z0-9+/=_]*\d)(?=[A-Za-z0-9+/=_]*[A-Za-z])[A-Za-z0-9+/=_]{40,}\b/,
];

/** Every credential-looking value in a piece of text. Prompt-bearing fields are
 * published and stored, so a value sitting in one has already escaped the
 * SecretRef discipline — whether it was typed or imported. */
export function credentialLike(text: string): string[] {
  const hits = CREDENTIAL_SHAPES.flatMap((re) => text.match(new RegExp(re.source, "g")) ?? []);
  return [...new Set(hits)];
}

/** Enough of a value to recognise it, never enough to use it. */
export const maskCredential = (v: string) => `${v.slice(0, 6)}…${v.length} chars`;

/** The identical rule set for save / publish / import / load. Order is stable so
 * a control can find its own violations; all are returned together. */
export function validateDraft(d: AgentDraft, catalog?: DraftCatalog): Violation[] {
  const out: Violation[] = [];
  const add = (path: string, kind: ViolationKind, message: string, file?: string) => out.push({ path, kind, message, file });

  // Identity + directive + goal
  if (!d.name.trim()) add("identity.name", "schema", "Name is required.", "agent.md");
  if (!d.stable.trim()) add("directive.stable", "schema", "The stable directive cannot be empty.", "directive/stable.md");
  if (!d.goal.trim()) add("goal.statement", "schema", "A goal statement is required.", "goal.md");
  if (d.measures.length === 0) add("goal.measures", "coherence", "Add at least one measure so the agent knows what it is judged on.", "goal.md");
  d.measures.forEach((m, i) => {
    if (!m.name.trim() || !m.target.trim()) add(`goal.measures[${i}]`, "schema", "Each measure needs a name and a target.", "goal.md");
  });

  // Model vs. the tools it would have to call (needs the catalog's profiles)
  const mounted = catalog?.mountedTools ?? [];
  if (d.model && catalog?.models) {
    const profile = catalog.models.find((m) => m.id === d.model);
    if (!profile) add("identity.model", "coherence", `No model profile for "${d.model}" — pick one the world can run.`, "agent.md");
    else if (!profile.tools && mounted.length > 0) {
      add("identity.model", "coherence", `${profile.label} cannot call tools, but this agent mounts ${mounted.length}.`, "agent.md");
    }
  }

  // Autonomy vs. tool effects (needs the catalog to know effects)
  const sideEffecting = mounted.filter((t) => SIDE_EFFECTS.includes(t.effect));
  if (d.autonomy === "read_only" && sideEffecting.length > 0) {
    add("identity.autonomy", "coherence", `read_only conflicts with ${sideEffecting.length} mounted write/execute/egress tool${sideEffecting.length === 1 ? "" : "s"}.`, "agent.md");
    for (const t of sideEffecting) add(`toolsets.${t.id}`, "coherence", `${t.id} (${t.effect}) cannot run under read_only autonomy.`, "toolsets.md");
  }

  // Connectors ⇒ a secret slot each. A secret is only ever a reference name;
  // a pasted value is refused rather than carried.
  for (const c of d.connectors) {
    const ref = d.secrets[c];
    if (!ref || !ref.trim() || ref === "<unbound>") add(`connectors.${c}.secret`, "slot", `${c} needs a SecretRef name.`, "toolsets.md");
    else if (!ref.startsWith("rusty:secret:")) {
      add(`connectors.${c}.secret`, "schema", `${c}'s credential must be a SecretRef name (rusty:secret:…), never a value.`, "toolsets.md");
    } else {
      const owner = catalog?.secretOwners?.[ref];
      if (owner && owner !== c) {
        add(`connectors.${c}.secret`, "coherence", `${c} is bound to ${owner}'s credential — a SecretRef belongs to one system.`, "toolsets.md");
      }
    }
  }

  // Channels / triggers: mounted connectors ⇒ a channel or a trigger; a channel kind ⇒ a target
  if (d.connectors.length > 0 && d.channelKind === "" && d.triggers.length === 0) {
    add("channels[0]", "slot", "Mounted connectors need a channel or a trigger to reach the agent.", "triggers.md");
  }
  if (d.channelKind !== "" && !d.channelTarget.trim()) {
    add("channels[0].target", "slot", "A channel kind needs a target.", "agent.md");
  }
  // A channel kind must be a channel the world has connected (needs the catalog)
  if (d.channelKind !== "" && catalog?.channels) {
    const ch = catalog.channels.find((c) => c.kind === d.channelKind);
    if (!ch) add("channels[0].kind", "slot", `No ${d.channelKind} channel exists — add its adapter in the Catalog.`, "agent.md");
    else if (ch.status === "paused") add("channels[0].kind", "slot", `The ${d.channelKind} channel is paused — resume its intake in the Catalog.`, "agent.md");
    else if (ch.status === "available") add("channels[0].kind", "slot", `The ${d.channelKind} channel is not connected — connect it in the Catalog.`, "agent.md");
  }

  // A credential in prompt-bearing text: it would be published into the
  // assembled prompt and stored with the agent. The SecretRef discipline holds
  // for every field, not only the credential slot.
  const prompted: { path: string; file: string; text: string }[] = [
    { path: "directive.stable", file: "directive/stable.md", text: d.stable },
    { path: "directive.context", file: "directive/context.md", text: d.context },
    { path: "goal.statement", file: "goal.md", text: d.goal },
    ...d.rules.map((r, i) => ({ path: `tool_rules[${i}]`, file: "rules.md", text: r.rule })),
    ...d.memory.map((m, i) => ({ path: `memory[${i}].description`, file: "memory.md", text: m.description })),
  ];
  for (const p of prompted) {
    const [hit] = credentialLike(p.text);
    if (hit) add(p.path, "schema", `A credential value (${maskCredential(hit)}) sits in this text and would be published into the prompt. Bind it as a SecretRef instead.`, p.file);
  }

  // A skill curates a set of tools. Mounting one whose procedure names a tool
  // the agent does not have leaves the procedure with a step it cannot take.
  for (const s of catalog?.skillTools ? d.skills : []) {
    const wants = catalog!.skillTools![s] ?? [];
    const missing = wants.filter((t) => !mounted.some((m) => m.id === t));
    if (wants.length && missing.length) {
      add(`skills.${s}`, "coherence", `${s} uses ${missing.join(", ")}, which this agent does not mount.`, "toolsets.md");
    }
  }

  // Memory: description load-bearing, labels unique
  const labels = new Set<string>();
  d.memory.forEach((m, i) => {
    if (!m.description.trim()) add(`memory[${i}].description`, "coherence", "Memory description is load-bearing prompt content — it cannot be empty.", "memory.md");
    if (labels.has(m.label)) add(`memory[${i}].label`, "coherence", `Memory label "${m.label}" is not unique.`, "memory.md");
    labels.add(m.label);
  });

  // Tool rules: referenced tool must be mounted; rule text non-empty
  const mountedIds = new Set((catalog?.mountedTools ?? []).map((t) => t.id));
  d.rules.forEach((r, i) => {
    if (!r.rule.trim()) add(`tool_rules[${i}]`, "schema", "A tool rule cannot be empty.", "rules.md");
    if (r.tool && catalog && !mountedIds.has(r.tool)) add(`tool_rules[${i}]`, "coherence", `Rule references "${r.tool}", which is not mounted.`, "rules.md");
  });

  // Triggers: cron 5 fields; event chosen + source mounted; seed prompt required
  d.triggers.forEach((t, i) => {
    if (t.kind === "cron" && t.spec.trim().split(/\s+/).filter(Boolean).length !== CRON_FIELDS) {
      add(`triggers[${i}].spec`, "schema", "A cron spec has five fields.", "triggers.md");
    }
    if (t.kind === "event") {
      if (!t.spec.trim()) add(`triggers[${i}].spec`, "schema", "Choose an event.", "triggers.md");
      else {
        const connector = t.spec.split(".")[0];
        if (!d.connectors.includes(connector)) add(`triggers[${i}].spec`, "coherence", `Event source "${connector}" must be a mounted connector.`, "triggers.md");
      }
    }
    if (!t.prompt.trim()) add(`triggers[${i}].prompt`, "schema", "A trigger needs a seed prompt.", "triggers.md");
  });

  // Learning: a promotion gate must name an eval suite
  if (d.reviewFork && !d.gateSuite.trim()) {
    add("learning.promotion_gate", "coherence", "The review fork's promotion gate must name an eval suite.", "learning.md");
  }

  return out;
}

/** The read-only assembled prompt (three tiers), a pure projection of the draft.
 * Never editable; the byte count is displayed. `promoted` = skill ids whose
 * runtime state is Promoted (index shows Promoted only). */
export function assembledPrompt(d: AgentDraft, promoted: { id: string; description: string }[] = []): { stable: string; context: string; volatile: string; bytes: number } {
  const measures = d.measures.length
    ? "You are measured on: " + d.measures.map((m) => `${m.name} ${m.target} (${m.window})`).join("; ") + "."
    : "";
  const toolRules = d.rules.length
    ? "\n\n## Tool rules\n" + d.rules.map((r) => `- ${r.tool || "*"}: ${r.rule}`).join("\n")
    : "";
  const stable = [`## Goal\n${d.goal}`, measures, d.stable, toolRules].filter(Boolean).join("\n\n");
  const context = d.context;
  const skills = promoted.length ? "## Skills\n" + promoted.map((s) => `- ${s.id}: ${s.description}`).join("\n") : "";
  const mem = d.memory.length ? "## Memory\n" + d.memory.map((m) => `- ${m.label} (${m.limit}): ${m.description}`).join("\n") : "";
  const now = `## Now\n${new Date().toISOString()} · ${d.channelKind || "no channel"}`;
  const volatile = [skills, mem, now].filter(Boolean).join("\n\n");
  const bytes = new TextEncoder().encode([stable, context, volatile].join("\n\n")).length;
  return { stable, context, volatile, bytes };
}
