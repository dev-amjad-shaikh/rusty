import type { ConnectorInstance, ConnectorManifest } from "../../lib/api/connectors";

export const SERVICENOW_HASH = "a".repeat(64);

/** The seeded demo pack, mirrored from `rusty-server/examples/server_demo.rs`. */
export function servicenowManifest(over: Record<string, unknown> = {}): ConnectorManifest {
  return {
    id: "servicenow",
    version: "1",
    display_name: "ServiceNow",
    description: "ServiceNow Table API: get and list records in any table, and create incidents.",
    documentation_url: "https://www.servicenow.com/docs/",
    base_url: "https://{instance}.service-now.com",
    connection_specification: {
      $schema: "http://json-schema.org/draft-07/schema#",
      title: "ServiceNow Connection Spec",
      type: "object",
      required: ["instance", "credentials"],
      additionalProperties: false,
      properties: {
        instance: {
          type: "string",
          title: "Instance",
          pattern: "^[a-z0-9-]+$",
          rusty_pattern_descriptor: "your-instance.service-now.com",
          rusty_order: 0,
        },
        credentials: {
          type: "object",
          title: "Authentication",
          rusty_order: 1,
          rusty_group: "auth",
          oneOf: [
            {
              title: "Basic",
              type: "object",
              required: ["auth", "username", "password"],
              additionalProperties: false,
              properties: {
                auth: { type: "string", const: "basic" },
                username: { type: "string", title: "Username", rusty_secret: true },
                password: { type: "string", title: "Password", rusty_secret: true },
              },
            },
            {
              title: "OAuth token",
              type: "object",
              required: ["auth", "token"],
              additionalProperties: false,
              properties: {
                auth: { type: "string", const: "oauth" },
                token: { type: "string", title: "Access token", rusty_secret: true },
              },
            },
          ],
        },
      },
    },
    operations: [
      { name: "check-connection", description: "Verify connectivity and credentials by reading one sys_user row.", method: "GET", path: "/api/now/table/sys_user?sysparm_limit=1", effect: "read_only", params_schema: { type: "object" } },
      { name: "create-incident", description: "Create an incident in ServiceNow.", method: "POST", path: "/api/now/table/incident", effect: "compensatable", params_schema: { type: "object" } },
      { name: "get-record", description: "Get one record from a ServiceNow table by sys_id.", method: "GET", path: "/api/now/table/{table}/{sys_id}", effect: "read_only", params_schema: { type: "object" } },
      { name: "list-records", description: "List records from a ServiceNow table, with sysparm filtering and pagination.", method: "GET", path: "/api/now/table/{table}", effect: "read_only", params_schema: { type: "object" } },
    ],
    check: "check-connection",
    hash: SERVICENOW_HASH,
    ...over,
  } as ConnectorManifest;
}

export function servedInstance(over: Record<string, unknown> = {}): ConnectorInstance {
  return {
    instance_id: "inst-0123456789abcdef",
    manifest_hash: SERVICENOW_HASH,
    config: {
      instance: "acme",
      credentials: { auth: "basic", username: { rusty_secret: true }, password: { rusty_secret: true } },
    },
    created_at: "2026-08-17T09:30:00Z",
    ...over,
  };
}
