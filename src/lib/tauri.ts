import { invoke } from "@tauri-apps/api/core";

import type {
  MoaConfigDto,
  ModelOptionDto,
  RuntimeInfoDto,
  SessionMetaDto,
  SessionPreviewDto,
} from "@/lib/types";

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
  getRuntimeInfo: () => invokeCommand<RuntimeInfoDto>("get_runtime_info"),
  getConfig: () => invokeCommand<MoaConfigDto>("get_config"),
  listModelOptions: () =>
    invokeCommand<ModelOptionDto[]>("list_model_options"),
  setModel: (model: string) => invokeCommand<string>("set_model", { model }),
};
