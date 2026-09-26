import type { GenericAuth } from "../net/client";

/** How a service signs in, read from its own API description — so a public
 * API asks the builder for nothing and a keyed one asks for the right thing. */
export interface SignIn { auth: GenericAuth; name?: string }

type Scheme = { type?: string; scheme?: string; in?: string; name?: string };

export function signInOf(spec: unknown): SignIn {
  if (!spec || typeof spec !== "object") return { auth: "bearer" };
  const doc = spec as { components?: { securitySchemes?: Record<string, Scheme> }; securityDefinitions?: Record<string, Scheme>; security?: Record<string, unknown>[] };
  const schemes = doc.components?.securitySchemes ?? doc.securityDefinitions ?? {};
  // The scheme the document requires first, else the first it declares.
  const required = doc.security?.flatMap((s) => Object.keys(s)) ?? [];
  const pick = required.map((n) => schemes[n]).find(Boolean) ?? Object.values(schemes)[0];
  if (!pick) return { auth: "none" };
  const type = (pick.type ?? "").toLowerCase();
  if (type === "apikey") return pick.in === "query" ? { auth: "query", name: pick.name } : { auth: "header", name: pick.name };
  if (type === "http" || type === "basic") return (pick.scheme ?? type).toLowerCase() === "basic" ? { auth: "basic" } : { auth: "bearer" };
  return { auth: "bearer" };
}

export const SIGN_IN_WORDS: Record<GenericAuth, string> = {
  none: "No sign-in — it is public",
  bearer: "A token",
  basic: "A username and password",
  header: "A key sent with each call",
  query: "A key in the address",
};
