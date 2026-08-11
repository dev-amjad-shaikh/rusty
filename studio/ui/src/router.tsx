import type { QueryClient } from "@tanstack/react-query";
import {
  createRootRouteWithContext,
  createRoute,
  createRouter,
  redirect,
} from "@tanstack/react-router";
import { AppShell } from "./app/AppShell";
import { AgentsPage } from "./features/agents/AgentsPage";
import { PromptStudio } from "./features/prompts/PromptStudio";
import { WorkPage } from "./features/work/WorkPage";
import { RunComparePage } from "./features/work/RunComparePage";
import { OperationsPage } from "./features/operations/OperationsPage";

interface RouterContext {
  queryClient: QueryClient;
}

const rootRoute = createRootRouteWithContext<RouterContext>()({
  component: AppShell,
  notFoundComponent: () => <div className="route-error">This Studio page does not exist.</div>,
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => { throw redirect({ to: "/work" }); },
});

const agentsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents", component: AgentsPage });
const promptsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents/prompts", component: PromptStudio });
const workRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work", component: WorkPage });
const workCompareRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/compare", component: RunComparePage });
const workRunRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId", component: WorkPage });
const workTraceRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId/trace", component: WorkPage });
const workEvaluateRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId/evaluate", component: WorkPage });
const operationsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/operations", component: OperationsPage });

const routeTree = rootRoute.addChildren([indexRoute, agentsRoute, promptsRoute, workRoute, workCompareRoute, workRunRoute, workTraceRoute, workEvaluateRoute, operationsRoute]);

export const router = createRouter({
  routeTree,
  context: { queryClient: undefined! },
  defaultPreload: "intent",
  defaultPreloadStaleTime: 10_000,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
