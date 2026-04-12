import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { CircularLoader } from "@/components/prompt-kit/loader";
import { ContextWindowBar } from "@/components/layout/context-window-bar";
import { useSessionMeta } from "@/hooks/use-session-meta";
import {
  formatAbsoluteDate,
  formatDuration,
  formatTokenCount,
  formatUsdFromCents,
} from "@/lib/utils";

type SessionInfoPanelProps = {
  activeSessionId: string | null;
};

export function SessionInfoPanel({ activeSessionId }: SessionInfoPanelProps) {
  const session = useSessionMeta(activeSessionId);

  return (
    <aside className="flex h-full flex-col border-l border-border bg-sidebar">
      <div className="px-4 py-3">
        <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
          Session info
        </p>
        <h2 className="mt-1 text-sm font-semibold">Resource usage</h2>
      </div>
      <Separator />
      <ScrollArea className="flex-1">
        <div className="space-y-4 p-4">
          {!activeSessionId ? (
            <EmptyState message="Select a session to inspect cost, token usage, tools, and context pressure." />
          ) : session.isLoading ? (
            <LoadingState />
          ) : session.isError || !session.summary ? (
            <EmptyState
              message={
                session.error instanceof Error
                  ? session.error.message
                  : "Failed to load session metadata."
              }
              tone="error"
            />
          ) : (
            <>
              <Card size="sm">
                <CardHeader>
                  <CardTitle className="truncate text-sm">
                    {session.summary.meta.title ?? "Untitled session"}
                  </CardTitle>
                </CardHeader>
                <CardContent className="space-y-3 text-sm">
                  <div className="flex items-center justify-between gap-3">
                    <Badge variant="secondary">{session.summary.meta.status}</Badge>
                    <span className="truncate text-xs text-muted-foreground">
                      {session.summary.meta.model}
                    </span>
                  </div>
                  <dl className="space-y-2 text-xs text-muted-foreground">
                    <div className="flex items-start justify-between gap-3">
                      <dt>Created</dt>
                      <dd className="text-right text-foreground">
                        {formatAbsoluteDate(session.summary.meta.createdAt)}
                      </dd>
                    </div>
                    <div className="flex items-start justify-between gap-3">
                      <dt>Updated</dt>
                      <dd className="text-right text-foreground">
                        {formatAbsoluteDate(session.summary.meta.updatedAt)}
                      </dd>
                    </div>
                    <div className="flex items-start justify-between gap-3">
                      <dt>Session ID</dt>
                      <dd className="max-w-[180px] break-all text-right font-mono text-[11px] text-foreground">
                        {session.summary.meta.id}
                      </dd>
                    </div>
                  </dl>
                </CardContent>
              </Card>

              <div className="grid grid-cols-2 gap-3">
                <MetricCard label="Duration" value={formatDuration(session.summary.durationMs)} />
                <MetricCard label="Turns" value={String(session.summary.turnCount)} />
                <MetricCard
                  label="Tokens"
                  value={formatTokenCount(session.summary.totalTokens)}
                />
                <MetricCard
                  label="Cost"
                  value={formatUsdFromCents(session.summary.meta.totalCostCents)}
                />
              </div>

              <Card size="sm">
                <CardHeader>
                  <CardTitle className="text-sm">Context window</CardTitle>
                </CardHeader>
                <CardContent>
                  <ContextWindowBar
                    contextWindow={session.summary.contextWindow}
                    totalTokens={session.summary.totalTokens}
                  />
                </CardContent>
              </Card>

              <Card size="sm">
                <CardHeader>
                  <CardTitle className="text-sm">Tools used</CardTitle>
                </CardHeader>
                <CardContent className="px-0">
                  {session.summary.toolsUsed.length ? (
                    <Table>
                      <TableHeader>
                        <TableRow>
                          <TableHead className="pl-4 text-xs">Tool</TableHead>
                          <TableHead className="text-xs">Calls</TableHead>
                          <TableHead className="text-xs">Success</TableHead>
                          <TableHead className="pr-4 text-xs">Avg</TableHead>
                        </TableRow>
                      </TableHeader>
                      <TableBody>
                        {session.summary.toolsUsed.map((tool) => (
                          <TableRow key={tool.toolName}>
                            <TableCell className="pl-4">
                              <div className="flex flex-col gap-1">
                                <span className="text-sm font-medium">
                                  {tool.toolName}
                                </span>
                                <div className="flex items-center gap-2">
                                  <StatusBadge status={tool.lastStatus} />
                                  {tool.lastUpdatedAt ? (
                                    <span className="text-[11px] text-muted-foreground">
                                      {formatAbsoluteDate(tool.lastUpdatedAt)}
                                    </span>
                                  ) : null}
                                </div>
                              </div>
                            </TableCell>
                            <TableCell>{tool.totalCalls}</TableCell>
                            <TableCell>
                              {tool.totalCalls
                                ? `${Math.round((tool.successes / tool.totalCalls) * 100)}%`
                                : "0%"}
                            </TableCell>
                            <TableCell className="pr-4">
                              {tool.avgDurationMs
                                ? formatDuration(tool.avgDurationMs)
                                : "—"}
                            </TableCell>
                          </TableRow>
                        ))}
                      </TableBody>
                    </Table>
                  ) : (
                    <div className="px-4 pb-2 text-sm text-muted-foreground">
                      No tools have been used in this session yet.
                    </div>
                  )}
                </CardContent>
              </Card>
            </>
          )}
        </div>
      </ScrollArea>
    </aside>
  );
}

function MetricCard({
  label,
  value,
}: {
  label: string;
  value: string;
}) {
  return (
    <Card size="sm">
      <CardHeader className="pb-0">
        <CardTitle className="text-[11px] uppercase tracking-widest text-muted-foreground">
          {label}
        </CardTitle>
      </CardHeader>
      <CardContent>
        <p className="text-sm font-medium">{value}</p>
      </CardContent>
    </Card>
  );
}

function StatusBadge({ status }: { status: "pending" | "running" | "done" | "error" }) {
  const variant =
    status === "done" ? "secondary" : status === "error" ? "destructive" : "outline";

  return <Badge variant={variant}>{status}</Badge>;
}

function LoadingState() {
  return (
    <div className="flex min-h-40 items-center justify-center rounded-xl border border-border bg-card">
      <div className="flex items-center gap-3 text-sm text-muted-foreground">
        <CircularLoader size="sm" />
        Loading session metadata…
      </div>
    </div>
  );
}

function EmptyState({
  message,
  tone = "default",
}: {
  message: string;
  tone?: "default" | "error";
}) {
  return (
    <div
      className={
        tone === "error"
          ? "rounded-xl border border-destructive/30 bg-destructive/10 p-4 text-sm text-destructive"
          : "rounded-xl border border-dashed border-border p-4 text-sm text-muted-foreground"
      }
    >
      {message}
    </div>
  );
}
