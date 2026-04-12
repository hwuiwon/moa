import { memo, useMemo, useState } from "react";

import { ContentBlockRenderer } from "@/components/chat/content-block-renderer";
import { ToolGroup } from "@/components/chat/tool-group";
import { FeedbackBar } from "@/components/prompt-kit/feedback-bar";
import { formatRelativeTime } from "@/lib/utils";
import type { ChatMessage, ContentBlock, ToolCallBlock } from "@/types/chat";

type AssistantMessageProps = {
  message: ChatMessage;
  onStop?: () => void;
};

type RenderGroup =
  | { type: "block"; block: ContentBlock }
  | { type: "tool-group"; tools: ToolCallBlock[] };

/**
 * Full-width assistant transcript row with mixed block rendering.
 */
export const AssistantMessage = memo(function AssistantMessage({
  message,
  onStop,
}: AssistantMessageProps) {
  const [feedbackVisible, setFeedbackVisible] = useState(true);
  const [feedbackState, setFeedbackState] = useState<"helpful" | "not-helpful" | null>(
    null,
  );
  const groups = useMemo(() => buildRenderGroups(message.blocks), [message.blocks]);

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

      <div className="mt-3 space-y-4">
        {groups.map((group, index) => {
          const key =
            group.type === "tool-group"
              ? `tool-group-${group.tools.map((tool) => tool.callId).join("-")}`
              : `block-${index}-${group.block.type}`;

          if (group.type === "tool-group") {
            return <ToolGroup key={key} tools={group.tools} />;
          }

          return (
            <ContentBlockRenderer
              block={group.block}
              isStreaming={streamingStateForGroup(message.isStreaming, groups, index)}
              key={key}
              onStop={onStop}
            />
          );
        })}
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

      {!message.isStreaming && feedbackVisible ? (
        <div className="mt-4">
          <FeedbackBar
            onClose={() => setFeedbackVisible(false)}
            onHelpful={() => setFeedbackState("helpful")}
            onNotHelpful={() => setFeedbackState("not-helpful")}
            title={feedbackTitle(feedbackState)}
          />
        </div>
      ) : null}
    </article>
  );
});

function buildRenderGroups(blocks: ContentBlock[]): RenderGroup[] {
  const groups: RenderGroup[] = [];
  let index = 0;

  while (index < blocks.length) {
    const block = blocks[index];
    if (!block) {
      index += 1;
      continue;
    }

    if (block.type !== "tool-call") {
      groups.push({
        block,
        type: "block",
      });
      index += 1;
      continue;
    }

    const tools: ToolCallBlock[] = [];
    while (blocks[index]?.type === "tool-call") {
      const tool = blocks[index];
      if (tool?.type === "tool-call") {
        tools.push(tool);
      }
      index += 1;
    }

    if (tools.length >= 3) {
      groups.push({
        tools,
        type: "tool-group",
      });
      continue;
    }

    for (const tool of tools) {
      groups.push({
        block: tool,
        type: "block",
      });
    }
  }

  return groups;
}

function isStreamingTextBlock(groups: RenderGroup[], index: number): boolean {
  const group = groups[index];
  if (group?.type !== "block" || group.block.type !== "text") {
    return false;
  }

  for (let cursor = groups.length - 1; cursor >= 0; cursor -= 1) {
    const candidate = groups[cursor];
    if (candidate?.type === "block" && candidate.block.type === "text") {
      return cursor === index;
    }
  }

  return false;
}

function streamingStateForGroup(
  isMessageStreaming: boolean,
  groups: RenderGroup[],
  index: number,
) {
  const group = groups[index];
  if (!isMessageStreaming || group?.type !== "block") {
    return false;
  }

  if (group.block.type !== "text") {
    return true;
  }

  return isStreamingTextBlock(groups, index);
}

function feedbackTitle(state: "helpful" | "not-helpful" | null) {
  switch (state) {
    case "helpful":
      return "Marked helpful";
    case "not-helpful":
      return "Marked not helpful";
    default:
      return "Was this helpful?";
  }
}
