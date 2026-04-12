import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown } from "lucide-react";

import { AssistantMessage } from "@/components/chat/assistant-message";
import { UserMessage } from "@/components/chat/user-message";
import { Button } from "@/components/ui/button";
import type { ChatMessage } from "@/types/chat";

type MessageListProps = {
  error: string | null;
  isLoading: boolean;
  messages: ChatMessage[];
  onStop?: () => void;
};

/**
 * Scrollable transcript with stick-to-bottom behavior and virtualization for long sessions.
 */
export function MessageList({
  error,
  isLoading,
  messages,
  onStop,
}: MessageListProps) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const shouldVirtualize = messages.length > 100;

  const updateBottomState = useCallback(() => {
    const element = scrollRef.current;
    if (!element) {
      return;
    }

    const distanceFromBottom =
      element.scrollHeight - element.scrollTop - element.clientHeight;
    setIsAtBottom(distanceFromBottom < 64);
  }, []);

  const scrollToBottom = useCallback(() => {
    const element = scrollRef.current;
    if (!element) {
      return;
    }

    element.scrollTo({
      behavior: "smooth",
      top: element.scrollHeight,
    });
  }, []);

  useEffect(() => {
    const element = scrollRef.current;
    if (!element) {
      return;
    }

    updateBottomState();
    element.addEventListener("scroll", updateBottomState, { passive: true });
    return () => {
      element.removeEventListener("scroll", updateBottomState);
    };
  }, [updateBottomState]);

  useEffect(() => {
    if (!isAtBottom) {
      return;
    }

    const frame = window.requestAnimationFrame(() => {
      const element = scrollRef.current;
      if (!element) {
        return;
      }

      element.scrollTop = element.scrollHeight;
    });

    return () => window.cancelAnimationFrame(frame);
  }, [isAtBottom, messages]);

  const virtualizer = useVirtualizer({
    count: messages.length,
    estimateSize: () => 220,
    getScrollElement: () => scrollRef.current,
    overscan: 8,
  });

  const emptyState = useMemo(() => {
    if (isLoading) {
      return "Loading session history…";
    }

    if (messages.length > 0) {
      return null;
    }

    return "Start with a concrete request. MOA will stream the response here and keep the full turn history in this session.";
  }, [isLoading, messages.length]);

  return (
    <div className="relative min-h-0 flex-1">
      <div
        className="h-full overflow-y-auto px-6 py-6"
        ref={scrollRef}
      >
        {error ? (
          <div className="mx-auto mb-4 max-w-4xl rounded-xl border border-destructive/30 bg-destructive/10 px-4 py-3 text-sm text-destructive">
            {error}
          </div>
        ) : null}

        {emptyState ? (
          <div className="mx-auto flex h-full max-w-4xl items-center justify-center">
            <div className="max-w-xl rounded-2xl border border-dashed border-border bg-card/60 px-6 py-8 text-center">
              <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
                Chat
              </p>
              <h2 className="mt-3 text-2xl font-semibold">
                Session is ready
              </h2>
              <p className="mt-3 text-sm leading-7 text-muted-foreground">
                {emptyState}
              </p>
            </div>
          </div>
        ) : shouldVirtualize ? (
          <div
            className="relative mx-auto w-full max-w-4xl"
            style={{ height: `${virtualizer.getTotalSize()}px` }}
          >
            {virtualizer.getVirtualItems().map((item) => {
              const message = messages[item.index];
              if (!message) {
                return null;
              }

              return (
                <div
                  className="absolute left-0 top-0 w-full pb-5"
                  key={message.id}
                  ref={virtualizer.measureElement}
                  style={{ transform: `translateY(${item.start}px)` }}
                >
                  <MessageRow message={message} onStop={onStop} />
                </div>
              );
            })}
          </div>
        ) : (
          <div className="mx-auto w-full max-w-4xl space-y-5">
            {messages.map((message) => (
              <MessageRow key={message.id} message={message} onStop={onStop} />
            ))}
          </div>
        )}
      </div>

      {!isAtBottom && messages.length > 0 ? (
        <div className="pointer-events-none absolute inset-x-0 bottom-5 flex justify-center">
          <Button
            className="pointer-events-auto shadow-lg"
            onClick={scrollToBottom}
            size="sm"
            type="button"
            variant="secondary"
          >
            <ArrowDown className="h-3.5 w-3.5" />
            Jump to bottom
          </Button>
        </div>
      ) : null}
    </div>
  );
}

function MessageRow({
  message,
  onStop,
}: {
  message: ChatMessage;
  onStop?: () => void;
}) {
  if (message.role === "user") {
    return <UserMessage message={message} />;
  }

  return <AssistantMessage message={message} onStop={onStop} />;
}
