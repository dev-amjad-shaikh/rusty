// Review — the convergence screen (R-A2, handoff 03). Every entry path lands
// here; this is the only place Publish exists. Left rail is the field-level
// diff against the published head; right rail carries validation, the
// assembled prompt, the eval gate, Publish, and export.

import { useEffect, useMemo, useRef, useState } from "react";
import type { AgentDraft } from "../draft/agent-draft.gen";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import type { DraftRecord } from "../draft/store";
import { validateDraft } from "../draft/validate";
import type { RestClient } from "../api/rest";
import { ProblemError } from "../api/rest";
import { Badge, Button, StatusDot } from "../controls/controls";
import { diffDraft, nextVersion, versionLabel, type DiffRow } from "./diff";
import { assemblePrompt } from "./prompt";
import {
  GATE_LABEL,
  INITIAL_GATE,
  contentHash,
  gateCompleted,
  gateStarted,
  gateStatus,
  type GateState,
  type GateStatus,
} from "./gate";
import { buildExportBundle, type ExportBundle } from "./bundle";
import {
  canPublish,
  getEvalRun,
  publishDraft,
  publishReadiness,
  runEvalGate,
  startFleetUpgrade,
  type PublishResult,
} from "./publish";
import styles from "./review.module.css";

export interface ReviewScreenProps {
  record: DraftRecord;
  catalogs?: DraftCatalogs;
  scopes: readonly string[];
  rest: RestClient;
  onBack?: () => void;
  onEditSpec?: (record: DraftRecord) => void;
  onAskBuilder?: (record: DraftRecord) => void;
  onPlayground?: (record: DraftRecord) => void;
  /** Violation click-through: open Compose at the owning spec file. */
  onOpenSpecFile?: (specFile: string) => void;
  onPublished?: (result: PublishResult) => void;
  /** Export hook; defaults to a browser download of the joined bundle. */
  onExportBundle?: (bundle: ExportBundle) => void;
  /** Gate polling cadence; inject a small value in tests. */
  pollIntervalMs?: number;
  /** Clock for the prompt's volatile tier; fixed in tests. */
  now?: string;
}

const GATE_TONE: Record<GateStatus, "ok" | "warn" | "err" | "neutral"> = {
  not_run: "neutral",
  running: "warn",
  passing: "ok",
  stale: "warn",
  failing: "err",
};

const SIGN_TONE: Record<DiffRow["sign"], "ok" | "warn" | "err"> = {
  "+": "ok",
  "~": "warn",
  "-": "err",
};

function defaultExport(bundle: ExportBundle): void {
  const text = bundle.files.map((f) => `--- ${f.path} ---\n${f.content}`).join("\n");
  const blob = new Blob([text], { type: "text/plain" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = "agent.rustyprint.txt";
  anchor.click();
  URL.revokeObjectURL(url);
}

function DiffRowView({ row }: { row: DiffRow }) {
  return (
    <li className={styles.diffRow}>
      <span className={`${styles.sign} ${styles[`sign${row.sign === "+" ? "Add" : row.sign === "~" ? "Change" : "Remove"}`]}`}>
        {row.sign}
      </span>
      <span className={styles.diffField}>{row.field}</span>
      <span className={styles.diffValue}>
        {row.oldValue !== undefined && <s className={styles.oldValue}>{row.oldValue}</s>}
        {row.sign === "-" ? row.value : row.value || "—"}
      </span>
    </li>
  );
}

export function ReviewScreen(props: ReviewScreenProps) {
  const { record, rest, scopes } = props;
  const catalogs = props.catalogs ?? EMPTY_CATALOGS;
  const draft = record.draft;

  const sections = useMemo(() => diffDraft(draft), [draft]);
  const violations = useMemo(() => validateDraft(draft, catalogs), [draft, catalogs]);
  const prompt = useMemo(() => assemblePrompt(draft, catalogs, props.now), [draft, catalogs, props.now]);
  const hash = useMemo(() => contentHash(draft), [draft]);
  const governanceSections = sections.filter((s) => s.governance);

  const [gate, setGate] = useState<GateState>(INITIAL_GATE);
  const status = gateStatus(gate, hash);
  const [showPrompt, setShowPrompt] = useState(false);
  const [copied, setCopied] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [published, setPublished] = useState<PublishResult | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const pollTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => () => {
    if (pollTimer.current) clearTimeout(pollTimer.current);
  }, []);

  const readiness = publishReadiness({
    violations,
    gate: status,
    scopes,
    agentId: draft.agentId,
  });
  const publishScopeHeld = canPublish(scopes, draft.agentId);

  async function runGate() {
    setNotice(null);
    try {
      const { data } = await runEvalGate(rest, draft.gateSuite, draft);
      setGate((state) => gateStarted(state, data.run_id));
      const poll = async () => {
        try {
          const report = await getEvalRun(rest, data.run_id);
          if (report.status === "running") {
            setGate((state) => ({
              ...state,
              cases: report.cases ?? state.cases,
            }));
            pollTimer.current = setTimeout(poll, props.pollIntervalMs ?? 1_500);
            return;
          }
          setGate(() =>
            gateCompleted(INITIAL_GATE, hash, report.status === "passed", report.cases ?? []),
          );
        } catch {
          setGate((state) => ({ ...state, runId: null }));
          setNotice("The gate run could not be read — try again.");
        }
      };
      pollTimer.current = setTimeout(poll, props.pollIntervalMs ?? 1_500);
    } catch {
      setNotice("The eval gate could not be started.");
    }
  }

  async function publish() {
    setPublishing(true);
    setNotice(null);
    try {
      const { data } = await publishDraft(rest, draft);
      setPublished(data);
      setConfirming(false);
      props.onPublished?.(data);
    } catch (error) {
      if (error instanceof ProblemError && error.status === 409) {
        setNotice("The published head moved while you were reviewing — re-diff before publishing.");
      } else {
        setNotice("Publish failed — nothing was appended.");
      }
    } finally {
      setPublishing(false);
    }
  }

  function onPublishClick() {
    if (!readiness.ready || publishing) return;
    if (governanceSections.length > 0 && !confirming) {
      setConfirming(true);
      return;
    }
    void publish();
  }

  async function copyPrompt() {
    try {
      await navigator.clipboard.writeText(prompt.text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1_500);
    } catch {
      setNotice("Copy failed — select the prompt text instead.");
    }
  }

  function exportBundle() {
    const bundle = buildExportBundle(draft, catalogs, props.now);
    if (!bundle.scan.ok) {
      setExportError(
        `Export blocked: the no-secret-values scan found ${bundle.scan.findings.length} problem(s).`,
      );
      return;
    }
    setExportError(null);
    (props.onExportBundle ?? defaultExport)(bundle);
  }

  return (
    <div className={styles.screen}>
      <header className={styles.header}>
        <Button variant="ghost" small onClick={props.onBack} ariaLabel="Back">
          ← {draft.source}
        </Button>
        <h1 className={styles.title}>{draft.name.trim() || "Untitled draft"}</h1>
        <Badge>{versionLabel(draft)}</Badge>
        <span className={styles.headerActions}>
          <Button variant="secondary" small onClick={() => props.onEditSpec?.(record)}>
            Edit spec
          </Button>
          <Button variant="secondary" small onClick={() => props.onAskBuilder?.(record)}>
            Ask builder
          </Button>
          <Button variant="secondary" small onClick={() => props.onPlayground?.(record)}>
            Playground
          </Button>
        </span>
      </header>

      <div className={styles.columns}>
        <section className={styles.changes} aria-label="Changes">
          {sections.length === 0 ? (
            <p className={styles.empty}>No changes against the published head.</p>
          ) : (
            sections.map((section) => (
              <article key={section.id} className={styles.card}>
                <div className={styles.cardHeader}>
                  <h2 className={styles.cardTitle}>{section.label}</h2>
                  {section.governance && <Badge tone="warn">GOVERNANCE</Badge>}
                </div>
                <ul className={styles.diffList}>
                  {section.rows.map((row, i) => (
                    <DiffRowView key={`${row.field}-${i}`} row={row} />
                  ))}
                </ul>
              </article>
            ))
          )}
        </section>

        <aside className={styles.rail}>
          <article className={styles.card} aria-label="Validation">
            <div className={styles.cardHeader}>
              <h2 className={styles.cardTitle}>Validation</h2>
              {violations.length === 0 ? (
                <Badge tone="ok">Passing</Badge>
              ) : (
                <Badge tone="err">{violations.length} open</Badge>
              )}
            </div>
            {violations.length > 0 && (
              <ul className={styles.violationList}>
                {violations.map((violation) => (
                  <li key={`${violation.path}-${violation.rule}`}>
                    <button
                      type="button"
                      className={styles.violation}
                      onClick={() => props.onOpenSpecFile?.(violation.specFile)}
                    >
                      <span className={styles.violationPath}>{violation.path}</span>
                      <span>{violation.message}</span>
                      <span className={styles.violationFile}>{violation.specFile}</span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </article>

          <article className={styles.card} aria-label="Assembled prompt">
            <div className={styles.cardHeader}>
              <h2 className={styles.cardTitle}>Assembled prompt</h2>
              <span className={styles.meta}>{prompt.bytes} bytes</span>
            </div>
            <div className={styles.cardActions}>
              <Button variant="secondary" small onClick={copyPrompt}>
                {copied ? "Copied" : "Copy"}
              </Button>
              <Button variant="ghost" small onClick={() => setShowPrompt((show) => !show)}>
                {showPrompt ? "Hide" : "Show"}
              </Button>
            </div>
            {showPrompt && (
              <div className={styles.tiers}>
                <pre className={styles.tier} data-tier="stable">{prompt.stable}</pre>
                <pre className={styles.tier} data-tier="context">{prompt.context}</pre>
                <pre className={styles.tier} data-tier="volatile">{prompt.volatile}</pre>
              </div>
            )}
          </article>

          <article className={styles.card} aria-label="Eval gate">
            <div className={styles.cardHeader}>
              <h2 className={styles.cardTitle}>Eval gate</h2>
              <StatusDot tone={GATE_TONE[status]} label={GATE_LABEL[status]} />
            </div>
            <p className={styles.meta}>
              {draft.gateSuite ? `${draft.gateSuite} · ${gate.cases.length} case${gate.cases.length === 1 ? "" : "s"}` : "No gate suite named"}
            </p>
            {gate.cases.length > 0 && (
              <ul className={styles.caseList}>
                {gate.cases.map((c) => (
                  <li key={c.name} className={styles.caseRow}>
                    <span className={c.pass ? styles.signAdd : styles.signRemove}>{c.pass ? "✓" : "✗"}</span>
                    <span className={styles.caseName}>{c.name}</span>
                    <span className={styles.meta}>{c.score.toFixed(2)}</span>
                  </li>
                ))}
              </ul>
            )}
            <div className={styles.cardActions}>
              <Button
                variant="secondary"
                small
                disabled={!draft.gateSuite || status === "running"}
                onClick={() => void runGate()}
              >
                {status === "not_run" ? "Run gate" : "Re-run"}
              </Button>
            </div>
          </article>

          {notice && <p className={styles.notice} role="alert">{notice}</p>}

          {published ? (
            <article className={styles.card} aria-label="Published">
              <div className={styles.cardHeader}>
                <h2 className={styles.cardTitle}>Published v{published.version}</h2>
                <Badge tone="ok">immutable</Badge>
              </div>
              {published.sessions !== undefined && published.sessions > 0 ? (
                <div className={styles.fleet}>
                  <p className={styles.meta}>
                    {published.sessions} live session{published.sessions === 1 ? "" : "s"} on the prior
                    version — adoption happens at turn boundaries.
                  </p>
                  <div className={styles.cardActions}>
                    <Button
                      variant="secondary"
                      small
                      onClick={() =>
                        void startFleetUpgrade(rest, published.blueprint_id, published.version)
                          .then(() => setNotice("Fleet upgrade started."))
                          .catch(() => setNotice("Fleet upgrade could not be started."))
                      }
                    >
                      Start upgrade
                    </Button>
                  </div>
                </div>
              ) : (
                <p className={styles.meta}>No live sessions to upgrade.</p>
              )}
            </article>
          ) : publishScopeHeld ? (
            <div className={styles.publishBlock}>
              {confirming && (
                <div className={styles.confirm} role="alert">
                  <p className={styles.meta}>
                    Governance-significant changes in{" "}
                    {governanceSections.map((s) => s.label).join(", ")} — confirming logs this
                    publish.
                  </p>
                </div>
              )}
              <Button
                variant="primary"
                disabled={!readiness.ready || publishing}
                onClick={onPublishClick}
              >
                {publishing ? "Publishing…" : confirming ? "Confirm and publish" : `Publish v${nextVersion(draft)}`}
              </Button>
              {!readiness.ready && (
                <ul className={styles.hints}>
                  {readiness.reasons.map((reason) => (
                    <li key={reason} className={styles.meta}>{reason}</li>
                  ))}
                </ul>
              )}
            </div>
          ) : (
            <p className={styles.meta}>
              Publish needs the publish scope for this blueprint — ask an admin.
            </p>
          )}

          <Button variant="secondary" onClick={exportBundle}>
            Export .rustyprint bundle
          </Button>
          {exportError && <p className={styles.notice} role="alert">{exportError}</p>}
        </aside>
      </div>
    </div>
  );
}
