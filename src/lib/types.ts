export type SessionSummaryDto = {
  sessionId: string;
  workspaceId: string;
  userId: string;
  title: string | null;
  status: string;
  platform: string;
  model: string;
  updatedAt: string;
  active: boolean;
};

export type SessionPreviewDto = {
  summary: SessionSummaryDto;
  lastMessage: string | null;
};

export type SessionMetaDto = {
  id: string;
  workspaceId: string;
  userId: string;
  title: string | null;
  status: string;
  platform: string;
  platformChannel: string | null;
  model: string;
  createdAt: string;
  updatedAt: string;
  completedAt: string | null;
  parentSessionId: string | null;
  totalInputTokens: number;
  totalOutputTokens: number;
  totalCostCents: number;
  eventCount: number;
  lastCheckpointSeq: number | null;
};

export type EventRecordDto = {
  id: string;
  sessionId: string;
  sequenceNum: number;
  eventType: string;
  timestamp: string;
  tokenCount: number | null;
  payload: unknown;
};

export type RuntimeInfoDto = {
  sessionId: string;
  workspaceId: string;
  model: string;
  sandboxRoot: string;
  runtimeKind: string;
};

export type MoaConfigDto = {
  defaultProvider: string;
  defaultModel: string;
  reasoningEffort: string;
  webSearchEnabled: boolean;
  workspaceInstructions: string | null;
  userInstructions: string | null;
  sandboxDir: string;
  memoryDir: string;
  daemonAutoConnect: boolean;
  observabilityEnabled: boolean;
  environment: string | null;
};

export type ModelOptionDto = {
  value: string;
  label: string;
  provider: string;
  contextWindow: number | null;
};
