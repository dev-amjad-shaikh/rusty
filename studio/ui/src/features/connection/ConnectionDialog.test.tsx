import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useConnectionStore } from "../../state/connection";
import { ConnectionDialog } from "./ConnectionDialog";

function renderDialog() {
  const client = new QueryClient();
  return render(<QueryClientProvider client={client}><button type="button" onClick={useConnectionStore.getState().openDialog}>Open connection</button><ConnectionDialog /></QueryClientProvider>);
}

beforeEach(() => useConnectionStore.setState({ connection: null, info: null, dialogOpen: false }));
afterEach(() => vi.unstubAllGlobals());

describe("connection boundary", () => {
  it("rejects credentialed or path-bearing server addresses", async () => {
    const fetchMock = vi.fn(); vi.stubGlobal("fetch", fetchMock);
    renderDialog();
    await userEvent.click(screen.getByRole("button", { name: "Open connection" }));
    const input = screen.getByLabelText("Server address");
    await userEvent.clear(input); await userEvent.type(input, "https://user:secret@rusty.example/api?key=x");
    await userEvent.click(screen.getByRole("button", { name: "Connect" }));
    expect(screen.getByRole("alert")).toHaveTextContent("full http or https address");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("closes with Escape and restores the opener focus", async () => {
    renderDialog();
    const opener = screen.getByRole("button", { name: "Open connection" }); opener.focus();
    await userEvent.click(opener);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Connect your Rusty server" })).toHaveFocus());
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(opener).toHaveFocus());
  });
});
