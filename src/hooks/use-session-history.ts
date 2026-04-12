import { useQuery } from "@tanstack/react-query";

import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";
import { eventsToMessages } from "@/types/chat";

/**
 * Loads and transforms a session event log into transcript messages.
 */
export function useSessionHistory(sessionId: string | undefined) {
  return useQuery({
    enabled: Boolean(sessionId),
    queryKey: queryKeys.sessionHistory(sessionId),
    queryFn: async () => {
      const events = await tauriClient.getSessionEvents(sessionId!);
      return eventsToMessages(events);
    },
    staleTime: 1_000,
  });
}
