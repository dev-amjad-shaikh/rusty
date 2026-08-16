import { describe, expect, it } from "vitest";
import { connectorManifestSchema } from "./connectors";

// The connector plane's contract tests: the fixtures below are trimmed
// copies of the exact JSON the runtime's service packs serialize (see
// `rusty-core/examples/pack_manifests.rs`), so a schema drift on either
// side fails here rather than in the UI.
const httpApiManifest = {
  id: "servicenow",
  version: "1",
  display_name: "ServiceNow",
  description: "ServiceNow Table API: list, read, create, update, and delete records on any table.",
  provider: {
    kind: "http_api",
    base_url: "https://example.service-now.com",
    auth: { style: "basic", username_slot: "username", password_slot: "password" },
    default_headers: [],
    health_check: null,
    operations: [
      {
        name: "list-records",
        description: "List records on a table, with sysparm filtering.",
        method: "GET",
        path: "/api/now/table/{table}",
        params_schema: {
          type: "object",
          properties: {
            table: { type: "string" },
            sysparm_query: { type: "string" },
          },
          required: ["table"],
        },
        query_params: ["sysparm_query"],
        body: { type: "none" },
        effect: "read_only",
        response: { projection: "/result", max_bytes: null },
        timeout_ms: null,
        idempotency_key_header: null,
      },
      {
        name: "create-record",
        description: "Create a record on a table.",
        method: "POST",
        path: "/api/now/table/{table}",
        params_schema: {
          type: "object",
          properties: {
            table: { type: "string" },
            short_description: { type: "string" },
          },
          required: ["table"],
        },
        query_params: [],
        body: { type: "json", params: ["short_description"] },
        effect: "compensatable",
        response: { projection: "/result", max_bytes: null },
        timeout_ms: null,
        idempotency_key_header: null,
      },
    ],
  },
  capabilities: ["servicenow table api"],
  credential_slots: [
    { name: "password", description: "Instance password or OAuth client secret." },
    { name: "username", description: "Integration user name." },
  ],
  hash: "ab".repeat(32),
};

const graphqlManifest = {
  ...httpApiManifest,
  id: "linear",
  display_name: "Linear",
  provider: {
    kind: "http_api",
    base_url: "https://api.linear.app",
    auth: { style: "bearer_token", credential_slot: "api_key" },
    default_headers: [],
    health_check: null,
    operations: [
      {
        name: "create-issue",
        description: "Create an issue.",
        method: "POST",
        path: "/graphql",
        params_schema: {
          type: "object",
          properties: { title: { type: "string" } },
          required: ["title"],
        },
        query_params: [],
        body: {
          type: "graphql",
          query: "mutation {{ issueCreate(input: {{ title: {title} }}) {{ success }} }}",
        },
        effect: "compensatable",
        response: { projection: "/data", max_bytes: null },
        timeout_ms: null,
        idempotency_key_header: null,
      },
    ],
  },
};

describe("connector manifest contract", () => {
  it("admits a real http_api pack manifest", () => {
    const parsed = connectorManifestSchema.parse(httpApiManifest);
    expect(parsed.provider.kind).toBe("http_api");
    if (parsed.provider.kind === "http_api") {
      expect(parsed.provider.operations).toHaveLength(2);
      expect(parsed.provider.auth).toMatchObject({ style: "basic" });
    }
  });

  it("admits the graphql body style with brace-escaped templates", () => {
    const parsed = connectorManifestSchema.parse(graphqlManifest);
    if (parsed.provider.kind === "http_api") {
      const body = parsed.provider.operations[0].body;
      expect(body).toMatchObject({ type: "graphql" });
    }
  });

  it("still rejects an unknown provider kind", () => {
    const forged = {
      ...httpApiManifest,
      provider: { kind: "grpc_stream", target: "localhost:50051" },
    };
    expect(connectorManifestSchema.safeParse(forged).success).toBe(false);
  });
});
