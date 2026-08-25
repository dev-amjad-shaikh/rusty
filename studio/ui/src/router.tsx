import type { QueryClient } from "@tanstack/react-query";
import {
  createRootRouteWithContext,
  createRoute,
  createRouter,
} from "@tanstack/react-router";
import { AppShell } from "./app/AppShell";
import { CommandCenter } from "./features/command-center/CommandCenter";
import { AgentsPage } from "./features/agents/AgentsPage";
import { AgentBuilderPage } from "./features/agents/AgentBuilderPage";
import { AgentWorkspace } from "./features/agents/AgentWorkspace";
import { PromptStudio } from "./features/prompts/PromptStudio";
import { WorkPage } from "./features/work/WorkPage";
import { RunComparePage } from "./features/work/RunComparePage";
import { SkillsPage } from "./features/skills-tools/SkillsPage";
import { KnowledgePage } from "./features/knowledge/KnowledgePage";
import { ConnectorsPage } from "./features/connectors/ConnectorsPage";
import { MemoryPage } from "./features/memory/MemoryPage";
import { OperationsPage } from "./features/operations/OperationsPage";
import { ReleasesPage } from "./features/operations/releases/ReleasesPage";

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
const skillsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/skills", component: SkillsPage });
const knowledgeRoute = createRoute({ getParentRoute: () => rootRoute, path: "/knowledge", component: KnowledgePage });
const connectorsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/connectors", component: ConnectorsPage });
const workRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work", component: WorkPage });
const workCompareRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/compare", component: RunComparePage });
const workRunRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId", component: WorkPage });
const workTraceRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId/trace", component: WorkPage });
const workEvaluateRoute = createRoute({ getParentRoute: () => rootRoute, path: "/work/$threadId/runs/$runId/evaluate", component: WorkPage });
const memoryRoute = createRoute({ getParentRoute: () => rootRoute, path: "/memory", component: MemoryPage });
const operationsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/operations", component: OperationsPage });
const releasesRoute = createRoute({ getParentRoute: () => rootRoute, path: "/operations/releases", component: ReleasesPage });
const releasesEnvironmentRoute = createRoute({ getParentRoute: () => rootRoute, path: "/operations/releases/$environment", component: ReleasesPage });
const releasesRevisionRoute = createRoute({ getParentRoute: () => rootRoute, path: "/operations/releases/$environment/revisions/$revisionId", component: ReleasesPage });

const routeTree = rootRoute.addChildren([indexRoute, agentsRoute, agentBuilderRoute, agentRoute, promptsRoute, skillsRoute, knowledgeRoute, connectorsRoute, workRoute, workCompareRoute, workRunRoute, workTraceRoute, workEvaluateRoute, memoryRoute, operationsRoute, releasesRoute, releasesEnvironmentRoute, releasesRevisionRoute]);

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
