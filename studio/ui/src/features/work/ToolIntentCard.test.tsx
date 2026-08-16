import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { RunEvent } from "../../lib/contracts";
import { ToolIntentCard } from "./ToolIntentCard";

function toolEvent(tool: string, args: unknown, result: unknown, overrides: Partial<RunEvent> = {}): RunEvent {
  return {
    id: "run-1:3",
    run_id: "run-1",
    thread_id: "thread-1",
    node_id: "tools",
    seq: "3",
    kind: "tool_call",
    effect: "read_only",
    input: { kind: "inline", value: { tool, arguments: args } },
    output: { kind: "inline", value: result },
    latency_ms: "4",
    tokens: null,
    cost_usd: null,
    status: "ok",
    parent: "run-1:2",
    recorded_at: "2026-08-12T00:00:00Z",
    rawJson: "{}",
    ...overrides,
  };
}

describe("ToolIntentCard", () => {
  it("renders a terminal card for run_cli", () => {
    render(<ToolIntentCard event={toolEvent("run_cli", { program: "git", args: ["status"] }, {
      cwd: ".", exit_code: 0, timed_out: false, truncated: false, stdout: " M README.md", stderr: "",
    })} />);
    const card = screen.getByLabelText("terminal card");
    expect(card).toHaveTextContent("$ git status");
    expect(card).toHaveTextContent("exit 0");
    expect(card).toHaveTextContent(" M README.md");
  });

  it("renders a search card with its hit list", () => {
    render(<ToolIntentCard event={toolEvent("search_knowledge", { query: "kernel" }, {
      query: "kernel",
      results: [{ id: "doc-1", title: "Effect kernel", score: 2, excerpt: "the effect taxonomy" }],
    })} />);
    const card = screen.getByLabelText("search card");
    expect(card).toHaveTextContent("Effect kernel");
    expect(card).toHaveTextContent("doc-1");
    expect(card).toHaveTextContent("the effect taxonomy");
    expect(card).toHaveTextContent("score 2");
  });

  it("renders a read card with the excerpt", () => {
    render(<ToolIntentCard event={toolEvent("read_document", { path: "notes/design.md" }, {
      path: "notes/design.md", kind: "markdown", bytes: 15, content: "# Design\n\nbody",
    })} />);
    const card = screen.getByLabelText("read card");
    expect(card).toHaveTextContent("notes/design.md");
    expect(card).toHaveTextContent("markdown");
    expect(card).toHaveTextContent("# Design");
  });

  it("renders a table card for trace-shaped results", () => {
    render(<ToolIntentCard event={toolEvent("session_trace", { run_id: "run-1", event_id: "run-1:4" }, {
      target: { seq: 4, kind: "tool_call", node_id: "tools", status: "ok", latency_ms: 9 },
      ancestors: [{ seq: 3, kind: "model_call", node_id: "agent", status: "ok", latency_ms: 30 }],
      descendants: [],
      truncated: false,
    })} />);
    const card = screen.getByLabelText("table card");
    expect(screen.getByRole("table")).toBeTruthy();
    expect(card).toHaveTextContent("ancestor");
    expect(card).toHaveTextContent("target");
  });

  it("renders a link card with a safe external anchor", () => {
    render(<ToolIntentCard event={toolEvent("browser_navigate", { url: "https://docs.rs/serde" }, {
      url: "https://docs.rs/serde", title: "Serde",
    })} />);
    const anchor = screen.getByRole("link", { name: "https://docs.rs/serde" });
    expect(anchor).toHaveAttribute("rel", expect.stringContaining("noreferrer"));
    expect(anchor).toHaveAttribute("target", "_blank");
  });

  it("renders a web card and surfaces the clamped note", () => {
    render(<ToolIntentCard event={toolEvent("browser_read", {}, {
      url: "https://example.test/", bytes: 5, truncated: true, text: "hello",
    })} />);
    const card = screen.getByLabelText("web card");
    expect(card).toHaveTextContent("hello");
    expect(card).toHaveTextContent("Clamped for display");
  });

  it("renders a diff card with before and after panes", () => {
    render(<ToolIntentCard event={toolEvent("acme/update_record", {}, {
      path: "records/42.json", before: "old", after: "new",
    })} />);
    const card = screen.getByLabelText("diff card");
    expect(card).toHaveTextContent("records/42.json");
    expect(card).toHaveTextContent("old");
    expect(card).toHaveTextContent("new");
  });

  it("renders the honest generic card for unknown tools", () => {
    render(<ToolIntentCard event={toolEvent("mystery_tool", {}, { answer: 42 })} />);
    const card = screen.getByLabelText("generic card");
    expect(card).toHaveTextContent("mystery_tool");
    expect(card).toHaveTextContent("no render intent matches");
    expect(card).toHaveTextContent("42");
  });

  it("renders the failure reason for a failed call", () => {
    render(<ToolIntentCard event={toolEvent("run_cli", { program: "rm" }, { error: "not allowed" }, { status: "error" })} />);
    const card = screen.getByLabelText("generic card");
    expect(card).toHaveTextContent("the tool call failed");
    expect(card).toHaveTextContent("not allowed");
  });

  it("renders nothing for non-tool events", () => {
    const { container } = render(<ToolIntentCard event={toolEvent("calculator", {}, {}, { kind: "model_call" })} />);
    expect(container).toBeEmptyDOMElement();
  });
});
