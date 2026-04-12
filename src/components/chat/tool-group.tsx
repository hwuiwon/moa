import { CheckCircle, Loader2, XCircle } from "lucide-react";

import { Steps, StepsContent, StepsItem, StepsTrigger } from "@/components/prompt-kit/steps";
import { Tool } from "@/components/prompt-kit/tool";
import type { ToolCallBlock } from "@/types/chat";

import { mapToolBlockToToolPart } from "@/components/chat/content-block-renderer";

type ToolGroupProps = {
  tools: ToolCallBlock[];
};

/**
 * Collapsible grouped rendering for multiple consecutive tool calls.
 */
export function ToolGroup({ tools }: ToolGroupProps) {
  const hasError = tools.some((tool) => tool.status === "error");
  const allDone = tools.every((tool) => tool.status === "done");

  return (
    <Steps defaultOpen={!allDone}>
      <StepsTrigger leftIcon={toolGroupIcon(hasError, allDone)}>
        {toolGroupLabel(tools.length, hasError, allDone)}
      </StepsTrigger>
      <StepsContent>
        {tools.map((tool) => (
          <StepsItem key={tool.callId}>
            <Tool
              defaultOpen={tool.status !== "done"}
              toolPart={mapToolBlockToToolPart(tool)}
            />
          </StepsItem>
        ))}
      </StepsContent>
    </Steps>
  );
}

function toolGroupIcon(hasError: boolean, allDone: boolean) {
  if (hasError) {
    return <XCircle className="size-4 text-red-500" />;
  }

  if (allDone) {
    return <CheckCircle className="size-4 text-green-500" />;
  }

  return <Loader2 className="size-4 animate-spin text-blue-500" />;
}

function toolGroupLabel(count: number, hasError: boolean, allDone: boolean) {
  if (hasError) {
    return `${count} tool${count === 1 ? "" : "s"} used with errors`;
  }

  if (allDone) {
    return `Used ${count} tool${count === 1 ? "" : "s"}`;
  }

  return `Running ${count} tool${count === 1 ? "" : "s"}...`;
}
