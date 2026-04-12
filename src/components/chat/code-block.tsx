import { useEffect, useMemo, useState } from "react";
import { Check, Copy } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

type CodeBlockProps = {
  code: string;
  language?: string;
  streaming?: boolean;
};

/**
 * Renders fenced markdown code with copy affordance and Shiki highlighting.
 */
export function CodeBlock({
  code,
  language,
  streaming = false,
}: CodeBlockProps) {
  const [copied, setCopied] = useState(false);
  const [html, setHtml] = useState("");

  const normalizedLanguage = useMemo(
    () => normalizeLanguage(language),
    [language],
  );

  useEffect(() => {
    let cancelled = false;

    async function highlight() {
      try {
        const { codeToHtml } = await import("shiki");
        const rendered = await codeToHtml(code, {
          lang: normalizedLanguage,
          theme: "github-dark",
        });

        if (!cancelled) {
          setHtml(rendered);
        }
      } catch {
        if (!cancelled) {
          setHtml("");
        }
      }
    }

    void highlight();

    return () => {
      cancelled = true;
    };
  }, [code, normalizedLanguage]);

  const handleCopy = async () => {
    await navigator.clipboard.writeText(code);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1_500);
  };

  return (
    <div className="my-4 overflow-hidden rounded-xl border border-border bg-card/70">
      <div className="flex items-center justify-between border-b border-border px-3 py-2 text-[11px] uppercase tracking-widest text-muted-foreground">
        <span>{normalizedLanguage}</span>
        <div className="flex items-center gap-2">
          {streaming ? <span className="text-primary">Streaming</span> : null}
          <Button
            className="h-6 px-2 text-[11px]"
            onClick={() => void handleCopy()}
            size="xs"
            type="button"
            variant="ghost"
          >
            {copied ? <Check className="h-3 w-3" /> : <Copy className="h-3 w-3" />}
            {copied ? "Copied" : "Copy"}
          </Button>
        </div>
      </div>

      {html ? (
        <div
          className={cn(
            "[&_pre]:m-0 [&_pre]:overflow-x-auto [&_pre]:bg-transparent [&_pre]:px-4 [&_pre]:py-3 [&_code]:font-mono [&_code]:text-[13px]",
            streaming && "opacity-95",
          )}
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className="overflow-x-auto px-4 py-3 text-[13px] leading-6">
          <code>{code}</code>
        </pre>
      )}
    </div>
  );
}

function normalizeLanguage(language?: string): string {
  if (!language) {
    return "text";
  }

  return language.toLowerCase();
}
