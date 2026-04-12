import { create } from "zustand";

type SessionStore = {
  activeSessionId: string | null;
  setActiveSession: (id: string | null) => void;
};

export const useSessionStore = create<SessionStore>((set) => ({
  activeSessionId: null,
  setActiveSession: (activeSessionId) => set({ activeSessionId }),
}));
