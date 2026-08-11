import { describe, expect, it } from "vitest";
import { bytePreview, evidencePreview, isUnicodeScalarString } from "./text";

describe("user-visible evidence text", () => {
  it("rejects strings Rust cannot deserialize", () => {
    expect(isUnicodeScalarString("valid 😀 text")).toBe(true);
    expect(isUnicodeScalarString("broken \ud800 text")).toBe(false);
    expect(isUnicodeScalarString("broken \udfff text")).toBe(false);
  });

  it("renders hidden controls injectively inside a byte boundary", () => {
    expect(evidencePreview("actual\u202evalue", 100)).toBe("actual\\u{202e}value");
    expect(evidencePreview("literal\\u{202e}value", 100)).toBe("literal\\\\u{202e}value");
    expect(new TextEncoder().encode(evidencePreview("😀".repeat(200), 64).replace(/…$/, "")).byteLength).toBeLessThanOrEqual(64);
  });

  it("cuts raw UTF-8 evidence only at a valid scalar boundary", () => {
    const preview = bytePreview(`prefix-${"😀".repeat(20)}`, 14);
    expect(preview).toMatchObject({ text: "prefix-😀", truncated: true, bytes: 87 });
    expect(new TextEncoder().encode(preview.text).byteLength).toBeLessThanOrEqual(14);
  });
});
