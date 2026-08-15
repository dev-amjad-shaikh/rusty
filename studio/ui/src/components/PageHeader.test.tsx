import { createRef } from "react";
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { PageHeader } from "./PageHeader";

describe("PageHeader", () => {
  it("keeps route context, title, orientation, detail, and actions in one landmark", () => {
    render(<PageHeader
      headingId="workspace-heading"
      eyebrow="Build / Agent"
      title="New agent"
      description="Shape one durable definition."
      detail={<span>Draft</span>}
      actions={<button type="button">Save</button>}
    />);

    const heading = screen.getByRole("heading", { level: 1, name: "New agent" });
    expect(heading).toHaveAttribute("id", "workspace-heading");
    expect(heading.closest("header")).toContainElement(screen.getByText("Build / Agent"));
    expect(heading.closest("header")).toContainElement(screen.getByText("Shape one durable definition."));
    expect(heading.closest("header")).toContainElement(screen.getByText("Draft"));
    expect(heading.closest("header")).toContainElement(screen.getByRole("button", { name: "Save" }));
  });

  it("exposes a programmatic focus owner only when the page supplies one", () => {
    const headingRef = createRef<HTMLHeadingElement>();
    const { rerender } = render(<PageHeader headingId="focused-heading" headingRef={headingRef} eyebrow="Operate" title="Operations" variant="compact" />);
    expect(headingRef.current).toHaveAttribute("tabindex", "-1");

    rerender(<PageHeader headingId="static-heading" eyebrow="Oversee" title="Work board" />);
    expect(screen.getByRole("heading", { name: "Work board" })).not.toHaveAttribute("tabindex");
  });
});
