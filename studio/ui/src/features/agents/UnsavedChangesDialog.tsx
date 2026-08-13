import { useEffect, useRef } from "react";
import styles from "./AgentWorkspace.module.css";

export function UnsavedChangesDialog({ onKeep, onDiscard }: { onKeep: () => void; onDiscard: () => void }) {
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
    requestAnimationFrame(() => returnFocus.current?.focus());
  }

  return <dialog ref={dialogRef} className={styles.discardDialog} aria-labelledby="discard-heading" onCancel={(event) => { event.preventDefault(); keepEditing(); }}>
    <span className="eyebrow">Unsaved definition</span>
    <h2 id="discard-heading">Discard your changes?</h2>
    <p>This draft exists only in this page. Keep editing, or discard it before continuing.</p>
    <div><button ref={keepRef} type="button" className="secondary-button" onClick={keepEditing}>Keep editing</button><button type="button" className="primary-button" onClick={onDiscard}>Discard changes</button></div>
  </dialog>;
}
