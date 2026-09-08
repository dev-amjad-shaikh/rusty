/**
 * The gateway wire protocol (contracts:gateway-protocol), generated from the
 * Rust types by `gateway_protocol_schema` — do not edit by hand; regenerate with
 * `cargo run -p rusty-agent-server --bin gateway_protocol_schema`.
 */

/** The closed run-admission verdict (`contracts:gateway-protocol`): decided at the exact branch that blocks or admits the run and carried verbatim to clients — never reverse-engineered from errors. Deliberately enumeration-safe about private agents. */
export type AdmissionReason =
  | "queued"
  | "coalesced"
  | "deferred"
  | "runtime_offline"
  | "runtime_unusable"
  | "agent_runtime_required"
  | "attribution_blocked"
  | "already_active"
  | "self_trigger_suppressed"
  | "lease_held"
  | "unauthorized";

/** One wire frame: typed request/response/event with a mandatory idempotency key on side-effecting methods, snapshot-on-connect, and monotonic event sequencing (`contracts:gateway-protocol`). */
export type Frame =
  | { frame: "request"; id: number; idempotency_key?: string | null; method: string; params: unknown; }
  | { frame: "response"; id: number; } & ({ ok: { result: unknown; }; } | { err: { error: unknown; }; })
  | { frame: "event"; name: string; payload: unknown; seq: number; };

/** Device pairing: hello → challenge → signed response → paired (`contracts:gateway-protocol`). Loopback devices may auto-approve; remote devices always require operator approval. Tokens never travel in snapshots. */
export type Pairing =
  | { device_id: string; platform: string; public_key: string; step: "hello"; }
  | { nonce: string; step: "challenge"; }
  | { device_id: string; signature: string; step: "answer"; }
  | { device_token: string; step: "paired"; }
  | { reason: string; step: "denied"; };

/** The flattened payload of `Frame::Response`. */
export type ResponseOutcome =
  | { ok: { result: unknown; }; }
  | { err: { error: unknown; }; };

/** Steering verbs (`contracts:gateway-protocol`). Drain semantics: `Steer` is drained at tool-launch boundaries only — before each sequential launch and once before a parallel batch's atomic launch; a running tool is never cancelled; skipped unstarted calls receive paired synthetic tool results. `Inject` waits silently until a waking message. */
export type SteeringVerb =
  | "followup"
  | "steer"
  | "inject";

/** The exclusive per resolved-session lock serializing load-history → run → flush (`contracts:gateway-protocol`). The lock key is the resolved session ID, never the routing key. */
export interface TurnLease {
  /** When the lease was acquired. */
  acquired_at: string;
  /** When the lease lapses. */
  expires_at: string;
  /** The run holding the lease. */
  holder_run_id: string;
  /** The locked session. */
  session_id: string;
}

