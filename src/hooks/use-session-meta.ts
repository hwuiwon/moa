import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import type { EventRecordDto, ModelOptionDto, SessionMetaDto } from "@/lib/bindings";
import { queryKeys } from "@/lib/query-keys";
import { tauriClient } from "@/lib/tauri";

type ToolStatus = "pending" | "running" | "done" | "error";

export type ToolUsageSummary = {
  toolName: string;
  totalCalls: number;
  successes: number;
  failures: number;
  avgDurationMs: number | null;
  lastStatus: ToolStatus;
  lastUpdatedAt: string | null;
};

export type SessionMetaSummary = {
  meta: SessionMetaDto;
  turnCount: number;
  totalTokens: number;
  durationMs: number;
  contextWindow: number | null;
  contextUsagePercent: number;
  toolsUsed: ToolUsageSummary[];
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function eventData(record: EventRecordDto): Record<string, unknown> {
  if (isRecord(record.payload) && isRecord(record.payload.data)) {
    return record.payload.data;
  }

  return {};
}

function stringField(data: Record<string, unknown>, key: string) {
  const value = data[key];
  return typeof value === "string" ? value : null;
}

function numberField(data: Record<string, unknown>, key: string) {
  const value = data[key];
  return typeof value === "number" ? value : null;
}

function booleanField(data: Record<string, unknown>, key: string) {
  const value = data[key];
  return typeof value === "boolean" ? value : null;
}

function computeDurationMs(meta: SessionMetaDto) {
  const start = new Date(meta.createdAt).getTime();
  const end = new Date(meta.completedAt ?? meta.updatedAt).getTime();

  if (Number.isNaN(start) || Number.isNaN(end)) {
    return 0;
  }

  return Math.max(0, end - start);
}

function computeToolUsage(events: EventRecordDto[]) {
  const toolNamesById = new Map<string, string>();
  const usage = new Map<string, ToolUsageSummary & { totalDurationMs: number }>();

  const ensureTool = (toolName: string) => {
    const existing = usage.get(toolName);
    if (existing) {
      return existing;
    }

    const created = {
      toolName,
      totalCalls: 0,
      successes: 0,
      failures: 0,
      avgDurationMs: null,
      totalDurationMs: 0,
      lastStatus: "pending" as ToolStatus,
      lastUpdatedAt: null as string | null,
    };
    usage.set(toolName, created);
    return created;
  };

  for (const record of events) {
    const data = eventData(record);
    const eventType = canonicalEventType(record.eventType);

    switch (eventType) {
      case "tool_call": {
        const toolId = stringField(data, "tool_id");
        const toolName = stringField(data, "tool_name") ?? "unknown";
        if (toolId) {
          toolNamesById.set(toolId, toolName);
        }
        const summary = ensureTool(toolName);
        summary.totalCalls += 1;
        summary.lastStatus = "running";
        summary.lastUpdatedAt = record.timestamp;
        break;
      }
      case "tool_result": {
        const toolId = stringField(data, "tool_id");
        const success = booleanField(data, "success") ?? false;
        const durationMs = numberField(data, "duration_ms") ?? 0;
        const toolName = (toolId && toolNamesById.get(toolId)) ?? "unknown";
        const summary = ensureTool(toolName);
        summary.successes += success ? 1 : 0;
        summary.failures += success ? 0 : 1;
        summary.totalDurationMs += durationMs;
        summary.lastStatus = success ? "done" : "error";
        summary.lastUpdatedAt = record.timestamp;
        break;
      }
      case "tool_error": {
        const toolId = stringField(data, "tool_id");
        const toolName =
          stringField(data, "tool_name") ??
          (toolId ? toolNamesById.get(toolId) : null) ??
          "unknown";
        const summary = ensureTool(toolName);
        summary.failures += 1;
        summary.lastStatus = "error";
        summary.lastUpdatedAt = record.timestamp;
        break;
      }
      default:
        break;
    }
  }

  return [...usage.values()]
    .map(({ totalDurationMs, ...summary }) => ({
      ...summary,
      avgDurationMs:
        summary.successes + summary.failures > 0
          ? totalDurationMs / (summary.successes + summary.failures)
          : null,
    }))
    .sort((left, right) => {
      if (right.totalCalls !== left.totalCalls) {
        return right.totalCalls - left.totalCalls;
      }
      return (right.lastUpdatedAt ?? "").localeCompare(left.lastUpdatedAt ?? "");
    });
}

function contextWindowForModel(
  meta: SessionMetaDto,
  modelOptions: ModelOptionDto[] | undefined,
) {
  return (
    modelOptions?.find((option) => option.value === meta.model)?.contextWindow ?? null
  );
}

function canonicalEventType(eventType: string) {
  return eventType.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
}

export function useSessionMeta(sessionId: string | null | undefined) {
  const metaQuery = useQuery({
    enabled: Boolean(sessionId),
    queryKey: queryKeys.session(sessionId),
    queryFn: () => tauriClient.getSession(sessionId!),
    refetchInterval: sessionId ? 1_000 : false,
  });

  const eventsQuery = useQuery({
    enabled: Boolean(sessionId),
    queryKey: queryKeys.sessionEvents(sessionId),
    queryFn: () => tauriClient.getSessionEvents(sessionId!),
    refetchInterval: sessionId ? 1_000 : false,
  });

  const modelOptionsQuery = useQuery({
    queryKey: queryKeys.modelOptions(),
    queryFn: tauriClient.listModelOptions,
    staleTime: 60_000,
  });

  const summary = useMemo<SessionMetaSummary | null>(() => {
    if (!metaQuery.data) {
      return null;
    }

    const totalTokens =
      metaQuery.data.totalInputTokens + metaQuery.data.totalOutputTokens;
    const contextWindow = contextWindowForModel(
      metaQuery.data,
      modelOptionsQuery.data,
    );
    const turnCount =
      eventsQuery.data?.filter(
        (record) => canonicalEventType(record.eventType) === "brain_response",
      )
        .length ?? 0;

    return {
      meta: metaQuery.data,
      turnCount,
      totalTokens,
      durationMs: computeDurationMs(metaQuery.data),
      contextWindow,
      contextUsagePercent: contextWindow
        ? Math.min(100, (totalTokens / contextWindow) * 100)
        : 0,
      toolsUsed: computeToolUsage(eventsQuery.data ?? []),
    };
  }, [eventsQuery.data, metaQuery.data, modelOptionsQuery.data]);

  return {
    ...metaQuery,
    events: eventsQuery.data ?? [],
    isLoading:
      metaQuery.isLoading || eventsQuery.isLoading || modelOptionsQuery.isLoading,
    isError: metaQuery.isError || eventsQuery.isError || modelOptionsQuery.isError,
    summary,
  };
}
