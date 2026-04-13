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
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import type { SettingsSectionProps } from "@/components/settings/settings-types";

const advancedSettingsSchema = arktype({
  daemonAutoConnect: "boolean",
  environment: "string",
  observabilityEnabled: "boolean",
});

type AdvancedSettingsValues = typeof advancedSettingsSchema.infer;

/**
 * Advanced runtime flags for daemon attachment and observability export.
 */
export function AdvancedSettings({
  config,
  isSaving,
  onSave,
}: SettingsSectionProps) {
  const form = useForm<AdvancedSettingsValues>({
    defaultValues: valuesFromConfig(config),
    resolver: arktypeResolver(advancedSettingsSchema),
  });

  useEffect(() => {
    form.reset(valuesFromConfig(config));
  }, [config, form]);

  const handleSubmit = form.handleSubmit(async (values) => {
    await onSave({
      daemonAutoConnect: values.daemonAutoConnect,
      environment: values.environment.trim() || null,
      observabilityEnabled: values.observabilityEnabled,
    });
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Advanced</CardTitle>
        <CardDescription>
          Tune daemon attachment and observability export behavior for the desktop runtime.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form className="space-y-5" onSubmit={(event) => void handleSubmit(event)}>
          <FieldSet>
            <FieldGroup>
              <Field orientation="horizontal">
                <FieldLabel htmlFor="daemon-auto-connect">Daemon auto-connect</FieldLabel>
                <FieldContent>
                  <Controller
                    control={form.control}
                    name="daemonAutoConnect"
                    render={({ field }) => (
                      <Switch
                        checked={field.value}
                        id="daemon-auto-connect"
                        onCheckedChange={field.onChange}
                      />
                    )}
                  />
                  <FieldDescription>
                    Reconnect to the daemon transport automatically when it is available.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.daemonAutoConnect]} />
                </FieldContent>
              </Field>

              <Field orientation="horizontal">
                <FieldLabel htmlFor="observability-enabled">Observability export</FieldLabel>
                <FieldContent>
                  <Controller
                    control={form.control}
                    name="observabilityEnabled"
                    render={({ field }) => (
                      <Switch
                        checked={field.value}
                        id="observability-enabled"
                        onCheckedChange={field.onChange}
                      />
                    )}
                  />
                  <FieldDescription>
                    Emit runtime traces and metrics when the configured backend is available.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.observabilityEnabled]} />
                </FieldContent>
              </Field>

              <Field>
                <FieldLabel htmlFor="observability-environment">Environment label</FieldLabel>
                <FieldContent>
                  <Input
                    id="observability-environment"
                    placeholder="development"
                    {...form.register("environment")}
                  />
                  <FieldDescription>
                    Optional environment tag attached to exported traces and metrics.
                  </FieldDescription>
                  <FieldError errors={[form.formState.errors.environment]} />
                </FieldContent>
              </Field>
            </FieldGroup>
          </FieldSet>

          <div className="flex justify-end">
            <Button disabled={isSaving} type="submit">
              {isSaving ? "Saving…" : "Save advanced settings"}
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  );
}

function valuesFromConfig(config: SettingsSectionProps["config"]): AdvancedSettingsValues {
  return {
    daemonAutoConnect: config.daemonAutoConnect,
    environment: config.environment ?? "",
    observabilityEnabled: config.observabilityEnabled,
  };
}
