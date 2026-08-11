import { beforeEach, describe, expect, it } from "vitest";
import { useConnectionStore } from "./connection";

const info = { service: "rusty-server" as const, version: "1", checkpointer: "json_file" as const, server_store: "json_file" as const, store_path: "/tmp", graphs: [] };

beforeEach(() => useConnectionStore.setState({ connection: null, info: null, dialogOpen: false }));

describe("connection identity", () => {
  it("derives a strong opaque tenant scope without exposing the access key", async () => {
    await useConnectionStore.getState().connect("https://rusty.example/", "tenant-secret", info);
    const connection = useConnectionStore.getState().connection!;
    expect(connection.origin).toBe("https://rusty.example");
    expect(connection.tenantFingerprint).toMatch(/^[0-9a-f]{32}$/);
    expect(connection.tenantFingerprint).not.toContain("tenant-secret");
  });

  it("does not share a namespace across distinct access boundaries", async () => {
    await useConnectionStore.getState().connect("https://rusty.example", "tenant-a", info);
    const first = useConnectionStore.getState().connection!;
    await useConnectionStore.getState().connect("https://rusty.example", "tenant-b", info);
    const second = useConnectionStore.getState().connection!;
    expect(second.tenantFingerprint).not.toBe(first.tenantFingerprint);
    expect(second.epoch).toBeGreaterThan(first.epoch);
  });
});
