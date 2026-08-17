import { describe, expect, it } from "vitest";
import {
  buildConfig,
  humanizeName,
  initialSelections,
  initialValues,
  interpretForm,
  knownPaths,
  pinFieldError,
  visibleFields,
  type VariantNode,
} from "./schema-form";

// The ServiceNow demo pack's connection_specification, mirrored from
// `rusty-server/examples/server_demo.rs` — the shipped idiom the interpreter
// must handle end to end.
const serviceNowSpec = {
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
};

describe("interpretForm", () => {
  it("walks the ServiceNow spec into an ordered field plus a variant picker", () => {
    const form = interpretForm(serviceNowSpec);
    expect(form.supported).toBe(true);
    expect(form.title).toBe("ServiceNow Connection Spec");
    expect(form.nodes.map((node) => node.name)).toEqual(["instance", "credentials"]);

    const instance = form.nodes[0];
    expect(instance).toMatchObject({
      kind: "field",
      path: "instance",
      title: "Instance",
      input: "text",
      required: true,
      order: 0,
      pattern: "^[a-z0-9-]+$",
      patternHint: "your-instance.service-now.com",
    });

    const credentials = form.nodes[1] as VariantNode;
    expect(credentials.kind).toBe("variant");
    expect(credentials.group).toBe("auth");
    expect(credentials.variants.map((variant) => variant.label)).toEqual(["Basic", "OAuth token"]);
    expect(credentials.variants[0].consts).toEqual({ auth: "basic" });
    expect(credentials.variants[1].consts).toEqual({ auth: "oauth" });
    // The const discriminator is not a rendered field; the secrets are.
    expect(credentials.variants[0].children.map((node) => node.name)).toEqual(["username", "password"]);
    expect(credentials.variants[0].children.every((node) => node.kind === "field" && node.input === "password")).toBe(true);
    expect(form.groups).toEqual(["auth"]);
  });

  it("renders enums as selects, numbers and booleans as their own inputs, and applies order", () => {
    const form = interpretForm({
      type: "object",
      required: ["region"],
      properties: {
        retries: { type: "integer", title: "Retries", default: 3, rusty_order: 2 },
        region: { type: "string", enum: ["eu", "us"], rusty_order: 0 },
        verbose: { type: "boolean", title: "Verbose", rusty_order: 1 },
      },
    });
    expect(form.nodes.map((node) => node.name)).toEqual(["region", "verbose", "retries"]);
    expect(form.nodes[0]).toMatchObject({ input: "select", enumValues: ["eu", "us"], required: true });
    expect(form.nodes[1]).toMatchObject({ input: "boolean", required: false });
    expect(form.nodes[2]).toMatchObject({ input: "number", defaultValue: 3 });
  });

  it("skips rusty_hidden fields and ignores unknown keywords", () => {
    const form = interpretForm({
      type: "object",
      properties: {
        visible: { type: "string", always_show: true, multiline: true, examples: ["x"] },
        legacy: { type: "string", rusty_hidden: true },
      },
    });
    expect(form.nodes.map((node) => node.name)).toEqual(["visible"]);
  });

  it("keeps standalone const fields out of the form and applies them to the config", () => {
    const form = interpretForm({
      type: "object",
      properties: {
        repository: { type: "string", const: "rusty" },
        name: { type: "string" },
      },
    });
    expect(form.nodes.map((node) => node.name)).toEqual(["repository", "name"]);
    const selections = initialSelections(form);
    const config = buildConfig(form, { name: "studio" }, selections);
    expect(config).toEqual({ repository: "rusty", name: "studio" });
  });

  it("nests plain object properties as fieldsets with dot paths", () => {
    const form = interpretForm({
      type: "object",
      properties: {
        tls: {
          type: "object",
          required: ["ca"],
          properties: { ca: { type: "string" }, verify: { type: "boolean" } },
        },
      },
    });
    const tls = form.nodes[0];
    expect(tls).toMatchObject({ kind: "object", path: "tls" });
    expect(tls.kind === "object" && tls.children.map((node) => node.path)).toEqual(["tls.ca", "tls.verify"]);
  });

  it("degrades gracefully on a non-object root and on type-less properties", () => {
    expect(interpretForm({ type: "array" }).supported).toBe(false);
    expect(interpretForm("nonsense").supported).toBe(false);
    const form = interpretForm({ type: "object", properties: { anything: { description: "no type" } } });
    expect(form.nodes[0]).toMatchObject({ kind: "field", input: "text" });
  });

  it("falls back to a picker with ordinal keys when variants carry no const discriminator", () => {
    const form = interpretForm({
      type: "object",
      properties: {
        mode: {
          oneOf: [
            { title: "Simple", type: "object", properties: { a: { type: "string" } } },
            { type: "object", properties: { b: { type: "string" } } },
          ],
        },
      },
    });
    const mode = form.nodes[0] as VariantNode;
    expect(mode.kind).toBe("variant");
    expect(mode.variants.map((variant) => variant.key)).toEqual(["variant-0", "variant-1"]);
    expect(mode.variants[0].label).toBe("Simple");
  });
});

describe("form state → config", () => {
  it("applies the selected variant's discriminator and its secrets", () => {
    const form = interpretForm(serviceNowSpec);
    const selections = { ...initialSelections(form), credentials: "oauth" };
    const config = buildConfig(form, { instance: "acme", "credentials.token": "tok-1" }, selections);
    expect(config).toEqual({ instance: "acme", credentials: { auth: "oauth", token: "tok-1" } });
  });

  it("defaults to the first variant and drops empty optional fields", () => {
    const form = interpretForm({
      type: "object",
      required: ["host"],
      properties: {
        host: { type: "string" },
        port: { type: "integer" },
        note: { type: "string" },
      },
    });
    const config = buildConfig(form, { host: "example.internal", port: "8443", note: "" }, {});
    expect(config).toEqual({ host: "example.internal", port: 8443 });
  });

  it("seeds defaults and booleans as initial values", () => {
    const form = interpretForm({
      type: "object",
      properties: {
        region: { type: "string", default: "eu" },
        verbose: { type: "boolean" },
      },
    });
    const selections = initialSelections(form);
    expect(initialValues(form, selections)).toEqual({ region: "eu", verbose: false });
    expect(visibleFields(form, selections).map((field) => field.path)).toEqual(["region", "verbose"]);
  });
});

describe("pinFieldError (the 422 contract)", () => {
  const paths = knownPaths(interpretForm(serviceNowSpec));

  it("pins a missing required property inside the selected variant", () => {
    expect(pinFieldError("credentials.username: required property missing", paths))
      .toEqual({ path: "credentials.username", reason: "required property missing" });
  });

  it("pins a pattern rejection at its own field", () => {
    expect(pinFieldError('instance: "Acme Corp" does not match pattern', paths))
      .toEqual({ path: "instance", reason: '"Acme Corp" does not match pattern' });
  });

  it("falls back to the variant parent for paths inside a branch", () => {
    expect(pinFieldError("credentials.passphrase: unknown property", paths))
      .toEqual({ path: "credentials", reason: "passphrase: unknown property" });
  });

  it("returns null for messages without a field path", () => {
    expect(pinFieldError("not valid under any of the given schemas", paths)).toBeNull();
    expect(pinFieldError("connection refused", paths)).toBeNull();
  });
});

describe("humanizeName", () => {
  it("spaces separators and capitalizes", () => {
    expect(humanizeName("access_token")).toBe("Access token");
    expect(humanizeName("your-instance")).toBe("Your instance");
  });
});
