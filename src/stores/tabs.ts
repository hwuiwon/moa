import { create } from "zustand";

type CycleDirection = 1 | -1;

type TabsStore = {
  openTabs: string[];
  openTab: (sessionId: string) => void;
  closeTab: (sessionId: string) => void;
  reorderTabs: (activeId: string, overId: string) => void;
  cycleTab: (currentId: string | null, direction: CycleDirection) => string | null;
};

function moveItem(items: string[], fromIndex: number, toIndex: number) {
  const next = [...items];
  const [item] = next.splice(fromIndex, 1);
  if (!item) {
    return items;
  }
  next.splice(toIndex, 0, item);
  return next;
}

export const useTabsStore = create<TabsStore>((set, get) => ({
  openTabs: [],
  openTab: (sessionId) =>
    set((state) => ({
      openTabs: state.openTabs.includes(sessionId)
        ? state.openTabs
        : [...state.openTabs, sessionId],
    })),
  closeTab: (sessionId) =>
    set((state) => ({
      openTabs: state.openTabs.filter((id) => id !== sessionId),
    })),
  reorderTabs: (activeId, overId) =>
    set((state) => {
      const fromIndex = state.openTabs.indexOf(activeId);
      const toIndex = state.openTabs.indexOf(overId);
      if (fromIndex < 0 || toIndex < 0 || fromIndex === toIndex) {
        return state;
      }
      return { openTabs: moveItem(state.openTabs, fromIndex, toIndex) };
    }),
  cycleTab: (currentId, direction) => {
    const { openTabs } = get();
    if (!openTabs.length) {
      return null;
    }

    const currentIndex = currentId ? openTabs.indexOf(currentId) : -1;
    if (currentIndex < 0) {
      return openTabs[0] ?? null;
    }

    const nextIndex =
      (currentIndex + direction + openTabs.length) % openTabs.length;
    return openTabs[nextIndex] ?? null;
  },
}));
