/**
 * @rusty-runtime/client e2e tests — run against the REAL rusty-server demo
 * binary (`cargo build --example server_demo` in rusty-server/).
 *
 * The suite builds the binary if missing, launches it as a child process
 * (the demo binds 127.0.0.1:8100), polls /ok, exercises the full client
 * surface, then kills the child and removes its scratch store directory.
 *
 * Run from the repo root:  node --test sdks/typescript/test/
 */

import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, rm } from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  RustyClient,
  RustyError,
  RustyTimeoutError,
} from '../src/index.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, '..', '..', '..');
// The repo is a Cargo workspace: examples build into the workspace-root
// target/. Fall back to the legacy per-crate path for pre-workspace clones.
const BINARY_CANDIDATES = [
  path.join(REPO_ROOT, 'target', 'debug', 'examples', 'server_demo'),
  path.join(REPO_ROOT, 'rusty-server', 'target', 'debug', 'examples', 'server_demo'),
];
const BINARY = BINARY_CANDIDATES.find((p) => existsSync(p)) ?? BINARY_CANDIDATES[0];
const SERVER_MANIFEST = path.join(REPO_ROOT, 'rusty-server', 'Cargo.toml');
const SCRATCH_DIR = path.join(REPO_ROOT, 'sdks', 'typescript', '.tmp-e2e');
const BASE_URL = 'http://127.0.0.1:8100';
const PORT = 8100;

/** @type {import('node:child_process').ChildProcess | null} */
let child = null;
/** @type {RustyClient} */
let client;

function isPortFree(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host: '127.0.0.1' });
    socket.once('connect', () => {
      socket.destroy();
      resolve(false);
    });
    socket.once('error', () => resolve(true));
    socket.setTimeout(1000, () => {
      socket.destroy();
      resolve(true);
    });
  });
}

async function waitForServer(url, attempts = 75, intervalMs = 200) {
  for (let i = 0; i < attempts; i += 1) {
    try {
      const res = await fetch(`${url}/ok`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    if (child && child.exitCode !== null) {
      throw new Error(`server_demo exited early with code ${child.exitCode}`);
    }
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  throw new Error(`server at ${url} did not become ready`);
}

before(async () => {
  if (!existsSync(BINARY)) {
    const cargoPath = `${process.env.HOME}/.cargo/bin:${process.env.PATH}`;
    execSync(`cargo build --manifest-path "${SERVER_MANIFEST}" --example server_demo`, {
      cwd: REPO_ROOT,
      stdio: 'inherit',
      env: { ...process.env, PATH: cargoPath },
    });
  }
  assert.ok(existsSync(BINARY), `demo binary missing at ${BINARY}`);

  if (!(await isPortFree(PORT))) {
    throw new Error(
      `port ${PORT} is already in use — the server_demo binary binds ` +
        `127.0.0.1:${PORT} unconditionally. Free the port and re-run.`,
    );
  }

  await rm(SCRATCH_DIR, { recursive: true, force: true });
  await mkdir(SCRATCH_DIR, { recursive: true });

  child = spawn(BINARY, [], {
    cwd: SCRATCH_DIR, // demo's ./data/server-demo-checkpoints lands here
    stdio: ['ignore', 'ignore', 'pipe'],
    // The suite assumes the dev-mode server: no sign-in wall. The demo
    // seeds a bootstrap admin unless RUSTY_OPEN=1 opts out.
    env: { ...process.env, RUSTY_OPEN: '1' },
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
    if (stderr.length > 8192) stderr = stderr.slice(-8192);
  });
  child.once('exit', (code) => {
    if (code !== null && code !== 0) {
      console.error(`server_demo exited with code ${code}:\n${stderr}`);
    }
  });
  // Last-resort cleanup if the test runner is killed mid-run.
  process.on('exit', () => {
    if (child && child.exitCode === null) child.kill('SIGKILL');
  });

  await waitForServer(BASE_URL);
  client = new RustyClient(BASE_URL);
}, { timeout: 300_000 });

after(async () => {
  if (child && child.exitCode === null) {
    child.kill('SIGTERM');
    await Promise.race([
      new Promise((r) => child.once('exit', r)),
      new Promise((r) => setTimeout(r, 3000)),
    ]);
    if (child.exitCode === null) child.kill('SIGKILL');
  }
  await rm(SCRATCH_DIR, { recursive: true, force: true });
});

// ---------------------------------------------------------------- helpers

/** Fresh pipeline thread per test (one active run per thread on the server). */
async function pipelineThread() {
  const { thread_id } = await client.createThread('pipeline');
  return thread_id;
}

async function pollRunTerminal(runId, attempts = 50, intervalMs = 100) {
  for (let i = 0; i < attempts; i += 1) {
    const status = await client.runStatus(runId);
    if (!['pending', 'running'].includes(status.status)) return status;
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  throw new Error(`run ${runId} did not reach a terminal state`);
}

// ------------------------------------------------------------------ tests

test('info() reports service metadata and registered graphs', async () => {
  const info = await client.info();
  assert.equal(info.service, 'rusty-server');
  assert.ok(info.version, 'version present');
  assert.equal(info.checkpointer, 'json_file');
  const names = info.graphs.map((g) => g.name).sort();
  assert.deepEqual(names, ['deep-dive', 'pipeline', 'react_agent']);
  const pipeline = info.graphs.find((g) => g.name === 'pipeline');
  assert.deepEqual(pipeline.channels, ['log']);
});

test('createThread() creates a thread bound to a graph, with metadata', async () => {
  const thread = await client.createThread('pipeline', { metadata: { origin: 'e2e' } });
  assert.ok(thread.thread_id, 'thread_id present');
  assert.equal(thread.graph, 'pipeline');
  assert.equal(thread.metadata.origin, 'e2e');
});

test('runWait() runs the pipeline graph to a terminal success', async () => {
  const tid = await pipelineThread();
  const terminal = await client.runWait(tid, {});
  assert.equal(terminal.status, 'success');
  assert.deepEqual(terminal.output.log, ['first', 'second']);
  assert.ok(terminal.run_id, 'run_id present');
});

test('getState() and history() expose checkpoints after a run', async () => {
  const tid = await pipelineThread();
  await client.runWait(tid, {});

  const state = await client.getState(tid);
  assert.deepEqual(state.values.log, ['first', 'second']);
  assert.deepEqual(state.next, []);
  assert.ok(state.checkpoint.checkpoint_id);
  assert.equal(state.checkpoint.thread_id, tid);

  const history = await client.history(tid);
  assert.equal(history.length, 2, 'pipeline run writes two checkpoints');
  assert.ok(history[0].checkpoint.step > history[1].checkpoint.step, 'newest first');

  const limited = await client.history(tid, { limit: 1 });
  assert.equal(limited.length, 1);
  assert.equal(
    limited[0].checkpoint.checkpoint_id,
    history[0].checkpoint.checkpoint_id,
  );
});

test('updateState() writes a new checkpoint', async () => {
  const tid = await pipelineThread();
  await client.runWait(tid, {});
  await client.updateState(tid, { log: ['manual'] });
  const state = await client.getState(tid);
  assert.deepEqual(state.values.log, ['manual']);
});

test('run() starts a background run; runStatus() polls it to terminal', async () => {
  const tid = await pipelineThread();
  const record = await client.run(tid, {});
  assert.ok(record.run_id, 'run_id present');
  assert.equal(record.thread_id, tid);

  const terminal = await pollRunTerminal(record.run_id);
  assert.equal(terminal.status, 'success');
  assert.equal(terminal.run_id, record.run_id);
  assert.equal(terminal.graph, 'pipeline');
  assert.deepEqual(terminal.output.log, ['first', 'second']);
});

test('runStream() yields metadata, updates/values, and end frames', async () => {
  const tid = await pipelineThread();
  const frames = [];
  for await (const frame of client.runStream(tid, {})) {
    frames.push(frame);
  }

  assert.ok(frames.length >= 3, `expected several frames, got ${frames.length}`);
  assert.equal(frames[0].event, 'metadata', 'first frame is run metadata');
  assert.ok(frames[0].data.run_id, 'metadata carries run_id');
  assert.equal(frames[0].data.thread_id, tid);

  const events = frames.map((f) => f.event);
  assert.ok(events.includes('updates'), 'updates frames present');
  assert.ok(events.includes('values'), 'values frames present');
  const end = frames.at(-1);
  assert.equal(end.event, 'end');
  assert.equal(end.data.status, 'success');

  // Frame ids look like {checkpoint_id}:{step}:{seq} with 1-based seq.
  const withIds = frames.filter((f) => typeof f.id === 'string' && f.id.includes(':'));
  assert.ok(withIds.length > 0, 'frames carry ids');
  const seqs = withIds.map((f) => Number.parseInt(f.id.split(':').at(-1), 10));
  assert.ok(
    seqs.every((s, i) => i === 0 || s > seqs[i - 1]),
    'sequence numbers increase monotonically',
  );
});

test('runStream() honors the streamMode option as a payload filter', async () => {
  const tid = await pipelineThread();
  const events = [];
  for await (const frame of client.runStream(tid, {}, { streamMode: ['updates'] })) {
    events.push(frame.event);
  }
  assert.ok(events.includes('updates'));
  assert.ok(!events.includes('values'), 'values frames filtered out');
  assert.equal(events.at(-1), 'end');
});

test('fork() + checkpoint replay re-runs the graph tail on the fork', async () => {
  const tid = await pipelineThread();
  await client.runWait(tid, {});

  const history = await client.history(tid);
  const earliest = history.at(-1); // step 0: log=["first"], next=["second"]
  assert.deepEqual(earliest.values.log, ['first']);

  const fork = await client.fork(tid, { checkpointId: earliest.checkpoint.checkpoint_id });
  assert.ok(fork.thread_id);
  assert.equal(fork.checkpoints_copied, 1);

  const forkState = await client.getState(fork.thread_id);
  assert.deepEqual(forkState.values.log, ['first']);
  assert.deepEqual(forkState.next, ['second']);

  const replay = await client.runWait(fork.thread_id, {
    checkpoint: { checkpoint_id: earliest.checkpoint.checkpoint_id },
  });
  assert.equal(replay.status, 'success');
  assert.deepEqual(replay.output.log, ['first', 'second']);
});

test('kv store: put/get/list/delete round trip with 404 semantics', async () => {
  const ns = 'e2e-kv';
  const created = await client.kvPut(ns, 'b-key', { n: 1 });
  assert.equal(created.namespace, ns);
  assert.equal(created.key, 'b-key');
  assert.deepEqual(created.value, { n: 1 });

  await client.kvPut(ns, 'a-key', 'alpha');

  const replaced = await client.kvPut(ns, 'b-key', { n: 2 });
  assert.deepEqual(replaced.value, { n: 2 });
  assert.equal(replaced.created_at, created.created_at, 'created_at preserved on replace');

  const got = await client.kvGet(ns, 'b-key');
  assert.deepEqual(got.value, { n: 2 });

  const items = await client.kvList(ns);
  assert.deepEqual(
    items.map((i) => i.key),
    ['a-key', 'b-key'],
    'namespace listing is sorted by key',
  );

  await client.kvDelete(ns, 'b-key');
  await assert.rejects(
    () => client.kvGet(ns, 'b-key'),
    (err) => err instanceof RustyError && err.status === 404,
  );

  // Cleanup.
  await client.kvDelete(ns, 'a-key');
  assert.deepEqual(await client.kvList('never-written'), []);
});

test('assistants: create/list/get, and runWait by assistant_id', async () => {
  const assistant = await client.createAssistant({
    name: 'e2e-react',
    graph: 'react_agent',
    config: { recursion_limit: 10 },
    metadata: { suite: 'e2e' },
  });
  assert.ok(assistant.assistant_id);

  const listed = await client.listAssistants();
  assert.ok(listed.some((a) => a.assistant_id === assistant.assistant_id));

  const fetched = await client.getAssistant(assistant.assistant_id);
  assert.equal(fetched.name, 'e2e-react');
  assert.equal(fetched.graph, 'react_agent');

  const tid = await client.createThread('react_agent');
  const terminal = await client.runWait(tid.thread_id, {
    assistant_id: assistant.assistant_id,
    input: { messages: [{ role: 'user', content: 'say pong' }] },
  });
  assert.equal(terminal.status, 'success');
});

test('crons: create/list/delete with 404 after delete', async () => {
  const cron = await client.createCron({
    graph: 'pipeline',
    intervalSecs: 3600, // far enough that it never fires during the suite
    input: {},
    metadata: { suite: 'e2e' },
  });
  const cronId = cron.cron_id ?? cron.id;
  assert.ok(cronId, 'cron id present');

  const listed = await client.listCrons();
  assert.ok(listed.some((c) => (c.cron_id ?? c.id) === cronId));

  await client.deleteCron(cronId);
  await assert.rejects(
    () => client.deleteCron(cronId),
    (err) => err instanceof RustyError && err.status === 404,
  );
});

test('runEvents() fetches the Flight Recorder journal of a completed run', async () => {
  const tid = await pipelineThread();
  const terminal = await client.runWait(tid, {});
  assert.equal(terminal.status, 'success');
  assert.ok(terminal.run_id, 'terminal JSON carries run_id');

  const body = await client.runEvents(terminal.run_id);
  assert.equal(body.run_id, terminal.run_id);
  assert.equal(body.complete, true);
  assert.ok(body.events.length > 0, 'a journaled run must have events');

  // Total order + deterministic ids: seq is 0..n, id is {run_id}:{seq}.
  body.events.forEach((event, seq) => {
    for (const field of [
      'id', 'run_id', 'thread_id', 'node_id', 'seq', 'kind', 'effect',
      'input', 'output', 'latency_ms', 'tokens', 'cost_usd', 'status',
      'parent', 'recorded_at',
    ]) {
      assert.ok(field in event, `event missing \`${field}\``);
    }
    assert.equal(event.seq, seq);
    assert.equal(event.id, `${terminal.run_id}:${seq}`);
    assert.equal(event.run_id, terminal.run_id);
    assert.equal(event.thread_id, tid);
  });

  // The executor journaled the full lifecycle, in the golden wire shape.
  const kinds = body.events.map((e) => e.kind);
  for (const expected of [
    'super_step_start',
    'super_step_end',
    'node_input',
    'node_output',
    'routing_decision',
    'checkpoint_written',
  ]) {
    assert.ok(kinds.includes(expected), `missing \`${expected}\` event`);
  }
  assert.equal(kinds[0], 'super_step_start');

  // Payload refs are adjacently tagged; demo pipeline nodes are pure.
  const nodeInput = body.events.find((e) => e.kind === 'node_input');
  assert.equal(nodeInput.input.kind, 'inline');
  assert.equal(nodeInput.effect, 'pure');
  // Causal parentage: a node input's parent is its super-step start.
  const parent = body.events.find((e) => e.id === nodeInput.parent);
  assert.equal(parent.kind, 'super_step_start');
});

test('runEvents() 404s for an unknown run', async () => {
  await assert.rejects(
    () => client.runEvents(crypto.randomUUID()),
    (err) => err instanceof RustyError && err.status === 404,
  );
});

test('replayRun() verifies a journaled pipeline run, exact response shape', async () => {
  const tid = await pipelineThread();
  const terminal = await client.runWait(tid, {});
  assert.equal(terminal.status, 'success');
  const eventCount = (await client.runEvents(terminal.run_id)).events.length;

  const report = await client.replayRun(terminal.run_id);
  assert.deepEqual(
    Object.keys(report).sort(),
    ['actual_events', 'expected_events', 'first_divergence', 'run_id', 'verified'],
  );
  assert.equal(report.run_id, terminal.run_id);
  assert.equal(report.verified, true);
  assert.equal(report.expected_events, eventCount);
  assert.equal(report.actual_events, eventCount);
  assert.equal(report.first_divergence, null);
});

test('replayRun() 404s for an unknown run', async () => {
  await assert.rejects(
    () => client.replayRun(crypto.randomUUID()),
    (err) => err instanceof RustyError && err.status === 404,
  );
});

test('diffRuns() shows divergence between a run and its fork with different input', async () => {
  const tid = await pipelineThread();
  const baseRun = (await client.runWait(tid, { input: { seed: 1 } })).run_id;

  const fork = await client.fork(tid);
  const branchRun = (await client.runWait(fork.thread_id, { input: { seed: 2 } })).run_id;

  const diff = await client.diffRuns(baseRun, branchRun);
  for (const field of [
    'first_divergent_seq',
    'added',
    'removed',
    'step_diffs',
    'base_totals',
    'branch_totals',
  ]) {
    assert.ok(field in diff, `diff missing \`${field}\``);
  }
  assert.notEqual(
    diff.first_divergent_seq,
    null,
    'forks with different inputs must diverge',
  );
  assert.ok(diff.added.length > 0);
  assert.ok(diff.removed.length > 0);
  // Same graph, same step count: content differs, not length.
  assert.equal(diff.base_totals.events, diff.branch_totals.events);

  // A run diffed against itself is logically identical.
  const same = await client.diffRuns(baseRun, baseRun);
  assert.equal(same.first_divergent_seq, null);
  assert.equal(same.added.length, 0);
  assert.equal(same.removed.length, 0);
});

test('diffRuns() 404s for an unknown run on either side', async () => {
  const tid = await pipelineThread();
  const runId = (await client.runWait(tid, {})).run_id;
  await assert.rejects(
    () => client.diffRuns(runId, crypto.randomUUID()),
    (err) => err instanceof RustyError && err.status === 404,
  );
  await assert.rejects(
    () => client.diffRuns(crypto.randomUUID(), runId),
    (err) => err instanceof RustyError && err.status === 404,
  );
});

test('getFixture() downloads a portable replay bundle', async () => {
  const tid = await pipelineThread();
  const terminal = await client.runWait(tid, {});

  const fixture = await client.getFixture(terminal.run_id);
  assert.equal(fixture.format_version, 1);
  assert.ok(fixture.graph_hash.length > 0);
  assert.equal(fixture.journal.run_id, terminal.run_id);
  assert.equal(fixture.journal.thread_id, tid);
  assert.ok(fixture.journal.events.length > 0);
  // The demo run wrote checkpoints, so the bundle carries the final one.
  assert.equal(fixture.final_checkpoint.thread_id, tid);

  await assert.rejects(
    () => client.getFixture(crypto.randomUUID()),
    (err) => err instanceof RustyError && err.status === 404,
  );
});

test('unknown thread surfaces RustyError with status and body', async () => {
  await assert.rejects(
    () => client.getState('no-such-thread'),
    (err) => {
      assert.ok(err instanceof RustyError);
      assert.equal(err.status, 404);
      assert.ok(err.body !== undefined, 'error body captured');
      return true;
    },
  );
});

test('client-side timeout rejects with RustyTimeoutError', async () => {
  // Deterministic: a fetch that only settles when the client's AbortController
  // fires. (Racing a 1 ms timeout against a localhost server is flaky.)
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
    () => impatient.info(),
    (err) => err instanceof RustyTimeoutError && err.timeoutMs === 10,
  );
});

test('auth: demo server runs in dev mode (no API key configured)', async (t) => {
  // The demo binary sets no API key, so requests without X-Api-Key must pass.
  const info = await client.info();
  assert.equal(info.service, 'rusty-server');

  // A 401-without-key assertion only applies when a key IS configured.
  // Detect auth enforcement by probing with a bogus key: in dev mode the
  // server ignores it; with auth configured a wrong key yields 401.
  const probe = await fetch(`${BASE_URL}/info`, { headers: { 'x-api-key': 'bogus' } });
  if (probe.status === 200) {
    t.skip('server has no API key configured (dev mode) — 401 case not applicable');
    return;
  }
  assert.equal(probe.status, 401, 'bogus key rejected when auth is configured');
});

// ------------------------------------------- SSE parser unit tests (no I/O)

test('SSE parser: multi-line data, comments, CRLF, Last-Event-ID header', async () => {
  const captured = { url: null, init: null };
  const encoder = new TextEncoder();
  const stream = new ReadableStream({
    start(controller) {
      controller.enqueue(encoder.encode(': keepalive\r\n\r\n'));
      controller.enqueue(
        encoder.encode('event: metadata\r\nid: -:0:1\r\ndata: {"run_id":"r1"}\r\n\r\n'),
      );
      controller.enqueue(encoder.encode('event: updates\nid: cp1:0:2\n'));
      controller.enqueue(encoder.encode('data: {"step":0,\ndata: "updates":{"first":{}}}\n\n'));
      controller.enqueue(encoder.encode('event: end\ndata: not-json\n\n'));
      controller.close();
    },
  });
  const fakeFetch = async (url, init) => {
    captured.url = url;
    captured.init = init;
    return new Response(stream, {
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
    });
  };

  const fake = new RustyClient('http://unit.test', { fetch: fakeFetch });
  const frames = [];
  for await (const frame of fake.runStream('t1', { input: {} }, { lastEventId: 'cp0:0:1' })) {
    frames.push(frame);
  }

  // Last-Event-ID resume header is sent verbatim.
  assert.equal(captured.init.headers['last-event-id'], 'cp0:0:1');
  assert.equal(captured.init.method, 'POST');
  assert.match(captured.url, /\/threads\/t1\/runs\/stream$/);

  assert.equal(frames.length, 3);
  assert.deepEqual(frames[0], {
    event: 'metadata',
    data: { run_id: 'r1' },
    id: '-:0:1',
  });
  // Multi-line data is joined with '\n' and JSON-parsed as one payload.
  assert.equal(frames[1].event, 'updates');
  assert.equal(frames[1].id, 'cp1:0:2');
  assert.deepEqual(frames[1].data, { step: 0, updates: { first: {} } });
  // Non-JSON payloads pass through as raw strings.
  assert.deepEqual(frames[2], { event: 'end', data: 'not-json' });
});

test('SSE parser: api key and extra headers are sent on requests', async () => {
  const captured = { init: null };
  const stream = new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode('event: end\ndata: {"status":"success"}\n\n'));
      controller.close();
    },
  });
  const fakeFetch = async (_url, init) => {
    captured.init = init;
    return new Response(stream, { status: 200 });
  };
  const authed = new RustyClient('http://unit.test', {
    apiKey: 'secret-key',
    fetch: fakeFetch,
  });
  const frames = [];
  for await (const frame of authed.runStream('t1')) frames.push(frame);
  assert.equal(captured.init.headers['x-api-key'], 'secret-key');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].data.status, 'success');
});

test('runStream cancels the HTTP body when the consumer breaks early', async () => {
  // Regression: breaking out of `for await` must tear down the response
  // stream instead of letting it drain until the server finishes the run.
  let cancelled = false;
  const stream = new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode('event: updates\ndata: {}\n\n'));
      // Never closes — simulates a long-running server stream.
    },
    cancel() {
      cancelled = true;
    },
  });
  const fakeFetch = async () =>
    new Response(stream, {
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
    });
  const fake = new RustyClient('http://unit.test', { fetch: fakeFetch });

  let first = null;
  for await (const frame of fake.runStream('t1')) {
    first = frame;
    break;
  }
  assert.equal(first.event, 'updates');
  assert.ok(cancelled, 'underlying stream cancelled after early break');
});
