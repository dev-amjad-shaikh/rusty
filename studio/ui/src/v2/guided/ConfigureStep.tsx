// Configure step (handoff 03 "Guided" step 2): template path → slot cards
// (grid 24px 1fr: ✓/· badge, title, JSON path mono, help, control, probe
// line for credentials); blank path → the full AgentDraftForm. The right
// aside carries the live Validation list and the Template summary.

import { Badge, Pill, Select, TextInput } from "../controls/controls";
import { EMPTY_CATALOGS, type DraftCatalogs } from "../draft/catalogs";
import { AgentDraftForm } from "../draft/form";
import type { Violation } from "../draft/validate";
import type { AgentDraft } from "../draft/agent-draft.gen";
import { deriveSlots, slotFilled, withSlotViolations, type GuidedSlot, type SecretProbe } from "./slots";
import type { Template } from "./templates";
import styles from "./guided.module.css";

const CHANNEL_KINDS: { value: AgentDraft["channelKind"]; label: string }[] = [
  { value: "slack", label: "Slack" },
  { value: "teams", label: "Microsoft Teams" },
  { value: "email", label: "Email" },
  { value: "web", label: "Web widget" },
];

export interface ConfigureStepProps {
  /** null = Start blank — renders the full AgentDraftForm. */
  template: Template | null;
  draft: AgentDraft;
  onChange: (draft: AgentDraft) => void;
  /** The live report from useLiveValidation; slot violations merge in here. */
  violations: Violation[];
  catalogs?: DraftCatalogs;
  /** Wire-probe status per connector id, for credential slot probe lines. */
  probes?: Record<string, SecretProbe>;
}

export function ConfigureStep(props: ConfigureStepProps) {
  const { template, draft, onChange, violations, catalogs = EMPTY_CATALOGS, probes = {} } = props;
  const slots = template ? deriveSlots(template.document) : [];
  const merged = template ? withSlotViolations(violations, draft, slots) : violations;
  return (
    <div className={styles.configure}>
      {template ? (
        <div className={styles.slots}>
          {slots.map((slot) => (
            <SlotCard
              key={slot.path}
              slot={slot}
              draft={draft}
              onChange={onChange}
              catalogs={catalogs}
              probe={slot.probe ? probes[slot.path.slice("secrets.".length)] : undefined}
            />
          ))}
        </div>
      ) : (
        <AgentDraftForm draft={draft} onChange={onChange} violations={merged} catalogs={catalogs} />
      )}
      <aside className={styles.aside}>
        <ValidationAside violations={merged} />
        {template && <TemplateSummary template={template} slotCount={slots.length} />}
      </aside>
    </div>
  );
}

function SlotCard(props: {
  slot: GuidedSlot;
  draft: AgentDraft;
  onChange: (draft: AgentDraft) => void;
  catalogs: DraftCatalogs;
  probe?: SecretProbe;
}) {
  const { slot, draft, onChange, catalogs, probe } = props;
  const filled = slotFilled(draft, slot);
  const set = (value: string) => {
    if (slot.path.startsWith("secrets.")) {
      const id = slot.path.slice("secrets.".length);
      onChange({ ...draft, secrets: { ...draft.secrets, [id]: value } });
    } else {
      onChange({ ...draft, [slot.path]: value } as AgentDraft);
    }
  };
  return (
    <div className={styles.slotRow}>
      <span
        className={`${styles.slotBadge} ${filled ? styles.slotBadgeFilled : ""}`}
        aria-label={filled ? `${slot.title} filled` : `${slot.title} unfilled`}
      >
        {filled ? "✓" : "·"}
      </span>
      <div className={styles.slotCard}>
        <div className={styles.slotHeader}>
          <span className={styles.slotTitle}>{slot.title}</span>
          <span className={styles.slotPath}>{slot.path}</span>
        </div>
        {slot.help && <p className={styles.slotHelp}>{slot.help}</p>}
        <SlotControl slot={slot} draft={draft} set={set} catalogs={catalogs} />
        {slot.probe && <ProbeLine probe={probe} />}
      </div>
    </div>
  );
}

function SlotControl(props: {
  slot: GuidedSlot;
  draft: AgentDraft;
  set: (value: string) => void;
  catalogs: DraftCatalogs;
}) {
  const { slot, draft, set, catalogs } = props;
  if (slot.control === "channel-kind") {
    return (
      <div className={styles.choiceRow}>
        {CHANNEL_KINDS.map((kind) => (
          <Pill
            key={kind.value}
            selected={draft.channelKind === kind.value}
            onClick={() => set(draft.channelKind === kind.value ? "" : kind.value)}
          >
            {kind.label}
          </Pill>
        ))}
      </div>
    );
  }
  if (slot.control === "model" && catalogs.models.length > 0) {
    return (
      <Select
        value={draft.model}
        ariaLabel="Model"
        options={[
          { value: "", label: "— choose a model —" },
          ...catalogs.models.map((id) => ({ value: id, label: id })),
        ]}
        onChange={set}
      />
    );
  }
  const value = slot.path.startsWith("secrets.")
    ? (draft.secrets[slot.path.slice("secrets.".length)] ?? "")
    : String((draft as unknown as Record<string, unknown>)[slot.path] ?? "");
  return (
    <TextInput
      value={value}
      mono={slot.mono}
      ariaLabel={slot.title}
      placeholder={slot.probe ? "rusty:secret:<store>:<key>" : undefined}
      onChange={set}
    />
  );
}

function ProbeLine({ probe }: { probe?: SecretProbe }) {
  if (!probe) return <span className={styles.slotProbe}>Probe: not run</span>;
  if (probe.status === "ok") {
    return <span className={`${styles.slotProbe} ${styles.probeOk}`}>Probe: ok{probe.detail ? ` · ${probe.detail}` : ""}</span>;
  }
  if (probe.status === "failed") {
    return (
      <span className={`${styles.slotProbe} ${styles.probeFailed}`}>
        Probe: failed{probe.detail ? ` · ${probe.detail}` : ""}
      </span>
    );
  }
  return <span className={styles.slotProbe}>Probe: pending…</span>;
}

function ValidationAside({ violations }: { violations: Violation[] }) {
  return (
    <div className={styles.asideCard} aria-label="Validation">
      <h3 className={styles.asideTitle}>Validation</h3>
      {violations.length === 0 ? (
        <p className={styles.clean}>Passing</p>
      ) : (
        <ul className={styles.violationList}>
          {violations.map((v) => (
            <li key={`${v.rule}:${v.path}`} className={styles.violationItem}>
              <span
                className={`${styles.kindDot} ${
                  v.kind === "schema" ? styles.kindSchema : v.kind === "coherence" ? styles.kindCoherence : styles.kindSlot
                }`}
                aria-label={v.kind}
              />
              <span className={styles.violationPath}>{v.path}</span>
              <span className={styles.violationMessage}>{v.message}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function TemplateSummary({ template, slotCount }: { template: Template; slotCount: number }) {
  return (
    <div className={styles.asideCard} aria-label="Template summary">
      <h3 className={styles.asideTitle}>Template</h3>
      <div className={styles.summaryRow}>
        <strong>{template.name}</strong>
        <span className={styles.templateVersion}>tpl v{template.version}</span>
      </div>
      <div className={styles.summaryRow}>
        {template.document.connectors.map((id) => (
          <Badge key={id}>{id}</Badge>
        ))}
        <span>
          {slotCount} slots · {template.document.skills.length} skills
        </span>
      </div>
    </div>
  );
}
