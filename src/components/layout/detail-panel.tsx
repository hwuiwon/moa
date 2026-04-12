import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Activity, Clock3, Layers3, Sigma } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { tauriClient } from "@/lib/tauri";
import { formatAbsoluteDate } from "@/lib/utils";

type DetailPanelProps = {
  activeSessionId: string | null;
};

export function DetailPanel({ activeSessionId }: DetailPanelProps) {
  const session = useQuery({
    queryKey: ["session", activeSessionId],
    queryFn: () => tauriClient.getSession(activeSessionId!),
    enabled: Boolean(activeSessionId),
  });

  return (
    <aside className="flex h-full flex-col border-l border-border bg-sidebar">
      <div className="px-4 py-3">
        <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
          Details
        </p>
        <h2 className="mt-1 text-sm font-semibold">
          Session context
        </h2>
      </div>
      <Separator />
      <ScrollArea className="flex-1">
        <div className="space-y-3 p-4">
          {!activeSessionId ? (
            <div className="rounded-lg border border-dashed border-border p-4 text-sm text-muted-foreground">
              Select a session to inspect metadata and quick stats.
            </div>
          ) : session.isLoading ? (
            <div className="rounded-lg border border-border p-4 text-sm text-muted-foreground">
              Loading session…
            </div>
          ) : session.isError ? (
            <div className="rounded-lg border border-destructive/30 bg-destructive/10 p-4 text-sm text-destructive">
              {String(session.error)}
            </div>
          ) : session.data ? (
            <>
              <div className="rounded-lg border border-border bg-card p-3">
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <p className="truncate text-sm font-medium">
                      {session.data.title ?? "Untitled session"}
                    </p>
                    <p className="mt-1 break-all font-mono text-[11px] text-muted-foreground">
                      {session.data.id}
                    </p>
                  </div>
                  <Badge variant="secondary">{session.data.status}</Badge>
                </div>
              </div>

              <InfoCard
                icon={<Layers3 className="h-3.5 w-3.5" />}
                label="Workspace"
                value={session.data.workspaceId}
              />
              <InfoCard
                icon={<Activity className="h-3.5 w-3.5" />}
                label="Model"
                value={session.data.model}
              />
              <InfoCard
                icon={<Clock3 className="h-3.5 w-3.5" />}
                label="Updated"
                value={formatAbsoluteDate(session.data.updatedAt)}
              />
              <InfoCard
                icon={<Sigma className="h-3.5 w-3.5" />}
                label="Usage"
                value={`${session.data.totalInputTokens + session.data.totalOutputTokens} tokens · $${(
                  session.data.totalCostCents / 100
                ).toFixed(4)}`}
              />

              <div className="rounded-lg border border-border bg-card p-3">
                <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
                  Notes
                </p>
                <p className="mt-2 text-sm text-muted-foreground">
                  This panel becomes the per-session inspection surface for tools,
                  approvals, and event details in later steps.
                </p>
              </div>
            </>
          ) : null}
        </div>
      </ScrollArea>
    </aside>
  );
}

function InfoCard({
  icon,
  label,
  value,
}: {
  icon: ReactNode;
  label: string;
  value: string;
}) {
  return (
    <div className="rounded-lg border border-border bg-card p-3">
      <div className="flex items-center gap-2 text-muted-foreground">
        {icon}
        <p className="text-[11px] uppercase tracking-widest">{label}</p>
      </div>
      <p className="mt-2 break-words text-sm">{value}</p>
    </div>
  );
}
