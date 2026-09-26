import type { QueryClient } from "@tanstack/react-query";
import { createRootRouteWithContext, createRoute, createRouter, redirect } from "@tanstack/react-router";
import { Knot } from "./knot/Knot";
import { BuilderView } from "./knot/agents/Builder";
import { LibraryView } from "./knot/Library";
import { ModelsView } from "./knot/views/Models";
import { HomeView } from "./knot/views/Home";
import { SkillsView } from "./knot/views/Skills";
import { ToolsView } from "./knot/views/Tools";
import { ConnectorsView } from "./knot/views/Connectors";
import { KnowledgeView } from "./knot/views/Knowledge";
import { EvalsView } from "./knot/views/Evals";
import { ObservabilityView } from "./knot/views/Observability";
import { Login } from "./knot/login/Login";

interface RouterContext { queryClient: QueryClient }

const rootRoute = createRootRouteWithContext<RouterContext>()({
  component: Knot,
  notFoundComponent: () => <LibraryView view="not-found" />,
});

const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", beforeLoad: () => { throw redirect({ to: "/home" }); } });
const loginRoute = createRoute({ getParentRoute: () => rootRoute, path: "/login", component: Login });
// The builder: the rail picks the agent, the URL names it.
const agentsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents", component: BuilderView });
const agentRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents/$id", component: BuilderView });
// Improve with AI: the assistant proposes a version from what the runs show.
const improveRoute = createRoute({ getParentRoute: () => rootRoute, path: "/agents/$id/improve", beforeLoad: ({ params }) => { try { localStorage.setItem("rusty.panelMode", "build"); } catch { /* per-viewer */ } throw redirect({ to: "/agents/$id", params: { id: params.id } }); } });
const modelsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/models", component: ModelsView });
const homeRoute = createRoute({ getParentRoute: () => rootRoute, path: "/home", component: HomeView });
const skillsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/skills", component: SkillsView });
const toolsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/tools", component: ToolsView });
const connectorsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/connectors", component: ConnectorsView });
const knowledgeRoute = createRoute({ getParentRoute: () => rootRoute, path: "/knowledge", component: KnowledgeView });
const evalsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/evals", component: EvalsView });
const analyticsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/analytics", component: ObservabilityView });

const routeTree = rootRoute.addChildren([indexRoute, loginRoute, agentsRoute, agentRoute, improveRoute, modelsRoute, homeRoute, skillsRoute, toolsRoute, connectorsRoute, knowledgeRoute, evalsRoute, analyticsRoute]);

export const router = createRouter({
  routeTree,
  context: { queryClient: undefined! },
  defaultPreload: "intent",
});

declare module "@tanstack/react-router" {
  interface Register { router: typeof router }
}
