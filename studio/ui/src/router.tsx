import type { QueryClient } from "@tanstack/react-query";
import { lazy } from "react";
import { createRootRouteWithContext, createRoute, createRouter } from "@tanstack/react-router";
import { AppShell } from "./app/AppShell";

const AgentsPage = lazy(() => import("./features/agents/AgentsPage").then((module) => ({ default: module.AgentsPage })));
const AgentBuilderPage = lazy(() => import("./features/agents/AgentBuilderPage").then((module) => ({ default: module.AgentBuilderPage })));
const AgentWorkspace = lazy(() => import("./features/agents/AgentWorkspace").then((module) => ({ default: module.AgentWorkspace })));
const PromptStudio = lazy(() => import("./features/prompts/PromptStudio").then((module) => ({ default: module.PromptStudio })));
const WorkPage = lazy(() => import("./features/work/WorkPage").then((module) => ({ default: module.WorkPage })));
const RunComparePage = lazy(() => import("./features/work/RunComparePage").then((module) => ({ default: module.RunComparePage })));
const OperationsPage = lazy(() => import("./features/operations/OperationsPage").then((module) => ({ default: module.OperationsPage })));
const CommandCenter = lazy(() => import("./features/command-center/CommandCenter").then((module) => ({ default: module.CommandCenter })));

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
  component: CommandCenter,
});

const agentsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents", component: AgentsPage });
const agentBuilderRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents/new", component: AgentBuilderPage });
const agentRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents/$assistantId", component: AgentWorkspace });
const promptsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents/prompts", component: PromptStudio });
const workRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work", component: WorkPage });
const workCompareRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/compare", component: RunComparePage });
const workRunRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId", component: WorkPage });
const workTraceRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId/trace", component: WorkPage });
const workEvaluateRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId/evaluate", component: WorkPage });
const operationsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/operations", component: OperationsPage });

const routeTree = rootRoute.addChildren([indexRoute, agentsRoute, agentBuilderRoute, agentRoute, promptsRoute, workRoute, workCompareRoute, workRunRoute, workTraceRoute, workEvaluateRoute, operationsRoute]);

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
