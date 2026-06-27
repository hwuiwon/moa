//! JSONL fixture loader for lineage audit tests.

use std::path::PathBuf;

use serde_json::Value;

/// Loads a JSONL fixture into one value per non-empty line.
pub(crate) fn fixture_jsonl(name: &str) -> Vec<Value> {
    fixture_text(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("failed to parse fixture {name} line: {error}"))
        })
        .collect()
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
