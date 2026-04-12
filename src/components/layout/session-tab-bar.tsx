import { CSS } from "@dnd-kit/utilities";
import {
  DndContext,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  horizontalListSortingStrategy,
  useSortable,
} from "@dnd-kit/sortable";
import { Circle, GripVertical, MoreHorizontal, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { SessionPreviewDto } from "@/lib/types";
import { cn } from "@/lib/utils";

type SessionTabBarProps = {
  activeSessionId: string | null;
  openTabs: string[];
  sessions: SessionPreviewDto[];
  onCloseTab: (sessionId: string) => void;
  onReorderTabs: (activeId: string, overId: string) => void;
  onSelectSession: (sessionId: string) => void;
};

type TabRecord = {
  sessionId: string;
  status: string;
  title: string;
};

export function SessionTabBar({
  activeSessionId,
  openTabs,
  sessions,
  onCloseTab,
  onReorderTabs,
  onSelectSession,
}: SessionTabBarProps) {
  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 8 },
    }),
  );

  const sessionById = new Map(
    sessions.map((preview) => [preview.summary.sessionId, preview]),
  );
  const tabs: TabRecord[] = openTabs.map((sessionId) => {
    const preview = sessionById.get(sessionId);
    return {
      sessionId,
      status: preview?.summary.status ?? "created",
      title: preview?.summary.title ?? "Untitled session",
    };
  });

  if (!tabs.length) {
    return null;
  }

  const handleDragEnd = (event: DragEndEvent) => {
    const activeId = String(event.active.id);
    const overId = event.over ? String(event.over.id) : null;
    if (!overId || activeId === overId) {
      return;
    }
    onReorderTabs(activeId, overId);
  };

  return (
    <div className="flex items-center gap-2 border-b border-border bg-background/95 px-3 py-2">
      <div className="min-w-0 flex-1 overflow-x-auto">
        <DndContext
          collisionDetection={closestCenter}
          onDragEnd={handleDragEnd}
          sensors={sensors}
        >
          <SortableContext
            items={tabs.map((tab) => tab.sessionId)}
            strategy={horizontalListSortingStrategy}
          >
            <div className="flex min-w-max items-center gap-2 pr-2">
              {tabs.map((tab) => (
                <SortableTab
                  active={tab.sessionId === activeSessionId}
                  key={tab.sessionId}
                  onClose={onCloseTab}
                  onSelect={onSelectSession}
                  tab={tab}
                />
              ))}
            </div>
          </SortableContext>
        </DndContext>
      </div>

      <DropdownMenu>
        <DropdownMenuTrigger
          className={cn(
            "inline-flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground",
          )}
        >
          <MoreHorizontal className="h-4 w-4" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-64">
          {tabs.map((tab) => (
            <DropdownMenuItem
              key={tab.sessionId}
              onClick={() => onSelectSession(tab.sessionId)}
            >
              <div className="flex min-w-0 items-center gap-2">
                <StatusDot status={tab.status} />
                <span className="truncate">{tab.title}</span>
              </div>
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}

function SortableTab({
  active,
  onClose,
  onSelect,
  tab,
}: {
  active: boolean;
  onClose: (sessionId: string) => void;
  onSelect: (sessionId: string) => void;
  tab: TabRecord;
}) {
  const { attributes, listeners, setNodeRef, transform, transition } =
    useSortable({
      id: tab.sessionId,
    });

  return (
    <div
      className={cn(
        "group flex h-9 min-w-[180px] max-w-[240px] items-center gap-2 rounded-lg border px-2 transition-colors",
        active
          ? "border-primary/30 bg-accent text-accent-foreground"
          : "border-border bg-card hover:bg-accent/50",
      )}
      ref={setNodeRef}
      style={{
        transform: CSS.Transform.toString(transform),
        transition,
      }}
    >
      <button
        className="flex min-w-0 flex-1 items-center gap-2 text-left"
        onClick={() => onSelect(tab.sessionId)}
        type="button"
      >
        <span
          className="cursor-grab text-muted-foreground active:cursor-grabbing"
          {...attributes}
          {...listeners}
        >
          <GripVertical className="h-3.5 w-3.5" />
        </span>
        <StatusDot status={tab.status} />
        <span className="truncate text-sm font-medium">{tab.title}</span>
      </button>
      <Button
        className="h-6 w-6 shrink-0 opacity-0 transition-opacity group-hover:opacity-100"
        onClick={() => onClose(tab.sessionId)}
        size="icon"
        type="button"
        variant="ghost"
      >
        <X className="h-3.5 w-3.5" />
      </Button>
    </div>
  );
}

function StatusDot({ status }: { status: string }) {
  const colorClass =
    status === "completed"
      ? "fill-emerald-500 text-emerald-500"
      : status === "running"
        ? "fill-blue-500 text-blue-500"
        : status === "waiting_approval"
          ? "fill-amber-500 text-amber-500"
          : status === "failed"
            ? "fill-destructive text-destructive"
            : "fill-muted-foreground/30 text-muted-foreground/30";

  return <Circle className={cn("h-2.5 w-2.5", colorClass)} />;
}
