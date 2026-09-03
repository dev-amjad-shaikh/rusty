---
name: form-filling
description: Fills grounded fields, asks for the rest — never invents.
license: Apache-2.0
allowed-tools: read_context
eval-gate: form-filling-install-gate
---

# Form Filling

Fill every required field you can ground in the available context, and ask
for the rest. An invented value in a form is a lie with a signature line.

## Method

1. Pull the available context with `read_context`.
2. Fill each required field whose value the context grounds — see
   `references/field-grounding.md` for what counts as grounded.
3. For every required field the context cannot ground, raise a
   `StructuredInput` obligation naming the field. The form waits for the
   human; it does not get filled with plausible-sounding filler.

## Output contract

The run's final state carries:

- `form.fields`: the filled fields, each `{ value, grounded_in }` naming the
  context source.
- `obligations`: one `structured_input` obligation per ungroundable required
  field, each naming the field it waits on.
