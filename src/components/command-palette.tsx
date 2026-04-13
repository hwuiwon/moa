import { useMemo, useState } from "react";

import type { SessionPreviewDto } from "@/lib/bindings";
import { PromptSuggestion } from "@/components/prompt-kit/prompt-suggestion";
import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
  CommandShortcut,
} from "@/components/ui/command";
import {
  buildCommandActions,
  commandSuggestions,
  type CommandAction,
} from "@/lib/command-actions";
import type { ActiveView } from "@/stores/layout";
import { formatRelativeTime } from "@/lib/utils";

type CommandPaletteProps = {
  activeSessionId: string | null;
  activeView: ActiveView;
  onCreateSession: () => void;
  onOpenChange: (open: boolean) => void;
  onOpenChat: () => void;
  onOpenMemory: () => void;
  onOpenSettings: () => void;
  onSelectSession: (sessionId: string) => void;
  onToggleDetailPanel: () => void;
  onToggleSidebar: () => void;
  open: boolean;
  sessions: SessionPreviewDto[];
};

/**
 * Global Cmd+K palette for session jumps and desktop navigation shortcuts.
 */
export function CommandPalette({
  activeSessionId,
  activeView,
  onCreateSession,
  onOpenChange,
  onOpenChat,
  onOpenMemory,
  onOpenSettings,
  onSelectSession,
  onToggleDetailPanel,
  onToggleSidebar,
  open,
  sessions,
}: CommandPaletteProps) {
  const [query, setQuery] = useState("");

  const actions = useMemo(
    () =>
      buildCommandActions({
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
      }),
    [
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
    ],
  );

  const actionGroups = useMemo(() => groupActions(actions), [actions]);
  const suggestions = useMemo(() => commandSuggestions(), []);

  const runAction = (action: CommandAction) => {
    action.perform();
    setQuery("");
    onOpenChange(false);
  };

  const runSuggestion = (label: string) => {
    const match = actions.find((action) => action.label === label);
    if (match) {
      runAction(match);
    }
  };

  return (
    <CommandDialog
      className="max-w-2xl"
      description="Run navigation and workspace actions."
      onOpenChange={(nextOpen) => {
        if (!nextOpen) {
          setQuery("");
        }
        onOpenChange(nextOpen);
      }}
      open={open}
      showCloseButton={false}
      title="Command palette"
    >
      <Command shouldFilter>
        <CommandInput
          onValueChange={setQuery}
          placeholder="Search sessions, settings, and actions…"
          value={query}
        />
        <CommandList className="max-h-[420px] px-1 pb-1">
          {query.trim().length === 0 ? (
            <div className="flex flex-wrap gap-2 px-3 py-3">
              {suggestions.map((suggestion) => (
                <PromptSuggestion
                  key={suggestion}
                  onClick={() => runSuggestion(suggestion)}
                  size="sm"
                  type="button"
                  variant="outline"
                >
                  {suggestion}
                </PromptSuggestion>
              ))}
            </div>
          ) : null}

          <CommandEmpty className="px-3 py-6 text-left">
            <p className="text-sm text-muted-foreground">
              No command matched “{query}”.
            </p>
          </CommandEmpty>

          {Array.from(actionGroups.entries()).map(([group, groupActions], index) => (
            <div key={group}>
              {index > 0 ? <CommandSeparator /> : null}
              <CommandGroup heading={group}>
                {groupActions.map((action) => (
                  <CommandItem
                    key={action.id}
                    onSelect={() => runAction(action)}
                    value={`${action.label} ${action.keywords}`}
                  >
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm font-medium">{action.label}</div>
                      {action.id.startsWith("session:") ? (
                        <SessionMetaLine
                          sessionId={action.id.slice("session:".length)}
                          sessions={sessions}
                        />
                      ) : null}
                    </div>
                    {action.shortcut ? (
                      <CommandShortcut>{action.shortcut}</CommandShortcut>
                    ) : null}
                  </CommandItem>
                ))}
              </CommandGroup>
            </div>
          ))}
        </CommandList>
      </Command>
    </CommandDialog>
  );
}

function groupActions(actions: CommandAction[]) {
  const groups = new Map<string, CommandAction[]>();
  for (const action of actions) {
    const entries = groups.get(action.group) ?? [];
    entries.push(action);
    groups.set(action.group, entries);
  }
  return groups;
}

function SessionMetaLine({
  sessionId,
  sessions,
}: {
  sessionId: string;
  sessions: SessionPreviewDto[];
}) {
  const preview = sessions.find((item) => item.summary.sessionId === sessionId);
  if (!preview) {
    return null;
  }

  return (
    <div className="truncate text-xs text-muted-foreground">
      {preview.summary.model} · {formatRelativeTime(preview.summary.updatedAt)}
    </div>
  );
}
