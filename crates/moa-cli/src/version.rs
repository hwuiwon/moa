//! Version command helpers.

use super::*;

/// Returns a plain-text version string.
pub(crate) fn version_text() -> String {
    format!("moa {}", env!("CARGO_PKG_VERSION"))
}
