import { useEffect, useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Navigate,
  Outlet,
  useNavigate,
  useRouterState,
} from "@tanstack/react-router";

import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { SessionInfoPanel } from "@/components/layout/session-info-panel";
import { SessionTabBar } from "@/components/layout/session-tab-bar";
import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";
import { useSessionList } from "@/hooks/use-session-list";
import { SessionSidebar } from "@/components/layout/session-sidebar";
import { TopBar } from "@/components/layout/top-bar";
import { useLayoutStore } from "@/stores/layout";
import { useSessionStore } from "@/stores/session";
import { useTabsStore } from "@/stores/tabs";

function chatSessionIdFromPath(pathname: string) {
  const match = pathname.match(/^\/chat\/([^/]+)$/);
  return match?.[1] ?? null;
}

export function AppLayout() {
  const navigate = useNavigate();
  const pathname = useRouterState({
    select: (state) => state.location.pathname,
  });
  const queryClient = useQueryClient();

  const sidebarOpen = useLayoutStore((state) => state.sidebarOpen);
  const detailPanelOpen = useLayoutStore((state) => state.detailPanelOpen);
  const commandPaletteOpen = useLayoutStore((state) => state.commandPaletteOpen);
  const activeView = useLayoutStore((state) => state.activeView);
  const toggleSidebar = useLayoutStore((state) => state.toggleSidebar);
  const toggleDetailPanel = useLayoutStore((state) => state.toggleDetailPanel);
  const toggleCommandPalette = useLayoutStore(
    (state) => state.toggleCommandPalette,
  );
  const setCommandPaletteOpen = useLayoutStore(
    (state) => state.setCommandPaletteOpen,
  );
  const setActiveView = useLayoutStore((state) => state.setActiveView);

  const activeSessionId = useSessionStore((state) => state.activeSessionId);
  const setActiveSession = useSessionStore((state) => state.setActiveSession);
  const openTabs = useTabsStore((state) => state.openTabs);
  const openTab = useTabsStore((state) => state.openTab);
  const closeTab = useTabsStore((state) => state.closeTab);
  const reorderTabs = useTabsStore((state) => state.reorderTabs);
  const cycleTab = useTabsStore((state) => state.cycleTab);

  const sessions = useSessionList();
  const runtimeInfo = useQuery({
    queryKey: queryKeys.runtimeInfo(),
    queryFn: tauriClient.getRuntimeInfo,
  });
  const modelOptions = useQuery({
    queryKey: queryKeys.modelOptions(),
    queryFn: tauriClient.listModelOptions,
  });
  const routeSessionId = chatSessionIdFromPath(pathname);

  useEffect(() => {
    if (pathname.startsWith("/memory")) {
      setActiveView("memory");
      return;
    }
    if (pathname.startsWith("/settings")) {
      setActiveView("settings");
      return;
    }
    setActiveView("chat");
  }, [pathname, setActiveView]);

  useEffect(() => {
    if (routeSessionId) {
      setActiveSession(routeSessionId);
      openTab(routeSessionId);
      return;
    }

    if (runtimeInfo.data?.sessionId && !activeSessionId) {
      setActiveSession(runtimeInfo.data.sessionId);
    }
  }, [
    activeSessionId,
    openTab,
    routeSessionId,
    runtimeInfo.data?.sessionId,
    setActiveSession,
  ]);

  const invalidateChromeQueries = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: queryKeys.sessions() }),
      queryClient.invalidateQueries({ queryKey: queryKeys.runtimeInfo() }),
      queryClient.invalidateQueries({ queryKey: queryKeys.config() }),
      queryClient.invalidateQueries({ queryKey: queryKeys.modelOptions() }),
    ]);
  };

  const createSession = useMutation({
    mutationFn: tauriClient.createSession,
    onSuccess: async (sessionId) => {
      openTab(sessionId);
      setActiveSession(sessionId);
      await invalidateChromeQueries();
      navigate({ to: "/chat/$sessionId", params: { sessionId } });
    },
  });

  const selectSession = useMutation({
    mutationFn: async (sessionId: string) => {
      await tauriClient.selectSession(sessionId);
      return sessionId;
    },
    onSuccess: async (sessionId) => {
      openTab(sessionId);
      setActiveSession(sessionId);
      await invalidateChromeQueries();
      navigate({ to: "/chat/$sessionId", params: { sessionId } });
    },
  });

  const setModel = useMutation({
    mutationFn: tauriClient.setModel,
    onSuccess: async (sessionId) => {
      openTab(sessionId);
      setActiveSession(sessionId);
      await invalidateChromeQueries();
      navigate({ to: "/chat/$sessionId", params: { sessionId } });
    },
  });

  const activateSession = (sessionId: string) => {
    openTab(sessionId);
    selectSession.mutate(sessionId);
  };

  const handleCloseTab = (sessionId: string) => {
    const tabIndex = openTabs.indexOf(sessionId);
    const nextSessionId =
      activeSessionId === sessionId
        ? openTabs[tabIndex + 1] ?? openTabs[tabIndex - 1] ?? null
        : null;

    closeTab(sessionId);

    if (nextSessionId) {
      activateSession(nextSessionId);
      return;
    }

    if (pathname.startsWith("/chat")) {
      setActiveSession(null);
      navigate({ to: "/chat" });
    }
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const modifier = event.metaKey || event.ctrlKey;
      if (!modifier) {
        return;
      }

      switch (event.key.toLowerCase()) {
        case "b":
          event.preventDefault();
          toggleSidebar();
          break;
        case "i":
          event.preventDefault();
          toggleDetailPanel();
          break;
        case "k":
          event.preventDefault();
          toggleCommandPalette();
          break;
        case "n":
          event.preventDefault();
          if (!createSession.isPending) {
            createSession.mutate();
          }
          break;
        case "tab": {
          event.preventDefault();
          const nextSessionId = cycleTab(
            activeSessionId,
            event.shiftKey ? -1 : 1,
          );
          if (nextSessionId) {
            activateSession(nextSessionId);
          }
          break;
        }
        default:
          break;
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    activateSession,
    activeSessionId,
    createSession,
    cycleTab,
    toggleCommandPalette,
    toggleDetailPanel,
    toggleSidebar,
  ]);

  const hasActiveSession = useMemo(() => Boolean(activeSessionId), [activeSessionId]);

  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <TopBar
        activeSessionId={activeSessionId}
        activeView={activeView}
        hasActiveSession={hasActiveSession}
        modelOptions={modelOptions.data ?? []}
        modelPending={setModel.isPending}
        onCreateSession={() => createSession.mutate()}
        onModelChange={(value) => setModel.mutate(value)}
        onToggleDetailPanel={toggleDetailPanel}
        onToggleSidebar={toggleSidebar}
        runtimeInfo={runtimeInfo.data}
      />

      <div className="min-h-0 w-full flex-1 overflow-hidden">
        <div className="flex h-full border-t border-border bg-background">
          {sidebarOpen ? (
            <>
              <div className="h-full w-[260px] shrink-0 overflow-hidden border-r border-border">
                <SessionSidebar
                  activeSessionId={activeSessionId}
                  isLoading={sessions.isLoading}
                  onCreateSession={() => createSession.mutate()}
                  onSelectSession={activateSession}
                  sessions={sessions.data ?? []}
                />
              </div>
            </>
          ) : null}

          <main className="min-w-0 flex-1 overflow-hidden">
            <div className="flex h-full min-h-0 flex-col">
              <SessionTabBar
                activeSessionId={activeSessionId}
                onCloseTab={handleCloseTab}
                onReorderTabs={reorderTabs}
                onSelectSession={activateSession}
                openTabs={openTabs}
                sessions={sessions.data ?? []}
              />
              <div className="min-h-0 flex-1 overflow-hidden">
                <Outlet />
              </div>
            </div>
          </main>

          {detailPanelOpen ? (
            <>
              <div className="h-full w-[300px] shrink-0 overflow-hidden border-l border-border">
                <SessionInfoPanel activeSessionId={activeSessionId} />
              </div>
            </>
          ) : null}
        </div>
      </div>

      {commandPaletteOpen ? (
        <div className="fixed inset-0 z-50 flex items-start justify-center bg-black/50 px-4 pt-[16vh] backdrop-blur-sm">
          <div className="w-full max-w-xl rounded-xl border border-border bg-popover p-6 shadow-lg">
            <div className="flex items-center justify-between gap-4">
              <div>
                <p className="text-xs uppercase tracking-widest text-muted-foreground">
                  Command Palette
                </p>
                <h2 className="mt-2 text-xl font-semibold">
                  Coming next
                </h2>
              </div>
              <Button onClick={() => setCommandPaletteOpen(false)} variant="ghost">
                Close
              </Button>
            </div>
            <Separator className="my-5" />
            <p className="text-sm leading-6 text-muted-foreground">
              The global shortcut is live. Palette actions land in the next UI
              pass once chat interactions and tool controls are in place.
            </p>
          </div>
        </div>
      ) : null}
    </div>
  );
}

export function HomeRedirect() {
  const sessions = useSessionList();
  const activeSessionId = useSessionStore((state) => state.activeSessionId);

  if (sessions.isLoading) {
    return null;
  }

  const sessionId =
    (activeSessionId &&
      sessions.data?.find((preview) => preview.summary.sessionId === activeSessionId)?.summary
        .sessionId) ??
    sessions.data?.find((preview) => preview.summary.active)?.summary.sessionId ??
    sessions.data?.[0]?.summary.sessionId;

  if (sessionId) {
    return <Navigate params={{ sessionId }} replace to="/chat/$sessionId" />;
  }

  return <Navigate replace to="/chat" />;
}
