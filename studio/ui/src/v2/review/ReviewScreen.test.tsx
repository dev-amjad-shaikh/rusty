// Review — the convergence screen (R-A2): diff cards, validation rail,
// assembled prompt, eval gate run/re-run over REST, publish preconditions,
// governance confirmation, fleet-upgrade offer, export scan.

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { RestClient } from "../api/rest";
import { ProblemError } from "../api/rest";
import { blankDraft } from "../draft/defaults";
import type { AgentDraft } from "../draft/agent-draft.gen";
import type { DraftCatalogs } from "../draft/catalogs";
import type { DraftRecord } from "../draft/store";
import type { ExportBundle } from "./bundle";
import { ReviewScreen } from "./ReviewScreen";

const catalogs: DraftCatalogs = {
  models: ["anthropic:claude"],
  connectors: [
    { id: "slack", name: "Slack", events: ["message"], tools: [{ id: "slack.read", effect: "read" }] },
  ],
  skills: [],
  evalSuites: ["scout-gate"],
};

function validDraft(): AgentDraft {
  return {
    ...blankDraft("compose"),
    name: "Scout",
    description: "Finds leads",
    model: "anthropic:claude",
    autonomy: "supervised",
    goal: "Find leads",
    measures: [{ name: "Leads", source: "outcome", target: "≥ 10", window: "weekly", kind: "target" }],
    stable: "You are Scout.",
    context: "Acme CRM.",
    connectors: ["slack"],
    secrets: { slack: "rusty:secret:core:slack-bot" },
    rules: [{ tool: "slack.read", rule: "Be quick" }],
    channelKind: "slack",
    channelTarget: "#sales",
    cadence: "0 3 * * *",
    gateSuite: "scout-gate",
  };
}

function recordOf(draft: AgentDraft): DraftRecord {
  return { id: "r1", draft, createdAt: "2026-09-09T09:00:00Z", updatedAt: "2026-09-09T09:00:00Z" };
}

interface Call {
  method: string;
  path: string;
  body?: unknown;
}

function fakeRest(routes: Record<string, unknown | (() => unknown)>) {
  const calls: Call[] = [];
  const resolve = (key: string) => {
    const route = routes[key];
    const value = typeof route === "function" ? route() : route;
    if (value === undefined) throw new Error(`no route: ${key}`);
    return value;
  };
  const client: RestClient = {
    get: <T,>(path: string) => {
      calls.push({ method: "GET", path });
      return Promise.resolve(resolve(`GET ${path}`) as T);
    },
    page: <T,>(path: string) => Promise.resolve(resolve(`GET ${path}`) as T),
    paginate: async function* <T>(): AsyncGenerator<T, void, undefined> {
      // Review never pages collections.
    },
    post: <T,>(path: string, body: unknown) => {
      calls.push({ method: "POST", path, body });
      return Promise.resolve({ status: 200, data: resolve(`POST ${path}`) as T, idempotencyKey: "k-1" });
    },
    put: <T,>(path: string, body: unknown) => {
      calls.push({ method: "PUT", path, body });
      return Promise.resolve({ status: 200, data: resolve(`PUT ${path}`) as T, idempotencyKey: "k-1" });
    },
    patch: <T,>(path: string, body: unknown) => {
      calls.push({ method: "PATCH", path, body });
      return Promise.resolve({ status: 200, data: resolve(`PATCH ${path}`) as T, idempotencyKey: "k-1" });
    },
  };
  return { client, calls };
}

function gateCard() {
  return within(screen.getByLabelText("Eval gate"));
}

function renderScreen(overrides: Partial<Parameters<typeof ReviewScreen>[0]> = {}) {
  const props: Parameters<typeof ReviewScreen>[0] = {
    record: recordOf(validDraft()),
    catalogs,
    scopes: ["blueprints:publish", "evals:run"],
    rest: fakeRest({}).client,
    pollIntervalMs: 1,
    now: "2026-09-09T10:00:00.000Z",
    ...overrides,
  };
  return render(<ReviewScreen {...props} />);
}

describe("ReviewScreen", () => {
  it("renders the header, diff cards, and GOVERNANCE badges", () => {
    const draft = {
      ...validDraft(),
      agentId: "b1",
      base: { ...blankDraft("compose"), version: 3 },
      triggers: [{ kind: "cron" as const, spec: "0 9 * * *", prompt: "Sweep" }],
    };
    renderScreen({ record: recordOf(draft) });
    expect(screen.getByRole("heading", { name: "Scout" })).toBeInTheDocument();
    expect(screen.getByText("v3 → v4")).toBeInTheDocument();
    expect(screen.getByText("Identity")).toBeInTheDocument();
    // autonomy, triggers, and toolsets (new connector) are governance-flagged
    const badges = screen.getAllByText("GOVERNANCE");
    expect(badges.length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("secrets.slack")).toBeInTheDocument();
  });

  it("keeps Publish disabled with hints until every precondition holds", () => {
    renderScreen({ record: recordOf(blankDraft("guided")) });
    const publish = screen.getByRole("button", { name: "Publish v1" });
    expect(publish).toBeDisabled();
    expect(screen.getByText(/open violations/)).toBeInTheDocument();
    expect(screen.getByText("Eval gate is not Passing")).toBeInTheDocument();
  });

  it("renders no Publish at all without the publish scope", () => {
    renderScreen({ scopes: ["blueprints:read"] });
    expect(screen.queryByRole("button", { name: /Publish/ })).not.toBeInTheDocument();
    expect(screen.getByText(/Publish needs the publish scope/)).toBeInTheDocument();
  });

  it("runs the gate over REST and then publishes, offering fleet upgrade on live sessions", async () => {
    const user = userEvent.setup();
    let polls = 0;
    const { client, calls } = fakeRest({
      "POST /v1/eval-suites/scout-gate/run": { run_id: "run-1" },
      "GET /v1/eval-runs/run-1": () =>
        ++polls === 1
          ? { status: "running", cases: [{ name: "case-a", pass: true, score: 0.9 }] }
          : { status: "passed", cases: [{ name: "case-a", pass: true, score: 0.9 }] },
      "POST /v1/blueprints": { blueprint_id: "b1", version: 1, sessions: 2 },
      "POST /v1/blueprints/b1/upgrade": { upgrade_id: "up-1", sessions: 2 },
    });
    renderScreen({ rest: client });

    expect(gateCard().getByText("Not run")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Run gate" }));
    await waitFor(() => expect(gateCard().getByText("Passing")).toBeInTheDocument());
    expect(screen.getByText("case-a")).toBeInTheDocument();
    expect(calls[0]).toMatchObject({ method: "POST", path: "/v1/eval-suites/scout-gate/run" });

    // Governance changes (autonomy on a first publish) require confirmation.
    await user.click(screen.getByRole("button", { name: "Publish v1" }));
    expect(screen.getByText(/Governance-significant changes/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Confirm and publish" }));

    await waitFor(() => expect(screen.getByText("Published v1")).toBeInTheDocument());
    expect(calls.some((c) => c.method === "POST" && c.path === "/v1/blueprints")).toBe(true);
    expect(screen.getByText(/2 live sessions/)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Start upgrade" }));
    await waitFor(() =>
      expect(calls.some((c) => c.path === "/v1/blueprints/b1/upgrade")).toBe(true),
    );
  });

  it("publishes an existing blueprint through the versions endpoint", async () => {
    const user = userEvent.setup();
    const draft = {
      ...validDraft(),
      agentId: "b1",
      base: { ...validDraft(), version: 3 },
    };
    const { client, calls } = fakeRest({
      "POST /v1/eval-suites/scout-gate/run": { run_id: "run-1" },
      "GET /v1/eval-runs/run-1": { status: "passed", cases: [] },
      "POST /v1/blueprints/b1/versions": { blueprint_id: "b1", version: 4 },
    });
    renderScreen({ record: recordOf(draft), rest: client });

    // No diff against an identical base.
    expect(screen.getByText("No changes against the published head.")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Run gate" }));
    await waitFor(() => expect(gateCard().getByText("Passing")).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: "Publish v4" }));
    await waitFor(() => expect(screen.getByText("Published v4")).toBeInTheDocument());
    expect(calls.some((c) => c.path === "/v1/blueprints/b1/versions")).toBe(true);
    expect(screen.getByText("No live sessions to upgrade.")).toBeInTheDocument();
  });

  it("marks the gate stale when the draft moves after a passing run", async () => {
    const user = userEvent.setup();
    const { client } = fakeRest({
      "POST /v1/eval-suites/scout-gate/run": { run_id: "run-1" },
      "GET /v1/eval-runs/run-1": { status: "passed", cases: [] },
    });
    const { rerender } = render(
      <ReviewScreen
        record={recordOf(validDraft())}
        catalogs={catalogs}
        scopes={["blueprints:publish"]}
        rest={client}
        pollIntervalMs={1}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Run gate" }));
    await waitFor(() => expect(gateCard().getByText("Passing")).toBeInTheDocument());

    rerender(
      <ReviewScreen
        record={recordOf({ ...validDraft(), goal: "A moved goal" })}
        catalogs={catalogs}
        scopes={["blueprints:publish"]}
        rest={client}
        pollIntervalMs={1}
      />,
    );
    await waitFor(() => expect(gateCard().getByText("Stale")).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Publish v1" })).toBeDisabled();
    expect(screen.getByText("Eval gate is stale — re-run it")).toBeInTheDocument();
  });

  it("surfaces a re-diff notice on a publish conflict", async () => {
    const user = userEvent.setup();
    const base = fakeRest({
      "POST /v1/eval-suites/scout-gate/run": { run_id: "run-1" },
      "GET /v1/eval-runs/run-1": { status: "passed", cases: [] },
    });
    const client: RestClient = {
      ...base.client,
      post: (path, body, options) => {
        if (path === "/v1/blueprints") {
          return Promise.reject(
            new ProblemError({ type: "about:blank", title: "Conflict", status: 409 }),
          );
        }
        return base.client.post(path, body, options);
      },
    };
    renderScreen({ rest: client });
    await user.click(screen.getByRole("button", { name: "Run gate" }));
    await waitFor(() => expect(gateCard().getByText("Passing")).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: "Publish v1" }));
    await user.click(screen.getByRole("button", { name: "Confirm and publish" }));
    await waitFor(() =>
      expect(screen.getByText(/published head moved/)).toBeInTheDocument(),
    );
  });

  it("exports the bundle after the no-secret-values scan asserts clean", async () => {
    const user = userEvent.setup();
    const onExportBundle = vi.fn<(bundle: ExportBundle) => void>();
    renderScreen({ onExportBundle });
    await user.click(screen.getByRole("button", { name: "Export .rustyprint bundle" }));
    expect(onExportBundle).toHaveBeenCalledOnce();
    const bundle = onExportBundle.mock.calls[0][0];
    expect(bundle.scan.ok).toBe(true);
    expect(bundle.files.map((f) => f.path)).toContain("assembled-prompt.txt");
  });

  it("blocks export when the scan finds a raw secret value", async () => {
    const user = userEvent.setup();
    const onExportBundle = vi.fn();
    const draft = { ...validDraft(), secrets: { slack: "xoxb-raw-token" } };
    renderScreen({ record: recordOf(draft), onExportBundle });
    await user.click(screen.getByRole("button", { name: "Export .rustyprint bundle" }));
    expect(onExportBundle).not.toHaveBeenCalled();
    expect(screen.getByText(/Export blocked: the no-secret-values scan/)).toBeInTheDocument();
  });

  it("shows and copies the assembled prompt with its byte count", async () => {
    const user = userEvent.setup();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    renderScreen();
    expect(screen.getByText(/bytes/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show" }));
    expect(within(screen.getByLabelText("Assembled prompt")).getByText(/You are Scout\./)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining("## Goal"));
  });

  it("routes violation clicks to the owning spec file", async () => {
    const user = userEvent.setup();
    const onOpenSpecFile = vi.fn();
    renderScreen({ record: recordOf(blankDraft("compose")), onOpenSpecFile });
    await user.click(screen.getAllByText("Name is required.")[0]);
    expect(onOpenSpecFile).toHaveBeenCalledWith("agent.md");
  });
});
