import { useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronRight,
  Circle,
  FolderPlus,
  Search,
  Sparkles,
} from "lucide-react";

import type { SessionPreviewDto } from "@/lib/bindings";
import { Button } from "@/components/ui/button";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  cn,
  formatAbsoluteDate,
  formatRelativeTime,
  sessionDateBucket,
} from "@/lib/utils";

type SessionSidebarProps = {
  sessions: SessionPreviewDto[];
  isLoading: boolean;
  activeSessionId: string | null;
  onCreateSession: () => void;
  onSelectSession: (sessionId: string) => void;
};

type SessionListRow =
  | { type: "header"; key: string; label: string }
  | { type: "session"; key: string; preview: SessionPreviewDto };

const BUCKET_ORDER = ["Today", "Yesterday", "Last 7 days", "Older"];

export function SessionSidebar({
  sessions,
  isLoading,
  activeSessionId,
  onCreateSession,
  onSelectSession,
}: SessionSidebarProps) {
  const [search, setSearch] = useState("");
  const parentRef = useRef<HTMLDivElement | null>(null);

  const filteredSessions = useMemo(() => {
    const normalized = search.trim().toLowerCase();
    if (!normalized) {
      return sessions;
    }

    return sessions.filter((preview) => {
      const haystack = `${preview.summary.title ?? ""}\n${preview.lastMessage ?? ""}`.toLowerCase();
      return haystack.includes(normalized);
    });
  }, [search, sessions]);

  const rows = useMemo<SessionListRow[]>(() => {
    const grouped = new Map<string, SessionPreviewDto[]>();

    for (const preview of filteredSessions) {
      const bucket = sessionDateBucket(preview.summary.updatedAt);
      const entries = grouped.get(bucket) ?? [];
      entries.push(preview);
      grouped.set(bucket, entries);
    }

    return BUCKET_ORDER.flatMap((bucket) => {
      const entries = grouped.get(bucket);
      if (!entries?.length) {
        return [];
      }

      return [
        { type: "header", key: `header-${bucket}`, label: bucket } as const,
        ...entries.map((preview) => ({
          type: "session" as const,
          key: preview.summary.sessionId,
          preview,
        })),
      ];
    });
  }, [filteredSessions]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: (index) => (rows[index]?.type === "header" ? 36 : 88),
    overscan: 8,
  });

  return (
    <aside className="flex h-full flex-col border-r border-border bg-sidebar">
      <div className="space-y-3 px-3 py-3">
        <Button
          className="w-full justify-center h-8 text-xs"
          onClick={onCreateSession}
          type="button"
          variant="secondary"
        >
          <FolderPlus className="h-3.5 w-3.5" />
          New Session
        </Button>

        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            className="h-8 pl-8 text-xs"
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search sessions"
            value={search}
          />
        </div>
      </div>

      <Separator />

      {isLoading ? (
        <div className="px-3 py-6 text-sm text-muted-foreground">Loading sessions…</div>
      ) : rows.length === 0 ? (
        <div className="px-3 py-6">
          <div className="rounded-lg border border-dashed border-border p-4 text-sm text-muted-foreground">
            <div className="flex items-center gap-2 text-foreground">
              <Sparkles className="h-4 w-4" />
              No sessions yet
            </div>
            <p className="mt-2 leading-6">
              Create a new session to start a desktop conversation with MOA.
            </p>
          </div>
        </div>
      ) : rows.length > 50 ? (
        <ScrollArea className="flex-1" viewportRef={parentRef}>
          <div
            className="relative px-2 py-3"
            style={{ height: `${virtualizer.getTotalSize()}px` }}
          >
            {virtualizer.getVirtualItems().map((item) => {
              const row = rows[item.index];
              if (!row) {
                return null;
              }

              return (
                <div
                  className="absolute left-0 top-0 w-full px-2"
                  key={row.key}
                  style={{ transform: `translateY(${item.start}px)` }}
                >
                  {row.type === "header" ? (
                    <SessionGroupHeader label={row.label} />
                  ) : (
                    <SessionRow
                      active={row.preview.summary.sessionId === activeSessionId}
                      preview={row.preview}
                      onSelect={onSelectSession}
                    />
                  )}
                </div>
              );
            })}
          </div>
        </ScrollArea>
      ) : (
        <ScrollArea className="flex-1">
          <div className="space-y-1 px-2 py-3">
            {rows.map((row) =>
              row.type === "header" ? (
                <SessionGroupHeader key={row.key} label={row.label} />
              ) : (
                <SessionRow
                  active={row.preview.summary.sessionId === activeSessionId}
                  key={row.key}
                  onSelect={onSelectSession}
                  preview={row.preview}
                />
              ),
            )}
          </div>
        </ScrollArea>
      )}
    </aside>
  );
}

function SessionGroupHeader({ label }: { label: string }) {
  return (
    <div className="px-3 pb-1 pt-4 text-[11px] font-medium uppercase tracking-widest text-muted-foreground first:pt-1">
      {label}
    </div>
  );
}

function SessionRow({
  preview,
  active,
  onSelect,
}: {
  preview: SessionPreviewDto;
  active: boolean;
  onSelect: (sessionId: string) => void;
}) {
  const { summary } = preview;

  return (
    <ContextMenu>
      <ContextMenuTrigger
        render={
          <div />
        }
      >
        <button
          className={cn(
            "group flex w-full flex-col rounded-lg px-3 py-2.5 text-left transition",
            active
              ? "bg-accent text-accent-foreground"
              : "hover:bg-accent/50",
          )}
          onClick={() => onSelect(summary.sessionId)}
          type="button"
        >
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <StatusDot status={summary.status} />
                <p className="truncate text-sm font-medium">
                  {summary.title ?? "Untitled session"}
                </p>
              </div>
              <p className="mt-0.5 line-clamp-2 pl-5 text-xs leading-5 text-muted-foreground">
                {preview.lastMessage ?? "No message preview yet."}
              </p>
            </div>
            <ChevronRight
              className={cn(
                "mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground/50 transition group-hover:text-muted-foreground",
                active && "text-foreground",
              )}
            />
          </div>
          <div className="mt-1.5 flex items-center justify-between gap-3 pl-5 text-[11px] text-muted-foreground">
            <span className="truncate">{summary.model}</span>
            <Tooltip>
              <TooltipTrigger render={<span className="inline-flex" />}>
                <time dateTime={summary.updatedAt}>
                  {formatRelativeTime(summary.updatedAt)}
                </time>
              </TooltipTrigger>
              <TooltipContent>{formatAbsoluteDate(summary.updatedAt)}</TooltipContent>
            </Tooltip>
          </div>
        </button>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem disabled inset>
          Rename
        </ContextMenuItem>
        <ContextMenuItem disabled inset>
          Delete
        </ContextMenuItem>
        <ContextMenuSeparator />
        <ContextMenuItem inset onSelect={() => onSelect(summary.sessionId)}>
          Open session
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}

function StatusDot({ status }: { status: string }) {
  const colorClass =
    status === "completed"
      ? "text-emerald-400"
      : status === "running"
        ? "text-sky-400"
        : status === "waiting_approval"
          ? "text-amber-400"
          : status === "failed"
            ? "text-rose-400"
            : "text-zinc-500";

  return (
    <Tooltip>
      <TooltipTrigger render={<span className="inline-flex" />}>
        <Circle className={cn("h-3.5 w-3.5 fill-current", colorClass)} />
      </TooltipTrigger>
      <TooltipContent>{status}</TooltipContent>
    </Tooltip>
  );
}
