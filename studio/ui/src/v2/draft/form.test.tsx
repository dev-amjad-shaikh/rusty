// AgentDraftForm: schema-driven rendering (R-A1's AC — a field added to the
// schema appears without per-path code) and anchored violations (R-A3).

import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import type { AgentDraft } from "./agent-draft.gen";
import { AGENT_DRAFT_SCHEMA, type DraftCatalogs, type DraftSchema } from "./catalogs";
import { blankDraft } from "./defaults";
import { AgentDraftForm } from "./form";
import { validateDraft } from "./validate";

const CATALOGS: DraftCatalogs = {
  models: ["gpt-5", "claude-opus"],
  connectors: [
    {
      id: "slack",
      name: "Slack",
      tools: [
        { id: "slack.read", effect: "read" },
        { id: "slack.post", effect: "write" },
      ],
      events: ["message"],
    },
  ],
  skills: [{ id: "triage", description: "Triage inbound tickets" }],
  evalSuites: ["service-desk-regression"],
};

function Harness(props: { initial?: AgentDraft; schema?: DraftSchema; catalogs?: DraftCatalogs }) {
  const [draft, setDraft] = useState<AgentDraft>(props.initial ?? blankDraft("guided"));
  return (
    <AgentDraftForm
      draft={draft}
      onChange={setDraft}
      violations={validateDraft(draft, props.catalogs ?? CATALOGS)}
      catalogs={props.catalogs ?? CATALOGS}
      schema={props.schema}
    />
  );
}

describe("AgentDraftForm", () => {
  it("renders every section and the core identity fields from the schema", () => {
    render(<Harness />);
    for (const label of [
      "Identity",
      "Goal & measures",
      "Directive",
      "Connectors & tools",
      "Skills",
      "Memory blocks",
      "Channel & triggers",
      "Learning policy",
    ]) {
      expect(screen.getByRole("heading", { name: label })).toBeInTheDocument();
    }
    expect(screen.getByRole("textbox", { name: "Name" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Model" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Autonomy" })).toBeInTheDocument();
    // Hidden bookkeeping fields never render.
    expect(screen.queryByText(/source|template|agentId/)).not.toBeInTheDocument();
  });

  it("edits flow back as a new draft", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "Support");
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Support");
  });

  it("violations anchor under the control they name, with kind and spec file", () => {
    render(<Harness />);
    // A blank draft: name violation inline under the Name field.
    const nameField = screen.getByRole("textbox", { name: "Name" }).closest("div")!;
    expect(within(nameField.parentElement!).getByText("Name is required.")).toBeInTheDocument();
    expect(within(nameField.parentElement!).getByText("agent.md")).toBeInTheDocument();
    // The measures minimum shows its handoff copy twice: as the list's empty
    // state and as the anchored violation.
    expect(
      screen.getAllByText("No measures. Add at least one so the agent knows what it is judged on."),
    ).toHaveLength(2);
  });

  it("R-A1 AC: a field added to the schema renders with zero per-path code", async () => {
    const user = userEvent.setup();
    const augmented: DraftSchema = JSON.parse(JSON.stringify(AGENT_DRAFT_SCHEMA));
    augmented.properties.nickname = {
      type: "string",
      "x-control": "text",
      "x-section": "identity",
      "x-label": "Nickname",
      "x-spec-file": "agent.md",
    };
    augmented.properties.priority = {
      enum: ["low", "high"],
      "x-control": "enum",
      "x-section": "identity",
      "x-label": "Priority",
    };
    render(<Harness schema={augmented} />);
    await user.type(screen.getByRole("textbox", { name: "Nickname" }), "Ace");
    expect(screen.getByRole("textbox", { name: "Nickname" })).toHaveValue("Ace");
    expect(screen.getByRole("combobox", { name: "Priority" })).toBeInTheDocument();
  });

  it("object lists add and remove rows with schema defaults", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "+ Measure" }));
    expect(screen.getByRole("combobox", { name: "Source" })).toBeInTheDocument();
    // New measure starts at the first enum variant and immediately flags its slots.
    expect(screen.getByText("A measure needs a name.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Remove measures 1" }));
    expect(screen.queryByRole("combobox", { name: "Source" })).not.toBeInTheDocument();
  });

  it("mounting a connector exposes its tools, secrets slot, and events", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: /Slack · 2 tools/ }));
    // Toolset chips appear with wrap toggles.
    expect(screen.getByText("slack.post")).toBeInTheDocument();
    await user.click(screen.getAllByRole("button", { name: "+ wrap" })[0]);
    expect(
      screen.getByRole("button", { name: "approval_required · org_admins" }),
    ).toBeInTheDocument();
    // The credential slot for the mounted connector.
    expect(screen.getByRole("textbox", { name: "Slack SecretRef name" })).toBeInTheDocument();
    // The connector's events become trigger options.
    await user.click(screen.getByRole("button", { name: "+ Trigger" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "Kind" }), "event");
    const spec = screen.getByRole("combobox", { name: "Spec" });
    expect(within(spec).getByRole("option", { name: "slack.message" })).toBeInTheDocument();
  });

  it("read_only conflicts anchor on the autonomy control and the tool chip", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: /Slack · 2 tools/ }));
    expect(screen.getByText(/Read-only autonomy conflicts with slack\.post/)).toBeInTheDocument();
    expect(
      screen.getByText("This tool writes, executes, or egresses — unavailable under read-only autonomy."),
    ).toBeInTheDocument();
  });
});
