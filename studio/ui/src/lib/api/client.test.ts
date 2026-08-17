import { afterEach, describe, expect, it, vi } from "vitest";
import { createAssistant, createThread, getOperationsSnapshot, getRunEvidence, getServerInfo, savePromptVersion, StudioApiError } from "./client";


function response(value: unknown, status = 200) {
  return new Response(typeof value === "string" ? value : JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
}

afterEach(() => vi.unstubAllGlobals());

describe("Studio API boundary", () => {
  it("validates the exact server information envelope", async () => {
    const fetchMock = vi.fn().mockResolvedValue(response({
      service: "rusty-server", version: "1.0.0", api_protocol_version: 1, checkpointer: "json_file",
      server_store: "json_file", store_path: "/tmp/rusty", graphs: [{ name: "agent", channels: ["messages"], tools: [{
        name: "search_knowledge", description: "Search local references.", effect: "read_only",
        parameters_schema: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
      }] }],
    }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(getServerInfo()).resolves.toMatchObject({ service: "rusty-server", api_protocol_version: 1, graphs: [{ name: "agent", tools: [{ name: "search_knowledge", effect: "read_only" }] }] });
    expect(fetchMock).toHaveBeenCalledWith("/api/info", expect.objectContaining({ headers: expect.not.objectContaining({ "X-Api-Key": expect.anything() }) }));
  });

  it("requires an exact 201 assistant receipt", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response({}, 200)));
    await expect(createAssistant({ assistant_id: "agent-1", name: "Analyst", graph: "agent", config: {}, metadata: {} }))
      .rejects.toMatchObject({ status: 200, mayHaveCommitted: true });
  });

  it("rejects crossed assistant and thread mutation receipts", async () => {
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(response({ assistant_id: "agent-1", name: "Crossed", graph: "agent", config: {}, metadata: {}, created_at: "2026-08-11T00:00:00Z", active_version_id: "v1", version_count: 1 }, 201))
      .mockResolvedValueOnce(response({ thread_id: "thread-1", tenant: "default", graph: "other", metadata: { assistant_id: "agent-1" }, created_at: "2026-08-11T00:00:00Z" }, 201)));
    await expect(createAssistant({ assistant_id: "agent-1", name: "Analyst", graph: "agent", config: {}, metadata: {} })).rejects.toThrow("exact reviewed agent");
    await expect(createThread("agent", "agent-1")).rejects.toThrow("exact reviewed agent and behavior");
  });

  it("preserves exact event number tokens and validates the complete sequence", async () => {
    const raw = '{"run_id":"run-1","events":[{"id":"run-1:0","run_id":"run-1","thread_id":"thread-1","node_id":"agent","seq":0,"kind":"node_output","effect":"pure","input":null,"output":{"inline":{"count":18446744073709551615}},"latency_ms":18446744073709551615,"tokens":{"prompt_tokens":18446744073709551613,"completion_tokens":2,"total_tokens":18446744073709551615},"cost_usd":1e-6,"status":"ok","parent":null,"recorded_at":"2026-08-11T00:00:00Z"}],"complete":true}';
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(raw)));
    const evidence = await getRunEvidence("run-1");
    expect(evidence.events[0].seq).toBe("0");
    expect(evidence.events[0].latency_ms).toBe("18446744073709551615");
    expect(evidence.events[0].tokens).toEqual({ prompt_tokens: "18446744073709551613", completion_tokens: "2", total_tokens: "18446744073709551615" });
    expect(evidence.events[0].rawJson).toContain("18446744073709551615");
    expect(evidence.events[0].rawJson).toContain("1e-6");
  });

  it("fails closed when journal events cross thread identity", async () => {
    const event = (id: string, thread: string, seq: number) => ({ id, run_id: "run-1", thread_id: thread, node_id: null, seq, kind: "super_step_start", effect: "pure", input: null, output: null, latency_ms: null, tokens: null, cost_usd: null, status: "ok", parent: null, recorded_at: "2026-08-11T00:00:00Z" });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response({ run_id: "run-1", events: [event("run-1:0", "thread-a", 0), event("run-1:1", "thread-b", 1)], complete: true })));
    await expect(getRunEvidence("run-1")).rejects.toThrow("crossed thread identity");
  });

  it("fails closed on forged event IDs and future causal parents", async () => {
    const event = (id: string, seq: number, parent: string | null) => ({ id, run_id: "run-1", thread_id: "thread-a", node_id: null, seq, kind: "super_step_start", effect: "pure", input: null, output: null, latency_ms: null, tokens: null, cost_usd: null, status: "ok", parent, recorded_at: "2026-08-11T00:00:00Z" });
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(response({ run_id: "run-1", events: [event("forged", 0, null)], complete: true }))
      .mockResolvedValueOnce(response({ run_id: "run-1", events: [event("run-1:0", 0, "run-1:1"), event("run-1:1", 1, null)], complete: true })));
    await expect(getRunEvidence("run-1")).rejects.toThrow("event identity");
    await expect(getRunEvidence("run-1")).rejects.toThrow("causal order");
  });

  it("reports malformed contracts without treating them as server truth", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response({ service: "rusty-server", graphs: [], extra: true })));
    await expect(getServerInfo()).rejects.toBeInstanceOf(StudioApiError);
  });

  it("separates actionable task evidence from routine system counts", async () => {
    const fetchMock = vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=dead") return Promise.resolve(response([{
        task_id: "task-1", kind: "publish_report", pool: "default", status: "dead",
        last_error: "Provider rejected the write.", next_attempt_at: null,
        run_id: "run-1", thread_id: "thread-1", updated_at: "2026-08-11T00:00:00Z",
      }]));
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=failed") return Promise.resolve(response([]));
      if (url.pathname.replace(/^\/api/, "") === "/crons") return Promise.resolve(response([{ cron_id: "daily" }]));
      if (url.pathname.replace(/^\/api/, "") === "/triggers") return Promise.resolve(response([{ trigger_id: "hook", enabled: true }]));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response({ run_id: "artifact-journal", events: [], complete: false }));
      throw new Error(`unexpected ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    const snapshot = await getOperationsSnapshot();
    expect(snapshot.attention).toEqual([expect.objectContaining({ id: "task-1", runId: "run-1", retryScheduled: false })]);
    expect(snapshot.systems).toEqual({ tasks: 1, automations: 1, schedules: 1 });
    expect(snapshot.unavailable).toEqual([]);
  });

  it("keeps unavailable operational evidence distinct from an all-clear", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks") return Promise.reject(new Error("offline"));
      if (url.pathname.replace(/^\/api/, "") === "/artifacts/journal") return Promise.resolve(response({ run_id: "artifact-journal", events: [], complete: false }));
      return Promise.resolve(response([]));
    }));
    const snapshot = await getOperationsSnapshot();
    expect(snapshot.attention).toEqual([]);
    expect(snapshot.systems.tasks).toBeNull();
    expect(snapshot.unavailable).toContain("task queue");
  });

  it("creates a prompt version under Rust's exact content address and commits it", async () => {
    const expected = "4859edea224c5cbe1fb1eab37fbee365f0c64b183839f51a3e5331f58574362f";
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const url = new URL(input, "http://studio.local");
      const body = init?.body ? JSON.parse(String(init.body)) : null;
      if (url.pathname.replace(/^\/api/, "") === "/registry/artifacts") return Promise.resolve(response({
        surface: "prompt:system", created: true,
        artifact: { surface: "prompt:system", family: "prompt", owner: { type: "human", human_id: "amjad" }, created_at: "2026-08-11T00:00:00Z" },
      }, 201));
      if (url.pathname.replace(/^\/api/, "") === "/learn/candidates") {
        expect(body.candidate.candidate_id).toBe(expected);
        expect(body.candidate.content).toEqual({ kind: "prompt", name: "system", prompt: "You are a careful support agent. Answer tersely." });
        return Promise.resolve(response({ candidate_id: expected, created: true, record: { candidate: body.candidate, status: "created" } }, 201));
      }
      if (url.pathname.replace(/^\/api/, "").endsWith("/commits")) return Promise.resolve(response({ surface: "prompt:system", committed: true, commit: { candidate_id: expected, committed_at: "2026-08-11T00:00:01Z" }, commits: 1 }));
      throw new Error(`unexpected ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);
    await expect(savePromptVersion({ name: "system", prompt: "You are a careful support agent. Answer tersely.", humanId: "amjad", runId: "run-1", artifactExists: false }))
      .resolves.toEqual({ candidateId: expected, created: true, committed: true });
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("rejects crossed prompt mutation status and invalid Unicode", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response({
      candidate_id: "a".repeat(64), created: true,
      record: { candidate: { candidate_id: "a".repeat(64), content: { kind: "prompt", name: "system", prompt: "Safe" }, distilled_by: { type: "human", human_id: "amjad" }, created_at: "2026-08-11T00:00:00Z" }, status: "created" },
    }, 200)));
    await expect(savePromptVersion({ name: "system", prompt: "Safe", humanId: "amjad", runId: "run-1", artifactExists: true })).rejects.toThrow("mutation status");
    await expect(savePromptVersion({ name: "system", prompt: "broken \ud800", humanId: "amjad", runId: "run-1", artifactExists: true })).rejects.toThrow("invalid Unicode");
  });

  it("accepts server-legal large task evidence while bounding its presentation", async () => {
    const longError = `failed\u202e${"x".repeat(70_000)}`;
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const url = new URL(input, "http://studio.local");
      if (url.pathname.replace(/^\/api/, "") === "/tasks" && url.search === "?status=dead") return Promise.resolve(response([{ task_id: "task-1", kind: "publish", pool: "default", status: "dead", last_error: longError, next_attempt_at: null, run_id: null, thread_id: null, updated_at: "2026-08-11T00:00:00Z" }]));
      if (url.pathname.replace(/^\/api/, "") === "/tasks") return Promise.resolve(response([]));
      return Promise.resolve(response([]));
    }));
    const snapshot = await getOperationsSnapshot();
    expect(snapshot.attention[0].detail).toContain("\\u{202e}");
    expect(new TextEncoder().encode(snapshot.attention[0].detail).byteLength).toBeLessThanOrEqual(503);
  });
});
