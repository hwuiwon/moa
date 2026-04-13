import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown } from "lucide-react";

import { AssistantMessage } from "@/components/chat/assistant-message";
import { Loader } from "@/components/prompt-kit/loader";
import { PromptSuggestion } from "@/components/prompt-kit/prompt-suggestion";
import { SystemMessage } from "@/components/prompt-kit/system-message";
import { TextShimmer } from "@/components/prompt-kit/text-shimmer";
import { UserMessage } from "@/components/chat/user-message";
import { Button } from "@/components/ui/button";
import type { ChatMessage } from "@/types/chat";

type MessageListProps = {
  error: string | null;
  isLoading: boolean;
  messages: ChatMessage[];
  onSuggestion?: (prompt: string) => void;
  onStop?: () => void;
};

const EMPTY_SESSION_SUGGESTIONS = [
  "Summarize the repository structure",
  "Search the web for the latest Rust release notes",
  "Draft a migration plan for the current workspace",
];

/**
 * Scrollable transcript with stick-to-bottom behavior and virtualization for long sessions.
 */
export function MessageList({
  error,
  isLoading,
  messages,
  onSuggestion,
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

  const showEmptyState = useMemo(
    () => !isLoading && messages.length === 0,
    [isLoading, messages.length],
  );

  return (
    <div className="relative min-h-0 flex-1">
      <div
        className="h-full overflow-y-auto px-6 py-6"
        ref={scrollRef}
      >
        {error ? (
          <SystemMessage
            className="mx-auto mb-4 max-w-4xl"
            fill
            variant="error"
          >
            {error}
          </SystemMessage>
        ) : null}

        {isLoading ? (
          <div className="mx-auto flex h-full max-w-4xl items-center justify-center">
            <div className="flex flex-col items-center gap-4 rounded-2xl border border-dashed border-border bg-card/60 px-8 py-10 text-center">
              <Loader variant="dots" />
              <TextShimmer as="p" className="text-sm">
                Loading session history…
              </TextShimmer>
            </div>
          </div>
        ) : showEmptyState ? (
          <div className="mx-auto flex h-full max-w-4xl items-center justify-center">
            <div className="max-w-2xl rounded-2xl border border-dashed border-border bg-card/60 px-6 py-8 text-center">
              <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
                Chat
              </p>
              <h2 className="mt-3 text-2xl font-semibold">
                Session is ready
              </h2>
              <p className="mt-3 text-sm leading-7 text-muted-foreground">
                Start with a concrete request. MOA will stream the response here and keep the full turn history in this session.
              </p>
              <div className="mt-5 flex flex-wrap justify-center gap-2">
                {EMPTY_SESSION_SUGGESTIONS.map((suggestion) => (
                  <PromptSuggestion
                    key={suggestion}
                    onClick={() => onSuggestion?.(suggestion)}
                    size="sm"
                    type="button"
                    variant="outline"
                  >
                    {suggestion}
                  </PromptSuggestion>
                ))}
              </div>
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
