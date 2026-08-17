import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { Assistant } from "../contracts";
import {
  activateAssistantVersion,
  assistantVersionContentAddress,
  createAssistantVersion,
  listAssistantVersions,
  setAssistantLifecycle,
  type AssistantVersion,
} from "./assistants";

let v1 = "";
let v2 = "";
const createdAt = "2026-08-11T00:00:00Z";
const baseConfig = { studio_intent: { format: "rusty.agent-intent/v3" } };
const baseMetadata = { description: "Verify a claim" };
const nextConfig = { studio_intent: { format: "rusty.agent-intent/v3", model: "model-v2" } };
const nextMetadata = { description: "Verify a claim with citations" };

beforeAll(async () => {
  v1 = await assistantVersionContentAddress({ parent_version_id: null, name: "Research analyst", graph: "research", config: baseConfig, metadata: baseMetadata });
  v2 = await assistantVersionContentAddress({ parent_version_id: v1, name: "Research analyst v2", graph: "research", config: nextConfig, metadata: nextMetadata });
});

function assistant(overrides: Partial<Assistant> = {}): Assistant {
  return {
    assistant_id: "analyst",
    name: "Research analyst",
    graph: "research",
    config: baseConfig,
    metadata: baseMetadata,
    created_at: createdAt,
    active_version_id: v1,
    version_count: 2,
    ...overrides,
  };
}

function version(overrides: Partial<AssistantVersion> = {}): AssistantVersion {
  return {
    version_id: v2,
    parent_version_id: v1,
    name: "Research analyst v2",
    graph: "research",
    config: nextConfig,
    metadata: nextMetadata,
    created_at: "2026-08-11T01:00:00Z",
    active: false,
    ...overrides,
  };
}

function response(value: unknown, status = 200) {
  return Promise.resolve(new Response(JSON.stringify(value), { status }));
}

afterEach(() => vi.unstubAllGlobals());

describe("assistant lifecycle API", () => {
  it("accepts one immutable lineage in any array order and rejects a disconnected parent", async () => {
    const valid = {
      assistant_id: "analyst",
      active_version_id: v1,
      assistant: assistant(),
      versions: [
        { version_id: v1, graph: "research", created_at: createdAt, active: true },
        { version_id: v2, parent_version_id: v1, graph: "research", created_at: "2026-08-11T01:00:00Z", active: false },
      ],
    };
    vi.stubGlobal("fetch", vi.fn().mockImplementationOnce(() => response(valid))
      .mockImplementationOnce(() => response({ ...valid, versions: [valid.versions[1], valid.versions[0]] }))
      .mockImplementationOnce(() => response({ ...valid, versions: [valid.versions[0], { ...valid.versions[1], parent_version_id: `av-${"f".repeat(64)}` }] })));

    await expect(listAssistantVersions("analyst")).resolves.toMatchObject({ active_version_id: v1 });
    await expect(listAssistantVersions("analyst")).resolves.toMatchObject({ active_version_id: v1 });
    await expect(listAssistantVersions("analyst")).rejects.toThrow("coherent immutable lineage");
  });

  it("fails closed before a legal Rust integer can be rounded in immutable evidence", async () => {
    const valid = {
      assistant_id: "analyst",
      active_version_id: v1,
      assistant: assistant(),
      versions: [{ version_id: v1, graph: "research", created_at: createdAt, active: true }],
    };
    valid.assistant.version_count = 1;
    const raw = JSON.stringify(valid).replace('"config":{"studio_intent"', '"config":{"unsafe":18446744073709551615,"studio_intent"');
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(raw)));

    await expect(listAssistantVersions("analyst")).rejects.toThrow("cannot preserve exactly");
  });

  it("rejects an active body that does not match the active content address", async () => {
    const crossed = {
      assistant_id: "analyst", active_version_id: v1,
      assistant: assistant({ name: "Crossed definition" }),
      versions: [{ version_id: v1, graph: "research", created_at: createdAt, active: true }],
    };
    crossed.assistant.version_count = 1;
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => response(crossed)));
    await expect(listAssistantVersions("analyst")).rejects.toThrow("content address");
  });

  it("binds a staged version to its base, exact fields, and HTTP status", async () => {
    const fields = { name: "Research analyst v2", graph: "research", config: version().config, metadata: version().metadata };
    const receipt = { assistant_id: "analyst", created: true, active_version_id: v1, version: version() };
    vi.stubGlobal("fetch", vi.fn().mockImplementationOnce(() => response(receipt, 201))
      .mockImplementationOnce(() => response({ ...receipt, version: { ...receipt.version, graph: "other" } }, 201)));

    await expect(createAssistantVersion("analyst", v1, fields)).resolves.toMatchObject({ created: true });
    await expect(createAssistantVersion("analyst", v1, fields)).rejects.toMatchObject({ mayHaveCommitted: true });
  });

  it("binds activation and lifecycle receipts to the exact reviewed snapshots", async () => {
    const target = version();
    const activated = assistant({ name: target.name, config: target.config, metadata: target.metadata, active_version_id: v2 });
    const archived = { ...activated, archived_at: "2026-08-11T02:00:00Z" };
    vi.stubGlobal("fetch", vi.fn()
      .mockImplementationOnce(() => response({ assistant: activated, activated: true }))
      .mockImplementationOnce(() => response({ assistant: archived, changed: true, lifecycle: "archived" }))
      .mockImplementationOnce(() => response({ assistant: { ...archived, graph: "other" }, changed: true, lifecycle: "archived" })));

    await expect(activateAssistantVersion("analyst", target, v1, 2)).resolves.toMatchObject({ activated: true });
    await expect(setAssistantLifecycle(activated, "archive")).resolves.toMatchObject({ lifecycle: "archived" });
    await expect(setAssistantLifecycle(activated, "archive")).rejects.toMatchObject({ mayHaveCommitted: true });
  });
});
