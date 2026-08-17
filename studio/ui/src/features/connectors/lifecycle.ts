import type {
  ConnectorManifest,
  CredentialSlot,
  LifecycleState,
  VaultConnection,
} from "../../lib/api/connectors";

export interface StatePresentation {
  label: string;
  tone: "neutral" | "active" | "good" | "warn" | "bad" | "off";
  summary: string;
}

export const statePresentation: Record<LifecycleState, StatePresentation> = {
  pending: { label: "pending", tone: "neutral", summary: "Registered, never connected" },
  connecting: { label: "connecting", tone: "active", summary: "Connection attempt in flight" },
  healthy: { label: "healthy", tone: "good", summary: "Connected and passing health checks" },
  degraded: { label: "degraded", tone: "warn", summary: "Connected but failing health checks" },
  failed: { label: "failed", tone: "bad", summary: "Connection or instantiation failed" },
  disabled: { label: "disabled", tone: "off", summary: "Parked; rejects connection attempts" },
};

/// Which lifecycle actions the server admits from each state. The server
/// remains the authority — a guard violation is a 409 and is surfaced —
/// but the buttons follow these gates so the fleet does not offer actions
/// that can only fail.
export function allowedActions(state: LifecycleState) {
  return {
    connect: state === "pending" || state === "failed" || state === "degraded",
    health: state === "healthy" || state === "degraded",
    disable: state !== "disabled",
    enable: state === "disabled",
  };
}

export function shortHash(hash: string) {
  return hash.slice(0, 12);
}

export function healthCheckTime(lastHealthCheckMs: number | null) {
  if (lastHealthCheckMs === null) return "Never checked";
  const date = new Date(lastHealthCheckMs);
  return Number.isNaN(date.valueOf()) ? "Check time unavailable" : date.toLocaleString();
}

export const effectLabels: Record<string, string> = {
  pure: "pure",
  read_only: "read-only",
  idempotent: "idempotent",
  compensatable: "compensatable",
  non_idempotent: "non-idempotent",
};

export function effectLabel(effect: string) {
  return effectLabels[effect] ?? effect;
}

export const providerKindLabels: Record<string, string> = {
  mcp_stdio: "mcp-stdio",
  http_search: "http-search",
  http_api: "http-api",
};

export function providerKindLabel(kind: string) {
  return providerKindLabels[kind] ?? kind;
}

/// The base url an http-api instance will actually call, with the
/// manifest's `{param}` placeholders substituted by the config values the
/// operator is typing. Unfilled params keep a `<name>` marker so the
/// preview reads as a template until every value is present. Returns null
/// for literal base urls — nothing to preview.
export function resolvedBaseUrlPreview(
  manifest: ConnectorManifest,
  values: Record<string, string>,
): string | null {
  if (manifest.provider.kind !== "http_api" || !manifest.provider.base_url.includes("{")) {
    return null;
  }
  const names = new Set(manifest.config_params.map((param) => param.name));
  return manifest.provider.base_url.replace(/\{([a-zA-Z][a-zA-Z0-9_]*)\}/g, (match, name: string) => {
    if (!names.has(name)) return match;
    const value = values[name]?.trim();
    return value ? value : `<${name}>`;
  });
}

export const connectionProviderLabels: Record<string, string> = {
  oauth2_authorization_code: "OAuth2 · authorization code",
  oauth2_client_credentials: "OAuth2 · client credentials",
  oauth2_password: "OAuth2 · password grant",
  api_key: "API key",
  basic: "Basic auth",
};

export function connectionProviderLabel(provider: string) {
  return connectionProviderLabels[provider] ?? provider;
}

/// The picker label for one vault connection: provider, account, and
/// status lead; the minted id trails, truncated — a connection is chosen
/// by what it is, not by its hex.
export function connectionLabel(record: VaultConnection) {
  const subject = record.subject ?? "service-level";
  return `${connectionProviderLabel(record.provider)} · ${subject} · ${record.status} · ${record.connection_id.slice(0, 17)}…`;
}

/// The credential-entry flow a manifest's auth declaration asks for.
///
/// `basic` is the http-api two-slot style (username + password legs);
/// `key` is any single-slot manifest, where one static secret fills the
/// slot (a bearer token, a header key, a query key — the wire style is the
/// manifest's business, the vault material is identical). `picker` covers
/// everything else: no tailored form exists, and the panel binds slots
/// from existing connections only.
export type CredentialFlow =
  | { kind: "basic"; usernameSlot: CredentialSlot; passwordSlot: CredentialSlot }
  | { kind: "key"; slot: CredentialSlot }
  | { kind: "picker" };

export function credentialFlow(manifest: ConnectorManifest): CredentialFlow {
  if (manifest.provider.kind === "http_api" && manifest.provider.auth?.style === "basic") {
    const { username_slot, password_slot } = manifest.provider.auth;
    const usernameSlot = manifest.credential_slots.find((slot) => slot.name === username_slot);
    const passwordSlot = manifest.credential_slots.find((slot) => slot.name === password_slot);
    if (usernameSlot && passwordSlot) return { kind: "basic", usernameSlot, passwordSlot };
  }
  if (manifest.credential_slots.length === 1) {
    return { kind: "key", slot: manifest.credential_slots[0] };
  }
  return { kind: "picker" };
}

/// Which existing connections a manifest's flow can consume. A `basic`
/// manifest needs basic-auth connections (each leg's value lives in the
/// connection's sealed access token); a single-slot manifest takes any
/// non-basic active connection; anything else accepts any active record.
/// Only `active` connections issue — `needs_reauth` and `revoked` fail at
/// the vault, so the picker never offers them.
export function usableConnections(
  manifest: ConnectorManifest,
  connections: VaultConnection[],
): VaultConnection[] {
  const flow = credentialFlow(manifest);
  return connections.filter(
    (connection) =>
      connection.status === "active" &&
      (flow.kind === "basic" ? connection.provider === "basic" : connection.provider !== "basic"),
  );
}
