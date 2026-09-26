import { describe, expect, it } from "vitest";
import { signInOf } from "./openapiSignIn";

describe("sign-in from an API description", () => {
  it("a document with no security scheme is public", () => {
    expect(signInOf({ openapi: "3.0.0", paths: {} })).toEqual({ auth: "none" });
  });
  it("reads an API key's place and name", () => {
    expect(signInOf({ components: { securitySchemes: { k: { type: "apiKey", in: "header", name: "X-Key" } } } })).toEqual({ auth: "header", name: "X-Key" });
    expect(signInOf({ components: { securitySchemes: { k: { type: "apiKey", in: "query", name: "appid" } } } })).toEqual({ auth: "query", name: "appid" });
  });
  it("prefers the scheme the document requires", () => {
    const spec = { security: [{ basicAuth: [] }], components: { securitySchemes: { bearerAuth: { type: "http", scheme: "bearer" }, basicAuth: { type: "http", scheme: "basic" } } } };
    expect(signInOf(spec)).toEqual({ auth: "basic" });
  });
});
