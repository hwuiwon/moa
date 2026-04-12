import {
  createHashHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";

import { AppLayout, HomeRedirect } from "@/components/layout/app-layout";
import { ChatView } from "@/views/chat-view";
import { MemoryView } from "@/views/memory-view";
import { SettingsView } from "@/views/settings-view";

const rootRoute = createRootRoute({
  component: AppLayout,
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: HomeRedirect,
});

const chatIndexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/chat",
  component: ChatView,
});

const chatRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/chat/$sessionId",
  component: ChatView,
});

const memoryRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/memory",
  component: MemoryView,
});

const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: SettingsView,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  chatIndexRoute,
  chatRoute,
  memoryRoute,
  settingsRoute,
]);

export const router = createRouter({
  history: createHashHistory(),
  routeTree,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
