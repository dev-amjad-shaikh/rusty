import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Badge, Button, Composer, Pill, Select, StatusDot, TextInput, ToastViewport } from "./controls";

afterEach(() => vi.useRealTimers());

describe("v2 base controls", () => {
  it("renders button variants and honors disabled", async () => {
    const user = userEvent.setup();
    const clicks: string[] = [];
    render(
      <>
        <Button variant="primary" onClick={() => clicks.push("primary")}>Publish</Button>
        <Button variant="secondary" onClick={() => clicks.push("secondary")}>Export</Button>
        <Button variant="ghost" onClick={() => clicks.push("ghost")}>Open session</Button>
        <Button variant="primary" disabled onClick={() => clicks.push("disabled")}>Blocked</Button>
      </>,
    );
    await user.click(screen.getByRole("button", { name: "Publish" }));
    await user.click(screen.getByRole("button", { name: "Blocked" }));
    expect(clicks).toEqual(["primary"]);
    expect(screen.getByRole("button", { name: "Blocked" })).toBeDisabled();
  });

  it("select renders options with the custom chevron and reports changes", async () => {
    const user = userEvent.setup();
    let value = "a";
    render(
      <Select
        value={value}
        ariaLabel="Autonomy"
        onChange={(next) => { value = next; }}
        options={[{ value: "a", label: "Read only" }, { value: "b", label: "Supervised" }]}
      />,
    );
    const select = screen.getByRole("combobox", { name: "Autonomy" });
    await user.selectOptions(select, "b");
    expect(value).toBe("b");
  });

  it("pill exposes pressed state", async () => {
    const user = userEvent.setup();
    let selected = false;
    render(<Pill selected={selected} onClick={() => { selected = !selected; }}>slack</Pill>);
    const pill = screen.getByRole("button", { name: "slack" });
    expect(pill).toHaveAttribute("aria-pressed", "false");
    await user.click(pill);
    expect(selected).toBe(true);
  });

  it("badge and status dot pair a label with a tone", () => {
    render(
      <>
        <Badge tone="warn">Trial</Badge>
        <StatusDot tone="ok" label="Passing" />
      </>,
    );
    expect(screen.getByText("Trial")).toBeInTheDocument();
    expect(screen.getByText("Passing")).toBeInTheDocument();
  });

  it("composer sends on Enter, keeps Shift+Enter as a newline, and blocks empty sends", async () => {
    const user = userEvent.setup();
    const sent: string[] = [];
    function Harness() {
      const [value, setValue] = useState("");
      return (
        <Composer
          value={value}
          onChange={setValue}
          onSend={() => sent.push(value)}
          placeholder="Describe the agent"
          sendLabel="Send"
        />
      );
    }
    render(<Harness />);
    const box = screen.getByRole("textbox");
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    await user.type(box, "watch the queue{Enter}");
    expect(sent).toEqual(["watch the queue"]);
    await user.type(box, "line one{Shift>}{Enter}{/Shift}line two");
    expect(sent).toHaveLength(1);
  });

  it("toast auto-dismisses after 2.6 s", () => {
    vi.useFakeTimers();
    const dismissed: string[] = [];
    render(<ToastViewport toasts={[{ id: "t1", text: "Draft saved" }]} onDismiss={(id) => dismissed.push(id)} />);
    expect(screen.getByText("Draft saved")).toBeInTheDocument();
    vi.advanceTimersByTime(2_700);
    expect(dismissed).toEqual(["t1"]);
  });

  it("text input reports edits", async () => {
    const user = userEvent.setup();
    const edits: string[] = [];
    function Harness() {
      const [value, setValue] = useState("");
      return <TextInput value={value} ariaLabel="Agent name" onChange={(next) => { edits.push(next); setValue(next); }} />;
    }
    render(<Harness />);
    await user.type(screen.getByRole("textbox", { name: "Agent name" }), "watcher");
    expect(edits.at(-1)).toBe("watcher");
    expect(screen.getByRole("textbox", { name: "Agent name" })).toHaveValue("watcher");
  });
});
