# How to — build with Rusty Studio

Step-by-step guides to the four things every builder does first: an agent, a skill, a
tool, and a connector. Every flow runs in [Rusty Studio](studio.md) against a local
Rusty server — the same HTTP/SSE surface the SDKs use.

**Prerequisite — run it locally:**

```bash
git clone https://github.com/dev-amjad-shaikh/rusty.git && cd rusty
./scripts/dev.sh        # Rusty Server on :8100 + Rusty Studio on http://localhost:4400
```

Open **http://localhost:4400**. With no configuration the demo agents answer from a
deterministic local model (no network, no credentials). To use a real model, add an
OpenAI-compatible endpoint to a git-ignored `.env.rusty-local` at the repo root
(see the [README](../README.md#try-it-in-one-command)) and restart `./scripts/dev.sh`.

The screens referenced below live in [screenshots/](screenshots/).

## Build an agent

![The agent builder](screenshots/agent-builder.png)

1. **Start the wizard.** Home → **New agent** (or Agents → **New agent**). Creation
   runs as six steps — Template · Identity · Goal · Instructions · Model & tools ·
   Review — and you can click back to any earlier step.
2. **Pick a template.** *Blank agent* starts empty. The starting templates (Data
   analyst, Coding agent, Research assistant) pre-fill instructions, a starter tool
   set, a goal, and a reasoning-steps budget you can edit later.
3. **Identity.** Give it a name and handle (`@slug`), a one-line description, a
   color, and an icon. The handle is how runs and evaluations refer to the agent.
4. **Goal.** One sentence a teammate could verify, plus a primary metric and target.
   The goal is measured two ways: live, on the agent's runs over a rolling seven
   days; and on publish, where the agent's test suites gate activation.
5. **Instructions.** The system prompt — who the agent is, how it works, what it
   must never do. This is the behavior contract; everything else is plumbing.
6. **Model & tools.** Pick the model (the workspace default is preselected; add
   providers under **AI models**), then choose the tools it may call. Each tool
   carries an effect class, so the runtime knows what it is allowed to do without
   asking a person.
7. **Review and save.** The agent lands as a **draft** in the builder. The
   **readiness** card lists what still separates it from publish — set a goal,
   attach a skill, add a trigger, pass an evaluation goal.
8. **Test before publish.** The rail beside the canvas runs the agent against real
   tool calls; the **Test** tab records what happened. Publish when readiness is
   clean — every publish is measured against the test suites.

## Build a skill

![The skills library](screenshots/skills.png)

A skill is a reusable procedure: a method written once, bundled with the tools it
needs, attachable to any agent. Skills are governed `SKILL.md` packages — parsed
fail-closed, provenance-stamped, versioned immutably.

1. **Open the editor.** Skills → **New skill** (or, from an agent's builder, the
   skill list → **Compose a skill** — the agent gets the skill attached on save).
2. **Name it** — a plain name, e.g. `Weekly notice`.
3. **Write "When to use it."** This is the routing contract: the agent reads it to
   decide whether the skill applies. Write the trigger, not the summary — *"Use when
   the user asks for …, or when …"*.
4. **Write the procedure.** Markdown, with the toolbar for headings, lists, and code.
   Reference tools inline (`` `tool_name` ``) — the editor detects which tools the
   procedure calls and records them as the skill's allowed tools.
5. **Save.** The skill joins the library; attach it to any agent from that agent's
   **Skills** section.

Two other ways skills arrive: **Browse library** imports governed packages from the
registry, and *From a repository* pulls `SKILL.md` files straight from a git URL —
the import report lists what was found, imported, and skipped (with reasons).

## Build a tool

The Tools catalog has no hand-written tools — every tool comes from what the server
can reach. **Tools → New tool** offers the three paths:

1. **From a connection.** Connect a service under [Connectors](#add-a-connector);
   every operation its manifest names becomes a tool, each carrying an effect class
   (`Pure`, `ReadOnly`, `Idempotent`, …) the admission gate reads before it lets a
   run through.
2. **From an OpenAPI document.** Connectors → **Browse all** → **Custom protocol**,
   and paste a spec; each operation becomes a tool the same way.
3. **As a skill.** A procedure *over* existing tools is a skill, not a tool — the
   drawer sends you to [Skills](#build-a-skill).

Grant a tool to an agent from its builder → **Tools**, where each tool also gets a
"when" note so the agent knows when to reach for it.

## Add a connector

![The connector library](screenshots/connectors.png)

Connectors are the systems your agents work in — GitHub, HubSpot, Jira, any public
REST API. One JSON Schema document *is* the connector, so a system nobody built yet
is still a connector you can author. Connect once; any agent you allow uses it.

1. **Browse the library.** Connectors → **Browse all** (or click **Connect** on any
   card in the library). Filter by what the system does, not by name.
2. **Choose the system.** Built-in manifests cover the common SaaS systems; **Custom
   protocol** takes an OpenAPI document for anything else.
3. **Authenticate.** The dialog shows the auth method the manifest declares — API
   key, OAuth sign-in, or none. Credentials go into the broker and are issued to
   tools as short-lived opaque handles: raw values never reach an agent, a prompt,
   or a log.
4. **Use the operations as tools.** The connector's operations appear in the
   [Tools](#build-a-tool) catalog with their effect classes; grant them to an agent
   from its builder.
5. **Test against a stand-in (optional).** A stand-in answers a connection's calls
   from a seeded dataset, so an agent can be exercised end to end — tools, skills,
   effects — without touching the live system. Connectors → **New stand-in**.

## Where to go next

- [studio.md](studio.md) — every view, in detail.
- [server-quickstart.md](server-quickstart.md) — the same flows over HTTP, for
  programmatic control.
- [architecture.md](architecture.md) — what "durable" means under the hood: one
  checkpoint primitive behind resume, approval, and time travel.
