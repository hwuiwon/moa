import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import type { MoaConfigDto } from "@/lib/bindings";
import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";

/**
 * Loads the current desktop configuration snapshot.
 */
export function useConfig() {
  return useQuery({
    queryKey: queryKeys.config(),
    queryFn: tauriClient.getConfig,
    staleTime: 5_000,
  });
}

/**
 * Persists a desktop configuration update and invalidates dependent chrome queries.
 */
export function useUpdateConfig() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (config: MoaConfigDto) => tauriClient.updateConfig(config),
    onSuccess: async (config) => {
      queryClient.setQueryData(queryKeys.config(), config);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: queryKeys.config() }),
        queryClient.invalidateQueries({ queryKey: queryKeys.runtimeInfo() }),
        queryClient.invalidateQueries({ queryKey: queryKeys.modelOptions() }),
      ]);
    },
  });
}
