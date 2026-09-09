// Live validation on pause-typing (R-A3): the full rule set reruns after the
// typist pauses, never on every keystroke. The default debounce keeps the
// report fresher than the 300 ms budget; the run itself is microseconds.

import { useEffect, useRef, useState } from "react";
import { EMPTY_CATALOGS, type DraftCatalogs } from "./catalogs";
import { validateDraft, type Violation } from "./validate";
import type { AgentDraft } from "./agent-draft.gen";

export const VALIDATION_DEBOUNCE_MS = 250;

export interface LiveValidation {
  violations: Violation[];
  /** True while a change is inside the debounce window. */
  pending: boolean;
}

export function useLiveValidation(
  draft: AgentDraft,
  catalogs: DraftCatalogs = EMPTY_CATALOGS,
  debounceMs: number = VALIDATION_DEBOUNCE_MS,
): LiveValidation {
  const [violations, setViolations] = useState<Violation[]>(() => validateDraft(draft, catalogs));
  const [pending, setPending] = useState(false);
  const first = useRef(true);

  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    setPending(true);
    const timer = setTimeout(() => {
      setViolations(validateDraft(draft, catalogs));
      setPending(false);
    }, debounceMs);
    return () => clearTimeout(timer);
  }, [draft, catalogs, debounceMs]);

  return { violations, pending };
}
