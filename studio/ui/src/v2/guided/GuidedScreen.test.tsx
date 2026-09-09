// Guided setup (R-A4, handoff 03): the three-step wizard — gallery with
// Start blank first, slot cards with live validation aside, the autonomy
// three-card step — producing an AgentDraft that lands in the draft store
// and hands off to Review (R-A2, R-A9).

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { listDrafts } from "../draft/store";
import { GuidedScreen } from "./GuidedScreen";
import { fixtureCatalogs, fixtureTemplate, freshStorage } from "./fixtures";

function setup(overrides: Partial<Parameters<typeof GuidedScreen>[0]> = {}) {
  const storage = freshStorage();
  const reviewed: string[] = [];
  render(
    <GuidedScreen
      templates={[fixtureTemplate()]}
      catalogs={fixtureCatalogs()}
      storage={storage}
      validationDebounceMs={0}
      onReview={(record) => reviewed.push(record.id)}
      {...overrides}
    />,
  );
  return { storage, reviewed };
}

describe("GuidedScreen · step 1 Template", () => {
  it("renders the header and the gallery, Start blank first", () => {
    setup();
    expect(screen.getByRole("heading", { name: "Guided setup" })).toBeInTheDocument();
    for (const pill of ["1 Template", "2 Configure", "3 Autonomy"]) {
      expect(screen.getByRole("button", { name: pill })).toBeInTheDocument();
    }
    const gallery = screen.getByLabelText("Templates");
    expect(gallery.firstElementChild).toHaveTextContent("Start blank");
    const card = screen.getByRole("button", { name: /Support triage/ });
    expect(card).toHaveTextContent("tpl v3");
    expect(card).toHaveTextContent("Triage the inbound support queue.");
    expect(card).toHaveTextContent("gmail");
    expect(card).toHaveTextContent("6 slots · 1 skills");
    // Never instantiable (R-A4): no run/instantiate affordance anywhere.
    expect(screen.queryByText(/instantiate/i)).toBeNull();
    expect(screen.queryByRole("button", { name: /run/i })).toBeNull();
  });
});

describe("GuidedScreen · step 2 Configure (template)", () => {
  it("renders slot cards derived from the template document and saves immediately", async () => {
    const user = userEvent.setup();
    const { storage } = setup({ probes: { gmail: { status: "ok" } } });
    await user.click(screen.getByRole("button", { name: /Support triage/ }));

    // Slot cards: badge, title, JSON path mono, probe line on the credential.
    for (const title of ["Agent name", "Goal", "Channel kind", "Channel target", "gmail credential", "Model"]) {
      expect(screen.getByText(title)).toBeInTheDocument();
    }
    for (const path of ["name", "goal", "channelKind", "channelTarget", "secrets.gmail", "model"]) {
      expect(screen.getAllByText(path).length).toBeGreaterThan(0);
    }
    expect(screen.getByText("Probe: ok")).toBeInTheDocument();
    expect(screen.getByLabelText("Agent name unfilled")).toBeInTheDocument();
    expect(screen.getByLabelText("Goal filled")).toBeInTheDocument();

    // The draft is already in the store, sourced from the template.
    const saved = listDrafts(storage);
    expect(saved).toHaveLength(1);
    expect(saved[0].draft.source).toBe("guided");
    expect(saved[0].draft.template).toBe("tpl-support");
    expect(saved[0].draft.name).toBe("");
    expect(saved[0].draft.connectors).toEqual(["gmail"]);
  });

  it("surfaces unfilled slots as violations and clears them as they fill", async () => {
    const user = userEvent.setup();
    setup();
    await user.click(screen.getByRole("button", { name: /Support triage/ }));

    const aside = screen.getByLabelText("Validation");
    // Slot-layer coverage: the shared rule set has no model rule.
    expect(await within(aside).findByText("Model is unfilled.")).toBeInTheDocument();
    // Paths the shared rule set already flags are not double-listed.
    expect(within(aside).getByText("Name is required.")).toBeInTheDocument();
    expect(within(aside).queryByText("Agent name is unfilled.")).toBeNull();

    await user.type(screen.getByLabelText("Agent name"), "Inbox triager");
    await waitFor(() => expect(within(aside).queryByText("Name is required.")).toBeNull());
    expect(within(aside).getByText("Model is unfilled.")).toBeInTheDocument();
    expect(screen.getByLabelText("Agent name filled")).toBeInTheDocument();
  });
});

describe("GuidedScreen · step 2 Configure (blank)", () => {
  it("renders the full AgentDraftForm", async () => {
    const user = userEvent.setup();
    const { storage } = setup();
    await user.click(screen.getByRole("button", { name: /Start blank/ }));
    expect(screen.getByRole("region", { name: "Goal & measures" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Connectors & tools" })).toBeInTheDocument();
    expect(screen.queryByLabelText("Template summary")).toBeNull();
    const saved = listDrafts(storage);
    expect(saved).toHaveLength(1);
    expect(saved[0].draft.template).toBeNull();
  });
});

describe("GuidedScreen · step 3 Autonomy", () => {
  it("shows the three cards with the read_only coherence note, then hands off to Review", async () => {
    const user = userEvent.setup();
    const { storage, reviewed } = setup();
    await user.click(screen.getByRole("button", { name: /Support triage/ }));
    await user.click(screen.getByRole("button", { name: "Next · Autonomy" }));

    for (const name of ["Read only", "Supervised", "Full"]) {
      expect(screen.getByRole("button", { name: new RegExp(name) })).toBeInTheDocument();
    }
    // gmail.send is an egress tool: read_only conflicts (validation table,
    // autonomy × toolsets row).
    expect(screen.getByText("Conflicts with 1 mounted Write/Egress tools")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /Supervised/ }));
    await user.click(screen.getByRole("button", { name: "Review" }));

    expect(reviewed).toHaveLength(1);
    const saved = listDrafts(storage);
    expect(saved[0].id).toBe(reviewed[0]);
    expect(saved[0].draft.autonomy).toBe("supervised");
  });
});
