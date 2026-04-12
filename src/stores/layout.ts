import { create } from "zustand";

export type ActiveView = "chat" | "memory" | "settings";

type LayoutStore = {
  sidebarOpen: boolean;
  detailPanelOpen: boolean;
  commandPaletteOpen: boolean;
  activeView: ActiveView;
  setSidebarOpen: (open: boolean) => void;
  setDetailPanelOpen: (open: boolean) => void;
  toggleSidebar: () => void;
  toggleDetailPanel: () => void;
  setCommandPaletteOpen: (open: boolean) => void;
  toggleCommandPalette: () => void;
  setActiveView: (view: ActiveView) => void;
};

export const useLayoutStore = create<LayoutStore>((set) => ({
  sidebarOpen: true,
  detailPanelOpen: true,
  commandPaletteOpen: false,
  activeView: "chat",
  setSidebarOpen: (sidebarOpen) => set({ sidebarOpen }),
  setDetailPanelOpen: (detailPanelOpen) => set({ detailPanelOpen }),
  toggleSidebar: () => set((state) => ({ sidebarOpen: !state.sidebarOpen })),
  toggleDetailPanel: () =>
    set((state) => ({ detailPanelOpen: !state.detailPanelOpen })),
  setCommandPaletteOpen: (commandPaletteOpen) => set({ commandPaletteOpen }),
  toggleCommandPalette: () =>
    set((state) => ({ commandPaletteOpen: !state.commandPaletteOpen })),
  setActiveView: (activeView) => set({ activeView }),
}));
