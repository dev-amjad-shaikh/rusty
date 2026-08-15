import { type FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Link, useBlocker, useNavigate } from "@tanstack/react-router";
import { connectionScope, createAssistant, jsonEquivalent, listAssistants, mutationScope, StudioApiError } from "../../lib/api/client";
import { useAgentMutationStore } from "../../state/agents";
import { useConnectionStore } from "../../state/connection";
import { PageHeader } from "../../components/PageHeader";
import {
  agentVersionFields,
  capabilities,
  capabilitySummary,
  emptyAgentDraft,
  hasPortableIntent,
  type AgentDraft,
  type Capability,
  AgentIntentEditor,
} from "./AgentIntentEditor";
import styles from "./AgentsPage.module.css";
import { UnsavedChangesDialog } from "./UnsavedChangesDialog";

interface BuilderSession { draft: AgentDraft; visited: Set<Capability>; assistantId: string }
interface CompletedCreate { assistantId: string; name: string }
const builderSessions = new Map<string, BuilderSession>();
const completedCreates = new Map<string, CompletedCreate>();
const builderErrors = new Map<string, string>();

export function clearAgentBuilderMemory() {
  builderSessions.clear();
  completedCreates.clear();
  builderErrors.clear();
}
interface CreateOperation {
  connection: NonNullable<ReturnType<typeof useConnectionStore.getState>["connection"]>;
  initiatingScope: string;
  durableScope: string;
  input: ReturnType<typeof agentVersionFields> & { assistant_id: string };
}

export function AgentBuilderPage() {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { connection, info, openDialog } = useConnectionStore();
  const durableMutationScope = connection ? mutationScope(connection) : "disconnected";
  const initialSession = builderSessions.get(durableMutationScope);
  const [visited, setVisited] = useState<Set<Capability>>(() => new Set(initialSession?.visited ?? ["purpose"]));
  const [draft, setDraft] = useState<AgentDraft>(() => initialSession?.draft ?? emptyAgentDraft());
  const [error, setError] = useState(() => builderErrors.get(durableMutationScope) ?? "");
  const [assistantId, setAssistantId] = useState<string>(() => initialSession?.assistantId ?? crypto.randomUUID());
  const [validationRequest, setValidationRequest] = useState<{ capability: Capability; nonce: number } | null>(null);
  const allowNavigation = useRef(false);
  const builderHeading = useRef<HTMLHeadingElement>(null);
  const previousWorkspace = useRef(durableMutationScope);
  const [completedCreate, setCompletedCreate] = useState<CompletedCreate | null>(() => completedCreates.get(durableMutationScope) ?? null);
  const mutationState = useAgentMutationStore();
  const uncertainty = mutationState.uncertainByConnection[durableMutationScope] ?? "";
  const builderChanged = !jsonEquivalent(draft, emptyAgentDraft());
  const hiddenDraftChanged = [...builderSessions.entries()].some(([scope, session]) => scope !== durableMutationScope && !jsonEquivalent(session.draft, emptyAgentDraft()));
  useEffect(() => {
    if (completedCreate || previousWorkspace.current !== durableMutationScope) return;
    builderSessions.set(durableMutationScope, { draft, visited: new Set(visited), assistantId });
  }, [assistantId, completedCreate, draft, durableMutationScope, visited]);
  useEffect(() => {
    const previous = previousWorkspace.current;
    previousWorkspace.current = durableMutationScope;
    if (previous === durableMutationScope) return;
    builderSessions.set(previous, { draft, visited: new Set(visited), assistantId });
    if (previous === "disconnected" && durableMutationScope !== "disconnected" && !builderChanged) builderSessions.delete(previous);
    const completed = completedCreates.get(durableMutationScope) ?? null;
    const next = builderSessions.get(durableMutationScope);
    if (previous === "disconnected" && durableMutationScope !== "disconnected" && builderChanged && !next) {
      builderSessions.set(durableMutationScope, { draft, visited: new Set(visited), assistantId });
      builderSessions.delete(previous);
      return;
    }
    setCompletedCreate(completed);
    setVisited(completed ? new Set(["purpose"]) : next ? new Set(next.visited) : new Set(["purpose"]));
    setDraft(completed ? emptyAgentDraft() : next?.draft ?? emptyAgentDraft());
    setAssistantId(completed ? crypto.randomUUID() : next?.assistantId ?? crypto.randomUUID());
    setError(builderErrors.get(durableMutationScope) ?? "");
    setValidationRequest(null);
    // Each connection owns its own page-memory draft. Switching back restores it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [durableMutationScope]);

  const queryKey = connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "assistants"] : ["assistants", "disconnected"];
  const create = useMutation({
    mutationFn: async (operation: CreateOperation) => {
      const { connection: initiatingConnection, input } = operation;
      try {
        const assistant = await createAssistant(initiatingConnection, input);
        return { assistant, operation };
      } catch (caught) {
        if (!(caught instanceof StudioApiError) || !caught.mayHaveCommitted) throw caught;
        try {
          const catalog = await listAssistants(initiatingConnection);
          const assistant = catalog.find((item) => item.assistant_id === input.assistant_id);
          if (assistant && assistant.name === input.name && assistant.graph === input.graph
            && jsonEquivalent(assistant.config, input.config) && jsonEquivalent(assistant.metadata, input.metadata)) {
            return { assistant, operation };
          }
        } catch { /* keep the uncertainty lock */ }
        mutationState.markUncertain(operation.durableScope, "Rusty may have created this agent, but Studio could not prove the result. Check the portfolio before allowing another create attempt.");
        throw new StudioApiError("The create result is uncertain. Studio locked retry to avoid a duplicate agent.", caught.status, true);
      }
    },
    onSuccess: ({ assistant, operation }) => {
      const completed = { assistantId: assistant.assistant_id, name: assistant.name };
      completedCreates.set(operation.durableScope, completed);
      builderSessions.delete(operation.durableScope);
      builderErrors.delete(operation.durableScope);
      mutationState.clearUncertain(operation.durableScope);
      const operationQueryKey = [operation.connection.epoch, operation.connection.origin, operation.connection.tenantFingerprint, "assistants"];
      queryClient.setQueryData(operationQueryKey, (value: unknown) => Array.isArray(value) && !value.some((item) => item && typeof item === "object" && "assistant_id" in item && item.assistant_id === assistant.assistant_id) ? [...value, assistant] : Array.isArray(value) ? value : [assistant]);
      const current = useConnectionStore.getState().connection;
      if (!current) return;
      if (mutationScope(current) === operation.durableScope && connectionScope(current) !== operation.initiatingScope) {
        setCompletedCreate(completed);
        setDraft(emptyAgentDraft()); setAssistantId(crypto.randomUUID()); setVisited(new Set(["purpose"])); setError("");
        queryClient.invalidateQueries({ queryKey: [current.epoch, current.origin, current.tenantFingerprint, "assistants"] });
        return;
      }
      if (connectionScope(current) !== operation.initiatingScope) return;
      completedCreates.delete(operation.durableScope);
      setCompletedCreate(null);
      setDraft(emptyAgentDraft()); setAssistantId(crypto.randomUUID()); setVisited(new Set(["purpose"])); setError("");
      allowNavigation.current = true;
      navigate({ to: "/agents/$assistantId", params: { assistantId: assistant.assistant_id } });
    },
    onError: (caught, operation) => {
      const current = useConnectionStore.getState().connection;
      const message = caught instanceof Error ? caught.message : "The agent could not be created.";
      builderErrors.set(operation.durableScope, message);
      if (!current || mutationScope(current) !== operation.durableScope) return;
      setValidationRequest(null);
      setError(message);
    },
  });
  const routeBlocker = useBlocker({
    shouldBlockFn: () => (builderChanged || hiddenDraftChanged || create.isPending) && !allowNavigation.current,
    enableBeforeUnload: () => builderChanged || hiddenDraftChanged || create.isPending,
    withResolver: true,
  });

  const progress = useMemo(() => ({
    purpose: Boolean(draft.name.trim() && draft.responsibility.trim() && draft.graph),
    goals: visited.has("goals"),
    model: visited.has("model"),
    knowledge: visited.has("knowledge") && (draft.memoryAccess === "none" || draft.scopes.length > 0),
    tools: visited.has("tools"),
    output: visited.has("output") && (draft.outputMode !== "json_schema" || Boolean(draft.outputSchema.trim())),
    guardrails: visited.has("guardrails"),
  }), [draft, visited]);
  const completed = Object.values(progress).filter(Boolean).length;
  const intentReady = hasPortableIntent(draft);
  const parkedDraftCount = [...builderSessions.entries()].filter(([scope, session]) => scope !== durableMutationScope && !jsonEquivalent(session.draft, emptyAgentDraft())).length;

  function update<K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) {
    setDraft((current) => ({ ...current, [key]: value }));
    builderErrors.delete(durableMutationScope);
    setError("");
    setValidationRequest(null);
  }
  function submit(event: FormEvent) {
    event.preventDefault();
    if (!connection) { openDialog(); return; }
    if (!uncertainty) {
      const initiatingConnection = connection;
      try {
        setValidationRequest(null);
        create.mutate({
          connection: initiatingConnection,
          initiatingScope: connectionScope(initiatingConnection),
          durableScope: mutationScope(initiatingConnection),
          input: { assistant_id: assistantId, ...agentVersionFields(draft) },
        });
      } catch (caught) {
        const message = caught instanceof Error ? caught.message : "Review the highlighted capability.";
        const capability: Capability = /tool/i.test(message) ? "tools" : /goal|success rate|latency|cost per successful/i.test(message) ? "goals" : /model/i.test(message) ? "model" : /schema|output/i.test(message) ? "output" : /step|approval/i.test(message) ? "guardrails" : /memory/i.test(message) ? "knowledge" : "purpose";
        setError(message);
        setValidationRequest((current) => ({ capability, nonce: (current?.nonce ?? 0) + 1 }));
      }
    }
  }
  function openCompletedCreate() {
    if (!completedCreate) return;
    completedCreates.delete(durableMutationScope);
    setCompletedCreate(null);
    setDraft(emptyAgentDraft()); setAssistantId(crypto.randomUUID()); setVisited(new Set(["purpose"])); setError("");
    allowNavigation.current = true;
    navigate({ to: "/agents/$assistantId", params: { assistantId: completedCreate.assistantId } });
  }

  return <section className={`${styles.builderPage} page`} aria-labelledby="agent-builder-heading">
    {routeBlocker.status === "blocked" && <UnsavedChangesDialog pending={create.isPending} title={parkedDraftCount ? `Discard ${parkedDraftCount + (builderChanged ? 1 : 0)} workspace drafts?` : undefined} message={parkedDraftCount ? "Drafts are parked in more than one workspace. Continuing will discard every parked definition from this page." : undefined} discardLabel={parkedDraftCount ? "Discard all drafts" : undefined} returnFocusRef={builderHeading} onKeep={routeBlocker.reset} onDiscard={() => { builderSessions.clear(); builderErrors.clear(); setDraft(emptyAgentDraft()); setVisited(new Set(["purpose"])); setError(""); setValidationRequest(null); routeBlocker.proceed(); }} />}
    <PageHeader
      headingId="agent-builder-heading"
      headingRef={builderHeading}
      eyebrow={<><Link to="/agents" activeOptions={{ exact: true }}>Agents</Link><span aria-hidden="true"> / </span><span>Builder</span></>}
      title="New agent"
      detail={<span className={styles.builderBadge}>Guided draft</span>}
      actions={<div className={styles.draftState}>{connection ? <Link to="/agents/prompts">Prompt library</Link> : <button type="button" onClick={openDialog}>Choose workspace</button>}<span>{completedCreate ? "version 1 created" : "draft · page memory only"}</span></div>}
      variant="compact"
    />
    <form className={styles.builder} id="agent-builder" onSubmit={submit} aria-busy={create.isPending}>
      <fieldset className={styles.builderGrid} disabled={create.isPending}>
      <AgentIntentEditor draft={draft} onChange={update} graphs={info?.graphs.map((graph) => graph.name) ?? []} progress={progress} validationRequest={validationRequest} validationMessage={error} onCapabilityVisit={(capability) => setVisited((current) => new Set(current).add(capability))} />
      <aside className={styles.review} aria-labelledby="agent-shape-heading">
        <div className={styles.shapeCard}><span className="eyebrow">Agent shape</span><h2 id="agent-shape-heading" className="sr-only">Agent shape</h2>
        <dl>{capabilities.map((item) => <div key={item.key}><dt>{item.label}</dt><dd>{progress[item.key] ? capabilitySummary(item.key, draft) : "Not set"}</dd></div>)}</dl></div>
        {completedCreate && <div className={styles.success} role="status"><p><strong>{completedCreate.name}</strong> was created in this workspace.</p><button type="button" onClick={openCompletedCreate}>Open agent</button></div>}
        {uncertainty && <div className={styles.error} role="alert"><p>{uncertainty}</p><button type="button" onClick={() => { mutationState.clearUncertain(durableMutationScope); builderErrors.delete(durableMutationScope); setError(""); }}>I checked the server — allow retry</button></div>}
        {error && !validationRequest && !uncertainty && <p className={styles.error} role="alert">{error}</p>}
        <button className="primary-button" type="submit" disabled={create.isPending || Boolean(uncertainty) || Boolean(completedCreate) || completed < capabilities.length || !intentReady}>{create.isPending ? "Creating…" : completedCreate ? "Agent created" : uncertainty ? "Create locked" : completed < capabilities.length ? `Review ${capabilities.length - completed} more` : !intentReady ? "Add one capability" : !connection ? "Choose workspace to save" : "Create agent"}</button>
        <p className={styles.boundary}>{connection ? "Nothing runs until you create version 1." : "This draft stays here until you choose a workspace."}</p>
        {connection && <details className={styles.runtimeBoundary}><summary>Runtime boundary</summary><p>Requirements apply only where the selected behavior and deployment support them.</p></details>}
      </aside>
      </fieldset>
    </form>
  </section>;
}
