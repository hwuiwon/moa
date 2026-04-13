import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { arktypeResolver } from "@hookform/resolvers/arktype";
import { type as arktype } from "arktype";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import type { SettingsSectionProps } from "@/components/settings/settings-types";

const memorySettingsSchema = arktype({
  memoryDir: "string > 0",
  sandboxDir: "string > 0",
});

type MemorySettingsValues = typeof memorySettingsSchema.infer;

/**
 * Editable local filesystem settings for memory and sandbox roots.
 */
export function MemorySettings({
  config,
  isSaving,
  onSave,
}: SettingsSectionProps) {
  const form = useForm<MemorySettingsValues>({
    defaultValues: valuesFromConfig(config),
    resolver: arktypeResolver(memorySettingsSchema),
  });

  useEffect(() => {
    form.reset(valuesFromConfig(config));
  }, [config, form]);

  const handleSubmit = form.handleSubmit(async (values) => {
    await onSave({
      memoryDir: values.memoryDir.trim(),
      sandboxDir: values.sandboxDir.trim(),
    });
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Memory</CardTitle>
        <CardDescription>
          Control where the desktop runtime stores workspace memory and sandbox state on disk.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="space-y-5" onSubmit={(event) => void handleSubmit(event)}>
          <FieldSet>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="memory-dir">Memory directory</FieldLabel>
                <FieldContent>
                  <Input id="memory-dir" {...form.register("memoryDir")} />
                  <FieldDescription>
                    Root directory for workspace wiki pages, indexes, and ingest artifacts.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.memoryDir]} />
                </FieldContent>
              </Field>

              <Field>
                <FieldLabel htmlFor="sandbox-dir">Sandbox directory</FieldLabel>
                <FieldContent>
                  <Input id="sandbox-dir" {...form.register("sandboxDir")} />
                  <FieldDescription>
                    Base path used for local execution sandboxes and scratch workspaces.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.sandboxDir]} />
                </FieldContent>
              </Field>
            </FieldGroup>
          </FieldSet>

          <div className="flex justify-end">
            <Button disabled={isSaving} type="submit">
              {isSaving ? "Saving…" : "Save memory settings"}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

function valuesFromConfig(config: SettingsSectionProps["config"]): MemorySettingsValues {
  return {
    memoryDir: config.memoryDir,
    sandboxDir: config.sandboxDir,
  };
}
