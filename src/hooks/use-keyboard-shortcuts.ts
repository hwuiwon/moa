import { useEffect } from "react";

import { tauriClient } from "@/lib/tauri";
import { useApprovalStore } from "@/stores/approval";

type UseKeyboardShortcutsArgs = {
  activeSessionId: string | null;
  commandPaletteOpen: boolean;
  cycleTab: (currentId: string | null, direction: 1 | -1) => string | null;
  onActivateSession: (sessionId: string) => void;
  onCloseCurrentTab: () => void;
  onCreateSession: () => void;
  onOpenSettings: () => void;
  onSetCommandPaletteOpen: (open: boolean) => void;
  onToggleCommandPalette: () => void;
  onToggleDetailPanel: () => void;
  onToggleSidebar: () => void;
};

/**
 * Installs the global desktop keyboard shortcuts used by the shell.
 */
export function useKeyboardShortcuts({
  activeSessionId,
  commandPaletteOpen,
  cycleTab,
  onActivateSession,
  onCloseCurrentTab,
  onCreateSession,
  onOpenSettings,
  onSetCommandPaletteOpen,
  onToggleCommandPalette,
  onToggleDetailPanel,
  onToggleSidebar,
}: UseKeyboardShortcutsArgs) {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      const modifier = event.metaKey || event.ctrlKey;

      if (!modifier) {
        if (!isEditableTarget(event.target)) {
          if (
            (key === "y" || key === "a" || key === "n") &&
            useApprovalStore.getState().invokeShortcut(key as "a" | "n" | "y")
          ) {
            event.preventDefault();
            return;
          }
        }

        if (key === "escape") {
          if (commandPaletteOpen) {
            event.preventDefault();
            onSetCommandPaletteOpen(false);
            return;
          }

          if (activeSessionId) {
            event.preventDefault();
            void tauriClient.cancelActiveGeneration();
          }
        }

        return;
      }

      switch (key) {
        case "b":
          event.preventDefault();
          onToggleSidebar();
          break;
        case "i":
          event.preventDefault();
          onToggleDetailPanel();
          break;
        case "k":
          event.preventDefault();
          onToggleCommandPalette();
          break;
        case "n":
          event.preventDefault();
          onCreateSession();
          break;
        case ",":
          event.preventDefault();
          onOpenSettings();
          break;
        case "w":
          if (!activeSessionId) {
            return;
          }
          event.preventDefault();
          onCloseCurrentTab();
          break;
        case "tab": {
          event.preventDefault();
          const nextSessionId = cycleTab(activeSessionId, event.shiftKey ? -1 : 1);
          if (nextSessionId) {
            onActivateSession(nextSessionId);
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
    activeSessionId,
    commandPaletteOpen,
    cycleTab,
    onActivateSession,
    onCloseCurrentTab,
    onCreateSession,
    onOpenSettings,
    onSetCommandPaletteOpen,
    onToggleCommandPalette,
    onToggleDetailPanel,
    onToggleSidebar,
  ]);
}

function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) {
    return false;
  }

  if (target.isContentEditable) {
    return true;
  }

  const tagName = target.tagName.toLowerCase();
  return tagName === "input" || tagName === "textarea" || tagName === "select";
}
