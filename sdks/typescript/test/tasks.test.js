/**
 * TasksClient unit tests (no I/O) — the R0.6 durable task queue's control
 * plane, exercised against a fake `fetch` (the same seam the SSE parser
 * unit tests in client.test.js use). Kept in a separate file from the e2e
 * suite so these run without booting `server_demo`.
 *
 * The wire shapes asserted here are read from rusty-server/src/routes.rs
 * (enqueue_task, get_task, list_tasks, cancel_task, cancel_run) — the
 * server API is the frozen contract.
 *
 * Run from the repo root:  node --test sdks/typescript/test/tasks.test.js
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { RustyClient, RustyError, TasksClient } from '../src/index.js';

const TASK_RECORD = {
  task_id: 't-123',
  kind: 'send_email',
  payload: { to: 'a@b.c' },
  pool: 'default',
  status: 'queued',
  attempt: 0,
  max_attempts: 3,
  error_class: null,
  effect: 'idempotent',
  last_error: null,
  idempotency_key: 'order-42',
  result: null,
  receipt: null,
  run_id: null,
  thread_id: null,
  cancel_requested: false,
  deadline: null,
  lease: null,
  next_attempt_at: null,
  created_at: '2026-08-10T09:00:00Z',
  updated_at: '2026-08-10T09:00:00Z',
};

/**
 * A fake fetch that records every call and answers `response` (or throws
 * it, when given an Error). A Response is cloned per call — bodies are
 * single-use, and several tests exercise two requests through one fake.
 */
function recordingFetch(response) {
  const calls = [];
  const fetch = async (url, init) => {
    calls.push({ url, init });
    if (response instanceof Error) throw response;
    return response instanceof Response ? response.clone() : response;
  };
  return { calls, fetch };
}

const jsonResponse = (status, body) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });

test('client.tasks returns a cached TasksClient bound to the parent', () => {
  const client = new RustyClient('http://unit.test', { fetch: async () => null });
  assert.ok(client.tasks instanceof TasksClient);
  assert.equal(client.tasks, client.tasks, 'the accessor caches one instance');
});

test('enqueue() posts kind + payload, omitting unset options', async () => {
  const { calls, fetch } = recordingFetch(
    jsonResponse(201, { task_id: 't-1', deduplicated: false }),
  );
  const client = new RustyClient('http://unit.test', { fetch });

  const out = await client.tasks.enqueue('send_email', { to: 'a@b.c' });

  assert.deepEqual(out, { task_id: 't-1', deduplicated: false });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, 'http://unit.test/tasks');
  assert.equal(calls[0].init.method, 'POST');
  assert.deepEqual(JSON.parse(calls[0].init.body), {
    kind: 'send_email',
    payload: { to: 'a@b.c' },
  });
});

test('enqueue() maps every option camelCase → snake_case', async () => {
  const { calls, fetch } = recordingFetch(
    jsonResponse(200, { task_id: 't-2', deduplicated: true }),
  );
  const client = new RustyClient('http://unit.test', { fetch });

  const out = await client.tasks.enqueue(
    'charge_card',
    { amount: 100 },
    {
      pool: 'billing',
      maxAttempts: 5,
      idempotencyKey: 'order-42',
      effect: 'idempotent',
      runId: 'r-9',
      threadId: 'th-7',
      deadline: '2026-08-11T00:00:00Z',
    },
  );

  // The dedup case (HTTP 200) folds into the boolean — same shape.
  assert.equal(out.deduplicated, true);
  assert.deepEqual(JSON.parse(calls[0].init.body), {
    kind: 'charge_card',
    payload: { amount: 100 },
    pool: 'billing',
    max_attempts: 5,
    idempotency_key: 'order-42',
    effect: 'idempotent',
    run_id: 'r-9',
    thread_id: 'th-7',
    deadline: '2026-08-11T00:00:00Z',
  });
});

test('enqueueOutbox() uses the outbox route with the same body shape', async () => {
  const { calls, fetch } = recordingFetch(
    jsonResponse(202, { task_id: 't-3', deduplicated: false }),
  );
  const client = new RustyClient('http://unit.test', { fetch });

  await client.tasks.enqueueOutbox('reindex', { shard: 3 });

  assert.equal(calls[0].url, 'http://unit.test/tasks/outbox');
  assert.deepEqual(JSON.parse(calls[0].init.body), {
    kind: 'reindex',
    payload: { shard: 3 },
  });
});

test('get() fetches the record and URL-encodes the id', async () => {
  const { calls, fetch } = recordingFetch(jsonResponse(200, TASK_RECORD));
  const client = new RustyClient('http://unit.test', { fetch });

  const out = await client.tasks.get('t/odd id');

  assert.equal(calls[0].url, 'http://unit.test/tasks/t%2Fodd%20id');
  assert.equal(calls[0].init.method, 'GET');
  assert.equal(out.task_id, 't-123');
});

test('list() hits the bare route without a filter, ?status= with one', async () => {
  const { calls, fetch } = recordingFetch(jsonResponse(200, [TASK_RECORD]));
  const client = new RustyClient('http://unit.test', { fetch });

  const all = await client.tasks.list();
  assert.equal(calls[0].url, 'http://unit.test/tasks');
  assert.equal(all.length, 1);

  // status=dead is the DLQ listing.
  await client.tasks.list({ status: 'dead' });
  assert.equal(calls[1].url, 'http://unit.test/tasks?status=dead');
});

test('cancel() posts to the cancel route and returns the updated record', async () => {
  const cancelled = { ...TASK_RECORD, status: 'cancelled', error_class: 'cancelled' };
  const { calls, fetch } = recordingFetch(jsonResponse(200, cancelled));
  const client = new RustyClient('http://unit.test', { fetch });

  const out = await client.tasks.cancel('t-123');

  assert.equal(calls[0].url, 'http://unit.test/tasks/t-123/cancel');
  assert.equal(calls[0].init.method, 'POST');
  assert.equal(out.status, 'cancelled');
});

test('cancelRunTasks() posts to the run route and splits the ids', async () => {
  const body = { run_id: 'r-9', cancelled: ['t-1'], signalled: ['t-2'] };
  const { calls, fetch } = recordingFetch(jsonResponse(200, body));
  const client = new RustyClient('http://unit.test', { fetch });

  const out = await client.tasks.cancelRunTasks('r-9');

  assert.equal(calls[0].url, 'http://unit.test/runs/r-9/cancel');
  assert.deepEqual(out.cancelled, ['t-1']);
  assert.deepEqual(out.signalled, ['t-2']);
});

test('a 404 surfaces RustyError with status and parsed body', async () => {
  const { fetch } = recordingFetch(
    jsonResponse(404, { error: 'not_found', message: 'task `nope` not found' }),
  );
  const client = new RustyClient('http://unit.test', { fetch });

  await assert.rejects(
    () => client.tasks.get('nope'),
    (err) => {
      assert.ok(err instanceof RustyError);
      assert.equal(err.status, 404);
      assert.equal(err.body.error, 'not_found');
      return true;
    },
  );
});

test('a 409 on cancel of a terminal task surfaces the server message', async () => {
  const { fetch } = recordingFetch(
    jsonResponse(409, {
      error: 'conflict',
      message: 'task `t-1` is already terminal (completed) and cannot be cancelled',
    }),
  );
  const client = new RustyClient('http://unit.test', { fetch });

  await assert.rejects(
    () => client.tasks.cancel('t-1'),
    (err) => {
      assert.ok(err instanceof RustyError);
      assert.equal(err.status, 409);
      assert.match(err.message, /already terminal/);
      return true;
    },
  );
});

test('the tasks client honors the parent timeout machinery', async () => {
  // Same AbortController path as every other method: a fetch that only
  // settles when the client's timeout fires must reject as a timeout.
  const hangingFetch = (_url, init) =>
    new Promise((_resolve, reject) => {
      init.signal.addEventListener('abort', () => {
        const err = new Error('The operation was aborted');
        err.name = 'AbortError';
        reject(err);
      });
    });
  const impatient = new RustyClient('http://unit.test', {
    timeout: 10,
    fetch: hangingFetch,
  });

  await assert.rejects(
    () => impatient.tasks.list(),
    (err) => err.name === 'RustyTimeoutError' && err.timeoutMs === 10,
  );
});
