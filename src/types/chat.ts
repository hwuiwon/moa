import type { EventRecordDto } from "@/lib/types";

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  content: string;
  timestamp: string;
  isStreaming: boolean;
  tokens?: {
    input: number;
    output: number;
  };
  cost?: number;
  duration?: number;
}

export type StreamEvent =
  | { event: "assistantStarted" }
  | { event: "assistantDelta"; data: { text: string } }
  | { event: "assistantFinished"; data: { text: string } }
  | {
      event: "toolUpdate";
      data: {
        callId: string;
        toolName: string;
        status: string;
        summary?: string | null;
      };
    }
  | {
      event: "approvalRequired";
      data: {
        requestId: string;
        toolName: string;
        riskLevel: string;
        inputSummary: string;
        diffPreview?: string | null;
      };
    }
  | { event: "usageUpdated"; data: { totalTokens: number } }
  | { event: "notice"; data: { message: string } }
  | { event: "turnCompleted" }
  | { event: "error"; data: { message: string } };

type SerializedEventPayload = {
  type?: string;
  data?: Record<string, unknown>;
};

/**
 * Transforms persisted session events into chat transcript messages.
 */
export function eventsToMessages(events: EventRecordDto[]): ChatMessage[] {
  return [...events]
    .sort((left, right) => left.sequenceNum - right.sequenceNum)
    .flatMap((record) => {
      const payload = asPayload(record.payload);
      if (!payload?.type) {
        return [];
      }

      switch (payload.type) {
        case "UserMessage":
          return [userMessageFromEvent(record, payload.data)];
        case "BrainResponse":
          return [assistantMessageFromEvent(record, payload.data)];
        default:
          return [];
      }
    })
    .filter((message): message is ChatMessage => Boolean(message));
}

function userMessageFromEvent(
  record: EventRecordDto,
  data: Record<string, unknown> | undefined,
): ChatMessage | null {
  const text = asString(data?.text);
  const attachments = Array.isArray(data?.attachments) ? data?.attachments.length : 0;
  const content = text || (attachments > 0 ? `[${attachments} attachment${attachments === 1 ? "" : "s"}]` : "");
  if (!content) {
    return null;
  }

  return {
    content,
    id: `event-${record.id}`,
    isStreaming: false,
    role: "user",
    timestamp: record.timestamp,
  };
}

function assistantMessageFromEvent(
  record: EventRecordDto,
  data: Record<string, unknown> | undefined,
): ChatMessage | null {
  const text = asString(data?.text);
  if (!text) {
    return null;
  }

  return {
    content: text,
    cost: asNumber(data?.cost_cents) / 100,
    duration: asNumber(data?.duration_ms),
    id: `event-${record.id}`,
    isStreaming: false,
    role: "assistant",
    timestamp: record.timestamp,
    tokens: {
      input: asNumber(data?.input_tokens),
      output: asNumber(data?.output_tokens),
    },
  };
}

function asPayload(value: unknown): SerializedEventPayload | null {
  if (!value || typeof value !== "object") {
    return null;
  }

  return value as SerializedEventPayload;
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function asNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
