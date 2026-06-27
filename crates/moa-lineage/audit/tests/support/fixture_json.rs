//! JSON fixture loader for lineage audit tests.

use std::path::PathBuf;

use serde_json::Value;

/// Loads a JSON fixture by name.
pub(crate) fn fixture_json(name: &str) -> Value {
    serde_json::from_str(&fixture_text(name))
        .unwrap_or_else(|error| panic!("failed to parse fixture {name}: {error}"))
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|error| panic!("failed to read fixture {name}: {error}"))
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join("fixtures")
        .join(name)
}
