/**
 * Names and descriptions as a person reads them. Tools and skills are named
 * for the machine (`servicenow.list-records`, `it-issue-intake`) and
 * described for the model; the studio shows them to people.
 */

const ACRONYMS = new Set(["it", "hr", "us", "uk", "eu", "vpn", "mfa", "sso", "os", "pc", "api", "sla", "ost", "pst", "dns", "soql", "sql", "crm", "kb", "id", "url", "q1", "q2", "q3", "q4"]);
const BRANDS: Record<string, string> = {
  servicenow: "ServiceNow", github: "GitHub", docusign: "DocuSign", hubspot: "HubSpot", onedrive: "OneDrive", sharepoint: "SharePoint",
  salesforce: "Salesforce", workday: "Workday", zendesk: "Zendesk", okta: "Okta", jamf: "Jamf", slack: "Slack", jira: "Jira", stripe: "Stripe",
  microsoft: "Microsoft", google: "Google", intune: "Intune", zoom: "Zoom", sap: "SAP", odoo: "Odoo", saas: "SaaS",
};

const word = (w: string, capital: boolean) => {
  const lower = w.toLowerCase();
  if (ACRONYMS.has(lower)) return lower.toUpperCase();
  if (BRANDS[lower]) return BRANDS[lower];
  return capital ? lower.charAt(0).toUpperCase() + lower.slice(1) : lower;
};

/** `servicenow.list-records` → "ServiceNow: list records"; `it-issue-intake` → "IT issue intake"; `search_knowledge` → "Search knowledge". */
export function plainName(id: string): string {
  const [head, ...rest] = id.split(".");
  const split = (s: string) => s.split(/[-_\s]+/).filter(Boolean);
  if (rest.length === 0) return split(head).map((w, n) => word(w, n === 0)).join(" ");
  const system = split(head).map((w) => word(w, true)).join(" ");
  const action = split(rest.join(" ")).map((w) => word(w, false)).join(" ");
  return `${system}: ${action}`;
}

/** The first sentence of a description, for a line a person scans; the whole of it stays available on hover. */
export function firstSentence(text: string, max = 150): string {
  const t = text.trim();
  // A sentence ends at a stop followed by the end or a space and a capital: "e.g. incident" does not end one.
  const end = t.search(/[.!?](?=$|\s+[^a-z\s])/);
  const one = end >= 0 ? t.slice(0, end + 1) : t;
  return one.length > max ? `${one.slice(0, max - 1).trimEnd()}…` : one;
}

/** `accounts/fireworks/models/deepseek-v4p1-flash` → "deepseek-v4p1-flash": the model's own name, not its path. */
export function shortModel(model: string): string {
  return model.split("/").filter(Boolean).pop() ?? model;
}

/** The one argument that says what a call was about: a page, a query, a key, a table. */
export function argSummary(args: string): string {
  let a: Record<string, unknown> = {};
  try { a = JSON.parse(args || "{}"); } catch { return ""; }
  const pick = ["url", "query", "q", "question", "contains", "key", "keys", "text", "table", "name", "label", "title", "subject", "statement"].map((k) => a[k]).find((v) => v != null && v !== "") ?? Object.values(a).find((v) => typeof v === "string" && v);
  if (pick == null) return "";
  let t = Array.isArray(pick) ? pick.join(", ") : typeof pick === "string" ? pick : JSON.stringify(pick);
  t = t.replace(/^https?:\/\//, "");
  return t.length > 60 ? `${t.slice(0, 59)}…` : t;
}

/** A call's result in one line a person reads: its own note, how many came back, or that it was done. */
export function resultSummary(res: string): string {
  if (!res) return "no result";
  if (/^(ERROR|DENIED|REPEATED)/.test(res)) return res.replace(/\s+/g, " ").slice(0, 110);
  try {
    const v = JSON.parse(res);
    if (Array.isArray(v)) return `${v.length} result${v.length === 1 ? "" : "s"}`;
    if (v && typeof v === "object") {
      const o = v as Record<string, unknown>;
      if (typeof o.note === "string") return o.note.length > 110 ? `${o.note.slice(0, 109)}…` : o.note;
      const list = Object.values(o).find(Array.isArray) as unknown[] | undefined;
      if (o.found === false || o.fetched === false || o.ok === false) return "nothing found";
      if (list) return `${list.length} result${list.length === 1 ? "" : "s"}`;
      // A count anywhere in the answer (an aggregate nests it under stats).
      const count = (x: unknown, depth = 0): number | null => { if (!x || typeof x !== "object" || depth > 3) return null; const r = x as Record<string, unknown>; const c = r.count; if (c != null && Number.isFinite(Number(c))) return Number(c); for (const y of Object.values(r)) { const n = count(y, depth + 1); if (n != null) return n; } return null; };
      const n = count(o);
      if (n != null) return `${n} found`;
      return "done";
    }
  } catch { /* plain text */ }
  const t = res.replace(/\s+/g, " ");
  return t.length > 110 ? `${t.slice(0, 109)}…` : t;
}

