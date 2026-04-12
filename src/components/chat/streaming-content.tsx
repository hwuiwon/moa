import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { CodeBlock } from "@/components/chat/code-block";
import { cn } from "@/lib/utils";

type StreamingContentProps = {
  content: string;
  isStreaming?: boolean;
};

/**
 * Renders assistant markdown while a response is still streaming.
 */
export function StreamingContent({
  content,
  isStreaming = false,
}: StreamingContentProps) {
  return (
    <div
      className={cn(
        "prose prose-neutral max-w-none text-sm leading-7 dark:prose-invert",
        "prose-headings:mb-3 prose-headings:mt-6 prose-p:my-3 prose-li:my-1.5",
        "prose-pre:bg-transparent prose-pre:p-0 prose-code:rounded prose-code:bg-muted prose-code:px-1 prose-code:py-0.5 prose-code:before:content-none prose-code:after:content-none",
        isStreaming && "after:ml-1 after:inline-block after:h-4 after:w-2 after:animate-pulse after:rounded-sm after:bg-primary after:align-middle after:content-['']",
      )}
    >
      <ReactMarkdown
        components={{
          a: ({ children, href }) => (
            <a
              className="text-primary underline underline-offset-4"
              href={href}
              rel="noreferrer"
              target="_blank"
            >
              {children}
            </a>
          ),
          code: ({ children, className }) => {
            const text = String(children).replace(/\n$/, "");
            const match = /language-([\w-]+)/.exec(className ?? "");

            if (match) {
              return (
                <CodeBlock
                  code={text}
                  language={match[1]}
                  streaming={isStreaming}
                />
              );
            }

            return (
              <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em]">
                {children}
              </code>
            );
          },
        }}
        remarkPlugins={[remarkGfm]}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}
