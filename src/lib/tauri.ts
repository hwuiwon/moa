import { Channel, invoke } from "@tauri-apps/api/core";

import type {
  EventRecordDto,
  MemorySearchResultDto,
  MoaConfigDto,
  ModelOptionDto,
  PageSummaryDto,
  RuntimeInfoDto,
  SessionMetaDto,
  SessionPreviewDto,
  StreamEvent,
  WikiPageDto,
} from "@/lib/bindings";

function errorMessage(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }

  if (error && typeof error === "object") {
    const message = Reflect.get(error, "message");
    if (typeof message === "string") {
      return message;
    }
  }

  return "Unknown desktop IPC error";
}

export async function invokeCommand<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw new Error(errorMessage(error));
  }
}

export const tauriClient = {
  createSession: () => invokeCommand<string>("create_session"),
  selectSession: (sessionId: string) =>
    invokeCommand<SessionMetaDto>("select_session", { sessionId }),
  listSessionPreviews: () =>
    invokeCommand<SessionPreviewDto[]>("list_session_previews"),
  getSession: (sessionId: string) =>
    invokeCommand<SessionMetaDto>("get_session", { sessionId }),
  getSessionEvents: (sessionId: string) =>
    invokeCommand<EventRecordDto[]>("get_session_events", { sessionId }),
  getRuntimeInfo: () => invokeCommand<RuntimeInfoDto>("get_runtime_info"),
  getConfig: () => invokeCommand<MoaConfigDto>("get_config"),
  listMemoryPages: (filter?: string | null) =>
    invokeCommand<PageSummaryDto[]>("list_memory_pages", { filter }),
  readMemoryPage: (path: string) =>
    invokeCommand<WikiPageDto>("read_memory_page", { path }),
  writeMemoryPage: (page: WikiPageDto) =>
    invokeCommand<WikiPageDto>("write_memory_page", { page }),
  searchMemory: (query: string, limit = 20) =>
    invokeCommand<MemorySearchResultDto[]>("search_memory", { limit, query }),
  deleteMemoryPage: (path: string) =>
    invokeCommand<void>("delete_memory_page", { path }),
  listModelOptions: () =>
    invokeCommand<ModelOptionDto[]>("list_model_options"),
  setModel: (model: string) => invokeCommand<string>("set_model", { model }),
  sendMessage: (
    sessionId: string,
    prompt: string,
    onEvent: Channel<StreamEvent>,
  ) => invokeCommand<void>("send_message", { sessionId, prompt, onEvent }),
  stopSession: (sessionId: string) =>
    invokeCommand<void>("stop_session", { sessionId }),
  respondToApproval: (requestId: string, decision: string) =>
    invokeCommand<void>("respond_to_approval", {
      decision,
      requestId,
      request_id: requestId,
    }),
};
