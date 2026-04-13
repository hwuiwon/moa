import { FilePlus2, Search } from "lucide-react";

import type { MemorySearchResultDto } from "@/lib/bindings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { formatRelativeTime } from "@/lib/utils";

type MemorySearchProps = {
  query: string;
  results: MemorySearchResultDto[];
  isLoading: boolean;
  onCreatePage: () => void;
  onQueryChange: (value: string) => void;
  onSelectPath: (path: string) => void;
};

/**
 * Memory search bar and result list for the wiki browser.
 */
export function MemorySearch({
  query,
  results,
  isLoading,
  onCreatePage,
  onQueryChange,
  onSelectPath,
}: MemorySearchProps) {
  const isSearching = query.trim().length >= 2;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="space-y-3 border-b border-border px-3 py-3">
        <Button className="h-8 w-full justify-center text-xs" onClick={onCreatePage} type="button">
          <FilePlus2 className="h-3.5 w-3.5" />
          New Page
        </Button>

        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            className="h-8 pl-8 text-xs"
            onChange={(event) => onQueryChange(event.target.value)}
            placeholder="Search memory"
            value={query}
          />
        </div>
      </div>

      {isSearching ? (
        <ScrollArea className="min-h-0 flex-1">
          <div className="space-y-1 px-2 py-3">
            {isLoading ? (
              <div className="px-2 py-6 text-sm text-muted-foreground">Searching memory…</div>
            ) : results.length ? (
              results.map((result) => (
                <button
                  className="w-full rounded-lg border border-transparent px-3 py-2 text-left transition hover:border-border hover:bg-accent/40"
                  key={`${result.scope}:${result.path}`}
                  onClick={() => onSelectPath(result.path)}
                  type="button"
                >
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-sm font-medium">{result.title}</p>
                      <p className="mt-1 line-clamp-2 text-xs leading-5 text-muted-foreground">
                        {result.snippet}
                      </p>
                    </div>
                    <span className="shrink-0 text-[11px] uppercase tracking-wide text-muted-foreground">
                      {result.pageType}
                    </span>
                  </div>
                  <div className="mt-2 flex items-center justify-between gap-3 text-[11px] text-muted-foreground">
                    <span className="truncate">{result.path}</span>
                    <time dateTime={result.updated}>{formatRelativeTime(result.updated)}</time>
                  </div>
                </button>
              ))
            ) : (
              <div className="px-2 py-6 text-sm text-muted-foreground">
                No memory pages matched “{query.trim()}”.
              </div>
            )}
          </div>
        </ScrollArea>
      ) : null}
    </div>
  );
}
