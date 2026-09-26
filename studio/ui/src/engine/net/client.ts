// The server client. Rusty's studio is the front end of the Rusty server —
// agents, runs, skills and connections live there, not in this browser. Every
// call goes through here so there is one place that knows the base URL, one
// shape of error, and one answer to "is the server actually there".
//
// Nothing in this module invents a value. When the server cannot be reached
// the caller is told so and the shell says so; it never falls back to a
// plausible-looking local substitute.

const BASE = (import.meta.env?.VITE_RUSTY_API as string | undefined)?.replace(/\/$/, "") ?? "http://127.0.0.1:8100";

export class ServerError extends Error {
  /** The server's structured refusal — `error`, and the fields it names
   * (a host, a connection) — so a screen can act on it, not parse prose. */
  constructor(readonly status: number, readonly path: string, message: string, readonly body: Record<string, unknown> = {}) {
    super(message);
    this.name = "ServerError";
  }
}

/** The refusal as an error: its sentence, and its body when it is JSON. */
async function refused(res: Response, path: string): Promise<ServerError> {
  const text = await res.text().catch(() => "");
  let body: Record<string, unknown> = {};
  try {
    const parsed = JSON.parse(text) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) body = parsed as Record<string, unknown>;
  } catch { /* plain text */ }
  return new ServerError(res.status, path, sentenceOf(text, body, res), body);
}

function sentenceOf(text: string, body: Record<string, unknown>, res: Response): string {
  if (!text) return `${res.status} ${res.statusText}`;
  for (const key of ["message", "detail", "error", "reason"]) {
    const value = body[key];
    if (typeof value === "string" && value) return value;
  }
  return text.slice(0, 400);
}

/** What the server said, not what the status code is called. A refusal
 * carries its reason — a schema path, a field, the system's own words — and
 * dropping it for "422 Unprocessable Entity" leaves the builder guessing. */
async function refusal(res: Response): Promise<string> {
  const body = await res.text().catch(() => "");
  if (!body) return `${res.status} ${res.statusText}`;
  try {
    const parsed = JSON.parse(body) as Record<string, unknown>;
    for (const key of ["message", "detail", "error", "reason"]) {
      const value = parsed[key];
      if (typeof value === "string" && value) return value;
    }
  } catch {
    // Not JSON — axum's extractor rejections are plain text, and they are
    // the most useful sentence the server has.
  }
  return body.slice(0, 400);
}

/** Who to tell when the server says nobody is signed in: the shell, which
 * sends the person to sign in again. Registered by the server store so the
 * client stays free of it. */
let signedOut: (() => void) | null = null;
export function onSignedOut(handler: () => void) { signedOut = handler; }

/** The paths where a 401 is the server's own answer to the person's words
 * (a wrong password), not a session that ended. */
const SIGN_IN_PATHS = ["/auth/login", "/me"];

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    // A person's session is a cookie the server set; it travels with
    // credentials. Programs use keys; this client is for people.
    res = await fetch(`${BASE}${path}`, {
      ...init,
      credentials: "include",
      headers: { "content-type": "application/json", ...(init?.headers ?? {}) },
    });
  } catch (err) {
    // The browser's own words ("Failed to fetch") name nothing a person
    // can act on; the studio says what happened.
    void err;
    throw new ServerError(0, path, "the server did not answer");
  }
  if (res.status === 401 && !SIGN_IN_PATHS.includes(path)) {
    // The session ended (the server restarted, or it expired). Not a
    // failure of whatever the person was doing: say so, and let the shell
    // take them to sign in — never let it read as "the system refused you".
    signedOut?.();
    throw new ServerError(401, path, "your session ended — sign in again");
  }
  if (!res.ok) throw await refused(res, path);
  return (res.status === 204 ? undefined : await res.json()) as T;
}

// ── Health ──────────────────────────────────────────────────────────────────

export interface HealthComponent { component: string; status: string }
export interface Health { status: string; components: HealthComponent[] }

export const health = () => request<Health>("/health");

// ── Who I am ────────────────────────────────────────────────────────────────

export type ServerRole = "admin" | "builder" | "operator" | "auditor";

/** Who made something, as the server stamps it. */
export interface Attribution { principal_id: string; name: string; kind: "user" | "service" }

export interface Me {
  principal: { id: string; name: string; kind: "user" | "service"; roles: ServerRole[] };
  tenant: string;
  roles: ServerRole[];
  scopes: string[];
}

/** The caller, as the server resolved them — the developer principal in open
 * mode, or the person whose session this browser holds. A 401 means nobody
 * is signed in. */
export const me = () => request<Me>("/me");

/** Sign in. The server answers with who you are and sets the session cookie. */
export const login = (username: string, password: string) =>
  request<Me>("/auth/login", { method: "POST", body: JSON.stringify({ username, password }) });

/** Sign out: the session ends on the server and the cookie is cleared. */
export const logout = () => request<void>("/auth/logout", { method: "POST" });

/** Change your own password, proving the current one. */
export const changePassword = (current: string, next: string) =>
  request<void>("/auth/password", { method: "POST", body: JSON.stringify({ current, new: next }) });

export interface UserSummary { id: string; name: string; roles: ServerRole[]; created_at: string; /** `false` once the directory deactivated the person. */ active?: boolean; /** The identity provider the account signs in through, when it does. */ external?: { issuer: string; subject: string } | null; /** The directory's own id, when it provisioned the account. */ external_id?: string | null; updated_at?: string | null }
export const listUsers = () => request<{ users: UserSummary[] }>("/users").then((r) => r.users);
export const createUser = (input: { username: string; name?: string; roles: ServerRole[]; password: string }) =>
  request<UserSummary>("/users", { method: "POST", body: JSON.stringify(input) });
export const deleteUser = (id: string) => request<void>(`/users/${id}`, { method: "DELETE" });
/** End every session of a user, everywhere: the next request on any of them is refused; a fresh sign-in works. */
export const revokeUserSessions = (id: string) => request<void>(`/users/${id}/sessions/revoke`, { method: "POST", body: "{}" });

// ── OIDC sign-in: the organisation's identity provider ──────────────────────
/** What the sign-in page may show without anyone signed in. */
export type OidcPublic = { configured: false } | { configured: true; name: string; issuer: string; start: string };
export const oidcPublic = () => request<OidcPublic>("/auth/oidc");
/** Where the browser goes to sign in through the provider: the server starts the flow. */
export const oidcStartUrl = (returnTo?: string) => `${apiBase()}/auth/oidc/start${returnTo ? `?return_to=${encodeURIComponent(returnTo)}` : ""}`;
export interface OidcProvider {
  name: string; issuer: string; client_id: string; has_secret: boolean; default_role: ServerRole; scopes: string;
  discovery: { authorization_endpoint: string; token_endpoint: string; userinfo_endpoint?: string | null; end_session_endpoint?: string | null; read_at: string };
  updated_by: Attribution; updated_at: string;
}
export const oidcProvider = () => request<{ provider: OidcProvider | null; callback_url: string }>("/auth/oidc/provider");
/** The secret is sent once; a later save without it keeps the sealed one. */
export const saveOidcProvider = (body: { name: string; issuer: string; client_id: string; client_secret?: string; default_role?: ServerRole; scopes?: string }) =>
  request<{ provider: OidcProvider; callback_url: string }>("/auth/oidc/provider", { method: "PUT", body: JSON.stringify(body) });
export const removeOidcProvider = () => request<{ removed: boolean }>("/auth/oidc/provider", { method: "DELETE" });

// ── SCIM provisioning: the directory creates, changes, deactivates people ──
export interface ScimGroupRole { group: string; role: ServerRole }
export interface ScimConfig {
  base_url: string; has_token: boolean; token_minted_at?: string | null; token_minted_by?: Attribution | null;
  group_roles: ScimGroupRole[]; last_seen_at?: string | null; provisioned: number; deactivated: number; deleted: number;
  groups?: { id: string; display_name: string; members: number }[];
}
export const scimConfig = () => request<ScimConfig>("/auth/scim");
/** Shown once; the server keeps only its hash. A new one replaces the old. */
export const mintScimToken = () => request<{ token: string; base_url: string; config: ScimConfig }>("/auth/scim/token", { method: "POST", body: "{}" });
export const revokeScimToken = () => request<{ revoked: boolean; config: ScimConfig }>("/auth/scim/token", { method: "DELETE" });
export const saveScimGroupRoles = (group_roles: ScimGroupRole[]) => request<ScimConfig>("/auth/scim/group-roles", { method: "PUT", body: JSON.stringify({ group_roles }) });
/** What remains of a forgotten person: the id, when, by whom, why. */
export interface Tombstone { tenant: string; principal: string; name?: string | null; at: string; by: Attribution; reason: string }
export interface Forgotten { forgotten: Tombstone; had_account: boolean; memory: { forgotten: number; invalidated: number }; /** Agent blocks that named them, with the lines removed from each. */ blocks_scrubbed?: { agent_id: string; label: string; lines_removed: number }[]; connections_revoked: number; threads_removed: number; runs_removed: number; assignments_removed: number; key_destroyed: boolean }
/** Forget a person for good: their memory, conversations, runs and grants
 * go, and their key is destroyed — every sealed copy of their records, in
 * any backup, is ciphertext from then on. Irreversible; the reason is kept. */
export const forgetUser = (id: string, reason: string) => request<Forgotten>(`/users/${encodeURIComponent(id)}/forget`, { method: "POST", body: JSON.stringify({ reason }) });

// ── Assistants (what the studio calls agents) ───────────────────────────────

/** One `{{name}}` in the charter: a fixed setting, or a field of the
 * trigger's event (`event.ticket.org`). The test value serves the Test panel. */
export interface AgentVariable { name: string; description?: string; source: "setting" | "trigger"; value?: string; path?: string; test_value?: string }

export interface AssistantIntent {
  /** The agent's charter — its standing instructions, read by the model
   * before anything else in every conversation. */
  instructions?: string;
  format?: string;
  model?: string;
  /** Its own fallback provider, by id — behind `model` (or the deployment's
   * primary) for this agent's runs; absent, the deployment's fallback. */
  fallback_model?: string;
  /** Tokens the work this agent starts — delegated rounds and queued tasks together — may spend before nothing more starts; the platform's default is 400,000. */
  chain_max_tokens?: number;
  /** The agent's sampling temperature (0–2); absent, the provider's default. */
  temperature?: number;
  approval?: string;
  /** The tools it may call, each with the builder's word on when it is for.
   * The note reaches the model on the tool itself. */
  tools?: { name: string; when?: string }[];
  memory?: { access?: string; scopes?: string[]; /** Blocks the builder declares beyond the platform's person, working and decisions, or resizes. */ blocks?: { label: string; description?: string; char_limit?: number }[] };
  binding?: { environment?: string; surfaces?: string[] };
  /** What one run may spend. The executor stops the run at the step that
   * crosses a bound. A cost bound needs a priced model. */
  budget?: { max_tokens?: number | string; max_cost_usd?: number | string; max_latency_ms?: string };
  /** The task-queue pool this agent works: the server claims its tasks one at
   * a time and runs the agent with each task's payload as the message. */
  pool?: string;
  output?: { mode?: string; schema?: string };
  /** Skills it follows, by name — the run's skills section. */
  skills?: string[];
  /** The `{{name}}` placeholders the charter and skills use, and where each
   * value comes from on every run. */
  variables?: AgentVariable[];
  /** The context it works with, where it differs from the deployment: its
   * window in estimated tokens, and how many recent messages compaction
   * keeps verbatim. */
  context?: { budget_tokens?: number | string; keep_recent_messages?: number | string };
  /** What happens to a proposal the candidate gate passes: `person` (default —
   * it waits for a person) or `auto` (it activates by itself while the
   * verifier's evidence stands under auto-promotion). */
  promotion?: "person" | "auto";
  /** The bar a version clears at the gate: each suite's pass rate in percent (50–100, default 100), and the most judging one suite may spend, in USD (default 1). */
  gate?: { pass_rate?: number; budget_usd?: number };
}

export interface Assistant {
  assistant_id: string;
  name: string;
  graph: string;
  version_count: number;
  active_version_id: string | null;
  created_at: string;
  /** Set once the agent is put away: it stays runnable by id and in every record, out of the rail. */
  archived_at?: string | null;
  metadata?: { description?: string; goals?: string; audience?: string };
  config?: { studio_intent?: AssistantIntent; recursion_limit?: number };
}

export const listAssistants = () => request<Assistant[]>("/assistants");
/** Put an agent away (or bring it back): a compare-and-set on the version it serves. */
export const setAssistantArchived = (assistantId: string, expectedActiveVersionId: string, archived: boolean) =>
  request<{ assistant: Assistant; changed: boolean; lifecycle: "archived" | "active" }>(`/assistants/${assistantId}/${archived ? "archive" : "restore"}`, { method: "POST", body: JSON.stringify({ expected_active_version_id: expectedActiveVersionId }) });

/** Create an agent on the server. It exists from the moment this returns —
 * versioned, runnable, journalled. */
export const createAssistant = (input: {
  name: string;
  graph: string;
  config?: { studio_intent?: AssistantIntent; recursion_limit?: number };
  metadata?: { description?: string; goals?: string; audience?: string };
}) => request<Assistant>("/assistants", { method: "POST", body: JSON.stringify(input) });

/** One stored version of an agent, as the server returns it on create. */
export interface AssistantVersion {
  version_id: string;
  parent_version_id: string | null;
  name: string;
  graph: string;
  config?: Assistant["config"];
  metadata?: Assistant["metadata"];
  created_at: string;
  active: boolean;
  /** On the listing: who proposed the version when an agent or the platform did (the Coach, a gap, the review). */
  proposed_by?: string | null;
  /** A person said no to this version: who, why, when. Activating it clears the mark. */
  declined?: VersionDecline | null;
}
export interface VersionDecline { version_id: string; by: { principal_id?: string; name?: string; kind?: string }; reason: string; at: string }

/** A change to an agent is a new version on top of the one the caller
 * loaded — created inactive. Activating it is the separate step below. */
export const createAssistantVersion = (
  assistantId: string,
  input: {
    base_version_id: string;
    name: string;
    graph: string;
    config?: Assistant["config"];
    metadata?: Assistant["metadata"];
  },
) =>
  request<{ assistant_id: string; created: boolean; active_version_id: string; version: AssistantVersion }>(
    `/assistants/${assistantId}/versions`,
    { method: "POST", body: JSON.stringify(input) },
  );

/** Make a version the one that runs — a compare-and-set against the version
 * the caller believed was active, so two builders cannot silently overwrite
 * each other. A stale expectation is refused in the server's own words. */
/** Every version of an agent, with which one is active. */
export const assistantVersions = (assistantId: string) =>
  request<{ assistant_id: string; active_version_id: string; versions: AssistantVersion[] }>(`/assistants/${assistantId}/versions`);
/** One version in full — its config and metadata, which the listing omits. */
export const assistantVersion = (assistantId: string, versionId: string) =>
  request<{ assistant_id: string; active_version_id: string; version: AssistantVersion }>(`/assistants/${assistantId}/versions/${versionId}`).then((r) => r.version);
/** Say no to a version filed for you — it stays in the lineage, marked with
 * the reason; the reason is what the next proposal is written against. */
export const declineAssistantVersion = (assistantId: string, versionId: string, reason: string) =>
  request<{ assistant_id: string; declined: boolean; active_version_id: string; version: AssistantVersion }>(`/assistants/${assistantId}/versions/${versionId}/decline`, { method: "POST", body: JSON.stringify({ reason }) });
/** A person's word on a run's verdict, and the verifier's own suite built from those words. */
export interface VerdictReview { run_id: string; verdict_then: string; verdict_right: string; agree: boolean; note?: string | null; by: Attribution; at: string }
export interface VerifierCase { run_id: string; asked: string; person: string; then: string; now: string; now_reason: string; agrees: boolean; skipped?: string | null }
export interface VerifierEvaluation { evaluation_id: string; judge: string; started_at: string; finished_at?: string | null; status: "running" | "done" | "error"; agreed: number; /** Cases the judge answered — the ratio's denominator; the rest were skipped. */ judged: number; total: number; cases: VerifierCase[] }
export interface VerifierFloor { stands: boolean; reviews: number; needs_reviews: number; agreement?: number | null; needs_agreement: number; why: string }
export interface VerifierEvidence { reviews: number; disagreed: number; judge?: string | null; floor: VerifierFloor; latest?: VerifierEvaluation | null; recent: VerdictReview[] }
export const reviewVerdict = (runId: string, agree: boolean, verdict?: "verified" | "failed", note?: string) =>
  request<{ review: VerdictReview; reviews: number }>(`/runs/${runId}/verdict/review`, { method: "POST", body: JSON.stringify({ agree, ...(verdict ? { verdict } : {}), ...(note ? { note } : {}) }) });
export const verifierEvidence = () => request<VerifierEvidence>("/verifier/evidence");
/** Judge the verifier: every reviewed run's transcript past the judge again; the answer is where it agrees with the person. */
export const judgeVerifier = () => request<VerifierEvaluation>("/verifier/evaluations", { method: "POST" });

/** One suite's verdict on a version, as the promotion gate sees it. */
export interface SuiteEvidence { name: string; version: string; cases: number; state: "passed" | "failed" | "missing" | "running" | "stale" | "over budget"; evaluation_id?: string | null; passed: number; total: number; evaluated_at?: string | null; baseline?: { version_id: string; passed: number; total: number; status: string } | null; /** Why a finished evaluation did not clear the bar. */ below?: string | null; /** Why a passing evaluation no longer counts: what changed since it ran. */ stale_because?: string[]; /** Why it failed, per case: the judge's words or the assertion that did not hold. */ failures?: { case_id: string; said: string }[] }
export interface VersionEvidence { version_id: string; suites: SuiteEvidence[]; ok: boolean; unevaluated: boolean; /** The share of each suite the agent's bar asks for (1 = every case). */ pass_rate_min?: number }
export interface Promotion { version_id: string; by: Attribution; at: string; evidence: VersionEvidence; override_reason?: string | null }
export const versionEvidence = (assistantId: string, versionId: string) =>
  request<{ evidence: VersionEvidence; promotions: Promotion[] }>(`/assistants/${assistantId}/versions/${versionId}/evidence`);
/** Judge a version by hand: every suite bound to the agent runs against it
 * under the candidate budget. The Coach's proposals are judged on filing;
 * this is for evidence gone stale, or a version nothing judged. */
export const judgeVersion = (assistantId: string, versionId: string) =>
  request<{ started: unknown[]; evidence: VersionEvidence }>(`/assistants/${assistantId}/versions/${versionId}/evidence`, { method: "POST" });
export const activateAssistantVersion = (assistantId: string, versionId: string, expectedActiveVersionId: string, overrideReason?: string) =>
  request<{ activated: boolean; assistant: Assistant; evidence?: VersionEvidence }>(
    `/assistants/${assistantId}/versions/${versionId}/activate`,
    { method: "POST", body: JSON.stringify({ expected_active_version_id: expectedActiveVersionId, ...(overrideReason ? { override_reason: overrideReason } : {}) }) },
  );

// ── What the server offers ──────────────────────────────────────────────────

export interface ServerTool {
  name: string;
  description: string;
  parameters_schema?: Record<string, unknown>;
  /** The effect class every gate and receipt reads. */
  effect: string;
}

export interface ServerGraph { name: string; tools: ServerTool[]; [k: string]: unknown }

/** The graphs this server hosts, each with the tools it offers right now —
 * built-ins and every operation a configured connection derives. */
export interface LlmBrief { id: string; name: string; model: string }
export const serverInfo = () => request<{ sweep_at?: string | null; llm?: { primary?: LlmBrief | null; fallback?: LlmBrief | null; active?: string | null }; /** The deployment's context policy in the numbers an agent may override; null when calls go raw. */ context?: { budget_tokens: number; keep_recent_messages?: number | null; memory: boolean } | null; graphs: ServerGraph[]; [k: string]: unknown }>("/info");

// ── Config: the models behind the platform ──────────────────────────────────
//
// Providers are stored documents with sealed keys; one is primary, one may
// be the fallback; a change is applied live (the graphs hold a swappable
// handle), no restart.

export interface LlmProvider {
  id: string;
  name: string;
  base_url: string;
  model: string;
  /** A key is held or not; it is never rendered. */
  has_key: boolean;
  extra_body?: Record<string, unknown> | null;
  price_input_per_m?: number | null;
  price_output_per_m?: number | null;
  /** USD per million cache-served prompt tokens; without it cached tokens bill at the input rate. */
  price_cached_input_per_m?: number | null;
  created_at: string;
  role: "primary" | "fallback" | "";
}
export interface LlmProviders { providers: LlmProvider[]; primary: string | null; fallback: string | null; updated_at: string | null; active: string | null; applied?: string; applied_error?: string; /** Cache hits per model over the newest runs (`GET /llm/providers` only). */ cache?: CacheStats }
export interface LlmProviderInput { id?: string; name: string; base_url: string; model: string; api_key?: string; extra_body?: Record<string, unknown> | null; price_input_per_m?: number | null; price_output_per_m?: number | null; price_cached_input_per_m?: number | null }
/** The providers; `withCache` adds cache hits per model over the newest journalled runs, which costs a scan. */
/** The goal's number as the server measures it on the agent's live runs of the last seven days — the same figure the Coach's brief reads. */
export interface GoalMeasure { metric: string; current: number | null; sample: number; trend: number[]; unit: string; window: string; runs_read: number }
export const goalMeasure = (assistantId: string, metric: string) => request<GoalMeasure>(`/assistants/${encodeURIComponent(assistantId)}/goal?metric=${encodeURIComponent(metric)}`);
export const llmProviders = (withCache = false) => request<LlmProviders>(`/llm/providers${withCache ? "?cache=1" : ""}`);
export const saveLlmProviders = (body: { providers: LlmProviderInput[]; primary: string | null; fallback: string | null }) =>
  request<LlmProviders>("/llm/providers", { method: "PUT", body: JSON.stringify(body) });
// ── Assignments: an outcome delegated to an agent, worked in rounds ─────────

export interface AssignmentAction { tool: string; effect?: string; status?: string; connection?: { instance_id: string; name: string }; arguments?: string | null; result?: string | null; /** The failure's class when the call failed: invalid_arguments, denied, not_found, transient, unknown_outcome, dependency, bounded, reconcile_first… */ failure?: string | null }

/** A tool failure the loop can act on, as a journal event's error carries it. */
export interface ToolFailure { kind: "tool_failure"; class: string; tool: string; detail: string; sent: boolean; retry_safe: boolean; next: string }
/** The failure inside a journal event's inline output, when it is one. */
export function toolFailureOf(output: unknown): ToolFailure | null {
  const value = (output as { value?: { error?: unknown } } | null)?.value?.error;
  if (typeof value !== "string") return null;
  const text = value.replace(/^\s*ERROR:\s*/, "").replace(/^tool error:\s*/, "");
  try { const parsed = JSON.parse(text) as ToolFailure; return parsed && parsed.kind === "tool_failure" ? parsed : null; } catch { return null; }
}
export interface AssignmentRound {
  round: number; run_id: string; thread_id: string; started_at: string; ended_at?: string | null;
  /** running | success | error | interrupted | cancelled | restart */
  status: string; resumed_run_id?: string | null;
  /** The closing turn: the round ended with an answer and no record, so the server asked once, on the same thread. */
  closing_run_id?: string | null; actions: AssignmentAction[]; summary?: string | null;
  spend?: { cost_usd?: number | null; tokens?: number | null; total_tokens?: number | null } | null; steer?: string | null; error?: string | null;
  /** For a round still marked running: the run's live status, or null when the server no longer has it. */
  live?: string | null;
}
export interface AssignmentProgress { done: string[]; unresolved: string[]; next_step?: string | null; waiting_for?: string | null; complete: boolean; recorded_at?: string | null; by_run?: string | null }
export type AssignmentState = "working" | "waiting" | "blocked" | "paused" | "done" | "cancelled";
export interface Assignment {
  assignment_id: string; tenant: string;
  /** Who delegated: a person, or an agent from one of its runs (`kind: "agent"`, with the run). */
  owner: Attribution | { principal_id: string; name: string; kind: "agent"; agent_id?: string; run_id?: string };
  assistant_id: string; assistant_name: string;
  /** Where it sits in a chain of agent-started work, when an agent delegated it. */
  chain?: { depth: number; run_id?: string; agent_id?: string } | null;
  /** The person's words, verbatim. */
  request: string; success?: string | null; constraints?: string | null; deadline?: string | null;
  max_rounds: number; rounds_used: number; state: AssignmentState; state_reason?: string | null;
  /** The most tokens one round may spend before it ends with its record. */
  max_tokens_per_round?: number;
  /** The world its rounds run in, when one was named at delegation. */
  world?: string | null; world_name?: string | null;
  /** Words a person gave it — `campaign:F5` makes it a campaign variant. */
  tags?: string[];
  /** Tools the agent must not call in this work; the platform refuses each attempt. */
  forbidden_tools?: string[];
  /** Delivery, apart from the record: each time the owner was told, and whether they saw it. */
  delivery?: { notice_id: string; state: string; at: string; seen_at?: string | null; channel: string }[];
  /** `seen`, `delivered` or `none` — the last notice's word. */
  told?: "seen" | "delivered" | "none";
  /** When an agent started this: what the chain of work has spent against the delegating agent's cap. */
  chain_spend?: { spent_tokens: number; max_tokens: number; runs: number; over: boolean };
  progress: AssignmentProgress; rounds: AssignmentRound[]; steers: { at: string; by: Attribution; text: string }[];
  next_wake?: { kind: string; run_id?: string } | null; spend_total: { cost_usd: number; total_tokens: number };
  goal?: { phase: string; revision: number }; created_at: string; updated_at: string;
}
export const listAssignments = () => request<{ assignments: Assignment[] }>("/assignments").then((r) => r.assignments);
export const getAssignment = (id: string) => request<Assignment>(`/assignments/${encodeURIComponent(id)}`);
export const createAssignment = (body: { assistant_id: string; request: string; success?: string; constraints?: string; max_rounds?: number; max_tokens_per_round?: number; deadline?: string; /** A world (by name) every round runs in. */ world?: string; /** Several, one per system; `world` is the first. */ worlds?: string[]; tags?: string[]; /** Tools the agent must not call in this work — refused by the platform every round. */ forbidden_tools?: string[] }) =>
  request<Assignment>("/assignments", { method: "POST", body: JSON.stringify(body) });
export const continueAssignment = (id: string, steer?: string) =>
  request<Assignment>(`/assignments/${encodeURIComponent(id)}/continue`, { method: "POST", body: JSON.stringify({ steer: steer ?? null }) });
export const pauseAssignment = (id: string) => request<Assignment>(`/assignments/${encodeURIComponent(id)}/pause`, { method: "POST", body: "{}" });
export const cancelAssignment = (id: string) => request<Assignment>(`/assignments/${encodeURIComponent(id)}/cancel`, { method: "POST", body: "{}" });
export const finishAssignment = (id: string) => request<Assignment>(`/assignments/${encodeURIComponent(id)}/done`, { method: "POST", body: "{}" });

// ── Egress ceiling ──────────────────────────────────────────────────────────

export interface CeilingHost { host: string; added_by: Record<string, unknown> | null; added_at: string; note?: string | null; used_by: { instance_id: string; name: string }[] }
// ── Worlds: stand-ins for connected systems that suites reset ──────────────
// A world is made from a connection: it answers what a run addresses to that
// system's host, in the system's own dialect, from records a suite seeds it
// with, and goes back to that seed on reset. A case tagged `world:<name>`
// resets the world, then runs in it.
export interface WorldDialect { id: string; name: string; fits: string[]; summary: string; seed_shape: string; starter: unknown }
export interface World {
  world_id: string; name: string; connector: string; instance_id?: string | null;
  /** The host whose calls the world answers. */
  stands_for: string; dialect: string; created_by: Attribution; created_at: string;
  reset_count: number; last_reset_at?: string | null; calls_since_reset: number;
  /** Answers the world still has to lose on purpose, now and in the seed. */
  faults_left?: number; seed_faults?: number;
  /** Rows per table now, and in the seed. */
  records: Record<string, number>; seed_records: Record<string, number>;
  /** The newest rows per table; only on one world's page. */
  tables?: Record<string, Record<string, unknown>[]>;
}
export const worldDialects = () => request<{ dialects: WorldDialect[] }>("/worlds/dialects").then((r) => r.dialects);
export const listWorlds = () => request<{ worlds: World[] }>("/worlds").then((r) => r.worlds);
export const getWorld = (id: string) => request<World>(`/worlds/${encodeURIComponent(id)}`);
export const createWorld = (body: { name: string; instance_id?: string; connector?: string; stands_for?: string; dialect?: string; seed?: unknown }) =>
  request<World>("/worlds", { method: "POST", body: JSON.stringify(body) });
/** The seed a new world of a system would start from — the dialect's own
 * starter, or one made from the connector's operations. */
export const worldStarter = (q: { connector?: string; instance_id?: string }) =>
  request<{ connector: string; dialect: string; starter: unknown }>(`/worlds/starter?${new URLSearchParams(q as Record<string, string>).toString()}`);
export const resetWorld = (id: string) => request<World>(`/worlds/${encodeURIComponent(id)}/reset`, { method: "POST", body: "{}" });
export const deleteWorld = (id: string) => request<{ deleted: boolean; name: string }>(`/worlds/${encodeURIComponent(id)}`, { method: "DELETE" });

// ── The campaign: eight task families, measured on the same counts ─────────

export interface CampaignCounts { unauthorized_attempts: number; unauthorized_refused: number; duplicate_effects: number; interventions: number; tool_calls: number }
export interface CampaignRepetition extends CampaignCounts {
  evaluation_id: string; started_at: string; assistant_id: string; run_id: string | null;
  verified: boolean; judge: string | null; violations: number; latency_ms: number; tokens: number; cost_usd: number;
  /** The skill revisions the run depended on — what a learning family compares across. */
  skills?: Record<string, number>;
}
export interface CampaignVariant { dataset: string; version: string; case_id: string; tags: string[]; /** `transfer` or `control` in a learning family. */ role?: "transfer" | "control" | null; repetitions: CampaignRepetition[] }
export interface CampaignRevisionRow { revision: number; transfer_verified: number; transfer_repetitions: number; control_verified: number; control_repetitions: number }
/** What a learning family's repetitions show across a skill's revisions, and the comparison in words. */
export interface CampaignLearning { skill: string; revisions: CampaignRevisionRow[]; said: string }
export interface CampaignTotals { variants: number; repetitions: number; verified: number; violations: number; unauthorized_attempts: number; unauthorized_refused: number; duplicate_effects: number; interventions: number; latency_ms: number; tokens: number; cost_usd: number }
export interface CampaignFamily { id: string; name: string; needs: string; variants: CampaignVariant[]; totals: CampaignTotals; learning?: CampaignLearning | null }
export interface Campaign { conditions: { server_version: string; llm: unknown; recorded_at: string }; families: CampaignFamily[] }
/** The eight families with their variants (cases tagged `campaign:Fn`), repetitions and totals. */
export const campaign = () => request<Campaign>("/campaign");
/** The same, as the Markdown the campaign note records per run. */
export const campaignMarkdown = () =>
  fetch(`${BASE}/campaign?format=markdown`, { credentials: "include" }).then(async (r) => { if (!r.ok) throw new ServerError(r.status, "/campaign", await refusal(r)); return r.text(); });

// ── The estate: the store as one archive, restored onto an empty store ──────

export interface EstateCounts { agents: number; skill_revisions: number; threads: number; runs: number; people: number; connections: number; memories: number; approvals: number; people_forgotten?: number }
export interface EstateManifest { taken_at: string; by: { principal_id?: string; name?: string; kind?: string }; server_version: string; store_path: string; counts: EstateCounts; files: number; bytes: number; person_keys?: number }
export interface EstateBackup { name: string; path: string; bytes: number; manifest?: EstateManifest | null; /** The sibling archive carrying the person keys, kept apart. */ person_keys?: string | null }
export type RestoreOutcome =
  | { outcome: "restored"; archive: string; manifest?: EstateManifest | null; files: number }
  | { outcome: "skipped"; archive: string; reason: string }
  | { outcome: "failed"; archive: string; reason: string };
export interface Estate {
  store: { kind: string; path: string; bytes: number };
  counts: EstateCounts;
  backups_dir: string;
  backups: EstateBackup[];
  /** Present when this store was filled from a backup at boot. */
  restored_from: { archive: string; at: string; manifest?: EstateManifest | null; files?: number } | null;
  /** This boot's restore request, if there was one. */
  restore: RestoreOutcome | null;
  server_version: string;
  /** Everyone forgotten: the tombstones. */
  forgotten?: Tombstone[];
}
export const estate = () => request<Estate>("/estate");

// ── Capacity: what this deployment carries, measured ───────────────────────
export interface Capacity {
  now: { runs_running: number; runs_queued: number; assignments: Record<string, number>; journals_held: number };
  /** Execution spans from the journals; the wait before a run starts is not in them. */
  window: { minutes: number; since: string; runs_finished: number; runs_per_minute: number; median_execution_ms: number | null; slow_half_execution_ms: number | null; slowest_execution_ms: number | null; tokens: number; model_calls: number; failures: number; note: string };
  store: { kind: string; path: string; runs: number; threads: number; agents: number };
  bounds: { runs_per_thread: number; sweep_at: string | null };
  restore: { archive?: string; at?: string } | null;
  /** The sentence the numbers support, and nothing beyond it. */
  bites_first: string;
}
export const capacity = (minutes?: number) => request<Capacity>(`/capacity${minutes ? `?minutes=${minutes}` : ""}`);
export interface ProbeResult {
  agent: string; copies: number; wall_ms: number; succeeded: number; failed: number; runs_per_minute: number;
  median_ms: number | null; slow_half_ms: number | null; slowest_ms: number | null;
  model_calls: number; tokens: number;
  /** Whether this measured agent work or only the run plumbing. */
  measured: string; note: string;
  copies_detail: { run_id: string | null; status: string; took_ms: number; error?: string }[];
}
/** Runs the agent `copies` times at once. Every copy is a real run and spends real tokens. */
export const capacityProbe = (assistant_id: string, copies: number, message?: string) =>
  request<ProbeResult>("/capacity/probe", { method: "POST", body: JSON.stringify({ assistant_id, copies, message }) });
/** The estate over days: what each day wrote to the journals and to memory, the runs it finished, and how late each schedule fired against its interval. */
export interface EstateRoster { days: number; since: string; by_day: { day: string; journal_files: number; journal_bytes: number; notes: number; runs: number }[]; journal_bytes_total: number; notes_total: number; schedules: { cron_id: string; assistant_id?: string | null; interval_secs?: number | null; cron_expr?: string | null; runs_fired: number; last_run_at?: string | null; fired_in_window: number; late_max_secs?: number | null; late_median_secs?: number | null; drift: string }[]; note: string }
export const estateRoster = (days = 7) => request<EstateRoster>(`/estate/roster?days=${days}`);
/** The fleet agents made (agents.spawn): each with its maker and how long it has been idle. */
export interface SpawnedAgent { assistant_id: string; name: string; spawned_by: { assistant_id?: string; run_id?: string }; created_at: string; archived_at?: string | null; last_run_at?: string | null; idle_days: number; schedules: { interval_secs?: number | null; cron_expr?: string | null; last_run_at?: string | null; runs_fired: number }[]; notes: number; open_gaps: number }
/** Every open proposal across the agents: filed by the Coach, a gap or the review, above the running version, not declined. */
export interface OpenProposal { assistant_id: string; agent_name: string; version_id: string; proposed_by: string; kind?: string | null; why: string; created_at: string }
export const listOpenProposals = () => request<{ proposals: OpenProposal[] }>("/proposals").then((r) => r.proposals);
export const estateSpawned = () => request<{ spawned: SpawnedAgent[]; retire_idle_days: number; retire_idle_days_default: number }>("/estate/spawned");
/** The nightly threshold: spawned agents idle at least this many days are retired by the sweep; 0 switches it off. */
export const saveSpawnedSettings = (retireIdleDays: number) => request<{ retire_idle_days: number }>("/estate/spawned/settings", { method: "PUT", body: JSON.stringify({ retire_idle_days: retireIdleDays }) });
/** Retire the spawned agents idle for at least `idleDays`: archived, their notes folded into their maker's memory, the maker's owner told. */
export const retireIdleSpawned = (idleDays: number) => request<{ idle_days: number; retired: { assistant_id: string; name: string; idle_days: number; notes_folded: number }[] }>("/estate/spawned/retire", { method: "POST", body: JSON.stringify({ idle_days: idleDays }) });
/** Take a backup of the whole store now, as the signed-in person. */
export const takeEstateBackup = () => request<EstateBackup>("/estate/backups", { method: "POST" });

export interface EgressCeiling {
  open: boolean;
  hosts: CeilingHost[];
  /** Hosts connections call that no entry names — only possible while open. */
  unlisted: { host: string; connections: { instance_id: string; name: string }[] }[];
  updated_by: Record<string, unknown> | null;
  updated_at: string | null;
}
export const egressCeiling = () => request<EgressCeiling>("/egress/ceiling");
export const saveEgressCeiling = (body: { open: boolean; hosts: { host: string; note?: string | null }[] }) =>
  request<EgressCeiling>("/egress/ceiling", { method: "PUT", body: JSON.stringify(body) });
export const testLlmProvider = (id: string) =>
  request<{ ok: boolean; id: string; model?: string; latency_ms: number; said?: string; error?: string }>(`/llm/providers/${encodeURIComponent(id)}/test`, { method: "POST", body: "{}" });
export const getAssistant = (id: string) => request<Assistant>(`/assistants/${id}`);

// ── Runs ────────────────────────────────────────────────────────────────────

export interface Run {
  run_id: string;
  thread_id: string;
  graph: string;
  status: string;
  created_at: string;
  assistant_id?: string;
  /** Who started it, as the server stamped it. */
  metadata?: { created_by?: Attribution; studio?: { objective?: string }; [k: string]: unknown };
  /** The outcome verdict, when the deployment verifies runs. */
  verification?: Verification;
  /** What the run was asked: the person's first message, as a line. */
  asked?: string | null;
  /** Where the run is, or ended up (`GET /runs/{id}`): tool calls so far, the last one, the plan as last written. */
  progress?: { tool_calls: number; last_tool?: string | null; plan?: { steps: { text: string; status: string; note?: string | null }[]; done: number; open: number } | null; updated_at?: string | null } | null;
  /** The worlds it acts in, by id and name — one per system when several;
   * `gone` when the world was deleted since (the name is then its id). */
  worlds?: { world_id: string; name: string; gone?: boolean }[];
  /** For a run that paused at the gate: the decision once made, so a list
   * says continued or denied rather than needs you. */
  decision?: { status: "approved" | "denied"; resumed_run_id?: string | null; decided_at?: string | null };
  /** The connections its tool calls went through, from the server's binding
   * history — still named after a connection is revoked. */
  connections?: RunConnection[];
  /** Present on `GET /runs/{id}` when the run left a journal. */
  usage?: RunUsage | null;
}

export interface RunConnection { instance_id: string; name: string; revoked_at?: string | null; tools: { tool: string; calls: number }[] }

/** The newest runs; the server caps `limit` at 100 (default 25). */
/** Recent runs, newest first. With `assistantId` the server returns that
 * agent's runs only — a client-side filter over the newest N loses a quiet
 * agent behind a busy one. */
export const listRuns = (limit?: number, assistantId?: string) => {
  const q = [limit ? `limit=${limit}` : "", assistantId ? `assistant_id=${encodeURIComponent(assistantId)}` : ""].filter(Boolean).join("&");
  return request<Run[]>(`/runs${q ? `?${q}` : ""}`);
};

// ── Plugins: a package of connectors and skills, installed whole ────────────

export interface PluginRecord {
  id: string;
  name: string;
  version: string;
  publisher: string;
  description: string;
  source: string;
  installed_at: string;
  installed_by?: Attribution;
  connectors: { id: string; display_name: string; version: string; hash: string }[];
  skills: { name: string; revision: number }[];
  /** The vendor documentation hosts its skills read; which the egress ceiling does not admit yet; whether the ceiling is open. */
  hosts?: string[];
  hosts_outside?: string[];
  ceiling_open?: boolean;
  /** The knowledge sources it registered, and how many its pack ships. */
  knowledge?: { source_id: string; title: string; content_hash: string }[];
  knowledge_shipped?: number;
}
/** Register the knowledge a library pack ships for an installed plugin (older records, or a pack that gained files). */
export const loadPluginKnowledge = (id: string) => request<{ loaded: { source_id: string; title: string }[]; note?: string; plugin: PluginRecord }>(`/plugins/${encodeURIComponent(id)}/knowledge/load`, { method: "POST" });
/** A person admits a plugin's vendor hosts to the egress ceiling, each noted with the plugin. */
export const allowPluginHosts = (id: string) => request<{ allowed: string[]; note?: string; plugin: PluginRecord }>(`/plugins/${encodeURIComponent(id)}/hosts/allow`, { method: "POST" });

export interface PluginOffer {
  id: string;
  name: string;
  version: string;
  publisher: string;
  description: string;
  connectors: number;
  skills: number;
  installed: boolean;
}

export const listPlugins = () => request<{ plugins: PluginRecord[] }>("/plugins").then((r) => r.plugins);
export const listPluginLibrary = () => request<{ plugins: PluginOffer[] }>("/plugins/library").then((r) => r.plugins);
/** Where a plugin comes from: a pack in the library, or a GitHub repository /
 * folder / .tar.gz fetched over the server's egress policy. */
export type PluginSource = { library: string } | { url: string; ref?: string; subpath?: string };
/** Install a plugin — every member validated first, then all registered, or nothing. */
export const installPlugin = (source: PluginSource) =>
  request<PluginRecord>("/plugins/install", { method: "POST", body: JSON.stringify(source) });
/** Refused while an agent names what the plugin brought or a connection runs on its connector. */
export const uninstallPlugin = (id: string) =>
  request<{ removed: string; note: string }>(`/plugins/${id}`, { method: "DELETE" });

/** One run as the server accepted it, with its exact input. */
/** The thread's state as the checkpointer holds it — the whole conversation, the record. */
export const threadState = (threadId: string) => request<{ values?: { messages?: ChatMessage[] }; next?: unknown }>(`/threads/${encodeURIComponent(threadId)}/state`);
export const getRun = (runId: string) => request<Run & { input?: unknown; output?: { messages?: ChatMessage[] } }>(`/runs/${encodeURIComponent(runId)}`);

// ── Datasets: cases from recorded runs ───────────────────────────────────────
//
// A dataset is versioned and immutable; every case names the recorded run it
// was taken from (the server checks the run, its thread and its agent are
// real) and states what should be true: the tools a run must call, in order,
// the tools it must never call, exact final-state values, and bounds.
export interface DatasetVersion {
  name: string;
  version: string;
  created_at: string;
  case_count: number;
  digest: string;
  /** The agent most of the cases were taken from. */
  agent_id?: string | null;
}
export interface EvalExpectation {
  tool_trajectory?: { name: string; args?: Record<string, unknown> }[];
  state?: { pointer: string; expected: unknown }[];
  forbid_tools?: string[];
  max_cost_usd?: number;
  max_latency_ms?: number;
  /** What a good reply must do, in words; a model judges the reply against it. */
  rubric?: string;
  /** Rows the run must leave in its world: a table, and JSON-pointer → value matchers a row it wrote there must satisfy. */
  world_writes?: { table: string; fields?: Record<string, unknown>; like?: Record<string, string> }[];
  /** Tables of the world the run must leave nothing in. */
  no_world_writes?: string[];
}
export interface EvalCase {
  id: string;
  input: unknown;
  expect?: EvalExpectation;
  tags?: string[];
  /** The recorded run this case was taken from. */
  source: { run_id: string; thread_id: string; agent_id: string; captured_at?: string };
}
export const listDatasets = () =>
  request<{ datasets: DatasetVersion[]; truncated: boolean }>("/datasets").then((r) => r.datasets);
export const datasetVersions = (name: string) =>
  request<{ name: string; versions: DatasetVersion[] }>(`/datasets/${encodeURIComponent(name)}`).then((r) => r.versions);
export const datasetCases = (name: string, version: string) =>
  request<{ cases: EvalCase[] }>(`/datasets/${encodeURIComponent(name)}/versions/${encodeURIComponent(version)}/cases`).then((r) => r.cases);
/** Publish a version. Publishing the same content again converges (200, created: false). */
export const createDataset = (body: { name: string; version: string; cases: EvalCase[] }) =>
  request<{ name: string; version: string; created: boolean; case_count: number; digest: string }>("/datasets", { method: "POST", body: JSON.stringify(body) });

/** One case of a dataset, run against an agent and judged. */
export interface CaseEvaluation {
  case_id: string;
  tags?: string[];
  run_id?: string;
  thread_id?: string;
  /** How the run ended; absent while it runs. */
  status?: "done" | "interrupted" | "failed" | null;
  passed: boolean;
  assertions: { assertion: string; passed: boolean; expected: unknown; observed: unknown; detail?: string }[];
  /** The model's verdict on the reply, when the case carries a rubric. */
  judge?: { score: number; passed: boolean; rationale: string };
  tool_calls: string[];
  /** Approvals the evaluation gave for itself because the case ran in a
   * world — the effect landed in the stand-in. */
  approvals_in_world?: number;
  latency_ms: number;
  cost_usd: number;
  total_tokens: number;
  error?: string;
  /** The world the case ran in (its `world:<name>` tag), reset first. */
  world?: string | null;
}
/** A dataset version run against one agent: every case as a real run, judged. */
export interface DatasetEvaluation {
  evaluation_id: string;
  name: string;
  version: string;
  assistant_id: string;
  /** The skill revision the cases ran under, when one was named. */
  skill_pin?: SkillPin | null;
  started_at: string;
  finished_at?: string;
  status: "running" | "done" | "error";
  error?: string;
  passed: number;
  total: number;
  cases: CaseEvaluation[];
  /** Who started it: a person, a skill revision (`kind: skill`), or a sweep (`kind: sweep`). */
  started_by?: { kind?: string; [k: string]: unknown } | null;
}
/** Every suite at once: each dataset's newest version against the agent its cases came from. */
export interface SweepEntry { name: string; version: string; cases: number; assistant_id?: string; assistant?: string; evaluation_id?: string; skipped?: string }
export const sweepDatasets = () =>
  request<{ started: SweepEntry[] }>("/datasets/sweep", { method: "POST", body: "{}" }).then((r) => r.started);
/** Remove a dataset — every version, its cases and evaluations; refused while one is running. */
export const deleteDataset = (name: string) =>
  request<{ deleted: boolean; name: string; versions: number }>(`/datasets/${encodeURIComponent(name)}`, { method: "DELETE" });
/** A skill revision to run a suite under instead of the current one — the before of a before/after. */
export interface SkillPin { name: string; revision: number }
export const runDataset = (name: string, version: string, assistantId: string, versionId?: string, skill?: SkillPin) =>
  request<DatasetEvaluation>(`/datasets/${encodeURIComponent(name)}/versions/${encodeURIComponent(version)}/evaluations`, { method: "POST", body: JSON.stringify({ assistant_id: assistantId, ...(versionId ? { version_id: versionId } : {}), ...(skill ? { skill } : {}) }) });
export const datasetEvaluations = (name: string, version: string) =>
  request<{ evaluations: DatasetEvaluation[] }>(`/datasets/${encodeURIComponent(name)}/versions/${encodeURIComponent(version)}/evaluations`).then((r) => r.evaluations);

// ── Work: the durable task queue ─────────────────────────────────────────────
//
// Tasks are units of work workers claim from pools (or that a mailbox
// delivers to a registered agent); the server leases, retries, dead-letters
// and settles them, and links each to the run that enqueued it.
export type TaskStatus = "queued" | "leased" | "failed" | "completed" | "dead" | "cancelled";
export interface Task {
  /** A leased task whose run — or a run it delegated to — is paused for a person's decision. */
  waiting_on?: { run_id: string; agent: string; tools: string[]; since: string } | null;
  task_id: string;
  kind: string;
  payload: unknown;
  pool: string;
  recipient?: string | null;
  status: TaskStatus;
  attempt: number;
  max_attempts: number;
  error_class?: string | null;
  effect?: string | null;
  last_error?: string | null;
  idempotency_key?: string | null;
  result?: unknown;
  tokens?: { total_tokens?: number } | null;
  cost_usd?: number | null;
  run_id?: string | null;
  thread_id?: string | null;
  lease?: { owner: string; expires_at: string } | null;
  next_attempt_at?: string | null;
  deadline?: string | null;
  cancel_requested?: boolean;
  created_at: string;
  updated_at: string;
}
export interface PoolMetrics {
  pool: string;
  queue_depth: number;
  leased: number;
  concurrency_limit?: number | null;
  lease_saturation?: number | null;
  oldest_visible_task_age_ms?: number | null;
}
/** One open gap in the work order: what an agent (or induction) lacked, what closes it, how many it affects. */
export interface Gap {
  gap_id: string;
  subject: { intent?: { intent_id: string }; question_shape?: { text: string } };
  statement: string;
  origin: string;
  status: string;
  volume: number;
  failure_cost_millis: number;
  priority_score: number;
  filed_at: string;
  closure_criteria: Record<string, unknown> | string;
  /** The tools whose arrival closes it; empty when a person must. */
  closes_on_tools: string[];
  evidence: { kind: string; id: string; note?: string | null }[];
  updated_at?: string;
  /** What closed it, once closed: `run:<id>:verified`, `capability:<tool>`, … */
  resolution?: string | null;
  /** Who filed it: an agent's id when an agent did. */
  filer?: string | null;
}
export const listGaps = () => request<{ work_order: Gap[] }>("/gaps").then((r) => r.work_order);
/** The queue, the gaps a run has claimed, and the newest closed ones. */
export const listGapsAll = () => request<{ work_order: Gap[]; claimed: Gap[]; closed: Gap[] }>("/gaps");
/** Reopen closures the numbers contradict, expire parked entries, close what the platform's tools now satisfy. */
export const sweepGaps = () => request<{ reopened: string[]; expired: string[]; closed_on_capability: { gap_id: string; resolution: string }[]; claims?: { run_id: string; gap_id: string; released?: string; settled?: string }[]; recall_answered?: string[] }>("/gaps/sweep", { method: "POST", body: JSON.stringify({ threshold_millis: 500 }) });

export const listTasks = (status?: TaskStatus) =>
  request<Task[]>(`/tasks${status ? `?status=${status}` : ""}`);
export const getTask = (taskId: string) => request<Task>(`/tasks/${encodeURIComponent(taskId)}`);
export const taskMetrics = () => request<{ pools: PoolMetrics[]; now: string }>("/tasks/metrics");
export const enqueueTask = (body: { kind: string; payload: unknown; pool?: string; max_attempts?: number; effect?: string; idempotency_key?: string }) =>
  request<{ task_id?: string; deduplicated: boolean }>("/tasks", { method: "POST", body: JSON.stringify(body) });
/** Cancel a task that has not settled; the server answers 409 for one that has. */
export const cancelTask = (taskId: string) =>
  request<Task>(`/tasks/${encodeURIComponent(taskId)}/cancel`, { method: "POST", body: "{}" });

// ── Memory: what agents remember, per person ─────────────────────────────────
//
// A record is scoped: `user:<id>` is what an agent learned about that person
// and reaches only their runs; `agent:<id>` is what the agent learned for
// itself. The server shows a person their own and every agent's; an
// administrator sees everyone's.
export interface MemoryRecord {
  memory_id: string;
  /** `pending` while a note an agent proposed about a person waits for them. */
  candidacy?: "pending" | string | null;
  kind: "fact" | "preference" | "example" | "summary" | string;
  scope: { scope: "user" | "agent" | "team" | "tenant" | "run" | string; id: string };
  content: { kind: string; value?: unknown };
  /** `trigger:<phrase>` recall hooks; `origin:untrusted` when the runtime saw the run read outside content before the write. */
  tags?: string[];
  provenance: { author: { type: string; agent_id?: string; human_id?: string; name?: string }; /** The run the note was learned in, when an agent or the review wrote it; a note without one is unsourced. */ evidence?: { run_id?: string | null; event_ids?: string[] }; written_at: string };
  confidence: number;
  key?: string;
  created_at: string;
}
export const queryMemory = (scope?: { scope: string; id: string }) =>
  request<{ records: MemoryRecord[] }>("/memory/query", { method: "POST", body: JSON.stringify(scope ? { scope } : {}) }).then((r) => r.records);
/** Notes agents proposed about people, waiting for the person to accept them. */
export const proposedMemory = () =>
  request<{ records: MemoryRecord[] }>("/memory/query", { method: "POST", body: JSON.stringify({ candidates_only: true }) }).then((r) => r.records);
/** The person a note is about accepts it: from then on it is recalled. */
/** A person vouches for a note the runtime marked as learned from outside content: the note is kept in their name, unmarked. */
export const confirmMemory = (memory_id: string) =>
  request<{ confirmed: string; supersedes: string }>(`/memory/${encodeURIComponent(memory_id)}/confirm`, { method: "POST" });
export const acceptMemory = (memory_id: string) =>
  request<{ accepted: string; proposal: string }>(`/memory/${encodeURIComponent(memory_id)}/accept`, { method: "POST" });
/** A pair of live records that contradict each other under one key — evidence
 * the server flags and never resolves on its own. */
export interface MemoryConflict { scope: { scope: string; id: string }; key: string; memory_ids: string[]; [k: string]: unknown }
export const memoryConflicts = (scope?: { scope: string; id: string }) =>
  request<{ conflicts: MemoryConflict[] }>("/memory/conflicts", { method: "POST", body: JSON.stringify(scope ? { scope } : {}) }).then((r) => r.conflicts);
/** Per note: how many verified runs it was read into, how many failed ones, and the smoothed success rate (basis points). */
export interface MemoryUtility { stamp: string; notes: number; entries: Record<string, { successful_uses: number; failed_uses: number; smoothed_success_bps: number }>; /** The last sweep: expired-and-unhelpful notes reaped, and old agent-written notes that never helped a verified run (for a person). */ sweep?: { stamp?: string | null; reaped: string[]; never_helped: string[]; /** Never-helped notes with no source run: decayed, ranked last at recall until a verified run uses them. */ decayed?: string[] } }
export const memoryUtility = () => request<MemoryUtility & { rolling?: boolean }>("/memory/utility");
/** Roll up now: answered at once; the roll-up, sweep and consolidation run in the background and `memoryUtility().rolling` says until when. */
export const rollUpMemoryUtility = () => request<{ started: boolean; rolling: boolean; stamp?: string | null }>("/memory/utility/roll-up", { method: "POST" });
/** One curated memory block as the agent's runs see it: declared label, description and limit, current text. */
export interface AgentBlock { label: string; description: string; char_limit: number; text: string; version?: string | null; author?: unknown; updated_at?: string | null; /** false when the block holds text but the published agent does not declare it (a working copy's block). */ declared?: boolean }
export const agentBlocks = (assistantId: string) => request<{ blocks: AgentBlock[] }>(`/assistants/${assistantId}/memory/blocks`).then((r) => r.blocks);
/** Every version a block has had, newest first: who wrote it, when, the text. */
export interface AgentBlockVersion { version: string; author: string; written_at: string; chars: number; lines: number; text: string; supersedes?: string | null; current: boolean }
export const agentBlockHistory = (assistantId: string, label: string) => request<{ label: string; versions: AgentBlockVersion[] }>(`/assistants/${assistantId}/memory/blocks/${encodeURIComponent(label)}/history`).then((r) => r.versions);
/** A person sets a block's text — the same record memory.block_edit writes, authored by the person; 422 past the block's limit. */
export const saveAgentBlock = (assistantId: string, label: string, text: string, charLimit?: number) => request<{ label: string; text: string; chars: number; limit: number; version: string }>(`/assistants/${assistantId}/memory/blocks/${encodeURIComponent(label)}`, { method: "PUT", body: JSON.stringify(charLimit != null ? { text, char_limit: charLimit } : { text }) });
export const forgetMemory = (memory_id: string) =>
  request<{ forgotten: string[] }>("/memory/forget", { method: "POST", body: JSON.stringify({ memory_id, reason: "erasure_request" }) });

// ── Channels: schedules and webhooks ────────────────────────────────────────

/** A schedule: the server fires the agent on a cadence, with a standing
 * message, attributed to whoever set it. */
export interface Schedule {
  cron_id: string;
  graph: string;
  assistant_id?: string;
  interval_secs?: number;
  cron_expr?: string;
  input?: { messages?: { role: string; content: string }[]; [k: string]: unknown } | null;
  created_by?: Attribution;
  created_at: string;
  last_run_at?: string | null;
  runs_fired: number;
  /** The most runs the schedule may ever fire; absent is no cap. */
  max_runs?: number | null;
  /** The most tokens its runs may spend, all told; absent is no cap. */
  max_tokens?: number | null;
  /** What the runs it fired have spent so far, in tokens. */
  tokens_spent?: number;
  /** What the studio put on it: the agent a Coach review schedule reviews. */
  metadata?: { studio?: { coach_review_of?: string; cadence?: string } } | null;
  /** Set while a run this schedule fired waits on a decision: it does not fire again until decided. */
  held?: { run_id: string; since: string } | null;
  /** Set while the schedule cannot fire at all — its agent is gone or archived. */
  stalled?: string | null;
  on_run_completed: "keep" | "delete";
  /** The world every fired run acts in, when the schedule was put in one — standing work rehearsed in the stand-in. */
  world?: string | null;
  world_name?: string | null;
}

export const listSchedules = () => request<Schedule[]>("/crons");
export const createSchedule = (body: { assistant_id: string; interval_secs?: number; cron_expr?: string; input?: unknown; on_run_completed?: "keep" | "delete"; metadata?: unknown; /** A world (by name) every fired run acts in. */ world?: string; /** Several, one per system; `world` is the first. */ worlds?: string[]; /** The most runs the schedule may ever fire; absent is no cap. */ max_runs?: number; /** The most tokens its runs may spend, all told. */ max_tokens?: number }) =>
  request<Schedule>("/crons", { method: "POST", body: JSON.stringify(body) });
export const deleteSchedule = (id: string) => request<{ deleted: boolean }>(`/crons/${id}`, { method: "DELETE" });

/** A webhook: a signed inbound URL bound to an agent; each event becomes a
 * run whose input is the template rendered against the event. */
export interface Webhook {
  trigger_id: string;
  name: string;
  target: { kind: "assistant" | "thread"; id: string };
  action: "start_run" | "resume_thread" | "send_message";
  input_template: unknown;
  enabled: boolean;
  /** The HMAC secret the sender signs with — the server returns it on create and read. */
  secret: string;
  created_by?: Attribution;
  /** The world every fired run acts in, when the webhook was put in one. */
  world?: string | null; world_name?: string | null;
  debounce_ms?: number | null;
  /** An identical event inside this window is a repeat of the one already run, not a second run. */
  repeat_window_ms?: number | null;
  created_at: string;
  events_received: number;
  runs_fired: number;
}

export interface WebhookEvent {
  event_id: string;
  payload_hash: string;
  payload: unknown;
  action: string;
  status: string;
  run_id?: string;
  error?: string;
  replayed_from?: string;
  /** Set on a repeat: the event whose run answers it. */
  duplicate_of?: string;
  created_at: string;
}

export const listWebhooks = () => request<Webhook[]>("/triggers");
export const createWebhook = (body: { name: string; target: { kind: "assistant"; id: string }; action: "start_run"; input_template: unknown; enabled: boolean; debounce_ms?: number; repeat_window_ms?: number; /** A world (by name) every fired run acts in. */ world?: string; /** Several, one per system; `world` is the first. */ worlds?: string[] }) =>
  request<Webhook>("/triggers", { method: "POST", body: JSON.stringify(body) });
export const deleteWebhook = (id: string) => request<{ deleted: boolean }>(`/triggers/${id}`, { method: "DELETE" });
export const listWebhookEvents = (id: string) => request<WebhookEvent[]>(`/triggers/${id}/events`);
/** Where a sender posts: the server's own address plus the trigger path. */
export const webhookUrl = (id: string) => `${apiBase()}/triggers/${id}/webhook`;

// ── Skills ──────────────────────────────────────────────────────────────────

export interface ServerSkill {
  name: string;
  description: string;
  revision: number;
  content_hash: string;
  /** Absent when the skill declares none — the listing omits empty fields. */
  allowed_tools?: string[];
  eval_gate?: string;
  license?: string;
}

export const listSkills = () => request<{ skills: ServerSkill[] }>("/skills").then((r) => r.skills);

// ── Connections ─────────────────────────────────────────────────────────────

export interface Connection {
  connection_id: string;
  provider?: string;
  status?: string;
  [k: string]: unknown;
}

export const listConnections = () => request<{ connections: Connection[] }>("/connections").then((r) => r.connections);
/** Every connection with its health, as the server last probed it. */
export const connectionsHealth = () =>
  request<{ connections: (Connection & { health?: unknown })[] }>("/connections/health").then((r) => r.connections);
/** Withdraw a connection's consent (admins); the substrate stops answering for it. */
export const revokeConnectionConsent = (connectionId: string) =>
  request<unknown>(`/connections/${encodeURIComponent(connectionId)}/revoke`, { method: "POST", body: "{}" });
/** The policy version in force, or null when none has been activated. */
export const activePolicy = () =>
  request<{ version: string; record: Record<string, unknown> }>("/policy/active").catch((err) => {
    if (err instanceof Error && /404|not found|no active/i.test(err.message)) return null;
    throw err;
  });

export const apiBase = () => BASE;

// ── A run's journal and its receipt ─────────────────────────────────────────

/** A model call reports its token split; other events report none. */
export interface RunTokens { prompt_tokens?: number; completion_tokens?: number; total_tokens?: number; cached_tokens?: number; reasoning_tokens?: number }

/** What a run's model calls cost in tokens, and how much of the prompt the
 * provider served from its cache — the frozen prefix's measure. */
export interface RunUsage {
  model_calls: number;
  prompt_tokens: number;
  cached_tokens: number;
  completion_tokens: number;
  cache_reported_calls: number;
  cache_hit_rate: number | null;
}

/** Cache hits per model over the newest journaled runs. */
export interface CacheStats {
  runs: number;
  models: Record<string, { calls: number; prompt_tokens: number; cached_tokens: number; cache_reported_calls: number; cache_hit_rate: number | null }>;
}

export interface RunEvent {
  id: string;
  seq: number;
  kind: string;
  effect: string;
  status: string;
  node_id: string | null;
  recorded_at: string;
  tokens: RunTokens | number | null;
  cost_usd: number | null;
  latency_ms: number | null;
  input?: unknown;
  output?: unknown;
}

export const runEvents = (runId: string) =>
  request<{ complete: boolean; events: RunEvent[] }>(`/runs/${runId}/events`);
/** One journaled payload an event names by content address instead of carrying inline — the assembled prompt of a model call, a long tool result. */
export const runPayload = (runId: string, sha256: string) =>
  request<{ run_id: string; sha256: string; payload: unknown }>(`/runs/${runId}/payloads/${sha256}`).then((r) => r.payload);

/** An artifact a run produced — named, versioned, its bytes readable by address. */
export interface RunArtifact {
  artifact_id: string; name?: string | null; media_kind: string; media_type?: string | null;
  lineage: { run_id: string; effect_id: string; event_id: string };
  versions?: { sha256: string; bytes: number; committed_at: string }[];
}
/** Every artifact the tenant's runs filed, by name or address. */
export const listArtifacts = () => request<{ artifacts: RunArtifact[] }>("/artifacts").then((r) => r.artifacts);
/** An artifact's bytes as text, for a preview. */
export const artifactText = (artifactId: string) =>
  fetch(artifactBytesUrl(artifactId), { credentials: "include" }).then(async (r) => { if (!r.ok) throw new ServerError(r.status, `/artifacts/${artifactId}/bytes`, await refusal(r)); return r.text(); });
/** The artifacts one run produced (the lineage join). */
export const listRunArtifacts = (runId: string) =>
  request<{ artifacts: RunArtifact[] }>(`/artifacts?run_id=${encodeURIComponent(runId)}`).then((r) => r.artifacts);
/** Where an artifact's bytes are read from, for a link. */
export const artifactBytesUrl = (artifactId: string) => `${BASE}/artifacts/${encodeURIComponent(artifactId)}/bytes`;

/** What the server will sign for: the journal head it saw, the policy that
 * executed it, and the signature over both. */
export interface Receipt {
  format_version: number;
  run_id: string;
  journal_head: { events: number; sha256: string };
  executor_policy: string;
  signer: string;
  signature: string;
}

export const runReceipt = (runId: string) => request<Receipt>(`/runs/${runId}/receipt`);

// ── Threads and running an agent ────────────────────────────────────────────

export interface Thread { thread_id: string; graph: string }

export const createThread = (graph: string) =>
  request<Thread>("/threads", { method: "POST", body: JSON.stringify({ graph }) });

/** One journaled message. An assistant turn that called a tool has
 * `tool_calls` and no content; the tool's answer comes back as a `tool` role
 * message carrying the call it answers. */
export interface ChatMessage {
  role: string;
  content?: string | null;
  tool_calls?: { id: string; type?: string; function?: { name?: string; arguments?: string } }[];
  tool_call_id?: string;
}

/** What the server decided about a completed run's outcome: by the
 * evidence — the calls the turn made and what came back — not by what the
 * reply claims. `unverified` is the judge saying it could not tell. */
export interface Verification {
  verdict: "verified" | "failed" | "unverified";
  reason: string;
  evidence: { calls: { tool: string; effect: string; outcome: string }[]; writes: number; refused: number; unanswered: number };
  at: string;
  /** Present when the verdict came after one repair turn: the first verdict,
   * the one that sent the model back. */
  repaired?: { verdict: string; reason: string };
}

/** How the server's repair notice begins — the judge's word, sent to the
 * model once as a system message when a turn's outcome was not achieved. */
export const REPAIR_NOTICE_PREFIX = "The verification step ";

/** Why a run stopped at a ceiling rather than finishing: the limit it met,
 * what it had spent, and the note a person reads before resuming it. */
export interface Halt { reason: "step_ceiling" | "budget_ceiling" | string; limit?: number; steps_run?: number; crossed?: string; note?: string }

/** Carry a halted run on: the thread resumes from its boundary checkpoint,
 * with a higher ceiling when the task genuinely needs the room. */
/** Stop a running agent: the run ends at its next step with its checkpoint kept. */
export const cancelRun = (runId: string) =>
  request<{ run_id: string; run: "stopping" | "cancelled" | "finished" | "unknown"; note?: string }>(`/runs/${encodeURIComponent(runId)}/cancel`, { method: "POST", body: "{}" });

export const resumeRun = (threadId: string, assistantId: string, maxSteps?: number) =>
  request<RunResult>(`/threads/${threadId}/runs/wait`, {
    method: "POST",
    body: JSON.stringify({ assistant_id: assistantId, resume: {}, ...(maxSteps ? { config: { recursion_limit: maxSteps } } : {}) }),
  });

export interface RunResult {
  run_id: string;
  thread_id: string;
  status: string;
  /** On `error`: the kind (`budget_exhausted`, `node_error`, …) and the words. */
  error?: string;
  message?: string;
  /** What the run spent: model calls, tokens, and cost when the model is priced (null when not). */
  spend?: { requests: number; tokens: number; cost_usd: number | null };
  output?: { messages?: ChatMessage[] };
  verification?: Verification;
  /** Present when the run paused: an approval interrupt carries what it asked;
   * a ceiling carries `rusty.halted` — why it stopped and what it had spent. */
  interrupt?: { kind?: string; requests?: ApprovalRequest[]; "rusty.halted"?: Halt; [k: string]: unknown };
}

// ── Approvals: a run that paused before an irreversible effect ─────────────

/** One call a paused run will not make without a decision. */
/** The connection a paused action runs through, as bound when it paused;
 * `revoked_at` when that connection was revoked while the approval waited —
 * approving then reaches nothing. */
export interface RequestConnection { instance_id: string; name: string; revoked_at?: string | null }
export interface ApprovalRequest { call_id: string; tool: string; arguments: unknown; kind: string; effect_id: string; connection?: RequestConnection }

export interface Approval {
  run_id: string;
  thread_id: string;
  graph: string;
  assistant_id?: string;
  requests: ApprovalRequest[];
  requested_at: string;
  requested_by?: Attribution;
  status: "pending" | "approved" | "denied";
  decided_at?: string;
  decided_by?: Attribution;
  reason?: string;
  resumed_run_id?: string;
  /** The agent by name, said by the server so the card never shows an id. */
  agent_name?: string;
  /** The world the paused run acts in, when it was put in one: the
   * approved effect lands there, not in the live system. `gone` when the
   * world was removed while the decision waited — approving reaches nothing. */
  world?: { world_id: string; name?: string; stands_for?: string; gone?: boolean };
}

// ── Notices: the platform telling a person something ────────────────────────
// Kept apart from the record of what was done: an assignment's progress says
// what happened; a notice says its owner was told, and when they saw it.
export interface Notice {
  notice_id: string; tenant: string; to: Attribution;
  about: { kind: string; assignment_id?: string; state?: string; /** An approval notice: the paused run, and the world it acts in when it does. */ run_id?: string; channel?: string; world?: string | null; /** A sweep's notice: the suite that did not pass. */ dataset?: string; version?: string; evaluation_id?: string; /** A task's notice: the task that could not be done. */ task_id?: string; pool?: string; task_kind?: string; /** A schedule's or a webhook's notice. */ cron_id?: string; trigger_id?: string; event_id?: string };
  key: string; title: string; text: string; channel: string; created_at: string; seen_at?: string | null;
}
/** The signed-in person's notices, unseen first, newest first. */
export const listNotices = () => request<{ notices: Notice[] }>("/notices").then((r) => r.notices);
export const markNoticeSeen = (id: string) => request<Notice>(`/notices/${encodeURIComponent(id)}/seen`, { method: "POST", body: "{}" });

export const listApprovals = (status?: "pending" | "approved" | "denied") =>
  request<{ approvals: Approval[] }>(`/approvals${status ? `?status=${status}` : ""}`).then((r) => r.approvals);
/** The decision: approving mints tokens for exactly the calls asked about and
 * resumes the run; denying resumes it with the refusal in the model's hands. */
/** Decide a paused run. With `standingHours`, an approval stands: the same
 * call — this agent, this tool, these arguments — runs without asking for
 * that long. */
export const decideApproval = (runId: string, decision: "approve" | "deny", reason?: string, standingHours?: number) =>
  request<Approval & { standing?: StandingApproval[] }>(`/approvals/${runId}/decide`, { method: "POST", body: JSON.stringify({ decision, ...(reason ? { reason } : {}), ...(standingHours ? { standing_hours: standingHours } : {}) }) });

/** A decision that stands: the same call from the same agent is approved
 * without asking until `until`, or until it is withdrawn. */
export interface StandingApproval {
  id: string; tenant: string; assistant_id?: string | null; tool: string; arguments: unknown;
  by: Attribution; from_run: string; made_at: string; until: string; uses: number;
  /** Whether it still decides, as the server says. */
  live?: boolean;
}
export const listStandingApprovals = () => request<{ standing: StandingApproval[] }>("/approvals/standing").then((r) => r.standing);
export const withdrawStandingApproval = (id: string) => request<{ withdrawn: boolean; id: string }>(`/approvals/standing/${encodeURIComponent(id)}`, { method: "DELETE" });

/** Run the assistant on this thread and wait for it to settle. The server owns
 * the turn: the model call, the tools, the journal and the receipt. */
/** One turn, waited for. A `world` (by name) answers every call the turn
 * makes to the system it stands in for, so an agent is tried against a
 * stand-in before it is pointed at the live one. */
/** Run the agent on this thread and wait. `world` is one stand-in, or one
 * per system the agent touches — the first is the run's `world`, all of
 * them its `worlds`. */
export const runAndWait = (threadId: string, assistantId: string, messages: ChatMessage[], world?: string | string[], overrides?: DraftConfig) => {
  const chosen = (Array.isArray(world) ? world : world ? [world] : []).filter(Boolean);
  // A draft under the builder runs as the agent but with the working copy's charter, tools and skills — what the person sees is what runs.
  const o = overrides ?? {};
  const noted = (o.tools ?? []).filter((t) => t.when?.trim());
  const draft = {
    ...(o.instructions !== undefined ? { instructions: o.instructions } : {}),
    ...(o.tools ? { tool_allowlist: o.tools.map((t) => t.name) } : {}),
    ...(noted.length ? { tool_notes: Object.fromEntries(noted.map((t) => [t.name, t.when!.trim()])) } : {}),
    ...(o.skills?.length ? { skill_names: o.skills } : {}),
    ...(o.model ? { model: o.model } : {}),
    ...(o.fallback_model ? { fallback_model: o.fallback_model } : {}),
    ...(o.variables && Object.keys(o.variables).length ? { variables: o.variables } : {}),
    ...(o.memory_access ? { memory_access: o.memory_access } : {}),
    ...(o.context ? { context: o.context } : {}),
    ...(o.recursion_limit ? { recursion_limit: o.recursion_limit } : {}),
    // Zero is a cap too: this agent may start no work.
    ...(o.chain_max_tokens != null ? { chain_max_tokens: o.chain_max_tokens } : {}),
    ...(o.temperature != null ? { temperature: o.temperature } : {}),
    ...(o.memory_blocks_declared ? { memory_blocks_declared: o.memory_blocks_declared } : {}),
  };
  const world_ = chosen.length > 1 ? { world: chosen[0], worlds: chosen } : chosen.length === 1 ? { world: chosen[0] } : {};
  const config = Object.keys({ ...draft, ...world_ }).length ? { config: { ...draft, ...world_ } } : {};
  return request<RunResult>(`/threads/${threadId}/runs/wait`, {
    method: "POST",
    body: JSON.stringify({ assistant_id: assistantId, input: { messages }, ...config }),
  });
};

/** What a draft is made of, tried before any agent exists: its charter, the
 * tools it may call with the builder's note on each, the skills it follows
 * by name, and the stand-in the run acts in. The server resolves skills and
 * notes at admission exactly as an agent's. */
export interface DraftConfig { /** The working copy's cap on the work it starts (tokens, the whole chain). */ chain_max_tokens?: number; /** The working copy's sampling temperature, so a draft under test samples as it will. */ temperature?: number; /** The working copy's declared memory blocks, so a draft under test renders and edits its own before publishing. */ memory_blocks_declared?: { label: string; description?: string; char_limit?: number }[]; instructions?: string; /** The working copy's step ceiling: the draft under test halts where the published agent would. */ recursion_limit?: number; tools?: { name: string; when?: string }[]; skills?: string[]; model?: string | null; fallback_model?: string | null; variables?: Record<string, string>; world?: string | string[]; /** The working copy's long-term memory switch: `none` runs without memory and without the memory tools. */ memory_access?: "read_write" | "none"; /** The working copy's context window and kept turns, as the Model & behavior card sets them. */ context?: { budget_tokens?: number; keep_recent_messages?: number } }

/** One turn of a draft agent on a thread. The charter enters the
 * conversation as the first message on the thread's first turn, as it does
 * for an agent; later turns carry only the person's message. */
export const runDraft = (threadId: string, draft: DraftConfig, messages: ChatMessage[], firstTurn: boolean) => {
  const chosen = (Array.isArray(draft.world) ? draft.world : draft.world ? [draft.world] : []).filter(Boolean);
  const tools = draft.tools ?? [];
  const notes = Object.fromEntries(tools.filter((t) => t.when?.trim()).map((t) => [t.name, t.when!.trim()]));
  const config: Record<string, unknown> = {
    instructions: draft.instructions ?? "",
    tool_allowlist: tools.map((t) => t.name),
    ...(Object.keys(notes).length ? { tool_notes: notes } : {}),
    ...(draft.skills?.length ? { skill_names: draft.skills } : {}),
    ...(draft.model ? { model: draft.model } : {}),
    ...(draft.fallback_model ? { fallback_model: draft.fallback_model } : {}),
    ...(draft.variables && Object.keys(draft.variables).length ? { variables: draft.variables } : {}),
    ...(draft.memory_access ? { memory_access: draft.memory_access } : {}),
    ...(draft.context ? { context: draft.context } : {}),
    ...(chosen.length > 1 ? { world: chosen[0], worlds: chosen } : chosen.length === 1 ? { world: chosen[0] } : {}),
  };
  const input = { messages: firstTurn ? [{ role: "system", content: draft.instructions }, ...messages] : messages };
  return request<RunResult>(`/threads/${threadId}/runs/wait`, { method: "POST", body: JSON.stringify({ input, config }) });
};

// ── Verifying a receipt ─────────────────────────────────────────────────────

/** The exported journal a receipt claims to cover. Verification is done on
 * evidence the caller holds, not on the server's word for it. */
export interface JournalSnapshot { run_id: string; thread_id: string; events: unknown[]; [k: string]: unknown }

export const runFixture = (runId: string) =>
  request<{ journal: JournalSnapshot }>(`/runs/${runId}/fixture`).then((f) => f.journal);

export interface VerifiedRun {
  run_id: string;
  journal_head: { events: number; sha256: string };
  manifest_digest: string;
  effect_receipts: number;
  executor_policy: string;
  signer: string;
}

/** Verify a receipt against the journal it claims to cover. A refusal is an
 * error from the server — a tampered journal or an unknown key does not come
 * back as a pass. */
export const verifyReceipt = (snapshot: JournalSnapshot, receipt: Receipt) =>
  request<VerifiedRun>("/receipts/verify", { method: "POST", body: JSON.stringify({ snapshot, receipt }) });

// ── Connectors ──────────────────────────────────────────────────────────────
//
// The connector surface is the server's, and it is one standard: a manifest
// declares its configuration as a JSON Schema (`connection_specification`) and
// its operations; a config is validated against that schema, checked against
// the live system, and only then saved as an instance with its secrets sealed.
// Studio renders the schema. It never knows what a connector needs — it reads
// what the connector declared.

export type ConnectorEffect = "read_only" | "idempotent" | "compensatable" | "irreversible";

export interface ConnectorOperation {
  name: string;
  description: string;
  method: string;
  path: string;
  effect: ConnectorEffect;
  params_schema: Record<string, unknown>;
  auth: unknown[];
}

/** The consent round trip a connector needs before it can be used. Present
 * only on a connector whose credentials are granted rather than typed. */
export interface Authorization {
  authorize_url: string;
  token_url: string;
  scopes: string;
}

export interface ConnectorManifest {
  id: string;
  version: string;
  display_name: string;
  description: string;
  documentation_url: string;
  base_url: string;
  connection_specification: Record<string, unknown>;
  operations: ConnectorOperation[];
  check: string;
  authorization?: Authorization | null;
  hash: string;
}

export const listConnectorManifests = () =>
  request<{ manifests: ConnectorManifest[] }>("/connectors").then((r) => r.manifests);

/** Register a manifest. The hash is the server's to compute — a manifest
 * written in the studio's field carries none. */
export const registerConnectorManifest = (manifest: unknown) =>
  request<{ hash: string; registered: boolean }>("/connectors", {
    method: "POST",
    body: JSON.stringify(manifest),
  });

export interface ConnectorInstance {
  instance_id: string;
  manifest_hash: string;
  /** The tools this connection derives — the ones agents run through. */
  tools?: string[];
  /** The agents that name one of those tools. */
  agents?: { assistant_id: string; name: string }[];
  /** What this is a connection to. Carried by the server, because the manifest
   * it was configured against may since have been superseded in the library. */
  connector: { id: string; display_name: string; version: string } | null;
  /** Whether this connection has actually been granted, and until when. */
  authorization: GrantStatus | null;
  /** Non-secret config; every sealed value comes back as `{rusty_secret: true}`. */
  config: Record<string, unknown>;
  created_at: string;
}

export type GrantStatus =
  | { kind: "not_required" }
  | { kind: "needs_auth" }
  | { kind: "expired"; expires_at: string }
  | { kind: "connected"; expires_at?: string; refreshable: boolean };

export const listConnectorInstances = () =>
  request<{ instances: ConnectorInstance[] }>("/connectors/instances").then((r) => r.instances);

export const createConnectorInstance = (manifestHash: string, config: unknown) =>
  request<ConnectorInstance>("/connectors/instances", {
    method: "POST",
    body: JSON.stringify({ manifest_hash: manifestHash, config }),
  });

/** Rotate: a new credential on the same connection. The instance id is what
 * every agent's tool list names, so it does not change. */
export const rotateConnection = (instanceId: string, config: unknown) =>
  request<ConnectorInstance>(`/connectors/instances/${instanceId}`, {
    method: "PUT",
    body: JSON.stringify({ config }),
  });

/** Revoke: the connection, its sealed secrets and every tool it derived are
 * gone. A run that names it from then on is refused at admission. */
/** A connection follows its connector to another version: same id, same
 * sealed credentials, the new manifest's operations — only after the
 * system answers the new version's check. */
/** A read-back a connector's own reads make possible for one of its writes. */
export interface ReadBackProposal { write: string; operation: string; arguments: Record<string, unknown>; why: string }
export interface ReadBacks { hash: string; id: string; version: string; proposals: ReadBackProposal[]; unproposable: { write: string; why: string }[]; declared: { write: string; operation: string }[] }
/** The writes of a connector version that would guess after a lost answer, and the read-backs proposed for them. */
export const readBackProposals = (manifestHash: string) => request<ReadBacks>(`/connectors/${encodeURIComponent(manifestHash)}/read-backs`);
/** Adopt read-backs: a new version of the connector in the library; move the connection to it with `upgradeConnection`. */
export const adoptReadBacks = (manifestHash: string, adopt: { write: string; operation: string; arguments: unknown }[], instanceId?: string) =>
  request<{ hash: string; id: string; version: string; registered: boolean; moved?: string | null }>(`/connectors/${encodeURIComponent(manifestHash)}/read-backs`, {
    method: "POST",
    body: JSON.stringify({ writes: adopt.map((a) => a.write), adopt: adopt.map((a) => ({ operation: a.operation, arguments: a.arguments })), ...(instanceId ? { instance_id: instanceId } : {}) }),
  });

export const upgradeConnection = (instanceId: string, manifestHash: string) =>
  request<ConnectorInstance>(`/connectors/instances/${instanceId}/upgrade`, {
    method: "POST",
    body: JSON.stringify({ manifest_hash: manifestHash }),
  });

export const revokeConnection = (instanceId: string) =>
  request<void>(`/connectors/instances/${instanceId}`, { method: "DELETE" });

/** The Airbyte verdict: `succeeded`, or `failed` with the reason the system
 * itself gave. A check runs the manifest's own check operation. */
export interface CheckOutcome { status: "succeeded" | "failed"; message?: string; /** The world the check was proved against, when it was. */ world?: string | null; /** Proved against a world while the live host sits outside the egress ceiling: the connection's live calls will meet it. */ outside_ceiling?: string | null }

export const checkConnectorConfig = (manifestHash: string, config: unknown, world?: string) =>
  request<CheckOutcome>("/connectors/check", {
    method: "POST",
    body: JSON.stringify({ manifest_hash: manifestHash, config, ...(world ? { world } : {}) }),
  });

export const checkConnectorInstance = (instanceId: string, world?: string) =>
  request<CheckOutcome>("/connectors/check", {
    method: "POST",
    body: JSON.stringify({ instance_id: instanceId, ...(world ? { world } : {}) }),
  });

export interface ConnectorTool {
  name: string;
  description: string;
  effect: ConnectorEffect;
  /** The read-only tool this write checks itself with when its answer is lost. */
  reconcile?: string | null;
  [k: string]: unknown;
}

/** The tools this connection puts in front of an agent — derived from the
 * manifest's operations, not written a second time. */
export const connectorInstanceCatalog = (instanceId: string) =>
  request<{ instance_id: string; manifest_hash: string; tools: ConnectorTool[] }>(
    `/connectors/instances/${instanceId}/catalog`,
  );

// ── Skills ──────────────────────────────────────────────────────────────────
//
// A skill is a versioned package: SKILL.md, the tools it is allowed to use, an
// eval gate, a licence, and an append-only revision history. The server owns
// all of it — registration is content-addressed, so registering the same text
// twice is the same revision, not a second one.

export interface SkillBody { name: string; revision: number; content_hash: string; body: string; references?: string[]; learns?: { reference: string | null; reads: number } | null }

export const skillBody = (name: string) => request<SkillBody>(`/skills/${name}/body`);
/** What a skill learned from the system it follows: one reference, as Markdown. */
/** What a learned reference was read from, and what the last check made of it. */
export interface SkillFreshness {
  skill: string; revision: number; reference: string; learned_at: string; checked_at?: string | null;
  /** `true` once the system has moved under it; the reference still reads, and says so. */
  stale: boolean;
  /** What moved, in words — empty while it is current. */
  because: string[];
  reads: { title: string; tool: string; records: number; version_field?: string | null; newest?: string | null }[];
}
export const skillFreshness = (name: string) =>
  request<{ learned: boolean; freshness?: SkillFreshness; note?: string }>(`/skills/${encodeURIComponent(name)}/freshness`);
/** Re-read the system now and compare: deterministic, no model. */
export const checkSkillFreshness = (name: string) =>
  request<{ learned: boolean; freshness: SkillFreshness }>(`/skills/${encodeURIComponent(name)}/freshness`, { method: "POST", body: "{}" });

export const skillReference = (name: string, path: string) =>
  fetch(`${BASE}/skills/${encodeURIComponent(name)}/reference?path=${encodeURIComponent(path)}`, { credentials: "include" }).then(async (r) => { if (!r.ok) throw new ServerError(r.status, path, await refusal(r)); return r.text(); });
export interface LearnReceipt { name: string; revision: number; content_hash: string; reference: string; rows: number; reads: number; already_registered: boolean; gate: unknown }
/** Run the skill's declared reads through the live connection and keep what came back as its reference. */
export const learnSkill = (name: string, body: { reference?: string; reads?: { title: string; tool: string; arguments: Record<string, unknown>; rows?: number }[] } = {}) =>
  request<LearnReceipt>(`/skills/${encodeURIComponent(name)}/learn`, { method: "POST", body: JSON.stringify(body) });

export interface SkillRegistration {
  name: string;
  revision: number;
  content_hash: string;
  already_registered: boolean;
  /** A new revision runs the suites of every agent that follows the skill;
   * one entry per suite started (or that could not be). */
  gate?: SkillGateEntry[];
  /** True when a follower has a suite: the revision waits at the gate and
   * the followers keep running `current` until it is promoted. */
  held?: boolean;
  /** The revision the followers run after this registration. */
  current?: number;
}

/** One follower's verdict on a candidate revision: its suite, evaluated
 * pinned to that revision. */
export interface SkillSuiteEvidence {
  assistant_id: string;
  assistant: string;
  dataset: string;
  version: string;
  cases: number;
  state: "passed" | "failed" | "missing" | "running";
  evaluation_id?: string | null;
  passed: number;
  total: number;
  evaluated_at?: string | null;
  /** Why it failed, per case. */
  failures?: { case_id: string; said: string }[];
}

export interface SkillPromotion {
  revision: number;
  by: unknown;
  at: string;
  override_reason?: string | null;
  evidence: { revision: number; suites: SkillSuiteEvidence[]; ok: boolean; unevaluated: boolean };
}

/** What the gate knows about one revision of a skill. */
export interface SkillEvidence {
  name: string;
  /** The revision the followers run. */
  current: number;
  latest: number;
  evidence: { revision: number; suites: SkillSuiteEvidence[]; ok: boolean; unevaluated: boolean };
  promotions: SkillPromotion[];
}

export const skillEvidence = (name: string, revision?: number) =>
  request<SkillEvidence>(`/skills/${encodeURIComponent(name)}/evidence${revision ? `?revision=${revision}` : ""}`);

/** Make a revision the one its followers run: needs every follower's suite
 * passed against it, or an admin's reason (kept with the promotion). */
export const promoteSkill = (name: string, revision?: number, overrideReason?: string) =>
  request<{ name: string; current: number; promoted: boolean }>(`/skills/${encodeURIComponent(name)}/promote`, {
    method: "POST",
    body: JSON.stringify({ ...(revision ? { revision } : {}), ...(overrideReason ? { override_reason: overrideReason } : {}) }),
  });

export interface SkillGateEntry {
  assistant_id: string;
  assistant: string;
  dataset: string;
  version: string;
  cases: number;
  evaluation_id?: string;
  error?: string;
}

/** Register (or re-register) a skill from its SKILL.md. */
/** Register a SKILL.md as the signed-in person. The server stamps the
 * author from the session; a program with its own name may pass one. */
/** Take a skill out of the library. Refused (409, in words) while an agent uses it; its history stays. */
export const removeSkill = (name: string) => request<{ name: string }>(`/skills/${encodeURIComponent(name)}`, { method: "DELETE" });
export const registerSkill = (skillMd: string, author?: string) =>
  request<SkillRegistration>("/skills", {
    method: "POST",
    body: JSON.stringify({ skill_md: skillMd, references: {}, assets: {}, ...(author ? { author } : {}) }),
  });

/** Where a version came from, as the server records it. */
export type SkillSource =
  | { type: "url"; url: string }
  | { type: "registry"; name: string }
  | { type: "local_path"; path: string }
  | { type: "package"; package_id: string; publisher: string; version: string };

export interface SkillReceipt {
  name: string;
  revision: number;
  content_hash: string;
  provenance: { source: SkillSource; author: string; content_hash: string };
  scan: { clean: boolean; warning_count: number; warnings: unknown[] };
  /** The revision the followers run (a promotion, or an unjudged registration, set it). */
  current?: number;
  latest?: number;
  /** A newer revision held at the gate, when there is one. */
  candidate?: number | null;
  [k: string]: unknown;
}

export const getSkill = (name: string) => request<SkillReceipt>(`/skills/${encodeURIComponent(name)}`);

/** A place the deployment suggests importing skills from. */
export interface SkillLibrarySource {
  id: string;
  name: string;
  url: string;
  description: string;
  publisher: string;
  license?: string | null;
  subpath?: string | null;
}

export const skillLibrary = () =>
  request<{ sources: SkillLibrarySource[] }>("/skills/library").then((r) => r.sources);

/** What an import did, skill by skill. */
export interface SkillImportReport {
  source: string;
  found: number;
  imported: {
    name: string;
    revision: number;
    content_hash: string;
    already_registered: boolean;
    path: string;
    /** Members outside SKILL.md / references/ / assets/ — named, not imported. */
    not_imported: string[];
  }[];
  skipped: { path: string; reason: string; findings?: unknown[] }[];
  oversized: string[];
}

/** A connector described by hand — a system with no OpenAPI document. The
 * server composes and registers the manifest; its operations become tools
 * once a connection is configured. */
export const describeConnector = (draft: unknown) =>
  request<{ id: string; hash: string; registered: boolean; tools: string[] }>("/connectors/describe", {
    method: "POST",
    body: JSON.stringify(draft),
  });

/** Import every skill in a GitHub repository or a .tar.gz, over the
 * server's egress policy. The report says what happened to each one. */
export const importSkills = (body: { url: string; ref?: string; subpath?: string }) =>
  request<SkillImportReport>("/skills/import", { method: "POST", body: JSON.stringify(body) });

// ── The MCP surface Rusty exposes ───────────────────────────────────────────
//
// `/mcp` is the bridge *outward*: every registered graph appears as one MCP
// tool, so another agent can call Rusty. `/mcp/servers` is the other
// direction: MCP servers this server spawns, whose tools mount onto agents.

export interface McpTool { name: string; description: string; inputSchema: Record<string, unknown> }

export const mcpTools = () =>
  request<{ result?: { tools?: McpTool[] } }>("/mcp", {
    method: "POST",
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
  }).then((r) => r.result?.tools ?? []);

export type ToolEffect = "pure" | "read_only" | "idempotent" | "compensatable" | "non_idempotent";
export const TOOL_EFFECTS: ToolEffect[] = ["read_only", "idempotent", "compensatable", "non_idempotent", "pure"];

export interface McpEnvEntry { name: string; value?: string; secret: boolean }

/** One MCP server this server connects to, as served. */
export interface McpServer {
  id: string;
  name: string;
  command: string;
  args: string[];
  env: McpEnvEntry[];
  tool_effects: Record<string, ToolEffect>;
  status: {
    state: "mounted" | "failed" | "not_mounted";
    server: string | null;
    tools: { name: string; tool: string; description: string; effect: ToolEffect }[];
    left_out: string[];
    error: string | null;
  };
  enabled: boolean;
  created_at: string;
  created_by: Attribution | null;
}

export const listMcpServers = () =>
  request<{ enabled: boolean; servers: McpServer[] }>("/mcp/servers");

export interface McpLaunch { command: string; args: string[]; env: { name: string; value: string; secret: boolean }[] }

/** What a server offers, before anything is stored. */
export interface McpProbe {
  server: { name: string; version: string; protocol: string };
  tools: { name: string; description: string; input_schema: Record<string, unknown>; annotations: unknown; suggested_effect: ToolEffect }[];
}

export const probeMcpServer = (launch: McpLaunch) =>
  request<McpProbe>("/mcp/servers/probe", { method: "POST", body: JSON.stringify(launch) });

export const createMcpServer = (body: McpLaunch & { name: string; tool_effects: Record<string, ToolEffect> }) =>
  request<McpServer>("/mcp/servers", { method: "POST", body: JSON.stringify(body) });

export const deleteMcpServer = (id: string) =>
  request<void>(`/mcp/servers/${encodeURIComponent(id)}`, { method: "DELETE" });

export const remountMcpServer = (id: string) =>
  request<McpServer>(`/mcp/servers/${encodeURIComponent(id)}/remount`, { method: "POST" });

/** How a generic API authenticates — the four shapes a manifest renders
 * without a flow of its own, which is the honest boundary of "any HTTP API". */
export type GenericAuth = "bearer" | "basic" | "header" | "query" | "none";

/** One operation an API description offers, before a builder chooses it. */
export interface OfferedOperation { name: string; description: string; method: string; path: string; effect: string }
export interface OpenApiDraft {
  /** Null when the API offers more than one connector holds: `choose` says how many may be picked from `available`. */
  manifest: ConnectorManifest | null;
  available?: OfferedOperation[];
  choose?: { count: number; cap: number };
  unmapped: { path: string; method: string; reason: string }[];
  /** Read-backs the document's own reads made possible, adopted into the
   * draft; and the writes that still guess after a lost answer. */
  read_backs?: { adopted: { write: string; operation: string; arguments: unknown; why: string }[]; still_guessing: string[] };
}

/** Read an OpenAPI document into a draft manifest. Nothing is registered —
 * the draft comes back with what could not be mapped, so a builder registers
 * something they have looked at. */
export const connectorFromOpenApi = (input: {
  id: string;
  display_name: string;
  description: string;
  documentation_url: string;
  base_url: string;
  auth: GenericAuth;
  auth_name?: string;
  /** The operations the builder chose, by name; absent means all. */
  operations?: string[];
  spec: unknown;
}) => request<OpenApiDraft>("/connectors/openapi", { method: "POST", body: JSON.stringify(input) });

/** Begin the consent for a granted connection. Returns the URL to send the
 * person to; nothing is granted until they come back through the callback. */
export const authorizeConnection = (instanceId: string) =>
  request<{ url: string; state: string; expires_in_minutes: number }>(
    `/connectors/instances/${instanceId}/authorize`,
    { method: "POST", body: JSON.stringify({}) },
  );

// ── Knowledge: governed, content-addressed sources agents retrieve from ─────
//
// A source is a body a person or agent posts (text, markdown, json, csv),
// chunked and indexed on the server; a query answers with cited chunks; a
// correction mints a superseding version, the old one stays addressable.
export type KnowledgeScope = { scope: "run" | "agent" | "team" | "user" | "tenant" | string; id: string };
export type KnowledgeRetention = { policy: "pinned" } | { policy: "ttl"; expires_at: string };
export interface KnowledgeSourceSummary {
  source_id: string; scope: KnowledgeScope; kind: "text" | "markdown" | "json" | "csv"; title: string; author: string; confidence: number;
  /** Where its authority comes from; retrieval ranks organization above vendor above generic. Absent = organization. */
  provenance?: "organization" | "vendor" | "generic";
  created_at: string; retention: KnowledgeRetention; content_hash: string; content_bytes: number; version: number; supersedes: string | null; chunk_count: number;
}
export const PROVENANCE_LABEL: Record<string, string> = { organization: "organization policy or record", vendor: "vendor documentation", generic: "generic guidance" };
export interface KnowledgeTombstone { source_id: string; scope: KnowledgeScope; title: string; purged_hashes: string[]; reason: string; purged_at: string }
export interface KnowledgeChunkRecord { chunk_id: string; source_id: string; source_hash: string; chunk_index: number; byte_start: number; byte_end: number; content_address: string; bytes: number; word_count: number }
export interface KnowledgeCitation { source_id: string; source_hash: string; title: string; chunk_id: string; chunk_index: number; content_address: string; byte_start: number; byte_end: number }
/** A suggested edit: an agent proposed the corrected body of a source while working; a person accepts (minting the superseding version) or declines with a reason. */
export interface KnowledgeEdit { edit_id: string; source_id: string; title: string; body: string; why: string; proposed_by: { kind: string; agent_id?: string; run_id?: string }; proposed_at: string; state: "waiting" | "accepted" | "declined" | string; decided_by?: Record<string, unknown> | null; decided_at?: string | null; reason?: string | null; version?: number | null }
export const listKnowledgeEdits = () => request<{ edits: KnowledgeEdit[]; waiting: number }>("/knowledge/edits");
/** Two sources that disagree on a claim; open until a person rules which stands. */
export interface KnowledgeConflict { conflict_id: string; source_a: string; source_b: string; title_a: string; title_b: string; claim: string; a_says: string; b_says: string; why: string; filed_by: { kind?: string; agent_id?: string; run_id?: string; principal_id?: string; name?: string }; filed_at: string; state: "open" | "ruled" | string; stands?: string | null; ruled_by?: Record<string, unknown> | null; ruled_at?: string | null; note?: string | null }
/** An agent's compiled knowledge unit: the chosen sources' text ranked by provenance, carried in its first turn. */
export interface KnowledgeUnit { agent_id: string; version: number; compiled_at: string; compiled_by: Record<string, unknown>; char_limit: number; sources: { source_id: string; title: string; provenance: string; version: number; chars: number; cut: boolean }[]; text: string }
export const knowledgeUnit = (agentId: string) => request<{ unit: KnowledgeUnit | null; chars?: number; stale?: { source_id: string; title: string; why: string }[]; contested?: { conflict_id: string; claim: string; between: string[] }[] }>(`/assistants/${encodeURIComponent(agentId)}/knowledge/unit`);
export const compileKnowledgeUnit = (agentId: string, sourceIds: string[], charLimit?: number) => request<{ unit: KnowledgeUnit; chars: number }>(`/assistants/${encodeURIComponent(agentId)}/knowledge/unit`, { method: "POST", body: JSON.stringify(charLimit ? { source_ids: sourceIds, char_limit: charLimit } : { source_ids: sourceIds }) });
export const removeKnowledgeUnit = (agentId: string) => request<{ removed: boolean }>(`/assistants/${encodeURIComponent(agentId)}/knowledge/unit`, { method: "DELETE" });
export const listKnowledgeConflicts = () => request<{ conflicts: KnowledgeConflict[]; open: number }>("/knowledge/conflicts");
export const fileKnowledgeConflict = (body: { source_a: string; source_b: string; claim: string; a_says: string; b_says: string; why?: string }) => request<{ conflict: KnowledgeConflict; created: boolean }>("/knowledge/conflicts", { method: "POST", body: JSON.stringify(body) });
export const ruleKnowledgeConflict = (id: string, stands: string, note?: string) => request<{ ruled: boolean; conflict: KnowledgeConflict }>(`/knowledge/conflicts/${encodeURIComponent(id)}/rule`, { method: "POST", body: JSON.stringify(note ? { stands, note } : { stands }) });
export const acceptKnowledgeEdit = (id: string) => request<{ accepted: boolean; edit: KnowledgeEdit; version: number }>(`/knowledge/edits/${encodeURIComponent(id)}/accept`, { method: "POST" });
export const declineKnowledgeEdit = (id: string, reason: string) => request<{ declined: boolean; edit: KnowledgeEdit }>(`/knowledge/edits/${encodeURIComponent(id)}/decline`, { method: "POST", body: JSON.stringify({ reason }) });
/** Retire a source now, every version of it: retrieval stops serving it, the bytes go, a tombstone keeps old citations resolvable. */
export const retireKnowledgeSource = (id: string) => request<{ retired: boolean; tombstone: KnowledgeTombstone }>(`/knowledge/sources/${encodeURIComponent(id)}/retire`, { method: "POST" });
export const listKnowledgeSources = () => request<{ sources: KnowledgeSourceSummary[]; tombstones: KnowledgeTombstone[] }>("/knowledge/sources");
export const getKnowledgeSource = (id: string) =>
  request<{ source?: Omit<KnowledgeSourceSummary, "chunk_count"> & { body_hash: string }; versions?: number; chunks?: KnowledgeChunkRecord[]; tombstone?: KnowledgeTombstone }>(`/knowledge/sources/${encodeURIComponent(id)}`);
export const knowledgeChunk = (id: string, chunk: string | number, version?: string) =>
  request<{ citation: KnowledgeCitation; text: string; word_count: number }>(`/knowledge/sources/${encodeURIComponent(id)}/chunks/${encodeURIComponent(String(chunk))}${version ? `?version=${encodeURIComponent(version)}` : ""}`);
/** Read a web page as text through the egress ceiling, to review before indexing it as a source. Nothing is stored. */
export const fetchPageForSource = (url: string) => request<{ url: string; status: number; title?: string | null; text: string; chars: number }>("/knowledge/fetch", { method: "POST", body: JSON.stringify({ url }) });
export const registerKnowledgeSource = (body: { source_id: string; kind: KnowledgeSourceSummary["kind"]; title: string; author: string; body: string; confidence?: number; retention?: KnowledgeRetention; scope?: KnowledgeScope; provenance?: KnowledgeSourceSummary["provenance"] }) =>
  request<{ source_id: string; content_hash: string; version: number; chunk_count: number; created: boolean }>("/knowledge/sources", { method: "POST", body: JSON.stringify(body) });
export const correctKnowledgeSource = (id: string, body: { author: string; body: string }) =>
  request<{ source_id: string; content_hash: string; version: number; supersedes: string | null; chunk_count: number }>(`/knowledge/sources/${encodeURIComponent(id)}/correct`, { method: "POST", body: JSON.stringify(body) });
export const queryKnowledge = (text: string, scope?: KnowledgeScope, limits?: { max_results: number; max_bytes: number }) =>
  request<{ query: string; results: { citation: KnowledgeCitation; text: string; score: number; word_count: number }[] }>("/knowledge/query", { method: "POST", body: JSON.stringify({ text, ...(scope ? { scope } : {}), ...(limits ? { limits } : {}) }) });
export const knowledgeRetentionPlan = (as_of?: string) =>
  request<{ entries: { source_id: string; title: string; version: number; expires_at: string; chunk_count: number; chunk_bytes: number }[]; total_chunk_bytes: number }>("/knowledge/retention/plan", { method: "POST", body: JSON.stringify(as_of ? { as_of } : {}) });
export const knowledgeRetentionApply = (as_of?: string) =>
  request<{ plan: { entries: unknown[]; total_chunk_bytes: number }; tombstones: KnowledgeTombstone[] }>("/knowledge/retention/apply", { method: "POST", body: JSON.stringify(as_of ? { as_of } : {}) });

// ── The release plane: environments, revisions, pointers ───────────────────
//
// A revision freezes what may serve (a graph, optionally an assistant); an
// environment names a surface with a gate and an approval rule; promote moves
// the environment's pointer to a revision; rollback re-points to what served
// before; a canary binds a second revision to a fraction of new runs.
export type DeployAuthor = { type: "human"; human_id: string } | { type: "agent"; agent_id: string };
export interface DeploymentRevision { revision_id: string; content: { graph: string; graph_hash: string; assistant?: string | null; source_environment: string; pins: { surface: string; candidate_id: string }[] }; author: DeployAuthor; created_at: string }
export interface DeploymentEnvironment { name: string; gate?: { policy: string; dataset_version: string } | null; approval_required: boolean; created_by: DeployAuthor; created_at: string }
export interface DeploymentPointer { surface: string; active?: string | null; canary?: { revision_id: string; fraction: number } | null }
export interface DeploymentHealthEntry { environment: string; gate: unknown; approval_required: boolean; active_revision: string | null; canary: { revision_id: string; fraction: number } | null; last_gate_decision: unknown; recent_runs: { active: { runs: number; errored: number; interrupted: number }; canary: { runs: number; errored: number; interrupted: number } } }
export const listEnvironments = () => request<{ environments: DeploymentEnvironment[] }>("/deployments/environments").then((r) => r.environments);
export const declareEnvironment = (body: { name: string; gate?: { policy: string; dataset_version: string }; approval_required?: boolean; author: DeployAuthor }) =>
  request<{ created: boolean; environment: DeploymentEnvironment }>("/deployments/environments", { method: "POST", body: JSON.stringify(body) });
export const listRevisions = () => request<{ revisions: DeploymentRevision[] }>("/deployments/revisions").then((r) => r.revisions);
export const createRevision = (body: { graph: string; assistant?: string; source_environment: string; surfaces?: string[]; author: DeployAuthor }) =>
  request<{ created: boolean; revision: DeploymentRevision }>("/deployments/revisions", { method: "POST", body: JSON.stringify(body) });
export const environmentPointer = (name: string) => request<{ pointer: DeploymentPointer }>(`/deployments/environments/${encodeURIComponent(name)}/pointer`).then((r) => r.pointer);
export const promoteRevision = (name: string, body: { revision_id: string; author: DeployAuthor; approval?: { effect_id: string; approved_by: string } }) =>
  request<{ applied: boolean; journaled: boolean; event_id?: string; pointer: DeploymentPointer }>(`/deployments/environments/${encodeURIComponent(name)}/promote`, { method: "POST", body: JSON.stringify(body) });
export const rollbackEnvironment = (name: string, body: { author: DeployAuthor; cause: string }) =>
  request<{ applied: boolean; journaled: boolean; event_id?: string; pointer: DeploymentPointer }>(`/deployments/environments/${encodeURIComponent(name)}/rollback`, { method: "POST", body: JSON.stringify(body) });
export const declareCanary = (name: string, body: { revision_id: string; fraction: number; author: DeployAuthor }) =>
  request<{ applied: boolean; pointer: DeploymentPointer }>(`/deployments/environments/${encodeURIComponent(name)}/canary`, { method: "PUT", body: JSON.stringify(body) });
export const clearCanary = (name: string, author: DeployAuthor) =>
  request<{ applied: boolean; pointer: DeploymentPointer }>(`/deployments/environments/${encodeURIComponent(name)}/canary`, { method: "DELETE", body: JSON.stringify({ author }) });
export const deploymentsHealth = () => request<{ environments: DeploymentHealthEntry[]; deployment_chain_head: unknown }>("/deployments/health");
export const listEnvSecrets = (environment?: string) => request<{ secrets: { name: string; environment: string; set_by: DeployAuthor; created_at: string; rotated_at?: string | null }[] }>(`/deployments/secrets${environment ? `?environment=${encodeURIComponent(environment)}` : ""}`).then((r) => r.secrets);
export const setEnvSecret = (body: { name: string; environment: string; value: unknown; author: DeployAuthor }) =>
  request<{ created: boolean; record: { name: string; environment: string } }>("/deployments/secrets", { method: "PUT", body: JSON.stringify(body) });
export const revokeEnvSecretNamed = (environment: string, name: string, author: DeployAuthor) =>
  request<void>(`/deployments/secrets/${encodeURIComponent(environment)}/${encodeURIComponent(name)}`, { method: "DELETE", body: JSON.stringify({ author }) });

// ── Quality: experiments, gates, conformance ────────────────────────────────
export interface ExperimentSummary { experiment_id: string; dataset_name: string; dataset_version: string; candidate_id: string; config: { runs_per_case: number; max_concurrency: number; target_metric: string; thresholds: { max_pass_rate_drop: number; max_latency_p95_ratio: number } }; status: { phase: "queued" } | { phase: "running"; completed_runs: number; total_runs: number } | { phase: "complete" } | { phase: "failed"; reason: string } | { phase: "cancelled" }; created_at: string; updated_at: string }
export const listExperiments = () => request<{ experiments: ExperimentSummary[]; truncated: boolean }>("/experiments");
export const getExperiment = (id: string) => request<ExperimentSummary & { comparison?: { regressed: boolean; regressions: unknown[]; latency: { baseline_p95: number; candidate_p95: number; p95_ratio: number | null } } }>(`/experiments/${encodeURIComponent(id)}`);
export interface GateRecord { name: string; blocked_target: string; experiment_id: string; dataset_name: string; dataset_version: string; policy: unknown; decision: unknown; created_at: string }
export const listGates = () => request<{ gates: GateRecord[]; truncated: boolean }>("/gates");
export const listConformanceSuites = () => request<{ suites: { name: string; version: string; created_at: string }[]; truncated: boolean }>("/conformance-suites");
export const listConformanceRuns = () => request<{ runs: { run_id: string; suite_name: string; suite_version: string; target: string; target_version: string; status: string; created_at: string; updated_at: string }[]; truncated: boolean }>("/conformance-runs");
export const conformanceChecks = () => request<{ passing: boolean; run_id?: string | null }>("/conformance-checks");

// ── The runtime fleet: live agent instances, supervision, repairs ───────────
export interface RuntimeAgent { agent_id: string; manifest: { name?: string; [k: string]: unknown }; team_id?: string | null; metadata?: Record<string, unknown>; created_at: string }
export const listRuntimeAgents = () => request<RuntimeAgent[]>("/agents");
export const runtimeAgentStatus = (id: string) => request<Record<string, unknown>>(`/agents/${encodeURIComponent(id)}/status`);
export const runtimeAgentSupervision = (id: string) => request<Record<string, unknown>>(`/agents/${encodeURIComponent(id)}/supervision`);
export const restartRuntimeAgent = (id: string) => request<Record<string, unknown>>(`/agents/${encodeURIComponent(id)}/restart`, { method: "POST", body: "{}" });
export const cancelRuntimeAgent = (id: string) => request<Record<string, unknown>>(`/agents/${encodeURIComponent(id)}/cancel`, { method: "POST", body: "{}" });
export interface RepairRecord { record_id: string; component: unknown; trigger: unknown; action: unknown; outcome: unknown; start_time: string; end_time?: string | null; session_id?: string | null; attempt_count?: number | null; citations?: unknown[] }
export const listRepairs = (limit = 100) => request<RepairRecord[]>(`/repairs?limit=${limit}`);
export const getCoordination = (id: string) => request<Record<string, unknown>>(`/coordination/${encodeURIComponent(id)}`);
