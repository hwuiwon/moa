//! RFC 8785 JSON Canonicalization Scheme support.
//!
//! MOA signs the canonical JSON bytes, not a pretty-printed or transport
//! representation. This module keeps canonicalization deliberately small:
//! object keys are sorted by UTF-16 code units, strings use minimal JSON
//! escapes, arrays preserve order, and event payloads are restricted to
//! integer JSON numbers.

use serde_json::Value;
use std::io::Write;
use thiserror::Error;

/// Canonicalization failures.
#[derive(Debug, Error)]
pub enum JcsError {
    /// Floating point numbers are intentionally rejected by MOA event payloads.
    #[error("floating point JSON numbers are not supported in MOA audit events")]
    FloatUnsupported,
}

/// Return RFC 8785-style canonical bytes for a JSON value.
///
/// Event payloads must not contain floating point numbers. Use string-encoded
/// fixed-decimal values if a future event requires decimal precision.
pub fn canonicalize(value: &Value) -> Result<Vec<u8>, JcsError> {
    let mut out = Vec::with_capacity(256);
    write_value(&mut out, value)?;
    Ok(out)
}

fn write_value(out: &mut Vec<u8>, value: &Value) -> Result<(), JcsError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => write_number(out, number)?,
        Value::String(string) => write_string(out, string),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_value(out, item)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|left, right| utf16_cmp(left, right));
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(out, key);
                out.push(b':');
                write_value(out, &map[*key])?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn utf16_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_units: Vec<u16> = left.encode_utf16().collect();
    let right_units: Vec<u16> = right.encode_utf16().collect();
    left_units.cmp(&right_units)
}

fn write_number(out: &mut Vec<u8>, number: &serde_json::Number) -> Result<(), JcsError> {
    if number.is_f64() {
        return Err(JcsError::FloatUnsupported);
    }
    out.extend_from_slice(number.to_string().as_bytes());
    Ok(())
}

fn write_string(out: &mut Vec<u8>, string: &str) {
    out.push(b'"');
    for ch in string.chars() {
        match ch {
            '"' => out.extend_from_slice(br#"\""#),
            '\\' => out.extend_from_slice(br#"\\"#),
            '\n' => out.extend_from_slice(br"\n"),
            '\r' => out.extend_from_slice(br"\r"),
            '\t' => out.extend_from_slice(br"\t"),
            '\u{08}' => out.extend_from_slice(br"\b"),
            '\u{0c}' => out.extend_from_slice(br"\f"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => {
                let mut buf = [0_u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonicalize_sorts_keys() {
        // Pins: object keys are sorted deterministically before signing.
        let value = json!({ "b": 1, "a": 2 });

        let canonical = canonicalize(&value).expect("canonicalize");

        assert_eq!(canonical, br#"{"a":2,"b":1}"#);
    }

    #[test]
    fn canonicalize_rejects_floats() {
        // Pins: MOA audit payloads cannot silently sign non-canonical floats.
        let value = json!({ "amount": 1.25 });

        let error = canonicalize(&value).expect_err("float should be rejected");

        assert!(matches!(error, JcsError::FloatUnsupported));
    }
}
