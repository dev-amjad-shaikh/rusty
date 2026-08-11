import { describe, expect, it } from "vitest";
import { primaryDestinations } from "./navigation";

describe("Studio product architecture", () => {
  it("has exactly three primary destinations", () => {
    expect(primaryDestinations.map((item) => item.label)).toEqual(["Agents", "Work", "Operations"]);
  });
});
