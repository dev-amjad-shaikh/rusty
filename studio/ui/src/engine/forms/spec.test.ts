// Reading a connection specification. The studio knows nothing about any
// particular connector — these tests hold that claim: the same code that
// renders ServiceNow's four auth alternatives renders a connector nobody has
// written yet, and the config it builds matches one declared shape exactly.

import { describe, expect, it } from "vitest";
import { buildConfig, fieldsOf, missingRequired, readSpec, valuesFromConfig, type SpecNode } from "./spec";

/** A spec with the shapes that matter: a plain field, a choice of credential
 * shapes with discriminators, secrets, and a declared order. */
const SPEC = {
  type: "object",
  required: ["instance", "credentials"],
  additionalProperties: false,
  properties: {
    instance: { type: "string", title: "Instance", rusty_pattern_descriptor: "your-instance.example.com", rusty_order: 0 },
    credentials: {
      type: "object",
      title: "Authentication",
      rusty_order: 1,
      oneOf: [
        {
          title: "Basic",
          type: "object",
          required: ["auth", "username", "password"],
          additionalProperties: false,
          properties: {
            auth: { type: "string", const: "basic" },
            username: { type: "string", title: "Username", rusty_order: 0 },
            password: { type: "string", title: "Password", rusty_secret: true, rusty_order: 1 },
          },
        },
        {
          title: "Access token",
          type: "object",
          required: ["auth", "token"],
          additionalProperties: false,
          properties: {
            auth: { type: "string", const: "token" },
            token: { type: "string", title: "Access token", rusty_secret: true, rusty_order: 0 },
          },
        },
      ],
    },
  },
};

const choice = (nodes: SpecNode[]) => nodes.find((n) => n.kind === "choice")!;

describe("the form a connector declares", () => {
  it("is the schema, in the order and the roles it declared", () => {
    const nodes = readSpec(SPEC);
    expect(nodes.map((n) => (n.kind === "field" ? n.field.title : n.title))).toEqual(["Instance", "Authentication"]);

    const [instance] = nodes;
    expect(instance).toMatchObject({
      kind: "field",
      field: { title: "Instance", kind: "text", required: true, placeholder: "your-instance.example.com" },
    });

    // A secret is a secret because the connector said so, not because of its name.
    const basic = choice(nodes).kind === "choice" ? (choice(nodes) as Extract<SpecNode, { kind: "choice" }>).variants[0] : null;
    expect(basic!.nodes.map((n) => n.kind === "field" && [n.field.title, n.field.kind])).toEqual([
      ["Username", "text"],
      ["Password", "secret"],
    ]);
  });

  it("hides the discriminator and carries it into the config", () => {
    const nodes = readSpec(SPEC);
    const auth = choice(nodes) as Extract<SpecNode, { kind: "choice" }>;
    // The const field is never rendered…
    expect(fieldsOf(auth.variants[0].nodes).map((f) => f.title)).toEqual(["Username", "Password"]);
    // …but it is what tells the server which shape this is.
    expect(auth.variants.map((v) => v.fixed)).toEqual([{ auth: "basic" }, { auth: "token" }]);
  });

  it("builds exactly one declared shape — never a field from the branch not chosen", () => {
    const nodes = readSpec(SPEC);
    const values = {
      instance: "dev394299",
      "credentials.username": "admin",
      "credentials.password": "hunter2",
      "credentials.token": "left over from the other branch",
    };
    expect(buildConfig(nodes, values, { credentials: 0 })).toEqual({
      instance: "dev394299",
      credentials: { auth: "basic", username: "admin", password: "hunter2" },
    });
    expect(buildConfig(nodes, values, { credentials: 1 })).toEqual({
      instance: "dev394299",
      credentials: { auth: "token", token: "left over from the other branch" },
    });
  });

  it("asks only for what the chosen shape requires", () => {
    const nodes = readSpec(SPEC);
    expect(missingRequired(nodes, {}, { credentials: 0 })).toEqual(["Instance", "Username", "Password"]);
    expect(missingRequired(nodes, {}, { credentials: 1 })).toEqual(["Instance", "Access token"]);
    expect(missingRequired(nodes, { instance: "dev", "credentials.token": "t" }, { credentials: 1 })).toEqual([]);
  });

  it("renders a connector it has never seen, with no rule of its own", () => {
    // Nothing about this connector exists anywhere in the studio.
    const nodes = readSpec({
      type: "object",
      required: ["api_key"],
      additionalProperties: false,
      properties: {
        api_key: { type: "string", title: "API key", rusty_secret: true, rusty_order: 0 },
        region: { type: "string", title: "Region", enum: ["eu", "us"], rusty_order: 1 },
      },
    });
    expect(nodes.map((n) => n.kind === "field" && [n.field.title, n.field.kind, n.field.required])).toEqual([
      ["API key", "secret", true],
      ["Region", "select", false],
    ]);
    expect(buildConfig(nodes, { api_key: "k", region: "eu" }, {})).toEqual({ api_key: "k", region: "eu" });
  });

  it("reads a stored connection back into the form, secrets left empty, the right shape chosen", () => {
    const nodes = readSpec(SPEC);
    // What the server serves: non-secret values in the clear, sealed ones marked.
    const stored = { instance: "dev394299", credentials: { auth: "token", token: { rusty_secret: true } } };
    const { values, chosen } = valuesFromConfig(nodes, stored);
    expect(chosen).toEqual({ credentials: 1 });
    expect(values).toEqual({ instance: "dev394299" });
    // Rotating is typing the new secret and nothing else.
    expect(missingRequired(nodes, values, chosen)).toEqual(["Access token"]);
    expect(buildConfig(nodes, { ...values, "credentials.token": "new-token" }, chosen)).toEqual({
      instance: "dev394299",
      credentials: { auth: "token", token: "new-token" },
    });
  });
});
