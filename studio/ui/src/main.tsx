import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import "@fontsource/outfit/200.css";
import "@fontsource/outfit/300.css";
import "@fontsource/outfit/400.css";
import "@fontsource/outfit/500.css";
import "@fontsource/ibm-plex-mono/300.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "./engine/theme.css";
import { router } from "./router";
import { initTheme } from "./engine/state";

initTheme();
document.title = "Rusty Studio";

// Loopback backend: never let the browser's online/offline heuristics park a
// request — an unreachable server must reach its designed error state, not wait.
const queryClient = new QueryClient({
  defaultOptions: {
    queries: { retry: false, staleTime: 15_000, refetchOnWindowFocus: false, networkMode: "always" },
    mutations: { retry: false, networkMode: "always" },
  },
});

const root = document.getElementById("root");
if (!root) throw new Error("Rusty Studio root element is missing.");

createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} context={{ queryClient }} />
    </QueryClientProvider>
  </StrictMode>,
);
