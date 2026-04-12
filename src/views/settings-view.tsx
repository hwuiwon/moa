import { SlidersHorizontal } from "lucide-react";

export function SettingsView() {
  return (
    <div className="flex h-full items-center justify-center px-8">
      <div className="max-w-md text-center">
        <SlidersHorizontal className="mx-auto h-10 w-10 text-muted-foreground" />
        <h2 className="mt-4 text-lg font-semibold">Settings</h2>
        <p className="mt-2 text-sm text-muted-foreground">
          Settings editing lands here. The route, layout, and top-bar navigation
          are ready.
        </p>
      </div>
    </div>
  );
}
