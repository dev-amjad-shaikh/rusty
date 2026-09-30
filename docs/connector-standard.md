# The connector standard

> **Rule.** Every connector in Rusty is one shape: a `ConnectorManifest`. There
> is no second way to reach an external system, no connector-specific code path,
> and no field a connector invents outside its declared schema. A connector that
> does not fit the standard is a gap in the standard, and the work is to close
> the gap — never to let one connector be the odd one out.

Connectors are where an agent platform fails first. Everything else can be
retried; a connector is a live credential against someone else's system, and it
breaks in ways that are somebody else's to fix. This document is what the
standard is, why each part of it exists, and where it is still thin.

## What a connector is

A manifest — one JSON document, content-hashed, registered on the server:

| Field | What it is |
| --- | --- |
| `id`, `version`, `display_name`, `description` | identity, kebab-case id |
| `documentation_url` | https, for the operator |
| `base_url` | the API root, templated over config (`https://{instance}.service-now.com`) — https only, checked on the template and on the rendered URL |
| `connection_specification` | JSON Schema draft-07: **everything** a connection needs |
| `operations` | the calls, each with a method, path, params schema, effect, and auth alternatives |
| `check` | the name of a parameterless read-only GET used by **Test connection** |
| `hash` | SHA-256 of the canonical serialization of all of the above — **derived, never chosen** |

Three properties follow from this and are worth naming:

- **One declaration, many surfaces.** Studio's form, the server's validation,
  the sealed-secret extraction, the tools an agent sees, and the egress policy
  all read the *same* document. Nothing is written twice, so nothing drifts.
- **Content addressing.** An instance points at a manifest hash. A manifest
  cannot change under a connection that was configured against it.
- **The check is a probe, not a gate.** Storing an instance
  (`POST /connectors/instances`) validates the config against the
  `connection_specification` and seals its secrets; proving the configuration
  against the real system is the separate `POST /connectors/check` call
  (Studio's **Test connection** button). "Saved" means "validated and sealed";
  "answered" is a deliberate second step.

## The rules

### 1. A `connection_specification` must constrain

Enforced by `ConnectorManifest::validate`, and it is the rule that exists because
it was broken: an open schema (`{"type": "object"}`) validates every config, so a
caller can carry any shape it found convenient in an undeclared corner — which is
exactly how a hand-rolled `credentials.oauth` block got past a "validated"
connector once. The rule, at every object level including inside `oneOf`:

- `additionalProperties: false` (or a schema for the values, for a genuine map).
  An undeclared field is a refusal, not a silent extra.
- at least one `required` field, when the object declares any properties. An
  object with no properties is already exact — the only config it accepts is `{}`,
  which is the honest shape for a connector that needs no configuration.

### 2. Alternatives are declared, not improvised

More than one way to authenticate is normal. It is expressed as a `oneOf` over
closed objects, each fixing a discriminator (`"auth": {"const": "basic"}`) so the
shapes are told apart by the schema and not by guessing. On the operation side,
`auth` is an ordered list of alternatives.

**Selection is a question about shapes, answered without touching the network.**
An alternative applies when every placeholder it uses names a field this config
carries. Only then is it performed — and a failure *there* is fatal and says what
the other system said. (Before this split, a refused OAuth token exchange fell
through to the next alternative and surfaced as "no alternative's placeholders
resolve against this config", which blamed the operator's config for the identity
provider's answer. An hour was lost to that message.)

### 3. Secrets are marked, sealed, and never come back

`rusty_secret: true` on a property means: Studio renders it as a secret, the
server extracts it before anything persists, seals it through the broker, and
serves the instance back with `{"rusty_secret": true}` in its place. The studio
cannot read a saved secret, and neither can a run's journal.

### 4. Every operation declares its effect

`read_only` · `idempotent` · `compensatable` · `irreversible`. This is what the
gate, the approval policy and the receipt all read. An operation without an
honest effect is a governance hole, not a convenience.

### 5. The hash is the server's to compute

A manifest written by hand — in Studio's manifest field, in a catalog file, by
any client that is not this Rust workspace — arrives with no `hash`, and the
registration door seals it (validates, canonically orders the operations,
computes the hash). A manifest that *carries* a hash still has to prove it.
Requiring the client to compute a canonical hash would make the connector surface
Rust-only, which is the opposite of a standard.

### 6. Wire names are what an author would write

`style: "oauth2_client_credentials"`, not the derived `o_auth2_client_credentials`.
Nobody authoring a manifest by hand would guess the second one, and a standard
that cannot be written by hand is not a standard.

### 7. Presentation order is `rusty_order`

A JSON object has no order: the server's `Value` is a sorted map and the canonical
hash sorts keys. So a form built from a spec with no `rusty_order` renders
alphabetically — Password above Username, which reads as a bug. **Set
`rusty_order` on every property of every connector we ship.**

### 8. A tool is named `{connector}.{operation}`, everywhere

The name a builder picks in the catalog is the name the agent registry offers
and the name the model calls. A dot, because every model provider accepts one
in a tool name and most reject a slash. A second connection to the same
connector is `{connector}@{instance}.{operation}` — the two derive the same
operations and are not interchangeable, so they do not share a name.

### 9. Connections are tools without a restart

The graph's registry is built once; the systems an agent can reach are not.
`ToolRegistry` takes a live `ToolSource`, consulted on every read — the model's
schema list, dispatch, and the catalog a run validates its allowlist against.
The server's `ConnectionTools` is that source: every stored connection's read
operations, credentials opened for this process only, refilled at boot and
whenever a connection is created, granted or refreshed. A statically
registered tool always wins a name collision — a source extends a graph, never
shadows it. A granted connection nobody has approved yet contributes nothing:
it is a connection in name only.

Only the default tenant's connections are sourced today. The registry is one
per process; a per-tenant tool view is authorization work (G13).

### 10. A write runs through the effect it declares

Every operation that is not the check is a tool, reads and writes alike. What
governs a write is not a refusal at the connector but the effect the manifest
declared, admitted by the executor: a `compensatable` call is admitted with its
compensation registered; an `irreversible` one is refused until an
`ApprovalToken` names that exact call. Arguments the path did not consume
travel as the query string on a GET or DELETE and as a JSON body on a POST,
PUT or PATCH — keys sorted, because the journal hashes the bytes.

### 11. Egress is on by default

The egress engine is an allow-list: a host with no endpoint policy is denied,
and a host that resolves to a private, loopback or link-local address is
refused at preflight unless pinned. What used to be missing was a policy at
all — with none configured, nothing was checked. Now one always exists: **every
configured connection's host** (its API root and its token endpoint), plus
whatever the operator allow-listed (`RUSTY_EGRESS_ALLOW`), and nothing else. A
connection is an operator's decision to reach a host; that decision is the
policy. It is recomputed whenever a connection is created, granted, rotated or
revoked, together with the connection tools.

A pre-save check may reach the candidate's own host — the operator is testing
exactly that host before storing it — but only where the policy is the one the
connections define. An allow-list the operator wrote is authoritative, and a
check does not widen it.

Preflight pins the **socket**, never the URL. The request keeps its hostname
for SNI, certificate verification and the Host header, and the connection goes
to precisely the address preflight approved. (Rewriting the URL to the pinned
IP failed every https host the moment a policy was on — a latent bug that only
surfaced once egress was actually enforced.)

## The presentation extensions

Ignored by validators, read by Studio:

| Key | Meaning |
| --- | --- |
| `rusty_secret` | this value is a secret: masked, sealed, never served back |
| `rusty_order` | the order to render this property in |
| `rusty_group` | a heading to gather properties under |
| `rusty_pattern_descriptor` | what the `pattern` means, in words, as a placeholder |
| `rusty_hidden` | present in the schema, not shown |

## The library, and the connector that is not in it

A schema-driven form is *how* a connector is configured; it is not what makes a
connector. Asking every builder to write a manifest is the same failure as
asking them to write Rust — so the product ships a **library**, and everything
in `catalog/*/manifest.json` is registered at boot. Adding a connector to Rusty
is adding a file: no code, no migration, no path of its own.

A connector is identified in the library by `id@version`. Registering different
content under the same identity **supersedes** it — the library shows one row
per connector, not one per edit — while the superseded manifest stays
resolvable by hash, because the connections configured against it still point
there. A connection therefore carries its connector's identity, rather than
asking the client to resolve a hash that may no longer be listed.

For a system the library does not carry, `POST /connectors/openapi` reads an
OpenAPI 3.x document into a draft manifest: operations with an `operationId`
become tools, the rest come back listed as unmapped, and the caller picks one
of four auth styles (bearer, basic, header, query) which becomes both the
connection specification and every operation's auth. Because a published
document almost never declares a parameterless read, the check operation is
*derived* — same method and path as a listing read, called with no parameters.
Nothing is invented: the path is the document's own.

## The grant: a connection that is authorized, not typed

Some systems issue no credential a person can type. What a builder has is a
client id and secret; the credential is minted after a *person* approves the
request. A `connection_specification` renders fields — it can no more express
that than a form can express a conversation. So the manifest declares the round
trip separately:

```json
"authorization": {
  "authorize_url": "https://slack.com/oauth/v2/authorize",
  "token_url": "https://slack.com/api/oauth.v2.access",
  "scopes": "channels:read,users:read,chat:write",
  "client_id": "{credentials.client_id}",
  "client_secret": "{credentials.client_secret}",
  "extra_params": {}
}
```

The shape is **configure → authorize → connected**. Configure saves what the
builder has. `POST /connectors/instances/{id}/authorize` mints a single-use
state and returns the provider's own consent URL. The provider redirects a
browser to `GET /connectors/oauth/callback` — **public**, because that redirect
carries no credential of ours and the state is what makes the answer
trustworthy — and the exchanged tokens are sealed into the connection.

From that moment the connector is an ordinary bearer-token connector: its
operations authenticate with `{credentials.access_token}` and nothing
downstream knows the difference. **A grant is a way of obtaining a config, not
a new kind of call.** An expired token is refreshed before a check runs, so
"does this still work" answers for the credentials and not for the clock. The
token fields are declared in the specification but `rusty_hidden`, so nobody is
asked to type what the provider will issue.

A connection therefore reports what it is worth without anyone pressing a
button: `needs_auth`, `connected` (with the expiry and whether it refreshes),
or `expired`. `RUSTY_PUBLIC_URL` is the origin the callback is registered at
with the provider.

## Where the standard is still thin

Honest list, in the order it will hurt:

1. **No token caching for `oauth2_client_credentials`.** `oauth2_client_credentials` mints a token per request,
   on a fresh thread with its own runtime. It works; it will not survive volume,
   and it makes every call as slow as the identity provider.
3. **Manifests cannot be removed or superseded.** Registering twice leaves two
   rows with different hashes and the same display name, and there is no delete.
   The same is true of instances.
5. **No pagination contract.** Every connector expresses cursors in its own
   params (`cursor`, `sysparm_offset`, `nextPageToken`); nothing in the standard
   says how an agent should follow them.
6. **No rate-limit or retry contract.** A 429 is a failed call, not a wait.
7. **The check is a single GET.** It proves reachability and credentials. It does
   not prove scope — a token that can read one table and nothing else passes.
