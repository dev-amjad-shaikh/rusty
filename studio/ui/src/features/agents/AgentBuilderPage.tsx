import { type FormEvent, type RefObject, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Link, useBlocker, useNavigate } from "@tanstack/react-router";
import { createAssistant, jsonEquivalent, listAssistants, StudioApiError } from "../../lib/api/client";
import { useAgentMutationStore } from "../../state/agents";
import { useRuntimeStore } from "../../state/runtime";
import { PageHeader } from "../../components/PageHeader";
import type { Assistant } from "../../lib/contracts";
import { evidencePreview } from "../../lib/text";
import { useWorkStore } from "../../state/work";
import {
  agentVersionFields,
  capabilities,
  capabilitySummary,
  capabilityValue,
  emptyAgentDraft,
  humanizeIdentifier,
  type AgentDraft,
  type Capability,
  AgentIntentEditor,
} from "./AgentIntentEditor";
import styles from "./AgentsPage.module.css";
import { UnsavedChangesDialog } from "./UnsavedChangesDialog";

interface BuilderSession { draft: AgentDraft; visited: Set<Capability>; assistantId: string }
interface CompletedCreate { assistant: Assistant }
interface CreationReviewState { draft: AgentDraft; input: CreateOperation["input"] }
// Page memory: one draft survives in-place refreshes of this page, nothing more.
let builderSession: BuilderSession | null = null;
let completedCreate: CompletedCreate | null = null;
let builderError = "";

export function clearAgentBuilderMemory() {
  builderSession = null;
  completedCreate = null;
  builderError = "";
}
interface CreateOperation {
  input: ReturnType<typeof agentVersionFields> & { assistant_id: string };
}

export function AgentBuilderPage() {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const work = useWorkStore();
  const info = useRuntimeStore((state) => state.info);
  const [visited, setVisited] = useState<Set<Capability>>(() => new Set(builderSession?.visited ?? ["purpose"]));
  const [draft, setDraft] = useState<AgentDraft>(() => builderSession?.draft ?? emptyAgentDraft());
  const [error, setError] = useState(() => builderError);
  const [assistantId, setAssistantId] = useState<string>(() => builderSession?.assistantId ?? crypto.randomUUID());
  const [validationRequest, setValidationRequest] = useState<{ capability: Capability; nonce: number } | null>(null);
  const [creationReview, setCreationReview] = useState<CreationReviewState | null>(null);
  const builderHeading = useRef<HTMLHeadingElement>(null);
  const reviewHeading = useRef<HTMLHeadingElement>(null);
  const completeHeading = useRef<HTMLHeadingElement>(null);
  const reviewAgentButton = useRef<HTMLButtonElement>(null);
  const [completed, setCompleted] = useState<CompletedCreate | null>(() => completedCreate);
  const mutationState = useAgentMutationStore();
  const uncertainty = mutationState.uncertain;
  const builderChanged = !jsonEquivalent(draft, emptyAgentDraft());
  useEffect(() => {
    if (completed) return;
    builderSession = { draft, visited: new Set(visited), assistantId };
  }, [assistantId, completed, draft, visited]);
  useEffect(() => {
    if (completed) requestAnimationFrame(() => completeHeading.current?.focus());
  }, [completed]);

  const create = useMutation({
    mutationFn: async (operation: CreateOperation) => {
      const { input } = operation;
      try {
        const assistant = await createAssistant(input);
        return { assistant };
      } catch (caught) {
        if (!(caught instanceof StudioApiError) || !caught.mayHaveCommitted) throw caught;
        try {
          const catalog = await listAssistants();
          const assistant = catalog.find((item) => item.assistant_id === input.assistant_id);
          if (assistant && assistant.name === input.name && assistant.graph === input.graph
            && jsonEquivalent(assistant.config, input.config) && jsonEquivalent(assistant.metadata, input.metadata)) {
            return { assistant };
          }
        } catch { /* keep the uncertainty lock */ }
        mutationState.markUncertain("Rusty may have created this agent, but Studio could not prove the result. Check the portfolio before allowing another create attempt.");
        throw new StudioApiError("The create result is uncertain. Studio locked retry to avoid a duplicate agent.", caught.status, true);
      }
    },
    onSuccess: ({ assistant }) => {
      completedCreate = { assistant };
      builderSession = null;
      builderError = "";
      mutationState.clearUncertain();
      queryClient.setQueryData(["assistants"], (value: unknown) => Array.isArray(value) && !value.some((item) => item && typeof item === "object" && "assistant_id" in item && item.assistant_id === assistant.assistant_id) ? [...value, assistant] : Array.isArray(value) ? value : [assistant]);
      setCompleted(completedCreate);
      setDraft(emptyAgentDraft()); setAssistantId(crypto.randomUUID()); setVisited(new Set(["purpose"])); setError("");
      setCreationReview(null);
      void queryClient.invalidateQueries({ queryKey: ["assistants"] });
    },
    onError: (caught) => {
      const message = caught instanceof Error ? caught.message : "The agent could not be created.";
      builderError = message;
      setValidationRequest(null);
      setCreationReview(null);
      setError(message);
    },
  });
  const routeBlocker = useBlocker({
    shouldBlockFn: () => builderChanged || create.isPending,
    enableBeforeUnload: () => builderChanged || create.isPending,
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
  const requiredPurposeReady = Boolean(draft.name.trim() && draft.responsibility.trim() && draft.graph);

  function update<K extends keyof AgentDraft>(key: K, value: AgentDraft[K]) {
    setDraft((current) => ({ ...current, [key]: value }));
    builderError = "";
    setError("");
    setValidationRequest(null);
    setCreationReview(null);
  }
  function openCreationReview() {
    if (uncertainty || completed) return;
    try {
      const input = { assistant_id: assistantId, ...agentVersionFields(draft) };
      setCreationReview({ draft: structuredClone(draft), input });
      setError("");
      setValidationRequest(null);
      requestAnimationFrame(() => reviewHeading.current?.focus());
    } catch (caught) {
      const message = caught instanceof Error ? caught.message : "Review the highlighted capability.";
      const capability: Capability = /tool/i.test(message) ? "tools" : /goal|success rate|latency|cost per successful/i.test(message) ? "goals" : /model/i.test(message) ? "model" : /schema|output/i.test(message) ? "output" : /step|approval/i.test(message) ? "guardrails" : /memory/i.test(message) ? "knowledge" : "purpose";
      setError(message);
      setValidationRequest((current) => ({ capability, nonce: (current?.nonce ?? 0) + 1 }));
    }
  }
  function submit(event: FormEvent) {
    event.preventDefault();
    if (!creationReview || uncertainty || completed) return;
    create.mutate({ input: creationReview.input });
  }
  function openCompletedCreate() {
    if (!completed) return;
    completedCreate = null;
    setCompleted(null);
    setDraft(emptyAgentDraft()); setAssistantId(crypto.randomUUID()); setVisited(new Set(["purpose"])); setError("");
    navigate({ to: "/agents/$assistantId", params: { assistantId: completed.assistant.assistant_id } });
  }
  function startCompletedCreate() {
    if (!completed) return;
    completedCreate = null;
    work.prepare(completed.assistant);
    navigate({ to: "/work" });
  }
  function returnToEditing() {
    setCreationReview(null);
    requestAnimationFrame(() => reviewAgentButton.current?.focus());
  }

  return <section className={`${styles.builderPage} page`} aria-labelledby="agent-builder-heading">
    {routeBlocker.status === "blocked" && <UnsavedChangesDialog pending={create.isPending} returnFocusRef={builderHeading} onKeep={routeBlocker.reset} onDiscard={() => { builderSession = null; builderError = ""; setDraft(emptyAgentDraft()); setVisited(new Set(["purpose"])); setError(""); setValidationRequest(null); routeBlocker.proceed(); }} />}
    <PageHeader
      headingId="agent-builder-heading"
      headingRef={builderHeading}
      eyebrow={<><Link to="/agents" activeOptions={{ exact: true }}>Agents</Link><span aria-hidden="true"> / </span><span>Builder</span></>}
      title="New agent"
      detail={<span className={styles.builderBadge}>Guided draft</span>}
      actions={<div className={styles.draftState}><Link to="/agents/prompts">Prompt library</Link><span>{completed ? "version 1 created" : "draft · page memory only"}</span></div>}
      variant="compact"
    />
    <form className={styles.builder} id="agent-builder" onSubmit={submit} aria-busy={create.isPending}>
      {completed ? <CreationCompletePanel completed={completed} headingRef={completeHeading} onStart={startCompletedCreate} onReview={openCompletedCreate} />
        : creationReview ? <CreationReviewPanel review={creationReview} headingRef={reviewHeading} pending={create.isPending} onBack={returnToEditing} /> : <>
      <fieldset className={styles.builderGrid} disabled={create.isPending}>
      <AgentIntentEditor draft={draft} onChange={update} graphs={info?.graphs ?? []} progress={progress} validationRequest={validationRequest} validationMessage={error} onCapabilityVisit={(capability) => setVisited((current) => new Set(current).add(capability))} />
      <aside className={styles.review} aria-labelledby="agent-shape-heading">
        <div className={styles.shapeCard}><span className="eyebrow">Agent shape</span><h2 id="agent-shape-heading" className="sr-only">Agent shape</h2>
        <dl>{capabilities.map((item) => <div key={item.key}><dt>{item.label}</dt><dd>{progress[item.key] ? capabilitySummary(item.key, draft) : "Not set"}</dd></div>)}</dl></div>
        {uncertainty && <div className={styles.error} role="alert"><p>{uncertainty}</p><button type="button" onClick={() => { mutationState.clearUncertain(); builderError = ""; setError(""); }}>I checked the server — allow retry</button></div>}
        {error && !validationRequest && !uncertainty && <p className={styles.error} role="alert">{error}</p>}
        <button ref={reviewAgentButton} className="primary-button" type="button" onClick={openCreationReview} disabled={create.isPending || Boolean(uncertainty) || !requiredPurposeReady}>{uncertainty ? "Create locked" : !requiredPurposeReady ? "Add name, behavior, and responsibility" : "Review agent"}</button>
        <p className={styles.boundary}>Nothing runs until you create version 1.</p>
        <details className={styles.runtimeBoundary}><summary>Runtime boundary</summary><p>Requirements apply only where the selected behavior and deployment support them.</p></details>
      </aside>
      </fieldset>
      </>}
    </form>
  </section>;
}

function CreationReviewPanel({ review, headingRef, pending, onBack }: { review: CreationReviewState; headingRef: RefObject<HTMLHeadingElement | null>; pending: boolean; onBack: () => void }) {
  return <section className={styles.creationReview} aria-labelledby="creation-review-heading">
    <header><span className="eyebrow">Final review</span><h2 ref={headingRef} tabIndex={-1} id="creation-review-heading">Review version 1</h2><p>This exact definition will become the active agent. Nothing runs until you start its first task.</p></header>
    <div className={styles.reviewDefinition}>
      <div className={styles.reviewIdentity}><span aria-hidden="true">{agentInitials(review.draft.name)}</span><div><h3>{review.draft.name}</h3><p>{review.draft.responsibility}</p></div></div>
      <dl>{capabilities.map((item) => <div key={item.key}><dt>{item.label}</dt><dd>{capabilityValue(item.key, review.draft)}</dd></div>)}</dl>
      <details><summary>Exact stored definition</summary><pre>{JSON.stringify(review.input, null, 2)}</pre></details>
    </div>
    <footer><button className="secondary-button" type="button" onClick={onBack}>Back to edit</button><button className="primary-button" type="submit" disabled={pending}>{pending ? "Creating…" : "Create version 1"}</button></footer>
  </section>;
}

function CreationCompletePanel({ completed, headingRef, onStart, onReview }: { completed: CompletedCreate; headingRef: RefObject<HTMLHeadingElement | null>; onStart: () => void; onReview: () => void }) {
  const assistant = completed.assistant;
  const graph = evidencePreview(assistant.graph, 256);
  return <section className={styles.creationComplete} aria-labelledby="creation-complete-heading">
    <div className={styles.completeSignal} aria-hidden="true"><span>01</span><i /></div>
    <div className={styles.completeBody}>
      <span className="eyebrow">Version 1 created</span>
      <h2 ref={headingRef} tabIndex={-1} id="creation-complete-heading">{evidencePreview(assistant.name, 256)} is ready for its first task</h2>
      <p>The active definition is saved in this workspace. Give it one real objective, then follow the run on the Work board.</p>
      <dl><div><dt>Behavior</dt><dd title={graph}>{humanizeIdentifier(graph)}</dd></div><div><dt>Active version</dt><dd><code>{assistant.active_version_id.slice(0, 16)}</code></dd></div></dl>
    </div>
    <div className={styles.completeActions}>
      <button className="primary-button" type="button" onClick={onStart}>Start first task</button>
      <button className="secondary-button" type="button" onClick={onReview}>Review agent</button>
      <Link to="/">Go to Work board</Link>
    </div>
  </section>;
}

function agentInitials(name: string) {
  return name.trim().split(/\s+/u).filter(Boolean).slice(0, 2).map((part) => Array.from(part)[0]?.toUpperCase() ?? "").join("") || "AG";
}
