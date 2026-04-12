import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

type ContextWindowBarProps = {
  contextWindow: number | null;
  totalTokens: number;
};

const SEGMENT_COUNT = 24;

export function ContextWindowBar({
  contextWindow,
  totalTokens,
}: ContextWindowBarProps) {
  if (!contextWindow) {
    return (
      <div className="rounded-lg border border-dashed border-border px-3 py-4 text-sm text-muted-foreground">
        Context window unavailable for this model.
      </div>
    );
  }

  const ratio = Math.min(1, totalTokens / contextWindow);
  const activeSegments =
    totalTokens > 0 ? Math.max(1, Math.round(ratio * SEGMENT_COUNT)) : 0;
  const toneClass =
    ratio >= 0.85
      ? "bg-destructive"
      : ratio >= 0.65
        ? "bg-amber-500"
        : "bg-primary";

  return (
    <Tooltip>
      <TooltipTrigger
        className="block w-full text-left"
        render={<button type="button" />}
      >
        <div className="space-y-3">
          <div className="flex items-baseline justify-between gap-3">
            <div>
              <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
                Context window
              </p>
              <p className="mt-1 text-sm font-medium">
                {Math.round(ratio * 100)}% of {new Intl.NumberFormat().format(contextWindow)} tokens
              </p>
            </div>
            <p className="text-xs text-muted-foreground">
              {new Intl.NumberFormat().format(totalTokens)} used
            </p>
          </div>

          <div className="grid grid-cols-12 gap-1">
            {Array.from({ length: SEGMENT_COUNT }, (_, index) => (
              <div
                className={cn(
                  "h-2 rounded-full bg-muted/70 transition-colors",
                  index < activeSegments && toneClass,
                )}
                key={index}
              />
            ))}
          </div>
        </div>
      </TooltipTrigger>
      <TooltipContent>
        {new Intl.NumberFormat().format(totalTokens)} /{" "}
        {new Intl.NumberFormat().format(contextWindow)} total tokens
      </TooltipContent>
    </Tooltip>
  );
}
