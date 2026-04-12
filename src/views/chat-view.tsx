import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import { MessageSquareDashed } from "lucide-react";
import { useParams } from "react-router-dom";

import { Badge } from "@/components/ui/badge";
import { tauriClient } from "@/lib/tauri";
import { formatAbsoluteDate } from "@/lib/utils";
import { useSessionStore } from "@/stores/session";

export function ChatView() {
  const { sessionId } = useParams();
  const setActiveSession = useSessionStore((state) => state.setActiveSession);

  useEffect(() => {
    setActiveSession(sessionId ?? null);
  }, [sessionId, setActiveSession]);

  const session = useQuery({
    queryKey: ["session", sessionId],
    queryFn: () => tauriClient.getSession(sessionId!),
    enabled: Boolean(sessionId),
  });

  if (!sessionId) {
    return (
      <div className="flex h-full items-center justify-center px-8">
        <div className="max-w-md text-center">
          <MessageSquareDashed className="mx-auto h-10 w-10 text-muted-foreground" />
          <h2 className="mt-4 text-lg font-semibold">
            No session selected
          </h2>
          <p className="mt-2 text-sm text-muted-foreground">
            Create a session from the sidebar or top bar. The chat surface will
            render here in the next step.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-6 py-6">
        <div className="flex items-center justify-between gap-4">
          <div>
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Chat
            </p>
            <h1 className="mt-1 text-xl font-semibold">
              {session.data?.title ?? "Session workspace"}
            </h1>
          </div>
          {session.data ? (
            <Badge variant="secondary">{session.data.status}</Badge>
          ) : null}
        </div>

        <div className="mt-6 grid gap-3 sm:grid-cols-2">
          <div className="rounded-lg border border-border bg-card p-3">
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Session ID
            </p>
            <p className="mt-1.5 break-all font-mono text-xs">
              {sessionId}
            </p>
          </div>
          <div className="rounded-lg border border-border bg-card p-3">
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Current model
            </p>
            <p className="mt-1.5 text-sm">
              {session.data?.model ?? "Loading…"}
            </p>
          </div>
          <div className="rounded-lg border border-border bg-card p-3">
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Updated
            </p>
            <p className="mt-1.5 text-sm">
              {session.data ? formatAbsoluteDate(session.data.updatedAt) : "Loading…"}
            </p>
          </div>
          <div className="rounded-lg border border-border bg-card p-3">
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Event count
            </p>
            <p className="mt-1.5 text-sm">
              {session.data?.eventCount ?? "Loading…"}
            </p>
          </div>
        </div>

        <div className="mt-6 rounded-lg border border-dashed border-border p-4 text-sm text-muted-foreground">
          Streaming chat composition, tool cards, approvals, and transcript
          rendering land here in the next steps. The routing and session shell
          are live now.
        </div>
      </div>
    </div>
  );
}
