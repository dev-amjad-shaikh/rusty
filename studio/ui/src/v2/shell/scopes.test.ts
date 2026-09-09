import { describe, expect, it } from "vitest";
import {
  NAV,
  ROLE_SCOPES,
  hasAnyScope,
  hasScope,
  isRoutePermitted,
  nearestPermittedRoute,
  visibleGroups,
} from "./scopes";

describe("v2 scope gating (handoff 02)", () => {
  it("matches exact scopes, *, and prefix wildcards", () => {
    expect(hasScope(["*"], "anything:at-all")).toBe(true);
    expect(hasScope(["evals:*"], "evals:run")).toBe(true);
    expect(hasScope(["evals:*"], "evals:read")).toBe(true);
    expect(hasScope(["evals:read"], "evals:run")).toBe(false);
    expect(hasScope(["blueprints:read"], "blueprints:write")).toBe(false);
    expect(hasAnyScope(["approvals:decide"], ["approvals:read", "approvals:decide"])).toBe(true);
  });

  it("builder sees Build and Test; Security stays absent", () => {
    const groups = visibleGroups(ROLE_SCOPES.builder);
    expect(groups.map((group) => group.id)).toEqual(["build", "test", "operate"]);
    const operate = groups.find((group) => group.id === "operate")!;
    expect(operate.items.map((item) => item.route)).toEqual(["/observe", "/improve"]);
  });

  it("operator sees Inbox and Work; Playground stays absent", () => {
    const groups = visibleGroups(ROLE_SCOPES.operator);
    const test = groups.find((group) => group.id === "test")!;
    expect(test.items.map((item) => item.route)).toEqual(["/evals"]);
    const operate = groups.find((group) => group.id === "operate")!;
    expect(operate.items.map((item) => item.route)).toEqual(["/inbox", "/work", "/observe", "/learning"]);
  });

  it("auditor has no Build or Test group at all", () => {
    const groups = visibleGroups(ROLE_SCOPES.auditor);
    expect(groups.map((group) => group.id)).toEqual(["operate"]);
    expect(groups[0].items.map((item) => item.route)).toEqual(["/inbox", "/observe", "/learning", "/security"]);
  });

  it("admin sees every group and item", () => {
    const groups = visibleGroups(ROLE_SCOPES.admin);
    expect(groups.map((group) => group.id)).toEqual(["build", "test", "operate"]);
    expect(groups.flatMap((group) => group.items)).toHaveLength(NAV.flatMap((group) => group.items).length);
  });

  it("permits nested routes of a permitted screen", () => {
    expect(isRoutePermitted("/agents/bp_1/review", ROLE_SCOPES.builder)).toBe(true);
    expect(isRoutePermitted("/security/egress", ROLE_SCOPES.builder)).toBe(false);
    expect(isRoutePermitted("/security/egress", ROLE_SCOPES.auditor)).toBe(true);
  });

  it("role change re-routes to the nearest permitted screen in nav order", () => {
    expect(nearestPermittedRoute("/agents", ROLE_SCOPES.auditor)).toBe("/inbox");
    expect(nearestPermittedRoute("/security", ROLE_SCOPES.builder)).toBe("/agents");
    expect(nearestPermittedRoute("/work", ROLE_SCOPES.operator)).toBe("/work");
    expect(nearestPermittedRoute("/agents", [])).toBeNull();
  });
});
