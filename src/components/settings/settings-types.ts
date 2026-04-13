import type { MoaConfigDto, ModelOptionDto } from "@/lib/bindings";

export type SettingsSaveHandler = (patch: Partial<MoaConfigDto>) => Promise<void>;

export type SettingsSectionProps = {
  config: MoaConfigDto;
  isSaving: boolean;
  modelOptions: ModelOptionDto[];
  onSave: SettingsSaveHandler;
};
