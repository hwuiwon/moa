//! Shared MOA types, traits, and error definitions.
//!
//! The crate root deliberately exports only the universal [`MoaError`],
//! [`Result`], and [`WORKSPACE_ID`] items. All other APIs are addressed through
//! their owning modules.

pub mod analytics;
pub mod canonical_json;
pub mod coordination_counters;
pub mod diff;
pub mod error;
pub mod events;
pub mod session_engine;
pub mod session_replay;
pub mod shell;
pub mod traits;
pub mod transcript;
pub mod truncation;
pub mod types;
pub mod workspace;

pub use error::{MoaError, Result};
pub use workspace::WORKSPACE_ID;
