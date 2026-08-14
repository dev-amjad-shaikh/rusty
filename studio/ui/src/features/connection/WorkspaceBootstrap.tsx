import { useEffect } from "react";
import { getServerInfo, StudioApiError } from "../../lib/api/client";
import type { ServerInfo } from "../../lib/contracts";
import { useConnectionStore } from "../../state/connection";

interface DiscoveredWorkspace {
  origin: string;
  info: ServerInfo;
}

export const WORKSPACE_DISCOVERY_TIMEOUT_MS = 2_500;

let pendingDiscovery: Promise<DiscoveredWorkspace> | null = null;

export function localWorkspaceOrigin(location: Pick<Location, "origin" | "protocol"> = window.location) {
  if (location.protocol === "http:" || location.protocol === "https:") {
    return `${location.origin.replace(/\/$/, "")}/api`;
  }
  return "http://127.0.0.1:8100";
}

export function workspaceDisplayName(origin: string) {
  if (origin === localWorkspaceOrigin() || origin === window.location.origin.replace(/\/$/, "")) return "Local workspace";
  try {
    return new URL(origin).host;
  } catch {
    return "Rusty workspace";
  }
}

export function workspaceDiscoveryMessage(error: unknown) {
  if (error instanceof StudioApiError && (error.status === 401 || error.status === 403)) {
    return "This workspace needs an access key.";
  }
  return "Rusty is not available at the local workspace yet.";
}

async function discoverWorkspace(): Promise<DiscoveredWorkspace> {
  const local = localWorkspaceOrigin();
  const sameOrigin = window.location.origin.replace(/\/$/, "");
  const candidates = [...new Set([local, sameOrigin])];
  let firstError: unknown = null;

  for (const origin of candidates) {
    try {
      const info = await getServerInfo({ epoch: 0, origin, apiKey: "", tenantFingerprint: "discovering" });
      return { origin, info };
    } catch (error) {
      firstError ??= error;
      if (error instanceof StudioApiError && (error.status === 401 || error.status === 403)) throw error;
    }
  }
  throw firstError ?? new StudioApiError("Rusty could not be reached.", 0);
}

function boundedDiscovery() {
  return new Promise<DiscoveredWorkspace>((resolve, reject) => {
    const timeout = window.setTimeout(() => {
      reject(new StudioApiError("Rusty did not answer the local workspace check.", 0));
    }, WORKSPACE_DISCOVERY_TIMEOUT_MS);
    void discoverWorkspace().then(resolve, reject).finally(() => window.clearTimeout(timeout));
  });
}

function sharedDiscovery() {
  pendingDiscovery ??= boundedDiscovery().finally(() => { pendingDiscovery = null; });
  return pendingDiscovery;
}

export function resetWorkspaceDiscoveryForTests() {
  pendingDiscovery = null;
}

export function WorkspaceBootstrap() {
  const connection = useConnectionStore((state) => state.connection);
  const dialogOpen = useConnectionStore((state) => state.dialogOpen);
  const workspaceStatus = useConnectionStore((state) => state.workspaceStatus);
  const discoveryAttempt = useConnectionStore((state) => state.discoveryAttempt);
  const acceptDiscovery = useConnectionStore((state) => state.acceptDiscovery);
  const failDiscovery = useConnectionStore((state) => state.failDiscovery);

  useEffect(() => {
    if (connection || dialogOpen || workspaceStatus !== "discovering") return;
    let ownsResult = true;
    void sharedDiscovery().then(({ origin, info }) => {
      if (ownsResult) void acceptDiscovery(discoveryAttempt, origin, info);
    }).catch((error) => {
      if (ownsResult) failDiscovery(discoveryAttempt, workspaceDiscoveryMessage(error), localWorkspaceOrigin());
    });
    return () => { ownsResult = false; };
  }, [acceptDiscovery, connection, dialogOpen, discoveryAttempt, failDiscovery, workspaceStatus]);

  return null;
}
