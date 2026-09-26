#!/usr/bin/env python3
"""Generate catalog/plugins/it-service-desk: connector manifests written from
the vendors' documented REST APIs, and method skills with per-platform
reference files. Manifests carry no hash; the server seals them at install."""
import json, os, textwrap


def spec(title, props, required, order0=None):
    return {"$schema": "http://json-schema.org/draft-07/schema#", "title": title, "type": "object",
            "required": required, "additionalProperties": False, "properties": props}

def secret(title, order, hidden=False, desc=None):
    p = {"type": "string", "title": title, "rusty_secret": True, "rusty_order": order}
    if hidden: p["rusty_hidden"] = True
    if desc: p["description"] = desc
    return p

def plain(title, order, desc=None, pattern=None):
    p = {"type": "string", "title": title, "rusty_order": order}
    if desc: p["description"] = desc
    if pattern: p["rusty_pattern_descriptor"] = pattern
    return p

def creds_client(title, desc):
    return {"type": "object", "title": title, "description": desc, "required": ["client_id", "client_secret"],
            "additionalProperties": False, "rusty_order": 1,
            "properties": {"client_id": secret("Client ID", 0), "client_secret": secret("Client secret", 1)}}

def creds_oauth_app(title, desc):
    # The shape the Slack connector uses: the person types the app's id and
    # secret; the tokens are issued when they authorize and never typed.
    return {"type": "object", "title": title, "description": desc, "required": ["client_id", "client_secret"],
            "additionalProperties": False, "rusty_order": 1,
            "properties": {"client_id": secret("Client ID", 0), "client_secret": secret("Client secret", 1),
                           "access_token": secret("Access token", 2, hidden=True), "refresh_token": secret("Refresh token", 3, hidden=True),
                           "expires_at": {"type": "string", "title": "Token expires", "rusty_hidden": True, "rusty_order": 4}}}

def op(name, description, method, path, effect, props=None, required=None, auth=None):
    o = {"name": name, "description": description, "method": method, "path": path, "effect": effect,
         "params_schema": {"type": "object"}}
    if props:
        o["params_schema"]["properties"] = props
        if required: o["params_schema"]["required"] = required
    if auth: o["auth"] = auth
    return o

def P(desc, typ="string", **kw):
    d = {"type": typ, "description": desc}; d.update(kw); return d

BEARER = [{"style": "bearer", "token": "{credentials.access_token}"}]

def manifest(id, display, description, doc, base, connection, ops, check, authorization=None):
    m = {"id": id, "version": "1", "display_name": display, "description": description, "documentation_url": doc,
         "base_url": base, "connection_specification": connection, "operations": ops, "check": check}
    if authorization: m["authorization"] = authorization
    return m


def skill(name, description, tools, deps, body, refs):
    fm = f"---\nname: {name}\ndescription: {description}\nallowed-tools: {', '.join(tools)}\n"
    if deps: fm += f"dependencies: {', '.join(deps)}\n"
    fm += "license: MIT\n---\n"
    return fm + textwrap.dedent(body).lstrip("\n"), {k: textwrap.dedent(v).lstrip("\n") for k, v in refs.items()}

COMMON = ["search_knowledge", "memory.recall", "memory.remember", "skills.read", "gaps.file", "servicenow.list-records", "servicenow.get-record"]
GAP = """
## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.
"""


def write_plugin(root, plugin_id, name, description, connectors, skills):
    os.makedirs(f"{root}/connectors", exist_ok=True)
    for cid, m in connectors.items():
        with open(f"{root}/connectors/{cid}.json", "w") as f: json.dump(m, f, indent=2, ensure_ascii=False); f.write("\n")
    for sname, (md, refs) in skills.items():
        d = f"{root}/skills/{sname}"; os.makedirs(f"{d}/references", exist_ok=True)
        open(f"{d}/SKILL.md", "w").write(md)
        for path, text in refs.items(): open(f"{d}/{path}", "w").write(text)
    plugin = {"id": plugin_id, "name": name, "version": "1.0.0", "publisher": "Rusty", "description": description,
              "connectors": [f"connectors/{cid}" + ".json" for cid in connectors], "skills": [f"skills/{n}" for n in skills]}
    json.dump(plugin, open(f"{root}/plugin.json", "w"), indent=2, ensure_ascii=False)
    tot = sum(len(open(os.path.join(r, f)).read()) for r, _, fs in os.walk(root) for f in fs)
    print(f"{plugin_id}: {len(connectors)} connectors, {len(skills)} skills, {sum(len(r) for _, r in skills.values())} references, {tot//1024} KB")
    for sname, (md, _) in skills.items():
        fm = md.split('---')[1]; tools = [l for l in fm.splitlines() if l.startswith('allowed-tools')][0].count(',') + 1
        desc = [l for l in fm.splitlines() if l.startswith('description')][0]
        assert tools <= 32 and len(desc.encode()) <= 1024, sname
        print(f"  {sname}: {tools} tools, description {len(desc.encode())} B")
