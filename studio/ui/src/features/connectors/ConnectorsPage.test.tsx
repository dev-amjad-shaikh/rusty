import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  checkConnectorConfig,
  checkConnectorInstance,
  createConnectorInstance,
  getConnectorCatalog,
  listConnectorInstances,
  listConnectors,
} from "../../lib/api/connectors";
import { servedInstance, servicenowManifest, SERVICENOW_HASH } from "./fixtures";
import { ConnectorsPage } from "./ConnectorsPage";

vi.mock("../../lib/api/connectors", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api/connectors")>();
  return {
    ...actual,
    listConnectors: vi.fn(),
    listConnectorInstances: vi.fn(),
    checkConnectorConfig: vi.fn(),
    checkConnectorInstance: vi.fn(),
    createConnectorInstance: vi.fn(),
    getConnectorCatalog: vi.fn(),
  };
});

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><ConnectorsPage /></QueryClientProvider>);
}

beforeEach(() => {
  vi.mocked(listConnectors).mockReset().mockResolvedValue([servicenowManifest()]);
  vi.mocked(listConnectorInstances).mockReset().mockResolvedValue([]);
});

describe("Connectors page", () => {
  it("renders the registered connector gallery", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "ServiceNow" })).toBeVisible();
    expect(screen.getByText(/Table API/)).toBeVisible();
    expect(screen.getByText("4 operations")).toBeVisible();
  });

  it("shows an empty state when no connectors are registered", async () => {
    vi.mocked(listConnectors).mockResolvedValue([]);
    renderPage();
    expect(await screen.findByText("No connectors registered")).toBeVisible();
  });

  it("names the failure and offers retry when the gallery cannot load", async () => {
    vi.mocked(listConnectors).mockRejectedValue(new Error("Rusty could not be reached."));
    renderPage();
    expect(await screen.findByText("Connectors could not be loaded")).toBeVisible();
    expect(screen.getByRole("alert")).toHaveTextContent("Rusty could not be reached.");
    vi.mocked(listConnectors).mockResolvedValue([servicenowManifest()]);
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("heading", { name: "ServiceNow" })).toBeVisible();
  });

  it("opens the generic schema form from Set up and returns after save", async () => {
    vi.mocked(checkConnectorConfig).mockResolvedValue({ status: "succeeded" });
    vi.mocked(createConnectorInstance).mockResolvedValue(servedInstance());
    vi.mocked(listConnectorInstances).mockResolvedValue([servedInstance()]);
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Set up" }));

    // The form derives from the schema: instance field + credentials variant picker.
    expect(await screen.findByRole("form", { name: "Set up ServiceNow" })).toBeInTheDocument();
    expect(screen.getByLabelText("Instance")).toBeInTheDocument();
    expect(screen.getByLabelText("Authentication")).toBeInTheDocument();

    await userEvent.type(screen.getByLabelText("Instance"), "acme");
    await userEvent.type(screen.getByLabelText("Username"), "admin");
    await userEvent.type(screen.getByLabelText("Password"), "s3cret");
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("Connection verified.")).toHaveRole("status");
    await userEvent.click(screen.getByRole("button", { name: "Save connection" }));

    // Saved → lands on the connections list with the masked secrets.
    expect(await screen.findByText("inst-0123456789abcdef")).toBeVisible();
    expect(screen.getAllByText("set — sealed")).toHaveLength(2);
    expect(screen.getByText("acme")).toBeVisible();
    expect(screen.queryByText("s3cret")).not.toBeInTheDocument();
  });

  it("re-checks an instance and shows the verdict inline", async () => {
    vi.mocked(listConnectorInstances).mockResolvedValue([servedInstance()]);
    vi.mocked(checkConnectorInstance).mockResolvedValue({ status: "failed", message: "Authentication failed (401)." });
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Connections" }));
    await userEvent.click(await screen.findByRole("button", { name: "Re-check" }));
    expect(await screen.findByText("Connection failed. Authentication failed (401).")).toHaveRole("status");
    expect(checkConnectorInstance).toHaveBeenCalledWith("inst-0123456789abcdef");
  });

  it("derives the tool catalog on disclosure", async () => {
    vi.mocked(listConnectorInstances).mockResolvedValue([servedInstance()]);
    vi.mocked(getConnectorCatalog).mockResolvedValue({
      instance_id: "inst-0123456789abcdef",
      manifest_hash: SERVICENOW_HASH,
      tools: [
        { name: "servicenow/get-record", description: "Get one record.", parameters_schema: { type: "object" }, effect: "read_only" },
        { name: "servicenow/create-incident", description: "Create an incident.", parameters_schema: { type: "object" }, effect: "compensatable" },
      ],
    });
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Connections" }));
    expect(getConnectorCatalog).not.toHaveBeenCalled();
    await userEvent.click(await screen.findByText("Tool catalog"));
    expect(await screen.findByText("servicenow/get-record")).toBeVisible();
    expect(screen.getByText("servicenow/create-incident")).toBeVisible();
  });

  it("labels an instance whose manifest is not in the gallery", async () => {
    vi.mocked(listConnectorInstances).mockResolvedValue([servedInstance({ manifest_hash: "f".repeat(64) })]);
    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Connections" }));
    expect(await screen.findByText("Unknown connector")).toBeVisible();
  });
});
