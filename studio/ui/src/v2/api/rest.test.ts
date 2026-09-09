import { describe, expect, it } from "vitest";
import { createRestClient, isMissingIdempotencyKey, ProblemError, type CursorPage } from "./rest";

function jsonResponse(status: number, body: unknown, contentType = "application/json"): Response {
  const text = typeof body === "string" ? body : JSON.stringify(body);
  return new Response(text, { status, headers: { "Content-Type": contentType } });
}

function recordingFetch(handler: (path: string, init: RequestInit) => Response) {
  const calls: { path: string; init: RequestInit }[] = [];
  const fetchImpl = (async (input: RequestInfo | URL, init: RequestInit = {}) => {
    calls.push({ path: String(input), init });
    return handler(String(input), init);
  }) as typeof fetch;
  return { calls, fetchImpl };
}

describe("v2 REST client", () => {
  it("carries an auto-generated Idempotency-Key on side-effecting POSTs", async () => {
    const { calls, fetchImpl } = recordingFetch(() => jsonResponse(201, { id: "bp_1" }));
    const client = createRestClient({ baseUrl: "/api", fetchImpl, newIdempotencyKey: () => "key-1" });
    const result = await client.post<{ id: string }>("/v1/blueprints", { name: "a" });
    expect(calls).toHaveLength(1);
    expect(calls[0].path).toBe("/api/v1/blueprints");
    expect(new Headers(calls[0].init.headers).get("Idempotency-Key")).toBe("key-1");
    expect(result.idempotencyKey).toBe("key-1");
    expect(result.data).toEqual({ id: "bp_1" });
  });

  it("reuses the caller's key so a retry is the same operation", async () => {
    const { calls, fetchImpl } = recordingFetch(() => jsonResponse(200, { ok: true }));
    const client = createRestClient({ baseUrl: "/api", fetchImpl });
    await client.post("/v1/runs", { objective: "x" }, { idempotencyKey: "fixed" });
    await client.post("/v1/runs", { objective: "x" }, { idempotencyKey: "fixed" });
    expect(new Headers(calls[0].init.headers).get("Idempotency-Key")).toBe("fixed");
    expect(new Headers(calls[1].init.headers).get("Idempotency-Key")).toBe("fixed");
  });

  it("does not send an Idempotency-Key on GETs", async () => {
    const { calls, fetchImpl } = recordingFetch(() => jsonResponse(200, { items: [], next_cursor: null }));
    const client = createRestClient({ baseUrl: "/api", fetchImpl });
    await client.get("/v1/blueprints");
    expect(new Headers(calls[0].init.headers).get("Idempotency-Key")).toBeNull();
  });

  it("paginates by cursor until next_cursor is null", async () => {
    const pages: Record<string, CursorPage<number>> = {
      "/api/v1/sessions?limit=2": { items: [1, 2], next_cursor: "c2" },
      "/api/v1/sessions?cursor=c2&limit=2": { items: [3], next_cursor: null },
    };
    const { fetchImpl } = recordingFetch((path) => jsonResponse(200, pages[path]));
    const client = createRestClient({ baseUrl: "/api", fetchImpl });
    const seen: number[] = [];
    for await (const item of client.paginate<number>("/v1/sessions", { limit: 2 })) seen.push(item);
    expect(seen).toEqual([1, 2, 3]);
  });

  it("surfaces RFC 9457 problem JSON with extension members", async () => {
    const problem = {
      type: "https://rusty.dev/problems/validation",
      title: "Validation failed",
      status: 422,
      detail: "goal.measures requires at least one measure",
      instance: "/v1/blueprints/bp_1/validate",
      violations: 3,
    };
    const { fetchImpl } = recordingFetch(() => jsonResponse(422, problem, "application/problem+json"));
    const client = createRestClient({ baseUrl: "/api", fetchImpl });
    const caught = (await client.get("/v1/blueprints/bp_1/validate").catch((error) => error)) as ProblemError;
    expect(caught).toBeInstanceOf(ProblemError);
    expect(caught.problem).toMatchObject(problem);
    expect(caught.status).toBe(422);
    expect(caught.message).toBe(problem.detail);
  });

  it("marks the 428 publish precondition as a typed, identifiable error", async () => {
    const { fetchImpl } = recordingFetch(() =>
      jsonResponse(428, { type: "about:blank", title: "Precondition Required", status: 428, detail: "Idempotency-Key required" }));
    const client = createRestClient({ baseUrl: "/api", fetchImpl });
    const caught = await client.post("/v1/blueprints/bp_1/versions", { draft: {} }).catch((error) => error);
    expect(isMissingIdempotencyKey(caught)).toBe(true);
  });

  it("synthesizes a problem for non-problem error bodies", async () => {
    const { fetchImpl } = recordingFetch(() => new Response("nope", { status: 500 }));
    const client = createRestClient({ baseUrl: "/api", fetchImpl });
    const caught = (await client.get("/v1/blueprints").catch((error) => error)) as ProblemError;
    expect(caught).toBeInstanceOf(ProblemError);
    expect(caught.problem.status).toBe(500);
  });

  it("flags mutations that may have committed on network failure", async () => {
    const failing = (async () => { throw new TypeError("fetch failed"); }) as typeof fetch;
    const client = createRestClient({ baseUrl: "/api", fetchImpl: failing });
    const mutation = (await client.post("/v1/runs", {}).catch((error) => error)) as ProblemError;
    expect(mutation.mayHaveCommitted).toBe(true);
    const read = (await client.get("/v1/runs").catch((error) => error)) as ProblemError;
    expect(read.mayHaveCommitted).toBe(false);
  });
});
