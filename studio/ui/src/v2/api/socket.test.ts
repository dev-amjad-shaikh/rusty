import { describe, expect, it } from "vitest";
import {
  connectGateway,
  type ConnectionState,
  type GatewayEvent,
  type GatewaySocket,
  type HelloOk,
  type WebSocketLike,
} from "./socket";

class FakeSocket implements WebSocketLike {
  static instances: FakeSocket[] = [];
  sent: string[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor() {
    FakeSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.onclose?.();
  }

  open(): void {
    this.onopen?.();
  }

  receive(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
}

function helloOk(seq = 0, scopes: string[] = ["blueprints:read"]) {
  return {
    frame: "response",
    id: 1,
    ok: { result: { protocol: 1, features: ["events"], snapshot: { seq }, scopes, policy: {} } },
  };
}

interface Harness {
  client: GatewaySocket;
  socket: FakeSocket;
  snapshots: HelloOk[];
  events: GatewayEvent[];
  states: ConnectionState[];
}

function connect(options: { reconnect?: false } = {}): Harness {
  FakeSocket.instances = [];
  const snapshots: HelloOk[] = [];
  const events: GatewayEvent[] = [];
  const states: ConnectionState[] = [];
  const client = connectGateway({
    url: "ws://127.0.0.1:8100/gateway",
    socketFactory: () => new FakeSocket(),
    onSnapshot: (hello) => snapshots.push(hello),
    onEvent: (event) => events.push(event),
    onState: (state) => states.push(state),
    reconnect: options.reconnect ?? { minMs: 1, maxMs: 4, maxAttempts: 2 },
    schedule: (fn) => fn(),
  });
  const socket = FakeSocket.instances[0];
  socket.open();
  return { client, socket, snapshots, events, states };
}

describe("v2 gateway socket", () => {
  it("sends hello on open and goes live on hello-ok with snapshot, scopes, policy", () => {
    const harness = connect();
    expect(JSON.parse(harness.socket.sent[0])).toMatchObject({ frame: "request", method: "hello" });
    harness.socket.receive(helloOk(7));
    expect(harness.snapshots).toHaveLength(1);
    expect(harness.snapshots[0].scopes).toEqual(["blueprints:read"]);
    expect(harness.states.at(-1)).toEqual({ kind: "live", seq: 7 });
  });

  it("delivers in-order events with their seq", () => {
    const harness = connect();
    harness.socket.receive(helloOk());
    harness.socket.receive({ frame: "event", name: "TurnStart", payload: {}, seq: 1 });
    harness.socket.receive({ frame: "event", name: "AssistantChunk", payload: { text: "hi" }, seq: 2 });
    expect(harness.events.map((event) => [event.name, event.seq])).toEqual([["TurnStart", 1], ["AssistantChunk", 2]]);
    expect(harness.states.at(-1)).toEqual({ kind: "live", seq: 2 });
  });

  it("on an injected seq gap discards local state, reports reconnecting, and re-snapshots", () => {
    const harness = connect();
    harness.socket.receive(helloOk());
    harness.socket.receive({ frame: "event", name: "TurnStart", payload: {}, seq: 1 });
    // Gap: seq jumps 1 → 5.
    harness.socket.receive({ frame: "event", name: "TurnEnd", payload: {}, seq: 5 });
    expect(harness.states.at(-1)).toEqual({ kind: "reconnecting", lastSeq: 1 });
    // The gap event is never applied; a fresh hello re-snapshots on the same socket.
    expect(harness.events).toHaveLength(1);
    expect(harness.socket.sent.filter((data) => data.includes('"hello"'))).toHaveLength(2);
    // Events arriving before the re-snapshot have no baseline and are dropped.
    harness.socket.receive({ frame: "event", name: "TurnEnd", payload: {}, seq: 6 });
    expect(harness.events).toHaveLength(1);
    harness.socket.receive({ ...helloOk(6), id: 2 });
    expect(harness.snapshots).toHaveLength(2);
    expect(harness.states.at(-1)).toEqual({ kind: "live", seq: 6 });
    harness.socket.receive({ frame: "event", name: "TurnStart", payload: {}, seq: 7 });
    expect(harness.events).toHaveLength(2);
  });

  it("passes unknown event kinds through with their raw type string", () => {
    const harness = connect();
    harness.socket.receive(helloOk());
    harness.socket.receive({ frame: "event", name: "FutureKindNotInV1", payload: { x: 1 }, seq: 1 });
    expect(harness.events).toEqual([{ name: "FutureKindNotInV1", payload: { x: 1 }, seq: 1 }]);
  });

  it("reconnects with backoff after close and goes offline when the budget is spent", () => {
    const harness = connect();
    harness.socket.receive(helloOk(3));
    // maxAttempts is 2: the first two closes schedule a reconnect (schedule
    // runs immediately in tests); the third close exhausts the budget.
    harness.socket.close();
    expect(harness.states.at(-1)).toEqual({ kind: "reconnecting", lastSeq: 3 });
    const second = FakeSocket.instances[1];
    second.open();
    second.close();
    const third = FakeSocket.instances[2];
    third.open();
    third.close();
    expect(harness.states.at(-1)).toEqual({ kind: "offline", lastSeq: 3 });
  });

  it("retryNow resets the backoff budget and reconnects", () => {
    const harness = connect({ reconnect: false });
    harness.socket.receive(helloOk());
    harness.socket.close();
    expect(harness.states.at(-1)).toEqual({ kind: "offline", lastSeq: 0 });
    harness.socket.onclose = null;
    const before = FakeSocket.instances.length;
    harness.client.retryNow();
    expect(FakeSocket.instances.length).toBe(before + 1);
    const reopened = FakeSocket.instances.at(-1)!;
    reopened.open();
    reopened.receive({ ...helloOk(0), id: 2 });
    expect(harness.states.at(-1)).toEqual({ kind: "live", seq: 0 });
  });
});
