// Studio v2 REST client (handoff 06): side-effecting POSTs carry an
// `Idempotency-Key`, collections paginate by cursor, and errors arrive as
// RFC 9457 problem JSON. Reads are safe to retry; mutations are retried only
// by the caller with the same idempotency key the first attempt used.

const DEFAULT_LIMIT = 8 * 1024 * 1024;

/** RFC 9457 problem-details object, with extension members preserved. */
export interface ProblemDetails {
  type: string;
  title: string;
  status: number;
  detail?: string;
  instance?: string;
  [extension: string]: unknown;
}

export class ProblemError extends Error {
  readonly problem: ProblemDetails;
  /** True when the request may have committed server-side (network failure or 5xx on a mutation). */
  readonly mayHaveCommitted: boolean;

  constructor(problem: ProblemDetails, mayHaveCommitted = false) {
    super(problem.detail ?? problem.title);
    this.name = "ProblemError";
    this.problem = problem;
    this.mayHaveCommitted = mayHaveCommitted;
  }

  get status(): number {
    return this.problem.status;
  }
}

/** The publish endpoint answers 428 when the idempotency key is missing (handoff 06). */
export function isMissingIdempotencyKey(error: unknown): boolean {
  return error instanceof ProblemError && error.status === 428;
}

/** Cursor-paginated collection envelope (handoff 06: "collections paginate by cursor"). */
export interface CursorPage<T> {
  items: T[];
  next_cursor: string | null;
}

export interface RestClientOptions {
  /** URL prefix for API paths; defaults to the dev-server proxy mount. */
  baseUrl?: string;
  fetchImpl?: typeof fetch;
  apiKey?: string;
  /** Key source for side-effecting POSTs; defaults to crypto.randomUUID. */
  newIdempotencyKey?: () => string;
  /** Response body safety cap in bytes. */
  maxBytes?: number;
}

export interface MutationResult<T> {
  status: number;
  data: T;
  /** The key the attempt carried — re-issue retries with this exact key. */
  idempotencyKey: string;
}

export interface RestClient {
  get<T>(path: string, options?: { cursor?: string | null; limit?: number; signal?: AbortSignal }): Promise<T>;
  page<T>(path: string, options?: { cursor?: string | null; limit?: number }): Promise<CursorPage<T>>;
  /** Follows `next_cursor` until the collection is exhausted, yielding each item. */
  paginate<T>(path: string, options?: { limit?: number }): AsyncGenerator<T, void, undefined>;
  post<T>(path: string, body: unknown, options?: { idempotencyKey?: string; signal?: AbortSignal }): Promise<MutationResult<T>>;
  put<T>(path: string, body: unknown, options?: { idempotencyKey?: string; signal?: AbortSignal }): Promise<MutationResult<T>>;
  patch<T>(path: string, body: unknown, options?: { idempotencyKey?: string; signal?: AbortSignal }): Promise<MutationResult<T>>;
}

function defaultIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return `idem-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 14)}`;
}

async function readBounded(response: Response, maxBytes: number): Promise<string> {
  const reader = response.body?.getReader();
  if (!reader) return "";
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > maxBytes) {
      await reader.cancel();
      throw new ProblemError({
        type: "about:blank",
        title: "Response too large",
        status: response.status,
        detail: `Response exceeded the ${Math.floor(maxBytes / 1024)} KiB safety boundary.`,
      });
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new ProblemError({
      type: "about:blank",
      title: "Invalid encoding",
      status: response.status,
      detail: "The server returned invalid UTF-8.",
    });
  }
}

function isProblemDetails(value: unknown): value is ProblemDetails {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Record<string, unknown>;
  return typeof candidate.type === "string" && typeof candidate.title === "string" && typeof candidate.status === "number";
}

function problemFrom(text: string, status: number): ProblemDetails {
  try {
    const value: unknown = JSON.parse(text);
    if (isProblemDetails(value)) return value;
  } catch {
    // fall through to the synthesized problem below
  }
  return {
    type: "about:blank",
    title: status >= 500 ? "Server error" : "Request failed",
    status,
    detail: `Rusty returned HTTP ${status}.`,
  };
}

export function createRestClient(options: RestClientOptions = {}): RestClient {
  const baseUrl = options.baseUrl ?? "/api";
  const fetchImpl = options.fetchImpl ?? fetch;
  const maxBytes = options.maxBytes ?? DEFAULT_LIMIT;
  const newKey = options.newIdempotencyKey ?? defaultIdempotencyKey;

  async function request(path: string, init: RequestInit, mutation: boolean): Promise<{ status: number; text: string }> {
    let response: Response;
    try {
      response = await fetchImpl(`${baseUrl}${path}`, {
        ...init,
        headers: {
          Accept: "application/json",
          ...(init.body !== undefined ? { "Content-Type": "application/json" } : {}),
          ...(options.apiKey ? { "X-Api-Key": options.apiKey } : {}),
          ...init.headers,
        },
      });
    } catch {
      throw new ProblemError(
        { type: "about:blank", title: "Unreachable", status: 0, detail: "Rusty could not be reached." },
        mutation,
      );
    }
    let text: string;
    try {
      text = await readBounded(response, maxBytes);
    } catch (caught) {
      if (mutation && caught instanceof ProblemError) {
        throw new ProblemError(caught.problem, true);
      }
      throw caught;
    }
    if (!response.ok) {
      throw new ProblemError(problemFrom(text, response.status), mutation && (response.status >= 500 || response.status === 408));
    }
    return { status: response.status, text };
  }

  function withQuery(path: string, query: { cursor?: string | null; limit?: number }): string {
    const params = new URLSearchParams();
    if (query.cursor) params.set("cursor", query.cursor);
    if (query.limit !== undefined) params.set("limit", String(query.limit));
    const suffix = params.toString();
    return suffix ? `${path}${path.includes("?") ? "&" : "?"}${suffix}` : path;
  }

  function parse<T>(text: string, context: string): T {
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new ProblemError({
        type: "about:blank",
        title: "Malformed response",
        status: 0,
        detail: `${context} was not valid JSON.`,
      });
    }
  }

  async function mutate<T>(
    method: string,
    path: string,
    body: unknown,
    mutateOptions: { idempotencyKey?: string; signal?: AbortSignal } = {},
  ): Promise<MutationResult<T>> {
    const idempotencyKey = mutateOptions.idempotencyKey ?? newKey();
    const { status, text } = await request(path, {
      method,
      body: JSON.stringify(body),
      signal: mutateOptions.signal,
      headers: { "Idempotency-Key": idempotencyKey },
    }, true);
    return { status, data: parse<T>(text, `${method} ${path}`), idempotencyKey };
  }

  async function get<T>(path: string, getOptions: { cursor?: string | null; limit?: number; signal?: AbortSignal } = {}): Promise<T> {
    const { text } = await request(withQuery(path, getOptions), { method: "GET", signal: getOptions.signal }, false);
    return parse<T>(text, `GET ${path}`);
  }

  async function* paginate<T>(path: string, paginateOptions: { limit?: number } = {}): AsyncGenerator<T, void, undefined> {
    let cursor: string | null = null;
    do {
      const page: CursorPage<T> = await get<CursorPage<T>>(path, { cursor, limit: paginateOptions.limit });
      for (const item of page.items) yield item;
      cursor = page.next_cursor;
    } while (cursor !== null);
  }

  return {
    get,
    page: get,
    paginate,
    post<T>(path: string, body: unknown, postOptions: { idempotencyKey?: string; signal?: AbortSignal } = {}): Promise<MutationResult<T>> {
      return mutate<T>("POST", path, body, postOptions);
    },
    put<T>(path: string, body: unknown, putOptions: { idempotencyKey?: string; signal?: AbortSignal } = {}): Promise<MutationResult<T>> {
      return mutate<T>("PUT", path, body, putOptions);
    },
    patch<T>(path: string, body: unknown, patchOptions: { idempotencyKey?: string; signal?: AbortSignal } = {}): Promise<MutationResult<T>> {
      return mutate<T>("PATCH", path, body, patchOptions);
    },
  };
}
