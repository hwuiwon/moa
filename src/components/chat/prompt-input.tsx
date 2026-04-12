import { useState } from "react";
import { LoaderCircle, Square, SendHorizonal } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";

type PromptInputProps = {
  currentModel?: string;
  disabled?: boolean;
  isStreaming: boolean;
  isStopping: boolean;
  totalTokens: number;
  onSend: (prompt: string) => Promise<void>;
  onStop: () => Promise<void>;
};

/**
 * Message composer with submit-on-enter and hard stop control.
 */
export function PromptInput({
  currentModel,
  disabled = false,
  isStreaming,
  isStopping,
  totalTokens,
  onSend,
  onStop,
}: PromptInputProps) {
  const [value, setValue] = useState("");

  const submit = async () => {
    const prompt = value.trim();
    if (!prompt || disabled || isStreaming) {
      return;
    }

    await onSend(prompt);
    setValue("");
  };

  return (
    <div className="border-t border-border bg-background/95 px-6 py-4 backdrop-blur">
      <div className="mx-auto max-w-4xl">
        <div className="rounded-2xl border border-border bg-card p-3 shadow-sm">
          <Textarea
            className="min-h-28 border-0 bg-transparent px-0 py-0 text-sm shadow-none focus-visible:ring-0"
            disabled={disabled || isStreaming}
            onChange={(event) => setValue(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                void submit();
              }
            }}
            placeholder={
              disabled
                ? "Create or select a session to start chatting."
                : "Message MOA…"
            }
            value={value}
          />

          <div className="mt-3 flex items-center justify-between gap-3">
            <div className="text-xs text-muted-foreground">
              <span>{currentModel ?? "Model loading…"}</span>
              {totalTokens > 0 ? <span> · {totalTokens} tokens</span> : null}
            </div>

            <div className="flex items-center gap-2">
              {isStreaming ? (
                <Button
                  onClick={() => void onStop()}
                  size="sm"
                  type="button"
                  variant="outline"
                >
                  {isStopping ? (
                    <LoaderCircle className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <Square className="h-3.5 w-3.5 fill-current" />
                  )}
                  {isStopping ? "Stopping" : "Stop"}
                </Button>
              ) : (
                <Button
                  disabled={disabled || !value.trim()}
                  onClick={() => void submit()}
                  size="sm"
                  type="button"
                >
                  <SendHorizonal className="h-3.5 w-3.5" />
                  Send
                </Button>
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
