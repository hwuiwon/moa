import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Channel } from "@tauri-apps/api/core";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { toast } from "sonner";

import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";
import {
  createTranscriptState,
  reduceTranscript,
  transcriptMessages,
} from "@/types/chat";
import type {
  ApprovalBlock,
  ChatMessage,
  NoticeBlock,
  StreamEvent,
  ToolCallBlock,
  ToolStatus,
  TranscriptAction,
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
    transcriptMessages(
      reduceTranscript(createTranscriptState(), {
        messages: initialMessages,
        type: "reset",
      }),
    ),
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
  const notifiedApprovalsRef = useRef(new Set<string>());
  const notificationPermissionRef = useRef<boolean | null>(null);
  const previousSessionIdRef = useRef(sessionId);

  const applyTranscriptAction = useCallback((action: TranscriptAction) => {
    setMessages((current) =>
      transcriptMessages(reduceTranscript(createTranscriptState(current), action)),
    );
  }, []);

  const ensureAssistantId = useCallback((runId: number) => {
    if (runId !== activeRunIdRef.current) {
      return null;
    }

    if (assistantMessageIdRef.current) {
      return assistantMessageIdRef.current;
    }

    localMessageIndexRef.current += 1;
    assistantMessageIdRef.current = `local-assistant-${localMessageIndexRef.current}`;
    return assistantMessageIdRef.current;
  }, []);

  const appendThinkingBlock = useCallback(
    (runId: number) => {
      const assistantId = ensureAssistantId(runId);
      if (!assistantId) {
        return;
      }

      applyTranscriptAction({
        assistantId,
        timestamp: new Date().toISOString(),
        type: "assistant-thinking",
      });
    },
    [applyTranscriptAction, ensureAssistantId],
  );

  const removeThinkingBlock = useCallback(
    (runId: number) => {
      const assistantId = assistantMessageIdRef.current;
      if (runId !== activeRunIdRef.current || !assistantId) {
        return;
      }

      applyTranscriptAction({
        assistantId,
        type: "assistant-remove-thinking",
      });
    },
    [applyTranscriptAction],
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

      if (!delta && finishedText == null) {
        return;
      }

      const assistantId = ensureAssistantId(runId);
      if (!assistantId) {
        return;
      }

      if (finishedText != null) {
        applyTranscriptAction({
          assistantId,
          text: finishedText,
          timestamp: new Date().toISOString(),
          type: "assistant-text-set",
        });
        return;
      }

      applyTranscriptAction({
        assistantId,
        text: delta,
        timestamp: new Date().toISOString(),
        type: "assistant-text-delta",
      });
    },
    [applyTranscriptAction, ensureAssistantId],
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
      const assistantId = ensureAssistantId(runId);
      if (!assistantId) {
        return;
      }

      applyTranscriptAction({
        assistantId,
        block: nextBlock,
        timestamp: new Date().toISOString(),
        type: "assistant-tool",
      });
    },
    [applyTranscriptAction, ensureAssistantId],
  );

  const addApprovalBlock = useCallback(
    (runId: number, approval: ApprovalBlock) => {
      const assistantId = ensureAssistantId(runId);
      if (!assistantId) {
        return;
      }

      applyTranscriptAction({
        assistantId,
        block: approval,
        timestamp: new Date().toISOString(),
        type: "assistant-approval",
      });
    },
    [applyTranscriptAction, ensureAssistantId],
  );

  const addNoticeBlock = useCallback(
    (runId: number, block: NoticeBlock) => {
      const assistantId = ensureAssistantId(runId);
      if (!assistantId) {
        return;
      }

      applyTranscriptAction({
        assistantId,
        block,
        timestamp: new Date().toISOString(),
        type: "assistant-notice",
      });
    },
    [applyTranscriptAction, ensureAssistantId],
  );

  const resetStreamState = useCallback(
    (nextMessages: ChatMessage[]) => {
      activeRunIdRef.current += 1;
      assistantMessageIdRef.current = null;
      pendingDeltaRef.current = "";
      pendingFinishTextRef.current = null;
      setMessages(
        transcriptMessages(
          reduceTranscript(createTranscriptState(), {
            messages: nextMessages,
            type: "reset",
          }),
        ),
      );
      setIsStreaming(false);
      setIsStopping(false);
      setError(null);
      setTotalTokens(0);
      notifiedApprovalsRef.current.clear();

      if (rafRef.current != null) {
        window.cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    },
    [],
  );

  const notifyApproval = useCallback(async (approval: ApprovalBlock) => {
    if (notifiedApprovalsRef.current.has(approval.requestId)) {
      return;
    }

    notifiedApprovalsRef.current.add(approval.requestId);
    toast.warning(`Approval required: ${approval.toolName}`, {
      description:
        approval.inputSummary || "Review the pending request in the transcript.",
    });

    try {
      if (notificationPermissionRef.current == null) {
        notificationPermissionRef.current = await isPermissionGranted();
      }

      if (!notificationPermissionRef.current) {
        notificationPermissionRef.current =
          (await requestPermission()) === "granted";
      }

      if (!notificationPermissionRef.current) {
        return;
      }

      await sendNotification({
        body:
          approval.inputSummary || `${approval.toolName} is waiting for a decision.`,
        title: "MOA approval required",
      });
    } catch {
      // Notification delivery is best-effort only.
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
      applyTranscriptAction({
        message: {
          blocks: [{ text: content, type: "text" }],
          id: `local-user-${localMessageIndexRef.current}`,
          isStreaming: false,
          role: "user",
          timestamp: new Date().toISOString(),
        },
        type: "user-message",
      });

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
            pendingDeltaRef.current += event.data.text;
            scheduleFlush(runId);
            break;
          case "assistantFinished":
            removeThinkingBlock(runId);
            pendingFinishTextRef.current = event.data.text;
            scheduleFlush(runId);
            break;
          case "toolUpdate": {
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
            break;
          }
          case "approvalRequired": {
            const approval = normalizeApprovalEvent(event.data);
            if (!approval) {
              break;
            }
            addApprovalBlock(runId, approval);
            void notifyApproval(approval);
            break;
          }
          case "notice": {
            const notice = normalizeNoticeEvent(event.data);
            if (!notice) {
              break;
            }
            addNoticeBlock(runId, notice);
            break;
          }
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
          if (assistantMessageIdRef.current) {
            applyTranscriptAction({
              assistantId: assistantMessageIdRef.current,
              type: "assistant-finish",
            });
          }
          setIsStreaming(false);
          setIsStopping(false);
          await Promise.all([
            queryClient.invalidateQueries({
              queryKey: queryKeys.sessionHistory(sessionId),
            }),
            queryClient.invalidateQueries({
              queryKey: queryKeys.sessionEvents(sessionId),
            }),
            queryClient.invalidateQueries({
              queryKey: queryKeys.session(sessionId),
            }),
            queryClient.invalidateQueries({ queryKey: queryKeys.sessions() }),
          ]);
        }
      }
    },
    [
      addApprovalBlock,
      addNoticeBlock,
      appendThinkingBlock,
      applyTranscriptAction,
      flushAssistantDelta,
      isStreaming,
      notifyApproval,
      queryClient,
      removeThinkingBlock,
      scheduleFlush,
      sessionId,
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
