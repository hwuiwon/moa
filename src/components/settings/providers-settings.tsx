import { useEffect, useMemo } from "react";
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
import type { SettingsSectionProps } from "@/components/settings/settings-types";

const providersSettingsSchema = arktype({
  defaultModel: "string > 0",
  defaultProvider: "'openai' | 'anthropic' | 'google'",
});

type ProvidersSettingsValues = typeof providersSettingsSchema.infer;

/**
 * Editable provider and default-model settings form.
 */
export function ProvidersSettings({
  config,
  isSaving,
  modelOptions,
  onSave,
}: SettingsSectionProps) {
  const form = useForm<ProvidersSettingsValues>({
    defaultValues: valuesFromConfig(config),
    resolver: arktypeResolver(providersSettingsSchema),
  });

  const selectedProvider = form.watch("defaultProvider");
  const availableModels = useMemo(
    () => modelOptions.filter((option) => option.provider === selectedProvider),
    [modelOptions, selectedProvider],
  );

  useEffect(() => {
    form.reset(valuesFromConfig(config));
  }, [config, form]);

  useEffect(() => {
    const currentModel = form.getValues("defaultModel");
    if (availableModels.some((option) => option.value === currentModel)) {
      return;
    }

    const nextModel = availableModels[0]?.value;
    if (nextModel) {
      form.setValue("defaultModel", nextModel, { shouldValidate: true });
    }
  }, [availableModels, form]);

  const handleSubmit = form.handleSubmit(async (values) => {
    await onSave({
      defaultModel: values.defaultModel,
      defaultProvider: values.defaultProvider,
    });
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Providers</CardTitle>
        <CardDescription>
          Select the default provider and model used for new sessions.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="space-y-5" onSubmit={(event) => void handleSubmit(event)}>
          <FieldSet>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="default-provider">Default provider</FieldLabel>
                <FieldContent>
                  <Controller
                    control={form.control}
                    name="defaultProvider"
                    render={({ field }) => (
                      <Select onValueChange={field.onChange} value={field.value}>
                        <SelectTrigger id="default-provider">
                          <SelectValue placeholder="Select provider" />
                        </SelectTrigger>
                        <SelectContent>
                          <SelectItem value="openai">OpenAI</SelectItem>
                          <SelectItem value="anthropic">Anthropic</SelectItem>
                          <SelectItem value="google">Google</SelectItem>
                        </SelectContent>
                      </Select>
                    )}
                  />
                <FieldDescription>
                    Determines which provider is used for new sessions.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.defaultProvider]} />
                </FieldContent>
              </Field>

              <Field>
                <FieldLabel htmlFor="default-model">Default model</FieldLabel>
                <FieldContent>
                  <Controller
                    control={form.control}
                    name="defaultModel"
                    render={({ field }) => (
                      <Select onValueChange={field.onChange} value={field.value}>
                        <SelectTrigger id="default-model">
                          <SelectValue placeholder="Select model" />
                        </SelectTrigger>
                        <SelectContent>
                          {availableModels.map((option) => (
                            <SelectItem key={option.value} value={option.value}>
                              {option.label}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    )}
                  />
                  <FieldDescription>
                    The model choice is validated by the backend before the config is saved.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.defaultModel]} />
                </FieldContent>
              </Field>
            </FieldGroup>
          </FieldSet>

          <div className="flex justify-end">
            <Button disabled={isSaving} type="submit">
              {isSaving ? "Saving…" : "Save provider settings"}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

function valuesFromConfig(config: SettingsSectionProps["config"]): ProvidersSettingsValues {
  const defaultProvider =
    config.defaultProvider === "anthropic" || config.defaultProvider === "google"
      ? config.defaultProvider
      : "openai";

  return {
    defaultModel: config.defaultModel,
    defaultProvider,
  };
}
