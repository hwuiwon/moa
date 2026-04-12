import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Channel } from "@tauri-apps/api/core";

import { tauriClient } from "@/lib/tauri";
import { normalizeChatMessage, normalizeChatMessages } from "@/types/chat";
import type {
  ApprovalBlock,
  ChatMessage,
  ContentBlock,
  NoticeBlock,
  StreamEvent,
  ToolCallBlock,
  ToolStatus,
} from "@/types/chat";

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
  const [messages, setMessages] = useState<ChatMessage[]>(
    normalizeChatMessages(initialMessages),
  );
  const [isStreaming, setIsStreaming] = useState(false);
  const [isStopping, setIsStopping] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [totalTokens, setTotalTokens] = useState(0);
  const activeRunIdRef = useRef(0);
  const assistantMessageIdRef = useRef<string | null>(null);
  const pendingDeltaRef = useRef("");
  const pendingFinishTextRef = useRef<string | null>(null);
  const rafRef = useRef<number | null>(null);
  const localMessageIndexRef = useRef(0);
  const previousSessionIdRef = useRef(sessionId);

  const updateAssistantMessage = useCallback(
    (
      runId: number,
      updater: (message: ChatMessage) => ChatMessage,
    ) => {
      if (runId !== activeRunIdRef.current || !assistantMessageIdRef.current) {
        return;
      }

      setMessages((current) =>
        current.map((message) =>
          message.id === assistantMessageIdRef.current
            ? updater(normalizeChatMessage(message))
            : normalizeChatMessage(message),
        ),
      );
    },
    [],
  );

  const ensureAssistantMessage = useCallback(
    (runId: number, initialBlocks: ContentBlock[] = []) => {
      if (runId !== activeRunIdRef.current) {
        return;
      }

      if (assistantMessageIdRef.current) {
        return;
      }

      localMessageIndexRef.current += 1;
      const assistantId = `local-assistant-${localMessageIndexRef.current}`;
      assistantMessageIdRef.current = assistantId;
      setMessages((current) => [
        ...current,
        {
          blocks: initialBlocks,
          id: assistantId,
          isStreaming: true,
          role: "assistant",
          timestamp: new Date().toISOString(),
        },
      ]);
    },
    [],
  );

  const appendThinkingBlock = useCallback(
    (runId: number) => {
      ensureAssistantMessage(runId, [{ type: "thinking" }]);
      updateAssistantMessage(runId, (message) => {
        if (message.blocks.some((block) => block.type === "thinking")) {
          return message;
        }

        return {
          ...message,
          blocks: [{ type: "thinking" }, ...message.blocks],
        };
      });
    },
    [ensureAssistantMessage, updateAssistantMessage],
  );

  const removeThinkingBlock = useCallback(
    (runId: number) => {
      updateAssistantMessage(runId, (message) => ({
        ...message,
        blocks: message.blocks.filter((block) => block.type !== "thinking"),
      }));
    },
    [updateAssistantMessage],
  );

  const flushAssistantDelta = useCallback(
    (runId: number) => {
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

      updateAssistantMessage(runId, (message) => {
        const blocks = message.blocks.filter((block) => block.type !== "thinking");

        if (finishedText != null) {
          return {
            ...message,
            blocks: upsertTrailingTextBlock(blocks, finishedText),
            isStreaming: false,
          };
        }

        return {
          ...message,
          blocks: appendTextDelta(blocks, delta),
        };
      });
    },
    [updateAssistantMessage],
  );

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

  const upsertToolBlock = useCallback(
    (runId: number, nextBlock: ToolCallBlock) => {
      const shouldSeedMessage = !assistantMessageIdRef.current;
      ensureAssistantMessage(runId, shouldSeedMessage ? [nextBlock] : []);
      if (shouldSeedMessage) {
        return;
      }
      updateAssistantMessage(runId, (message) => {
        const blocks = [...message.blocks];
        const index = blocks.findIndex(
          (block) => block.type === "tool-call" && block.callId === nextBlock.callId,
        );

        if (index === -1) {
          blocks.push(nextBlock);
          return {
            ...message,
            blocks,
          };
        }

        const existing = blocks[index];
        if (existing?.type !== "tool-call") {
          return message;
        }

        blocks[index] = {
          ...existing,
          ...nextBlock,
          input: nextBlock.input ?? existing.input,
          output: nextBlock.output ?? existing.output,
          errorText: nextBlock.errorText ?? existing.errorText,
          toolName: nextBlock.toolName || existing.toolName,
        };

        return {
          ...message,
          blocks,
        };
      });
    },
    [ensureAssistantMessage, updateAssistantMessage],
  );

  const addApprovalBlock = useCallback(
    (runId: number, approval: ApprovalBlock) => {
      const shouldSeedMessage = !assistantMessageIdRef.current;
      ensureAssistantMessage(runId, shouldSeedMessage ? [approval] : []);
      if (shouldSeedMessage) {
        return;
      }
      updateAssistantMessage(runId, (message) => {
        const blocks = [...message.blocks];
        const index = blocks.findIndex(
          (block) => block.type === "approval" && block.requestId === approval.requestId,
        );

        if (index === -1) {
          blocks.push(approval);
        } else {
          blocks[index] = approval;
        }

        return {
          ...message,
          blocks,
        };
      });
    },
    [ensureAssistantMessage, updateAssistantMessage],
  );

  const addNoticeBlock = useCallback(
    (runId: number, block: NoticeBlock) => {
      const shouldSeedMessage = !assistantMessageIdRef.current;
      ensureAssistantMessage(runId, shouldSeedMessage ? [block] : []);
      if (shouldSeedMessage) {
        return;
      }
      updateAssistantMessage(runId, (message) => {
        const lastBlock = message.blocks[message.blocks.length - 1];
        if (lastBlock?.type === "notice" && lastBlock.message === block.message) {
          return message;
        }

        return {
          ...message,
          blocks: [...message.blocks, block],
        };
      });
    },
    [ensureAssistantMessage, updateAssistantMessage],
  );

  const resetStreamState = useCallback((nextMessages: ChatMessage[]) => {
    activeRunIdRef.current += 1;
    assistantMessageIdRef.current = null;
    pendingDeltaRef.current = "";
    pendingFinishTextRef.current = null;
    setMessages(normalizeChatMessages(nextMessages));
    setIsStreaming(false);
    setIsStopping(false);
    setError(null);
    setTotalTokens(0);

    if (rafRef.current != null) {
      window.cancelAnimationFrame(rafRef.current);
      rafRef.current = null;
    }
  }, []);

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
        blocks: [{ text: content, type: "text" }],
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
            appendThinkingBlock(runId);
            break;
          case "assistantDelta":
            removeThinkingBlock(runId);
            ensureAssistantMessage(runId);
            pendingDeltaRef.current += event.data.text;
            scheduleFlush(runId);
            break;
          case "assistantFinished":
            removeThinkingBlock(runId);
            ensureAssistantMessage(runId);
            pendingFinishTextRef.current = event.data.text;
            scheduleFlush(runId);
            break;
          case "toolUpdate":
          {
              const update = normalizeToolUpdateEvent(event.data);
              if (!update) {
                break;
              }
              upsertToolBlock(
                runId,
                streamToolBlock(
                  update.callId,
                  update.toolName,
                  update.status,
                  update.summary,
                  update.detail,
                ),
              );
            }
            break;
          case "approvalRequired":
          {
              const approval = normalizeApprovalEvent(event.data);
              if (!approval) {
                break;
              }
              addApprovalBlock(runId, approval);
            }
            break;
          case "notice":
          {
              const notice = normalizeNoticeEvent(event.data);
              if (!notice) {
                break;
              }
              addNoticeBlock(runId, notice);
            }
            break;
          case "turnCompleted":
            removeThinkingBlock(runId);
            break;
          case "usageUpdated":
            setTotalTokens(event.data.totalTokens);
            break;
          case "error":
            removeThinkingBlock(runId);
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

          removeThinkingBlock(runId);
          flushAssistantDelta(runId);
          updateAssistantMessage(runId, (message) => ({
            ...message,
            isStreaming: false,
          }));
          setIsStreaming(false);
          setIsStopping(false);
          await Promise.all([
            queryClient.invalidateQueries({ queryKey: ["session-history", sessionId] }),
            queryClient.invalidateQueries({ queryKey: ["session-events", sessionId] }),
            queryClient.invalidateQueries({ queryKey: ["session", sessionId] }),
            queryClient.invalidateQueries({ queryKey: ["sessions"] }),
          ]);
        }
      }
    },
    [
      addApprovalBlock,
      addNoticeBlock,
      appendThinkingBlock,
      ensureAssistantMessage,
      flushAssistantDelta,
      isStreaming,
      queryClient,
      removeThinkingBlock,
      scheduleFlush,
      sessionId,
      updateAssistantMessage,
      upsertToolBlock,
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

function streamToolBlock(
  callId: string,
  toolName: string,
  rawStatus: string,
  summary?: string | null,
  detail?: string | null,
): ToolCallBlock {
  const status = normalizeToolStatus(rawStatus);
  const details = toolSummaryRecord(summary, detail);

  return {
    callId,
    errorText: status === "error" ? detail ?? summary ?? "Tool failed." : undefined,
    input: status === "pending" || status === "running" ? details : undefined,
    output: status === "done" ? details : undefined,
    status,
    toolName,
    type: "tool-call",
  };
}

function toolSummaryRecord(
  summary?: string | null,
  detail?: string | null,
): Record<string, unknown> | undefined {
  const normalizedSummary = summary?.trim();
  const normalizedDetail = detail?.trim();
  if (!normalizedSummary && !normalizedDetail) {
    return undefined;
  }

  return {
    ...(normalizedSummary ? { summary: normalizedSummary } : {}),
    ...(normalizedDetail ? { detail: normalizedDetail } : {}),
  };
}

function normalizeToolStatus(status: string): ToolStatus {
  switch (status) {
    case "pending":
    case "running":
    case "done":
    case "error":
      return status;
    default:
      return "pending";
  }
}

function normalizeToolUpdateEvent(data: unknown) {
  const value = asObject(data);
  const callId = readStringField(value, "callId", "call_id");
  const toolName = readStringField(value, "toolName", "tool_name");
  const status = readStringField(value, "status");

  if (!callId || !toolName || !status) {
    return null;
  }

  return {
    callId,
    detail: readOptionalStringField(value, "detail"),
    status,
    summary: readOptionalStringField(value, "summary"),
    toolName,
  };
}

function normalizeApprovalEvent(data: unknown): ApprovalBlock | null {
  const value = asObject(data);
  const requestId = readStringField(value, "requestId", "request_id");
  const toolName = readStringField(value, "toolName", "tool_name");

  if (!requestId || !toolName) {
    return null;
  }

  return {
    diffPreview:
      readOptionalStringField(value, "diffPreview", "diff_preview") ?? undefined,
    inputSummary:
      readOptionalStringField(value, "inputSummary", "input_summary") ?? "",
    requestId,
    riskLevel:
      readOptionalStringField(value, "riskLevel", "risk_level") ?? "medium",
    toolName,
    type: "approval",
  };
}

function normalizeNoticeEvent(data: unknown): NoticeBlock | null {
  const value = asObject(data);
  const message = readStringField(value, "message");
  if (!message) {
    return null;
  }

  return {
    message,
    type: "notice",
  };
}

function appendTextDelta(blocks: ContentBlock[], delta: string): ContentBlock[] {
  if (!delta) {
    return blocks;
  }

  const nextBlocks = [...blocks];
  const lastBlock = nextBlocks[nextBlocks.length - 1];
  if (lastBlock?.type === "text") {
    nextBlocks[nextBlocks.length - 1] = {
      ...lastBlock,
      text: `${lastBlock.text}${delta}`,
    };
    return nextBlocks;
  }

  nextBlocks.push({
    text: delta,
    type: "text",
  });
  return nextBlocks;
}

function upsertTrailingTextBlock(blocks: ContentBlock[], text: string): ContentBlock[] {
  const nextBlocks = [...blocks];
  const textIndex = findLastTextBlockIndex(nextBlocks);
  if (textIndex >= 0) {
    nextBlocks[textIndex] = {
      text,
      type: "text",
    };
    return nextBlocks;
  }

  nextBlocks.push({
    text,
    type: "text",
  });
  return nextBlocks;
}

function findLastTextBlockIndex(blocks: ContentBlock[]): number {
  for (let index = blocks.length - 1; index >= 0; index -= 1) {
    if (blocks[index]?.type === "text") {
      return index;
    }
  }

  return -1;
}

function isCancellationMessage(message: string): boolean {
  const normalized = message.toLowerCase();
  return normalized.includes("cancelled") || normalized.includes("canceled");
}

function asObject(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return {};
  }

  return value as Record<string, unknown>;
}

function readStringField(
  value: Record<string, unknown>,
  ...keys: string[]
): string {
  for (const key of keys) {
    const field = value[key];
    if (typeof field === "string" && field.length > 0) {
      return field;
    }
  }

  return "";
}

function readOptionalStringField(
  value: Record<string, unknown>,
  ...keys: string[]
): string | null {
  for (const key of keys) {
    const field = value[key];
    if (typeof field === "string") {
      return field;
    }
  }

  return null;
}
