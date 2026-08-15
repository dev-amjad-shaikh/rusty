import type { ReactNode, RefObject } from "react";
import styles from "./PageHeader.module.css";

export function PageHeader({
  headingId,
  eyebrow,
  title,
  description,
  actions,
  detail,
  headingRef,
  variant = "standard",
}: {
  headingId: string;
  eyebrow: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  detail?: ReactNode;
  headingRef?: RefObject<HTMLHeadingElement | null>;
  variant?: "standard" | "compact";
}) {
  return <header className={styles.header} data-variant={variant}>
    <div className={styles.copy}>
      <div className={styles.eyebrow}>{eyebrow}</div>
      <div className={styles.titleLine}>
        <h1 id={headingId} ref={headingRef} tabIndex={headingRef ? -1 : undefined}>{title}</h1>
        {detail && <div className={styles.detail}>{detail}</div>}
      </div>
      {description && <p>{description}</p>}
    </div>
    {actions && <div className={styles.actions}>{actions}</div>}
  </header>;
}
