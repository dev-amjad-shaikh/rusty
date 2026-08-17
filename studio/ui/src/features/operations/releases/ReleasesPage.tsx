import { useMemo, useState } from "react";
import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useLocation, useNavigate, useParams } from "@tanstack/react-router";
import {
  clearCanary,
  createRevision,
  declareCanary,
  declareEnvironment,
  getDeploymentHealth,
  getDeploymentJournal,
  getEnvironmentPointer,
  listEnvironments,
  listRevisions,
  listSecrets,
  promoteRevision,
  rollbackRevision,
  type DeploymentEnvironment,
  type DeploymentEvent,
  type DeploymentHealth,
  type DeploymentPointer,
  type DeploymentRevision,
} from "../../../lib/api/deployments";
import { evidencePreview, shortId } from "../../../lib/text";
import styles from "./ReleasesPage.module.css";

const author = (humanId: string) => ({ type: "human" as const, human_id: humanId });

const modes = [
  { to: "/operations" as const, label: "Attention" },
  { to: "/operations/releases" as const, label: "Releases" },
  { to: "/operations" as const, hash: "systems", label: "Systems" },
];

function ModeNav() {
  const { pathname, hash } = useLocation({ select: (location) => ({ pathname: location.pathname, hash: location.hash }) });
  return (
    <nav className={styles.modeNav} aria-label="Operations modes">
      {modes.map((mode) => (
        <Link
          key={mode.label}
          to={mode.to}
          hash={mode.hash}
          aria-current={pathname === mode.to && (mode.hash ? hash === `#${mode.hash}` : true) ? "page" : undefined}
        >
          {mode.label}
        </Link>
      ))}
    </nav>
  );
}

function useReleaseQueries(enabled: boolean) {
  const baseKey = ["releases"];
  const health = useQuery({
    queryKey: [...baseKey, "deployment-health"],
    queryFn: () => getDeploymentHealth(),
    enabled,
    refetchInterval: 15_000,
  });
  const revisions = useQuery({
    queryKey: [...baseKey, "revisions"],
    queryFn: () => listRevisions(),
    enabled,
  });
  const journal = useQuery({
    queryKey: [...baseKey, "deployment-journal"],
    queryFn: () => getDeploymentJournal(),
    enabled,
    refetchInterval: 15_000,
  });
  const environments = useQuery({
    queryKey: [...baseKey, "environments"],
    queryFn: () => listEnvironments(),
    enabled,
  });
  return { health, revisions, journal, environments };
}

type ReleaseAction =
  | { kind: "promote"; revisionId: string }
  | { kind: "canary"; revisionId: string }
  | { kind: "rollback" }
  | { kind: "clear-canary" };

function DecisionPanel({
  env,
  pointer,
  selectedRevision,
  board,
  onAction,
}: {
  env: DeploymentEnvironment;
  pointer: DeploymentPointer;
  selectedRevision: DeploymentRevision | null;
  board: DeploymentHealth["environments"][number] | undefined;
  onAction: (action: ReleaseAction) => void;
}) {
  const [ack, setAck] = useState(false);
  const [humanId, setHumanId] = useState("");

  const currentState = useMemo(() => {
    if (!board) return { label: "Loading environment state", action: null as ReleaseAction | null };
    if (board.canary) {
      return {
        label: `Canary serving ${Math.round(board.canary.fraction * 100)}%`,
        action: { kind: "promote" as const, revisionId: board.canary.revision_id },
        secondary: { kind: "clear-canary" as const },
      };
    }
    if (board.active_revision) {
      if (selectedRevision && selectedRevision.revision_id !== board.active_revision) {
        return {
          label: `Ready to promote ${shortId(selectedRevision.revision_id)} to ${env.name}`,
          action: { kind: "promote" as const, revisionId: selectedRevision.revision_id },
        };
      }
      if (pointer.active && env.name) {
        return {
          label: `Active revision ${shortId(board.active_revision)} can roll back`,
          action: { kind: "rollback" as const },
        };
      }
      return { label: `Revision ${shortId(board.active_revision)} is active in ${env.name}`, action: null as ReleaseAction | null };
    }
    if (selectedRevision) {
      return {
        label: `Ready to promote ${shortId(selectedRevision.revision_id)} to ${env.name}`,
        action: { kind: "promote" as const, revisionId: selectedRevision.revision_id },
      };
    }
    return { label: `Nothing serves ${env.name}`, action: null as ReleaseAction | null };
  }, [board, env.name, env.name, pointer.active, selectedRevision]);

  const action = currentState.action;
  const secondary = (currentState as { secondary?: ReleaseAction }).secondary;
  const needsAck = action && (action.kind === "promote" || action.kind === "rollback" || action.kind === "canary");

  return (
    <section className={styles.decision} aria-labelledby="release-decision-heading">
      <header>
        <span className="eyebrow">Current decision</span>
        <h2 id="release-decision-heading">{currentState.label}</h2>
      </header>
      {env.approval_required && (
        <p className={styles.approvalNotice}>This environment requires an approval token. Studio cannot mint approvals; use the Rusty CLI or API.</p>
      )}
      {action ? (
        <div className={styles.actionForm}>
          <label>Author<input type="text" value={humanId} onChange={(e) => setHumanId(e.target.value)} placeholder="operator id" /></label>
          {needsAck && (
            <label className={styles.ack}>
              <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />
              I understand this {action.kind === "promote" ? "sends all traffic to the selected revision" : action.kind === "rollback" ? "reverts to the previous revision" : action.kind === "canary" ? "sends a fraction of traffic to a candidate" : "clears the canary"}.
            </label>
          )}
          <div className={styles.actionRow}>
            <button
              type="button"
              className="primary-button"
              disabled={!humanId || (needsAck && !ack) || env.approval_required}
              onClick={() => onAction(action)}
            >
              {action.kind === "promote" ? "Promote" : action.kind === "rollback" ? "Roll back" : action.kind === "canary" ? "Start canary" : "Clear canary"}
            </button>
            {secondary && (
              <button type="button" className="secondary-button" disabled={!humanId} onClick={() => onAction(secondary)}>Clear canary</button>
            )}
          </div>
        </div>
      ) : (
        <p className={styles.noAction}>Select a revision to prepare a promotion, or use the environment list.</p>
      )}
    </section>
  );
}

function EvidenceSpine({ board, revision }: { board: DeploymentHealth["environments"][number] | undefined; revision: DeploymentRevision | null }) {
  if (!board) return null;
  const steps = [
    { label: "Agent version", state: revision?.content.assistant ? "set" : "unknown", value: revision?.content.assistant },
    { label: "Run", state: "unknown", value: null },
    { label: "Evaluation", state: "unknown", value: null },
    { label: "Revision", state: revision ? "ready" : "unknown", value: revision?.revision_id },
    { label: "Environment", state: board.active_revision ? "active" : revision ? "ready" : "unknown", value: board.environment },
  ];
  return (
    <nav className={styles.spine} aria-label="Evidence spine">
      <ol>{steps.map((step) => (
        <li key={step.label} data-state={step.state}>
          <span>{step.label}</span>
          {step.value && <code>{shortId(String(step.value))}</code>}
        </li>
      ))}</ol>
    </nav>
  );
}

function EnvironmentList({
  environments,
  selected,
  onSelect,
}: {
  environments: DeploymentEnvironment[] | undefined;
  selected: string | undefined;
  onSelect: (name: string) => void;
}) {
  return (
    <section className={styles.envList} aria-labelledby="env-list-heading">
      <header><span className="eyebrow">Environments</span><h2 id="env-list-heading">Declared targets</h2></header>
      {!environments?.length ? <p className={styles.emptyList}>No environments declared.</p> : (
        <ul>
          {environments.map((env) => (
            <li key={env.name}>
              <button
                type="button"
                aria-current={selected === env.name ? "true" : undefined}
                className={selected === env.name ? styles.selected : ""}
                onClick={() => onSelect(env.name)}
              >
                <b>{env.name}</b>
                {env.gate && <span>Gate: {env.gate.policy}</span>}
                {env.approval_required && <span>Approval required</span>}
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function EnvironmentInspector({ env, board, secrets }: { env: DeploymentEnvironment; board: DeploymentHealth["environments"][number] | undefined; secrets: { data: DeploymentEvent[] } }) {
  return (
    <section className={styles.inspector} aria-labelledby="env-inspector-heading">
      <header><span className="eyebrow">Evidence</span><h2 id="env-inspector-heading">{env.name}</h2></header>
      <dl>
        <div><dt>Gate policy</dt><dd>{env.gate ? `${env.gate.policy} · ${env.gate.dataset_version}` : "None"}</dd></div>
        <div><dt>Approval required</dt><dd>{env.approval_required ? "Yes" : "No"}</dd></div>
        <div><dt>Active revision</dt><dd>{board?.active_revision ? shortId(board.active_revision) : "Nothing serving"}</dd></div>
        <div><dt>Canary</dt><dd>{board?.canary ? `${shortId(board.canary.revision_id)} at ${Math.round(board.canary.fraction * 100)}%` : "No canary"}</dd></div>
        <div><dt>Recent active runs</dt><dd>{board ? `${board.recent_runs.active.runs} · ${board.recent_runs.active.errored} errors · ${board.recent_runs.active.interrupted} interrupted` : "Not loaded"}</dd></div>
      </dl>
    </section>
  );
}

function DeploymentTimeline({ events, selectedId, onSelect }: { events: DeploymentEvent[]; selectedId?: string; onSelect?: (id: string) => void }) {
  return (
    <section className={styles.timeline} aria-labelledby="timeline-heading">
      <header><span className="eyebrow">Deployment timeline</span><h2 id="timeline-heading">Every control-plane decision</h2></header>
      {!events.length ? <p className={styles.emptyList}>No deployment events yet.</p> : (
        <ol>
          {events.map((event) => {
            const summary = eventSummary(event);
            return (
              <li key={event.id} className={selectedId === event.id ? styles.selectedEvent : ""}>
                <button type="button" onClick={() => onSelect?.(event.id)}>
                  <time>{new Date(event.recorded_at).toLocaleString()}</time>
                  <strong>{summary.title}</strong>
                  <span>{summary.detail}</span>
                </button>
              </li>
            );
          })}
        </ol>
      )}
    </section>
  );
}

function eventSummary(event: DeploymentEvent): { title: string; detail: string } {
  // Journal outputs are PayloadRef-tagged (`{kind, value}`); the summary
  // reads the payload itself.
  const ref = event.output as { value?: unknown } | null;
  const out = (ref && typeof ref === "object" && "value" in ref ? ref.value : ref) as Record<string, unknown> | null;
  const env = out && typeof out === "object" ? String((out.environment || out.declaration && typeof out.declaration === "object" && (out.declaration as { environment?: unknown }).environment) ?? "") : "";
  switch (event.kind) {
    case "revision_registered": return { title: "Revision registered", detail: out && typeof out === "object" ? `revision ${shortId(String((out.revision as { revision_id?: unknown })?.revision_id ?? ""))}` : "" };
    case "environment_declared": return { title: "Environment declared", detail: env ? `environment ${env}` : "" };
    case "gate_decision_recorded": return { title: "Gate decision", detail: out && typeof out === "object" ? String((out.decision as { allowed?: unknown })?.allowed ? "allowed" : "blocked") : "" };
    case "canary_declared": return { title: "Canary started", detail: env ? `${env}` : "" };
    case "canary_cleared": return { title: "Canary cleared", detail: env ? `${env}` : "" };
    case "revision_promoted": return { title: "Revision promoted", detail: env ? `${env}` : "" };
    case "revision_rolled_back": return { title: "Revision rolled back", detail: env ? `${env}` : "" };
    case "shadow_run_started": return { title: "Shadow started", detail: "" };
    case "shadow_verdict": {
      const outcome = out && typeof out === "object" ? out.outcome : null;
      return { title: "Shadow verdict", detail: typeof outcome === "string" ? outcome : outcome ? "failed" : "" };
    }
    case "env_secret_set": return { title: "Secret set", detail: out && typeof out === "object" ? String((out.record as { name?: unknown })?.name ?? "") : "" };
    case "env_secret_revoked": return { title: "Secret revoked", detail: out && typeof out === "object" ? String((out.record as { name?: unknown })?.name ?? "") : "" };
    case "env_secret_denied": return { title: "Secret denied", detail: "" };
    default: return { title: event.kind.replace(/_/g, " "), detail: "" };
  }
}

function DeclareEnvironmentDialog({ open, onClose, onDeclare }: { open: boolean; onClose: () => void; onDeclare: (created: boolean) => void }) {
  const [name, setName] = useState("");
  const [policy, setPolicy] = useState("");
  const [dataset, setDataset] = useState("");
  const [approval, setApproval] = useState(false);
  const [authorId, setAuthorId] = useState("");
  const [error, setError] = useState("");
  const declare = useMutation({
    mutationFn: () => declareEnvironment({
      name: name.trim(),
      gate: policy.trim() && dataset.trim() ? { policy: policy.trim(), dataset_version: dataset.trim() } : null,
      approval_required: approval,
      author: author(authorId.trim() || "studio"),
    }),
    onSuccess: ({ created }) => { setName(""); setPolicy(""); setDataset(""); setApproval(false); setError(""); onDeclare(created); },
    onError: (caught) => setError(caught instanceof Error ? caught.message : "Declaration failed."),
  });
  if (!open) return null;
  return (
    <div className={styles.dialogBackdrop} role="dialog" aria-modal="true" aria-labelledby="declare-env-title">
      <div className={styles.dialog}>
        <header><h2 id="declare-env-title">Create environment</h2><button type="button" onClick={onClose}>Close</button></header>
        <div className={styles.dialogBody}>
          {error && <p className={styles.error} role="alert">{error}</p>}
          <label>Name <input type="text" value={name} onChange={(e) => setName(e.target.value)} placeholder="staging" /></label>
          <label>Gate policy <input type="text" value={policy} onChange={(e) => setPolicy(e.target.value)} placeholder="r0.12-default" /></label>
          <label>Dataset version <input type="text" value={dataset} onChange={(e) => setDataset(e.target.value)} placeholder="support-v3" /></label>
          <label className={styles.chk}><input type="checkbox" checked={approval} onChange={(e) => setApproval(e.target.checked)} /> Require human approval for promotion</label>
          <label>Author <input type="text" value={authorId} onChange={(e) => setAuthorId(e.target.value)} placeholder="operator id" /></label>
          <p className={styles.review}>Environment declarations are immutable. A conflicting name with different rules will fail.</p>
        </div>
        <footer>
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button type="button" className="primary-button" disabled={!name.trim() || declare.isPending} onClick={() => declare.mutate()}>{declare.isPending ? "Declaring…" : "Create environment"}</button>
        </footer>
      </div>
    </div>
  );
}

function CreateRevisionDialog({
  open,
  onClose,
  environments,
  onCreated,
}: {
  open: boolean;
  onClose: () => void;
  environments: DeploymentEnvironment[] | undefined;
  onCreated: () => void;
}) {
  const [graph, setGraph] = useState("");
  const [sourceEnv, setSourceEnv] = useState("");
  const [surfaces, setSurfaces] = useState("");
  const [authorId, setAuthorId] = useState("");
  const [error, setError] = useState("");
  const create = useMutation({
    mutationFn: () => createRevision({
      graph: graph.trim(),
      source_environment: sourceEnv,
      surfaces: surfaces.split(",").map((s) => s.trim()).filter(Boolean),
      author: author(authorId.trim() || "studio"),
    }),
    onSuccess: () => { setGraph(""); setSourceEnv(""); setSurfaces(""); setError(""); onCreated(); },
    onError: (caught) => setError(caught instanceof Error ? caught.message : "Revision creation failed."),
  });
  if (!open) return null;
  return (
    <div className={styles.dialogBackdrop} role="dialog" aria-modal="true" aria-labelledby="create-rev-title">
      <div className={styles.dialog}>
        <header><h2 id="create-rev-title">Prepare revision</h2><button type="button" onClick={onClose}>Close</button></header>
        <div className={styles.dialogBody}>
          {error && <p className={styles.error} role="alert">{error}</p>}
          <label>Graph <input type="text" value={graph} onChange={(e) => setGraph(e.target.value)} placeholder="pipeline" /></label>
          <label>Source environment
            <select value={sourceEnv} onChange={(e) => setSourceEnv(e.target.value)}>
              <option value="">Select environment</option>
              {environments?.map((env) => <option key={env.name} value={env.name}>{env.name}</option>)}
            </select>
          </label>
          <label>Registry surfaces to freeze <input type="text" value={surfaces} onChange={(e) => setSurfaces(e.target.value)} placeholder="prompt:system, tool:search" /></label>
          <label>Author <input type="text" value={authorId} onChange={(e) => setAuthorId(e.target.value)} placeholder="operator id" /></label>
          <p className={styles.review}>This freezes the current pointers from the source environment. Later registry changes will not alter this revision.</p>
        </div>
        <footer>
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button type="button" className="primary-button" disabled={!graph.trim() || !sourceEnv || create.isPending} onClick={() => create.mutate()}>{create.isPending ? "Preparing…" : "Prepare revision"}</button>
        </footer>
      </div>
    </div>
  );
}

function SecretsPanel({ environment }: { environment?: string }) {
  const secrets = useQuery({
    queryKey: ["secrets", environment ?? "idle"],
    queryFn: () => listSecrets(environment),
    enabled: Boolean(environment),
  });
  if (!environment) return null;
  return (
    <section className={styles.secrets} aria-labelledby="secrets-heading">
      <header><span className="eyebrow">Secrets</span><h2 id="secrets-heading">Environment-scoped metadata</h2></header>
      {secrets.isLoading ? <p>Loading secret metadata…</p> : secrets.isError ? <p className={styles.error}>Could not load secret metadata.</p> : !secrets.data?.length ? <p className={styles.emptyList}>No secrets declared for {environment}.</p> : (
        <ul>
          {secrets.data.map((secret) => (
            <li key={`${secret.environment}-${secret.name}`}>
              <b>{secret.name}</b>
              <span>{secret.environment}</span>
              <small>Set by {secret.set_by?.type === "human" ? secret.set_by.human_id : "system"} · {new Date(secret.created_at).toLocaleString()}</small>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

export function ReleasesPage() {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { environment?: string; revisionId?: string };
  const queryClient = useQueryClient();
  const { health, revisions, journal, environments } = useReleaseQueries(true);
  const [declareOpen, setDeclareOpen] = useState(false);
  const [createRevOpen, setCreateRevOpen] = useState(false);
  const [selectedEventId, setSelectedEventId] = useState<string | undefined>();

  const selectedEnv = params.environment;
  const env = environments.data?.find((item) => item.name === selectedEnv);
  const board = health.data?.environments.find((item) => item.environment === selectedEnv);
  const pointer = useQuery({
    queryKey: ["pointer", selectedEnv ?? "idle"],
    queryFn: () => getEnvironmentPointer(selectedEnv!),
    enabled: Boolean(selectedEnv),
  });
  const selectedRevision = useMemo(() => {
    if (!params.revisionId) return null;
    return revisions.data?.find((item) => item.revision_id === params.revisionId) ?? null;
  }, [params.revisionId, revisions.data]);

  const invalidate = () => {
    const base = ["releases"];
    queryClient.invalidateQueries({ queryKey: [...base, "deployment-health"] });
    queryClient.invalidateQueries({ queryKey: [...base, "revisions"] });
    queryClient.invalidateQueries({ queryKey: [...base, "deployment-journal"] });
    queryClient.invalidateQueries({ queryKey: [...base, "pointer", selectedEnv] });
  };

  const promote = useMutation({
    mutationFn: ({ revisionId, authorId }: { revisionId: string; authorId: string }) =>
      promoteRevision(selectedEnv!, { revision_id: revisionId, author: author(authorId) }),
    onSuccess: invalidate,
  });
  const canary = useMutation({
    mutationFn: ({ revisionId, authorId, fraction }: { revisionId: string; authorId: string; fraction: number }) =>
      declareCanary(selectedEnv!, { revision_id: revisionId, fraction, author: author(authorId) }),
    onSuccess: invalidate,
  });
  const rollback = useMutation({
    mutationFn: ({ authorId, cause }: { authorId: string; cause: string }) =>
      rollbackRevision(selectedEnv!, { author: author(authorId), cause }),
    onSuccess: invalidate,
  });
  const clear = useMutation({
    mutationFn: ({ authorId }: { authorId: string }) => clearCanary(selectedEnv!, author(authorId)),
    onSuccess: invalidate,
  });

  function handleAction(action: { kind: "promote" | "canary" | "rollback" | "clear-canary"; revisionId?: string }) {
    // A simple prompt for author/cause; in a real product this would be a modal review.
    const authorId = window.prompt("Enter operator id:")?.trim() ?? "";
    if (!authorId) return;
    if (action.kind === "promote" && action.revisionId) {
      promote.mutate({ revisionId: action.revisionId, authorId });
    } else if (action.kind === "canary" && action.revisionId) {
      const fraction = Number(window.prompt("Canary fraction (e.g. 0.1):", "0.1"));
      if (Number.isNaN(fraction)) return;
      canary.mutate({ revisionId: action.revisionId, authorId, fraction });
    } else if (action.kind === "rollback") {
      const cause = window.prompt("Rollback cause:")?.trim() ?? "operator initiated";
      rollback.mutate({ authorId, cause });
    } else if (action.kind === "clear-canary") {
      clear.mutate({ authorId });
    }
  }

  function selectEnvironment(name: string) {
    navigate({ to: "/operations/releases/$environment", params: { environment: name } });
  }

  function selectRevision(revisionId: string) {
    if (selectedEnv) {
      navigate({ to: "/operations/releases/$environment/revisions/$revisionId", params: { environment: selectedEnv, revisionId } });
    }
  }

  return (
    <section className="page" aria-labelledby="releases-heading">
      <header className="page-header">
        <div><span className="eyebrow">Operations</span><h1 id="releases-heading">Releases</h1><p>Move reviewed changes through environments with exact evidence.</p></div>
        <div className={styles.actions}>
          <button type="button" className="secondary-button" onClick={() => setDeclareOpen(true)}>Create environment</button>
          <button type="button" className="secondary-button" onClick={() => setCreateRevOpen(true)}>Prepare revision</button>
        </div>
      </header>
      <ModeNav />

      <div className={styles.workspace}>
            <EnvironmentList environments={environments.data} selected={selectedEnv} onSelect={selectEnvironment} />
            {env && (
              <>
                <DecisionPanel env={env} pointer={pointer.data ?? { surface: `deployment:${selectedEnv}` }} selectedRevision={selectedRevision} board={board} onAction={handleAction} />
                <EnvironmentInspector env={env} board={board} secrets={{ data: [] }} />
              </>
            )}
            {!selectedEnv && (
              <div className={styles.emptyWorkspace}><p>Select an environment to review or change what serves there.</p></div>
            )}
          </div>
          {env && <EvidenceSpine board={board} revision={selectedRevision} />}
          <section className={styles.revisions} aria-labelledby="revisions-heading">
            <header><span className="eyebrow">Revisions</span><h2 id="revisions-heading">Immutable pin sets</h2></header>
            {!revisions.data?.length ? <p className={styles.emptyList}>No revisions registered.</p> : (
              <ul>
                {revisions.data.map((rev) => (
                  <li key={rev.revision_id} className={selectedRevision?.revision_id === rev.revision_id ? styles.selectedRev : ""}>
                    <button type="button" onClick={() => selectRevision(rev.revision_id)}>
                      <code>{shortId(rev.revision_id)}</code>
                      <span>{rev.content.graph} · {rev.content.pins.map((pin) => pin.surface).join(", ") || "no pins"}</span>
                      <small>{rev.content.source_environment}</small>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </section>
          {selectedRevision && (
            <section className={styles.revisionDetail} aria-labelledby="revision-detail-heading">
              <header><span className="eyebrow">Revision</span><h2 id="revision-detail-heading">{shortId(selectedRevision.revision_id)}</h2></header>
              <dl>
                <div><dt>Graph</dt><dd>{selectedRevision.content.graph}</dd></div>
                <div><dt>Source environment</dt><dd>{selectedRevision.content.source_environment}</dd></div>
                <div><dt>Graph hash</dt><dd><code>{shortId(selectedRevision.content.graph_hash)}</code></dd></div>
                <div><dt>Frozen pins</dt><dd>{selectedRevision.content.pins.length ? selectedRevision.content.pins.map((pin) => `${pin.surface} → ${shortId(pin.candidate_id)}`).join("; ") : "None"}</dd></div>
                <div><dt>Author</dt><dd>{selectedRevision.author?.type === "human" ? selectedRevision.author.human_id : "system"}</dd></div>
              </dl>
            </section>
          )}
          <DeploymentTimeline events={journal.data?.events ?? []} selectedId={selectedEventId} onSelect={setSelectedEventId} />
          <SecretsPanel environment={selectedEnv} />
      <DeclareEnvironmentDialog open={declareOpen} onClose={() => setDeclareOpen(false)} onDeclare={() => { setDeclareOpen(false); invalidate(); }} />
      <CreateRevisionDialog open={createRevOpen} onClose={() => setCreateRevOpen(false)} environments={environments.data} onCreated={() => { setCreateRevOpen(false); invalidate(); }} />
    </section>
  );
}
