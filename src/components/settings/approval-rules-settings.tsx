import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import type { SettingsSectionProps } from "@/components/settings/settings-types";

/**
 * Placeholder surface for approval-rule editing until the desktop backend exposes rule mutation.
 */
export function ApprovalRulesSettings({ config }: SettingsSectionProps) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Approval rules</CardTitle>
        <CardDescription>
          Approval policies still live in the config file and CLI. The desktop app shows the
          current runtime posture so the user can verify what is active.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4 text-sm text-muted-foreground">
        <div className="rounded-xl border border-border bg-muted/20 p-4">
          <p className="font-medium text-foreground">Current defaults</p>
          <ul className="mt-2 space-y-1 leading-6">
            <li>Provider-native web search: {config.webSearchEnabled ? "enabled" : "disabled"}</li>
            <li>Daemon auto-connect: {config.daemonAutoConnect ? "enabled" : "disabled"}</li>
            <li>
              Observability export: {config.observabilityEnabled ? "enabled" : "disabled"}
            </li>
          </ul>
        </div>

        <div className="rounded-xl border border-dashed border-border p-4">
          Desktop rule editing is intentionally blocked until the approval-rule model is exposed as
          structured DTOs. Use `moa config set` or edit the TOML directly for now.
        </div>
      </CardContent>
    </Card>
  );
}
