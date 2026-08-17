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
  registerApiKeyConnection,
  registerBasicConnection,
  registerConnectorManifest,
  registerVaultConnection,
  sweepConnectors,
  type ConnectorInstance,
  type ConnectorManifest,
  type VaultConnection,
} from "../../lib/api/connectors";
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
    registerApiKeyConnection: vi.fn(),
    registerBasicConnection: vi.fn(),
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

const servicenowHash = "ef".repeat(32);
const servicenowManifest: ConnectorManifest = {
  id: "servicenow",
  version: "1",
  display_name: "ServiceNow",
  description: "ServiceNow Table API for the `example` instance: list, get, create, update, and delete records in any table.",
  provider: {
    kind: "http_api",
    base_url: "https://example.service-now.com",
    auth: { style: "basic", username_slot: "username", password_slot: "password" },
    default_headers: [],
    health_check: null,
    operations: [
      {
        name: "list-records",
        description: "List records from a ServiceNow table, with sysparm filtering and pagination.",
        method: "GET",
        path: "/api/now/table/{table}",
        params_schema: { type: "object" },
        query_params: [],
        body: { type: "none" },
        effect: "read_only",
        response: { projection: "/result", max_bytes: null },
        timeout_ms: null,
        idempotency_key_header: null,
      },
    ],
  },
  capabilities: ["servicenow table api"],
  credential_slots: [
    { name: "username", description: "ServiceNow user name for basic authentication." },
    { name: "password", description: "ServiceNow password for basic authentication." },
  ],
  hash: servicenowHash,
};

function connection(overrides: Partial<VaultConnection>): VaultConnection {
  return {
    connection_id: "conn-0123456789abcdef0123456789abcdef",
    provider: "api_key",
    scopes: [],
    status: "active",
    health: { consecutive_failures: 0 },
    created_at: "2026-08-11T00:00:00Z",
    updated_at: "2026-08-11T00:00:00Z",
    ...overrides,
  };
}

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

async function rowFor(instanceId: string) {
  const marker = await screen.findByText(instanceId);
  const row = marker.closest("article");
  expect(row).not.toBeNull();
  return within(row!);
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(listConnectorManifests).mockResolvedValue([]);
  vi.mocked(listConnectorInstances).mockResolvedValue([]);
  vi.mocked(listVaultConnections).mockResolvedValue([]);
});

describe("Connectors", () => {
  it("renders distinct empty states for the fleet and the gallery", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "No instances yet" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "No manifests registered" })).toBeVisible();
    expect(screen.getByText(/secrets stay in the vault/i)).toBeVisible();
  });

  it("registers a manifest and admits the exact receipt", async () => {
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
    const payload = vi.mocked(registerConnectorManifest).mock.calls[0][0];
    expect(payload).toMatchObject({ id: "brave-search", provider: { kind: "http_search" } });
    expect(await screen.findByRole("status")).toHaveTextContent("Manifest registered.");
    expect(screen.getByRole("status")).toHaveTextContent(manifestHash.slice(0, 12));
  });

  it("renders manifest field errors and the server's 422 readably", async () => {
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

  it("instantiates through the existing-connection picker with plain labels, never raw ids first", async () => {
    vi.mocked(listConnectorManifests).mockResolvedValue([manifest]);
    vi.mocked(listVaultConnections).mockResolvedValue([
      connection({ connection_id: "conn-0123456789abcdef0123456789abcdef", scopes: ["search"] }),
    ]);
    const created = instance({ instance_id: "inst-000002" });
    vi.mocked(createConnectorInstance).mockResolvedValue(created);
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    expect(panel).toHaveTextContent(manifestHash);
    expect(panel).toHaveTextContent("never secret material");
    expect(within(panel).queryByRole("textbox", { name: /secret|token|key/i })).not.toBeInTheDocument();

    // The option leads with provider, account, and status — the minted id
    // trails, truncated.
    const select = within(panel).getByLabelText(/api_key/);
    await waitFor(() => expect(select).toBeEnabled());
    const option = within(select).getByRole("option", { name: /API key · service-level · active/ });
    expect(option.textContent).toContain("conn-0123456789ab…");
    await userEvent.selectOptions(select, "conn-0123456789abcdef0123456789abcdef");
    await userEvent.click(within(panel).getByRole("button", { name: "Instantiate connector" }));

    await waitFor(() => expect(createConnectorInstance).toHaveBeenCalledTimes(1));
    expect(vi.mocked(createConnectorInstance).mock.calls[0][0]).toEqual({
      manifest_hash: manifestHash,
      credentials: { api_key: "conn-0123456789abcdef0123456789abcdef" },
    });
    await waitFor(() => expect(screen.queryByRole("complementary")).not.toBeInTheDocument());
  });

  it("goes straight into credential entry on an empty vault and instantiates in one motion (api_key)", async () => {
    vi.mocked(listConnectorManifests).mockResolvedValue([manifest]);
    const registered = connection({ connection_id: "conn-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" });
    vi.mocked(listVaultConnections).mockResolvedValueOnce([]).mockResolvedValue([registered]);
    vi.mocked(registerApiKeyConnection).mockResolvedValue(registered);
    vi.mocked(createConnectorInstance).mockResolvedValue(instance({ instance_id: "inst-000010" }));
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    // No detour through a connection id: the credential form is already there.
    expect(await within(panel).findByRole("heading", { name: "Connect with an API key" })).toBeVisible();
    expect(panel).toHaveTextContent("No usable connection in this workspace yet");
    expect(within(panel).queryByRole("button", { name: "Register a connection" })).not.toBeInTheDocument();

    await userEvent.click(within(panel).getByLabelText("API key"));
    await userEvent.paste("brave-search-key");
    await userEvent.click(within(panel).getByRole("button", { name: "Connect and instantiate" }));

    await waitFor(() => expect(registerApiKeyConnection).toHaveBeenCalledTimes(1));
    expect(vi.mocked(registerApiKeyConnection).mock.calls[0][0]).toEqual({ key: "brave-search-key" });
    // Registration chains straight into instantiation with the fresh binding.
    await waitFor(() => expect(createConnectorInstance).toHaveBeenCalledTimes(1));
    expect(vi.mocked(createConnectorInstance).mock.calls[0][0]).toEqual({
      manifest_hash: manifestHash,
      credentials: { api_key: "conn-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" },
    });
    await waitFor(() => expect(screen.queryByRole("complementary")).not.toBeInTheDocument());
  });

  it("serves the ServiceNow basic flow: username + password, sealed as a pair, slots auto-bound", async () => {
    vi.mocked(listConnectorManifests).mockResolvedValue([servicenowManifest]);
    const pair = {
      username_connection: connection({ connection_id: "conn-cccccccccccccccccccccccccccccccc", provider: "basic", subject: "nexus.connector" }),
      password_connection: connection({ connection_id: "conn-dddddddddddddddddddddddddddddddd", provider: "basic", subject: "nexus.connector" }),
    };
    vi.mocked(listVaultConnections)
      .mockResolvedValueOnce([])
      .mockResolvedValue([pair.username_connection, pair.password_connection]);
    vi.mocked(registerBasicConnection).mockResolvedValue(pair);
    vi.mocked(createConnectorInstance).mockResolvedValue(
      instance({ instance_id: "inst-000011", connector_id: "servicenow", manifest_hash: servicenowHash, credential_slots: ["username", "password"] }),
    );
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    expect(await within(panel).findByRole("heading", { name: "Connect with instance credentials" })).toBeVisible();
    // A basic-auth manifest has no use for the OAuth grant: no disclosure.
    expect(within(panel).queryByText("Advanced: OAuth2 password grant")).not.toBeInTheDocument();

    await userEvent.click(within(panel).getByLabelText("Instance username"));
    await userEvent.paste("nexus.connector");
    await userEvent.click(within(panel).getByLabelText("Instance password"));
    await userEvent.paste("instance-password");
    await userEvent.click(within(panel).getByRole("button", { name: "Connect and instantiate" }));

    await waitFor(() => expect(registerBasicConnection).toHaveBeenCalledTimes(1));
    expect(vi.mocked(registerBasicConnection).mock.calls[0][0]).toEqual({
      username: "nexus.connector",
      password: "instance-password",
    });
    await waitFor(() => expect(createConnectorInstance).toHaveBeenCalledTimes(1));
    // Each slot binds its own leg of the pair.
    expect(vi.mocked(createConnectorInstance).mock.calls[0][0]).toEqual({
      manifest_hash: servicenowHash,
      credentials: {
        username: "conn-cccccccccccccccccccccccccccccccc",
        password: "conn-dddddddddddddddddddddddddddddddd",
      },
    });
    await waitFor(() => expect(screen.queryByRole("complementary")).not.toBeInTheDocument());
  });

  it("drops into credential entry when every existing connection is unusable", async () => {
    vi.mocked(listConnectorManifests).mockResolvedValue([manifest]);
    vi.mocked(listVaultConnections).mockResolvedValue([
      connection({ status: "revoked" }),
      connection({ connection_id: "conn-99999999999999999999999999999999", status: "needs_reauth" }),
    ]);
    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "Instantiate" }));
    const panel = await screen.findByRole("complementary");
    expect(await within(panel).findByRole("heading", { name: "Connect with an API key" })).toBeVisible();
    expect(within(panel).queryByRole("combobox")).not.toBeInTheDocument();
  });

  it("keeps the OAuth2 password grant behind the advanced disclosure and chains instantiation", async () => {
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
    // The simple flow is primary; the grant is collapsed behind the disclosure.
    expect(await within(panel).findByRole("heading", { name: "Connect with an API key" })).toBeVisible();
    expect(within(panel).queryByLabelText("Token endpoint URL")).not.toBeVisible();
    await userEvent.click(within(panel).getByText("Advanced: OAuth2 password grant"));

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
    await userEvent.click(within(panel).getByRole("button", { name: "Exchange and instantiate" }));
    expect(await within(panel).findByRole("alert")).toHaveTextContent("https://");
    expect(registerVaultConnection).not.toHaveBeenCalled();

    await userEvent.clear(within(panel).getByLabelText("Token endpoint URL"));
    await userEvent.click(within(panel).getByLabelText("Token endpoint URL"));
    await userEvent.paste("https://dev394299.service-now.com/oauth_token.do");
    await userEvent.click(within(panel).getByRole("button", { name: "Exchange and instantiate" }));

    await waitFor(() => expect(registerVaultConnection).toHaveBeenCalledTimes(1));
    expect(vi.mocked(registerVaultConnection).mock.calls[0][0]).toEqual({
      token_url: "https://dev394299.service-now.com/oauth_token.do",
      client_id: "client-id",
      client_secret: "client-secret",
      username: "nexus.connector",
      password: "account-password",
    });

    // One motion: the grant's minted token binds the slot and instantiates.
    await waitFor(() => expect(createConnectorInstance).toHaveBeenCalledTimes(1));
    expect(vi.mocked(createConnectorInstance).mock.calls[0][0]).toEqual({
      manifest_hash: manifestHash,
      credentials: { api_key: "conn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" },
    });
    await waitFor(() => expect(screen.queryByRole("complementary")).not.toBeInTheDocument());
  });

  it("reflects the server's missing-slot 422 inline on the named slot", async () => {
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
    expect(getInstanceCatalog).toHaveBeenCalledWith("inst-000001", undefined);
  });

  it("handles a catalog pin mismatch by offering the live generation", async () => {
    vi.mocked(listConnectorInstances).mockResolvedValue([
      instance({ state: "healthy", catalog_generation: 2, catalog_hash: "cd".repeat(32), last_health_check_ms: 1_754_900_000_000 }),
    ]);
    vi.mocked(getInstanceCatalog).mockImplementation((_id, generation) => {
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
    vi.mocked(sweepConnectors).mockResolvedValue([]);
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Run health sweep" }));
    expect(await screen.findByRole("heading", { name: "Nothing to re-check" })).toBeVisible();
    expect(screen.getByText(/Pending, failed, and disabled instances are not swept/)).toBeVisible();
  });

  it("keeps other lifecycle mutations honest", async () => {
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
    await waitFor(() => expect(checkConnectorInstanceHealth).toHaveBeenCalledWith("inst-000001"));
    await userEvent.click(row.getByRole("button", { name: "Disable" }));
    await waitFor(() => expect(disableConnectorInstance).toHaveBeenCalledWith("inst-000001"));
  });
});
