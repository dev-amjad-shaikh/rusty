import { afterEach, describe, expect, it, vi } from "vitest";
import { StudioApiError } from "./client";
import {
  checkConnectorConfig,
  checkConnectorInstance,
  createConnectorInstance,
  getConnectorCatalog,
  listConnectorInstances,
  listConnectors,
} from "./connectors";

const HASH = "a".repeat(64);

function servicenowManifest() {
  return {
    id: "servicenow",
    version: "1",
    display_name: "ServiceNow",
    description: "ServiceNow Table API: get and list records in any table, and create incidents.",
    documentation_url: "https://www.servicenow.com/docs/",
    base_url: "https://{instance}.service-now.com",
    connection_specification: {
      type: "object",
      required: ["instance", "credentials"],
      properties: {
        instance: { type: "string", pattern: "^[a-z0-9-]+$", rusty_pattern_descriptor: "your-instance.service-now.com" },
        credentials: {
          type: "object",
          oneOf: [
            { title: "Basic", type: "object", required: ["auth", "username", "password"], properties: { auth: { type: "string", const: "basic" }, username: { type: "string", rusty_secret: true }, password: { type: "string", rusty_secret: true } } },
            { title: "OAuth token", type: "object", required: ["auth", "token"], properties: { auth: { type: "string", const: "oauth" }, token: { type: "string", rusty_secret: true } } },
          ],
        },
      },
    },
    operations: [
      { name: "check-connection", description: "Verify connectivity.", method: "GET", path: "/api/now/table/sys_user?sysparm_limit=1", effect: "read_only", params_schema: { type: "object" } },
    ],
    check: "check-connection",
    hash: HASH,
  };
}

function servedInstance(over: Record<string, unknown> = {}) {
  return {
    instance_id: "inst-0123456789abcdef",
    manifest_hash: HASH,
    config: { instance: "acme", credentials: { auth: "basic", username: { rusty_secret: true }, password: { rusty_secret: true } } },
    created_at: "2026-08-17T09:30:00Z",
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

describe("connector api contracts", () => {
  it("parses the manifest catalog and keeps the schema opaque", async () => {
    stubFetch((url) => {
      expect(url.pathname).toBe("/api/connectors");
      return json({ manifests: [servicenowManifest()] });
    });
    const manifests = await listConnectors();
    expect(manifests).toHaveLength(1);
    expect(manifests[0].id).toBe("servicenow");
    expect(manifests[0].hash).toBe(HASH);
    expect(manifests[0].operations[0]).toMatchObject({ name: "check-connection", method: "GET", effect: "read_only" });
  });

  it("rejects a manifest catalog that drifts from the wire contract", async () => {
    stubFetch(() => json({ manifests: [{ id: "servicenow" }] }));
    await expect(listConnectors()).rejects.toThrow(/did not match the Rusty contract/);
  });

  it("parses served instances with masked secret markers intact", async () => {
    stubFetch(() => json({ instances: [servedInstance()] }));
    const instances = await listConnectorInstances();
    expect(instances[0].config.credentials).toEqual({
      auth: "basic",
      username: { rusty_secret: true },
      password: { rusty_secret: true },
    });
  });

  it("creates an instance and verifies the receipt's manifest hash", async () => {
    stubFetch((url, init) => {
      expect(url.pathname).toBe("/api/connectors/instances");
      const body = JSON.parse(String(init?.body));
      expect(body).toEqual({ manifest_hash: HASH, config: { instance: "acme" } });
      return json(servedInstance(), 201);
    });
    const instance = await createConnectorInstance({ manifest_hash: HASH, config: { instance: "acme" } });
    expect(instance.instance_id).toBe("inst-0123456789abcdef");
  });

  it("rejects an instance receipt that names a different connector", async () => {
    stubFetch(() => json(servedInstance({ manifest_hash: "b".repeat(64) }), 201));
    const attempt = createConnectorInstance({ manifest_hash: HASH, config: { instance: "acme" } });
    await expect(attempt).rejects.toThrow(/different connector/);
    await attempt.catch((caught) => expect((caught as StudioApiError).mayHaveCommitted).toBe(true));
  });

  it("surfaces the 422 contract message for field pinning", async () => {
    stubFetch(() => json({ error: "invalid_config", message: "credentials.username: required property missing" }, 422));
    const attempt = createConnectorInstance({ manifest_hash: HASH, config: { instance: "acme" } });
    await attempt.catch((caught) => {
      expect(caught).toBeInstanceOf(StudioApiError);
      expect((caught as StudioApiError).status).toBe(422);
      expect((caught as StudioApiError).message).toBe("credentials.username: required property missing");
    });
  });

  it("checks a candidate config pre-save with manifest_hash + config", async () => {
    stubFetch((url, init) => {
      expect(url.pathname).toBe("/api/connectors/check");
      expect(JSON.parse(String(init?.body))).toEqual({ manifest_hash: HASH, config: { instance: "acme" } });
      return json({ status: "failed", message: "Authentication failed (401)." });
    });
    const outcome = await checkConnectorConfig(HASH, { instance: "acme" });
    expect(outcome).toEqual({ status: "failed", message: "Authentication failed (401)." });
  });

  it("re-checks a live instance by id", async () => {
    stubFetch((url, init) => {
      expect(JSON.parse(String(init?.body))).toEqual({ instance_id: "inst-0123456789abcdef" });
      return json({ status: "succeeded" });
    });
    await expect(checkConnectorInstance("inst-0123456789abcdef")).resolves.toEqual({ status: "succeeded" });
  });

  it("serves the derived tool catalog and pins its instance identity", async () => {
    stubFetch((url) => {
      expect(url.pathname).toBe("/api/connectors/instances/inst-0123456789abcdef/catalog");
      return json({
        instance_id: "inst-0123456789abcdef",
        manifest_hash: HASH,
        tools: [{ name: "servicenow/list-records", description: "List records.", parameters_schema: { type: "object" }, effect: "read_only" }],
      });
    });
    const catalog = await getConnectorCatalog("inst-0123456789abcdef");
    expect(catalog.tools[0].name).toBe("servicenow/list-records");
  });

  it("rejects a catalog that crosses instance identity", async () => {
    stubFetch(() => json({ instance_id: "inst-other", manifest_hash: HASH, tools: [] }));
    await expect(getConnectorCatalog("inst-0123456789abcdef")).rejects.toThrow(/different instance/);
  });
});
