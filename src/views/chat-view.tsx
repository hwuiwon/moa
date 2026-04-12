import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";

import { MessageList } from "@/components/chat/message-list";
import { PromptInput } from "@/components/chat/prompt-input";
import { Badge } from "@/components/ui/badge";
import { tauriClient } from "@/lib/tauri";
import { formatAbsoluteDate } from "@/lib/utils";
import { useChatStream } from "@/hooks/use-chat-stream";
import { useSessionHistory } from "@/hooks/use-session-history";
import { useSessionStore } from "@/stores/session";

/**
 * Main chat transcript view for one session.
 */
export function ChatView() {
  const { sessionId } = useParams({ strict: false });
  const setActiveSession = useSessionStore((state) => state.setActiveSession);

  useEffect(() => {
    setActiveSession(sessionId ?? null);
  }, [sessionId, setActiveSession]);

  const session = useQuery({
    enabled: Boolean(sessionId),
    queryKey: ["session", sessionId],
    queryFn: () => tauriClient.getSession(sessionId!),
  });
  const history = useSessionHistory(sessionId);
  const stream = useChatStream({
    initialMessages: history.data ?? [],
    sessionId,
  });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="border-b border-border px-6 py-4">
        <div className="mx-auto flex max-w-4xl items-center justify-between gap-6">
          <div className="min-w-0">
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Chat
            </p>
            <h1 className="mt-1 truncate text-xl font-semibold">
              {session.data?.title ?? "Session workspace"}
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              {session.data
                ? `Updated ${formatAbsoluteDate(session.data.updatedAt)}`
                : "Select a session or create a new one to start chatting."}
            </p>
          </div>

          <div className="flex items-center gap-2">
            {session.data ? (
              <Badge variant="secondary">{session.data.status}</Badge>
            ) : null}
            {stream.isStreaming ? (
              <Badge variant="outline">Streaming</Badge>
            ) : null}
          </div>
        </div>
      </div>

      <MessageList
        error={stream.error}
        isLoading={history.isLoading}
        messages={stream.messages}
        onStop={() => {
          void stream.stopMessage();
        }}
      />

      <PromptInput
        currentModel={session.data?.model}
        disabled={!sessionId}
        isStopping={stream.isStopping}
        isStreaming={stream.isStreaming}
        onSend={stream.sendMessage}
        onStop={stream.stopMessage}
        totalTokens={stream.totalTokens}
      />
    </div>
  );
}
