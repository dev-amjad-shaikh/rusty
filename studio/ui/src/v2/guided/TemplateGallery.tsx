// Template gallery (R-A4, handoff 03 "Guided" step 1): the "Start blank"
// dashed card first, then one card per template — name, `tpl vN` mono,
// blurb, connector pills, "N slots · N skills". Templates are never
// instantiable: a card's only action is starting a draft from the document.

import { Badge } from "../controls/controls";
import { deriveSlots } from "./slots";
import type { Template } from "./templates";
import styles from "./guided.module.css";

export interface TemplateGalleryProps {
  templates: Template[];
  /** `null` = Start blank. */
  onChoose: (template: Template | null) => void;
}

export function TemplateGallery({ templates, onChoose }: TemplateGalleryProps) {
  // No run / instantiate affordance here by design — templates are never
  // instantiable (R-A4, TEMPLATE_INSTANTIABLE); a card only starts a draft.
  return (
    <div className={styles.gallery} aria-label="Templates">
      <button type="button" className={styles.blankCard} onClick={() => onChoose(null)}>
        <span className={styles.templateName}>Start blank</span>
        <p className={styles.templateBlurb}>The full form, every section open.</p>
      </button>
      {templates.map((template) => (
        <button
          type="button"
          key={template.id}
          className={styles.templateCard}
          onClick={() => onChoose(template)}
        >
          <span className={styles.templateName}>{template.name}</span>
          <span className={styles.templateVersion}>tpl v{template.version}</span>
          <p className={styles.templateBlurb}>{template.blurb}</p>
          <span className={styles.templateMeta}>
            {template.document.connectors.map((id) => (
              <Badge key={id}>{id}</Badge>
            ))}
            <span>
              {deriveSlots(template.document).length} slots · {template.document.skills.length} skills
            </span>
          </span>
        </button>
      ))}
    </div>
  );
}
