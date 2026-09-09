// Guided setup (R-A4, handoff 03 "Guided"): the three-step wizard —
// 1 Template · 2 Configure · 3 Autonomy — producing an AgentDraft that lands
// in the draft store (R-A9) and converges on Review with every other entry
// path (R-A2). Publish never happens here.

import { useState } from "react";
import { Button } from "../controls/controls";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import { blankDraft } from "../draft/defaults";
import { newDraftId, saveDraft, useDraftAutosave, type DraftRecord } from "../draft/store";
import { useLiveValidation, VALIDATION_DEBOUNCE_MS } from "../draft/useLiveValidation";
import type { AgentDraft } from "../draft/agent-draft.gen";
import { AutonomyStep } from "./AutonomyStep";
import { ConfigureStep } from "./ConfigureStep";
import { TemplateGallery } from "./TemplateGallery";
import type { SecretProbe } from "./slots";
import { draftFromTemplate, type Template } from "./templates";
import styles from "./guided.module.css";

const STEPS = ["1 Template", "2 Configure", "3 Autonomy"] as const;

export interface GuidedScreenProps {
  templates: Template[];
  catalogs?: DraftCatalogs;
  /** Injectable for tests; defaults to window.localStorage. */
  storage?: Storage;
  /** Wire-probe status per connector id, shown on credential slots. */
  probes?: Record<string, SecretProbe>;
  /** Test hook — defaults to the R-A3 pause-typing budget. */
  validationDebounceMs?: number;
  /** ← Agents. */
  onExit?: () => void;
  /** The draft is in the store; Review takes over (R-A2). */
  onReview?: (record: DraftRecord) => void;
}

export function GuidedScreen(props: GuidedScreenProps) {
  const {
    templates,
    catalogs = EMPTY_CATALOGS,
    storage,
    probes,
    validationDebounceMs = VALIDATION_DEBOUNCE_MS,
    onExit,
    onReview,
  } = props;

  const [record, setRecord] = useState<DraftRecord | null>(null);
  const [template, setTemplate] = useState<Template | null>(null);
  const [step, setStep] = useState<0 | 1 | 2>(0);
  const draft = record?.draft ?? blankDraft("guided");
  const { violations } = useLiveValidation(draft, catalogs, validationDebounceMs);
  useDraftAutosave(record, storage);

  const choose = (chosen: Template | null) => {
    const seeded = chosen ? draftFromTemplate(chosen) : { ...blankDraft("guided"), template: null };
    const now = new Date().toISOString();
    const created: DraftRecord = { id: newDraftId(), draft: seeded, createdAt: now, updatedAt: now };
    // Lands in the draft store immediately — closing the tab mid-flow loses nothing (R-A9).
    saveDraft(created, storage, now);
    setRecord(created);
    setTemplate(chosen);
    setStep(1);
  };

  const change = (next: AgentDraft) => {
    setRecord((current) => (current ? { ...current, draft: next } : current));
  };

  const review = () => {
    if (!record) return;
    const saved = saveDraft(record, storage);
    onReview?.(saved);
  };

  return (
    <div className={styles.screen}>
      <header className={styles.header}>
        <button type="button" className={styles.back} onClick={onExit}>
          ← Agents
        </button>
        <h1 className={styles.title}>Guided setup</h1>
        <nav className={styles.steps} aria-label="Steps">
          {STEPS.map((label, i) => (
            <button
              type="button"
              key={label}
              className={`${styles.stepPill} ${i === step ? styles.stepPillActive : ""}`}
              disabled={record === null || i > step}
              onClick={() => setStep(i as 0 | 1 | 2)}
            >
              {label}
            </button>
          ))}
        </nav>
      </header>
      {step === 0 && <TemplateGallery templates={templates} onChoose={choose} />}
      {step === 1 && (
        <>
          <ConfigureStep
            template={template}
            draft={draft}
            onChange={change}
            violations={violations}
            catalogs={catalogs}
            probes={probes}
          />
          <div className={styles.footer}>
            <Button variant="secondary" onClick={() => setStep(0)}>
              Back
            </Button>
            <Button variant="primary" onClick={() => setStep(2)}>
              Next · Autonomy
            </Button>
          </div>
        </>
      )}
      {step === 2 && (
        <AutonomyStep draft={draft} onChange={change} catalogs={catalogs} onBack={() => setStep(1)} onReview={review} />
      )}
    </div>
  );
}
