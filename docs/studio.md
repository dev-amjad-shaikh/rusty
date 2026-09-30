# Rusty Studio

Rusty Studio is the product workspace for creating agents, doing real work with them, understanding what
happened, and intervening when durable operations need attention. It is the local companion of
[`rusty-agent-server`](../rusty-server): when Studio is running, the backend is running on the same
machine. There is no server picker and no hosted control plane.

## Product structure

Studio is one workspace with nine destinations on the left strip:

- **Home** — the front door: a greeting, what ran recently, and anything waiting on a person.
- **Agents** — create, configure, test, and publish an agent. Creation is a guided wizard; the builder
  then keeps identity, goal, instructions, model, tools, skills, memory, triggers, and evaluation on one
  canvas, with a test rail beside it for trying the agent against the local server.
- **Skills** — the governed library of `SKILL.md` procedures: compose one in place,
  import a repository, or browse the registry, then attach it to any agent.
- **Tools** — the catalog of what agents can call: built-ins, platform tools, and
  connector operations, each carrying an effect class.
- **Connectors** — the systems your agents can work in. Connect one once — credentials
  live in the broker as opaque handles, never as raw values — and every operation its
  manifest names becomes a tool any allowed agent can use.
- **Knowledge** — governed sources an agent reads from, retrieved as cited chunks.
- **Tests** — versioned datasets, experiments, and the release gates an agent's
  publish is measured against.
- **AI models** — the providers and models the workspace thinks with.
- **Activity** — what got done, where agents are waiting for a person, what went
  wrong, and the verifier's own evidence.

Step-by-step guides to each flow live in [how-to.md](how-to.md); the screens are in
[screenshots/](screenshots/). The workspace renders in the Rusty design language —
oxidised dark canvas, glass surfaces, one ember accent, Outfit and IBM Plex Mono.

While the local runtime boots, Studio shows a plain startup screen and keeps waiting for it; only a
definitive refusal becomes an error with exact recovery guidance.

## Signing in

The demo server creates its first administrator on first boot and writes the generated password to a file
next to the store (the path is logged at startup). Studio opens on a sign-in screen; use those credentials
and change the password. To run without sign-in on a laptop or in a test harness, start the server with
`RUSTY_OPEN=1` — note this only skips *creating* the administrator: a store that already has users keeps
requiring sign-in regardless.

## Development layout

Studio is a typed React application. The build output (`studio/ui/dist/`) is
git-ignored and produced on demand: `studio/serve.py` hosts a previously built
bundle and proxies `/api/*` to the Rusty server for same-origin local use — no
Node.js needed at serve time. During development, `npm run dev` in `studio/ui`
boots the Vite dev server on :8878 against the local backend on :8100 — the first
`cargo build` can take a few minutes; the script waits for `/info` before starting
Vite.

```
studio/
├── ui/                ← React, TypeScript, routes, feature modules, and design system
│   ├── src/           ← the knot workspace (views, flows, engine) under src/knot + src/engine
│   └── dist/          ← git-ignored production bundle, produced by `npm run build`
└── serve.py           ← typed Studio host + same-origin API proxy
```

## How to open

### Development — `npm run dev` (backend + Studio in one command)

```bash
cd studio/ui
npm ci
npm run dev
```

This boots the repository's demo server (`rusty-agent-server`'s `examples/server_demo`) on
`http://127.0.0.1:8100` — reusing one that is already answering there — waits until it proves itself via
`/info`, then starts Vite on `http://127.0.0.1:8878`. The first launch can take a few minutes while cargo
compiles; Studio shows "Starting the local runtime…" and keeps polling instead of failing. Ctrl-C stops
both halves; a server Studio did not start is left running. `npm run dev:ui` starts only the Vite half
when the backend is already up.

If the local server has open mode disabled and refuses Studio, the startup screen says to set
`VITE_RUSTY_API_KEY` and restart; the key is a build-time override and never appears in the UI.

### Committed bundle — `serve.py` (same-origin static host, no Node.js)

```bash
# terminal 1: the demo server
cargo run --example server_demo          # http://127.0.0.1:8100

# terminal 2: the studio
python3 studio/serve.py                  # http://127.0.0.1:8000/
```

Open `http://127.0.0.1:8000/`. Both the Vite proxy and `serve.py` forward `/api` to the local backend, so
the browser only ever talks to its own origin. The local host also flushes SSE per chunk and sets
`X-Accel-Buffering: no`, so streams render live.

### Build Studio after changing the typed source

```bash
cd studio/ui
npm ci
npm run typecheck && npm test && npm run build
```

## CORS

Cross-origin access is governed by `ServerConfig::cors_allowed_origins`. Empty, a dev server answers any
origin (a permissive mirror layer, so Studio's dev proxy on another port works out of the box), while a
`production` server serves same-origin only — a cross-origin browser client must be named in
`cors_allowed_origins`, never mirrored by default. Same-origin through `serve.py` or the Vite proxy sidesteps
CORS entirely: the browser only ever talks to its own origin.

## Demo flow (against `examples/server_demo`)

The demo registers two graphs on `127.0.0.1:8100`: `pipeline` (channel `log`, two nodes `first → second`,
no network) and `react_agent` (channel `messages`, scripted model + echo tool, no network).

1. Start both halves (`npm run dev` in `studio/ui`, or `scripts/dev.sh` from the repo root) and open
   Studio. Sign in with the administrator credentials from the password file the server logged at first
   boot.
2. **Agents → New agent** walks the creation wizard; the builder then opens with the agent's goal,
   instructions, model, and tools on one canvas.
3. Use the **test rail** beside the builder to run the agent against the local server and watch the run
   stream live.
4. **Activity** shows what the workspace has done and where a run is waiting on a person's decision;
   **Tests** holds the datasets and experiments a publish is measured against.

For a graph with a human-approval interrupt, see
[`docs/server-quickstart.md`](server-quickstart.md) — the decision surfaces in Studio for review before
the run resumes.

## Limitations

- Studio is a **local companion**: it talks to exactly one backend, the Rusty server on this machine,
  through the same-origin `/api` proxy. There is no hosted control plane and no multi-server switcher.
- The production bundle (`studio/ui/dist/`) is git-ignored: run `npm run build` in `studio/ui` before
  `serve.py` can host anything.
- Sign-in is required unless the server was started with `RUSTY_OPEN=1` — and `RUSTY_OPEN=1` does not open
  a store that already has users (see [Signing in](#signing-in)).

## Verification

- `npm test` in `studio/ui` — the vitest suites over the knot workspace (views, flows, engine, forms).
- `node studio/test-v1-experience.mjs` — the legacy end-to-end experience contract.
- The server-side behavior Studio exercises is covered by `rusty-server`'s integration tests
  (`cargo test -p rusty-agent-server`).
