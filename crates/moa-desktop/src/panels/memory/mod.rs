//! Memory browser: sidebar list + center-panel viewer for graph memory nodes.

mod memory_list;
mod memory_viewer;

pub use memory_list::{MemoryList, MemoryPageSelected};
pub use memory_viewer::MemoryViewer;
