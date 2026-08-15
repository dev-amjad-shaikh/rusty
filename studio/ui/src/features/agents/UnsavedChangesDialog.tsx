import { useEffect, useRef, type RefObject } from "react";
import styles from "./AgentWorkspace.module.css";

export function UnsavedChangesDialog({ onKeep, onDiscard, pending = false, title, message, discardLabel = "Discard changes", returnFocusRef }: { onKeep: () => void; onDiscard: () => void; pending?: boolean; title?: string; message?: string; discardLabel?: string; returnFocusRef?: RefObject<HTMLElement | null> }) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const keepRef = useRef<HTMLButtonElement>(null);
  const returnFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    returnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const dialog = dialogRef.current;
    if (dialog && !dialog.open) {
      if (typeof dialog.showModal === "function") dialog.showModal();
      else dialog.setAttribute("open", "");
    }
    requestAnimationFrame(() => keepRef.current?.focus());
    return () => { if (dialog?.open && typeof dialog.close === "function") dialog.close(); };
  }, []);

  function keepEditing() {
    onKeep();
    requestAnimationFrame(() => (returnFocusRef?.current ?? returnFocus.current)?.focus());
  }

  return <dialog ref={dialogRef} className={styles.discardDialog} aria-labelledby="discard-heading" onCancel={(event) => { event.preventDefault(); keepEditing(); }}>
    <span className="eyebrow">{pending ? "Creation in progress" : "Unsaved definition"}</span>
    <h2 id="discard-heading">{pending ? "Rusty is still creating this agent" : title ?? "Discard your changes?"}</h2>
    <p>{pending ? "Stay here until Rusty confirms whether the agent was created. This request cannot be cancelled safely." : message ?? "This draft exists only in this page. Keep editing, or discard it before continuing."}</p>
    <div><button ref={keepRef} type="button" className="secondary-button" onClick={keepEditing}>{pending ? "Stay here" : "Keep editing"}</button>{!pending && <button type="button" className="primary-button" onClick={onDiscard}>{discardLabel}</button>}</div>
  </dialog>;
}
