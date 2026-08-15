import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import type { Assistant, InfoGraph, ToolCapability } from "../../lib/contracts";
import { isUnicodeScalarString } from "../../lib/text";
import styles from "./AgentsPage.module.css";

export const capabilities = [
  { key: "purpose", label: "Purpose", detail: "The job and who it serves" },
  { key: "goals", label: "Goals", detail: "What good looks like" },
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
  goals: string;
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
    name: "", responsibility: "", audience: "", goals: "", graph: "", model: "",
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
    || ("audience" in agent.metadata && typeof agent.metadata.audience !== "string")
    || ("goals" in agent.metadata && typeof agent.metadata.goals !== "string")) return false;
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
    goals: stringValue(metadata.goals),
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

export function validateAgentDraft(draft: AgentDraft) {
  if (!draft.name.trim() || !draft.responsibility.trim() || !draft.graph) throw new Error("Name, responsibility, and behavior are required.");
  if (draft.memoryAccess !== "none" && !draft.scopes.length) throw new Error("Choose what memory this agent may use.");
  const recursion = draft.recursionLimit.trim() ? Number(draft.recursionLimit) : null;
  if (recursion !== null && (!Number.isSafeInteger(recursion) || recursion < 1 || recursion > 100_000)) {
    throw new Error("The step limit must be between 1 and 100,000.");
  }
  modelRequirement(draft.model);
  toolContracts(draft.tools);
  outputSchemaRequirement(draft.outputSchema, draft.outputMode);
  validateGoalNotes(draft.goals);
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
      memory: { access: draft.memoryAccess, scopes: draft.memoryAccess === "none" ? [] : memoryScopes.filter((scope) => draft.scopes.includes(scope)) },
      approval: draft.approval,
      output: { mode: draft.outputMode, schema: outputSchemaRequirement(draft.outputSchema, draft.outputMode) },
      budget: plainObject(previousIntent.budget) ? previousIntent.budget : { max_tokens: "", max_cost_usd: "", max_latency_ms: "" },
      binding: plainObject(previousIntent.binding) ? previousIntent.binding : { environment: "", surfaces: [] },
    },
  };
  if (recursion === null) delete config.recursion_limit;
  else config.recursion_limit = recursion;
  const metadata: Record<string, unknown> = { ...sourceMetadata, description: draft.responsibility.trim(), audience: draft.audience.trim() };
  if (draft.goals.trim() || "goals" in sourceMetadata) metadata.goals = draft.goals.trim();
  return {
    name: draft.name.trim(),
    graph: draft.graph,
    config,
    metadata,
  };
}

export function capabilitySummary(capability: Capability, draft: AgentDraft) {
  if (capability === "purpose") return draft.name || "Not set";
  if (capability === "goals") return draft.goals || draft.audience || "Not set";
  if (capability === "model") return draft.model || "Deployment default";
  if (capability === "knowledge") return draft.memoryAccess === "none" ? "No memory" : `${draft.memoryAccess.replace("_", " ")} · ${draft.scopes.length}`;
  if (capability === "tools") return `${draft.tools.trim().split("\n").filter(Boolean).length} selected`;
  if (capability === "output") return draft.outputMode.replaceAll("_", " ");
  return draft.approval.replaceAll("_", " ");
}

export function capabilityValue(capability: Capability, draft: AgentDraft) {
  if (capability === "purpose") {
    return draft.responsibility || "No responsibility";
  }
  if (capability === "goals") return [draft.goals || "No success criteria", draft.audience ? `Audience: ${draft.audience}` : ""].filter(Boolean).join(" · ");
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
  return ({ purpose: "What should this agent own?", goals: "What does good look like?", model: "How should it reason?", knowledge: "What may it know and remember?", tools: "What actions may it take?", output: "What must it deliver?", guardrails: "Where should it stop and ask?" } as const)[capability];
}

export function AgentIntentEditor({ draft, onChange, graphs, initialCapability = "purpose", progress, validationRequest, validationMessage, onCapabilityVisit }: {
  draft: AgentDraft;
  onChange: <K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) => void;
  graphs: InfoGraph[];
  initialCapability?: Capability;
  progress?: Record<Capability, boolean>;
  validationRequest?: { capability: Capability; nonce: number } | null;
  validationMessage?: string;
  onCapabilityVisit?: (capability: Capability) => void;
}) {
  const [active, setActive] = useState<Capability>(initialCapability);
  const tabs = useRef<Array<HTMLButtonElement | null>>([]);
  function selectCapability(capability: Capability, focus = false) {
    setActive(capability);
    onCapabilityVisit?.(capability);
    if (focus) requestAnimationFrame(() => tabs.current[capabilities.findIndex((item) => item.key === capability)]?.focus());
  }
  useEffect(() => {
    if (validationRequest) selectCapability(validationRequest.capability, true);
    // Each nonce represents a deliberate request to revisit the invalid control group.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [validationRequest]);
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
  const activeIndex = capabilities.findIndex((item) => item.key === active);
  const next = capabilities[activeIndex + 1];
  return <div className={styles.intentEditor}>
    <div className={styles.capabilityMap} role="tablist" aria-label="Agent capabilities">
      {capabilities.map((capability, index) => <button key={capability.key} ref={(node) => { tabs.current[index] = node; }} type="button" role="tab" id={`agent-tab-${capability.key}`} aria-controls="agent-capability-panel" aria-selected={active === capability.key} tabIndex={active === capability.key ? 0 : -1} className={active === capability.key ? styles.activeCapability : ""} onClick={() => selectCapability(capability.key)} onKeyDown={(event) => onTabKey(event, index)}>
        <span>{progress?.[capability.key] ? "✓" : String(index + 1).padStart(2, "0")}</span><b>{capability.label}</b><small>{capability.detail}</small>
      </button>)}
    </div>
    <section className={styles.editor} role="tabpanel" id="agent-capability-panel" aria-labelledby={`agent-tab-${active}`} tabIndex={0}>
      <h2>{capabilityHeading(active)}</h2>
      <p className={styles.editorLead}>{capabilities[activeIndex].detail}.</p>
      {validationRequest && validationRequest.capability === active && validationMessage && <p className={styles.editorError} role="alert">{validationMessage}</p>}
      <CapabilityFields capability={active} draft={draft} update={onChange} graphs={graphs} />
      <div className={styles.editorNext}>{activeIndex > 0 ? <button type="button" onClick={() => selectCapability(capabilities[activeIndex - 1].key, true)}><span aria-hidden="true">←</span> Back</button> : <span />}{next && <button type="button" onClick={() => selectCapability(next.key, true)}>Next: {next.label} <span aria-hidden="true">→</span></button>}</div>
    </section>
  </div>;
}

function CapabilityFields({ capability, draft, update, graphs }: { capability: Capability; draft: AgentDraft; update: <K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) => void; graphs: InfoGraph[] }) {
  const graph = graphs.find((item) => item.name === draft.graph);
  if (capability === "purpose") return <div className={styles.fields}><label>Name<input value={draft.name} onChange={(event) => update("name", event.target.value)} placeholder="Receipt Chaser" /></label><label>Behavior<select value={draft.graph} onChange={(event) => {
    const selected = graphs.find((item) => item.name === event.target.value);
    update("graph", event.target.value);
    update("tools", selected?.tools ? toolContractText(selected.tools) : "");
  }} disabled={!graphs.length}><option value="">{graphs.length ? "Choose a behavior" : "Available when workspace opens"}</option>{graphs.map((item) => <option key={item.name} value={item.name}>{humanizeIdentifier(item.name)}</option>)}</select></label><label className={styles.wide}>Responsibility<textarea rows={2} value={draft.responsibility} onChange={(event) => update("responsibility", event.target.value)} placeholder="Chase missing receipts and give each owner a clear deadline." /></label><label className={styles.wide}>Audience<input value={draft.audience} onChange={(event) => update("audience", event.target.value)} placeholder="Finance operations, accounting" /><small>Who this agent serves. Use the names your team already knows.</small></label></div>;
  if (capability === "goals") return <GoalFields value={draft.goals} onChange={(value) => update("goals", value)} />;
  if (capability === "model") return <div className={styles.choiceStack}><button aria-pressed={!draft.model} className={!draft.model ? styles.selectedChoice : ""} type="button" onClick={() => update("model", "")}><i /><span><b>Deployment default</b><small>Follow the model chosen when this agent runs</small></span></button><button aria-pressed={Boolean(draft.model)} className={draft.model ? styles.selectedChoice : ""} type="button" onClick={() => document.getElementById("agent-model-requirement")?.focus()}><i /><span><b>Require a model</b><small>Ask the deployment to resolve this exact identifier</small></span></button><label className={styles.inlineRequirement}>Model requirement<input id="agent-model-requirement" value={draft.model} onChange={(event) => update("model", event.target.value)} placeholder="provider/model" /></label><p className={styles.choiceNote}>This records a deployment requirement, not a verified catalog choice. Credentials never belong here.</p></div>;
  if (capability === "knowledge") return <div className={styles.knowledgeStack}><div className={styles.choiceStack}>{([ ["none", "No memory", "Work only from this run"], ["read_only", "Read memory", "Use approved context without changing it"], ["read_write", "Read and write", "Use context and add to its own history"] ] as const).map(([access, label, detail]) => <button key={access} aria-pressed={draft.memoryAccess === access} className={draft.memoryAccess === access ? styles.selectedChoice : ""} type="button" onClick={() => { update("memoryAccess", access); if (access === "none") update("scopes", []); }}><i /><span><b>{label}</b><small>{detail}</small></span></button>)}</div>{draft.memoryAccess !== "none" && <fieldset className={styles.scopeLayers}><legend>Allowed scope</legend>{memoryScopes.map((scope, index) => <label key={scope} style={{ marginLeft: `${index * 7}px` }}><input type="checkbox" checked={draft.scopes.includes(scope)} onChange={(event) => update("scopes", memoryScopes.filter((item) => event.target.checked ? item === scope || draft.scopes.includes(item) : item !== scope && draft.scopes.includes(item)))} /><span>{humanizeIdentifier(scope)}</span><small>{index < 2 ? "local context" : "wider inherited context"}</small></label>)}</fieldset>}</div>;
  if (capability === "tools") return <ToolFields value={draft.tools} graph={graph} />;
  if (capability === "output") return <div className={styles.choiceStack}>{([ ["runtime_default", "Deployment default", "Use the workspace output policy"], ["text", "Text", "Return a human-readable response"], ["json_object", "JSON object", "Return structured JSON"], ["json_schema", "Named JSON schema", "Require a schema identifier at deployment"] ] as const).map(([mode, label, detail]) => <button key={mode} aria-pressed={draft.outputMode === mode} className={draft.outputMode === mode ? styles.selectedChoice : ""} type="button" onClick={() => { update("outputMode", mode); if (mode !== "json_schema") update("outputSchema", ""); }}><i /><span><b>{label}</b><small>{detail}</small></span></button>)}{draft.outputMode === "json_schema" && <><label className={styles.inlineRequirement}>Required schema identifier<input value={draft.outputSchema} onChange={(event) => update("outputSchema", event.target.value)} placeholder="report.v1" /></label><p className={styles.choiceNote}>Studio records the requirement here; the deployment must resolve it before use.</p></>}</div>;
  return <div className={styles.fields}><label>Approval boundary<select value={draft.approval} onChange={(event) => update("approval", event.target.value as AgentDraft["approval"])}><option value="runtime_policy">Deployment policy</option><option value="irreversible">Before irreversible actions</option><option value="external_effect">Before every external action</option></select></label><label>Maximum steps<input value={draft.recursionLimit} onChange={(event) => update("recursionLimit", event.target.value)} inputMode="numeric" placeholder="Deployment default" /></label><p className={styles.helper}>A hard stop keeps the agent bounded. Leave the step limit blank to use the deployment default.</p></div>;
}

const goalPresets = [
  { name: "Task success rate", hint: "runs that achieve the intended outcome", direction: "≥", target: "90", unit: "%" },
  { name: "Median latency", hint: "time from start to terminal result", direction: "≤", target: "5", unit: "s" },
  { name: "Cost per successful run", hint: "average model and tool cost", direction: "≤", target: "0.10", unit: "USD" },
] as const;

const memoryScopes = ["run", "agent", "user", "team", "tenant"] as const;

function presetGoalTarget(line: string, preset: typeof goalPresets[number]) {
  const prefix = `${preset.name} ${preset.direction} `;
  const suffix = ` ${preset.unit}`;
  if (!line.startsWith(prefix) || !line.endsWith(suffix)) return null;
  const target = line.slice(prefix.length, -suffix.length);
  return !/\s/.test(target) ? target : null;
}

function validateGoalNotes(value: string) {
  for (const line of value.split("\n").map((item) => item.trim()).filter(Boolean)) {
    for (const preset of goalPresets) {
      const prefix = `${preset.name} ${preset.direction} `;
      const suffix = ` ${preset.unit}`;
      if (!line.startsWith(prefix) || !line.endsWith(suffix)) continue;
      const target = line.slice(prefix.length, -suffix.length);
      const number = Number(target);
      if (!target || !Number.isFinite(number) || number < 0 || (preset.unit === "%" && number > 100)) {
        throw new Error(`${preset.name} needs a valid numeric target${preset.unit === "%" ? " from 0 to 100" : ""}.`);
      }
    }
  }
}

function GoalFields({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  const lines = value.split("\n").map((line) => line.trim()).filter(Boolean);
  const custom = lines.filter((line) => !goalPresets.some((preset) => presetGoalTarget(line, preset) !== null));
  const commit = (preset: typeof goalPresets[number], enabled: boolean, target: string = preset.target) => {
    const remaining = lines.filter((line) => presetGoalTarget(line, preset) === null);
    onChange([...remaining, ...(enabled ? [`${preset.name} ${preset.direction} ${target} ${preset.unit}`] : [])].join("\n"));
  };
  return <div className={styles.goalRows}><p className={styles.goalBoundary}>Success notes travel with this version. Bind executable evaluators in Run &amp; Evaluate.</p>{goalPresets.map((preset) => {
    const current = lines.find((line) => presetGoalTarget(line, preset) !== null);
    const target = current ? presetGoalTarget(current, preset) ?? preset.target : preset.target;
    return <div key={preset.name} className={current ? styles.goalActive : styles.goalRow}><span><b>{preset.name}</b><small>{preset.hint}</small></span><label><span>{preset.direction}</span><input type="number" min="0" max={preset.unit === "%" ? "100" : undefined} step="any" value={target} disabled={!current} onChange={(event) => commit(preset, true, event.target.value)} aria-label={`${preset.name} target`} /></label><code>{preset.unit}</code><button type="button" aria-pressed={Boolean(current)} aria-label={`${current ? "Remove" : "Add"} ${preset.name}`} onClick={() => commit(preset, !current)}>{current ? "✓" : "+"}</button></div>;
  })}{custom.map((line) => <div className={styles.goalActive} key={line}><span><b>{line}</b><small>Goal retained from this definition</small></span><button type="button" aria-label={`Remove ${line}`} onClick={() => onChange(lines.filter((item) => item !== line).join("\n"))}>×</button></div>)}<label className={styles.customGoal}>Custom goal<input placeholder="Describe another measurable outcome" onKeyDown={(event) => { if (event.key === "Enter" && event.currentTarget.value.trim()) { event.preventDefault(); onChange([...lines, event.currentTarget.value.trim()].join("\n")); event.currentTarget.value = ""; } }} /><small>Press Enter to add it to this version.</small></label></div>;
}

function toolContractText(tools: ToolCapability[]) {
  return tools.map((tool) => `${tool.name} | ${tool.effect}`).join("\n");
}

function ToolFields({ value, graph }: { value: string; graph?: InfoGraph }) {
  const advertised = graph?.tools;
  const configured = (() => { try { return toolContracts(value); } catch { return []; } })();
  const catalogMatches = advertised !== undefined && toolContractText(advertised) === value;
  return <div className={styles.toolEditor}>
    <div className={styles.toolHeading}><div><b>Executable tools</b><p>{graph ? "Included by this behavior and enforced by its runtime graph." : "Choose a behavior to see what it can execute."}</p></div>{advertised && <span>{advertised.length} available</span>}</div>
    {!graph ? <p className={styles.emptyTools}>Choose a behavior first.</p>
      : advertised === undefined ? <div className={styles.emptyTools}><b>Tool catalog unavailable</b><p>This server does not advertise executable tools for this behavior.</p>{configured.length > 0 && <small>{configured.length} stored requirement{configured.length === 1 ? "" : "s"} remain unchanged.</small>}</div>
      : advertised.length === 0 ? <div className={styles.emptyTools}><b>Tool-free behavior</b><p>This graph answers without callable tools.</p></div>
      : <div className={styles.toolRows}>{advertised.map((tool) => <article className={styles.toolCapability} key={tool.name}>
        <div><b>{humanizeIdentifier(tool.name)}</b><code>{tool.name}</code></div><p>{tool.description}</p><dl><div><dt>Effect</dt><dd>{humanizeIdentifier(tool.effect)}</dd></div><div><dt>Inputs</dt><dd>{toolInputSummary(tool)}</dd></div></dl>
      </article>)}</div>}
    {advertised && !catalogMatches && configured.length > 0 && <p className={styles.toolMismatch} role="status">This saved draft names a different tool set. Choose the behavior again to adopt its current executable catalog.</p>}
  </div>;
}

function toolInputSummary(tool: ToolCapability) {
  const properties = tool.parameters_schema.properties;
  if (!properties || typeof properties !== "object" || Array.isArray(properties)) return "Structured input";
  const names = Object.keys(properties);
  return names.length ? names.map(humanizeIdentifier).join(", ") : "No input";
}
