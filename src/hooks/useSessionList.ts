import { useQuery } from "@tanstack/react-query";

import { tauriClient } from "@/lib/tauri";

export function useSessionList() {
  return useQuery({
    queryKey: ["sessions"],
    queryFn: tauriClient.listSessionPreviews,
    staleTime: 5_000,
    refetchInterval: 5_000,
  });
}
