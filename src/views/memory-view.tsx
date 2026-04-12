import { BookMarked } from "lucide-react";

export function MemoryView() {
  return (
    <div className="flex h-full items-center justify-center px-8">
      <div className="max-w-md text-center">
        <BookMarked className="mx-auto h-10 w-10 text-muted-foreground" />
        <h2 className="mt-4 text-lg font-semibold">Memory</h2>
        <p className="mt-2 text-sm text-muted-foreground">
          Memory browsing and editing will plug into this route. The shell and
          navigation are already wired.
        </p>
      </div>
    </div>
  );
}
