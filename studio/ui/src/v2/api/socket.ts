// Studio v2 gateway WebSocket client (handoff 06, contracts:gateway-protocol).
// Snapshot-on-connect: `hello` → `hello-ok` carrying { protocol, features,
// snapshot, scopes, policy }. Event frames carry a monotonic `seq`; a gap
// discards local state and re-snapshots. Unknown event kinds are delivered
// verbatim — never dropped (they render generically downstream).

import type { Frame } from "../wire";

/** Typed connection state driving the shell banner (R-X1). */
export type ConnectionState =
  | { kind: "connecting" }
  | { kind: "live"; seq: number }
  | { kind: "reconnecting"; lastSeq: number }
  | { kind: "offline"; lastSeq: number };

/** The `hello-ok` handshake payload. `snapshot` and `policy` stay `unknown` here; later phases narrow them from the generated schema. */
export interface HelloOk {
  protocol: number;
  features: string[];
  snapshot: unknown;
  scopes: string[];
  policy: unknown;
}

/** One event frame as delivered to projections. Unknown kinds keep their raw `name`. */
export interface GatewayEvent {
  name: string;
  payload: unknown;
  seq: number;
}

/** Structural subset of the DOM WebSocket so tests can drive a fake. */
export interface WebSocketLike {
  send(data: string): void;
  close(): void;
  onopen: (() => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
}

export interface GatewaySocketOptions {
  url: string;
  socketFactory?: (url: string) => WebSocketLike;
  onSnapshot?: (hello: HelloOk) => void;
  onEvent?: (event: GatewayEvent) => void;
  onState?: (state: ConnectionState) => void;
  /** Reconnect backoff bounds; `false` disables automatic reconnect. */
  reconnect?: { minMs: number; maxMs: number; maxAttempts: number } | false;
  /** Injectable scheduler for backoff — tests advance it manually. */
  schedule?: (fn: () => void, ms: number) => void;
}

export interface GatewaySocket {
  state(): ConnectionState;
  /** Manual retry from the banner; resets the backoff budget. */
  retryNow(): void;
  close(): void;
}

const DEFAULT_RECONNECT = { minMs: 500, maxMs: 8_000, maxAttempts: 10 };

function defaultFactory(url: string): WebSocketLike {
  return new WebSocket(url) as unknown as WebSocketLike;
}

function parseFrame(data: unknown): Frame | null {
  if (typeof data !== "string") return null;
  try {
    const value = JSON.parse(data) as Frame;
    if (value && typeof value === "object" && typeof (value as { frame?: unknown }).frame === "string") return value;
  } catch {
    // unparseable frames are ignored — they carry no trustworthy seq
  }
  return null;
}

function parseHelloOk(result: unknown): HelloOk | null {
  if (!result || typeof result !== "object") return null;
  const value = result as Record<string, unknown>;
  if (typeof value.protocol !== "number" || !Array.isArray(value.features) || !Array.isArray(value.scopes)) return null;
  return {
    protocol: value.protocol,
    features: value.features.filter((feature): feature is string => typeof feature === "string"),
    snapshot: value.snapshot,
    scopes: value.scopes.filter((scope): scope is string => typeof scope === "string"),
    policy: value.policy,
  };
}

export function connectGateway(options: GatewaySocketOptions): GatewaySocket {
  const factory = options.socketFactory ?? defaultFactory;
  const reconnect = options.reconnect === false ? false : { ...DEFAULT_RECONNECT, ...options.reconnect };
  const schedule = options.schedule ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));

  let socket: WebSocketLike | null = null;
  let state: ConnectionState = { kind: "connecting" };
  let lastSeq = 0;
  let expectedSeq: number | null = null;
  let helloId = 0;
  let nextRequestId = 1;
  let attempts = 0;
  let closed = false;

  function setState(next: ConnectionState): void {
    state = next;
    options.onState?.(next);
  }

  function sendHello(): void {
    helloId = nextRequestId++;
    const frame: Frame = { frame: "request", id: helloId, method: "hello", params: { client: "studio-v2" } };
    socket?.send(JSON.stringify(frame));
  }

  function acceptSnapshot(hello: HelloOk): void {
    // The snapshot is the baseline; the next live event must continue its seq.
    const snapshotSeq = (hello.snapshot as { seq?: unknown } | null)?.seq;
    lastSeq = typeof snapshotSeq === "number" ? snapshotSeq : 0;
    expectedSeq = lastSeq + 1;
    options.onSnapshot?.(hello);
    setState({ kind: "live", seq: lastSeq });
  }

  function handleMessage(event: { data: unknown }): void {
    const frame = parseFrame(event.data);
    if (!frame) return;
    if (frame.frame === "response" && frame.id === helloId) {
      if ("ok" in frame) {
        const hello = parseHelloOk(frame.ok.result);
        if (hello) acceptSnapshot(hello);
      }
      return;
    }
    if (frame.frame !== "event") return;
    // Until a snapshot lands, events have no baseline to apply against — they
    // are covered by the snapshot itself, so they are not delivered.
    if (expectedSeq === null) return;
    if (frame.seq !== expectedSeq) {
      // Gap: discard local state and re-snapshot (handoff 06).
      expectedSeq = null;
      setState({ kind: "reconnecting", lastSeq });
      sendHello();
      return;
    }
    lastSeq = frame.seq;
    expectedSeq = frame.seq + 1;
    // Unknown kinds pass through with their raw type string — never dropped.
    options.onEvent?.({ name: frame.name, payload: frame.payload, seq: frame.seq });
    if (state.kind === "live") setState({ kind: "live", seq: lastSeq });
  }

  function handleClose(): void {
    if (closed) return;
    socket = null;
    expectedSeq = null;
    if (reconnect && attempts < reconnect.maxAttempts) {
      attempts += 1;
      setState({ kind: "reconnecting", lastSeq });
      const delay = Math.min(reconnect.maxMs, reconnect.minMs * 2 ** (attempts - 1));
      schedule(open, delay);
    } else {
      setState({ kind: "offline", lastSeq });
    }
  }

  function open(): void {
    if (closed) return;
    if (state.kind !== "reconnecting") setState({ kind: "connecting" });
    socket = factory(options.url);
    socket.onopen = sendHello;
    socket.onmessage = handleMessage;
    socket.onclose = handleClose;
    socket.onerror = () => socket?.close();
  }

  open();

  return {
    state: () => state,
    retryNow() {
      if (closed) return;
      attempts = 0;
      socket?.close();
      socket = null;
      open();
    },
    close() {
      closed = true;
      socket?.close();
      socket = null;
    },
  };
}
