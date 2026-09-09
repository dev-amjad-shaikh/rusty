// The Drafts screen (R-A9, handoff 03): every autosaved draft as a card —
// name, source pill, description or goal, connector pills, validation label,
// Discard + Resume. Validation status comes from the same rule set the form
// uses, so the label here never disagrees with the entry path.

import { useState } from "react";
import { Badge, Button } from "../controls/controls";
import { EMPTY_CATALOGS, type DraftCatalogs } from "./catalogs";
import { discardDraft, listDrafts, type DraftRecord } from "./store";
import { validateDraft } from "./validate";
import styles from "./drafts.module.css";

export interface DraftsScreenProps {
  /** Injectable for tests; defaults to window.localStorage. */
  storage?: Storage;
  catalogs?: DraftCatalogs;
  onResume?: (record: DraftRecord) => void;
}

export function DraftsScreen({ storage, catalogs = EMPTY_CATALOGS, onResume }: DraftsScreenProps) {
  const [records, setRecords] = useState<DraftRecord[]>(() => listDrafts(storage));
  const refresh = () => setRecords(listDrafts(storage));

  if (!records.length) {
    return <p className={styles.empty}>No drafts. Start one from Agents.</p>;
  }
  return (
    <div className={styles.grid}>
      {records.map((record) => {
        const { draft } = record;
        const violations = validateDraft(draft, catalogs);
        return (
          <article key={record.id} className={styles.card}>
            <div className={styles.cardHeader}>
              <h3 className={styles.cardTitle}>{draft.name.trim() || "Untitled draft"}</h3>
              <Badge>{draft.source}</Badge>
            </div>
            <p className={styles.cardBody}>{draft.description.trim() || draft.goal.trim() || "—"}</p>
            <div className={styles.cardMeta}>
              {draft.connectors.map((id) => (
                <Badge key={id}>{id}</Badge>
              ))}
              {violations.length === 0 ? (
                <Badge tone="ok">Passing</Badge>
              ) : (
                <Badge tone="warn">{violations.length} open violations</Badge>
              )}
              <span className={styles.cardTime}>{new Date(record.updatedAt).toLocaleString()}</span>
            </div>
            <div className={styles.cardActions}>
              <Button
                variant="secondary"
                small
                onClick={() => {
                  discardDraft(record.id, storage);
                  refresh();
                }}
              >
                Discard
              </Button>
              <Button variant="primary" small onClick={() => onResume?.(record)}>
                Resume
              </Button>
            </div>
          </article>
        );
      })}
    </div>
  );
}
