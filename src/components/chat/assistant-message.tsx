import { memo } from "react";

import { StreamingContent } from "@/components/chat/streaming-content";
import type { ChatMessage } from "@/types/chat";
import { formatRelativeTime } from "@/lib/utils";

type AssistantMessageProps = {
  message: ChatMessage;
};

/**
 * Full-width assistant transcript row with markdown rendering.
 */
export const AssistantMessage = memo(function AssistantMessage({
  message,
}: AssistantMessageProps) {
  return (
    <article className="px-1 py-1">
      <header className="flex items-center justify-between gap-4">
        <p className="text-[11px] font-medium uppercase tracking-widest text-muted-foreground">
          MOA
        </p>
        <time
          className="text-xs text-muted-foreground"
          dateTime={message.timestamp}
        >
          {formatRelativeTime(message.timestamp)}
        </time>
      </header>

      <div className="mt-3">
        <StreamingContent
          content={message.content}
          isStreaming={message.isStreaming}
        />
      </div>

      {message.tokens || message.cost || message.duration ? (
        <footer className="mt-3 flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
          {message.tokens ? (
            <span>
              {message.tokens.input + message.tokens.output} tokens
            </span>
          ) : null}
          {typeof message.cost === "number" ? (
            <span>${message.cost.toFixed(4)}</span>
          ) : null}
          {typeof message.duration === "number" ? (
            <span>{Math.round(message.duration)} ms</span>
          ) : null}
        </footer>
      ) : null}
    </article>
  );
});
