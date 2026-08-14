import { useRef, useState, type KeyboardEvent } from "react";
import type { Assistant } from "../../lib/contracts";
import { isUnicodeScalarString } from "../../lib/text";
import styles from "./AgentsPage.module.css";

export const capabilities = [
  { key: "purpose", label: "Purpose", detail: "The job and who it serves" },
  { key: "model", label: "Model", detail: "How the agent reasons" },
  { key: "knowledge", label: "Knowledge", detail: "Memory and context" },
  { key: "tools", label: "Tools", detail: "Actions it can take" },
  { key: "output", label: "Output", detail: "What it delivers" },
  { key: "guardrails", label: "Guardrails", detail: "Limits and approval" },
] as const;

export type Capability = (typeof capabilities)[number]["key"];

export interface AgentDraft {
  name: string;
  responsibility: string;
  audience: string;
  graph: string;
  model: string;
  memoryAccess: "none" | "read_only" | "read_write";
  scopes: string[];
  tools: string;
  outputMode: "runtime_default" | "text" | "json_object" | "json_schema";
  outputSchema: string;
  approval: "runtime_policy" | "irreversible" | "external_effect";
  recursionLimit: string;
}

export type AgentDefinition = Pick<Assistant, "name" | "graph" | "config" | "metadata">;

export function emptyAgentDraft(): AgentDraft {
  return {
    name: "", responsibility: "", audience: "", graph: "", model: "",
    memoryAccess: "none", scopes: [], tools: "", outputMode: "runtime_default",
    outputSchema: "", approval: "runtime_policy", recursionLimit: "",
  };
}

function plainObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function stringValue(value: unknown, fallback = "") {
  return typeof value === "string" ? value : fallback;
}

function stringList(value: unknown) {
  return Array.isArray(value) && value.every((item) => typeof item === "string") ? value : [];
}

function safeJsonNumbers(value: unknown): boolean {
  if (typeof value === "number") return Number.isFinite(value) && (!Number.isInteger(value) || Number.isSafeInteger(value));
  if (Array.isArray(value)) return value.every(safeJsonNumbers);
  if (plainObject(value)) return Object.values(value).every(safeJsonNumbers);
  return true;
}

export function editableAgent(agent: AgentDefinition) {
  if (!plainObject(agent.config) || !plainObject(agent.metadata)
    || !safeJsonNumbers(agent.config) || !safeJsonNumbers(agent.metadata)) return false;
  const intent = agent.config.studio_intent;
  if (intent !== undefined && !roundTripsStudioIntent(intent)) return false;
  if (("description" in agent.metadata && typeof agent.metadata.description !== "string")
    || ("audience" in agent.metadata && typeof agent.metadata.audience !== "string")) return false;
  const recursion = agent.config.recursion_limit;
  return recursion === undefined || (typeof recursion === "number" && Number.isSafeInteger(recursion) && recursion >= 1 && recursion <= 100_000);
}

function exactKeys(value: Record<string, unknown>, keys: string[]) {
  const actual = Object.keys(value).sort();
  return actual.length === keys.length && keys.slice().sort().every((key, index) => key === actual[index]);
}

function roundTripsStudioIntent(value: unknown) {
  if (!plainObject(value) || !exactKeys(value, ["format", "model", "tools", "memory", "approval", "output", "budget", "binding"])
    || value.format !== "rusty.agent-intent/v3" || typeof value.model !== "string"
    || !Array.isArray(value.tools) || value.tools.length > 16 || !plainObject(value.memory)
    || !plainObject(value.output) || !plainObject(value.budget) || !plainObject(value.binding)
    || !["runtime_policy", "irreversible", "external_effect"].includes(String(value.approval))) return false;
  if (!value.tools.every((tool) => plainObject(tool) && exactKeys(tool, ["name", "effect"])
    && typeof tool.name === "string" && /^[A-Za-z0-9._:-]{1,128}$/.test(tool.name)
    && ["pure", "read_only", "idempotent", "compensatable", "non_idempotent"].includes(String(tool.effect)))) return false;
  const toolNames = value.tools.map((tool) => (tool as Record<string, unknown>).name);
  if (new Set(toolNames).size !== toolNames.length) return false;
  if (!exactKeys(value.memory, ["access", "scopes"]) || !["none", "read_only", "read_write"].includes(String(value.memory.access))
    || !Array.isArray(value.memory.scopes) || !value.memory.scopes.every((scope) => typeof scope === "string" && ["run", "agent", "user", "team", "tenant"].includes(scope))
    || new Set(value.memory.scopes).size !== value.memory.scopes.length
    || (value.memory.access === "none" && value.memory.scopes.length !== 0)) return false;
  if (!exactKeys(value.output, ["mode", "schema"]) || !["runtime_default", "text", "json_object", "json_schema"].includes(String(value.output.mode))
    || typeof value.output.schema !== "string" || (value.output.mode !== "json_schema" && value.output.schema !== "")) return false;
  try {
    modelRequirement(value.model);
    outputSchemaRequirement(value.output.schema, value.output.mode as AgentDraft["outputMode"]);
    return true;
  } catch { return false; }
}

export function draftFromAgent(agent: AgentDefinition): AgentDraft {
  if (!editableAgent(agent)) throw new Error("This agent uses a configuration format the visual editor cannot change safely.");
  const config = agent.config as Record<string, unknown>;
  const metadata = agent.metadata as Record<string, unknown>;
  const intent = plainObject(config.studio_intent) ? config.studio_intent : {};
  const memory = plainObject(intent.memory) ? intent.memory : {};
  const output = plainObject(intent.output) ? intent.output : {};
  const tools = Array.isArray(intent.tools) ? intent.tools.flatMap((tool) => {
    if (!plainObject(tool) || typeof tool.name !== "string" || typeof tool.effect !== "string") return [];
    return [`${tool.name} | ${tool.effect}`];
  }).join("\n") : "";
  const memoryAccess = ["none", "read_only", "read_write"].includes(String(memory.access))
    ? memory.access as AgentDraft["memoryAccess"] : "none";
  const outputMode = ["runtime_default", "text", "json_object", "json_schema"].includes(String(output.mode))
    ? output.mode as AgentDraft["outputMode"] : "runtime_default";
  const approval = ["runtime_policy", "irreversible", "external_effect"].includes(String(intent.approval))
    ? intent.approval as AgentDraft["approval"] : "runtime_policy";
  const recursion = config.recursion_limit;
  return {
    name: agent.name,
    responsibility: stringValue(metadata.description),
    audience: stringValue(metadata.audience),
    graph: agent.graph,
    model: stringValue(intent.model),
    memoryAccess,
    scopes: memoryAccess === "none" ? [] : stringList(memory.scopes),
    tools,
    outputMode,
    outputSchema: outputMode === "json_schema" ? stringValue(output.schema) : "",
    approval,
    recursionLimit: typeof recursion === "number" && Number.isSafeInteger(recursion) ? String(recursion) : "",
  };
}

export function toolContracts(text: string) {
  const tools = text.trim() ? text.split("\n").map((line) => {
    const match = line.trim().match(/^([A-Za-z0-9._:-]{1,128})\s*\|\s*(pure|read_only|idempotent|compensatable|non_idempotent)$/);
    if (!match) throw new Error("Use one `tool_name | effect` contract per line.");
    return { name: match[1], effect: match[2] };
  }) : [];
  if (tools.length > 16) throw new Error("Choose no more than 16 tool contracts.");
  if (new Set(tools.map((tool) => tool.name)).size !== tools.length) throw new Error("Each tool may appear only once.");
  return tools;
}

export function modelRequirement(value: string) {
  if (!value) return "";
  if (!isUnicodeScalarString(value) || value !== value.trim() || new TextEncoder().encode(value).byteLength > 256 || /[\p{Cc}\p{Cf}]/u.test(value)) {
    throw new Error("Use a model name up to 256 bytes without surrounding spaces or hidden controls.");
  }
  if (!/^[A-Za-z0-9][A-Za-z0-9._/@:-]*$/.test(value) || value.includes("://") || /[^/]+:[^/]+@/.test(value)
    || /^sk-[A-Za-z0-9_-]{16,}$/i.test(value)) {
    throw new Error("Use a model identifier or registry reference, not a URL, credential, or secret token.");
  }
  return value;
}

export function outputSchemaRequirement(value: string, mode: AgentDraft["outputMode"]) {
  if (!value) {
    if (mode === "json_schema") throw new Error("Choose the named schema this agent must return.");
    return "";
  }
  if (mode !== "json_schema" || value !== value.trim() || !isUnicodeScalarString(value)
    || new TextEncoder().encode(value).byteLength > 256 || /[\p{Cc}\p{Cf}]/u.test(value)
    || !/^[A-Za-z0-9][A-Za-z0-9._/@:-]*$/.test(value) || value.includes("://")
    || /[^/]+:[^/]+@/.test(value) || /^sk-[A-Za-z0-9_-]{16,}$/i.test(value)) {
    throw new Error("Use a named schema identifier up to 256 bytes, without a URL, credential, hidden text, or surrounding spaces.");
  }
  return value;
}

export function hasPortableIntent(draft: AgentDraft) {
  return Boolean(draft.model || draft.tools.trim() || draft.memoryAccess !== "none"
    || draft.approval !== "runtime_policy" || draft.outputMode !== "runtime_default" || draft.outputSchema);
}

export function validateAgentDraft(draft: AgentDraft) {
  if (!draft.name.trim() || !draft.responsibility.trim() || !draft.graph) throw new Error("Name, responsibility, and behavior are required.");
  if (draft.memoryAccess !== "none" && !draft.scopes.length) throw new Error("Choose what memory this agent may use.");
  if (!hasPortableIntent(draft)) throw new Error("Add at least one model, memory, tool, output, or approval requirement.");
  const recursion = draft.recursionLimit.trim() ? Number(draft.recursionLimit) : null;
  if (recursion !== null && (!Number.isSafeInteger(recursion) || recursion < 1 || recursion > 100_000)) {
    throw new Error("The step limit must be between 1 and 100,000.");
  }
  modelRequirement(draft.model);
  toolContracts(draft.tools);
  outputSchemaRequirement(draft.outputSchema, draft.outputMode);
  return recursion;
}

export function agentVersionFields(draft: AgentDraft, source?: AgentDefinition) {
  const recursion = validateAgentDraft(draft);
  const sourceConfig = source && plainObject(source.config) ? source.config : {};
  const sourceMetadata = source && plainObject(source.metadata) ? source.metadata : {};
  const previousIntent = plainObject(sourceConfig.studio_intent) ? sourceConfig.studio_intent : {};
  const config: Record<string, unknown> = {
    ...sourceConfig,
    studio_intent: {
      format: "rusty.agent-intent/v3",
      model: modelRequirement(draft.model),
      tools: toolContracts(draft.tools),
      memory: { access: draft.memoryAccess, scopes: draft.memoryAccess === "none" ? [] : draft.scopes },
      approval: draft.approval,
      output: { mode: draft.outputMode, schema: outputSchemaRequirement(draft.outputSchema, draft.outputMode) },
      budget: plainObject(previousIntent.budget) ? previousIntent.budget : { max_tokens: "", max_cost_usd: "", max_latency_ms: "" },
      binding: plainObject(previousIntent.binding) ? previousIntent.binding : { environment: "", surfaces: [] },
    },
  };
  if (recursion === null) delete config.recursion_limit;
  else config.recursion_limit = recursion;
  return {
    name: draft.name.trim(),
    graph: draft.graph,
    config,
    metadata: { ...sourceMetadata, description: draft.responsibility.trim(), audience: draft.audience.trim() },
  };
}

export function capabilitySummary(capability: Capability, draft: AgentDraft) {
  if (capability === "purpose") return draft.name || "Not set";
  if (capability === "model") return draft.model || "Deployment default";
  if (capability === "knowledge") return draft.memoryAccess === "none" ? "No memory" : `${draft.memoryAccess.replace("_", " ")} · ${draft.scopes.length}`;
  if (capability === "tools") return `${draft.tools.trim().split("\n").filter(Boolean).length} selected`;
  if (capability === "output") return draft.outputMode.replaceAll("_", " ");
  return draft.approval.replaceAll("_", " ");
}

export function capabilityValue(capability: Capability, draft: AgentDraft) {
  if (capability === "purpose") {
    return [draft.responsibility || "No responsibility", draft.audience ? `Audience: ${draft.audience}` : ""].filter(Boolean).join(" · ");
  }
  if (capability === "model") return draft.model || "Use the deployment model";
  if (capability === "knowledge") {
    if (draft.memoryAccess === "none") return "No memory access";
    return `${humanizeIdentifier(draft.memoryAccess)} · ${draft.scopes.map(humanizeIdentifier).join(", ") || "No scopes"}`;
  }
  if (capability === "tools") {
    const tools = toolContracts(draft.tools);
    return tools.length ? tools.map((tool) => `${humanizeIdentifier(tool.name)} (${humanizeIdentifier(tool.effect)})`).join(", ") : "No tools";
  }
  if (capability === "output") return draft.outputMode === "json_schema"
    ? `Named JSON · ${draft.outputSchema}` : humanizeIdentifier(draft.outputMode === "runtime_default" ? "deployment default" : draft.outputMode);
  const boundary = humanizeIdentifier(draft.approval === "runtime_policy" ? "deployment policy" : draft.approval);
  return `${boundary}${draft.recursionLimit ? ` · ${draft.recursionLimit} steps maximum` : " · deployment step limit"}`;
}

export function humanizeIdentifier(value: string) {
  return value.replaceAll("_", " ").replaceAll("-", " ").replace(/\b\w/g, (letter) => letter.toUpperCase());
}

function capabilityHeading(capability: Capability) {
  return ({ purpose: "What should this agent own?", model: "How should it reason?", knowledge: "What may it remember?", tools: "What actions may it take?", output: "What must it deliver?", guardrails: "Where should it stop and ask?" } as const)[capability];
}

export function AgentIntentEditor({ draft, onChange, graphs, initialCapability = "purpose", onCapabilityVisit }: {
  draft: AgentDraft;
  onChange: <K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) => void;
  graphs: string[];
  initialCapability?: Capability;
  onCapabilityVisit?: (capability: Capability) => void;
}) {
  const [active, setActive] = useState<Capability>(initialCapability);
  const tabs = useRef<Array<HTMLButtonElement | null>>([]);
  function selectCapability(capability: Capability, focus = false) {
    setActive(capability);
    onCapabilityVisit?.(capability);
    if (focus) requestAnimationFrame(() => tabs.current[capabilities.findIndex((item) => item.key === capability)]?.focus());
  }
  function onTabKey(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    let next = index;
    if (event.key === "ArrowDown" || event.key === "ArrowRight") next = (index + 1) % capabilities.length;
    else if (event.key === "ArrowUp" || event.key === "ArrowLeft") next = (index - 1 + capabilities.length) % capabilities.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = capabilities.length - 1;
    else return;
    event.preventDefault();
    selectCapability(capabilities[next].key, true);
  }
  return <div className={styles.intentEditor}>
    <div className={styles.capabilityMap} role="tablist" aria-label="Agent capabilities">
      {capabilities.map((capability, index) => <button key={capability.key} ref={(node) => { tabs.current[index] = node; }} type="button" role="tab" id={`agent-tab-${capability.key}`} aria-controls="agent-capability-panel" aria-selected={active === capability.key} tabIndex={active === capability.key ? 0 : -1} className={active === capability.key ? styles.activeCapability : ""} onClick={() => selectCapability(capability.key)} onKeyDown={(event) => onTabKey(event, index)}>
        <span>{String(index + 1).padStart(2, "0")}</span><b>{capability.label}</b><small>{capability.detail}</small>
      </button>)}
    </div>
    <section className={styles.editor} role="tabpanel" id="agent-capability-panel" aria-labelledby={`agent-tab-${active}`} tabIndex={0}>
      <span className="eyebrow">{capabilities.find((item) => item.key === active)?.label}</span>
      <h2>{capabilityHeading(active)}</h2>
      <CapabilityFields capability={active} draft={draft} update={onChange} graphs={graphs} />
    </section>
  </div>;
}

function CapabilityFields({ capability, draft, update, graphs }: { capability: Capability; draft: AgentDraft; update: <K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) => void; graphs: string[] }) {
  if (capability === "purpose") return <div className={styles.fields}><label>Name<input value={draft.name} onChange={(event) => update("name", event.target.value)} placeholder="Research analyst" /></label><label>Behavior<select value={draft.graph} onChange={(event) => update("graph", event.target.value)} disabled={!graphs.length}><option value="">{graphs.length ? "Choose a behavior" : "Available when workspace opens"}</option>{graphs.map((graph) => <option key={graph} value={graph}>{humanizeIdentifier(graph)}</option>)}</select></label><label className={styles.wide}>Responsibility<textarea rows={5} value={draft.responsibility} onChange={(event) => update("responsibility", event.target.value)} placeholder="Investigate a question, verify the sources, and return a concise answer." /></label><label>Audience<input value={draft.audience} onChange={(event) => update("audience", event.target.value)} placeholder="Product team" /></label></div>;
  if (capability === "model") return <div className={styles.fields}><label className={styles.wide}>Model requirement<input value={draft.model} onChange={(event) => update("model", event.target.value)} placeholder="Leave blank to use the deployment default" /></label><p className={styles.helper}>Choose a model identifier. Provider credentials remain in the deployment.</p></div>;
  if (capability === "knowledge") return <div className={styles.fields}><label>Memory access<select value={draft.memoryAccess} onChange={(event) => { const access = event.target.value as AgentDraft["memoryAccess"]; update("memoryAccess", access); if (access === "none") update("scopes", []); }}><option value="none">No memory</option><option value="read_only">Read only</option><option value="read_write">Read and write</option></select></label><fieldset className={styles.checks} disabled={draft.memoryAccess === "none"}><legend>Allowed scope</legend>{["run", "agent", "user", "team", "tenant"].map((scope) => <label key={scope}><input type="checkbox" checked={draft.scopes.includes(scope)} onChange={(event) => update("scopes", event.target.checked ? [...draft.scopes, scope] : draft.scopes.filter((item) => item !== scope))} />{scope}</label>)}</fieldset></div>;
  if (capability === "tools") return <ToolFields value={draft.tools} onChange={(value) => update("tools", value)} />;
  if (capability === "output") return <div className={styles.fields}><label>Required output<select value={draft.outputMode} onChange={(event) => { const mode = event.target.value as AgentDraft["outputMode"]; update("outputMode", mode); if (mode !== "json_schema") update("outputSchema", ""); }}><option value="runtime_default">Deployment default</option><option value="text">Text</option><option value="json_object">JSON object</option><option value="json_schema">Named JSON schema</option></select></label>{draft.outputMode === "json_schema" && <label>Schema binding<input value={draft.outputSchema} onChange={(event) => update("outputSchema", event.target.value)} placeholder="report.v1" /></label>}</div>;
  return <div className={styles.fields}><label>Approval boundary<select value={draft.approval} onChange={(event) => update("approval", event.target.value as AgentDraft["approval"])}><option value="runtime_policy">Deployment policy</option><option value="irreversible">Before irreversible actions</option><option value="external_effect">Before every external action</option></select></label><label>Maximum steps<input value={draft.recursionLimit} onChange={(event) => update("recursionLimit", event.target.value)} inputMode="numeric" placeholder="Deployment default" /></label></div>;
}

const toolEffects = ["pure", "read_only", "idempotent", "compensatable", "non_idempotent"] as const;

function ToolFields({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  const rows = value.trim() ? value.split("\n").map((line) => {
    const [name = "", effect = "read_only"] = line.split("|").map((part) => part.trim());
    return { name, effect: toolEffects.includes(effect as typeof toolEffects[number]) ? effect : "read_only" };
  }) : [];
  const commit = (next: Array<{ name: string; effect: string }>) => onChange(next.map((row) => `${row.name} | ${row.effect}`).join("\n"));
  return <div className={styles.toolEditor}>
    <div className={styles.toolHeading}><div><b>Allowed tools</b><p>Choose only the actions this agent needs.</p></div><button type="button" onClick={() => commit([...rows, { name: "", effect: "read_only" }])} disabled={rows.length >= 16}>Add tool</button></div>
    {rows.length === 0 ? <p className={styles.emptyTools}>No tools selected.</p> : <div className={styles.toolRows}>{rows.map((row, index) => <div className={styles.toolRow} key={index}>
      <label>Tool name<input value={row.name} onChange={(event) => commit(rows.map((item, rowIndex) => rowIndex === index ? { ...item, name: event.target.value } : item))} placeholder="search" /></label>
      <label>Effect<select value={row.effect} onChange={(event) => commit(rows.map((item, rowIndex) => rowIndex === index ? { ...item, effect: event.target.value } : item))}>{toolEffects.map((effect) => <option key={effect} value={effect}>{humanizeIdentifier(effect)}</option>)}</select></label>
      <button type="button" aria-label={`Remove ${row.name || `tool ${index + 1}`}`} onClick={() => commit(rows.filter((_, rowIndex) => rowIndex !== index))}>Remove</button>
    </div>)}</div>}
  </div>;
}
