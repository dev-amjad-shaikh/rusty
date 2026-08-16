import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRootRoute, createRoute, createRouter, Outlet, RouterProvider } from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { SkillsPage } from "./SkillsPage";
import { clearPublishedMembers } from "./publishedMembers";
import { memberPathProblem, parseSkillFrontmatter } from "./PublishSkill";

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const root = createRootRoute({ component: Outlet });
  const skills = createRoute({ getParentRoute: () => root, path: "/skills", component: SkillsPage });
  const router = createRouter({ routeTree: root.addChildren([skills]), history: createMemoryHistory({ initialEntries: ["/skills"] }) });
  return render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
}

function json(value: unknown, status = 200) { return Promise.resolve(new Response(JSON.stringify(value), { status })); }

function hash(letter: string) { return "0123456789abcdef"[letter.charCodeAt(0) % 16].repeat(64); }

function metadata(name: string, revision = 1, description = `The ${name} skill.`) {
  return { name, description, revision, content_hash: hash(name.slice(0, 1)) };
}

function receipt(name: string, revision = 1, options: { revisions?: number; author?: string; warnings?: unknown[] } = {}) {
  const contentHash = hash(name.slice(0, 1));
  return {
    metadata: metadata(name, revision),
    name,
    revision,
    content_hash: contentHash,
    provenance: { source: { type: "registry", name: "rusty-server" }, author: options.author ?? "operator:ada", content_hash: contentHash },
    scan: { clean: !options.warnings?.length, warnings: options.warnings ?? [], warning_count: options.warnings?.length ?? 0 },
    revisions: options.revisions ?? revision,
  };
}

function connected() {
  useConnectionStore.setState({
    connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
    info: {
      service: "rusty-server", version: "1", checkpointer: "json_file", server_store: "json_file", store_path: "/tmp",
      graphs: [
        { name: "research", channels: [], tools: [
          { name: "calculator", description: "Perform bounded arithmetic.", effect: "pure" as const, parameters_schema: { type: "object" } },
          { name: "search_knowledge", description: "Search approved local references.", effect: "read_only" as const, parameters_schema: { type: "object" } },
        ] },
        { name: "digest", channels: [] },
      ],
    },
    dialogOpen: false,
  });
}

beforeEach(() => {
  clearPublishedMembers();
  useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", discoveryAttempt: 0, discoveryError: "", suggestedOrigin: "", dialogOpen: false });
});
afterEach(() => vi.unstubAllGlobals());

describe("Skills & Tools", () => {
  it("asks for a workspace before reading the registry", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "Open a workspace to review skills and tools" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Choose workspace" }));
    expect(useConnectionStore.getState().dialogOpen).toBe(true);
  });

  it("lists published skills in deterministic name order with receipt evidence and filters them", async () => {
    connected();
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/skills") return json({ skills: [metadata("web-research", 2), metadata("alpha-checks"), metadata("ledger-review")] });
      if (path === "/skills/web-research") return json(receipt("web-research", 2, { revisions: 2, author: "operator:ada" }));
      if (path === "/skills/alpha-checks") return json(receipt("alpha-checks", 1, { author: "pipeline:nightly" }));
      if (path === "/skills/ledger-review") return json(receipt("ledger-review", 1, {
        warnings: [{ severity: "warning", kind: "base64_blob", location: "SKILL.md", detail: "a 96-character base64 run at offset 412" }],
      }));
      return json({ message: "unexpected" }, 404);
    }));
    renderPage();

    const table = await screen.findByRole("table", { name: "Skill library" });
    await waitFor(() => expect(table).toHaveTextContent("operator:ada"));
    const rows = screen.getAllByRole("row").filter((row) => row.tagName === "ARTICLE");
    expect(rows.map((row) => row.querySelector("b")?.textContent)).toEqual(["alpha-checks", "ledger-review", "web-research"]);
    expect(table).toHaveTextContent("r2");
    expect(table).toHaveTextContent("pipeline:nightly");
    expect(table).toHaveTextContent("registry:rusty-server");
    expect(screen.getAllByText("clean", { selector: "span" }).length).toBe(2);
    expect(screen.getByText("1 warning")).toBeVisible();
    expect(screen.getByText("registry · 3 admitted")).toBeVisible();

    await userEvent.type(screen.getByLabelText("Filter skills"), "ledger");
    expect(screen.queryByText("alpha-checks")).not.toBeInTheDocument();
    expect(screen.getByText("ledger-review")).toBeVisible();
    await userEvent.clear(screen.getByLabelText("Filter skills"));
    await userEvent.type(screen.getByLabelText("Filter skills"), "nothing-matches");
    expect(await screen.findByRole("heading", { name: "No skills match this filter" })).toBeVisible();
  });

  it("shows the empty registry with a publish call to action and an error state with retry", async () => {
    connected();
    let attempts = 0;
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => {
      attempts += 1;
      return attempts === 1 ? json({ message: "store unavailable" }, 500) : json({ skills: [] });
    }));
    renderPage();
    expect(await screen.findByRole("heading", { name: "Skills could not be loaded" })).toBeVisible();
    expect(screen.getByText("store unavailable")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("heading", { name: "No skills published yet" })).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Publish first skill" }));
    expect(await screen.findByLabelText("SKILL.md")).toBeVisible();
  });

  it("opens the detail receipt, lazily loads the body on demand, and pins a revision from history", async () => {
    connected();
    const fetchMock = vi.fn().mockImplementation((input: string) => {
      const path = new URL(input).pathname;
      if (path === "/skills") return json({ skills: [metadata("web-research", 2)] });
      if (path === "/skills/web-research") return json(receipt("web-research", 2, { revisions: 2 }));
      if (path === "/skills/web-research/history") return json({ name: "web-research", history: [metadata("web-research", 1), metadata("web-research", 2, "Search, then cite.")] });
      if (path === "/skills/web-research/versions/1") {
        const pinned = receipt("web-research", 1);
        delete (pinned as Record<string, unknown>).revisions;
        return json(pinned);
      }
      if (path === "/skills/web-research/body") return json({ name: "web-research", revision: 2, content_hash: hash("w"), body: "Search, then summarize.\n" });
      return json({ message: "unexpected" }, 404);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Open web-research" }));
    const drawer = await screen.findByRole("dialog", { name: "web-research" });
    await waitFor(() => expect(drawer).toHaveTextContent("r2 of 2"));
    expect(drawer).toHaveTextContent(hash("w"));
    expect(drawer).toHaveTextContent("operator:ada");
    expect(drawer).toHaveTextContent("clean — no findings");
    expect(fetchMock.mock.calls.some((call) => new URL(String(call[0])).pathname === "/skills/web-research/body")).toBe(false);

    await userEvent.click(screen.getByRole("button", { name: "Load instructions" }));
    expect(await screen.findByLabelText("Skill instructions")).toHaveTextContent("Search, then summarize.");
    expect(fetchMock.mock.calls.some((call) => new URL(String(call[0])).pathname === "/skills/web-research/body")).toBe(true);

    await userEvent.click(await screen.findByRole("button", { name: /r1/ }));
    expect(await screen.findByText(/Viewing pinned revision r1 of r2/)).toBeVisible();
    expect(screen.queryByRole("button", { name: "Load instructions" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Back to latest" }));
    expect(await screen.findByLabelText("Skill instructions")).toBeVisible();

    await userEvent.click(screen.getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("publishes a package with hex-encoded assets and lands on the new skill's detail with its members", async () => {
    connected();
    let posted: Record<string, unknown> | null = null;
    const fetchMock = vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (init?.method === "POST" && path === "/skills") {
        posted = JSON.parse(String(init.body));
        const published = receipt("web-research", 1) as Record<string, unknown>;
        delete published.revisions;
        return json({ ...published, already_registered: false }, 201);
      }
      if (path === "/skills") return json({ skills: posted ? [metadata("web-research")] : [] });
      if (path === "/skills/web-research") return json(receipt("web-research", 1));
      if (path === "/skills/web-research/history") return json({ name: "web-research", history: [metadata("web-research")] });
      if (path === "/skills/web-research/files/references/guide.md") {
        return Promise.resolve(new Response("# Guide\n\nDetails on demand.\n", { status: 200, headers: { "content-type": "text/markdown; charset=utf-8" } }));
      }
      return json({ message: "unexpected" }, 404);
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Publish first skill" }));
    const editor = screen.getByLabelText("SKILL.md");
    await userEvent.clear(editor);
    await userEvent.type(editor, "---\nname: web-research\ndescription: Search, then summarize.\n---\n\nSearch, then summarize.");
    expect(await screen.findByText(/name · web-research/)).toBeVisible();
    expect(screen.getByText(/body · 2\d bytes/)).toBeVisible();

    await userEvent.click(screen.getByRole("button", { name: "Add a reference or asset" }));
    await userEvent.type(screen.getByLabelText("Path"), "guide.md");
    await userEvent.type(screen.getByLabelText("Content for guide.md"), "# Guide");
    await userEvent.click(screen.getByRole("button", { name: "Add a reference or asset" }));
    const kindSelects = screen.getAllByLabelText("Kind");
    await userEvent.selectOptions(kindSelects[1], "asset");
    await userEvent.type(screen.getAllByLabelText("Path")[1], "seal.txt");
    await userEvent.type(screen.getByLabelText("Content for seal.txt"), "AB");
    await userEvent.type(screen.getByLabelText("Author"), "operator:ada");
    await userEvent.click(screen.getByRole("button", { name: "Publish skill" }));

    const drawer = await screen.findByRole("dialog", { name: "web-research" });
    await waitFor(() => expect(drawer).toHaveTextContent("r1 of 1"));
    expect(posted).toMatchObject({
      skill_md: "---\nname: web-research\ndescription: Search, then summarize.\n---\n\nSearch, then summarize.",
      references: { "guide.md": "# Guide" },
      assets: { "seal.txt": "4142" },
      author: "operator:ada",
    });

    await userEvent.click(screen.getByRole("button", { name: "references/guide.md" }));
    expect(await screen.findByLabelText("Member content")).toHaveTextContent("# Guide");
    expect(screen.getByText(/references\/guide\.md · 28 bytes · text\/markdown/)).toBeVisible();
  });

  it("renders scan denial findings readably and keeps the draft intact", async () => {
    connected();
    vi.stubGlobal("fetch", vi.fn().mockImplementation((input: string, init?: RequestInit) => {
      const path = new URL(input).pathname;
      if (init?.method === "POST" && path === "/skills") {
        return json({
          error: "scan_denied",
          message: "the security scan denied the package: 1 denial(s)",
          findings: [{ severity: "denial", kind: "credentialed_url", location: "references/feed.md", detail: "credentialed URL with host internal.example at offset 17" }],
        }, 422);
      }
      if (path === "/skills") return json({ skills: [metadata("web-research")] });
      if (path === "/skills/web-research") return json(receipt("web-research"));
      return json({ message: "unexpected" }, 404);
    }));
    renderPage();

    await screen.findByRole("table", { name: "Skill library" });
    await userEvent.click(screen.getByRole("button", { name: "Publish skill" }));
    const editor = screen.getByLabelText("SKILL.md");
    await userEvent.clear(editor);
    await userEvent.type(editor, "---\nname: risky-skill\ndescription: Fetches a feed.\n---\n\nFetch the feed.");
    await userEvent.type(screen.getByLabelText("Author"), "operator:ada");
    await userEvent.click(screen.getByRole("button", { name: "Publish skill" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The security scan denied this package");
    expect(alert).toHaveTextContent("Credentialed URL · denial");
    expect(alert).toHaveTextContent("references/feed.md");
    expect(alert).toHaveTextContent("host internal.example");
    expect(screen.getByLabelText("SKILL.md")).toHaveValue("---\nname: risky-skill\ndescription: Fetches a feed.\n---\n\nFetch the feed.");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("reviews the tool catalog per behavior with honest empty states", async () => {
    connected();
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => json({ skills: [] })));
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Tools" }));
    expect(await screen.findByRole("heading", { name: "Research" })).toBeVisible();
    const tools = screen.getByRole("table", { name: "Tools included by Research" });
    expect(tools).toHaveTextContent("calculator");
    expect(tools).toHaveTextContent("Search approved local references.");
    expect(screen.getByText("Pure")).toBeVisible();
    expect(screen.getByText("Read only")).toBeVisible();

    expect(screen.getByRole("heading", { name: "Digest" })).toBeVisible();
    expect(screen.getByText(/This behavior includes no executable tools/)).toBeVisible();
  });

  it("validates frontmatter and member paths before submit", () => {
    const missing = parseSkillFrontmatter("Just text, no block.");
    expect(missing.hasBlock).toBe(false);
    expect(missing.issues[0]).toMatch(/frontmatter block/);

    const open = parseSkillFrontmatter("---\nname: a\n");
    expect(open.issues[0]).toMatch(/Close the frontmatter/);

    const bad = parseSkillFrontmatter("---\nname: Bad Name\n---\n\nBody.");
    expect(bad.issues).toContain("The name must be kebab-case: lowercase letters, digits, and single dashes.");
    expect(bad.issues).toContain("Declare description: — it is the discovery text agents read first.");

    const good = parseSkillFrontmatter('---\nname: "web-research"\ndescription: Search.\n---\n\nBody.');
    expect(good).toMatchObject({ name: "web-research", description: "Search.", issues: [] });
    expect(good.bodyBytes).toBe(5);

    expect(memberPathProblem("")).toMatch(/required/);
    expect(memberPathProblem("../secret.md")).toMatch(/\.\./);
    expect(memberPathProblem("/abs.md")).toMatch(/leading slash/);
    expect(memberPathProblem("a\\b.md")).toMatch(/forward slashes/);
    expect(memberPathProblem("nested/guide.md")).toBe("");
  });
});
