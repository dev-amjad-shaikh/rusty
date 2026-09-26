import { describe, expect, it } from "vitest";
import { connectorProblems, effectFor, emptyConnectorDraft, emptyOperation } from "./connectorDraft";

const draft = () => ({
  ...emptyConnectorDraft(),
  name: "Acme Billing",
  description: "Invoices and customers.",
  base_url: "https://api.acme.com/",
  documentation_url: "https://docs.acme.com",
  auth: "bearer" as const,
  check_path: "/me",
  operations: [
    { ...emptyOperation(), name: "List invoices", description: "List invoices for a customer.", method: "GET" as const, path: "/customers/{customer_id}/invoices", effect: "read_only" as const,
      params: [{ name: "customer_id", type: "string" as const, required: true, description: "The customer" }, { name: "limit", type: "integer" as const, required: false, description: "" }] },
    { ...emptyOperation(), name: "Void invoice", description: "Void an invoice.", method: "POST" as const, path: "/invoices/{id}/void", effect: "irreversible" as const,
      params: [{ name: "id", type: "string" as const, required: true, description: "" }] },
  ],
});

describe("connectorDraft", () => {
  it("names the problems a builder must fix, in order", () => {
    const empty = emptyConnectorDraft();
    expect(connectorProblems(empty)).toEqual([
      "Name the system.",
      "The API root must start with https://.",
      "Link to its documentation.",
      "Operation 1: give it a name.",
      "Operation 1: say what it does — the model reads this to choose it.",
    ]);
    const d = draft();
    d.operations[0].params = [];
    expect(connectorProblems(d)).toEqual(["Operation List invoices: {customer_id} in the path needs a parameter of that name."]);
    expect(connectorProblems(draft())).toEqual([]);
  });

  it("defaults the effect to the honest one for the method", () => {
    expect(effectFor("GET")).toBe("read_only");
    expect(effectFor("PUT")).toBe("idempotent");
    expect(effectFor("POST")).toBe("compensatable");
    expect(effectFor("DELETE")).toBe("irreversible");
  });
});
