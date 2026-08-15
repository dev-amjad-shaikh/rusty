import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { connectionScope, getPromptCandidate, getPromptHistory, listPromptArtifacts, mutationScope, savePromptVersion, StudioApiError, type PromptArtifact } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { useWorkStore } from "../../state/work";
import { isUnicodeScalarString } from "../../lib/text";
import { usePromptMutationStore } from "../../state/prompts";
import { PageHeader } from "../../components/PageHeader";
import styles from "./PromptStudio.module.css";

function promptName(surface: string) { return surface.startsWith("prompt:") ? surface.slice(7) : surface; }
function short(value: string) { return value.length > 16 ? `${value.slice(0, 9)}…${value.slice(-5)}` : value; }
function bytes(value: string) { return new TextEncoder().encode(value).byteLength; }

export function PromptStudio() {
  const queryClient = useQueryClient();
  const { connection, openDialog } = useConnectionStore();
  const comparisons = useWorkStore((state) => state.comparisons);
  const scope = connection ? connectionScope(connection) : "disconnected";
  const durableMutationScope = connection ? mutationScope(connection) : "disconnected";
  const sourceRuns = useMemo(() => comparisons.filter((item) => item.connectionKey === scope).reverse(), [comparisons, scope]);
  const [sourceRun, setSourceRun] = useState("");
  const [selectedName, setSelectedName] = useState("");
  const [selectedVersion, setSelectedVersion] = useState("");
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [author, setAuthor] = useState("");
  const [draft, setDraft] = useState("");
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");
  const mutationState = usePromptMutationStore();
  const uncertainty = mutationState.uncertainByConnection[durableMutationScope] ?? "";

  useEffect(() => {
    setSelectedName(""); setSelectedVersion(""); setCreating(false); setName(""); setAuthor(""); setDraft("");
    setSourceRun(""); setError(""); setSaved("");
  }, [scope]);

  const key = connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "prompts"] : ["prompts", "disconnected"];
  const catalog = useQuery({ queryKey: key, queryFn: () => listPromptArtifacts(connection!), enabled: Boolean(connection) });
  useEffect(() => {
    if (!creating && !selectedName && catalog.data?.length) setSelectedName(promptName(catalog.data[0].surface));
  }, [catalog.data, creating, selectedName]);
  const history = useQuery({
    queryKey: connection && selectedName ? [...key, selectedName, "history"] : ["prompt-history", "idle"],
    queryFn: () => getPromptHistory(connection!, selectedName),
    enabled: Boolean(connection && selectedName && !creating),
  });
  useEffect(() => {
    const latest = history.data?.commits.at(-1)?.candidate_id ?? "";
    if (latest && !history.data?.commits.some((item) => item.candidate_id === selectedVersion)) setSelectedVersion(latest);
  }, [history.data, selectedVersion]);
  const version = useQuery({
    queryKey: connection && selectedVersion ? [...key, "version", selectedVersion] : ["prompt-version", "idle"],
    queryFn: () => getPromptCandidate(connection!, selectedVersion),
    enabled: Boolean(connection && selectedVersion),
  });
  useEffect(() => {
    if (!creating && version.data) {
      setDraft(version.data.candidate.content.prompt); setName(version.data.candidate.content.name);
      setError("");
    }
  }, [creating, version.data]);

  const base = version.data?.candidate.content.prompt ?? "";
  const changes = useMemo(() => lineChanges(base, draft), [base, draft]);
  const save = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Open a workspace before saving a prompt.");
      if (!name || name !== name.trim() || !isUnicodeScalarString(name) || bytes(name) > 128 || /[\p{Cc}\p{Cf}@/]/u.test(name)) throw new Error("Use a prompt name up to 128 bytes without surrounding spaces, hidden controls, @, or /.");
      if (!author.trim() || !isUnicodeScalarString(author.trim()) || bytes(author.trim()) > 128 || /[\p{Cc}\p{Cf}]/u.test(author.trim())) throw new Error("Name the person authoring this version without hidden controls.");
      if (!draft.trim()) throw new Error("Write the prompt before saving a version.");
      if (!isUnicodeScalarString(draft)) throw new Error("The prompt contains an invalid Unicode character. Remove the broken character before saving.");
      if (bytes(draft) > 256 * 1024) throw new Error("Studio authors prompts up to 256 KiB. Split larger instructions before versioning them here.");
      if (!sourceRun || !sourceRuns.some((item) => item.run.run_id === sourceRun)) throw new Error("Choose the completed run that informed this prompt version.");
      try {
        const receipt = await savePromptVersion(connection, { name, prompt: draft, humanId: author.trim(), runId: sourceRun, artifactExists: Boolean(catalog.data?.some((item) => promptName(item.surface) === name)) });
        return { receipt, initiatingScope: connectionScope(connection) };
      } catch (caught) {
        if (!(caught instanceof StudioApiError) || !caught.mayHaveCommitted) throw caught;
        try {
          const exactHistory = await getPromptHistory(connection, name);
          const candidateId = exactHistory.commits.at(-1)?.candidate_id;
          if (candidateId) {
            const candidate = await getPromptCandidate(connection, candidateId);
            const runs = candidate.candidate.evidence?.run_ids ?? [];
            if (candidate.candidate.content.name === name && candidate.candidate.content.prompt === draft && runs.includes(sourceRun)) {
              return { receipt: { candidateId, created: false, committed: true }, initiatingScope: connectionScope(connection) };
            }
          }
        } catch { /* lock below */ }
        mutationState.markUncertain(mutationScope(connection), "Rusty may have saved part or all of this prompt version. Check its history before allowing another save.");
        throw new StudioApiError("The save result is uncertain. Studio locked retry to avoid a duplicate version operation.", caught.status, true);
      }
    },
    onSuccess: async ({ receipt, initiatingScope }) => {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== initiatingScope) return;
      mutationState.clearUncertain(mutationScope(current));
      setSaved(receipt.candidateId); setCreating(false); setSelectedName(name); setSelectedVersion(receipt.candidateId); setError("");
      await queryClient.invalidateQueries({ queryKey: key });
    },
    onError: (caught) => {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scope) return;
      setError(caught instanceof Error ? caught.message : "The prompt version could not be saved.");
    },
  });

  function newPrompt() { setCreating(true); setSelectedName(""); setSelectedVersion(""); setName(""); setDraft(""); setError(""); setSaved(""); }
  function selectArtifact(artifact: PromptArtifact) { setCreating(false); setSelectedName(promptName(artifact.surface)); setSelectedVersion(artifact.commits.at(-1)?.candidate_id ?? ""); setError(""); setSaved(""); }

  return <section className="page" aria-labelledby="prompts-heading">
    <PageHeader headingId="prompts-heading" eyebrow="Agents / Prompts" title="Prompt library" description="Edit instructions with their immutable history and exact source run beside you." actions={<div className={styles.headerActions}><Link className="secondary-button" to="/agents">Back to agents</Link>{connection && <button className="primary-button" type="button" onClick={newPrompt}>New prompt</button>}</div>} />
      {!connection ? <div className="empty-state"><span className="eyebrow">Prompt library</span><h2>Open a workspace to load prompts</h2><p>Prompts stay in your Rusty workspace; Studio does not host a separate copy.</p><button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button></div>
      : catalog.isLoading ? <div className={styles.loading}>Loading prompts…</div>
      : catalog.isError ? <div className="empty-state"><h2>Prompt history is unavailable</h2><p>{catalog.error instanceof Error ? catalog.error.message : "Try again."}</p><button className="primary-button" type="button" onClick={() => catalog.refetch()}>Retry</button></div>
      : <div className={styles.workspace}>
        <aside className={styles.library}><header><span className="eyebrow">Library</span><h2>{catalog.data?.length ?? 0} prompt{catalog.data?.length === 1 ? "" : "s"}</h2></header>{catalog.data?.length ? <nav aria-label="Prompt artifacts">{catalog.data.map((artifact) => <button type="button" key={artifact.surface} aria-current={!creating && selectedName === promptName(artifact.surface) ? "page" : undefined} onClick={() => selectArtifact(artifact)}><b>{promptName(artifact.surface)}</b><span>{artifact.commits.length} version{artifact.commits.length === 1 ? "" : "s"}</span></button>)}</nav> : <p>No prompts yet. Create one after reviewing a Work run.</p>}</aside>
        <main className={styles.editor}>
          <header className={styles.editorHead}><div><span className="eyebrow">{creating ? "New prompt" : "Prompt workshop"}</span><h2>{creating ? "Start with one durable instruction" : selectedName || "Choose a prompt"}</h2></div>{!creating && history.data?.commits.length ? <label>Version<select aria-label="Prompt version" value={selectedVersion} onChange={(event) => setSelectedVersion(event.target.value)}>{[...history.data.commits].reverse().map((commit, index) => <option key={commit.candidate_id} value={commit.candidate_id}>v{history.data.commits.length - index} · {short(commit.candidate_id)}</option>)}</select></label> : null}</header>
          {(creating || selectedName) ? <div className={styles.editGrid}>
            <section className={styles.formPane}><label>Prompt name<input value={name} readOnly={!creating} onChange={(event) => { setName(event.target.value); setSaved(""); setError(""); }} placeholder="support-system" /></label><label>Author<input value={author} onChange={(event) => { setAuthor(event.target.value); setSaved(""); setError(""); }} placeholder="Your name or team identity" /></label><label>Evidence run<select value={sourceRun} onChange={(event) => { setSourceRun(event.target.value); setSaved(""); setError(""); }}><option value="">Choose the completed run that informed this version</option>{sourceRuns.map((item) => <option key={item.run.run_id} value={item.run.run_id}>{item.agentName} · {short(item.run.run_id)}</option>)}</select></label><label>Instructions<textarea rows={18} value={draft} onChange={(event) => { setDraft(event.target.value); setSaved(""); setError(""); }} placeholder="You are a careful support agent…" /></label></section>
            <aside className={styles.review}><span className="eyebrow">Review changes</span><h3>{changes.changed ? `${changes.added} added · ${changes.removed} removed` : creating ? "New prompt" : "No text changes"}</h3><div className={styles.versionMeta}><span>Base</span><code>{selectedVersion ? short(selectedVersion) : "No previous version"}</code><span>Evidence run</span><code>{sourceRun ? short(sourceRun) : "Not selected"}</code></div><pre aria-label="Prompt preview">{draft || "Your prompt will appear here."}</pre>{error && <p className={styles.error} role="alert">{error}</p>}{uncertainty && <div className={styles.error} role="alert"><p>{uncertainty}</p><button type="button" onClick={() => { mutationState.clearUncertain(durableMutationScope); setError(""); }}>I checked history — allow another save</button></div>}{saved && <p className={styles.success} role="status">Version {short(saved)} is in the prompt history.</p>}<button className="primary-button" type="button" disabled={save.isPending || Boolean(uncertainty) || (!creating && !changes.changed)} onClick={() => save.mutate()}>{save.isPending ? "Saving version…" : uncertainty ? "Save locked" : "Save version"}</button><p className={styles.boundary}>Saving creates an immutable prompt version. It does not promote it into serving traffic.</p></aside>
          </div> : <div className={styles.blank}><h2>Choose a prompt or create one</h2><p>The editor keeps version history and the current draft in one place.</p></div>}
        </main>
      </div>}
  </section>;
}

function lineChanges(base: string, draft: string) {
  const before = base.split("\n"), after = draft.split("\n");
  const beforeCounts = new Map<string, number>(), afterCounts = new Map<string, number>();
  before.forEach((line) => beforeCounts.set(line, (beforeCounts.get(line) ?? 0) + 1));
  after.forEach((line) => afterCounts.set(line, (afterCounts.get(line) ?? 0) + 1));
  const removed = [...beforeCounts].reduce((sum, [line, count]) => sum + Math.max(0, count - (afterCounts.get(line) ?? 0)), 0);
  const added = [...afterCounts].reduce((sum, [line, count]) => sum + Math.max(0, count - (beforeCounts.get(line) ?? 0)), 0);
  return { added, removed, changed: base !== draft };
}
