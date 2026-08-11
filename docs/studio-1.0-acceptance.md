# Studio 1.0 experience acceptance

Studio 1.0 is ready when people can create an agent, give it real work, understand what happened, improve it with evidence, and handle exceptions without learning Rusty's internal architecture first.

## Product model

The default product has three primary destinations:

- **Agents** — shape an agent's purpose, model, knowledge, tools, output, and guardrails.
- **Work** — give an agent an objective and stay in one continuous run, trace, and evaluation workspace.
- **Operations** — find blocked or failed work first, then reach schedules, automations, and durable tasks when needed.

Specialist and forensic tools remain available through contextual handoffs or the Advanced console. They are not equal-weight primary navigation.

## Acceptance criteria

### First use

- A disconnected first visit presents one clear action: connect a Rusty server.
- Primary chrome contains no release commentary, endpoint paths, protocol vocabulary, or implementation history.
- The product remains usable at 390 px without horizontal overflow, clipped actions, or an overwhelming navigation stack.

### Agents

- Agent creation is one visual capability system: Purpose, Model, Knowledge, Tools, Output, and Guardrails.
- A person can review the complete configuration before creating the agent.
- Create operations bind the exact request to the exact server receipt and never invite unsafe retry after an ambiguous outcome.
- Advanced manifests, immutable versions, and lifecycle controls remain reachable without dominating the creation journey.

### Work

- An objective flows into one continuous **Run → Trace → Evaluate** workspace.
- Thread, run, agent, and objective provenance remain exact across navigation and late responses.
- Trace inspection provides a causal step view, latency/token/cost summaries when observed, search and filtering, bounded previews, and exact evidence downloads.
- Evaluation is deliberately attached to the reviewed run; unrelated page-memory evidence cannot be presented as proof for it.
- Ambiguous launches remain visibly locked until the user reconciles or explicitly abandons them.

### Prompts and evaluation

- Prompt editing, immutable versions, and live-run provenance are deliberate rather than inferred.
- Dataset and evaluation work is reachable from a run and supports comparison of observed outcomes.
- Drafts and selections never cross a server or tenant boundary.

### Operations

- Exceptions appear before healthy catalog inventory.
- The UI states exactly which systems were observed and never implies that an unread failure source is healthy.
- Selected evidence and mutation state are scoped to the active connection and tenant.
- Routine lifecycle controls remain available through contextual handoffs.

### Safety and accessibility

- Late callbacks cannot overwrite a newer server, tenant, agent, thread, run, prompt, or view.
- Every mutation has exact receipt validation and an explicit ambiguous-outcome policy.
- Credentials never enter URLs or persisted browser storage; recent-work storage contains only bounded opaque identifiers for the current tab.
- Keyboard focus survives rerenders, dialogs trap and restore focus, live regions are stable, and all essential actions have accessible names and states.
- Hostile, oversized, control-bearing, and unsafe-integer evidence is rendered safely and bounded without silently changing exact values.

## Verification gates

Release requires all of the following:

1. Typed frontend type checking and component/API/accessibility tests.
2. The complete Studio compatibility suite.
3. A production build whose committed distribution exactly matches source.
4. Live desktop and 390 px browser validation with no console errors or horizontal overflow.
5. Direct deep-link and legacy-console serving checks.
6. Independent correctness and UX review with no unresolved actionable findings.
7. Successful repository CI after integration into `main`.

The compatibility suite alone is not evidence that the default typed experience is complete. The typed product gates and live browser checks are equally required.
