/**
 * @rusty-runtime/client — zero-dependency JS/TS client for rusty-server.
 *
 * ESM-only. Works in Node.js >= 18 (global `fetch`, `ReadableStream`,
 * `TextDecoder`) and in modern browsers. No runtime dependencies.
 *
 * The API mirrors the rusty-server HTTP surface (Agent-Protocol-compatible
 * subset): threads, runs (background / blocking / SSE-streamed), checkpoints,
 * fork & replay time travel, assistants, crons, the cross-thread KV store,
 * and the R0.6 durable task queue's control plane (`client.tasks`).
 *
 * @module @rusty-runtime/client
 */

/**
 * Error thrown for any non-2xx response from the server, for network-level
 * failures, and (via the {@link RustyTimeoutError} subclass) for
 * client-side timeouts.
 */
export class RustyError extends Error {
  /**
   * @param {string} message Human-readable message.
   * @param {object} [init]
   * @param {number} [init.status] HTTP status code (0 when no response was received).
   * @param {unknown} [init.body] Parsed response body (JSON value or raw text).
   * @param {string} [init.url] Request URL.
   * @param {unknown} [init.cause] Underlying error, if any.
   */
  constructor(message, init = {}) {
    super(message, init.cause !== undefined ? { cause: init.cause } : undefined);
    this.name = 'RustyError';
    /** @type {number} HTTP status code, or 0 when no response was received. */
    this.status = init.status ?? 0;
    /** @type {unknown} Parsed response body (JSON value or raw text), if any. */
    this.body = init.body;
    /** @type {string|undefined} Request URL. */
    this.url = init.url;
  }

  /**
   * Build an RustyError from a non-ok fetch Response, consuming its body.
   * @param {Response} response
   * @param {string} url
   * @returns {Promise<RustyError>}
   */
  static async fromResponse(response, url) {
    let body;
    const text = await response.text().catch(() => '');
    if (text) {
      try {
        body = JSON.parse(text);
      } catch {
        body = text;
      }
    }
    // The server's error body is {"error": <kind>, "message": <detail>};
    // surface the human-readable message, falling back to the machine kind.
    const detail =
      body && typeof body === 'object' && body !== null
        ? String(
            /** @type {{message?: unknown, error?: unknown}} */ (body).message ??
              /** @type {{message?: unknown, error?: unknown}} */ (body).error ??
              response.statusText,
          )
        : typeof body === 'string' && body
          ? body
          : response.statusText;
    return new RustyError(
      `rusty-server request failed: ${response.status} ${detail}`.trim(),
      { status: response.status, body, url },
    );
  }
}

/**
 * Thrown when a request exceeds the client's (or per-call) timeout.
 * `status` is 0 and `body` is undefined.
 */
export class RustyTimeoutError extends RustyError {
  /**
   * @param {string} url
   * @param {number} timeoutMs
   */
  constructor(url, timeoutMs) {
    super(`rusty-server request timed out after ${timeoutMs} ms`, { status: 0, url });
    this.name = 'RustyTimeoutError';
    /** @type {number} */
    this.timeoutMs = timeoutMs;
  }
}

/**
 * Client for an rusty-server deployment.
 *
 * @example
 * import { RustyClient } from '@rusty-runtime/client';
 *
 * const client = new RustyClient('http://localhost:8100');
 * const { thread_id } = await client.createThread('pipeline');
 * const terminal = await client.runWait(thread_id, {});
 * console.log(terminal.status, terminal.output);
 */
export class RustyClient {
  #baseUrl;
  #apiKey;
  #timeout;
  #fetch;
  #extraHeaders;
  /** @type {TasksClient|undefined} lazily built on first `.tasks` access */
  #tasksClient;

  /**
   * @param {string} baseUrl Server base URL, e.g. `"http://localhost:8100"`.
   *   A trailing slash is stripped.
   * @param {object} [options]
   * @param {string} [options.apiKey] Static API key sent as the `X-Api-Key`
   *   header. Required only when the server is configured with one.
   * @param {number} [options.timeout=30000] Per-request timeout in
   *   milliseconds. For `runStream` the timeout covers establishing the
   *   stream (until response headers arrive), not the stream lifetime.
   *   Set to `0` to disable.
   * @param {typeof fetch} [options.fetch] Custom fetch implementation
   *   (defaults to the global `fetch`). Useful for tests and proxies.
   * @param {Record<string, string>} [options.headers] Extra headers sent on
   *   every request.
   */
  constructor(baseUrl, options = {}) {
    if (typeof baseUrl !== 'string' || baseUrl.length === 0) {
      throw new TypeError('RustyClient: baseUrl must be a non-empty string');
    }
    this.#baseUrl = baseUrl.replace(/\/+$/, '');
    this.#apiKey = options.apiKey;
    this.#timeout = options.timeout ?? 30_000;
    this.#fetch = options.fetch ?? globalThis.fetch?.bind(globalThis);
    if (typeof this.#fetch !== 'function') {
      throw new Error(
        'RustyClient: no fetch implementation available. ' +
          'Use Node.js >= 18, a modern browser, or pass { fetch }.',
      );
    }
    this.#extraHeaders = options.headers ?? {};
  }

  /** @returns {string} The normalized server base URL. */
  get baseUrl() {
    return this.#baseUrl;
  }

  /**
   * The durable task queue's control plane (R0.6): submit, observe, and
   * cancel tasks. Returns a cached {@link TasksClient} bound to this
   * client's transport (same base URL, API key, timeout, and fetch).
   * @returns {TasksClient}
   */
  get tasks() {
    if (!this.#tasksClient) {
      // The sub-client shares the parent's private transport through a
      // bound closure — one request path, one error mapping, no second
      // client to configure.
      this.#tasksClient = new TasksClient((method, path, opts) =>
        this.#request(method, path, opts),
      );
    }
    return this.#tasksClient;
  }

  /**
   * Perform an HTTP request and parse a JSON response.
   * @param {string} method
   * @param {string} path Path beginning with `/`.
   * @param {object} [opts]
   * @param {unknown} [opts.body] JSON-serializable request body.
   * @param {Record<string, string>} [opts.headers] Per-request headers.
   * @param {number} [opts.timeout] Per-request timeout override (ms).
   * @param {AbortSignal} [opts.signal] Caller-supplied abort signal.
   * @param {boolean} [opts.raw] Return the raw Response instead of parsed JSON.
   * @returns {Promise<any>}
   */
  async #request(method, path, opts = {}) {
    const url = this.#baseUrl + path;
    const controller = new AbortController();
    const timeoutMs = opts.timeout ?? this.#timeout;
    let timedOut = false;
    const timer =
      timeoutMs > 0
        ? setTimeout(() => {
            timedOut = true;
            controller.abort();
          }, timeoutMs)
        : null;
    const callerSignal = opts.signal;
    const onCallerAbort = () => controller.abort();
    if (callerSignal) {
      if (callerSignal.aborted) controller.abort();
      else callerSignal.addEventListener('abort', onCallerAbort, { once: true });
    }

    const headers = { ...this.#extraHeaders, ...(opts.headers ?? {}) };
    if (this.#apiKey != null) headers['x-api-key'] = this.#apiKey;
    if (opts.body !== undefined) headers['content-type'] = 'application/json';

    try {
      const response = await this.#fetch(url, {
        method,
        headers,
        body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
        signal: controller.signal,
      });
      if (!response.ok) throw await RustyError.fromResponse(response, url);
      if (opts.raw) {
        // For raw (streaming) responses the timeout only covered connection
        // setup; hand the open stream to the caller.
        if (timer) clearTimeout(timer);
        if (callerSignal) callerSignal.removeEventListener('abort', onCallerAbort);
        return response;
      }
      if (response.status === 204) return null;
      const text = await response.text();
      return text ? JSON.parse(text) : null;
    } catch (err) {
      if (timedOut) throw new RustyTimeoutError(url, timeoutMs);
      if (err instanceof RustyError) throw err;
      if (err instanceof Error && err.name === 'AbortError') {
        throw new RustyError('rusty-server request aborted', {
          status: 0,
          url,
          cause: err,
        });
      }
      if (err instanceof TypeError) {
        // fetch rejects with TypeError for network-level failures (DNS,
        // refused connection, TLS) before any response exists.
        throw new RustyError(`rusty-server network error: ${err.message}`, {
          status: 0,
          url,
          cause: err,
        });
      }
      throw err;
    } finally {
      if (timer) clearTimeout(timer);
      if (callerSignal) callerSignal.removeEventListener('abort', onCallerAbort);
    }
  }

  // ------------------------------------------------------------------ meta

  /**
   * Liveness probe.
   * @returns {Promise<{ok: boolean}>}
   */
  async ok() {
    return this.#request('GET', '/ok');
  }

  /**
   * Service metadata: version, checkpointer kind, store path, and the
   * registered graphs with their state channels.
   * @returns {Promise<import('./index.d.ts').InfoResponse>}
   */
  async info() {
    return this.#request('GET', '/info');
  }

  // --------------------------------------------------------------- threads

  /**
   * Create a thread bound to a registered graph.
   * @param {string} graph Registered graph name.
   * @param {object} [opts]
   * @param {Record<string, unknown>} [opts.metadata] Free-form thread metadata.
   * @param {string} [opts.threadId] Explicit thread id (server-generated
   *   UUID when omitted). Re-using an existing id re-attaches to its
   *   persisted checkpoints.
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').Thread>}
   */
  async createThread(graph, opts = {}) {
    const body = { graph };
    if (opts.metadata !== undefined) body.metadata = opts.metadata;
    if (opts.threadId !== undefined) body.thread_id = opts.threadId;
    return this.#request('POST', '/threads', { body, signal: opts.signal });
  }

  /**
   * Latest checkpoint of a thread: `{ values, next, checkpoint }`.
   * @param {string} threadId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').ThreadState>}
   */
  async getState(threadId, opts = {}) {
    return this.#request('GET', `/threads/${encodeURIComponent(threadId)}/state`, {
      signal: opts.signal,
    });
  }

  /**
   * Write a new checkpoint (the `update_state` analog).
   * @param {string} threadId
   * @param {Record<string, unknown>} values Channel values to write.
   * @param {object} [opts]
   * @param {string} [opts.asNode] Attribute the update to this node.
   * @param {string[]} [opts.nextNodes] Override the next-node set.
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').ThreadState>} The written checkpoint state.
   */
  async updateState(threadId, values, opts = {}) {
    const body = { values };
    if (opts.asNode !== undefined) body.as_node = opts.asNode;
    if (opts.nextNodes !== undefined) body.next_nodes = opts.nextNodes;
    return this.#request('POST', `/threads/${encodeURIComponent(threadId)}/state`, {
      body,
      signal: opts.signal,
    });
  }

  /**
   * Checkpoint history, newest first.
   * @param {string} threadId
   * @param {object} [opts]
   * @param {number} [opts.limit] Max checkpoints to return.
   * @param {string} [opts.before] Only return checkpoints before this checkpoint id.
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').ThreadState[]>}
   */
  async history(threadId, opts = {}) {
    const body = {};
    if (opts.limit !== undefined) body.limit = opts.limit;
    if (opts.before !== undefined) body.before = opts.before;
    return this.#request('POST', `/threads/${encodeURIComponent(threadId)}/history`, {
      body,
      signal: opts.signal,
    });
  }

  /**
   * Fork a thread's checkpoint history into a new thread (time travel).
   * @param {string} threadId Source thread id.
   * @param {object} [opts]
   * @param {string} [opts.newThreadId] Explicit id for the fork
   *   (server-generated UUID when omitted).
   * @param {string} [opts.checkpointId] Copy only up to and including this
   *   checkpoint (mid-history fork). Omit for a full-history fork.
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').ForkResult>}
   */
  async fork(threadId, opts = {}) {
    const body = {};
    if (opts.newThreadId !== undefined) body.new_thread_id = opts.newThreadId;
    if (opts.checkpointId !== undefined) body.checkpoint_id = opts.checkpointId;
    return this.#request('POST', `/threads/${encodeURIComponent(threadId)}/fork`, {
      body,
      signal: opts.signal,
    });
  }

  // ------------------------------------------------------------------ runs

  /**
   * Start a background run. Returns immediately with the run record.
   * @param {string} threadId
   * @param {import('./index.d.ts').RunPayload} [payload]
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').RunRecord>} Contains `run_id`.
   */
  async run(threadId, payload = {}, opts = {}) {
    return this.#request('POST', `/threads/${encodeURIComponent(threadId)}/runs`, {
      body: payload,
      signal: opts.signal,
    });
  }

  /**
   * Run to completion and return the terminal JSON
   * (`{status, output | interrupt, …}`).
   * @param {string} threadId
   * @param {import('./index.d.ts').RunPayload} [payload]
   * @param {object} [opts]
   * @param {number} [opts.timeout] Timeout override (ms). Defaults to the
   *   client timeout; raise it for long-running graphs.
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').RunTerminal>}
   */
  async runWait(threadId, payload = {}, opts = {}) {
    return this.#request('POST', `/threads/${encodeURIComponent(threadId)}/runs/wait`, {
      body: payload,
      timeout: opts.timeout,
      signal: opts.signal,
    });
  }

  /**
   * Run with SSE streaming. Yields one frame per server event
   * (`metadata`, `updates`, `values`, `messages`, `error`, `end`).
   *
   * @param {string} threadId
   * @param {import('./index.d.ts').RunPayload} [payload]
   * @param {object} [options]
   * @param {string|number} [options.lastEventId] Resume position: sent as the
   *   `Last-Event-ID` header; the server replays the run's event-log tail
   *   after this frame id before streaming live frames.
   * @param {string[]} [options.streamMode] Convenience for
   *   `payload.stream_mode` (e.g. `["updates", "values"]`).
   * @param {number} [options.timeout] Timeout (ms) for establishing the
   *   stream; the open stream itself is not timed.
   * @param {AbortSignal} [options.signal] Abort the stream at any time.
   * @returns {AsyncGenerator<import('./index.d.ts').StreamFrame, void, void>}
   *
   * @example
   * for await (const frame of client.runStream(threadId, { input })) {
   *   if (frame.event === 'end') console.log('done:', frame.data.status);
   * }
   */
  async *runStream(threadId, payload = {}, options = {}) {
    const body = { ...payload };
    const streamMode = options.streamMode ?? options.stream_mode;
    if (streamMode !== undefined && body.stream_mode === undefined) {
      body.stream_mode = streamMode;
    }
    const headers = { accept: 'text/event-stream' };
    if (options.lastEventId !== undefined && options.lastEventId !== null) {
      headers['last-event-id'] = String(options.lastEventId);
    }
    const response = await this.#request(
      'POST',
      `/threads/${encodeURIComponent(threadId)}/runs/stream`,
      { body, headers, raw: true, timeout: options.timeout, signal: options.signal },
    );
    if (!response.body) {
      throw new RustyError('rusty-server stream response has no body', {
        status: response.status,
        url: response.url,
      });
    }
    yield* parseSseStream(response.body, options.signal);
  }

  /**
   * Poll a run's status. Terminal runs also carry `output` / `error` /
   * `interrupt` fields.
   * @param {string} runId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').RunStatus>}
   */
  async runStatus(runId, opts = {}) {
    return this.#request('GET', `/runs/${encodeURIComponent(runId)}`, { signal: opts.signal });
  }

  /**
   * Fetch a run's Flight Recorder journal: the journaled `RunEvent`s in
   * `seq` order plus a `complete` flag (`true` once the run is terminal,
   * i.e. the served snapshot is the final journal; while active it trails
   * the live journal by at most one checkpoint boundary).
   * @param {string} runId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').RunEventsResponse>}
   */
  async runEvents(runId, opts = {}) {
    return this.#request('GET', `/runs/${encodeURIComponent(runId)}/events`, {
      signal: opts.signal,
    });
  }

  /**
   * Download a run as a portable replay fixture: the integrity-verified
   * journal, the graph's topology hash, the final checkpoint, and provenance
   * metadata — feed it to `ReplayFixture::import` to re-drive the run in CI.
   * (`404` unknown run; `409` before the first persisted journal.)
   * @param {string} runId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').ReplayFixture>}
   */
  async getFixture(runId, opts = {}) {
    return this.#request('GET', `/runs/${encodeURIComponent(runId)}/fixture`, {
      signal: opts.signal,
    });
  }

  /**
   * Re-drive a journaled run server-side and verify the replayed evidence:
   * the server re-executes the run's registered graph against the persisted
   * journal (zero outbound calls) and compares it event-for-event against
   * the recording. (`404` unknown run; `409` no persisted journal or the run
   * is still executing; `422` the graph is not registered in the server
   * process, or the journal carries recorded model/tool calls — those
   * replay through the CI fixture instead.)
   * @param {string} runId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').ReplayReport>}
   */
  async replayRun(runId, opts = {}) {
    return this.#request('POST', '/runs/replay', {
      body: { run_id: runId },
      signal: opts.signal,
    });
  }

  /**
   * Structural diff of two runs' journals — typically two branches forked
   * from one point. Events compare logically (identity/timing excluded), so
   * the shared prefix reads as equal; a run diffed against itself reports
   * `first_divergent_seq: null`. (`404` unknown run on either side; `409`
   * when either run has no persisted journal yet.)
   * @param {string} base base run id (the branch is diffed against it)
   * @param {string} branch branch run id
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').BranchDiff>}
   */
  async diffRuns(base, branch, opts = {}) {
    const query = new URLSearchParams({ base, branch });
    return this.#request('GET', `/runs/diff?${query}`, { signal: opts.signal });
  }

  /**
   * Roll back a finished run: delete its checkpoints and re-anchor the thread
   * to the pre-run checkpoint. (`409` while the run is active.)
   * @param {string} threadId
   * @param {string} runId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<unknown>}
   */
  async deleteRun(threadId, runId, opts = {}) {
    return this.#request(
      'DELETE',
      `/threads/${encodeURIComponent(threadId)}/runs/${encodeURIComponent(runId)}`,
      { signal: opts.signal },
    );
  }

  // ------------------------------------------------------------ assistants

  /**
   * Create a named graph alias (assistant).
   * @param {object} assistant
   * @param {string} assistant.name Human-readable name.
   * @param {string} assistant.graph Registered graph the assistant binds to.
   * @param {Record<string, unknown>} [assistant.config] Default run config
   *   (e.g. `{ recursion_limit: 25 }`).
   * @param {Record<string, unknown>} [assistant.metadata]
   * @param {string} [assistant.assistantId] Explicit id (UUID when omitted).
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').Assistant>}
   */
  async createAssistant(assistant, opts = {}) {
    const body = { name: assistant.name, graph: assistant.graph };
    if (assistant.config !== undefined) body.config = assistant.config;
    if (assistant.metadata !== undefined) body.metadata = assistant.metadata;
    if (assistant.assistantId !== undefined) body.assistant_id = assistant.assistantId;
    return this.#request('POST', '/assistants', { body, signal: opts.signal });
  }

  /**
   * List all assistants.
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').Assistant[]>}
   */
  async listAssistants(opts = {}) {
    return this.#request('GET', '/assistants', { signal: opts.signal });
  }

  /**
   * Fetch one assistant by id.
   * @param {string} assistantId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').Assistant>}
   */
  async getAssistant(assistantId, opts = {}) {
    return this.#request('GET', `/assistants/${encodeURIComponent(assistantId)}`, {
      signal: opts.signal,
    });
  }

  // ----------------------------------------------------------------- crons

  /**
   * Schedule recurring runs. Exactly one schedule kind is required:
   * `intervalSecs` (fixed interval, >= 1 s) or `cronExpr` (5-field cron, UTC).
   * @param {object} cron
   * @param {string} cron.graph Registered graph to run.
   * @param {number} [cron.intervalSecs] Fixed interval in seconds.
   * @param {string} [cron.cronExpr] 5-field cron expression (UTC).
   * @param {Record<string, unknown>} [cron.input] Run input for fired runs.
   * @param {Record<string, unknown>} [cron.metadata]
   * @param {'keep'|'delete'} [cron.onRunCompleted] `"delete"` makes the cron
   *   a one-shot: it removes itself after its first fired run terminates.
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').Cron>}
   */
  async createCron(cron, opts = {}) {
    const body = { graph: cron.graph };
    if (cron.intervalSecs !== undefined) body.interval_secs = cron.intervalSecs;
    if (cron.cronExpr !== undefined) body.cron_expr = cron.cronExpr;
    if (cron.input !== undefined) body.input = cron.input;
    if (cron.metadata !== undefined) body.metadata = cron.metadata;
    if (cron.onRunCompleted !== undefined) body.on_run_completed = cron.onRunCompleted;
    return this.#request('POST', '/crons', { body, signal: opts.signal });
  }

  /**
   * List crons (with `runs_fired` / `last_run_at` bookkeeping).
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').Cron[]>}
   */
  async listCrons(opts = {}) {
    return this.#request('GET', '/crons', { signal: opts.signal });
  }

  /**
   * Delete a cron (`404` when unknown).
   * @param {string} cronId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<unknown>}
   */
  async deleteCron(cronId, opts = {}) {
    return this.#request('DELETE', `/crons/${encodeURIComponent(cronId)}`, {
      signal: opts.signal,
    });
  }

  // ------------------------------------------------------------------- kv

  /**
   * Fetch one item from the cross-thread KV store (`404` when absent).
   * @param {string} namespace Namespace segment (`[A-Za-z0-9._-]`, 1–128 chars).
   * @param {string} key Key segment (same character rules).
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').StoreItem>}
   */
  async kvGet(namespace, key, opts = {}) {
    return this.#request(
      'GET',
      `/store/${encodeURIComponent(namespace)}/${encodeURIComponent(key)}`,
      { signal: opts.signal },
    );
  }

  /**
   * Upsert a JSON value in a namespace. `201` on create, `200` on replace
   * (the server's `created_at` is preserved on replace).
   * @param {string} namespace
   * @param {string} key
   * @param {unknown} value Any JSON-serializable value.
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').StoreItem>}
   */
  async kvPut(namespace, key, value, opts = {}) {
    return this.#request(
      'PUT',
      `/store/${encodeURIComponent(namespace)}/${encodeURIComponent(key)}`,
      { body: value, signal: opts.signal },
    );
  }

  /**
   * Delete one item (`404` when absent).
   * @param {string} namespace
   * @param {string} key
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<unknown>}
   */
  async kvDelete(namespace, key, opts = {}) {
    return this.#request(
      'DELETE',
      `/store/${encodeURIComponent(namespace)}/${encodeURIComponent(key)}`,
      { signal: opts.signal },
    );
  }

  /**
   * List a namespace's items, sorted by key (empty array for an unwritten
   * namespace).
   * @param {string} namespace
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').StoreItem[]>}
   */
  async kvList(namespace, opts = {}) {
    return this.#request('GET', `/store/${encodeURIComponent(namespace)}`, {
      signal: opts.signal,
    });
  }
}

/**
 * Control-plane client for the R0.6 durable task queue.
 *
 * Obtained as `client.tasks`; it shares the parent client's transport, so
 * base URL, API key, timeout, and any custom `fetch` apply unchanged.
 * Covers the operations a controlling application needs:
 *
 * - {@link TasksClient#enqueue} / {@link TasksClient#enqueueOutbox} — submit work.
 * - {@link TasksClient#get} / {@link TasksClient#list} — observe records
 *   (`list({ status: 'dead' })` is the dead-letter queue).
 * - {@link TasksClient#cancel} / {@link TasksClient#cancelRunTasks} — cancellation.
 *
 * Why `claim` / `heartbeat` / `complete` / `fail` are deliberately absent:
 * those four endpoints are the queue's *worker-machine* half — lease-guarded
 * by `worker_id`, they exist so a worker process can hold, renew, and settle
 * exactly one lease at a time. A control-plane caller that claimed a lease
 * would either sit on it (starving real workers until the visibility timeout
 * reclaims the task) or race a real worker's settlement into 409s. That
 * surface belongs to the worker SDK (`rusty-worker`'s `ActivityWorker`);
 * this client never holds leases.
 */
export class TasksClient {
  /** @type {(method: string, path: string, opts?: object) => Promise<any>} */
  #transport;

  /**
   * @param {(method: string, path: string, opts?: object) => Promise<any>} transport
   *   The parent client's request function. Not part of the public API —
   *   use `client.tasks` instead of constructing this directly.
   */
  constructor(transport) {
    this.#transport = transport;
  }

  /**
   * Build the enqueue body shared by `POST /tasks` and `POST /tasks/outbox`:
   * camelCase options map onto the wire's snake_case; unset options are
   * omitted so server defaults apply (pool `default`, max_attempts 3).
   * @param {string} kind
   * @param {unknown} payload
   * @param {import('./index.d.ts').EnqueueTaskOptions} opts
   */
  static #enqueueBody(kind, payload, opts) {
    const body = { kind, payload };
    if (opts.pool !== undefined) body.pool = opts.pool;
    if (opts.maxAttempts !== undefined) body.max_attempts = opts.maxAttempts;
    if (opts.idempotencyKey !== undefined) body.idempotency_key = opts.idempotencyKey;
    if (opts.effect !== undefined) body.effect = opts.effect;
    if (opts.runId !== undefined) body.run_id = opts.runId;
    if (opts.threadId !== undefined) body.thread_id = opts.threadId;
    if (opts.deadline !== undefined) body.deadline = opts.deadline;
    return body;
  }

  /**
   * Enqueue a durable task (`POST /tasks`).
   * @param {string} kind Work classification the worker fleet dispatches on
   *   (free-form, e.g. `"send_email"`).
   * @param {unknown} payload Work payload — any JSON-serializable value,
   *   stored verbatim.
   * @param {import('./index.d.ts').EnqueueTaskOptions} [opts]
   * @returns {Promise<import('./index.d.ts').EnqueueTaskResult>}
   *   `{ task_id, deduplicated }` — `deduplicated` is `true` when the
   *   idempotency key already named a live task (HTTP 200) rather than a
   *   fresh one being created (HTTP 201); the two cases differ in meaning,
   *   not in shape, so the status code folds into the boolean.
   */
  async enqueue(kind, payload, opts = {}) {
    return this.#transport('POST', '/tasks', {
      body: TasksClient.#enqueueBody(kind, payload, opts),
      signal: opts.signal,
    });
  }

  /**
   * Enqueue through the transactional outbox (`POST /tasks/outbox` → 202
   * accepted). Same arguments as {@link TasksClient#enqueue}. The task is
   * written to the outbox and becomes claimable only when the relay
   * publishes it into the queue (within one poll interval). Delivery is
   * at-least-once: the relay dedupes on the task's idempotency key, so a
   * crash anywhere in the pipe neither loses nor doubles the task. Prefer
   * this when the submission must commit atomically with other state;
   * prefer `enqueue` when the task should be claimable immediately.
   * @param {string} kind
   * @param {unknown} payload
   * @param {import('./index.d.ts').EnqueueTaskOptions} [opts]
   * @returns {Promise<import('./index.d.ts').EnqueueTaskResult>}
   */
  async enqueueOutbox(kind, payload, opts = {}) {
    return this.#transport('POST', '/tasks/outbox', {
      body: TasksClient.#enqueueBody(kind, payload, opts),
      signal: opts.signal,
    });
  }

  /**
   * Fetch one task record (`GET /tasks/{id}`; `404` for unknown or
   * cross-tenant ids).
   * @param {string} taskId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').TaskRecord>}
   */
  async get(taskId, opts = {}) {
    return this.#transport('GET', `/tasks/${encodeURIComponent(taskId)}`, {
      signal: opts.signal,
    });
  }

  /**
   * List the tenant's tasks, oldest first (`GET /tasks`).
   * @param {object} [opts]
   * @param {import('./index.d.ts').TaskStatus} [opts.status] Lifecycle
   *   filter; `"dead"` is the dead-letter queue. The server answers 400
   *   for an unknown status rather than silently returning everything.
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').TaskRecord[]>}
   */
  async list(opts = {}) {
    const query = opts.status !== undefined
      ? `?${new URLSearchParams({ status: opts.status })}`
      : '';
    return this.#transport('GET', `/tasks${query}`, { signal: opts.signal });
  }

  /**
   * Cancel a non-terminal task (`POST /tasks/{id}/cancel`). Queued and
   * retry-scheduled tasks move to the terminal `cancelled` state
   * immediately; a leased task keeps its lease with `cancel_requested`
   * set, so its holder learns on the next heartbeat and reports the
   * attempt as cancelled. Cancellation is a hint for promptness — lease
   * expiry stays the correctness mechanism. `409` when the task is
   * already terminal, `404` for unknown or cross-tenant ids.
   * @param {string} taskId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').TaskRecord>} The updated record.
   */
  async cancel(taskId, opts = {}) {
    return this.#transport('POST', `/tasks/${encodeURIComponent(taskId)}/cancel`, {
      signal: opts.signal,
    });
  }

  /**
   * Cancel every non-terminal task belonging to a run
   * (`POST /runs/{run_id}/cancel`). Returns task ids split by how each
   * cancellation landed: `cancelled` (moved terminal immediately) versus
   * `signalled` (leased; holders learn via `cancel_requested`). Scope:
   * this is the queue half of run cancellation — a task enqueued *after*
   * the call is not retroactively cancelled. `404` for unknown or
   * cross-tenant runs.
   * @param {string} runId
   * @param {object} [opts]
   * @param {AbortSignal} [opts.signal]
   * @returns {Promise<import('./index.d.ts').RunCancellation>}
   */
  async cancelRunTasks(runId, opts = {}) {
    return this.#transport('POST', `/runs/${encodeURIComponent(runId)}/cancel`, {
      signal: opts.signal,
    });
  }
}

/**
 * Incrementally parse an SSE byte stream into frames.
 *
 * Handles the full SSE field grammar: `event`, multi-line `data` (joined with
 * `\n`), `id`, `retry`, and `:` comment/keepalive lines; both `\n` and `\r\n`
 * line endings. Frame payloads are JSON-parsed when possible, otherwise
 * yielded as raw strings.
 *
 * @param {ReadableStream<Uint8Array>} body
 * @param {AbortSignal} [signal]
 * @returns {AsyncGenerator<import('./index.d.ts').StreamFrame, void, void>}
 */
export async function* parseSseStream(body, signal) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let event = 'message';
  /** @type {string[]} */
  let dataLines = [];
  let id;
  let retry;

  const onAbort = () => reader.cancel().catch(() => {});
  if (signal) {
    if (signal.aborted) {
      reader.cancel().catch(() => {});
    } else {
      signal.addEventListener('abort', onAbort, { once: true });
    }
  }

  /** Take one dispatched frame, if the event buffer holds one. */
  const dispatch = () => {
    if (dataLines.length === 0) {
      // Comment-only or empty block: reset and emit nothing.
      event = 'message';
      id = undefined;
      retry = undefined;
      return undefined;
    }
    const raw = dataLines.join('\n');
    /** @type {unknown} */
    let data = raw;
    try {
      data = JSON.parse(raw);
    } catch {
      /* not JSON — yield the raw string */
    }
    const frame = { event, data };
    if (id !== undefined) frame.id = id;
    if (retry !== undefined) frame.retry = retry;
    event = 'message';
    dataLines = [];
    id = undefined;
    retry = undefined;
    return frame;
  };

  /** @type {Array<import('./index.d.ts').StreamFrame|undefined>} */
  const pending = [];
  const processLine = (line) => {
    if (line.endsWith('\r')) line = line.slice(0, -1);
    if (line === '') {
      pending.push(dispatch());
      return;
    }
    if (line.startsWith(':')) return; // comment / keepalive
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    switch (field) {
      case 'event':
        event = value;
        break;
      case 'data':
        dataLines.push(value);
        break;
      case 'id':
        if (!value.includes(String.fromCharCode(0))) id = value; // NUL in id kills the field per spec
        break;
      case 'retry': {
        const n = Number.parseInt(value, 10);
        if (Number.isFinite(n)) retry = n;
        break;
      }
      default:
        break; // unknown fields are ignored per spec
    }
  };

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let nl;
      while ((nl = buffer.indexOf('\n')) !== -1) {
        processLine(buffer.slice(0, nl));
        buffer = buffer.slice(nl + 1);
      }
      while (pending.length > 0) {
        const frame = pending.shift();
        if (frame) yield frame;
      }
    }
    buffer += decoder.decode();
    if (buffer.length > 0) processLine(buffer);
    pending.push(dispatch());
    while (pending.length > 0) {
      const frame = pending.shift();
      if (frame) yield frame;
    }
  } finally {
    if (signal) signal.removeEventListener('abort', onAbort);
    // A consumer that breaks out of `for await` early lands here with the
    // HTTP body still streaming; cancel it so the connection is torn down
    // instead of draining until the server finishes the run. (No-op after
    // a normal EOF or an abort, which already cancels via onAbort.)
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

export default RustyClient;
