import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { StudioApiError } from "./client";
import { forgetMemory, getMemory, queryMemory, submitCorrection, writeMemory } from "./memory";


const idA = "a".repeat(64);
const idB = "b".repeat(64);

function memoryRecord(over: Record<string, unknown> = {}) {
  return {
    memory_id: idA,
    kind: "fact",
    scope: { scope: "user", id: "user-7" },
    provenance: { author: { type: "human", human_id: "amjad" }, written_at: "2026-08-09T06:00:00Z" },
    confidence: 1,
    validity: { valid_from: "2026-08-09T06:00:00Z" },
    created_at: "2026-08-09T06:00:00Z",
    content: { kind: "inline", value: { timezone: "Asia/Dubai" } },
    ...over,
  };
}

function stubFetch(handler: (url: URL, init?: RequestInit) => Response) {
  vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string | URL | Request, init?: RequestInit) => {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input : input.url, "http://studio.local");
    return Promise.resolve(handler(url, init));
  }));
}

function json(value: unknown, status = 200) { return new Response(JSON.stringify(value), { status }); }

afterEach(() => vi.unstubAllGlobals());

describe("memory api receipts", () => {
  it("rejects a query result that crosses the declared filters", async () => {
    stubFetch(() => json({ records: [memoryRecord({ scope: { scope: "tenant", id: "acme" } })] }));
    await expect(queryMemory({ scope: { scope: "user", id: "user-7" } })).rejects.toThrow(/outside the exact filters/);
  });

  it("rejects a write receipt whose record drifts from the reviewed write", async () => {
    stubFetch(() => json({ memory_id: idA, created: true, record: memoryRecord({ scope: { scope: "tenant", id: "acme" } }) }, 201));
    const attempt = writeMemory({
      kind: "fact",
      scope: { scope: "user", id: "user-7" },
      content: { timezone: "Asia/Dubai" },
      author: { type: "human", human_id: "amjad" },
    });
    await expect(attempt).rejects.toThrow(/different kind, scope, or author/);
    await attempt.catch((caught) => expect((caught as StudioApiError).mayHaveCommitted).toBe(true));
  });

  it("accepts an idempotent re-write receipt (200, created: false)", async () => {
    stubFetch(() => json({ memory_id: idA, created: false, record: memoryRecord() }));
    const receipt = await writeMemory({
      kind: "fact",
      scope: { scope: "user", id: "user-7" },
      content: { timezone: "Asia/Dubai" },
      author: { type: "human", human_id: "amjad" },
    });
    expect(receipt.created).toBe(false);
    expect(receipt.memory_id).toBe(idA);
  });

  it("rejects a record read that crosses identity", async () => {
    stubFetch(() => json(memoryRecord({ memory_id: idB })));
    await expect(getMemory(idA)).rejects.toThrow(/different content address/);
  });

  it("rejects a correction receipt that loses the attribution", async () => {
    stubFetch(() => json({
      correction_id: "corr-1",
      attribution: "human:maya via correction:corr-1",
      candidate: true,
      memory_id: idB,
      created: true,
      record: memoryRecord({ memory_id: idB, provenance: { author: { type: "human", human_id: "maya" }, written_at: "2026-08-09T06:00:00Z" } }),
      superseded: idA,
      example_id: null,
    }, 201));
    await expect(submitCorrection({
      correction_id: "corr-1",
      author: "maya",
      targetMemoryId: idA,
      corrected: { timezone: "Asia/Dubai" },
      scope: { scope: "user", id: "user-7" },
    })).rejects.toThrow(/different correction/);
  });

  it("rejects a forget receipt that names a different record", async () => {
    stubFetch(() => json({
      forgotten: [idB],
      invalidated: [],
      tombstone: { memory_id: idB, scope: { scope: "user", id: "user-7" }, reason: "retracted" },
    }));
    const attempt = forgetMemory(idA, "retracted");
    await expect(attempt).rejects.toThrow(/different record/);
    await attempt.catch((caught) => expect((caught as StudioApiError).mayHaveCommitted).toBe(true));
  });
});
