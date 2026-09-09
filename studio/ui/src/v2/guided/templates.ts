// Template gallery model (R-A4, handoff 03 "Guided" / 06): a template is a
// published blueprint flagged as a template. Templates are never
// instantiable — the gallery's only affordance is starting a new AgentDraft
// from the template document. `GET /v1/templates` is declared in handoff 06;
// the gateway protocol has no Template type yet, so the shape lives here
// until contracts:gateway-protocol grows it and wire.ts takes over (R-X1).

import type { RestClient } from "../api/rest";
import type { AgentDraft } from "../draft/agent-draft.gen";

/** A published blueprint flagged as a template (handoff 06, GET /v1/templates). */
export interface Template {
  id: string;
  name: string;
  /** Published version the template points at — rendered as `tpl vN`. */
  version: number;
  /** One-line pitch for the gallery card. */
  blurb: string;
  /** The template document; slots and the draft seed derive from it. */
  document: AgentDraft;
}

/**
 * Templates are never instantiable (R-A4). Kept as a exported constant so
 * the invariant is testable instead of implicit in the gallery's markup.
 */
export const TEMPLATE_INSTANTIABLE = false;

/** Fetch the gallery from the gateway (handoff 06). */
export function fetchTemplates(
  client: RestClient,
  options: { signal?: AbortSignal } = {},
): Promise<Template[]> {
  return client.get<Template[]>("/v1/templates", options);
}

/**
 * Seed a draft from a template document. The new draft is a fresh agent:
 * source flips to guided, the template id is recorded for provenance, and
 * the name clears — the author names their own agent.
 */
export function draftFromTemplate(template: Template): AgentDraft {
  return {
    ...template.document,
    source: "guided",
    template: template.id,
    name: "",
  };
}
