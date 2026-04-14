//! Memory browser: sidebar list + center-panel viewer for wiki pages.

mod memory_list;
mod memory_viewer;

pub use memory_list::{MemoryList, MemoryPageSelected};
pub use memory_viewer::MemoryViewer;
