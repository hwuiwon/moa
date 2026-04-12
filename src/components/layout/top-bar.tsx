import { Link } from "@tanstack/react-router";
import { Menu, PanelRightOpen, Plus, Settings2 } from "lucide-react";

import { Button, buttonVariants } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import type { ModelOptionDto, RuntimeInfoDto } from "@/lib/types";
import { cn } from "@/lib/utils";
import type { ActiveView } from "@/stores/layout";

type TopBarProps = {
  activeView: ActiveView;
  activeSessionId: string | null;
  hasActiveSession: boolean;
  runtimeInfo?: RuntimeInfoDto;
  modelOptions: ModelOptionDto[];
  modelPending: boolean;
  onCreateSession: () => void;
  onModelChange: (value: string) => void;
  onToggleDetailPanel: () => void;
  onToggleSidebar: () => void;
};

const navItems: Array<{ href: string; label: string; view: ActiveView }> = [
  { href: "/chat", label: "Chat", view: "chat" },
  { href: "/memory", label: "Memory", view: "memory" },
];

export function TopBar({
  activeView,
  activeSessionId,
  hasActiveSession,
  runtimeInfo,
  modelOptions,
  modelPending,
  onCreateSession,
  onModelChange,
  onToggleDetailPanel,
  onToggleSidebar,
}: TopBarProps) {
  return (
    <header className="bg-background px-4 py-2.5">
      <div className="flex items-center gap-3">
        <Button onClick={onToggleSidebar} size="icon" type="button" variant="ghost">
          <Menu className="h-4 w-4" />
        </Button>

        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h1 className="text-sm font-semibold">MOA</h1>
            <span className="truncate text-xs text-muted-foreground">
              workspace: {runtimeInfo?.workspaceId ?? "loading"}
            </span>
          </div>
        </div>

        <Separator className="mx-1 hidden h-5 md:block" orientation="vertical" />

        <nav className="hidden items-center gap-0.5 md:flex">
          {navItems.map((item) => (
            item.view === "chat" ? (
              hasActiveSession ? (
                <Link
                  className={cn(
                    buttonVariants({ variant: "ghost", size: "sm" }),
                    activeView === item.view && "bg-accent text-accent-foreground",
                  )}
                  key={item.label}
                  params={{ sessionId: activeSessionId! }}
                  to="/chat/$sessionId"
                >
                  {item.label}
                </Link>
              ) : (
                <Link
                  className={cn(
                    buttonVariants({ variant: "ghost", size: "sm" }),
                    activeView === item.view && "bg-accent text-accent-foreground",
                  )}
                  key={item.label}
                  to="/chat"
                >
                  {item.label}
                </Link>
              )
            ) : (
              <Link
                className={cn(
                  buttonVariants({ variant: "ghost", size: "sm" }),
                  activeView === item.view && "bg-accent text-accent-foreground",
                )}
                key={item.label}
                to={item.href}
              >
                {item.label}
              </Link>
            )
          ))}
        </nav>

        <div className="ml-auto flex items-center gap-1.5">
          <div className="hidden min-w-[180px] md:block">
            <Select
              disabled={modelPending}
              onValueChange={(value) => {
                if (value) {
                  onModelChange(value);
                }
              }}
              value={runtimeInfo?.model}
            >
              <SelectTrigger className="h-8 text-xs">
                <SelectValue placeholder="Select model" />
              </SelectTrigger>
              <SelectContent>
                {modelOptions.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <Button className="h-8 text-xs" onClick={onCreateSession} type="button" variant="secondary">
            <Plus className="h-3.5 w-3.5" />
            New Session
          </Button>

          <Button onClick={onToggleDetailPanel} size="icon" type="button" variant="ghost">
            <PanelRightOpen className="h-4 w-4" />
          </Button>

          <Link
            aria-label="Open settings"
            className={buttonVariants({ variant: "ghost", size: "icon" })}
            to="/settings"
          >
            <Settings2 className="h-4 w-4" />
          </Link>
        </div>
      </div>
    </header>
  );
}
