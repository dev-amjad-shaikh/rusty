import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { StudioApiError } from "../../lib/api/client";
import { checkConnectorConfig, createConnectorInstance } from "../../lib/api/connectors";
import { servedInstance, servicenowManifest, SERVICENOW_HASH } from "./fixtures";
import { ConnectorSetup } from "./ConnectorSetup";

vi.mock("../../lib/api/connectors", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api/connectors")>();
  return {
    ...actual,
    checkConnectorConfig: vi.fn(),
    createConnectorInstance: vi.fn(),
  };
});

function renderSetup(onDone = vi.fn(), onCancel = vi.fn()) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <ConnectorSetup manifest={servicenowManifest()} onDone={onDone} onCancel={onCancel} />
    </QueryClientProvider>,
  );
  return { onDone, onCancel };
}

async function fillBasicAuth() {
  await userEvent.type(screen.getByLabelText("Instance"), "acme");
  await userEvent.type(screen.getByLabelText("Username"), "admin");
  await userEvent.type(screen.getByLabelText("Password"), "s3cret");
}

const expectedConfig = {
  instance: "acme",
  credentials: { auth: "basic", username: "admin", password: "s3cret" },
};

beforeEach(() => {
  vi.mocked(checkConnectorConfig).mockReset();
  vi.mocked(createConnectorInstance).mockReset();
});

describe("ConnectorSetup", () => {
  it("tests the current form state and shows the server's success verdict", async () => {
    vi.mocked(checkConnectorConfig).mockResolvedValue({ status: "succeeded" });
    renderSetup();
    await fillBasicAuth();
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("Connection verified.")).toHaveRole("status");
    expect(checkConnectorConfig).toHaveBeenCalledWith(SERVICENOW_HASH, expectedConfig);
  });

  it("shows the server's failure message when the check fails", async () => {
    vi.mocked(checkConnectorConfig).mockResolvedValue({ status: "failed", message: "Authentication failed (401)." });
    renderSetup();
    await fillBasicAuth();
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("Connection failed. Authentication failed (401).")).toHaveRole("status");
    expect(createConnectorInstance).not.toHaveBeenCalled();
  });

  it("pins a 422 from the check to the failing field", async () => {
    vi.mocked(checkConnectorConfig).mockRejectedValue(new StudioApiError("credentials.username: required property missing", 422));
    renderSetup();
    await userEvent.type(screen.getByLabelText("Instance"), "acme");
    await userEvent.type(screen.getByLabelText("Password"), "s3cret");
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("required property missing")).toHaveRole("alert");
    expect(screen.getByLabelText("Username")).toHaveAttribute("aria-invalid", "true");
  });

  it("saves the built config and hands the served instance back", async () => {
    const instance = servedInstance();
    vi.mocked(createConnectorInstance).mockResolvedValue(instance);
    const { onDone } = renderSetup();
    await fillBasicAuth();
    await userEvent.click(screen.getByRole("button", { name: "Save connection" }));
    await vi.waitFor(() => expect(onDone).toHaveBeenCalledWith(instance));
    expect(createConnectorInstance).toHaveBeenCalledWith({ manifest_hash: SERVICENOW_HASH, config: expectedConfig });
  });

  it("pins a 422 from save without losing the form", async () => {
    vi.mocked(createConnectorInstance).mockRejectedValue(new StudioApiError("instance: does not match pattern", 422));
    const { onDone } = renderSetup();
    await fillBasicAuth();
    await userEvent.click(screen.getByRole("button", { name: "Save connection" }));
    expect(await screen.findByText("does not match pattern")).toHaveRole("alert");
    expect(screen.getByLabelText("Instance")).toHaveAttribute("aria-invalid", "true");
    expect(onDone).not.toHaveBeenCalled();
  });

  it("switches to the OAuth variant and sends its discriminator", async () => {
    vi.mocked(checkConnectorConfig).mockResolvedValue({ status: "succeeded" });
    renderSetup();
    await userEvent.type(screen.getByLabelText("Instance"), "acme");
    await userEvent.selectOptions(screen.getByLabelText("Authentication"), "oauth");
    await userEvent.type(screen.getByLabelText("Access token"), "tok-1");
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    await screen.findByText("Connection verified.");
    expect(checkConnectorConfig).toHaveBeenCalledWith(SERVICENOW_HASH, {
      instance: "acme",
      credentials: { auth: "oauth", token: "tok-1" },
    });
  });

  it("names the failure when the check cannot run", async () => {
    vi.mocked(checkConnectorConfig).mockRejectedValue(new Error("Rusty could not be reached."));
    renderSetup();
    await fillBasicAuth();
    await userEvent.click(screen.getByRole("button", { name: "Test connection" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Rusty could not be reached.");
  });
});
