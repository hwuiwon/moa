import { memo } from "react";

import type { ChatMessage } from "@/types/chat";
import { formatRelativeTime } from "@/lib/utils";

type UserMessageProps = {
  message: ChatMessage;
};

/**
 * Flat user transcript row with subtle emphasis.
 */
export const UserMessage = memo(function UserMessage({
  message,
}: UserMessageProps) {
  return (
    <article className="rounded-2xl border border-border bg-muted/40 px-4 py-3">
      <header className="flex items-center justify-between gap-4">
        <p className="text-[11px] font-medium uppercase tracking-widest text-muted-foreground">
          You
        </p>
        <time
          className="text-xs text-muted-foreground"
          dateTime={message.timestamp}
        >
          {formatRelativeTime(message.timestamp)}
        </time>
      </header>
      <div className="mt-2 whitespace-pre-wrap break-words text-sm leading-7">
        {message.content}
      </div>
    </article>
  );
});
