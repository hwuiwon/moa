import {
  Reasoning,
  ReasoningContent,
  ReasoningTrigger,
} from "@/components/prompt-kit/reasoning";
import { SystemMessage } from "@/components/prompt-kit/system-message";
import { ThinkingBar } from "@/components/prompt-kit/thinking-bar";
import { Tool, type ToolPart } from "@/components/prompt-kit/tool";
import type { ContentBlock, ToolCallBlock } from "@/types/chat";

import { ApprovalCard } from "@/components/chat/approval-card";
import { StreamingContent } from "@/components/chat/streaming-content";

type ContentBlockRendererProps = {
  block: ContentBlock;
  isStreaming?: boolean;
  onStop?: () => void;
};

/**
 * Renders one assistant content block using prompt-kit primitives where possible.
 */
export function ContentBlockRenderer({
  block,
  isStreaming = false,
  onStop,
}: ContentBlockRendererProps) {
  switch (block.type) {
    case "text":
      return (
        <StreamingContent
          content={block.text}
          isStreaming={isStreaming}
        />
      );
    case "thinking":
      return <ThinkingBar onStop={onStop} text="Thinking" />;
    case "tool-call":
      return (
        <Tool
          defaultOpen={block.status !== "done"}
          toolPart={mapToolBlockToToolPart(block)}
        />
      );
    case "approval":
      return <ApprovalCard block={block} />;
    case "notice":
      if (isStreaming) {
        return (
          <Reasoning className="w-full" isStreaming={isStreaming}>
            <ReasoningTrigger>Runtime update</ReasoningTrigger>
            <ReasoningContent className="mt-2">
              <SystemMessage>{block.message}</SystemMessage>
            </ReasoningContent>
          </Reasoning>
        );
      }

      return <SystemMessage>{block.message}</SystemMessage>;
    default:
      return null;
  }
}

/**
 * Maps a tool-call content block to the prompt-kit tool part shape.
 */
export function mapToolBlockToToolPart(block: ToolCallBlock): ToolPart {
  return {
    errorText: block.errorText,
    input: block.input,
    output: block.output,
    state: toolState(block.status),
    toolCallId: block.callId,
    type: block.toolName,
  };
}

function toolState(status: ToolCallBlock["status"]): ToolPart["state"] {
  switch (status) {
    case "done":
      return "output-available";
    case "error":
      return "output-error";
    case "pending":
    case "running":
      return "input-streaming";
    default:
      return "input-streaming";
  }
}
