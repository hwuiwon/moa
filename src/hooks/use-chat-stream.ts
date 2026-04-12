import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Channel } from "@tauri-apps/api/core";

import { tauriClient } from "@/lib/tauri";
import type { ChatMessage, StreamEvent } from "@/types/chat";

type UseChatStreamOptions = {
  sessionId?: string;
  initialMessages: ChatMessage[];
};

type UseChatStreamResult = {
  messages: ChatMessage[];
  isStreaming: boolean;
  isStopping: boolean;
  error: string | null;
  totalTokens: number;
  sendMessage: (prompt: string) => Promise<void>;
  stopMessage: () => Promise<void>;
};

/**
 * Manages optimistic chat messages and a live streaming turn over the Tauri channel.
 */
export function useChatStream({
  sessionId,
  initialMessages,
}: UseChatStreamOptions): UseChatStreamResult {
  const queryClient = useQueryClient();
  const [messages, setMessages] = useState<ChatMessage[]>(initialMessages);
  const [isStreaming, setIsStreaming] = useState(false);
  const [isStopping, setIsStopping] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [totalTokens, setTotalTokens] = useState(0);
  const activeRunIdRef = useRef(0);
  const assistantMessageIdRef = useRef<string | null>(null);
  const pendingDeltaRef = useRef("");
  const rafRef = useRef<number | null>(null);
  const pendingFinishTextRef = useRef<string | null>(null);
  const localMessageIndexRef = useRef(0);
  const previousSessionIdRef = useRef(sessionId);

  const flushAssistantDelta = useCallback((runId: number) => {
    if (runId !== activeRunIdRef.current) {
      pendingDeltaRef.current = "";
      pendingFinishTextRef.current = null;
      return;
    }

    const delta = pendingDeltaRef.current;
    const finishedText = pendingFinishTextRef.current;
    pendingDeltaRef.current = "";
    pendingFinishTextRef.current = null;

    if (!assistantMessageIdRef.current) {
      return;
    }

    if (!delta && finishedText == null) {
      return;
    }

    setMessages((current) =>
      current.map((message) => {
        if (message.id !== assistantMessageIdRef.current) {
          return message;
        }

        if (finishedText != null) {
          return {
            ...message,
            content: finishedText,
            isStreaming: false,
          };
        }

        return {
          ...message,
          content: `${message.content}${delta}`,
        };
      }),
    );
  }, []);

  const scheduleFlush = useCallback(
    (runId: number) => {
      if (rafRef.current != null) {
        return;
      }

      rafRef.current = window.requestAnimationFrame(() => {
        rafRef.current = null;
        flushAssistantDelta(runId);
      });
    },
    [flushAssistantDelta],
  );

  const resetStreamState = useCallback(
    (nextMessages: ChatMessage[]) => {
      activeRunIdRef.current += 1;
      assistantMessageIdRef.current = null;
      pendingDeltaRef.current = "";
      pendingFinishTextRef.current = null;
      setMessages(nextMessages);
      setIsStreaming(false);
      setIsStopping(false);
      setError(null);
      setTotalTokens(0);

      if (rafRef.current != null) {
        window.cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    },
    [],
  );

  useEffect(() => {
    if (previousSessionIdRef.current !== sessionId) {
      previousSessionIdRef.current = sessionId;
      resetStreamState(initialMessages);
      return;
    }

    if (isStreaming) {
      return;
    }

    resetStreamState(initialMessages);
  }, [initialMessages, isStreaming, resetStreamState, sessionId]);

  useEffect(() => {
    return () => {
      if (rafRef.current != null) {
        window.cancelAnimationFrame(rafRef.current);
      }
    };
  }, []);

  const appendAssistantPlaceholder = useCallback((runId: number) => {
    if (assistantMessageIdRef.current || runId !== activeRunIdRef.current) {
      return;
    }

    localMessageIndexRef.current += 1;
    const assistantId = `local-assistant-${localMessageIndexRef.current}`;
    assistantMessageIdRef.current = assistantId;
    setMessages((current) => [
      ...current,
      {
        content: "",
        id: assistantId,
        isStreaming: true,
        role: "assistant",
        timestamp: new Date().toISOString(),
      },
    ]);
  }, []);

  const sendMessage = useCallback(
    async (prompt: string) => {
      const content = prompt.trim();
      if (!sessionId || !content || isStreaming) {
        return;
      }

      const runId = activeRunIdRef.current + 1;
      activeRunIdRef.current = runId;
      assistantMessageIdRef.current = null;
      pendingDeltaRef.current = "";
      pendingFinishTextRef.current = null;
      setError(null);
      setIsStreaming(true);
      setIsStopping(false);
      setTotalTokens(0);

      localMessageIndexRef.current += 1;
      const userMessage: ChatMessage = {
        content,
        id: `local-user-${localMessageIndexRef.current}`,
        isStreaming: false,
        role: "user",
        timestamp: new Date().toISOString(),
      };
      setMessages((current) => [...current, userMessage]);

      const channel = new Channel<StreamEvent>();
      channel.onmessage = (event) => {
        if (runId !== activeRunIdRef.current) {
          return;
        }

        switch (event.event) {
          case "assistantStarted":
            appendAssistantPlaceholder(runId);
            break;
          case "assistantDelta":
            appendAssistantPlaceholder(runId);
            pendingDeltaRef.current += event.data.text;
            scheduleFlush(runId);
            break;
          case "assistantFinished":
            appendAssistantPlaceholder(runId);
            pendingFinishTextRef.current = event.data.text;
            scheduleFlush(runId);
            break;
          case "usageUpdated":
            setTotalTokens(event.data.totalTokens);
            break;
          case "error":
            if (!isCancellationMessage(event.data.message)) {
              setError(event.data.message);
            }
            break;
          default:
            break;
        }
      };

      try {
        await tauriClient.sendMessage(sessionId, content, channel);
      } catch (invokeError) {
        if (runId === activeRunIdRef.current) {
          const message =
            invokeError instanceof Error ? invokeError.message : String(invokeError);
          if (!isCancellationMessage(message)) {
            setError(message);
          }
        }
      } finally {
        if (runId === activeRunIdRef.current) {
          if (rafRef.current != null) {
            window.cancelAnimationFrame(rafRef.current);
            rafRef.current = null;
          }
          flushAssistantDelta(runId);
          setMessages((current) =>
            current.map((message) =>
              message.id === assistantMessageIdRef.current
                ? { ...message, isStreaming: false }
                : message,
            ),
          );
          setIsStreaming(false);
          setIsStopping(false);
          await Promise.all([
            queryClient.invalidateQueries({ queryKey: ["session-events", sessionId] }),
            queryClient.invalidateQueries({ queryKey: ["session", sessionId] }),
            queryClient.invalidateQueries({ queryKey: ["sessions"] }),
          ]);
        }
      }
    },
    [
      appendAssistantPlaceholder,
      flushAssistantDelta,
      isStreaming,
      queryClient,
      scheduleFlush,
      sessionId,
    ],
  );

  const stopMessage = useCallback(async () => {
    if (!sessionId || !isStreaming || isStopping) {
      return;
    }

    setIsStopping(true);
    try {
      await tauriClient.stopSession(sessionId);
    } catch (stopError) {
      setError(stopError instanceof Error ? stopError.message : String(stopError));
      setIsStopping(false);
    }
  }, [isStopping, isStreaming, sessionId]);

  return useMemo(
    () => ({
      error,
      isStopping,
      isStreaming,
      messages,
      sendMessage,
      stopMessage,
      totalTokens,
    }),
    [error, isStopping, isStreaming, messages, sendMessage, stopMessage, totalTokens],
  );
}

function isCancellationMessage(message: string): boolean {
  const normalized = message.toLowerCase();
  return normalized.includes("cancelled") || normalized.includes("canceled");
}
