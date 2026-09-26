# Rusty Feature Evidence — Studio v0.12.0

Captured 2026-09-04 against local `rusty-server` demo (`server_demo` example) + Studio UI.
The screenshots are not kept in the repository (every clone would pay for them); they are
published with the matching GitHub Release, and the table below names each one.

## Screenshots

| # | Feature | File |
|---|---------|------|
| 01 | **Work Board** — Run pipeline with queued/working/needs-you/stuck/done lanes | `01-work-board.png` |
| 01b | **Thread Detail** — Deep-dive run paused waiting for input | `01b-thread-detail.png` |
| 02 | **Agent Portfolio** — 9 active agents (Customer insight analyst, Tool-Free Evidence Agent, Support Triage, Invoice Auditor, etc.) | `02-agent-portfolio.png` |
| 03 | **Agent Builder** — New agent creation form (Purpose, Goals, Model, Knowledge, Tools, Output, Guardrails) | `03-agent-builder.png` |
| 04 | **Prompt Library** — Prompt management surface | `04-prompt-library.png` |
| 05 | **Skills & Tools** — Skill registry and tool management | `05-skills-tools.png` |
| 06 | **Knowledge** — Knowledge base / memory governance | `06-knowledge.png` |
| 07 | **Connectors** — ServiceNow Table API connector with manifest hash and setup flow | `07-connectors.png` |
| 08 | **Run & Evaluate** — Eval execution and run history | `08-run-evaluate.png` |
| 09 | **Memory** — Memory governance and retention | `09-memory.png` |
| 10 | **Operations** — Task queue and operational exceptions | `10-operations.png` |
| 12 | **Server Info** — `/info` endpoint showing graphs (pipeline, react_agent, deep-dive) | `12-server-info.png` |
| 13 | **Connectors API** — `/connectors` endpoint showing seeded ServiceNow manifest | `13-connectors-api.png` |
| 14 | **Threads API** — `/threads` endpoint listing active sessions | `14-threads-api.png` |

## How to reproduce

```bash
cd rusty   # the cloned repo root
cargo run --example server_demo &
cd studio/ui && npm ci && npm run build
cd ../.. && python3 studio/serve.py --port 8000 --target http://127.0.0.1:8100 &
open http://127.0.0.1:8000
```

## Key observations

- **Agent creation**: Full guided draft with 7-step form (Purpose → Goals → Model → Knowledge → Tools → Output → Guardrails)
- **Agent evaluation**: Run & Evaluate surface with history and proof/receipt review
- **Connector adding**: ServiceNow Table API connector seeded with manifest-driven setup
- **Tools building**: Skills & Tools surface with registry integration
- **Work board**: Kanban-style pipeline with deep-dive, react_agent, pipeline runs
- **Thread detail**: Run/Trace/Evaluate tabs with pause/resume lifecycle
