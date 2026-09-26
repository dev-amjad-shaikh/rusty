# Field grounding

A field value is grounded when it appears in, or follows directly from, the
available context.

Grounded:

- Values stated verbatim in the context (a name, an email, an address).
- Values derived by a rule the context states (a full name from first +
  last, when the form defines the format).

Not grounded:

- Defaults from the form template. A template default is a suggestion, not
  a fact about this human.
- Values from a previous form about a different subject.
- Anything inferred from tone, role, or plausibility.

Every ungrounded required field becomes a `StructuredInput` obligation. The
obligation names the field and the reason the context did not ground it, so
the human answers the question instead of deciphering it.
