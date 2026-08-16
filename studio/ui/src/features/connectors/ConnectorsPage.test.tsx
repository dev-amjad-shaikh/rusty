import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { StudioApiError } from "../../lib/api/client";
import {
  checkConnectorInstanceHealth,
  connectConnectorInstance,
  createConnectorInstance,
  disableConnectorInstance,
  enableConnectorInstance,
  getInstanceCatalog,
  listConnectorInstances,
  listConnectorManifests,
  listVaultConnections,
  registerConnectorManifest,
  registerVaultConnection,
  sweepConnectors,
  type ConnectorInstance,
  type ConnectorManifest,
  type VaultConnection,
} from "../../lib/api/connectors";
import { useConnectionStore } from "../../state/connection";
import { ConnectorsPage } from "./ConnectorsPage";

vi.mock("../../lib/api/connectors", async (importActual) => {
  const actual = await importActual<typeof import("../../lib/api/connectors")>();
  return {
    ...actual,
    listConnectorManifests: vi.fn(),
    registerConnectorManifest: vi.fn(),
    listConnectorInstances: vi.fn(),
    createConnectorInstance: vi.fn(),
    connectConnectorInstance: vi.fn(),
    checkConnectorInstanceHealth: vi.fn(),
    disableConnectorInstance: vi.fn(),
    enableConnectorInstance: vi.fn(),
    sweepConnectors: vi.fn(),
    getInstanceCatalog: vi.fn(),
    listVaultConnections: vi.fn(),
    registerVaultConnection: vi.fn(),
  };
});

const manifestHash = "ab".repeat(32);
const manifest: ConnectorManifest = {
  id: "brave-search",
  version: "1.0.0",
  display_name: "Brave Search",
  description: "Bounded web search over an HTTPS endpoint.",
  provider: { kind: "http_search", base_url: "https://api.search.example.com/search", auth: { header: "X-Api-Key", credential_slot: "api_key" } },
  capabilities: ["web search"],
  credential_slots: [{ name: "api_key", description: "Search API key issued to this tenant" }],
  hash: manifestHash,
};

function instance(overrides: Partial<ConnectorInstance>): ConnectorInstance {
  return {
    instance_id: "inst-000001",
    connector_id: "brave-search",
    manifest_hash: manifestHash,
    credential_slots: ["api_key"],
    state: "pending",
    state_reason: null,
    consecutive_failures: 0,
    last_health_check_ms: null,
    catalog_generation: null,
    catalog_hash: null,
    created_at: "2026-08-11T00:00:00Z",
    updated_at: "2026-08-11T00:00:00Z",
    ...overrides,
  };
}

const catalogGeneration2 = {
  instance_id: "inst-000001",
  generation: 2,
  hash: "cd".repeat(32),
  tools: [
    { name: "brave-search/search", description: "Search the web.", parameters_schema: { type: "object" }, effect: "read_only" as const },
    { name: "brave-search/fetch", description: "Fetch one page.", parameters_schema: { type: "object" }, effect: "non_idempotent" as const },
  ],
};

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><ConnectorsPage /></QueryClientProvider>);
}

function openWorkspace() {
  useConnectionStore.setState({
    connection: { epoch: 1, origin: "https://rusty.example", apiKey: "key", tenantFingerprint: "a" },
    info: null,
  });
}

async function rowFor(instanceId: string) {
  const marker = await screen.findByText(instanceId);
  const row = marker.closest("article");
  expect(row).not.toBeNull();
  return within(row!);
}

beforeEach(() => {
  useConnectionStore.setState({ connection: null, info: null, workspaceStatus: "unavailable", dialogOpen: false });
  vi.clearAllMocks();
  vi.mocked(listConnectorManifests).mockResolvedValue([]);
  vi.mocked(listConnectorInstances).mockResolvedValue([]);
  vi.mocked(listVaultConnections).mockResolvedValue([]);
});

describe("Connectors", () => {
  it("gates the plane behind an open workspace", () => {
    renderPage();
    expect(screen.getByRole("heading", { name: "Connectors" })).toBeVisible();
    expect(screen.getByText("Open a workspace to manage connectors.")).toBeVisible();
    expect(screen.getByRole("heading", { name: "Open a workspace to work with connectors" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Choose workspace" })).toBeVisible();
  });

  it("renders distinct empty states for the fleet and the gallery", async () => {
    openWorkspace();
    renderPage();
    expect(await screen.findByRole("heading", { name: "No instances yet" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "No manifests registered" })).toBeVisible();
    expect(screen.getByText(/secrets stay in the vault/i)).toBeVisible();
  });

  it("registers a manifest and admits the exact receipt", async () => {
    openWorkspace();
    vi.mocked(registerConnectorManifest).mockResolvedValue({
      id: "brave-search", version: "1.0.0", manifest_hash: manifestHash, already_registered: false,
    });
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Register manifest" }));
    await userEvent.click(screen.getByLabelText("Manifest JSON"));
    await userEvent.paste(JSON.stringify({
      id: "brave-search",
      version: "1.0.0",
      display_name: "Brave Search",
      description: "Bounded web search.",
      provider: { kind: "http_search", base_url: "https://api.search.example.com/search", auth: null },
      capabilities: ["web search"],
      credential_slots: [{ name: "api_key", description: "Search API key" }],
    }));
    await userEvent.click(screen.getByRole("button", { name: "Register manifest" }));
    await waitFor(() => expect(registerConnectorManifest).toHaveBeenCalledTimes(1));
    const payload = vi.mocked(registerConnectorManifest).mock.calls[0][1];
    expect(payload).toMatchObject({ id: "brave-search", provider: { kind: "http_search" } });
    expect(await screen.findByRole("status")).toHaveTextContent("Manifest registered.");
    expect(screen.getByRole("status")).toHaveTextContent(manifestHash.slice(0, 12));
  });

  it("renders manifest field errors and the server's 422 readably", async () => {
    openWorkspace();
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Register manifest" }));

    // Broken JSON never leaves the form.
    await userEvent.click(screen.getByLabelText("Manifest JSON"));
    await userEvent.paste("{ not json");
    await userEvent.click(screen.getByRole("button", { name: "Register manifest" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("The manifest needs attention");
    expect(registerConnectorManifest).not.toHaveBeenCalled();

    // Structurally invalid JSON names each offending field.
    await userEvent.clear(screen.getByLabelText("Manifest JSON"));
    await userEvent.click(screen.getByLabelText("Manifest JSON"));
    await userEvent.paste(JSON.stringify({ id: "brave-search" }));
    await userEvent.click(screen.getByRole("button", { name: "Register manifest" }));
    const fieldErrors = await screen.findByRole("alert");
    expect(fieldErrors).toHaveTextContent("version");
    expect(fieldErrors).toHaveTextContent("provider");
    expect(registerConnectorManifest).not.toHaveBeenCalled();

    // A valid payload refused by the server shows the 422 verbatim.
    vi.mocked(registerConnectorManifest).mockRejectedValue(
      new StudioApiError("connector id `Brave_Search` must be kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`) of at most 64 bytes", 422),
    );
    await userEvent.clear(screen.getByLabelText("Manifest JSON"));
    await userEvent.click(screen.getByLabelText("Manifest JSON"));
    await userEvent.paste(JSON.stringify({
      id: "Brave_Search",
      version: "1.0.0",
      display_name: "Brave Search",
      description: "Bounded web search.",
      provider: { kind: "http_search", base_url: "https://api.search.example.com/search", auth: null },
    }));
    await userEvent.click(screen.getByRole("button", { name: "Register manifest" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("must be kebab-case");
  });

  it("instantiates through vault connection bindings and never accepts raw secrets", async () => {
    openWorkspace();
    vi.mocked(listConnectorManifests).mockResolvedValue([manifest]);
    vi.mocked(listVaultConnections).mockResolvedValue([
      { connection_id: "conn-0123456789abcdef0123456789abcdef", provider: "api_key", scopes: ["search"], status: "active", health: { consecutive_failures: 0 }, created_at: "2026-08-11T00:00:00Z", updated_at: "2026-08-11T00:00:00Z" },
    ]);
    const created = instance({ instance_id: "inst-000002" });
    vi.mocked(createConnectorInstance).mockResolvedValue(created);
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    expect(panel).toHaveTextContent(manifestHash);
    expect(panel).toHaveTextContent("never secret material");
    expect(within(panel).queryByRole("textbox", { name: /secret|token|key/i })).not.toBeInTheDocument();

    const select = within(panel).getByLabelText(/api_key/);
    await waitFor(() => expect(select).toBeEnabled());
    await userEvent.selectOptions(select, "conn-0123456789abcdef0123456789abcdef");
    await userEvent.click(within(panel).getByRole("button", { name: "Instantiate connector" }));

    await waitFor(() => expect(createConnectorInstance).toHaveBeenCalledTimes(1));
    expect(vi.mocked(createConnectorInstance).mock.calls[0][1]).toEqual({
      manifest_hash: manifestHash,
      credentials: { api_key: "conn-0123456789abcdef0123456789abcdef" },
    });
    await waitFor(() => expect(screen.queryByRole("complementary")).not.toBeInTheDocument());
  });

  it("registers an oauth2_password connection from the instantiate panel and binds it to the slots", async () => {
    openWorkspace();
    vi.mocked(listConnectorManifests).mockResolvedValue([manifest]);
    const registered: VaultConnection = {
      connection_id: "conn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      provider: "oauth2_password",
      subject: "nexus.connector",
      scopes: [],
      status: "active",
      health: { consecutive_failures: 0 },
      created_at: "2026-08-16T10:00:00Z",
      updated_at: "2026-08-16T10:00:00Z",
    };
    // The vault starts empty; the receipt's invalidation re-lists it with the new record.
    vi.mocked(listVaultConnections).mockResolvedValueOnce([]).mockResolvedValue([registered]);
    vi.mocked(registerVaultConnection).mockResolvedValue(registered);
    vi.mocked(createConnectorInstance).mockResolvedValue(instance({ instance_id: "inst-000009" }));
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    expect(panel).toHaveTextContent("No vault connections in this workspace");
    await userEvent.click(within(panel).getAllByRole("button", { name: "Register a connection" })[0]);
    expect(await within(panel).findByRole("heading", { name: "Register a vault connection" })).toBeVisible();

    // Client-side validation: a plain-http token endpoint never leaves the form.
    await userEvent.click(within(panel).getByLabelText("Token endpoint URL"));
    await userEvent.paste("http://dev394299.service-now.com/oauth_token.do");
    await userEvent.click(within(panel).getByLabelText("OAuth client ID"));
    await userEvent.paste("client-id");
    await userEvent.click(within(panel).getByLabelText("OAuth client secret"));
    await userEvent.paste("client-secret");
    await userEvent.click(within(panel).getByLabelText("Username"));
    await userEvent.paste("nexus.connector");
    await userEvent.click(within(panel).getByLabelText("Password"));
    await userEvent.paste("account-password");
    await userEvent.click(within(panel).getByRole("button", { name: "Register connection" }));
    expect(await within(panel).findByRole("alert")).toHaveTextContent("https://");
    expect(registerVaultConnection).not.toHaveBeenCalled();

    await userEvent.clear(within(panel).getByLabelText("Token endpoint URL"));
    await userEvent.click(within(panel).getByLabelText("Token endpoint URL"));
    await userEvent.paste("https://dev394299.service-now.com/oauth_token.do");
    await userEvent.click(within(panel).getByRole("button", { name: "Register connection" }));

    await waitFor(() => expect(registerVaultConnection).toHaveBeenCalledTimes(1));
    expect(vi.mocked(registerVaultConnection).mock.calls[0][1]).toEqual({
      token_url: "https://dev394299.service-now.com/oauth_token.do",
      client_id: "client-id",
      client_secret: "client-secret",
      username: "nexus.connector",
      password: "account-password",
    });

    // The receipt stays visible until the operator binds it into the slots.
    const receipt = await within(panel).findByRole("status");
    expect(receipt).toHaveTextContent("Connection registered.");
    expect(receipt).toHaveTextContent("conn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    await userEvent.click(within(receipt).getByRole("button", { name: "Bind and continue" }));

    // Every still-empty slot is bound to the new connection id.
    const select = within(panel).getByLabelText(/api_key/);
    await waitFor(() => expect(select).toHaveValue("conn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    await userEvent.click(within(panel).getByRole("button", { name: "Instantiate connector" }));
    await waitFor(() => expect(createConnectorInstance).toHaveBeenCalledTimes(1));
    expect(vi.mocked(createConnectorInstance).mock.calls[0][1]).toEqual({
      manifest_hash: manifestHash,
      credentials: { api_key: "conn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
    });
  });

  it("reflects the server's missing-slot 422 inline on the named slot", async () => {
    openWorkspace();
    vi.mocked(listConnectorManifests).mockResolvedValue([manifest]);
    vi.mocked(listVaultConnections).mockResolvedValue([
      { connection_id: "conn-0123456789abcdef0123456789abcdef", provider: "api_key", scopes: [], status: "active", health: { consecutive_failures: 0 }, created_at: "2026-08-11T00:00:00Z", updated_at: "2026-08-11T00:00:00Z" },
    ]);
    vi.mocked(createConnectorInstance).mockRejectedValue(
      new StudioApiError("credential slot `api_key` requires a connection id in `credentials`", 422),
    );
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    await waitFor(() => expect(within(panel).getByLabelText(/api_key/)).toBeEnabled());
    await userEvent.click(within(panel).getByRole("button", { name: "Instantiate connector" }));

    const slotError = await within(panel).findByRole("alert");
    expect(slotError).toHaveTextContent("credential slot `api_key` requires a connection id");
    expect(within(panel).getByLabelText(/api_key/)).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByRole("complementary")).toBeInTheDocument();
  });

  it("gates lifecycle actions by state and surfaces 409 guard answers", async () => {
    openWorkspace();
    vi.mocked(listConnectorInstances).mockResolvedValue([
      instance({ instance_id: "inst-000001", state: "pending" }),
      instance({ instance_id: "inst-000002", state: "healthy", last_health_check_ms: 1_754_900_000_000, catalog_generation: 2, catalog_hash: "cd".repeat(32) }),
      instance({ instance_id: "inst-000003", state: "disabled" }),
    ]);
    vi.mocked(connectConnectorInstance).mockRejectedValue(new StudioApiError("connector instance `inst-000001` is disabled", 409));
    renderPage();

    const pending = await rowFor("inst-000001");
    expect(pending.getByRole("button", { name: "Connect" })).toBeVisible();
    expect(pending.getByRole("button", { name: "Disable" })).toBeVisible();
    expect(pending.queryByRole("button", { name: "Health check" })).not.toBeInTheDocument();
    expect(pending.queryByRole("button", { name: "Enable" })).not.toBeInTheDocument();

    const healthy = await rowFor("inst-000002");
    expect(healthy.getByRole("button", { name: "Health check" })).toBeVisible();
    expect(healthy.queryByRole("button", { name: "Connect" })).not.toBeInTheDocument();
    expect(healthy.getByText("gen 2")).toBeVisible();

    const disabled = await rowFor("inst-000003");
    expect(disabled.getByRole("button", { name: "Enable" })).toBeVisible();
    expect(disabled.queryByRole("button", { name: "Connect" })).not.toBeInTheDocument();
    expect(disabled.queryByRole("button", { name: "Disable" })).not.toBeInTheDocument();

    // A guard violation the gate could not foresee surfaces the 409 verbatim.
    await userEvent.click(pending.getByRole("button", { name: "Connect" }));
    expect(await pending.findByRole("alert")).toHaveTextContent("connector instance `inst-000001` is disabled");
  });

  it("renders the generation-pinned catalog with effect badges", async () => {
    openWorkspace();
    vi.mocked(listConnectorInstances).mockResolvedValue([
      instance({ state: "healthy", catalog_generation: 2, catalog_hash: "cd".repeat(32), last_health_check_ms: 1_754_900_000_000 }),
    ]);
    vi.mocked(getInstanceCatalog).mockResolvedValue(catalogGeneration2);
    renderPage();

    const row = await rowFor("inst-000001");
    await userEvent.click(row.getByRole("button", { name: "Catalog" }));
    expect(await screen.findByText("generation 2")).toBeVisible();
    expect(screen.getByText("brave-search/search")).toBeVisible();
    expect(screen.getByText("brave-search/fetch")).toBeVisible();
    expect(screen.getByText("read-only")).toBeVisible();
    expect(screen.getByText("non-idempotent")).toBeVisible();
    expect(screen.getByText(/Generations bump only when the derived catalog bytes change/)).toBeVisible();
    expect(getInstanceCatalog).toHaveBeenCalledWith(expect.anything(), "inst-000001", undefined);
  });

  it("handles a catalog pin mismatch by offering the live generation", async () => {
    openWorkspace();
    vi.mocked(listConnectorInstances).mockResolvedValue([
      instance({ state: "healthy", catalog_generation: 2, catalog_hash: "cd".repeat(32), last_health_check_ms: 1_754_900_000_000 }),
    ]);
    vi.mocked(getInstanceCatalog).mockImplementation((_connection, _id, generation) => {
      if (generation === 1) {
        return Promise.reject(new StudioApiError("catalog generation pin 1 does not match the live generation 2", 409));
      }
      return Promise.resolve(catalogGeneration2);
    });
    renderPage();

    const row = await rowFor("inst-000001");
    await userEvent.click(row.getByRole("button", { name: "Catalog" }));
    expect(await screen.findByText("generation 2")).toBeVisible();

    await userEvent.type(screen.getByLabelText("Pin generation"), "1");
    await userEvent.click(screen.getByRole("button", { name: "Load pinned" }));
    const mismatch = await screen.findByRole("alert");
    expect(mismatch).toHaveTextContent("catalog generation pin 1 does not match the live generation 2");

    await userEvent.click(within(mismatch).getByRole("button", { name: "Load live generation 2" }));
    expect(await screen.findByText("generation 2")).toBeVisible();
    expect(screen.getByText("brave-search/search")).toBeVisible();
  });

  it("answers the pre-catalog 404 without pretending a catalog exists", async () => {
    openWorkspace();
    vi.mocked(listConnectorInstances).mockResolvedValue([instance({ state: "pending" })]);
    vi.mocked(getInstanceCatalog).mockRejectedValue(
      new StudioApiError("connector instance `inst-000001` has served no catalog yet; connect it first", 404),
    );
    renderPage();
    const row = await rowFor("inst-000001");
    await userEvent.click(row.getByRole("button", { name: "Catalog" }));
    expect(await screen.findByText(/has served no catalog yet; connect it first/)).toBeVisible();
  });

  it("runs the tenant health sweep and renders per-instance outcomes", async () => {
    openWorkspace();
    vi.mocked(listConnectorInstances).mockResolvedValue([
      instance({ state: "healthy", catalog_generation: 1, catalog_hash: "cd".repeat(32) }),
    ]);
    vi.mocked(sweepConnectors).mockResolvedValue([
      {
        instance_id: "inst-000001",
        previous_state: { state: "healthy", reason: null },
        current_state: { state: "degraded", reason: "health check timed out" },
        catalog_bumped: true,
      },
    ]);
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Run health sweep" }));
    expect(await screen.findByRole("heading", { name: "1 instance re-checked" })).toBeVisible();
    expect(screen.getByText("healthy → degraded")).toBeVisible();
    expect(screen.getByText("health check timed out")).toBeVisible();
    expect(screen.getByText("catalog bumped")).toBeVisible();
    await waitFor(() => expect(listConnectorInstances).toHaveBeenCalledTimes(2));
  });

  it("names an empty sweep instead of implying failure", async () => {
    openWorkspace();
    vi.mocked(sweepConnectors).mockResolvedValue([]);
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Run health sweep" }));
    expect(await screen.findByRole("heading", { name: "Nothing to re-check" })).toBeVisible();
    expect(screen.getByText(/Pending, failed, and disabled instances are not swept/)).toBeVisible();
  });

  it("keeps other lifecycle mutations honest", async () => {
    openWorkspace();
    vi.mocked(listConnectorInstances).mockResolvedValue([
      instance({ state: "healthy", catalog_generation: 1, catalog_hash: "cd".repeat(32) }),
    ]);
    vi.mocked(checkConnectorInstanceHealth).mockResolvedValue({
      outcome: { instance_id: "inst-000001", previous_state: { state: "healthy", reason: null }, current_state: { state: "healthy", reason: null }, catalog_bumped: false },
      instance: instance({ state: "healthy" }),
    });
    vi.mocked(disableConnectorInstance).mockResolvedValue(instance({ state: "disabled" }));
    vi.mocked(enableConnectorInstance).mockResolvedValue(instance({ state: "pending" }));
    renderPage();

    const row = await rowFor("inst-000001");
    await userEvent.click(row.getByRole("button", { name: "Health check" }));
    await waitFor(() => expect(checkConnectorInstanceHealth).toHaveBeenCalledWith(expect.anything(), "inst-000001"));
    await userEvent.click(row.getByRole("button", { name: "Disable" }));
    await waitFor(() => expect(disableConnectorInstance).toHaveBeenCalledWith(expect.anything(), "inst-000001"));
  });
});
