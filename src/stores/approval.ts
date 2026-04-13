import { create } from "zustand";

type ApprovalActions = {
  allowOnce: () => void;
  alwaysAllow: () => void;
  deny: () => void;
};

type ApprovalStore = {
  activeApprovalIds: string[];
  approvalActions: Record<string, ApprovalActions>;
  registerApproval: (requestId: string, actions: ApprovalActions) => void;
  unregisterApproval: (requestId: string) => void;
  invokeShortcut: (shortcut: "a" | "n" | "y") => boolean;
};

/**
 * Tracks the most recent unresolved approval so global shortcuts can act on it.
 */
export const useApprovalStore = create<ApprovalStore>((set, get) => ({
  activeApprovalIds: [],
  approvalActions: {},
  invokeShortcut: (shortcut) => {
    const { activeApprovalIds, approvalActions } = get();
    const requestId = activeApprovalIds[activeApprovalIds.length - 1];
    if (!requestId) {
      return false;
    }

    const actions = approvalActions[requestId];
    if (!actions) {
      return false;
    }

    switch (shortcut) {
      case "y":
        actions.allowOnce();
        return true;
      case "a":
        actions.alwaysAllow();
        return true;
      case "n":
        actions.deny();
        return true;
      default:
        return false;
    }
  },
  registerApproval: (requestId, actions) =>
    set((state) => ({
      activeApprovalIds: [
        ...state.activeApprovalIds.filter((entry) => entry !== requestId),
        requestId,
      ],
      approvalActions: {
        ...state.approvalActions,
        [requestId]: actions,
      },
    })),
  unregisterApproval: (requestId) =>
    set((state) => {
      const nextActions = { ...state.approvalActions };
      delete nextActions[requestId];
      return {
        activeApprovalIds: state.activeApprovalIds.filter((entry) => entry !== requestId),
        approvalActions: nextActions,
      };
    }),
}));
