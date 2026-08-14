import { type FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useBlocker, useNavigate } from "@tanstack/react-router";
import { connectionScope, createAssistant, jsonEquivalent, listAssistants, mutationScope, StudioApiError } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { useAgentMutationStore } from "../../state/agents";
import { evidencePreview } from "../../lib/text";
import {
  agentVersionFields,
  capabilities,
  capabilitySummary,
  emptyAgentDraft,
  hasPortableIntent,
  humanizeIdentifier,
  type AgentDraft,
  type Capability,
  AgentIntentEditor,
} from "./AgentIntentEditor";
import styles from "./AgentsPage.module.css";
import { UnsavedChangesDialog } from "./UnsavedChangesDialog";

export function AgentsPage() {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { connection, info, openDialog } = useConnectionStore();
  const [creating, setCreating] = useState(false);
  const [visited, setVisited] = useState<Set<Capability>>(() => new Set(["purpose"]));
  const [draft, setDraft] = useState<AgentDraft>(() => emptyAgentDraft());
  const [error, setError] = useState("");
  const [assistantId, setAssistantId] = useState(() => crypto.randomUUID());
  const allowNavigation = useRef(false);
  const previousWorkspace = useRef("disconnected");
  const mutationState = useAgentMutationStore();
  const scope = connection ? connectionScope(connection) : "disconnected";
  const durableMutationScope = connection ? mutationScope(connection) : "disconnected";
  const uncertainty = mutationState.uncertainByConnection[durableMutationScope] ?? "";
  const builderChanged = !jsonEquivalent(draft, emptyAgentDraft());
  const routeBlocker = useBlocker({
    shouldBlockFn: () => builderChanged && !allowNavigation.current,
    enableBeforeUnload: () => builderChanged,
    withResolver: true,
  });
  useEffect(() => {
    const previous = previousWorkspace.current;
    previousWorkspace.current = durableMutationScope;
    if (previous === durableMutationScope || (previous === "disconnected" && durableMutationScope !== "disconnected")) return;
    setCreating(false); setVisited(new Set(["purpose"])); setDraft(emptyAgentDraft());
    setAssistantId(crypto.randomUUID()); setError("");
  }, [durableMutationScope]);

  const queryKey = connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "assistants"] : ["assistants", "disconnected"];
  const catalog = useQuery({ queryKey, queryFn: () => listAssistants(connection!), enabled: Boolean(connection) });
  const create = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Open a workspace before creating an agent.");
      const fields = agentVersionFields(draft);
      const input = {
        assistant_id: assistantId,
        ...fields,
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
      setDraft(emptyAgentDraft());
      setAssistantId(crypto.randomUUID());
      setCreating(false);
      setVisited(new Set(["purpose"]));
      setError("");
      allowNavigation.current = true;
      navigate({ to: "/agents/$assistantId", params: { assistantId: assistant.assistant_id } });
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

  function update<K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) { setDraft((current) => ({ ...current, [key]: value })); setError(""); }
  function submit(event: FormEvent) {
    event.preventDefault();
    if (!connection) { openDialog(); return; }
    if (uncertainty) return;
    setError("");
    create.mutate();
  }

  return (
    <section className="page" aria-labelledby="agents-heading">
      <header className={creating ? `page-header ${styles.compactHeader}` : styles.forgeHeader}>
        <div className={styles.forgeCopy}>
          <span className="eyebrow">The Forge</span>
          <h1 id="agents-heading">Build an agent that <span>earns trust.</span></h1>
          <p>Give it one clear responsibility. Shape its capabilities, test the real behavior, then activate a version you can account for.</p>
          <div className={styles.headerActions}>{connection ? <Link className="secondary-button" to="/agents/prompts">Prompt library</Link> : <button className="secondary-button" type="button" onClick={openDialog}>Choose workspace</button>}{(creating || Boolean(catalog.data?.length)) && <button className="primary-button" type="button" onClick={() => setCreating((value) => !value)} aria-expanded={creating} aria-controls="agent-builder">{creating ? "Close builder" : "Create agent"}</button>}</div>
        </div>
        {!creating && <AgentOrbit />}
      </header>
      {routeBlocker.status === "blocked" && <UnsavedChangesDialog onKeep={routeBlocker.reset} onDiscard={() => { setDraft(emptyAgentDraft()); setVisited(new Set(["purpose"])); setError(""); routeBlocker.proceed(); }} />}

      {creating ? (
        <form className={styles.builder} id="agent-builder" onSubmit={submit}>
          <AgentIntentEditor draft={draft} onChange={update} graphs={info?.graphs.map((graph) => graph.name) ?? []} onCapabilityVisit={(capability) => setVisited((current) => new Set(current).add(capability))} />
          <aside className={styles.review}>
            <span className="eyebrow">Live review</span><h2>Agent shape</h2>
            <dl>{capabilities.map((item) => <div key={item.key}><dt>{item.label}</dt><dd>{progress[item.key] ? capabilitySummary(item.key, draft) : "Not set"}</dd></div>)}</dl>
            {error && <p className={styles.error} role="alert">{error}</p>}
            {uncertainty && <div className={styles.error} role="alert"><p>{uncertainty}</p><button type="button" onClick={() => { mutationState.clearUncertain(durableMutationScope); setError(""); }}>I checked the server — allow retry</button></div>}
            <button className="primary-button" type="submit" disabled={create.isPending || Boolean(uncertainty) || completed < capabilities.length || !intentReady}>{create.isPending ? "Creating…" : uncertainty ? "Create locked" : completed < capabilities.length ? `Review ${capabilities.length - completed} more` : !intentReady ? "Add one capability" : !connection ? "Choose workspace to save" : "Create agent"}</button>
            <p className={styles.boundary}>{connection ? "These are portable requirements stored with the agent. Enforcement depends on the selected behavior and deployment policies, so test the agent before relying on them. Nothing begins running yet." : "This draft stays on this page. Open a workspace to choose an available behavior, save the definition, and test it."}</p>
          </aside>
        </form>
      ) : !connection ? (
        <div className="empty-state"><span className="eyebrow">Start here</span><h2>Design your first agent now</h2><p>Shape an unsaved draft without setup. Choose a workspace when you are ready to save, run, and share it.</p><button className="primary-button" type="button" onClick={() => setCreating(true)}>Start a draft</button></div>
      ) : catalog.isLoading ? (
        <div className={styles.loading} aria-live="polite">Loading agents…</div>
      ) : catalog.isError ? (
        <div className="empty-state"><span className="eyebrow">Agents unavailable</span><h2>The agent library could not be loaded</h2><p>{catalog.error instanceof Error ? catalog.error.message : "Try the request again."}</p><button className="primary-button" type="button" onClick={() => catalog.refetch()}>Retry</button></div>
      ) : catalog.data?.length ? (
        <section className={styles.librarySection} aria-labelledby="agent-library-heading">
          <header><div><span className="eyebrow">Serving definitions</span><h2 id="agent-library-heading">Your agents</h2></div><span>{catalog.data.length} definition{catalog.data.length === 1 ? "" : "s"}</span></header>
          <div className={styles.library}>{catalog.data.map((agent) => { const available = Boolean(info?.graphs.some((graph) => graph.name === agent.graph)); return <article key={agent.assistant_id} className={styles.agentCard}><header><span className={agent.archived_at || !available ? styles.archived : styles.live}>{agent.archived_at ? "Archived" : available ? "Active" : "Unavailable"}</span><code>{agent.active_version_id.slice(0, 12)}</code></header><h2>{evidencePreview(agent.name, 256)}</h2><p>{typeof agent.metadata === "object" && agent.metadata && "description" in agent.metadata ? evidencePreview(String(agent.metadata.description), 500) : "No purpose has been added."}</p><footer><span title={agent.graph}>{humanizeIdentifier(evidencePreview(agent.graph, 256))}</span><div><b>{agent.version_count} version{agent.version_count === 1 ? "" : "s"}</b><Link to="/agents/$assistantId" params={{ assistantId: agent.assistant_id }}>Open agent</Link></div></footer></article>; })}</div>
        </section>
      ) : (
        <div className="empty-state"><span className="eyebrow">Your agent library</span><h2>Create your first worker</h2><p>Start with one clear responsibility. Model, knowledge, tools, output, and guardrails stay in one reviewable capability map.</p><button className="primary-button" type="button" onClick={() => setCreating(true)}>Create agent</button></div>
      )}
    </section>
  );
}

function AgentOrbit() {
  return <div className={styles.orbit} role="img" aria-label="Agent capability system: behavior, models, memory, tools, and safeguards orbit the agent definition.">
    <span className={styles.orbitRingOne} aria-hidden="true" />
    <span className={styles.orbitRingTwo} aria-hidden="true" />
    <span className={styles.orbitRingThree} aria-hidden="true" />
    <span className={styles.orbitCore} aria-hidden="true"><b>R</b><small>agent</small></span>
    <span className={styles.signalBehavior} aria-hidden="true"><i />behavior</span>
    <span className={styles.signalMemory} aria-hidden="true"><i />memory</span>
    <span className={styles.signalTools} aria-hidden="true"><i />tools</span>
    <span className={styles.signalGuardrails} aria-hidden="true"><i />guardrails</span>
  </div>;
}
