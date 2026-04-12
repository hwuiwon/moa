import type { EventRecordDto } from "@/lib/types";

export type ToolStatus = "pending" | "running" | "done" | "error";

export type TextBlock = {
  type: "text";
  text: string;
};

export type ThinkingBlock = {
  type: "thinking";
};

export type ToolCallBlock = {
  type: "tool-call";
  callId: string;
  toolName: string;
  status: ToolStatus;
  input?: Record<string, unknown>;
  output?: Record<string, unknown>;
  errorText?: string;
  duration?: number;
};

export type ApprovalBlock = {
  type: "approval";
  requestId: string;
  toolName: string;
  riskLevel: string;
  inputSummary: string;
  diffPreview?: string;
  decision?: string;
};

export type NoticeBlock = {
  type: "notice";
  message: string;
};

export type ContentBlock =
  | TextBlock
  | ThinkingBlock
  | ToolCallBlock
  | ApprovalBlock
  | NoticeBlock;

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  blocks: ContentBlock[];
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
        detail?: string | null;
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
  const messages: ChatMessage[] = [];
  let currentAssistant: ChatMessage | null = null;

  const flushAssistant = () => {
    if (!currentAssistant || currentAssistant.blocks.length === 0) {
      currentAssistant = null;
      return;
    }

    messages.push(currentAssistant);
    currentAssistant = null;
  };

  const ensureAssistant = (record: EventRecordDto): ChatMessage => {
    if (!currentAssistant) {
      currentAssistant = {
        blocks: [],
        id: `assistant-${record.id}`,
        isStreaming: false,
        role: "assistant",
        timestamp: record.timestamp,
      };
    }

    return currentAssistant;
  };

  for (const record of [...events].sort((left, right) => left.sequenceNum - right.sequenceNum)) {
    const payload = asPayload(record.payload);
    const type = payload?.type ?? record.eventType;
    const data = payload?.data;

    switch (type) {
      case "UserMessage": {
        flushAssistant();
        const message = userMessageFromEvent(record, data);
        if (message) {
          messages.push(message);
        }
        break;
      }
      case "ToolCall": {
        const assistant = ensureAssistant(record);
        upsertToolBlock(assistant.blocks, {
          callId: asString(data?.tool_id) || `tool-${record.id}`,
          input: valueToRecord(data?.input),
          status: "pending",
          toolName: asString(data?.tool_name) || "tool",
          type: "tool-call",
        });
        break;
      }
      case "ToolResult": {
        const assistant = ensureAssistant(record);
        const callId = asString(data?.tool_id) || `tool-${record.id}`;
        const success = asBoolean(data?.success);
        upsertToolBlock(assistant.blocks, {
          callId,
          duration: asNumber(data?.duration_ms),
          errorText: success ? undefined : toolOutputErrorText(data?.output),
          output: toolOutputToRecord(data?.output),
          status: success ? "done" : "error",
          toolName: findToolName(assistant.blocks, callId),
          type: "tool-call",
        });
        break;
      }
      case "ToolError": {
        const assistant = ensureAssistant(record);
        upsertToolBlock(assistant.blocks, {
          callId: asString(data?.tool_id) || `tool-${record.id}`,
          errorText: asString(data?.error) || "Tool execution failed.",
          status: "error",
          toolName: asString(data?.tool_name) || "tool",
          type: "tool-call",
        });
        break;
      }
      case "ApprovalRequested": {
        const assistant = ensureAssistant(record);
        const approval = approvalBlockFromEvent(data);
        if (approval) {
          upsertApprovalBlock(assistant.blocks, approval);
        }
        break;
      }
      case "BrainResponse": {
        const assistant = ensureAssistant(record);
        const text = asString(data?.text);
        if (text) {
          appendTextBlock(assistant.blocks, text);
        }
        assistant.tokens = {
          input: asNumber(data?.input_tokens),
          output: asNumber(data?.output_tokens),
        };
        assistant.cost = asNumber(data?.cost_cents) / 100;
        assistant.duration = asNumber(data?.duration_ms);
        break;
      }
      case "BrainThinking": {
        const assistant = ensureAssistant(record);
        const summary = asString(data?.summary);
        if (summary) {
          assistant.blocks.push({
            message: summary,
            type: "notice",
          });
        }
        break;
      }
      case "Warning":
      case "Error": {
        const assistant = ensureAssistant(record);
        const message = asString(data?.message);
        if (message) {
          assistant.blocks.push({
            message,
            type: "notice",
          });
        }
        break;
      }
      default:
        break;
    }
  }

  flushAssistant();

  return messages;
}

/**
 * Returns the concatenated text content from a chat message.
 */
export function messageText(message: ChatMessage): string {
  return message.blocks
    .filter((block): block is TextBlock => block.type === "text")
    .map((block) => block.text)
    .join("\n\n");
}

function userMessageFromEvent(
  record: EventRecordDto,
  data: Record<string, unknown> | undefined,
): ChatMessage | null {
  const text = asString(data?.text);
  const attachments = Array.isArray(data?.attachments) ? data.attachments.length : 0;
  const content =
    text ||
    (attachments > 0
      ? `[${attachments} attachment${attachments === 1 ? "" : "s"}]`
      : "");
  if (!content) {
    return null;
  }

  return {
    blocks: [{ text: content, type: "text" }],
    id: `event-${record.id}`,
    isStreaming: false,
    role: "user",
    timestamp: record.timestamp,
  };
}

function approvalBlockFromEvent(
  data: Record<string, unknown> | undefined,
): ApprovalBlock | null {
  const requestId = asString(data?.request_id);
  const toolName = asString(data?.tool_name);
  if (!requestId || !toolName) {
    return null;
  }

  return {
    diffPreview: firstDiffPreview(data?.prompt),
    inputSummary: asString(data?.input_summary),
    requestId,
    riskLevel: asString(data?.risk_level) || "medium",
    toolName,
    type: "approval",
  };
}

function firstDiffPreview(prompt: unknown): string | undefined {
  const promptRecord = asRecord(prompt);
  const firstDiff = Array.isArray(promptRecord?.file_diffs)
    ? asRecord(promptRecord.file_diffs[0])
    : undefined;
  if (!firstDiff) {
    return undefined;
  }

  const path = asString(firstDiff.path);
  const before = asString(firstDiff.before);
  const after = asString(firstDiff.after);
  const preview = [path, "--- before ---", before, "--- after ---", after]
    .filter(Boolean)
    .join("\n");

  return preview ? preview.slice(0, 1_000) : undefined;
}

function appendTextBlock(blocks: ContentBlock[], text: string) {
  const lastBlock = blocks[blocks.length - 1];
  if (lastBlock?.type === "text") {
    lastBlock.text = `${lastBlock.text}${text}`;
    return;
  }

  blocks.push({
    text,
    type: "text",
  });
}

function upsertToolBlock(blocks: ContentBlock[], next: ToolCallBlock) {
  const index = blocks.findIndex(
    (block) => block.type === "tool-call" && block.callId === next.callId,
  );

  if (index === -1) {
    blocks.push(next);
    return;
  }

  const existing = blocks[index];
  if (existing?.type !== "tool-call") {
    return;
  }

  blocks[index] = {
    ...existing,
    ...next,
    input: next.input ?? existing.input,
    output: next.output ?? existing.output,
    errorText: next.errorText ?? existing.errorText,
    toolName: next.toolName || existing.toolName,
  };
}

function upsertApprovalBlock(blocks: ContentBlock[], next: ApprovalBlock) {
  const index = blocks.findIndex(
    (block) => block.type === "approval" && block.requestId === next.requestId,
  );

  if (index === -1) {
    blocks.push(next);
    return;
  }

  blocks[index] = next;
}

function findToolName(blocks: ContentBlock[], callId: string): string {
  const block = blocks.find(
    (entry): entry is ToolCallBlock =>
      entry.type === "tool-call" && entry.callId === callId,
  );
  return block?.toolName ?? "tool";
}

function toolOutputToRecord(output: unknown): Record<string, unknown> | undefined {
  const outputRecord = asRecord(output);
  if (!outputRecord) {
    return undefined;
  }

  const structured = asRecord(outputRecord.structured);
  if (structured && Object.keys(structured).length > 0) {
    return structured;
  }

  const text = toolOutputText(outputRecord);
  if (!text) {
    return undefined;
  }

  return { text };
}

function toolOutputErrorText(output: unknown): string | undefined {
  const outputRecord = asRecord(output);
  if (!outputRecord) {
    return undefined;
  }

  return toolOutputText(outputRecord) || undefined;
}

function toolOutputText(outputRecord: Record<string, unknown>): string {
  if (!Array.isArray(outputRecord.content)) {
    return "";
  }

  return outputRecord.content
    .map((entry) => {
      const block = asRecord(entry);
      if (!block) {
        return "";
      }

      if (block.type === "text") {
        return asString(block.text);
      }

      if (block.type === "json") {
        return JSON.stringify(block.data, null, 2);
      }

      return "";
    })
    .filter(Boolean)
    .join("\n\n");
}

function valueToRecord(value: unknown): Record<string, unknown> | undefined {
  const record = asRecord(value);
  if (record) {
    return record;
  }

  if (typeof value === "string") {
    return { value };
  }

  return undefined;
}

function asPayload(value: unknown): SerializedEventPayload | null {
  if (!value || typeof value !== "object") {
    return null;
  }

  return value as SerializedEventPayload;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }

  return value as Record<string, unknown>;
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function asNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function asBoolean(value: unknown): boolean {
  return typeof value === "boolean" ? value : false;
}
