import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { CheckCircle2, Loader2, Search, SlidersHorizontal } from "lucide-react";

import { AdvancedSettings } from "@/components/settings/advanced-settings";
import { AppearanceSettings } from "@/components/settings/appearance-settings";
import { ApprovalRulesSettings } from "@/components/settings/approval-rules-settings";
import { GeneralSettings } from "@/components/settings/general-settings";
import { MemorySettings } from "@/components/settings/memory-settings";
import { ProvidersSettings } from "@/components/settings/providers-settings";
import { ToolsAndMcpSettings } from "@/components/settings/tools-and-mcp-settings";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";
import { cn } from "@/lib/utils";
import { useConfig, useUpdateConfig } from "@/hooks/use-config";
import type { MoaConfigDto } from "@/lib/bindings";

type SettingsCategory =
  | "general"
  | "providers"
  | "tools-mcp"
  | "approval-rules"
  | "memory"
  | "appearance"
  | "advanced";

const settingsCategories: Array<{
  id: SettingsCategory;
  label: string;
  description: string;
}> = [
  {
    id: "general",
    label: "General",
    description: "Reasoning defaults and instruction layers.",
  },
  {
    id: "providers",
    label: "Providers",
    description: "Default provider and model selection.",
  },
  {
    id: "tools-mcp",
    label: "Tools & MCP",
    description: "Provider-native web search and tool surface notes.",
  },
  {
    id: "approval-rules",
    label: "Approval Rules",
    description: "Current runtime posture and policy notes.",
  },
  {
    id: "memory",
    label: "Memory",
    description: "Filesystem roots for memory and local sandboxes.",
  },
  {
    id: "appearance",
    label: "Appearance",
    description: "Theme and density controls.",
  },
  {
    id: "advanced",
    label: "Advanced",
    description: "Daemon attachment and observability flags.",
  },
];

/**
 * Searchable settings surface backed by Rust DTOs and ArkType-validated forms.
 */
export function SettingsView() {
  const [activeCategory, setActiveCategory] = useState<SettingsCategory>("general");
  const [searchQuery, setSearchQuery] = useState("");
  const configQuery = useConfig();
  const modelOptionsQuery = useQuery({
    queryKey: queryKeys.modelOptions(),
    queryFn: tauriClient.listModelOptions,
    staleTime: 5_000,
  });
  const updateConfig = useUpdateConfig();

  const activeCategoryMeta = useMemo(
    () => settingsCategories.find((category) => category.id === activeCategory) ?? settingsCategories[0],
    [activeCategory],
  );
  const normalizedSearch = searchQuery.trim().toLowerCase();
  const visibleCategories = useMemo(() => {
    if (!normalizedSearch) {
      return settingsCategories;
    }

    return settingsCategories.filter((category) =>
      `${category.label} ${category.description}`.toLowerCase().includes(normalizedSearch),
    );
  }, [normalizedSearch]);

  const handleSave = async (patch: Partial<MoaConfigDto>) => {
    const currentConfig = configQuery.data;
    if (!currentConfig) {
      return;
    }

    await updateConfig.mutateAsync({
      ...currentConfig,
      ...patch,
    });
  };

  if (configQuery.isLoading || modelOptionsQuery.isLoading) {
    return (
      <div className="flex h-full items-center justify-center px-8">
        <Card className="w-full max-w-xl">
          <CardContent className="flex items-center gap-3 py-8 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading desktop settings…
          </CardContent>
        </Card>
      </div>
    );
  }

  if (!configQuery.data) {
    return (
      <div className="flex h-full items-center justify-center px-8">
        <Card className="w-full max-w-xl">
          <CardContent className="py-8 text-center">
            <SlidersHorizontal className="mx-auto h-10 w-10 text-muted-foreground" />
            <h2 className="mt-4 text-lg font-semibold">Settings unavailable</h2>
            <p className="mt-2 text-sm text-muted-foreground">
              The desktop runtime did not return a config snapshot.
            </p>
          </CardContent>
        </Card>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 bg-background">
      <nav className="flex w-[200px] shrink-0 flex-col border-r border-border p-4">
        <div className="mb-4">
          <p className="text-[11px] uppercase tracking-[0.24em] text-muted-foreground">
            Settings
          </p>
          <h1 className="mt-2 text-lg font-semibold">Desktop runtime</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            Rust-backed configuration with validation before persist.
          </p>
        </div>

        <div className="relative mb-4">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            className="pl-8"
            onChange={(event) => setSearchQuery(event.target.value)}
            placeholder="Search settings"
            value={searchQuery}
          />
        </div>

        <div className="space-y-1">
          {visibleCategories.map((category) => (
            <button
              key={category.id}
              className={cn(
                "flex w-full flex-col rounded-xl px-3 py-2 text-left transition",
                activeCategory === category.id
                  ? "bg-accent text-accent-foreground"
                  : "hover:bg-accent/50",
              )}
              onClick={() => setActiveCategory(category.id)}
              type="button"
            >
              <span className="text-sm font-medium">{category.label}</span>
              <span className="mt-0.5 text-xs leading-5 text-muted-foreground">
                {category.description}
              </span>
            </button>
          ))}
        </div>
      </nav>

      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-5xl flex-col gap-6 px-6 py-6">
          <div className="flex items-center justify-between gap-3">
            <div>
              <p className="text-[11px] uppercase tracking-[0.24em] text-muted-foreground">
                {normalizedSearch ? "Search results" : activeCategoryMeta?.label}
              </p>
              <h2 className="mt-2 text-2xl font-semibold">
                {normalizedSearch ? "Filtered settings" : activeCategoryMeta?.label}
              </h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {normalizedSearch
                  ? `Showing settings matching “${searchQuery}”.`
                  : activeCategoryMeta?.description}
              </p>
            </div>

            {updateConfig.isSuccess && !updateConfig.isPending ? (
              <Badge className="gap-1.5" variant="secondary">
                <CheckCircle2 className="h-3.5 w-3.5" />
                Saved
              </Badge>
            ) : null}
          </div>

          {updateConfig.isError ? (
            <Card className="border-destructive/30 bg-destructive/10">
              <CardContent className="py-4 text-sm text-destructive">
                {updateConfig.error instanceof Error
                  ? updateConfig.error.message
                  : "Failed to save configuration."}
              </CardContent>
            </Card>
          ) : null}

          {visibleCategories.length === 0 ? (
            <Card>
              <CardContent className="py-8 text-sm text-muted-foreground">
                No settings matched “{searchQuery}”.
              </CardContent>
            </Card>
          ) : normalizedSearch ? (
            visibleCategories.map((category) => (
              <div key={category.id} className="space-y-3">
                <div>
                  <p className="text-[11px] uppercase tracking-[0.24em] text-muted-foreground">
                    {category.label}
                  </p>
                </div>
                {renderSettingsSection(category.id, {
                  config: configQuery.data,
                  isSaving: updateConfig.isPending,
                  modelOptions: modelOptionsQuery.data ?? [],
                  onSave: handleSave,
                })}
              </div>
            ))
          ) : (
            renderSettingsSection(activeCategory, {
              config: configQuery.data,
              isSaving: updateConfig.isPending,
              modelOptions: modelOptionsQuery.data ?? [],
              onSave: handleSave,
            })
          )}
        </div>
      </div>
    </div>
  );
}

function renderSettingsSection(
  category: SettingsCategory,
  props: Parameters<typeof GeneralSettings>[0],
) {
  switch (category) {
    case "general":
      return <GeneralSettings {...props} />;
    case "providers":
      return <ProvidersSettings {...props} />;
    case "tools-mcp":
      return <ToolsAndMcpSettings {...props} />;
    case "approval-rules":
      return <ApprovalRulesSettings {...props} />;
    case "memory":
      return <MemorySettings {...props} />;
    case "appearance":
      return <AppearanceSettings {...props} />;
    case "advanced":
      return <AdvancedSettings {...props} />;
    default:
      return null;
  }
}
