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
            commands::search_memory,
            commands::delete_memory_page,
            commands::memory_index,
            commands::get_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running MOA desktop application");
}
