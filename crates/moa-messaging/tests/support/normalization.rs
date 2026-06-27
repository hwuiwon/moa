//! Normalization test fixtures.

use std::path::PathBuf;

use moa_core::{InboundMessage, MoaError};

/// Loads a JSON fixture, stripping leading provenance comments before parsing.
pub fn fixture_text(name: &str) -> String {
    let path = fixture_path(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read fixture {}: {error}", path.display()));
    raw.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Asserts a normalizer returned a typed messaging/core error rather than panicking.
pub fn assert_typed_messaging_error(result: moa_core::Result<InboundMessage>) {
    assert!(
        matches!(
            result,
            Err(MoaError::SerdeJson(_)) | Err(MoaError::ValidationError(_))
        ),
        "expected typed messaging error, got {result:?}"
    );
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join("fixtures")
        .join(name)
}
