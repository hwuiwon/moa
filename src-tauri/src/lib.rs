//! Tauri v2 desktop backend for MOA.

mod commands;
mod dto;
mod error;
mod stream;

use std::error::Error as StdError;

use moa_core::{MoaConfig, Platform};
use moa_runtime::ChatRuntime;
use tauri::Manager;
use tokio::sync::Mutex;

/// Managed application state shared across Tauri commands.
pub struct AppState {
    /// Shared runtime used by the desktop application.
    pub runtime: Mutex<ChatRuntime>,
}

fn boxed_error(error: impl StdError + Send + Sync + 'static) -> Box<dyn StdError> {
    Box::new(error)
}

/// Starts the MOA desktop application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let config = MoaConfig::load().map_err(boxed_error)?;
            let runtime =
                tauri::async_runtime::block_on(ChatRuntime::from_config(config, Platform::Desktop))
                    .map_err(boxed_error)?;
            app.manage(AppState {
                runtime: Mutex::new(runtime),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_session,
            commands::select_session,
            commands::list_sessions,
            commands::list_session_previews,
            commands::get_session,
            commands::get_session_events,
            commands::get_runtime_info,
            commands::set_workspace,
            commands::reset_session,
            commands::set_model,
            commands::list_model_options,
            commands::get_tool_names,
            commands::queue_message,
            commands::send_message,
            commands::stop_session,
            commands::soft_cancel_session,
            commands::hard_cancel_session,
            commands::respond_to_approval,
            commands::respond_to_session_approval,
            commands::cancel_active_generation,
            commands::list_memory_pages,
            commands::recent_memory_entries,
            commands::read_memory_page,
            commands::write_memory_page,
            commands::search_memory,
            commands::delete_memory_page,
            commands::memory_index,
            commands::get_config,
            commands::update_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MOA desktop application");
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use ts_rs::TS;

    use crate::{
        dto::{
            EventRecordDto, MemorySearchResultDto, MoaConfigDto, ModelOptionDto, PageSummaryDto,
            RuntimeInfoDto, SessionMetaDto, SessionPreviewDto, SessionSummaryDto, WikiPageDto,
        },
        error::MoaAppError,
        stream::StreamEvent,
    };

    #[test]
    fn export_bindings() {
        RuntimeInfoDto::export().expect("failed to export RuntimeInfoDto");
        SessionSummaryDto::export().expect("failed to export SessionSummaryDto");
        SessionPreviewDto::export().expect("failed to export SessionPreviewDto");
        SessionMetaDto::export().expect("failed to export SessionMetaDto");
        EventRecordDto::export().expect("failed to export EventRecordDto");
        MemorySearchResultDto::export().expect("failed to export MemorySearchResultDto");
        PageSummaryDto::export().expect("failed to export PageSummaryDto");
        WikiPageDto::export().expect("failed to export WikiPageDto");
        MoaConfigDto::export().expect("failed to export MoaConfigDto");
        ModelOptionDto::export().expect("failed to export ModelOptionDto");
        StreamEvent::export().expect("failed to export StreamEvent");
        MoaAppError::export().expect("failed to export MoaAppError");

        write_bindings_barrel().expect("failed to write bindings barrel");
    }

    fn write_bindings_barrel() -> std::io::Result<()> {
        let bindings_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/lib/bindings");
        fs::create_dir_all(&bindings_dir)?;

        let contents = r#"// Auto-generated barrel for ts-rs bindings. Do not edit manually.
export type { EventRecordDto } from "./EventRecordDto";
export type { MemorySearchResultDto } from "./MemorySearchResultDto";
export type { MoaAppError } from "./MoaAppError";
export type { MoaConfigDto } from "./MoaConfigDto";
export type { ModelOptionDto } from "./ModelOptionDto";
export type { PageSummaryDto } from "./PageSummaryDto";
export type { RuntimeInfoDto } from "./RuntimeInfoDto";
export type { SessionMetaDto } from "./SessionMetaDto";
export type { SessionPreviewDto } from "./SessionPreviewDto";
export type { SessionSummaryDto } from "./SessionSummaryDto";
export type { StreamEvent } from "./StreamEvent";
export type { WikiPageDto } from "./WikiPageDto";
"#;

        fs::write(bindings_dir.join("index.ts"), contents)
    }
}
