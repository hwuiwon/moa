//! Normalization test fixtures.

use std::path::PathBuf;

use moa_core::error::MoaError;

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

/// Asserts a normalizer rejected an unparseable payload with the `SerdeJson` variant.
pub fn assert_serde_json_error<T: std::fmt::Debug>(result: moa_core::error::Result<T>) {
    assert!(
        matches!(result, Err(MoaError::SerdeJson(_))),
        "expected a SerdeJson deserialization error, got {result:?}"
    );
}

/// Asserts a normalizer rejected a well-formed but unsupported event with the
/// `ValidationError` variant (not a deserialization failure).
pub fn assert_validation_error<T: std::fmt::Debug>(result: moa_core::error::Result<T>) {
    assert!(
        matches!(result, Err(MoaError::ValidationError(_))),
        "expected a ValidationError, got {result:?}"
    );
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join("fixtures")
        .join(name)
}
