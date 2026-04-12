/**
 * Central React Query key factory for the desktop app.
 */
export const queryKeys = {
  config: () => ["config"] as const,
  modelOptions: () => ["model-options"] as const,
  runtimeInfo: () => ["runtime-info"] as const,
  session: (sessionId: string | null | undefined) =>
    ["session", sessionId ?? null] as const,
  sessionEvents: (sessionId: string | null | undefined) =>
    ["session-events", sessionId ?? null] as const,
  sessionHistory: (sessionId: string | null | undefined) =>
    ["session-history", sessionId ?? null] as const,
  sessions: () => ["sessions"] as const,
};
