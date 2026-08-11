import { type FormEvent, useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { connectionScope, createAssistant, jsonEquivalent, listAssistants, mutationScope, StudioApiError } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { useAgentMutationStore } from "../../state/agents";
import { evidencePreview, isUnicodeScalarString } from "../../lib/text";
import styles from "./AgentsPage.module.css";

const capabilities = [
  { key: "purpose", label: "Purpose", detail: "The job and who it serves" },
  { key: "model", label: "Model", detail: "How the agent reasons" },
  { key: "knowledge", label: "Knowledge", detail: "Memory and context" },
  { key: "tools", label: "Tools", detail: "Actions it can take" },
  { key: "output", label: "Output", detail: "What it delivers" },
  { key: "guardrails", label: "Guardrails", detail: "Limits and approval" },
] as const;

type Capability = (typeof capabilities)[number]["key"];

interface Draft {
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

const emptyDraft: Draft = {
  name: "", responsibility: "", audience: "", graph: "", model: "",
  memoryAccess: "none", scopes: [], tools: "", outputMode: "runtime_default",
  outputSchema: "", approval: "runtime_policy", recursionLimit: "",
};

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

export function outputSchemaRequirement(value: string, mode: Draft["outputMode"]) {
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

export function hasPortableIntent(draft: Draft) {
  return Boolean(draft.model || draft.tools.trim() || draft.memoryAccess !== "none"
    || draft.approval !== "runtime_policy" || draft.outputMode !== "runtime_default" || draft.outputSchema);
}

export function AgentsPage() {
  const queryClient = useQueryClient();
  const { connection, info, openDialog } = useConnectionStore();
  const [creating, setCreating] = useState(false);
  const [activeCapability, setActiveCapability] = useState<Capability>("purpose");
  const [visited, setVisited] = useState<Set<Capability>>(() => new Set(["purpose"]));
  const [draft, setDraft] = useState<Draft>(emptyDraft);
  const [error, setError] = useState("");
  const [assistantId, setAssistantId] = useState(() => crypto.randomUUID());
  const mutationState = useAgentMutationStore();
  const scope = connection ? connectionScope(connection) : "disconnected";
  const durableMutationScope = connection ? mutationScope(connection) : "disconnected";
  const uncertainty = mutationState.uncertainByConnection[durableMutationScope] ?? "";
  useEffect(() => {
    setCreating(false); setActiveCapability("purpose"); setVisited(new Set(["purpose"])); setDraft(emptyDraft);
    setAssistantId(crypto.randomUUID()); setError("");
  }, [durableMutationScope]);

  const queryKey = connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "assistants"] : ["assistants", "disconnected"];
  const catalog = useQuery({ queryKey, queryFn: () => listAssistants(connection!), enabled: Boolean(connection) });
  const create = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Connect Rusty before creating an agent.");
      if (!draft.name.trim() || !draft.responsibility.trim() || !draft.graph) throw new Error("Name, responsibility, and behavior are required.");
      if (draft.memoryAccess !== "none" && !draft.scopes.length) throw new Error("Choose what memory this agent may use.");
      if (!hasPortableIntent(draft)) throw new Error("Add at least one model, memory, tool, output, or approval requirement.");
      const recursion = draft.recursionLimit.trim() ? Number(draft.recursionLimit) : null;
      if (recursion !== null && (!Number.isSafeInteger(recursion) || recursion < 1 || recursion > 100_000)) throw new Error("The step limit must be between 1 and 100,000.");
      const config: Record<string, unknown> = {
        studio_intent: {
          format: "rusty.agent-intent/v3",
          model: modelRequirement(draft.model),
          tools: toolContracts(draft.tools),
          memory: { access: draft.memoryAccess, scopes: draft.memoryAccess === "none" ? [] : draft.scopes },
          approval: draft.approval,
          output: { mode: draft.outputMode, schema: outputSchemaRequirement(draft.outputSchema, draft.outputMode) },
          budget: { max_tokens: "", max_cost_usd: "", max_latency_ms: "" },
          binding: { environment: "", surfaces: [] },
        },
      };
      if (recursion !== null) config.recursion_limit = recursion;
      const input = {
        assistant_id: assistantId,
        name: draft.name.trim(),
        graph: draft.graph,
        config,
        metadata: { description: draft.responsibility.trim(), audience: draft.audience.trim() },
      };
      try {
        const assistant = await createAssistant(connection, input);
        return { assistant, initiatingScope: connectionScope(connection) };
      } catch (caught) {
        if (!(caught instanceof StudioApiError) || !caught.mayHaveCommitted) throw caught;
        try {
          const catalog = await listAssistants(connection);
          const assistant = catalog.find((item) => item.assistant_id === input.assistant_id);
          if (assistant && assistant.name === input.name && assistant.graph === input.graph
            && jsonEquivalent(assistant.config, input.config) && jsonEquivalent(assistant.metadata, input.metadata)) {
            return { assistant, initiatingScope: connectionScope(connection) };
          }
        } catch { /* retain the uncertainty lock below */ }
        mutationState.markUncertain(mutationScope(connection), "Rusty may have created this agent, but Studio could not prove the result. Check the agent library before allowing another create attempt.");
        throw new StudioApiError("The create result is uncertain. Studio locked retry to avoid a duplicate agent.", caught.status, true);
      }
    },
    onSuccess: ({ assistant, initiatingScope }) => {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== initiatingScope) return;
      mutationState.clearUncertain(mutationScope(current));
      queryClient.setQueryData(queryKey, (current: unknown) => Array.isArray(current) ? [...current, assistant] : [assistant]);
      setDraft(emptyDraft);
      setAssistantId(crypto.randomUUID());
      setCreating(false);
      setActiveCapability("purpose");
      setVisited(new Set(["purpose"]));
      setError("");
    },
    onError: (caught) => {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scope) return;
      setError(caught instanceof StudioApiError || caught instanceof Error ? caught.message : "The agent could not be created.");
    },
  });

  const progress = useMemo(() => ({
    purpose: Boolean(draft.name.trim() && draft.responsibility.trim() && draft.graph),
    model: visited.has("model"),
    knowledge: visited.has("knowledge") && (draft.memoryAccess === "none" || draft.scopes.length > 0),
    tools: visited.has("tools"),
    output: visited.has("output") && (draft.outputMode !== "json_schema" || Boolean(draft.outputSchema.trim())),
    guardrails: visited.has("guardrails"),
  }), [draft, visited]);
  const completed = Object.values(progress).filter(Boolean).length;
  const intentReady = hasPortableIntent(draft);

  function update<K extends keyof Draft>(key: K, value: Draft[K]) { setDraft((current) => ({ ...current, [key]: value })); setError(""); }
  function submit(event: FormEvent) { event.preventDefault(); if (uncertainty) return; setError(""); create.mutate(); }

  return (
    <section className="page" aria-labelledby="agents-heading">
      <header className="page-header">
        <div><span className="eyebrow">Agents</span><h1 id="agents-heading">Build a worker you can trust</h1><p>Give it a clear job, connect only the capabilities it needs, and test it before it joins real work.</p></div>
        {connection && <div className={styles.headerActions}><Link className="secondary-button" to="/agents/prompts">Prompt library</Link>{(creating || Boolean(catalog.data?.length)) && <button className="primary-button" type="button" onClick={() => setCreating((value) => !value)} aria-expanded={creating} aria-controls="agent-builder">{creating ? "Close builder" : "Create agent"}</button>}</div>}
      </header>

      {!connection ? (
        <div className="empty-state"><span className="eyebrow">Your agent library</span><h2>Connect Rusty to load agents</h2><p>Your deployment stays the source of truth. Studio will show each agent’s purpose, active version, recent work, and next safe action.</p><button className="primary-button" type="button" onClick={openDialog}>Connect Rusty</button></div>
      ) : creating ? (
        <form className={styles.builder} id="agent-builder" onSubmit={submit}>
          <nav className={styles.capabilityMap} aria-label="Agent capability steps">
            <div className={styles.agentCore}><span>Agent</span><strong>{draft.name.trim() || "New worker"}</strong><small>{completed} of 6 decisions shaped</small></div>
            {capabilities.map((capability, index) => (
              <button key={capability.key} type="button" className={activeCapability === capability.key ? styles.activeCapability : ""} onClick={() => { setActiveCapability(capability.key); setVisited((current) => new Set(current).add(capability.key)); }} aria-current={activeCapability === capability.key ? "step" : undefined}>
                <span>{progress[capability.key] ? "✓" : String(index + 1).padStart(2, "0")}</span><b>{capability.label}</b><small>{capability.detail}</small>
              </button>
            ))}
          </nav>
          <section className={styles.editor} aria-labelledby="capability-heading">
            <span className="eyebrow">{capabilities.find((item) => item.key === activeCapability)?.label}</span>
            <h2 id="capability-heading">{capabilityHeading(activeCapability)}</h2>
            <CapabilityEditor capability={activeCapability} draft={draft} update={update} graphs={info?.graphs.map((graph) => graph.name) ?? []} />
          </section>
          <aside className={styles.review}>
            <span className="eyebrow">Live review</span><h2>Agent shape</h2>
            <dl>{capabilities.map((item) => <div key={item.key}><dt>{item.label}</dt><dd>{progress[item.key] ? summary(item.key, draft) : "Not set"}</dd></div>)}</dl>
            {error && <p className={styles.error} role="alert">{error}</p>}
            {uncertainty && <div className={styles.error} role="alert"><p>{uncertainty}</p><button type="button" onClick={() => { mutationState.clearUncertain(durableMutationScope); setError(""); }}>I checked the server — allow retry</button></div>}
            <button className="primary-button" type="submit" disabled={create.isPending || Boolean(uncertainty) || completed < capabilities.length || !intentReady}>{create.isPending ? "Creating…" : uncertainty ? "Create locked" : completed < capabilities.length ? `Review ${capabilities.length - completed} more` : !intentReady ? "Add one capability" : "Create agent"}</button>
            <p className={styles.boundary}>These are portable requirements stored with the agent. Enforcement depends on the selected behavior and deployment policies, so test the agent before relying on them. Nothing begins running yet.</p>
          </aside>
        </form>
      ) : catalog.isLoading ? (
        <div className={styles.loading} aria-live="polite">Loading agents…</div>
      ) : catalog.isError ? (
        <div className="empty-state"><span className="eyebrow">Agents unavailable</span><h2>The agent library could not be loaded</h2><p>{catalog.error instanceof Error ? catalog.error.message : "Try the request again."}</p><button className="primary-button" type="button" onClick={() => catalog.refetch()}>Retry</button></div>
      ) : catalog.data?.length ? (
        <div className={styles.library}>{catalog.data.map((agent) => <article key={agent.assistant_id} className={styles.agentCard}><header><span className={agent.archived_at ? styles.archived : styles.live}>{agent.archived_at ? "Archived" : "Ready"}</span><code>{agent.active_version_id.slice(0, 12)}</code></header><h2>{evidencePreview(agent.name, 256)}</h2><p>{typeof agent.metadata === "object" && agent.metadata && "description" in agent.metadata ? evidencePreview(String(agent.metadata.description), 500) : "No purpose has been added."}</p><footer><span>{evidencePreview(agent.graph, 256)}</span><div><b>{agent.version_count} version{agent.version_count === 1 ? "" : "s"}</b><a href={`/advanced/legacy?studio=agents&agent=${encodeURIComponent(agent.assistant_id)}`}>Manage</a></div></footer></article>)}</div>
      ) : (
        <div className="empty-state"><span className="eyebrow">Your agent library</span><h2>Create your first worker</h2><p>Start with one clear responsibility. Model, knowledge, tools, output, and guardrails stay in one reviewable capability map.</p><button className="primary-button" type="button" onClick={() => setCreating(true)}>Create agent</button></div>
      )}
    </section>
  );
}

function capabilityHeading(capability: Capability) {
  return ({ purpose: "What should this agent own?", model: "How should it reason?", knowledge: "What may it remember?", tools: "What actions may it take?", output: "What must it deliver?", guardrails: "Where should it stop and ask?" } as const)[capability];
}

function summary(capability: Capability, draft: Draft) {
  if (capability === "purpose") return draft.name || "Shaped";
  if (capability === "model") return draft.model || "Deployment default";
  if (capability === "knowledge") return draft.memoryAccess === "none" ? "No memory" : `${draft.memoryAccess.replace("_", " ")} · ${draft.scopes.length}`;
  if (capability === "tools") return `${draft.tools.trim().split("\n").filter(Boolean).length} selected`;
  if (capability === "output") return draft.outputMode.replaceAll("_", " ");
  return draft.approval.replaceAll("_", " ");
}

function CapabilityEditor({ capability, draft, update, graphs }: { capability: Capability; draft: Draft; update: <K extends keyof Draft>(key: K, value: Draft[K]) => void; graphs: string[] }) {
  if (capability === "purpose") return <div className={styles.fields}><label>Name<input value={draft.name} onChange={(event) => update("name", event.target.value)} placeholder="Research analyst" /></label><label>Behavior<select value={draft.graph} onChange={(event) => update("graph", event.target.value)}><option value="">Choose a behavior</option>{graphs.map((graph) => <option key={graph}>{graph}</option>)}</select></label><label className={styles.wide}>Responsibility<textarea rows={5} value={draft.responsibility} onChange={(event) => update("responsibility", event.target.value)} placeholder="Investigate a question, verify the sources, and return a concise answer." /></label><label>Audience<input value={draft.audience} onChange={(event) => update("audience", event.target.value)} placeholder="Product team" /></label></div>;
  if (capability === "model") return <div className={styles.fields}><label className={styles.wide}>Model requirement<input value={draft.model} onChange={(event) => update("model", event.target.value)} placeholder="Leave blank to use the deployment default" /></label><p className={styles.helper}>This is a requirement, not a credential. Provider secrets stay in the deployment.</p></div>;
  if (capability === "knowledge") return <div className={styles.fields}><label>Memory access<select value={draft.memoryAccess} onChange={(event) => { const access = event.target.value as Draft["memoryAccess"]; update("memoryAccess", access); if (access === "none") update("scopes", []); }}><option value="none">No memory</option><option value="read_only">Read only</option><option value="read_write">Read and write</option></select></label><fieldset className={styles.checks} disabled={draft.memoryAccess === "none"}><legend>Allowed scope</legend>{["run", "agent", "user", "team", "tenant"].map((scope) => <label key={scope}><input type="checkbox" checked={draft.scopes.includes(scope)} onChange={(event) => update("scopes", event.target.checked ? [...draft.scopes, scope] : draft.scopes.filter((item) => item !== scope))} />{scope}</label>)}</fieldset></div>;
  if (capability === "tools") return <div className={styles.fields}><label className={styles.wide}>Tool contracts<textarea rows={7} value={draft.tools} onChange={(event) => update("tools", event.target.value)} placeholder={"search | read_only\npublish_report | idempotent"} /></label><p className={styles.helper}>Declare only the effects this agent needs. One contract per line.</p></div>;
  if (capability === "output") return <div className={styles.fields}><label>Required output<select value={draft.outputMode} onChange={(event) => { const mode = event.target.value as Draft["outputMode"]; update("outputMode", mode); if (mode !== "json_schema") update("outputSchema", ""); }}><option value="runtime_default">Deployment default</option><option value="text">Text</option><option value="json_object">JSON object</option><option value="json_schema">Named JSON schema</option></select></label>{draft.outputMode === "json_schema" && <label>Schema binding<input value={draft.outputSchema} onChange={(event) => update("outputSchema", event.target.value)} placeholder="report.v1" /></label>}</div>;
  return <div className={styles.fields}><label>Approval boundary<select value={draft.approval} onChange={(event) => update("approval", event.target.value as Draft["approval"])}><option value="runtime_policy">Deployment policy</option><option value="irreversible">Before irreversible actions</option><option value="external_effect">Before every external action</option></select></label><label>Maximum steps<input value={draft.recursionLimit} onChange={(event) => update("recursionLimit", event.target.value)} inputMode="numeric" placeholder="Deployment default" /></label></div>;
}
