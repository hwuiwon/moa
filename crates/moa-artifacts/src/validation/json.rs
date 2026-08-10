//! JSON Schema and RFC 6901 pointer validation helpers.

use serde_json::Value;

use super::ValidationReport;

/// Requires a JSON Schema payload to use an object envelope.
pub(super) fn validate_json_schema(path: &str, schema: &Value, report: &mut ValidationReport) {
    if !schema.is_object() {
        report.push_error(path, "JSON schema must be an object");
    }
}

/// Records an error when a value is not an RFC 6901 JSON Pointer.
pub(super) fn validate_json_pointer(path: &str, pointer: &str, report: &mut ValidationReport) {
    if !is_json_pointer(pointer) {
        report.push_error(path, "value must be an RFC 6901 JSON Pointer");
    }
}

/// Returns whether a string is a syntactically valid RFC 6901 JSON Pointer.
pub(super) fn is_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }
    let bytes = pointer.as_bytes();
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            let Some(next) = bytes.get(index + 1) else {
                return false;
            };
            if !matches!(next, b'0' | b'1') {
                return false;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    true
}

/// Decodes a valid JSON Pointer into its unescaped path segments.
pub(super) fn decode_json_pointer_segments(pointer: &str) -> Option<Vec<String>> {
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    let encoded_segments = pointer.strip_prefix('/')?;
    encoded_segments
        .split('/')
        .map(|encoded| {
            let mut decoded = String::with_capacity(encoded.len());
            let mut characters = encoded.chars();
            while let Some(character) = characters.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match characters.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    Some(_) | None => return None,
                }
            }
            Some(decoded)
        })
        .collect()
}

/// Returns whether the left pointer segments are a strict prefix of the right.
pub(super) fn pointer_segments_are_strict_prefix(left: &[String], right: &[String]) -> bool {
    left.len() < right.len() && right.starts_with(left)
}
