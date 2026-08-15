# Studio v4: creation to first work

Status: implementation contract.

## Outcome

Creating an agent is one deliberate journey: shape the definition, review the exact version that will be stored, create it once, and start its first task. The Command Center then becomes the place where that task is followed. The user should never have to infer which screen comes next.

## Flow

1. **Shape** — the seven-capability builder remains the authoring surface.
2. **Review** — Studio freezes the complete version-1 request and shows the user-facing capability values before any mutation is sent.
3. **Create** — only the frozen reviewed request is submitted. Existing exact-receipt, ambiguity-lock, workspace, and late-response ownership rules remain in force.
4. **Start work** — an exact create receipt opens a completion surface with one primary action: start the agent's first task. Reviewing the saved agent and returning to the Work board remain secondary actions.
5. **Follow work** — a verified empty Command Center offers the next truthful action. If an available agent exists it can be handed directly to the run composer; if none exists the board begins the creation journey.

## Interaction invariants

- Opening review performs complete validation and moves focus to the review heading.
- Editing cannot continue behind the review. Returning to edit restores focus to the review action.
- Creation uses an immutable snapshot captured when review opened; visible review and submitted bytes cannot drift.
- A workspace change closes review and restores that workspace's owned draft.
- No success surface appears without an exact create receipt or successful exact reconciliation.
- Starting first work prepares the exact returned assistant version; Work still enforces the server's active-version guard at admission.
- Command Center does not invent a run or show a newly created agent as work. It only provides a handoff to the run composer.
- Loading, unavailable-agent, no-agent, and available-agent empty states remain distinct.

## Visual contract

- Review and completion use the supplied v4 graphite/copper material language, not generic modal cards.
- The builder retains its compact nested header and three-part authoring layout.
- Review replaces the authoring grid so the final decision has one visual focus.
- Completion is calm and sparse: created identity, active version, the next task, and secondary destinations.
- Desktop and 320px layouts keep actions visible without horizontal scrolling.

## Acceptance

- No create request is sent before the deliberate final review action.
- The exact reviewed request is the exact request submitted.
- Exact create success can hand directly to Work with the new agent selected.
- A verified empty Work board chooses among Create first agent, Review agents, and Start with agent from corroborated catalog state.
- Focus, route blocking, reconnect ownership, mutation ambiguity, reduced motion, and forced-colors behavior remain covered.
- Typed Studio, legacy Studio, production build, live desktop/mobile review, and source-to-dist parity pass.
