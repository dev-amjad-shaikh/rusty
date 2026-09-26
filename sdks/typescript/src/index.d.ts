/**
 * @rusty-runtime/client — TypeScript declarations.
 *
 * Zero-dependency JS/TS client for rusty-server (HTTP + SSE).
 * Works in Node.js >= 18 and modern browsers.
 */

/** JSON-serializable value. */
export type JsonValue =
  | string
  | number
  | boolean
  | null
  | JsonValue[]
  | { [key: string]: JsonValue };

/** Client constructor options. */
export interface RustyClientOptions {
  /** Static API key sent as the `X-Api-Key` header. */
  apiKey?: string;
  /**
   * Per-request timeout in milliseconds (default 30000).
   * For `runStream`, covers establishing the stream only. `0` disables.
   */
  timeout?: number;
  /** Custom fetch implementation (defaults to global `fetch`). */
  fetch?: typeof fetch;
  /** Extra headers sent on every request. */
  headers?: Record<string, string>;
}

/** Error thrown for non-2xx responses and network-level failures. */
export class RustyError extends Error {
  /** HTTP status code, or 0 when no response was received. */
  status: number;
  /** Parsed response body (JSON value or raw text), if any. */
  body: unknown;
  /** Request URL. */
  url?: string;
  constructor(
    message: string,
    init?: { status?: number; body?: unknown; url?: string; cause?: unknown },
  );
  static fromResponse(response: Response, url: string): Promise<RustyError>;
}

/** Thrown when a request exceeds the client or per-call timeout. */
export class RustyTimeoutError extends RustyError {
  timeoutMs: number;
  constructor(url: string, timeoutMs: number);
}

/** One registered graph as reported by `GET /info`. */
export interface GraphInfo {
  name: string;
  channels: string[];
}

/** `GET /info` response. */
export interface InfoResponse {
  service: string;
  version: string;
  checkpointer: string;
  server_store?: string;
  store_path: string;
  graphs: GraphInfo[];
}

/** Thread record returned by `POST /threads`. */
export interface Thread {
  thread_id: string;
  graph: string;
  metadata: Record<string, JsonValue> | null;
  created_at: string;
}

/** Checkpoint metadata. */
export interface Checkpoint {
  checkpoint_id: string;
  thread_id: string;
  step: number;
  created_at: string;
}

/** Thread state snapshot: `{ values, next, checkpoint }`. */
export interface ThreadState {
  values: Record<string, JsonValue>;
  next: string[];
  checkpoint: Checkpoint;
}

/** `POST /threads/{id}/fork` response. */
export interface ForkResult {
  thread_id: string;
  checkpoints_copied: number;
}

/** Run-create payload (subset of the LangGraph Platform shape). */
export interface RunPayload {
  /** Input state for the run (e.g. `{ messages: [...] }`). */
  input?: Record<string, JsonValue>;
  /** Human-in-the-loop resume channel. */
  command?: { resume?: JsonValue };
  config?: { recursion_limit?: number } & Record<string, JsonValue>;
  /** Time travel: replay from this checkpoint of the thread. */
  checkpoint?: { checkpoint_id: string };
  metadata?: Record<string, JsonValue>;
  /** SSE frame families for streaming runs. Default `["values", "updates"]`. */
  stream_mode?: Array<
    'values' | 'updates' | 'messages' | 'metadata' | 'error' | 'end' | (string & {})
  >;
  multitask_strategy?: 'enqueue' | 'reject';
  /** Run through a named assistant (must bind the thread's graph). */
  assistant_id?: string;
  [key: string]: unknown;
}

/** Background-run record returned by `POST /threads/{id}/runs`. */
export interface RunRecord {
  run_id: string;
  thread_id: string;
  status: string;
  [key: string]: unknown;
}

/** Terminal result of a blocking run. */
export interface RunTerminal {
  status: 'success' | 'interrupted' | 'error' | (string & {});
  output?: Record<string, JsonValue>;
  interrupt?: JsonValue;
  error?: string;
  checkpoint_id?: string;
  state?: Record<string, JsonValue>;
  run_id?: string;
  thread_id?: string;
  [key: string]: unknown;
}

/** `GET /runs/{run_id}` response. Terminal runs carry output/error/interrupt. */
export interface RunStatus {
  run_id: string;
  thread_id: string;
  graph: string;
  attempt: number;
  status: 'pending' | 'running' | 'success' | 'interrupted' | 'error' | (string & {});
  output?: Record<string, JsonValue>;
  error?: string;
  interrupt?: JsonValue;
  [key: string]: unknown;
}

/** One parsed SSE frame from a streaming run. */
export interface StreamFrame {
  /** Frame family: `metadata`, `updates`, `values`, `messages`, `error`, `end`. */
  event: string;
  /** JSON-parsed payload, or the raw data string when not JSON. */
  data: any;
  /** Frame id: `{checkpoint_id}:{step}:{seq}` (seq is 1-based per run). */
  id?: string;
  /** Server-suggested reconnection delay (ms), when present. */
  retry?: number;
}

// ---------------------------------------------------------------------
// Flight Recorder (GET /runs/{run_id}/events)
// ---------------------------------------------------------------------

/** The effect taxonomy: what a journaled event did to the world. */
export type Effect =
  | 'pure'
  | 'read_only'
  | 'idempotent'
  | 'compensatable'
  | 'non_idempotent';

/** What a {@link RunEvent} records (closed set). */
export type RunEventKind =
  | 'super_step_start'
  | 'super_step_end'
  | 'node_input'
  | 'node_output'
  | 'model_call'
  | 'tool_call'
  | 'remote_call'
  | 'wasm_call'
  | 'interrupt'
  | 'resume'
  | 'routing_decision'
  | 'checkpoint_written';

/** The outcome status of a journaled event. */
export type EventStatus = 'ok' | 'error' | 'interrupted';

/** A content-addressed reference to an oversized payload. */
export interface ArtifactRef {
  /** Lowercase hex SHA-256 of the payload's canonical JSON serialization. */
  sha256: string;
  /** Serialized size of the payload in bytes. */
  bytes: number;
}

/** How an event's input or output payload is carried (adjacently tagged). */
export type PayloadRef =
  | { kind: 'inline'; value: JsonValue }
  | { kind: 'artifact'; value: ArtifactRef };

/** Token usage reported for a model call. */
export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

/**
 * One recorded fact about a run: the Flight Recorder's atomic evidence.
 * Events form a total order via `seq` and a causal chain via `parent`;
 * ids are deterministic (`{run_id}:{seq}`).
 */
export interface RunEvent {
  id: string;
  run_id: string;
  thread_id: string;
  node_id: string | null;
  seq: number;
  kind: RunEventKind;
  effect: Effect;
  input: PayloadRef | null;
  output: PayloadRef | null;
  latency_ms: number | null;
  tokens: TokenUsage | null;
  cost_usd: number | null;
  status: EventStatus;
  parent: string | null;
  recorded_at: string;
}

/** `GET /runs/{run_id}/events` response. */
export interface RunEventsResponse {
  run_id: string;
  /** The journaled events in `seq` order. */
  events: RunEvent[];
  /**
   * `true` once the run is terminal — the served snapshot is the final
   * journal. While active it trails the live journal by at most one
   * checkpoint boundary.
   */
  complete: boolean;
}

/** A checkpoint as carried by a replay fixture (`thread_id` is external). */
export interface FixtureCheckpoint {
  id: string;
  thread_id: string;
  step: number;
  state: Record<string, JsonValue>;
  next_nodes: string[];
  created_at: string;
  [key: string]: unknown;
}

/** Provenance and determinism metadata of a {@link ReplayFixture}. */
export interface FixtureMetadata {
  /** Human-readable fixture name. */
  name: string;
  /** The recorded run's logical-clock parameters (CI replay only). */
  clock?: { start_ms: number; tick_ms: number };
  /** The recorded run's RNG seed (CI replay only). */
  rng_seed?: number;
}

/**
 * `GET /runs/{run_id}/fixture` response: a portable exact-replay bundle
 * (core's `ReplayFixture` envelope). Feed the JSON to
 * `ReplayFixture::import` to re-drive the run in CI.
 */
export interface ReplayFixture {
  /** Fixture envelope version (`1` for anything written now). */
  format_version: number;
  /** SHA-256 of the recorded graph's topology. */
  graph_hash: string;
  /** The application-declared graph version of the recorded run. */
  graph_version: string;
  /** The recorded run's complete journal. */
  journal: {
    run_id: string;
    thread_id: string;
    events: RunEvent[];
    artifacts: Record<string, JsonValue>;
    head_hash: string;
  };
  /** The recorded run's final checkpoint, when it ran with a checkpointer. */
  final_checkpoint: FixtureCheckpoint | null;
  metadata: FixtureMetadata;
}

/**
 * `POST /runs/replay` response (exact shape — consumers, including the
 * Studio, key off these five fields).
 */
export interface ReplayReport {
  run_id: string;
  /**
   * `true` when the replayed evidence reproduces the recorded journal:
   * same kinds, nodes, sequences, effect classes, statuses, and payloads
   * (per-run minted checkpoint ids and wall-clock measurements excluded).
   */
  verified: boolean;
  /** Number of events in the recorded journal. */
  expected_events: number;
  /** Number of events the replay journaled. */
  actual_events: number;
  /** The `seq` of the first disagreeing event, or `null` when identical. */
  first_divergence: number | null;
}

/** Token and cost totals of one branch of a {@link BranchDiff}. */
export interface BranchTotals {
  /** Number of events in the branch. */
  events: number;
  /** Summed token usage across the branch's model calls. */
  tokens: TokenUsage;
  /** Summed recorded cost in USD across the branch's events. */
  cost_usd: number;
}

/** One channel's differing value at one super-step of a {@link BranchDiff}. */
export interface ChannelDiff {
  /** The state channel that differs. */
  channel: string;
  /** The post-reducer value in the base branch, when present. */
  base: JsonValue | null;
  /** The post-reducer value in the branch, when present. */
  branch: JsonValue | null;
}

/** The state-channel differences of one super-step between two branches. */
export interface StepDiff {
  /** The super-step index. */
  step: number;
  /** Channels whose post-reducer values differ, sorted by name. */
  channels: ChannelDiff[];
}

/**
 * `GET /runs/diff?base=&branch=` response (core's `BranchDiff` serde
 * shape): the structural diff of two runs' journals. Events compare
 * logically — identity and timing fields excluded — so branches forked
 * from one point show their shared prefix as equal.
 */
export interface BranchDiff {
  /** The first `seq` where the branches differ, or `null` when identical. */
  first_divergent_seq: number | null;
  /** Branch events at and after the divergence point (its new work). */
  added: RunEvent[];
  /** Base events at and after the divergence point (the replaced work). */
  removed: RunEvent[];
  /** State-channel differences per super-step (steps with diffs only). */
  step_diffs: StepDiff[];
  /** Totals over the whole base journal. */
  base_totals: BranchTotals;
  /** Totals over the whole branch journal. */
  branch_totals: BranchTotals;
}

/** Assistant record: a named graph + config alias. */
export interface Assistant {
  assistant_id: string;
  name: string;
  graph: string;
  config?: Record<string, JsonValue> | null;
  metadata?: Record<string, JsonValue> | null;
  created_at?: string;
  [key: string]: unknown;
}

/** Cron record. */
export interface Cron {
  cron_id: string;
  graph: string;
  interval_secs?: number;
  cron_expr?: string;
  input?: Record<string, JsonValue> | null;
  metadata?: Record<string, JsonValue> | null;
  on_run_completed?: 'keep' | 'delete';
  runs_fired?: number;
  last_run_at?: string | null;
  [key: string]: unknown;
}

/** One KV-store item. */
export interface StoreItem {
  namespace: string;
  key: string;
  value: JsonValue;
  created_at: string;
  updated_at: string;
}

// ---------------------------------------------------------------------
// Durable task queue (R0.6) — control plane via `client.tasks`
// ---------------------------------------------------------------------

/** Task lifecycle status. `dead` is the dead-letter queue. */
export type TaskStatus =
  | 'queued'
  | 'leased'
  | 'failed'
  | 'completed'
  | 'dead'
  | 'cancelled';

/**
 * The closed failure taxonomy a failed attempt is classified under
 * (core's `ErrorClass`, snake_case on the wire), shared verbatim with the
 * worker SDK so both sides of the queue agree.
 */
export type TaskErrorClass =
  | 'transient'
  | 'rate_limited'
  | 'timeout'
  | 'invalid_input'
  | 'dependency_failure'
  | 'resource_exhausted'
  | 'cancelled'
  | 'unknown';

/** A live lease: the worker allowed to settle the task, and its expiry. */
export interface TaskLease {
  owner: string;
  expires_at: string;
}

/**
 * The provider's confirmation of an idempotent effect, reported on task
 * completion and journaled into the task's run as an `effect_receipt`
 * event (core's `EffectReceipt`).
 */
export interface EffectReceipt {
  /** The system that confirmed the effect (e.g. `stripe`, `sendgrid`). */
  provider: string;
  /** The provider's own confirmation id (charge id, message id, …). */
  provider_id: string;
  /** The idempotency key the effect was performed under. */
  idempotency_key: string;
  /** The durable task whose completion produced this receipt, if any. */
  task_id?: string;
}

/**
 * One durable task (the server's wire record; `tenant` is an internal
 * isolation detail and never appears on the wire).
 *
 * The record carries the *last* attempt's error (`error_class` +
 * `last_error`); per-attempt history is not on the HTTP surface. A
 * `failed` task with `next_attempt_at` set has a retry scheduled (and is
 * non-terminal); with it `null` the task failed outright — terminal, but
 * not dead-lettered.
 */
export interface TaskRecord {
  task_id: string;
  kind: string;
  payload: JsonValue;
  pool: string;
  status: TaskStatus;
  /** 1-based number of the current/last attempt (0 = never claimed). */
  attempt: number;
  max_attempts: number;
  error_class: TaskErrorClass | null;
  /** The enqueuer's declared effect classification, when supplied. */
  effect: Effect | null;
  last_error: string | null;
  idempotency_key: string | null;
  /** The completed task's result. */
  result: JsonValue | null;
  receipt: EffectReceipt | null;
  run_id: string | null;
  thread_id: string | null;
  /** Cancellation signalled to the lease holder (see `cancel`). */
  cancel_requested: boolean;
  /** Whole-task deadline (RFC 3339), across attempts. */
  deadline: string | null;
  /** Present exactly while `status === 'leased'`. */
  lease: TaskLease | null;
  /** When a `failed` task becomes claimable again (`null` otherwise). */
  next_attempt_at: string | null;
  created_at: string;
  updated_at: string;
}

/** Options for {@link TasksClient.enqueue} and {@link TasksClient.enqueueOutbox}. */
export interface EnqueueTaskOptions {
  /** Named pool (default `"default"`); workers claim from named pools. */
  pool?: string;
  /** Attempt ceiling before dead-lettering (server default 3, max 100). */
  maxAttempts?: number;
  /**
   * Dedup key, unique per tenant across live tasks: re-enqueueing with
   * the same key returns the existing task (`deduplicated: true`).
   */
  idempotencyKey?: string;
  /**
   * Declared effect classification of the work. A declared non-repeatable
   * effect is never silently retried; when absent, the worker's
   * per-attempt `retryable` flag decides.
   */
  effect?: Effect;
  /** Run this task belongs to — `cancelRunTasks` cancels every non-terminal task carrying it. */
  runId?: string;
  /** Thread linkage (companion to `runId`). */
  threadId?: string;
  /** Whole-task deadline as an RFC 3339 timestamp, across attempts. */
  deadline?: string;
  /** Abort the request. */
  signal?: AbortSignal;
}

/** `POST /tasks` / `POST /tasks/outbox` response. */
export interface EnqueueTaskResult {
  task_id: string;
  /**
   * `true` when the idempotency key already named a live task (HTTP 200)
   * rather than a fresh one being created (HTTP 201).
   */
  deduplicated: boolean;
}

/** `POST /runs/{run_id}/cancel` response: task ids split by landing. */
export interface RunCancellation {
  run_id: string;
  /** Tasks moved to the terminal `cancelled` state immediately. */
  cancelled: string[];
  /** Leased tasks whose holders were signalled via `cancel_requested`. */
  signalled: string[];
}

/**
 * Control-plane client for the R0.6 durable task queue, obtained as
 * `client.tasks`. Submit (`enqueue` / `enqueueOutbox`), observe (`get` /
 * `list`), and cancel (`cancel` / `cancelRunTasks`). The worker-machine
 * operations (`claim` / `heartbeat` / `complete` / `fail`) are
 * deliberately absent — they are lease-guarded by `worker_id` and belong
 * to the worker SDK; a control-plane client never holds leases.
 */
export declare class TasksClient {
  /** Enqueue a durable task (`POST /tasks`). */
  enqueue(
    kind: string,
    payload: JsonValue,
    opts?: EnqueueTaskOptions,
  ): Promise<EnqueueTaskResult>;

  /** Enqueue through the transactional outbox (`POST /tasks/outbox` → 202). */
  enqueueOutbox(
    kind: string,
    payload: JsonValue,
    opts?: EnqueueTaskOptions,
  ): Promise<EnqueueTaskResult>;

  /** Fetch one task record (`404` when unknown or cross-tenant). */
  get(taskId: string, opts?: RequestOptions): Promise<TaskRecord>;

  /** List the tenant's tasks, oldest first; `status: 'dead'` is the DLQ. */
  list(opts?: { status?: TaskStatus } & RequestOptions): Promise<TaskRecord[]>;

  /**
   * Cancel a non-terminal task (`409` when already terminal). A leased
   * task is signalled via `cancel_requested` rather than forced.
   */
  cancel(taskId: string, opts?: RequestOptions): Promise<TaskRecord>;

  /** Cancel every non-terminal task belonging to a run (`404` unknown run). */
  cancelRunTasks(runId: string, opts?: RequestOptions): Promise<RunCancellation>;
}

/** Options accepted by most read methods. */
export interface RequestOptions {
  signal?: AbortSignal;
}

/** Options for {@link RustyClient.runWait}. */
export interface RunWaitOptions extends RequestOptions {
  /** Timeout override (ms) — raise for long-running graphs. */
  timeout?: number;
}

/** Options for {@link RustyClient.runStream}. */
export interface RunStreamOptions {
  /** Resume position sent as the `Last-Event-ID` header. */
  lastEventId?: string | number;
  /** Convenience for `payload.stream_mode`. */
  streamMode?: string[];
  /** snake_case alias of `streamMode` (accepted; `streamMode` wins when both are set). */
  stream_mode?: string[];
  /** Timeout (ms) for establishing the stream; the open stream is not timed. */
  timeout?: number;
  /** Abort the stream at any time. */
  signal?: AbortSignal;
}

/** Options for {@link RustyClient.createThread}. */
export interface CreateThreadOptions extends RequestOptions {
  metadata?: Record<string, JsonValue>;
  /** Explicit thread id (server-generated UUID when omitted). */
  threadId?: string;
}

/** Options for {@link RustyClient.updateState}. */
export interface UpdateStateOptions extends RequestOptions {
  /** Attribute the update to this node. */
  asNode?: string;
  /** Override the next-node set. */
  nextNodes?: string[];
}

/** Options for {@link RustyClient.history}. */
export interface HistoryOptions extends RequestOptions {
  limit?: number;
  /** Only return checkpoints before this checkpoint id. */
  before?: string;
}

/** Options for {@link RustyClient.fork}. */
export interface ForkOptions extends RequestOptions {
  /** Explicit id for the fork (server-generated UUID when omitted). */
  newThreadId?: string;
  /** Copy only up to and including this checkpoint (mid-history fork). */
  checkpointId?: string;
}

/** Input for {@link RustyClient.createAssistant}. */
export interface CreateAssistantInput {
  name: string;
  graph: string;
  config?: Record<string, JsonValue>;
  metadata?: Record<string, JsonValue>;
  assistantId?: string;
}

/** Input for {@link RustyClient.createCron}. Exactly one schedule kind. */
export interface CreateCronInput {
  graph: string;
  /** Fixed interval in seconds (>= 1). */
  intervalSecs?: number;
  /** 5-field cron expression (UTC). */
  cronExpr?: string;
  input?: Record<string, JsonValue>;
  metadata?: Record<string, JsonValue>;
  /** `"delete"` turns the cron into a one-shot. */
  onRunCompleted?: 'keep' | 'delete';
}

export declare class RustyClient {
  constructor(baseUrl: string, options?: RustyClientOptions);

  /** The normalized server base URL. */
  readonly baseUrl: string;

  /** The durable task queue's control plane (R0.6). */
  readonly tasks: TasksClient;

  /** Liveness probe. */
  ok(): Promise<{ ok: boolean }>;

  /** Service metadata: version, checkpointer, store path, registered graphs. */
  info(): Promise<InfoResponse>;

  /** Create a thread bound to a registered graph. */
  createThread(graph: string, opts?: CreateThreadOptions): Promise<Thread>;

  /** Latest checkpoint of a thread. */
  getState(threadId: string, opts?: RequestOptions): Promise<ThreadState>;

  /** Write a new checkpoint (the `update_state` analog). */
  updateState(
    threadId: string,
    values: Record<string, JsonValue>,
    opts?: UpdateStateOptions,
  ): Promise<ThreadState>;

  /** Checkpoint history, newest first. */
  history(threadId: string, opts?: HistoryOptions): Promise<ThreadState[]>;

  /** Fork a thread's checkpoint history into a new thread (time travel). */
  fork(threadId: string, opts?: ForkOptions): Promise<ForkResult>;

  /** Start a background run; resolves with the run record (`run_id`). */
  run(threadId: string, payload?: RunPayload, opts?: RequestOptions): Promise<RunRecord>;

  /** Run to completion; resolves with the terminal JSON. */
  runWait(
    threadId: string,
    payload?: RunPayload,
    opts?: RunWaitOptions,
  ): Promise<RunTerminal>;

  /**
   * Run with SSE streaming. Yields `metadata` / `updates` / `values` /
   * `messages` / `error` / `end` frames.
   */
  runStream(
    threadId: string,
    payload?: RunPayload,
    options?: RunStreamOptions,
  ): AsyncGenerator<StreamFrame, void, void>;

  /** Poll a run's status. */
  runStatus(runId: string, opts?: RequestOptions): Promise<RunStatus>;

  /** Fetch a run's Flight Recorder journal (`{run_id, events, complete}`). */
  runEvents(runId: string, opts?: RequestOptions): Promise<RunEventsResponse>;

  /** Download a run as a portable replay fixture (`ReplayFixture` envelope). */
  getFixture(runId: string, opts?: RequestOptions): Promise<ReplayFixture>;

  /** Re-drive a journaled run server-side and verify the replayed evidence. */
  replayRun(runId: string, opts?: RequestOptions): Promise<ReplayReport>;

  /** Structural diff of two runs' journals (core's `BranchDiff` shape). */
  diffRuns(base: string, branch: string, opts?: RequestOptions): Promise<BranchDiff>;

  /** Roll back a finished run's checkpoints. */
  deleteRun(threadId: string, runId: string, opts?: RequestOptions): Promise<unknown>;

  /** Create a named graph alias. */
  createAssistant(input: CreateAssistantInput, opts?: RequestOptions): Promise<Assistant>;

  /** List all assistants. */
  listAssistants(opts?: RequestOptions): Promise<Assistant[]>;

  /** Fetch one assistant by id. */
  getAssistant(assistantId: string, opts?: RequestOptions): Promise<Assistant>;

  /** Schedule recurring runs (interval or cron expression). */
  createCron(input: CreateCronInput, opts?: RequestOptions): Promise<Cron>;

  /** List crons with `runs_fired` / `last_run_at` bookkeeping. */
  listCrons(opts?: RequestOptions): Promise<Cron[]>;

  /** Delete a cron (`404` when unknown). */
  deleteCron(cronId: string, opts?: RequestOptions): Promise<unknown>;

  /** Fetch one KV item (`404` when absent). */
  kvGet(namespace: string, key: string, opts?: RequestOptions): Promise<StoreItem>;

  /** Upsert a JSON value in a namespace. */
  kvPut(
    namespace: string,
    key: string,
    value: JsonValue,
    opts?: RequestOptions,
  ): Promise<StoreItem>;

  /** Delete one KV item (`404` when absent). */
  kvDelete(namespace: string, key: string, opts?: RequestOptions): Promise<unknown>;

  /** List a namespace's items, sorted by key. */
  kvList(namespace: string, opts?: RequestOptions): Promise<StoreItem[]>;
}

/**
 * Incrementally parse an SSE byte stream into frames. Exported for reuse and
 * testing; `runStream` uses it internally.
 */
export declare function parseSseStream(
  body: ReadableStream<Uint8Array>,
  signal?: AbortSignal,
): AsyncGenerator<StreamFrame, void, void>;

export default RustyClient;
