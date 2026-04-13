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
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldSet,
} from "@/components/ui/field";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import type { SettingsSectionProps } from "@/components/settings/settings-types";

const generalSettingsSchema = arktype({
  reasoningEffort: "'low' | 'medium' | 'high'",
  userInstructions: "string",
  workspaceInstructions: "string",
});

type GeneralSettingsValues = typeof generalSettingsSchema.infer;

/**
 * Editable general settings form with ArkType validation.
 */
export function GeneralSettings({
  config,
  isSaving,
  onSave,
}: SettingsSectionProps) {
  const form = useForm<GeneralSettingsValues>({
    defaultValues: valuesFromConfig(config),
    resolver: arktypeResolver(generalSettingsSchema),
  });

  useEffect(() => {
    form.reset(valuesFromConfig(config));
  }, [config, form]);

  const handleSubmit = form.handleSubmit(async (values) => {
    await onSave({
      reasoningEffort: values.reasoningEffort,
      userInstructions: values.userInstructions.trim() || null,
      workspaceInstructions: values.workspaceInstructions.trim() || null,
    });
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>General</CardTitle>
        <CardDescription>
          Default reasoning behavior and instruction layers for new turns.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="space-y-5" onSubmit={(event) => void handleSubmit(event)}>
          <FieldSet>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="reasoning-effort">Reasoning effort</FieldLabel>
                <FieldContent>
                  <Controller
                    control={form.control}
                    name="reasoningEffort"
                    render={({ field }) => (
                      <Select onValueChange={field.onChange} value={field.value}>
                        <SelectTrigger id="reasoning-effort">
                          <SelectValue placeholder="Select reasoning effort" />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="low">Low</SelectItem>
                          <SelectItem value="medium">Medium</SelectItem>
                          <SelectItem value="high">High</SelectItem>
                        </SelectContent>
                      </Select>
                    )}
                  />
                  <FieldDescription>
                    Controls the default reasoning budget for compatible models.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.reasoningEffort]} />
                </FieldContent>
              </Field>

              <Field>
                <FieldLabel htmlFor="workspace-instructions">Workspace instructions</FieldLabel>
                <FieldContent>
                  <Textarea
                    id="workspace-instructions"
                    placeholder="Add shared instructions for this workspace"
                    rows={5}
                    {...form.register("workspaceInstructions")}
                  />
                  <FieldDescription>
                    Injected into prompts before user turns for this desktop workspace.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.workspaceInstructions]} />
                </FieldContent>
              </Field>

              <Field>
                <FieldLabel htmlFor="user-instructions">User instructions</FieldLabel>
                <FieldContent>
                  <Textarea
                    id="user-instructions"
                    placeholder="Add personal preferences for the assistant"
                    rows={5}
                    {...form.register("userInstructions")}
                  />
                  <FieldDescription>
                    Personal defaults layered on top of workspace instructions.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.userInstructions]} />
                </FieldContent>
              </Field>
            </FieldGroup>
          </FieldSet>

          <div className="flex justify-end">
            <Button disabled={isSaving} type="submit">
              {isSaving ? "Saving…" : "Save general settings"}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

function valuesFromConfig(config: SettingsSectionProps["config"]): GeneralSettingsValues {
  return {
    reasoningEffort:
      config.reasoningEffort === "low" || config.reasoningEffort === "high"
        ? config.reasoningEffort
        : "medium",
    userInstructions: config.userInstructions ?? "",
    workspaceInstructions: config.workspaceInstructions ?? "",
  };
}
