import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { initialSelections, initialValues, interpretForm } from "../../lib/schema-form";
import { servicenowManifest } from "./fixtures";
import { SchemaForm } from "./SchemaForm";

function renderForm(spec: unknown, props: Partial<Parameters<typeof SchemaForm>[0]> = {}) {
  const form = interpretForm(spec);
  const selections = initialSelections(form);
  const onValue = vi.fn();
  const onSelect = vi.fn();
  render(
    <SchemaForm
      form={form}
      values={initialValues(form, selections)}
      selections={selections}
      errors={{}}
      onValue={onValue}
      onSelect={onSelect}
      {...props}
    />,
  );
  return { onValue, onSelect };
}

describe("SchemaForm", () => {
  it("renders the ServiceNow spec: text field with descriptor hint and a credentials variant picker", () => {
    renderForm(servicenowManifest().connection_specification);
    const instance = screen.getByLabelText("Instance");
    expect(instance).toHaveAttribute("type", "text");
    expect(instance).toHaveAttribute("placeholder", "your-instance.service-now.com");

    const picker = screen.getByLabelText("Authentication");
    expect(picker).toHaveRole("combobox");
    expect(screen.getByRole("option", { name: "Basic" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "OAuth token" })).toBeInTheDocument();

    // The Basic variant renders first; both secrets are masked password inputs.
    expect(screen.getByLabelText("Username")).toHaveAttribute("type", "password");
    expect(screen.getByLabelText("Password")).toHaveAttribute("type", "password");
    // The const discriminator is applied, never rendered.
    expect(screen.queryByLabelText("Auth", { selector: "input" })).not.toBeInTheDocument();
  });

  it("groups grouped fields under a humanized section heading", () => {
    renderForm(servicenowManifest().connection_specification);
    expect(screen.getByRole("region", { name: "Auth" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Auth" })).toBeInTheDocument();
  });

  it("swaps the sub-form when the variant changes", () => {
    const { onSelect } = renderForm(servicenowManifest().connection_specification);
    expect(screen.queryByLabelText("Access token")).not.toBeInTheDocument();
    // The picker reports the selection; the parent re-renders with it.
    const form = interpretForm(servicenowManifest().connection_specification);
    const selections = { ...initialSelections(form), credentials: "oauth" };
    render(
      <SchemaForm
        form={form}
        values={{}}
        selections={selections}
        errors={{}}
        onValue={vi.fn()}
        onSelect={onSelect}
      />,
    );
    expect(screen.getByLabelText("Access token")).toHaveAttribute("type", "password");
  });

  it("reports edits by dot path", async () => {
    const { onValue } = renderForm(servicenowManifest().connection_specification);
    await userEvent.type(screen.getByLabelText("Instance"), "acme");
    expect(onValue).toHaveBeenLastCalledWith("instance", "e");
  });

  it("renders each input type from the schema", () => {
    renderForm({
      type: "object",
      properties: {
        region: { type: "string", enum: ["eu", "us"], title: "Region" },
        retries: { type: "integer", title: "Retries" },
        verbose: { type: "boolean", title: "Verbose" },
        legacy: { type: "string", title: "Legacy", rusty_hidden: true },
      },
    });
    expect(screen.getByLabelText("Region")).toHaveRole("combobox");
    expect(screen.getByLabelText("Retries")).toHaveAttribute("type", "number");
    expect(screen.getByLabelText("Verbose")).toHaveAttribute("type", "checkbox");
    expect(screen.queryByLabelText("Legacy")).not.toBeInTheDocument();
  });

  it("pins a 422 field error under its input", () => {
    renderForm(servicenowManifest().connection_specification, {
      errors: { "credentials.username": "required property missing" },
    });
    const input = screen.getByLabelText("Username");
    expect(input).toHaveAttribute("aria-invalid", "true");
    const pinned = screen.getByText("required property missing");
    expect(pinned).toHaveRole("alert");
    expect(input.getAttribute("aria-describedby")).toBe(pinned.id);
  });

  it("pins a variant-level rejection on the picker", () => {
    renderForm(servicenowManifest().connection_specification, {
      errors: { credentials: "not valid under any of the given schemas" },
    });
    expect(screen.getByLabelText("Authentication")).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByText("not valid under any of the given schemas")).toHaveRole("alert");
  });
});
