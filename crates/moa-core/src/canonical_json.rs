//! Canonical JSON byte serialization shared across MOA domains.

use serde::Serialize;

/// Serializes a value with deterministic object-key ordering and valid JSON control escapes.
pub fn canonical_json_bytes<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Vec<u8>> {
    let value = serde_json::to_value(value)?;
    let mut output = Vec::new();
    write_canonical_value(&value, &mut output)?;
    Ok(output)
}

fn write_canonical_value(
    value: &serde_json::Value,
    output: &mut Vec<u8>,
) -> serde_json::Result<()> {
    match value {
        serde_json::Value::Null => output.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            output.extend_from_slice(if *value { b"true" } else { b"false" });
        }
        serde_json::Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => serde_json::to_writer(output, value)?,
        serde_json::Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_value(value, output)?;
            }
            output.push(b']');
        }
        serde_json::Value::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)?;
                output.push(b':');
                write_canonical_value(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the shared canonical byte contract.

    use super::canonical_json_bytes;
    use serde_json::Value;

    #[test]
    fn canonical_json_bytes_escapes_newlines_and_tabs_and_round_trips() {
        // Pins: multiline values remain valid canonical JSON without changing the established
        // control-free representation.
        let multiline = serde_json::json!({"text": "first\n\tsecond"});
        let canonical = canonical_json_bytes(&multiline).expect("canonicalize multiline JSON");

        assert_eq!(canonical, br#"{"text":"first\n\tsecond"}"#);
        assert_eq!(
            serde_json::from_slice::<Value>(&canonical).expect("canonical JSON should parse"),
            multiline
        );
        assert_eq!(
            canonical_json_bytes(&serde_json::json!({"z": 2, "a": "plain"}))
                .expect("canonicalize control-free JSON"),
            br#"{"a":"plain","z":2}"#
        );
    }

    #[test]
    fn canonical_json_bytes_escapes_remaining_controls_with_lowercase_hex() {
        // Pins: every other JSON-forbidden ASCII control uses a stable lowercase unicode escape
        // and survives deserialization unchanged.
        let controls = String::from_utf8(vec![
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x0b, 0x0e, 0x0f, 0x10, 0x11, 0x12,
            0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
        ])
        .expect("ASCII controls are valid UTF-8");
        let value = serde_json::json!({"value": controls});
        let canonical = canonical_json_bytes(&value).expect("canonicalize ASCII controls");

        assert_eq!(
            canonical,
            br#"{"value":"\u0000\u0001\u0002\u0003\u0004\u0005\u0006\u0007\u000b\u000e\u000f\u0010\u0011\u0012\u0013\u0014\u0015\u0016\u0017\u0018\u0019\u001a\u001b\u001c\u001d\u001e\u001f"}"#
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&canonical).expect("canonical JSON should parse"),
            value
        );
    }

    #[test]
    fn canonical_json_bytes_supports_finite_floats() {
        // Pins: metric and calibration payloads share the canonical serializer without losing
        // serde_json's deterministic shortest finite-number representation.
        assert_eq!(
            canonical_json_bytes(&serde_json::json!({"z": 0.9, "a": 1.25}))
                .expect("canonicalize finite floats"),
            br#"{"a":1.25,"z":0.9}"#
        );
    }
}
