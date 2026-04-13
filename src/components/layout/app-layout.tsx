import { useCallback, useEffect, useMemo } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Navigate,
  Outlet,
  useNavigate,
  useRouterState,
} from "@tanstack/react-router";

import { CommandPalette } from "@/components/command-palette";
import { SessionInfoPanel } from "@/components/layout/session-info-panel";
import { SessionTabBar } from "@/components/layout/session-tab-bar";
import { useKeyboardShortcuts } from "@/hooks/use-keyboard-shortcuts";
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

  const openChat = useCallback(() => {
    if (activeSessionId) {
      navigate({ to: "/chat/$sessionId", params: { sessionId: activeSessionId } });
      return;
    }

    navigate({ to: "/chat" });
  }, [activeSessionId, navigate]);

  const openMemory = useCallback(() => {
    navigate({ to: "/memory" });
  }, [navigate]);

  const openSettings = useCallback(() => {
    navigate({ to: "/settings" });
  }, [navigate]);

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

  useKeyboardShortcuts({
    activeSessionId,
    commandPaletteOpen,
    cycleTab,
    onActivateSession: activateSession,
    onCloseCurrentTab: () => {
      if (activeSessionId) {
        handleCloseTab(activeSessionId);
      }
    },
    onCreateSession: () => {
      if (!createSession.isPending) {
        createSession.mutate();
      }
    },
    onOpenSettings: openSettings,
    onSetCommandPaletteOpen: setCommandPaletteOpen,
    onToggleCommandPalette: toggleCommandPalette,
    onToggleDetailPanel: toggleDetailPanel,
    onToggleSidebar: toggleSidebar,
  });

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

      <CommandPalette
        activeSessionId={activeSessionId}
        activeView={activeView}
        onCreateSession={() => createSession.mutate()}
        onOpenChange={setCommandPaletteOpen}
        onOpenChat={openChat}
        onOpenMemory={openMemory}
        onOpenSettings={openSettings}
        onSelectSession={activateSession}
        onToggleDetailPanel={toggleDetailPanel}
        onToggleSidebar={toggleSidebar}
        open={commandPaletteOpen}
        sessions={sessions.data ?? []}
      />
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
