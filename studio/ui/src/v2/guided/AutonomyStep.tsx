// Autonomy step (handoff 03 "Guided" step 3): three cards — read_only /
// supervised / full — with a description each and a coherence note on
// read_only when mounted Write/Execute/Egress tools conflict with it
// (validation table, autonomy × toolsets row). Back returns to Configure;
// Review converges with every other entry path (R-A2).

import { mountedTools, EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import { Button } from "../controls/controls";
import type { AgentDraft } from "../draft/agent-draft.gen";
import styles from "./guided.module.css";

const LEVELS: { value: AgentDraft["autonomy"]; name: string; description: string }[] = [
  {
    value: "read_only",
    name: "Read only",
    description: "Reads and reports only — the agent never writes, executes, or sends.",
  },
  {
    value: "supervised",
    name: "Supervised",
    description: "Reads freely; writes, executes, and egress wait for approval.",
  },
  {
    value: "full",
    name: "Full",
    description: "Every mounted tool runs without asking.",
  },
];

export interface AutonomyStepProps {
  draft: AgentDraft;
  onChange: (draft: AgentDraft) => void;
  catalogs?: DraftCatalogs;
  onBack: () => void;
  onReview: () => void;
}

/** Mounted Write/Execute/Egress tools — what read_only autonomy conflicts with. */
export function readOnlyConflicts(
  draft: AgentDraft,
  catalogs: DraftCatalogs = EMPTY_CATALOGS,
): string[] {
  return mountedTools(draft, catalogs)
    .filter((tool) => tool.effect !== "read")
    .map((tool) => tool.id);
}

export function AutonomyStep({ draft, onChange, catalogs = EMPTY_CATALOGS, onBack, onReview }: AutonomyStepProps) {
  const conflicts = readOnlyConflicts(draft, catalogs);
  return (
    <div className={styles.screen}>
      <div className={styles.autonomyGrid}>
        {LEVELS.map((level) => (
          <button
            type="button"
            key={level.value}
            className={`${styles.autonomyCard} ${draft.autonomy === level.value ? styles.autonomyCardActive : ""}`}
            aria-pressed={draft.autonomy === level.value}
            onClick={() => onChange({ ...draft, autonomy: level.value })}
          >
            <span className={styles.autonomyName}>{level.name}</span>
            <p className={styles.autonomyDesc}>{level.description}</p>
            {level.value === "read_only" && conflicts.length > 0 && (
              <p className={styles.autonomyNote}>
                Conflicts with {conflicts.length} mounted Write/Egress tools
              </p>
            )}
          </button>
        ))}
      </div>
      <div className={styles.footer}>
        <Button variant="secondary" onClick={onBack}>
          Back
        </Button>
        <Button variant="primary" onClick={onReview}>
          Review
        </Button>
      </div>
    </div>
  );
}
