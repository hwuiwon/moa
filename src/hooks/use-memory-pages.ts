import { useQuery } from "@tanstack/react-query";

import type {
  MemorySearchResultDto,
  PageSummaryDto,
  WikiPageDto,
} from "@/lib/bindings";
import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";

/**
 * Lists memory pages for the active workspace.
 */
export function useMemoryPages(filter?: string | null) {
  return useQuery<PageSummaryDto[]>({
    queryKey: queryKeys.memoryPages(filter),
    queryFn: () => tauriClient.listMemoryPages(filter),
  });
}

/**
 * Loads one memory page by logical path.
 */
export function useMemoryPage(path: string | null) {
  return useQuery<WikiPageDto>({
    queryKey: queryKeys.memoryPage(path),
    queryFn: () => tauriClient.readMemoryPage(path!),
    enabled: Boolean(path),
  });
}

/**
 * Searches memory pages in the active workspace.
 */
export function useMemorySearch(query: string) {
  return useQuery<MemorySearchResultDto[]>({
    queryKey: queryKeys.memorySearch(query),
    queryFn: () => tauriClient.searchMemory(query, 20),
    enabled: query.trim().length >= 2,
  });
}
