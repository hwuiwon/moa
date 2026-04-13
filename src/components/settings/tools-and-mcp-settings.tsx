import { useEffect } from "react";
import { Controller, useForm } from "react-hook-form";
import { arktypeResolver } from "@hookform/resolvers/arktype";
import { type as arktype } from "arktype";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldSet,
} from "@/components/ui/field";
import { Switch } from "@/components/ui/switch";
import type { SettingsSectionProps } from "@/components/settings/settings-types";

const toolsSettingsSchema = arktype({
  webSearchEnabled: "boolean",
});

type ToolsSettingsValues = typeof toolsSettingsSchema.infer;

/**
 * Tool and MCP settings exposed in the current desktop build.
 */
export function ToolsAndMcpSettings({
  config,
  isSaving,
  onSave,
}: SettingsSectionProps) {
  const form = useForm<ToolsSettingsValues>({
    defaultValues: {
      webSearchEnabled: config.webSearchEnabled,
    },
    resolver: arktypeResolver(toolsSettingsSchema),
  });

  useEffect(() => {
    form.reset({
      webSearchEnabled: config.webSearchEnabled,
    });
  }, [config, form]);

  const handleSubmit = form.handleSubmit(async (values) => {
    await onSave({
      webSearchEnabled: values.webSearchEnabled,
    });
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Tools &amp; MCP</CardTitle>
        <CardDescription>
          Control provider-native web search and inspect the current desktop surface area.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="space-y-5" onSubmit={(event) => void handleSubmit(event)}>
          <FieldSet>
            <FieldGroup>
              <Field orientation="horizontal">
                <FieldLabel htmlFor="web-search-enabled">
                  Provider-native web search
                </FieldLabel>
                <FieldContent>
                  <Controller
                    control={form.control}
                    name="webSearchEnabled"
                    render={({ field }) => (
                      <Switch
                        checked={field.value}
                        id="web-search-enabled"
                        onCheckedChange={field.onChange}
                      />
                    )}
                  />
                  <FieldDescription>
                    Allows supported providers to use built-in web search without routing through
                    MOA tools.
                  </FieldDescription>
                </FieldContent>
              </Field>
            </FieldGroup>
          </FieldSet>

          <div className="rounded-xl border border-dashed border-border p-4 text-sm text-muted-foreground">
            MCP server editing is not exposed in the desktop UI yet. Use the config file or CLI for
            per-server changes.
          </div>

          <div className="flex justify-end">
            <Button disabled={isSaving} type="submit">
              {isSaving ? "Saving…" : "Save tool settings"}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}
