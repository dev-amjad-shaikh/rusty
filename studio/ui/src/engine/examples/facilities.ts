import { createAssistant, createConnectorInstance, createWorld, listAssistants, listConnectorManifests, listWorlds, registerConnectorManifest, registerSkill } from "../net/client";
import { composeSkillMd } from "../forms/skillDraft";

/**
 * The example estate: a facilities desk with a stand-in, a skill that says
 * how to triage, and two agents — one that triages and files, one that
 * fronts for people and asks it. Loaded through the same endpoints a
 * person uses from the studio, one after another, so nothing about it is
 * special; a fresh deployment gets something to try, and the walk that
 * proved this product (describe → shape → try → decide → observe) can be
 * taken on any machine. Loading twice adds nothing: each piece is made
 * only when its name is not there yet.
 */
export const FACILITIES_MANIFEST = {
  id: "facilities-desk",
  version: "1",
  display_name: "Facilities Desk",
  description: "The facilities desk's tickets: read what is open, file what is missing.",
  documentation_url: "https://facilities.example.internal/docs",
  base_url: "https://facilities.example.internal",
  connection_specification: { $schema: "http://json-schema.org/draft-07/schema#", type: "object", required: ["token"], properties: { token: { type: "string", title: "Desk API token" } }, additionalProperties: false },
  operations: [
    { name: "whoami", description: "Who this token belongs to.", method: "GET", path: "/me", effect: "read_only", params_schema: { type: "object" }, headers: [], auth: [{ style: "bearer", token: "{token}" }], max_response_bytes: null },
    { name: "list-tickets", description: "The desk's tickets, optionally by status.", method: "GET", path: "/tickets", effect: "read_only", params_schema: { type: "object", properties: { status: { type: "string" } } }, headers: [], auth: [{ style: "bearer", token: "{token}" }], max_response_bytes: null },
    { name: "create-ticket", description: "File a ticket: a one-line title and the room.", method: "POST", path: "/tickets", effect: "irreversible", params_schema: { type: "object", required: ["title"], properties: { title: { type: "string" }, room: { type: "string" } } }, headers: [], auth: [{ style: "bearer", token: "{token}" }], max_response_bytes: null },
  ],
  check: "whoami",
};

export const DESK_TRIAGE_SKILL = composeSkillMd({
  name: "desk-triage",
  description: "How a facilities desk answers a reported problem: read first, file once, say what was already covered.",
  license: "",
  evalGate: "",
  tools: ["facilities-desk.list-tickets", "facilities-desk.create-ticket"],
  body: `# Desk triage

When a person reports a facilities problem (a room, a piece of equipment), do this and nothing else:

1. Call facilities-desk.list-tickets first and look for an open ticket about the same room and problem.
2. If one exists, answer with its number and its title. Do not file another.
3. If none exists, call facilities-desk.create-ticket exactly once with a one-line title and the room.
4. Answer with the number the desk returned, and say which problems were already covered.

Done looks like: every problem the person named is either matched to an existing open ticket or filed once — never twice, never for a problem that already has an open ticket.`,
});

export const EXAMPLE_AGENTS = [
  {
    name: "Facilities Triage",
    description: "Reads the facilities desk before it files: an existing ticket is answered, a new problem is filed once.",
    instructions: "You are the facilities triage agent. For every problem a person reports, follow the desk-triage skill exactly; it says when to read, when to file, and how to answer.",
    tools: [{ name: "facilities-desk.list-tickets", when: "before anything else, to see what is already open" }, { name: "facilities-desk.create-ticket", when: "once, for a problem no open ticket covers" }],
    skills: ["desk-triage"],
  },
  {
    name: "Front Desk",
    description: "Takes any building request and hands facilities problems to Facilities Triage; answers with what the triage desk said.",
    instructions: "You are the front desk. You have no desk tools of your own. When a person reports a facilities problem, work out the room, then call agents.ask with agent \"Facilities Triage\" and the problem with the room named, and answer the person with what the triage desk said, naming any ticket number. For anything else, answer directly.",
    tools: [{ name: "agents.ask", when: "for any facilities problem, with the room named" }],
    skills: [] as string[],
  },
];

export const EXAMPLE_WORLD = "facilities-twin";

/** What the twin holds before anyone tries it: a desk with two open tickets
 * in other rooms, so a reported problem is plainly new or plainly covered.
 * A believable desk, not the library's placeholder rows: measured 2026-09-13,
 * the model re-read a desk holding "Example room / Example title" five times
 * of five and filed nothing; against these rows it filed four of five, and
 * the verifier's repair turn covers the fifth. */
export const EXAMPLE_SEED = {
  tables: {
    me: [{ id: 1, name: "Facilities Desk", email: "desk@facilities.example.internal" }],
    tickets: [
      { id: 1, number: 1, room: "1B", title: "Vending machine jammed", status: "open" },
      { id: 2, number: 2, room: "3F kitchen", title: "Tap drips over the sink", status: "open" },
    ],
  },
  counters: { tickets: 3 },
};

/** What loading made, or found already there. */
export interface ExampleLoaded {
  made: string[];
  found: string[];
}

/** Load the example, piece by piece, saying what was made and what was
 * already there. `graph` is the graph the studio creates agents on. */
export async function loadFacilitiesExample(graph: string, say?: (line: string) => void): Promise<ExampleLoaded> {
  const made: string[] = [];
  const found: string[] = [];
  const tell = (line: string) => say?.(line);

  tell("Registering the Facilities Desk connector…");
  const manifests = await listConnectorManifests();
  let hash = manifests.find((m) => m.id === FACILITIES_MANIFEST.id)?.hash;
  if (hash) found.push("the Facilities Desk connector");
  else { hash = (await registerConnectorManifest(FACILITIES_MANIFEST)).hash; made.push("the Facilities Desk connector"); }

  tell("Making the stand-in world…");
  const worlds = await listWorlds();
  if (worlds.some((w) => w.name === EXAMPLE_WORLD)) found.push(`the world ${EXAMPLE_WORLD}`);
  else {
    await createWorld({ name: EXAMPLE_WORLD, connector: FACILITIES_MANIFEST.id, dialect: "manifest-rest", seed: EXAMPLE_SEED });
    made.push(`the world ${EXAMPLE_WORLD}`);
  }

  tell("Connecting the desk (an example token; the stand-in answers)…");
  const instances = await import("../net/client").then((c) => c.listConnectorInstances());
  if (instances.some((i) => i.manifest_hash === hash)) found.push("the connection");
  else { await createConnectorInstance(hash, { token: "example-token" }); made.push("the connection"); }

  tell("Registering the desk-triage skill…");
  try {
    const registered = await registerSkill(DESK_TRIAGE_SKILL);
    (registered.already_registered ? found : made).push("the desk-triage skill");
  } catch { found.push("the desk-triage skill"); }

  const existing = await listAssistants();
  for (const agent of EXAMPLE_AGENTS) {
    tell(`Creating ${agent.name}…`);
    if (existing.some((a) => a.name === agent.name)) { found.push(agent.name); continue; }
    await createAssistant({ name: agent.name, graph, config: { studio_intent: { instructions: agent.instructions, tools: agent.tools, ...(agent.skills.length ? { skills: agent.skills } : {}) } }, metadata: { description: agent.description } });
    made.push(agent.name);
  }
  return { made, found };
}
