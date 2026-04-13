import { useMemo, useState } from "react";
import ReactDiffViewer, { DiffMethod } from "react-diff-viewer-continued";

import { CodeBlock, CodeBlockCode, CodeBlockGroup } from "@/components/prompt-kit/code-block";
import { Badge } from "@/components/ui/badge";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { cn } from "@/lib/utils";

type DiffViewerProps = {
  className?: string;
  diff: string;
};

type DiffMode = "unified" | "split";

/**
 * Renders a unified diff with a split/unified toggle and prompt-kit code styling.
 */
export function DiffViewer({ className, diff }: DiffViewerProps) {
  const [mode, setMode] = useState<DiffMode>("unified");
  const parsed = useMemo(() => parseUnifiedDiff(diff), [diff]);

  if (!parsed) {
    return (
      <CodeBlock className={className}>
        <CodeBlockCode code={diff} language="diff" theme="github-dark" />
      </CodeBlock>
    );
  }

  return (
    <CodeBlock className={cn("overflow-hidden", className)}>
      <CodeBlockGroup className="border-b border-border px-3 py-2">
        <div className="flex items-center gap-2">
          <Badge variant="outline">Diff</Badge>
          <span className="text-xs text-muted-foreground">
            {parsed.hunkCount} hunk{parsed.hunkCount === 1 ? "" : "s"}
          </span>
        </div>

        <ToggleGroup
          onValueChange={(value) => {
            const nextMode = value[0];
            if (nextMode === "unified" || nextMode === "split") {
              setMode(nextMode);
            }
          }}
          value={[mode]}
          variant="outline"
        >
          <ToggleGroupItem aria-label="Unified diff" value="unified">
            Unified
          </ToggleGroupItem>
          <ToggleGroupItem aria-label="Split diff" value="split">
            Split
          </ToggleGroupItem>
        </ToggleGroup>
      </CodeBlockGroup>

      <div className="overflow-auto bg-card text-[13px]">
        <ReactDiffViewer
          compareMethod={DiffMethod.WORDS}
          disableWordDiff={false}
          hideLineNumbers={false}
          newValue={parsed.after}
          oldValue={parsed.before}
          renderContent={(content) => (
            <span className="font-mono text-[12px] leading-6 whitespace-pre-wrap">
              {content}
            </span>
          )}
          splitView={mode === "split"}
          styles={diffViewerStyles}
          useDarkTheme
        />
      </div>
    </CodeBlock>
  );
}

function parseUnifiedDiff(diff: string) {
  const lines = diff.split("\n");
  if (!lines.length) {
    return null;
  }

  let sawChange = false;
  let hunkCount = 0;
  const before: string[] = [];
  const after: string[] = [];

  for (const line of lines) {
    if (line.startsWith("---") || line.startsWith("+++")) {
      continue;
    }

    if (line.startsWith("@@")) {
      hunkCount += 1;
      continue;
    }

    if (line.startsWith("-")) {
      before.push(line.slice(1));
      sawChange = true;
      continue;
    }

    if (line.startsWith("+")) {
      after.push(line.slice(1));
      sawChange = true;
      continue;
    }

    const shared = line.startsWith(" ") ? line.slice(1) : line;
    before.push(shared);
    after.push(shared);
  }

  if (!sawChange) {
    return null;
  }

  return {
    after: after.join("\n"),
    before: before.join("\n"),
    hunkCount,
  };
}

const diffViewerStyles = {
  contentText: {
    color: "inherit",
    fontFamily:
      'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace',
    fontSize: "12px",
    lineHeight: "1.6",
  },
  diffContainer: {
    border: "none",
  },
  emptyGutter: {
    background: "transparent",
  },
  gutter: {
    background: "transparent",
    minWidth: "40px",
  },
  highlightedGutter: {
    background: "rgba(120, 119, 198, 0.16)",
  },
  highlightedLine: {
    background: "rgba(120, 119, 198, 0.12)",
  },
  line: {
    padding: "0 12px",
  },
  marker: {
    color: "rgb(156 163 175)",
  },
  splitView: {
    borderCollapse: "collapse" as const,
  },
};
