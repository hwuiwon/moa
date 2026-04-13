import type { SessionPreviewDto } from "@/lib/bindings";
import type { ActiveView } from "@/stores/layout";

export type CommandAction = {
  id: string;
  group: string;
  keywords: string;
  label: string;
  shortcut?: string;
  perform: () => void;
};

type BuildCommandActionsArgs = {
  activeSessionId: string | null;
  activeView: ActiveView;
  onCreateSession: () => void;
  onOpenChat: () => void;
  onOpenMemory: () => void;
  onOpenSettings: () => void;
  onSelectSession: (sessionId: string) => void;
  onToggleDetailPanel: () => void;
  onToggleSidebar: () => void;
  sessions: SessionPreviewDto[];
};

/**
 * Builds the palette action list for navigation, chrome toggles, and session jumps.
 */
export function buildCommandActions({
  activeSessionId,
  activeView,
  onCreateSession,
  onOpenChat,
  onOpenMemory,
  onOpenSettings,
  onSelectSession,
  onToggleDetailPanel,
  onToggleSidebar,
  sessions,
}: BuildCommandActionsArgs): CommandAction[] {
  const actions: CommandAction[] = [
    {
      id: "new-session",
      group: "Actions",
      keywords: "new create session conversation",
      label: "New session",
      shortcut: "⌘N",
      perform: onCreateSession,
    },
    {
      id: "toggle-sidebar",
      group: "Layout",
      keywords: "toggle sidebar left panel sessions",
      label: "Toggle session sidebar",
      shortcut: "⌘B",
      perform: onToggleSidebar,
    },
    {
      id: "toggle-detail-panel",
      group: "Layout",
      keywords: "toggle detail panel right info sidebar",
      label: "Toggle detail panel",
      shortcut: "⌘I",
      perform: onToggleDetailPanel,
    },
    {
      id: "open-chat",
      group: "Navigate",
      keywords: "chat home conversation session",
      label: "Open chat",
      perform: onOpenChat,
    },
    {
      id: "open-memory",
      group: "Navigate",
      keywords: "memory wiki browser knowledge pages",
      label: "Open memory",
      perform: onOpenMemory,
    },
    {
      id: "open-settings",
      group: "Navigate",
      keywords: "settings preferences config runtime",
      label: "Open settings",
      perform: onOpenSettings,
    },
  ];

  const sessionActions = sessions.map<CommandAction>((preview) => {
    const title = preview.summary.title ?? "Untitled session";
    const previewText = preview.lastMessage ?? "";
    const isActive = preview.summary.sessionId === activeSessionId;
    const inView = activeView === "chat" && isActive;

    return {
      id: `session:${preview.summary.sessionId}`,
      group: "Sessions",
      keywords: `${title} ${preview.summary.model} ${previewText} ${preview.summary.status}`,
      label: inView ? `${title} · active` : title,
      perform: () => onSelectSession(preview.summary.sessionId),
    };
  });

  return [...actions, ...sessionActions];
}

/**
 * Default suggestion chips shown when the palette opens without a query.
 */
export function commandSuggestions() {
  return [
    "New session",
    "Open memory",
    "Open settings",
    "Toggle session sidebar",
  ];
}
