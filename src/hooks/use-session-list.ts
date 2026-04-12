import { useQuery } from "@tanstack/react-query";

import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";

export function useSessionList() {
  return useQuery({
    queryKey: queryKeys.sessions(),
    queryFn: tauriClient.listSessionPreviews,
    staleTime: 5_000,
    refetchInterval: 5_000,
  });
}
