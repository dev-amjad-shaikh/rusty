import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useBlocker, useNavigate, useParams } from "@tanstack/react-router";
import { connectionScope, jsonEquivalent, StudioApiError } from "../../lib/api/client";
import {
  activateAssistantVersion,
  createAssistantVersion,
  getAssistant,
  getAssistantVersion,
  listAssistantVersions,
  setAssistantLifecycle,
  type AssistantVersion,
  type AssistantVersionSummary,
} from "../../lib/api/assistants";
import type { Assistant } from "../../lib/contracts";
import { evidencePreview } from "../../lib/text";
import { useConnectionStore } from "../../state/connection";
import { useWorkStore } from "../../state/work";
import { PageHeader } from "../../components/PageHeader";
import {
  AgentIntentEditor,
  agentVersionFields,
  capabilities,
  capabilityValue,
  draftFromAgent,
  editableAgent,
  humanizeIdentifier,
  type AgentDraft,
} from "./AgentIntentEditor";
import styles from "./AgentWorkspace.module.css";
import { UnsavedChangesDialog } from "./UnsavedChangesDialog";

type LifecycleAction = "archive" | "restore";
type VersionReviewState = { summary: AssistantVersionSummary; expectedActiveVersionId: string; expectedVersionCount: number; activeSnapshot: Assistant };
type LifecycleReviewState = { action: LifecycleAction; snapshot: Assistant };
type DiscardAction = "close" | "lifecycle";

export function AgentWorkspace() {
  const { assistantId } = useParams({ strict: false }) as { assistantId: string };
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { connection, info, openDialog } = useConnectionStore();
  const work = useWorkStore();
  const scope = connection ? connectionScope(connection) : "disconnected";
  const key = assistantHistoryKey(connection, assistantId);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<AgentDraft | null>(null);
  const [editSource, setEditSource] = useState<Assistant | null>(null);
  const [versionReview, setVersionReview] = useState<VersionReviewState | null>(null);
  const [activationAcknowledged, setActivationAcknowledged] = useState(false);
  const [lifecycleReview, setLifecycleReview] = useState<LifecycleReviewState | null>(null);
  const [discardAction, setDiscardAction] = useState<DiscardAction | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const noticeRef = useRef<HTMLParagraphElement>(null);
  const reviewHeadingRef = useRef<HTMLHeadingElement>(null);
  const versionErrorRef = useRef<HTMLHeadingElement>(null);
  const createVersionRef = useRef<HTMLButtonElement>(null);
  const lifecycleHeadingRef = useRef<HTMLHeadingElement>(null);
  const workspaceRef = useRef({ assistantId, scope, mounted: true });
  workspaceRef.current = { assistantId, scope, mounted: true };

  useEffect(() => () => { workspaceRef.current.mounted = false; }, []);

  useEffect(() => {
    setEditing(false); setDraft(null); setEditSource(null); setVersionReview(null);
    setActivationAcknowledged(false); setLifecycleReview(null); setDiscardAction(null); setError(""); setNotice("");
  }, [assistantId, scope]);

  const history = useQuery({
    queryKey: key,
    queryFn: () => listAssistantVersions(connection!, assistantId),
    enabled: Boolean(connection && assistantId),
  });
  const assistant = history.data?.assistant ?? null;
  const selectedVersionId = versionReview?.summary.version_id ?? "";
  const selectedVersion = useQuery({
    queryKey: connection && selectedVersionId ? [...key, "version", selectedVersionId] : ["assistant-version", "idle"],
    queryFn: () => getAssistantVersion(connection!, assistantId, versionReview!.summary, versionReview!.expectedActiveVersionId),
    enabled: Boolean(connection && versionReview && !versionReview.summary.active),
  });

  const activeDraft = useMemo(() => readDraft(assistant), [assistant]);
  const reviewedActiveDraft = useMemo(() => readDraft(versionReview?.activeSnapshot ?? null), [versionReview]);
  const reviewedDraft = useMemo(() => readDraft(selectedVersion.data?.version ?? null), [selectedVersion.data]);
  const reviewedChanges = useMemo(() => reviewedActiveDraft && reviewedDraft && versionReview && selectedVersion.data?.version
    ? changedCapabilities(reviewedActiveDraft, reviewedDraft, versionReview.activeSnapshot, selectedVersion.data.version) : [],
  [reviewedActiveDraft, reviewedDraft, selectedVersion.data, versionReview]);
  const versionFields = useMemo(() => {
    if (!draft || !editSource) return { fields: null, error: "" };
    try { return { fields: agentVersionFields(draft, editSource), error: "" }; }
    catch (caught) { return { fields: null, error: caught instanceof Error ? caught.message : "The definition is not ready." }; }
  }, [draft, editSource]);
  const sourceDraft = useMemo(() => readDraft(editSource), [editSource]);
  const draftChanged = Boolean(draft && sourceDraft && !jsonEquivalent(draft, sourceDraft));
  const routeBlocker = useBlocker({
    shouldBlockFn: () => draftChanged,
    enableBeforeUnload: () => draftChanged,
    withResolver: true,
  });

  useEffect(() => {
    if (!versionReview || !history.data || history.data.active_version_id === versionReview.expectedActiveVersionId) return;
    setVersionReview(null); setActivationAcknowledged(false);
    setNotice("The active version changed while this review was open. Open the version again to review it against current server truth.");
  }, [history.data, versionReview]);

  useEffect(() => {
    if (!lifecycleReview || !assistant || assistant.active_version_id === lifecycleReview.snapshot.active_version_id) return;
    setLifecycleReview(null);
    setNotice("The active version changed while lifecycle confirmation was open. Review the current agent before continuing.");
  }, [assistant, lifecycleReview]);

  useEffect(() => {
    if (selectedVersion.data?.version) reviewHeadingRef.current?.focus({ preventScroll: true });
  }, [selectedVersion.data]);

  useEffect(() => {
    if (selectedVersion.isError) versionErrorRef.current?.focus({ preventScroll: true });
  }, [selectedVersion.isError]);

  const saveVersion = useMutation({
    mutationFn: async (input: { connection: NonNullable<typeof connection>; scope: string; source: Assistant; fields: NonNullable<typeof versionFields.fields> }) => {
      try {
        const receipt = await createAssistantVersion(input.connection, input.source.assistant_id, input.source.active_version_id, input.fields);
        return { receipt, initiatingScope: input.scope, initiatingAssistantId: input.source.assistant_id, activeSnapshot: input.source };
      } catch (caught) {
        if (caught instanceof StudioApiError && caught.mayHaveCommitted) {
          throw new StudioApiError("Studio could not confirm the saved version. Retrying this unchanged draft is safe because Rusty converges identical content on the same base version.", caught.status, true);
        }
        throw caught;
      }
    },
    onSuccess: async ({ receipt, initiatingScope, initiatingAssistantId, activeSnapshot }) => {
      if (!ownsWorkspace(initiatingScope, initiatingAssistantId)) return;
      setEditing(false); setDraft(null); setEditSource(null); setError("");
      setVersionReview({
        summary: { version_id: receipt.version.version_id, parent_version_id: receipt.version.parent_version_id, graph: receipt.version.graph, created_at: receipt.version.created_at, active: false },
        expectedActiveVersionId: receipt.active_version_id,
        expectedVersionCount: activeSnapshot.version_count + (receipt.created ? 1 : 0),
        activeSnapshot: structuredClone(activeSnapshot),
      });
      setActivationAcknowledged(false);
      setNotice(receipt.created ? "Draft version saved. The active version is unchanged." : "This exact draft version was already saved. The active version is unchanged.");
      await queryClient.invalidateQueries({ queryKey: assistantHistoryKey(connection, initiatingAssistantId) });
      if (ownsWorkspace(initiatingScope, initiatingAssistantId)) noticeRef.current?.focus({ preventScroll: true });
    },
    onError: async (caught, input) => {
      if (!ownsWorkspace(input.scope, input.source.assistant_id)) return;
      setError(caught instanceof Error ? caught.message : "The draft version could not be saved.");
      if (caught instanceof StudioApiError && caught.status === 409) await queryClient.invalidateQueries({ queryKey: assistantHistoryKey(input.connection, input.source.assistant_id) });
    },
  });

  const activation = useMutation({
    mutationFn: async (input: { connection: NonNullable<typeof connection>; scope: string; assistantId: string; target: AssistantVersion; activeVersionId: string; versionCount: number }) => {
      try {
        const receipt = await activateAssistantVersion(input.connection, input.assistantId, input.target, input.activeVersionId, input.versionCount);
        return { assistant: receipt.assistant, reconciled: false, initiatingScope: input.scope, initiatingAssistantId: input.assistantId };
      } catch (caught) {
        if (!(caught instanceof StudioApiError) || !caught.mayHaveCommitted) throw caught;
        let current: Assistant;
        try { current = await getAssistant(input.connection, input.assistantId); }
        catch { throw new StudioApiError("Activation may have completed, but Studio could not refresh Rusty. Do not repeat it until server truth is available.", caught.status, true); }
        if (current.active_version_id === input.target.version_id && current.name === input.target.name && current.graph === input.target.graph
          && jsonEquivalent(current.config, input.target.config) && jsonEquivalent(current.metadata, input.target.metadata)) {
          return { assistant: current, reconciled: true, initiatingScope: input.scope, initiatingAssistantId: input.assistantId };
        }
        throw new StudioApiError("Rusty did not confirm which version is active. Studio refreshed server truth; review the current version before trying again.", caught.status, true);
      }
    },
    onSuccess: async ({ assistant: activated, reconciled, initiatingScope, initiatingAssistantId }) => {
      if (!ownsWorkspace(initiatingScope, initiatingAssistantId)) return;
      setVersionReview(null); setActivationAcknowledged(false); setError("");
      setNotice(reconciled ? "The reviewed version is active; Studio confirmed it after refreshing Rusty." : "The reviewed version is now active.");
      const operationKey = assistantHistoryKey(connection, initiatingAssistantId);
      queryClient.setQueryData(operationKey, (current: unknown) => replaceHistoryAssistant(current, activated));
      await queryClient.invalidateQueries({ queryKey: operationKey });
      if (ownsWorkspace(initiatingScope, initiatingAssistantId)) noticeRef.current?.focus({ preventScroll: true });
    },
    onError: async (caught, input) => {
      if (!ownsWorkspace(input.scope, input.assistantId)) return;
      setError(caught instanceof Error ? caught.message : "The version could not be activated.");
      setActivationAcknowledged(false);
      await queryClient.invalidateQueries({ queryKey: assistantHistoryKey(input.connection, input.assistantId) });
    },
  });

  const lifecycle = useMutation({
    mutationFn: async (input: { connection: NonNullable<typeof connection>; scope: string; snapshot: Assistant; action: LifecycleAction }) => {
      try {
        const receipt = await setAssistantLifecycle(input.connection, input.snapshot, input.action);
        return { assistant: receipt.assistant, reconciled: false, action: input.action, initiatingScope: input.scope, initiatingAssistantId: input.snapshot.assistant_id };
      } catch (caught) {
        if (!(caught instanceof StudioApiError) || !caught.mayHaveCommitted) throw caught;
        let current: Assistant;
        try { current = await getAssistant(input.connection, input.snapshot.assistant_id); }
        catch { throw new StudioApiError("The lifecycle change may have completed, but Studio could not refresh Rusty. Do not repeat it until server truth is available.", caught.status, true); }
        const expectedArchived = input.action === "archive";
        if (Boolean(current.archived_at) === expectedArchived && current.active_version_id === input.snapshot.active_version_id) {
          return { assistant: current, reconciled: true, action: input.action, initiatingScope: input.scope, initiatingAssistantId: input.snapshot.assistant_id };
        }
        throw new StudioApiError("Rusty did not confirm the lifecycle change. Studio refreshed the agent before allowing another attempt.", caught.status, true);
      }
    },
    onSuccess: async ({ assistant: changed, reconciled, action, initiatingScope, initiatingAssistantId }) => {
      if (!ownsWorkspace(initiatingScope, initiatingAssistantId)) return;
      setLifecycleReview(null); setError("");
      setNotice(`${action === "archive" ? "Agent archived" : "Agent restored"}${reconciled ? " after Studio confirmed server state" : ""}.`);
      const operationKey = assistantHistoryKey(connection, initiatingAssistantId);
      queryClient.setQueryData(operationKey, (current: unknown) => replaceHistoryAssistant(current, changed));
      await queryClient.invalidateQueries({ queryKey: operationKey });
      if (ownsWorkspace(initiatingScope, initiatingAssistantId)) noticeRef.current?.focus({ preventScroll: true });
    },
    onError: async (caught, input) => {
      if (!ownsWorkspace(input.scope, input.snapshot.assistant_id)) return;
      setError(caught instanceof Error ? caught.message : "The lifecycle change could not be confirmed.");
      await queryClient.invalidateQueries({ queryKey: assistantHistoryKey(input.connection, input.snapshot.assistant_id) });
    },
  });

  function ownsWorkspace(expectedScope: string, expectedAssistantId: string) {
    const current = useConnectionStore.getState().connection;
    const workspace = workspaceRef.current;
    return Boolean(workspace.mounted && workspace.scope === expectedScope && workspace.assistantId === expectedAssistantId
      && current && connectionScope(current) === expectedScope);
  }

  function beginEdit() {
    if (!assistant) return;
    try {
      setDraft(draftFromAgent(assistant)); setEditSource(structuredClone(assistant)); setEditing(true);
      setVersionReview(null); setLifecycleReview(null); setError(""); setNotice("");
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : "This agent cannot be edited safely.");
    }
  }

  function updateDraft<K extends keyof AgentDraft>(field: K, value: AgentDraft[K]) {
    setDraft((current) => current ? { ...current, [field]: value } : current);
    setError(""); setNotice("");
  }

  function runActive() {
    if (!assistant || !connection || assistant.archived_at || !info?.graphs.some((graph) => graph.name === assistant.graph)) return;
    work.prepare(scope, assistant);
    navigate({ to: "/work" });
  }

  function closeDraft() {
    setEditing(false); setDraft(null); setEditSource(null); setDiscardAction(null); setError("");
  }

  function requestLifecycle() {
    if (!assistant) return;
    if (draftChanged) { setDiscardAction("lifecycle"); return; }
    setLifecycleReview({ action: archived ? "restore" : "archive", snapshot: structuredClone(assistant) });
    setVersionReview(null); setError("");
  }

  function confirmDiscard() {
    const next = discardAction;
    closeDraft();
    if (next === "lifecycle" && assistant) {
      setLifecycleReview({ action: archived ? "restore" : "archive", snapshot: structuredClone(assistant) });
      requestAnimationFrame(() => lifecycleHeadingRef.current?.focus());
    } else {
      requestAnimationFrame(() => createVersionRef.current?.focus());
    }
  }

  const workspaceEyebrow = <><Link to="/agents" activeOptions={{ exact: true }}>Agents</Link><span aria-hidden="true"> / </span><span>Workspace</span></>;
  if (!connection) return <section className="page" aria-labelledby="agent-heading"><PageHeader headingId="agent-heading" eyebrow={workspaceEyebrow} title="Open an agent" variant="compact" /><div className="empty-state"><h2>Open its workspace to continue</h2><p>The active definition and immutable history stay in your Rusty workspace.</p><button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button></div></section>;
  if (history.isLoading) return <section className="page" aria-labelledby="agent-heading"><PageHeader headingId="agent-heading" eyebrow={workspaceEyebrow} title="Opening agent" variant="compact" /><div className={styles.loading} role="status">Loading agent…</div></section>;
  if (history.isError || !history.data || !assistant) return <section className="page" aria-labelledby="agent-heading"><PageHeader headingId="agent-heading" eyebrow={workspaceEyebrow} title="Agent unavailable" description={history.error instanceof Error ? history.error.message : "Rusty did not return this agent."} actions={<Link className="secondary-button" to="/agents">Back to agents</Link>} variant="compact" /><div className="empty-state"><h2>Reload the active definition</h2><button className="primary-button" type="button" onClick={() => history.refetch()}>Retry</button></div></section>;

  const historyData = history.data;
  const archived = Boolean(assistant.archived_at);
  const target = selectedVersion.data?.version ?? null;
  const graphAvailable = Boolean(info?.graphs.some((graph) => graph.name === assistant.graph));
  const visuallyEditable = editableAgent(assistant);
  const editBlocked = archived || !visuallyEditable || !graphAvailable;
  const availabilityNote = !graphAvailable
    ? "This behavior is not available in the connected deployment."
    : !visuallyEditable ? "This stored definition is view-only in the visual editor." : "";
  const displayedVersions = [...historyData.versions].sort((left, right) => right.created_at.localeCompare(left.created_at)
    || right.version_id.localeCompare(left.version_id));

  return <section className="page" aria-labelledby="agent-heading">
    <PageHeader
      headingId="agent-heading"
      eyebrow={workspaceEyebrow}
      title={evidencePreview(assistant.name, 512)}
      description={agentDescription(assistant)}
      detail={<div className={styles.titleLine}><span className={archived ? styles.archived : styles.active}>{archived ? "Archived" : "Active"}</span><code>{shortId(assistant.active_version_id)}</code></div>}
      actions={<div className={styles.primaryActions}><div><button className="primary-button" type="button" onClick={runActive} disabled={archived || !graphAvailable || editing}>Run active version</button><button ref={createVersionRef} className="secondary-button" type="button" onClick={beginEdit} disabled={editBlocked || editing}>Create version</button><button className="secondary-button" type="button" onClick={requestLifecycle} disabled={lifecycle.isPending}>{archived ? "Restore" : "Archive"}</button></div>{availabilityNote && <p>{availabilityNote}</p>}</div>}
    />

    <p ref={noticeRef} className={notice ? styles.notice : styles.srHeading} role="status" tabIndex={notice ? -1 : undefined}>{notice}</p>
    {error && <p className={styles.error} role="alert">{error}</p>}

    {lifecycleReview && <section className={styles.lifecycleReview} aria-labelledby="lifecycle-heading">
      <div><span className="eyebrow">Confirm lifecycle</span><h2 ref={lifecycleHeadingRef} tabIndex={-1} id="lifecycle-heading">{lifecycleReview.action === "archive" ? "Archive this agent?" : "Restore this agent?"}</h2><p>{lifecycleReview.action === "archive" ? "New runs will stop. Existing runs and every version remain available." : "The reviewed active version will be available for new runs again."}</p></div>
      <div><button type="button" className="secondary-button" onClick={() => setLifecycleReview(null)} disabled={lifecycle.isPending}>Cancel</button><button type="button" className="primary-button" onClick={() => lifecycle.mutate({ connection, scope, snapshot: lifecycleReview.snapshot, action: lifecycleReview.action })} disabled={lifecycle.isPending}>{lifecycle.isPending ? "Confirming…" : lifecycleReview.action === "archive" ? "Archive agent" : "Restore agent"}</button></div>
    </section>}

    {(discardAction || routeBlocker.status === "blocked") && <UnsavedChangesDialog onKeep={() => { setDiscardAction(null); routeBlocker.reset?.(); }} onDiscard={() => { if (routeBlocker.status === "blocked") { closeDraft(); routeBlocker.proceed(); } else confirmDiscard(); }} />}

    <div className={styles.workspace}>
      <div className={styles.definition}>
        {editing && draft && editSource ? <>
          <header className={styles.sectionHeader}><div><span className="eyebrow">Draft version</span><h2>Draft a new definition</h2><p>Saving creates an immutable draft. It does not change what runs.</p></div><button type="button" onClick={() => draftChanged ? setDiscardAction("close") : closeDraft()}>Cancel</button></header>
          <AgentIntentEditor draft={draft} onChange={updateDraft} graphs={info?.graphs.map((graph) => graph.name) ?? []} />
          <footer className={styles.saveBar}><div><b>{draftChanged ? "Unsaved definition" : "No changes yet"}</b><span>{versionFields.error || "The active version remains available after this draft is saved."}</span></div><button className="primary-button" type="button" disabled={saveVersion.isPending || !draftChanged || !versionFields.fields} onClick={() => versionFields.fields && saveVersion.mutate({ connection, scope, source: editSource, fields: versionFields.fields })}>{saveVersion.isPending ? "Saving version…" : "Save draft version"}</button></footer>
        </> : target && versionReview ? <VersionReview headingRef={reviewHeadingRef} target={target} activeSnapshot={versionReview.activeSnapshot} activeDraft={reviewedActiveDraft} targetDraft={reviewedDraft} changes={reviewedChanges} acknowledged={activationAcknowledged} onAcknowledged={setActivationAcknowledged} onCancel={() => { const id = versionReview.summary.version_id; setVersionReview(null); setActivationAcknowledged(false); setError(""); requestAnimationFrame(() => document.querySelector<HTMLButtonElement>(`[data-review-version="${id}"]`)?.focus()); }} onActivate={() => activation.mutate({ connection, scope, assistantId, target, activeVersionId: versionReview.expectedActiveVersionId, versionCount: versionReview.expectedVersionCount })} pending={activation.isPending} />
        : selectedVersion.isLoading ? <div className={styles.loading} role="status">Loading immutable version…</div>
        : selectedVersion.isError ? <div className={styles.versionError} role="alert"><h2 ref={versionErrorRef} tabIndex={-1}>Version evidence unavailable</h2><p>{selectedVersion.error instanceof Error ? selectedVersion.error.message : "Rusty did not return the selected immutable version."}</p><div><button type="button" className="secondary-button" onClick={() => { const id = versionReview?.summary.version_id; setVersionReview(null); setError(""); requestAnimationFrame(() => id && document.querySelector<HTMLButtonElement>(`[data-review-version="${id}"]`)?.focus()); }}>Close review</button><button type="button" className="primary-button" onClick={() => selectedVersion.refetch()}>Retry</button></div></div>
        : <ActiveDefinition assistant={assistant} draft={activeDraft} />}
      </div>

      <aside className={styles.history} aria-labelledby="version-history-heading">
        <header><span className="eyebrow">Version history</span><h2 id="version-history-heading">{historyData.versions.length} immutable version{historyData.versions.length === 1 ? "" : "s"}</h2></header>
        <ol>{displayedVersions.map((version) => <li key={version.version_id} className={version.active ? styles.currentVersion : ""}><span className={styles.versionNode} aria-hidden="true" /><div><span>{version.active ? "Active" : "Saved version"}</span><code>{shortId(version.version_id)}</code><small>{formatTime(version.created_at)} · <span title={version.graph}>{humanizeIdentifier(evidencePreview(version.graph, 128))}</span></small></div>{version.active ? <b>Serving</b> : <button data-review-version={version.version_id} type="button" disabled={editing} aria-label={`Review version ${shortId(version.version_id)}`} onClick={() => { setVersionReview({ summary: structuredClone(version), expectedActiveVersionId: historyData.active_version_id, expectedVersionCount: historyData.versions.length, activeSnapshot: structuredClone(assistant) }); setActivationAcknowledged(false); setEditing(false); setLifecycleReview(null); setError(""); requestAnimationFrame(() => reviewHeadingRef.current?.focus()); }}>Review</button>}</li>)}</ol>
      </aside>
    </div>
  </section>;
}

function ActiveDefinition({ assistant, draft }: { assistant: Assistant; draft: AgentDraft | null }) {
  return <section aria-labelledby="active-definition-heading">
    <header className={styles.sectionHeader}><div><span className="eyebrow">Active definition</span><h2 id="active-definition-heading">What this agent is set up to do</h2></div><div className={styles.definitionIdentity}><span>Behavior</span><b title={assistant.graph}>{humanizeIdentifier(evidencePreview(assistant.graph, 256))}</b></div></header>
    {draft ? <div className={styles.capabilitySpine}>{capabilities.map((capability, index) => <article key={capability.key}><span>{index + 1}</span><div><small>{capability.label}</small><h3>{capabilityValue(capability.key, draft)}</h3></div></article>)}</div>
      : <div className={styles.unavailableDefinition}><h3>Visual editing is unavailable for this definition</h3><p>The stored definition remains intact. It uses a legacy or non-round-trippable shape, so Studio will not rewrite it.</p></div>}
    <details className={styles.evidence}><summary>Configuration evidence</summary><p>These are stored requirements. Their runtime enforcement depends on the selected behavior and deployment policies.</p><dl><div><dt>Agent ID</dt><dd><code>{evidencePreview(assistant.assistant_id, 256)}</code></dd></div><div><dt>Active version</dt><dd><code>{assistant.active_version_id}</code></dd></div><div><dt>Created</dt><dd>{formatTime(assistant.created_at)}</dd></div></dl><pre>{JSON.stringify({ config: assistant.config, metadata: assistant.metadata }, null, 2)}</pre></details>
  </section>;
}

function VersionReview({ headingRef, target, activeSnapshot, activeDraft, targetDraft, changes, acknowledged, onAcknowledged, onCancel, onActivate, pending }: {
  headingRef: RefObject<HTMLHeadingElement | null>;
  target: AssistantVersion;
  activeSnapshot: Assistant;
  activeDraft: AgentDraft | null;
  targetDraft: AgentDraft | null;
  changes: CapabilityChange[];
  acknowledged: boolean;
  onAcknowledged: (value: boolean) => void;
  onCancel: () => void;
  onActivate: () => void;
  pending: boolean;
}) {
  return <section aria-labelledby="version-review-heading">
    <header className={styles.sectionHeader}><div><span className="eyebrow">Activation review</span><h2 ref={headingRef} tabIndex={-1} id="version-review-heading">Review before changing future runs</h2><p>This version is already stored. Activation moves only the serving pointer.</p></div><button type="button" onClick={onCancel} disabled={pending}>Close review</button></header>
    <div className={styles.reviewIdentity}><div><span>Selected version</span><code>{target.version_id}</code></div><div><span>Parent</span><code>{target.parent_version_id ?? "Initial version"}</code></div><div><span>Created</span><b>{formatTime(target.created_at)}</b></div></div>
    {activeDraft && targetDraft ? <div className={styles.changeList}><h3>{changes.length ? `${changes.length} capability change${changes.length === 1 ? "" : "s"}` : "No visible capability changes"}</h3>{changes.length ? <div className={styles.changeRows}>{changes.map((change) => <article key={change.label}><h4>{change.label}</h4><div><span>Current</span><p>{change.before}</p></div><div><span>Selected</span><p>{change.after}</p></div></article>)}</div> : <p>Name, behavior, portable intent, and reviewed metadata match the active definition.</p>}</div> : <div className={styles.unavailableDefinition}><h3>Visual comparison unavailable</h3><p>Use the exact stored definitions below. Activation stays locked until Studio can present a safe visual comparison.</p></div>}
    <details className={styles.evidence} open={changes.some((change) => change.label === "Advanced settings")}><summary>Exact stored definitions</summary><div className={styles.rawComparison}><section><h3>Current</h3><pre>{JSON.stringify({ name: activeSnapshot.name, graph: activeSnapshot.graph, config: activeSnapshot.config, metadata: activeSnapshot.metadata }, null, 2)}</pre></section><section><h3>Selected</h3><pre>{JSON.stringify({ name: target.name, graph: target.graph, config: target.config, metadata: target.metadata }, null, 2)}</pre></section></div></details>
    <label className={styles.acknowledgement}><input type="checkbox" checked={acknowledged} disabled={!activeDraft || !targetDraft} onChange={(event) => onAcknowledged(event.target.checked)} />I reviewed every change. Future runs should use this version.</label>
    <div className={styles.reviewActions}><button type="button" className="secondary-button" onClick={onCancel} disabled={pending}>Keep current version</button><button type="button" className="primary-button" onClick={onActivate} disabled={pending || !acknowledged}>{pending ? "Activating…" : "Activate version"}</button></div>
  </section>;
}

function readDraft(source: Pick<Assistant, "name" | "graph" | "config" | "metadata"> | null) {
  if (!source || !editableAgent(source)) return null;
  try { return draftFromAgent(source); }
  catch { return null; }
}

interface CapabilityChange { label: string; before: string; after: string }

function changedCapabilities(active: AgentDraft, target: AgentDraft, activeSource: Pick<Assistant, "config" | "metadata">, targetSource: Pick<Assistant, "config" | "metadata">) {
  const changes: CapabilityChange[] = [];
  const add = (key: (typeof capabilities)[number]["key"], changed: boolean, before = capabilityValue(key, active), after = capabilityValue(key, target)) => {
    if (changed) changes.push({ label: capabilities.find((item) => item.key === key)!.label, before, after });
  };
  add("purpose", active.name !== target.name || active.responsibility !== target.responsibility || active.graph !== target.graph,
  `${active.name} · ${capabilityValue("purpose", active)} · ${humanizeIdentifier(active.graph)}`,
  `${target.name} · ${capabilityValue("purpose", target)} · ${humanizeIdentifier(target.graph)}`);
  add("goals", active.goals !== target.goals || active.audience !== target.audience);
  add("model", active.model !== target.model);
  add("knowledge", active.memoryAccess !== target.memoryAccess || !jsonEquivalent(active.scopes, target.scopes));
  add("tools", active.tools !== target.tools);
  add("output", active.outputMode !== target.outputMode || active.outputSchema !== target.outputSchema);
  add("guardrails", active.approval !== target.approval || active.recursionLimit !== target.recursionLimit);
  const activeAdvanced = advancedDefinition(activeSource);
  const targetAdvanced = advancedDefinition(targetSource);
  if (!jsonEquivalent(activeAdvanced, targetAdvanced)) {
    changes.push({ label: "Advanced settings", before: advancedValue(activeSource), after: advancedValue(targetSource) });
  }
  return changes;
}

function advancedDefinition(source: Pick<Assistant, "config" | "metadata">) {
  const config = structuredClone(source.config) as Record<string, unknown>;
  const metadata = structuredClone(source.metadata) as Record<string, unknown>;
  const intent = config.studio_intent as Record<string, unknown> | undefined;
  const advancedIntent = intent ? { budget: intent.budget, binding: intent.binding } : {};
  delete config.studio_intent;
  delete config.recursion_limit;
  delete metadata.description;
  delete metadata.audience;
  delete metadata.goals;
  return { intent: advancedIntent, config, metadata };
}

function advancedValue(source: Pick<Assistant, "config" | "metadata">) {
  const text = JSON.stringify(advancedDefinition(source));
  return text === '{"intent":{},"config":{},"metadata":{}}' ? "No additional settings" : evidencePreview(text, 2_000);
}

function replaceHistoryAssistant(value: unknown, assistant: Assistant) {
  if (!value || typeof value !== "object") return value;
  const current = value as { versions?: Array<Record<string, unknown>> } & Record<string, unknown>;
  const versions = current.versions?.map((version) => ({
    ...version,
    active: version.version_id === assistant.active_version_id,
  }));
  return { ...current, assistant, active_version_id: assistant.active_version_id, versions };
}

function agentDescription(assistant: Assistant) {
  return typeof assistant.metadata === "object" && assistant.metadata && "description" in assistant.metadata
    ? evidencePreview(String(assistant.metadata.description), 1_000) : "No responsibility has been added.";
}

function shortId(value: string) { return value.length > 18 ? `${value.slice(0, 10)}…${value.slice(-6)}` : value; }
function formatTime(value: string) { return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(value)); }
function assistantHistoryKey(connection: ReturnType<typeof useConnectionStore.getState>["connection"], assistantId: string) {
  return connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "assistant", assistantId, "history"] : ["assistant-history", "disconnected", assistantId];
}
