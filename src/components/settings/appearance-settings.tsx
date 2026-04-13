import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import type { SettingsSectionProps } from "@/components/settings/settings-types";

/**
 * Appearance controls are intentionally narrow until theme persistence lands in the backend.
 */
export function AppearanceSettings(_: SettingsSectionProps) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Appearance</CardTitle>
        <CardDescription>
          Desktop theming is currently fixed to the dark shell while the app surface stabilizes.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4 text-sm text-muted-foreground">
        <div className="rounded-xl border border-border bg-muted/20 p-4">
          Theme persistence and density controls are intentionally deferred until the message and
          memory surfaces stop shifting.
        </div>

        <div className="rounded-xl border border-dashed border-border p-4">
          The current build always boots into the shared dark theme tokens from shadcn/base-ui.
        </div>
      </CardContent>
    </Card>
  );
}
