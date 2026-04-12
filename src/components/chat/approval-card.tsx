import { useState } from "react";
import { AlertTriangle, CheckCircle2, ShieldAlert, ShieldCheck } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { tauriClient } from "@/lib/tauri";
import type { ApprovalBlock } from "@/types/chat";
import { cn } from "@/lib/utils";

type ApprovalCardProps = {
  block: ApprovalBlock;
};

/**
 * Inline approval request card with workspace-risk styling and action buttons.
 */
export function ApprovalCard({ block }: ApprovalCardProps) {
  const [decision, setDecision] = useState<string | null>(block.decision ?? null);
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const riskTone = riskToneForLevel(block.riskLevel);

  const submitDecision = async (nextDecision: string) => {
    if (isSubmitting || decision) {
      return;
    }

    setIsSubmitting(true);
    setError(null);
    try {
      await tauriClient.respondToApproval(block.requestId, nextDecision);
      setDecision(nextDecision);
    } catch (approvalError) {
      setError(
        approvalError instanceof Error ? approvalError.message : String(approvalError),
      );
    } finally {
      setIsSubmitting(false);
    }
  };

  return (
    <Card className={cn("border-l-4", riskTone.borderClass)} size="sm">
      <CardHeader className="gap-2">
        <div className="flex items-start justify-between gap-3">
          <div className="space-y-1">
            <CardTitle className="flex items-center gap-2 text-sm">
              {riskTone.icon}
              Approval required
            </CardTitle>
            <CardDescription className="text-sm">
              {block.toolName} wants permission to continue.
            </CardDescription>
          </div>
          <Badge variant={riskTone.badgeVariant}>{riskTone.badgeLabel}</Badge>
        </div>
      </CardHeader>

      <CardContent className="space-y-3 text-sm">
        <div>
          <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
            Input summary
          </p>
          <p className="mt-1 whitespace-pre-wrap leading-6">
            {block.inputSummary || "No input summary provided."}
          </p>
        </div>

        {block.diffPreview ? (
          <div>
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Diff preview
            </p>
            <pre className="mt-1 max-h-60 overflow-auto rounded-lg border border-border bg-muted/30 p-3 text-xs leading-5 whitespace-pre-wrap">
              {block.diffPreview}
            </pre>
          </div>
        ) : null}

        {decision ? (
          <div className="flex items-center gap-2 rounded-lg border border-border bg-muted/30 px-3 py-2 text-sm">
            <CheckCircle2 className="h-4 w-4 text-green-500" />
            <span>Decision recorded: {decisionLabel(decision)}</span>
          </div>
        ) : null}

        {error ? (
          <div className="rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            {error}
          </div>
        ) : null}
      </CardContent>

      {decision ? null : (
        <CardFooter className="flex flex-wrap gap-2">
          <Button
            disabled={isSubmitting}
            onClick={() => void submitDecision("allow_once")}
            size="sm"
            type="button"
          >
            Allow once
          </Button>
          <Button
            disabled={isSubmitting}
            onClick={() => void submitDecision("always_allow")}
            size="sm"
            type="button"
            variant="outline"
          >
            Always allow
          </Button>
          <Button
            disabled={isSubmitting}
            onClick={() => void submitDecision("deny")}
            size="sm"
            type="button"
            variant="destructive"
          >
            Deny
          </Button>
        </CardFooter>
      )}
    </Card>
  );
}

function riskToneForLevel(level: string) {
  switch (level) {
    case "low":
      return {
        badgeLabel: "Safe",
        badgeVariant: "secondary" as const,
        borderClass: "border-l-green-500",
        icon: <ShieldCheck className="h-4 w-4 text-green-500" />,
      };
    case "high":
      return {
        badgeLabel: "Dangerous",
        badgeVariant: "destructive" as const,
        borderClass: "border-l-red-500",
        icon: <ShieldAlert className="h-4 w-4 text-red-500" />,
      };
    default:
      return {
        badgeLabel: "Moderate",
        badgeVariant: "outline" as const,
        borderClass: "border-l-amber-500",
        icon: <AlertTriangle className="h-4 w-4 text-amber-500" />,
      };
  }
}

function decisionLabel(decision: string) {
  switch (decision) {
    case "allow_once":
      return "Allow once";
    case "always_allow":
      return "Always allow";
    case "deny":
      return "Denied";
    default:
      return decision;
  }
}
