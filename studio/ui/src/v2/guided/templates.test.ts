// Template gallery model (R-A4): templates are published blueprints flagged
// as templates, never instantiable; starting one seeds a fresh guided draft
// from the template document.

import { describe, expect, it } from "vitest";
import {
  TEMPLATE_INSTANTIABLE,
  draftFromTemplate,
  fetchTemplates,
} from "./templates";
import { fixtureTemplate } from "./fixtures";

describe("templates", () => {
  it("are never instantiable", () => {
    expect(TEMPLATE_INSTANTIABLE).toBe(false);
  });

  it("seeds a guided draft: template id kept, source flipped, name cleared", () => {
    const template = fixtureTemplate();
    const draft = draftFromTemplate({ ...template, document: { ...template.document, name: "Tpl name" } });
    expect(draft.source).toBe("guided");
    expect(draft.template).toBe("tpl-support");
    expect(draft.name).toBe("");
    expect(draft.goal).toBe("Zero unanswered tickets.");
    expect(draft.connectors).toEqual(["gmail"]);
  });

  it("fetches the gallery from GET /v1/templates (handoff 06)", async () => {
    const calls: string[] = [];
    const templates = [fixtureTemplate()];
    const client = {
      get: <T,>(path: string): Promise<T> => {
        calls.push(path);
        return Promise.resolve(templates as T);
      },
    };
    // The structural subset is all fetchTemplates needs from RestClient.
    const result = await fetchTemplates(client as never);
    expect(calls).toEqual(["/v1/templates"]);
    expect(result).toEqual(templates);
  });
});
